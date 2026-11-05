use crate::meta::{PatchMeta, SourceKind};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

pub enum ProgressMsg {
    CurrentFile(String),
    Overall(f32),
    Done(Result<(), String>),
}

const XDELTA_BYTES: &[u8] = include_bytes!("xdelta3.exe");

pub fn ensure_xdelta_exe() -> Result<PathBuf, String> {
    let exe_dir = std::env::current_exe()
        .map_err(|e| format!("Cannot resolve current exe path: {}", e))?
        .parent()
        .ok_or_else(|| "Cannot resolve current exe directory".to_string())?
        .to_path_buf();

    let xdelta_path = exe_dir.join("xdelta3.exe");

    if xdelta_path.exists() {
        return Ok(xdelta_path);
    }

    std::fs::write(&xdelta_path, XDELTA_BYTES)
        .map_err(|e| format!("Failed to extract xdelta3.exe to {}: {}", xdelta_path.display(), e))?;

    Ok(xdelta_path)
}

fn check_exes(xdelta_exe: &Path) -> Result<(), String> {
    if !xdelta_exe.exists() {
        return Err(format!("Cannot find xdelta3.exe at: {}", xdelta_exe.display()));
    }
    Ok(())
}

fn decode_stderr(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            let (cow, _, _) = encoding_rs::WINDOWS_1251.decode(bytes);
            cow.to_string()
        }
    }
}

fn empty_source_path() -> Result<PathBuf, String> {
    let path = std::env::temp_dir().join("xdelta3gui_empty_source.bin");
    if !path.exists() {
        std::fs::File::create(&path)
            .map_err(|e| format!("Failed to create empty source file: {}", e))?;
    }
    Ok(path)
}

pub fn temp_dir_for_zip(zip_path: &Path) -> PathBuf {
    let parent = zip_path.parent().unwrap_or_else(|| Path::new("."));
    let stem = zip_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "patch".to_string());
    parent.join(format!("{}_temp", stem))
}

pub fn unpack_dir_for_zip(zip_path: &Path) -> PathBuf {
    let parent = zip_path.parent().unwrap_or_else(|| Path::new("."));
    let stem = zip_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "patch".to_string());
    parent.join(format!("{}_temp_unpack", stem))
}

fn native_rel(rel: &str) -> String {
    if cfg!(windows) {
        rel.replace('/', "\\")
    } else {
        rel.to_string()
    }
}

fn run_cmd(program: &Path, args: &[&str]) -> Result<(), String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let output = cmd
        .output()
        .map_err(|e| format!("Failed to start {}: {}", program.display(), e))?;

    if !output.status.success() {
        let err = decode_stderr(&output.stderr);
        return Err(format!("Command failed: {:?}\nStderr: {}", program, err.trim()));
    }
    Ok(())
}

pub fn create_patch_to_dir(
    old_dir: &Path,
    new_dir: &Path,
    work_dir: &Path,
    meta: &PatchMeta,
    xdelta_exe: &Path,
    tx: std::sync::mpsc::Sender<ProgressMsg>,
    cancel_flag: Arc<AtomicBool>,
) -> bool {
    if let Err(e) = check_exes(xdelta_exe) {
        let _ = tx.send(ProgressMsg::Done(Err(e)));
        return false;
    }

    let empty_src = match empty_source_path() {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(ProgressMsg::Done(Err(e)));
            return false;
        }
    };

    if let Err(e) = std::fs::create_dir_all(work_dir) {
        let _ = tx.send(ProgressMsg::Done(Err(format!("Failed to create work dir: {}", e))));
        return false;
    }

    let meta_path = work_dir.join("xdelta_metainfo.json");
    if let Err(e) = meta.save(&meta_path) {
        let _ = tx.send(ProgressMsg::Done(Err(format!("Failed to save metainfo: {}", e))));
        return false;
    }

    let total = meta.operations.len();
    for (i, op) in meta.operations.iter().enumerate() {
        if cancel_flag.load(Ordering::Relaxed) {
            let _ = tx.send(ProgressMsg::Done(Err("Cancelled by user".into())));
            return false;
        }

        let rel_path = op.relative_path().clone();
        let _ = tx.send(ProgressMsg::CurrentFile(rel_path.clone()));

        let rel_native = native_rel(op.relative_path());
        let new_file = new_dir.join(&rel_native);
        let out_file = work_dir.join(op.target_file());
        if let Some(parent) = out_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let source_path: PathBuf = match op.source {
            SourceKind::Existing => old_dir.join(&rel_native),
            SourceKind::Empty => empty_src.clone(),
        };

        let res = run_cmd(
            xdelta_exe,
            &[
                "-e", "-9", "-S", "lzma", "-f",
                "-s", &source_path.to_string_lossy(),
                &new_file.to_string_lossy(),
                &out_file.to_string_lossy(),
            ],
        );

        if let Err(e) = res {
            let _ = tx.send(ProgressMsg::Done(Err(e)));
            return false;
        }

        let _ = tx.send(ProgressMsg::Overall((i + 1) as f32 / total as f32));
    }

    true
}

pub fn pack_dir_to_zip(
    work_dir: &Path,
    zip_path: &Path,
    tx: std::sync::mpsc::Sender<ProgressMsg>,
    cancel_flag: Arc<AtomicBool>,
) -> Result<bool, String> {
    if let Some(parent) = zip_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create zip parent dir: {}", e))?;
    }

    let zip_file = std::fs::File::create(zip_path)
        .map_err(|e| format!("Failed to create zip: {}", e))?;
    let mut zip = ZipWriter::new(zip_file);
    let opts = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(5));

    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for entry in walkdir::WalkDir::new(work_dir).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            let rel = entry
                .path()
                .strip_prefix(work_dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            files.push((rel, entry.path().to_path_buf()));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let total = files.len().max(1);
    for (i, (rel, path)) in files.iter().enumerate() {
        if cancel_flag.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(zip_path);
            return Ok(false);
        }

        zip.start_file(rel, opts)
            .map_err(|e| format!("zip start_file {}: {}", rel, e))?;

        let mut f = std::fs::File::open(path)
            .map_err(|e| format!("open {}: {}", path.display(), e))?;
        std::io::copy(&mut f, &mut zip)
            .map_err(|e| format!("zip write {}: {}", rel, e))?;

        let _ = tx.send(ProgressMsg::Overall((i + 1) as f32 / total as f32));
    }

    zip.finish().map_err(|e| format!("zip finish: {}", e))?;
    Ok(true)
}

pub fn create_patch(
    old_dir: &Path,
    new_dir: &Path,
    zip_path: &Path,
    meta: &PatchMeta,
    xdelta_exe: &Path,
    tx: std::sync::mpsc::Sender<ProgressMsg>,
    cancel_flag: Arc<AtomicBool>,
) {
    let work_dir = temp_dir_for_zip(zip_path);

    let _ = std::fs::remove_dir_all(&work_dir);

    let ok = create_patch_to_dir(
        old_dir, new_dir, &work_dir, meta, xdelta_exe,
        tx.clone(), cancel_flag.clone(),
    );
    if !ok {
        let _ = std::fs::remove_dir_all(&work_dir);
        return;
    }

    match pack_dir_to_zip(&work_dir, zip_path, tx.clone(), cancel_flag.clone()) {
        Ok(true) => {
            let _ = std::fs::remove_dir_all(&work_dir);
            let _ = tx.send(ProgressMsg::Done(Ok(())));
        }
        Ok(false) => {
            let _ = std::fs::remove_dir_all(&work_dir);
            let _ = tx.send(ProgressMsg::Done(Err("Cancelled by user".into())));
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&work_dir);
            let _ = tx.send(ProgressMsg::Done(Err(e)));
        }
    }
}

pub fn apply_patch(
    old_dir: &Path,
    out_dir: &Path,
    zip_path: &Path,
    xdelta_exe: &Path,
    tx: std::sync::mpsc::Sender<ProgressMsg>,
    cancel_flag: Arc<AtomicBool>,
) {
    if let Err(e) = check_exes(xdelta_exe) {
        let _ = tx.send(ProgressMsg::Done(Err(e)));
        return;
    }

    let empty_src = match empty_source_path() {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(ProgressMsg::Done(Err(e)));
            return;
        }
    };

    let unpack_dir = unpack_dir_for_zip(zip_path);
    let _ = std::fs::remove_dir_all(&unpack_dir);
    if let Err(e) = std::fs::create_dir_all(&unpack_dir) {
        let _ = tx.send(ProgressMsg::Done(Err(format!(
            "Failed to create unpack dir: {}", e
        ))));
        return;
    }

    let zip_file = match std::fs::File::open(zip_path) {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&unpack_dir);
            let _ = tx.send(ProgressMsg::Done(Err(format!("Failed to open zip: {}", e))));
            return;
        }
    };
    let mut archive = match ZipArchive::new(zip_file) {
        Ok(a) => a,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&unpack_dir);
            let _ = tx.send(ProgressMsg::Done(Err(format!("Invalid zip: {}", e))));
            return;
        }
    };

    let meta: PatchMeta = {
        let mut f = match archive.by_name("xdelta_metainfo.json") {
            Ok(f) => f,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&unpack_dir);
                let _ = tx.send(ProgressMsg::Done(Err(format!("meta not found in zip: {}", e))));
                return;
            }
        };
        let mut s = String::new();
        if let Err(e) = f.read_to_string(&mut s) {
            let _ = std::fs::remove_dir_all(&unpack_dir);
            let _ = tx.send(ProgressMsg::Done(Err(format!("read meta: {}", e))));
            return;
        }
        match serde_json::from_str(&s) {
            Ok(m) => m,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&unpack_dir);
                let _ = tx.send(ProgressMsg::Done(Err(format!("parse meta: {}", e))));
                return;
            }
        }
    };

    let total = meta.operations.len();
    for (i, op) in meta.operations.iter().enumerate() {
        if cancel_flag.load(Ordering::Relaxed) {
            let _ = std::fs::remove_dir_all(&unpack_dir);
            let _ = tx.send(ProgressMsg::Done(Err("Cancelled by user".into())));
            return;
        }

        let rel_path = op.relative_path().clone();
        let rel_native = native_rel(op.relative_path());

        let target_file = out_dir.join(&rel_native);
        if let Some(parent) = target_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let _ = tx.send(ProgressMsg::CurrentFile(rel_path.clone()));

        let source_path: PathBuf = match op.source {
            SourceKind::Existing => old_dir.join(&rel_native),
            SourceKind::Empty => empty_src.clone(),
        };

        let entry_name = op.target_file();

        let delta_temp = unpack_dir.join(&rel_native);
        let delta_temp = delta_temp.with_extension(
            delta_temp
                .extension()
                .map(|e| format!("{}.xdelta", e.to_string_lossy()))
                .unwrap_or_else(|| "xdelta".to_string()),
        );
        if let Some(parent) = delta_temp.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        {
            let mut entry = match archive.by_name(&entry_name) {
                Ok(e) => e,
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&unpack_dir);
                    let _ = tx.send(ProgressMsg::Done(Err(format!(
                        "zip entry {} not found: {}",
                        entry_name, e
                    ))));
                    return;
                }
            };
            let mut out = match std::fs::File::create(&delta_temp) {
                Ok(f) => f,
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&unpack_dir);
                    let _ = tx.send(ProgressMsg::Done(Err(format!(
                        "Failed to create temp delta {}: {}",
                        delta_temp.display(),
                        e
                    ))));
                    return;
                }
            };
            if let Err(e) = std::io::copy(&mut entry, &mut out) {
                let _ = std::fs::remove_dir_all(&unpack_dir);
                let _ = tx.send(ProgressMsg::Done(Err(format!(
                    "Failed to extract {} from zip: {}",
                    entry_name, e
                ))));
                return;
            }
        }

        let res = run_cmd(
            xdelta_exe,
            &[
                "-d", "-f",
                "-s", &source_path.to_string_lossy(),
                &delta_temp.to_string_lossy(),
                &target_file.to_string_lossy(),
            ],
        );

        let _ = std::fs::remove_file(&delta_temp);

        if let Err(e) = res {
            let _ = std::fs::remove_dir_all(&unpack_dir);
            let _ = tx.send(ProgressMsg::Done(Err(format!(
                "xdelta3 -d failed for {}: {}",
                rel_path, e
            ))));
            return;
        }

        let _ = tx.send(ProgressMsg::Overall((i + 1) as f32 / total as f32));
    }

    let _ = std::fs::remove_dir_all(&unpack_dir);
    let _ = tx.send(ProgressMsg::Done(Ok(())));
}