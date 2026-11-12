use crate::meta::{PatchMeta, SourceKind};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

pub type CurrentPid = Arc<Mutex<Option<u32>>>;

#[derive(Clone, Debug)]
pub struct FileError {
    pub details: String,
}

pub enum ProgressMsg {
    CurrentFile(String),
    Overall(f32),
    ErrorCount(usize),
    Done(Result<(), String>),
    DoneWithErrors { total_ops: usize, errors: Vec<FileError> },
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

pub fn validate_patch_zip(zip_path: &Path) -> Result<(), String> {
    let f = std::fs::File::open(zip_path)
        .map_err(|e| format!("Failed to open zip: {}", e))?;
    let mut archive = ZipArchive::new(f)
        .map_err(|e| format!("Invalid zip: {}", e))?;

    let meta = read_meta_from_archive(&mut archive)?;
    if meta.operations.is_empty() {
        return Err("Patch metadata contains no operations (empty patch)".into());
    }
    Ok(())
}

fn read_meta_from_archive(archive: &mut ZipArchive<std::fs::File>) -> Result<PatchMeta, String> {
    let mut f = archive
        .by_name("xdelta_metainfo.json")
        .map_err(|e| format!("meta not found in zip: {}", e))?;
    let mut s = String::new();
    f.read_to_string(&mut s)
        .map_err(|e| format!("read meta: {}", e))?;
    serde_json::from_str(&s).map_err(|e| format!("parse meta: {}", e))
}

fn check_exes(xdelta_exe: &Path) -> Result<(), String> {
    if !xdelta_exe.exists() {
        return Err(format!("Cannot find xdelta3.exe at: {}", xdelta_exe.display()));
    }
    Ok(())
}

fn looks_like_utf16le(bytes: &[u8]) -> bool {
    if bytes.len() < 4 || bytes.len() % 2 != 0 {
        return false;
    }
    let pairs = bytes.len() / 2;
    let mut zero_high = 0usize;
    for i in 0..pairs.min(256) {
        if bytes[i * 2 + 1] == 0 {
            zero_high += 1;
        }
    }
    zero_high * 2 >= pairs.min(256)
}

fn decode_utf16(bytes: &[u8], le: bool) -> Option<String> {
    if bytes.len() % 2 != 0 {
        return None;
    }
    let mut units = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        let u = if le {
            u16::from_le_bytes([chunk[0], chunk[1]])
        } else {
            u16::from_be_bytes([chunk[0], chunk[1]])
        };
        units.push(u);
    }
    String::from_utf16(&units).ok()
}

#[cfg(windows)]
fn decode_ansi_codepage(bytes: &[u8]) -> Option<String> {
    unsafe extern "system" {
        fn GetACP() -> u32;
        fn MultiByteToWideChar(
            code_page: u32,
            flags: u32,
            s: *const u8,
            cb: i32,
            out: *mut u16,
            cch: i32,
        ) -> i32;
    }
    if bytes.is_empty() {
        return Some(String::new());
    }
    let cp = unsafe { GetACP() };
    let len = unsafe {
        MultiByteToWideChar(
            cp,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            std::ptr::null_mut::<u16>(),
            0,
        )
    };
    if len <= 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    let written = unsafe {
        MultiByteToWideChar(
            cp,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            buf.as_mut_ptr(),
            len,
        )
    };
    if written <= 0 {
        return None;
    }
    buf.truncate(written as usize);
    String::from_utf16(&buf).ok()
}

#[cfg(not(windows))]
fn decode_ansi_codepage(_bytes: &[u8]) -> Option<String> {
    None
}

fn decode_stderr(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.trim_start_matches('\u{feff}').to_string();
    }
    if bytes.len() >= 2 {
        if bytes[0] == 0xFF && bytes[1] == 0xFE {
            if let Some(s) = decode_utf16(&bytes[2..], true) {
                return s;
            }
        }
        if bytes[0] == 0xFE && bytes[1] == 0xFF {
            if let Some(s) = decode_utf16(&bytes[2..], false) {
                return s;
            }
        }
    }
    if looks_like_utf16le(bytes) {
        if let Some(s) = decode_utf16(bytes, true) {
            return s;
        }
    }
    if let Some(s) = decode_ansi_codepage(bytes) {
        return s;
    }
    let (cow, _, _) = encoding_rs::WINDOWS_1252.decode(bytes);
    cow.to_string()
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

fn run_cmd_with_pid(
    program: &Path,
    args: &[&str],
    current_pid: &CurrentPid,
) -> Result<(), String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let child = cmd
        .spawn()
        .map_err(|e| format!("Failed to start {}: {}", program.display(), e))?;

    let pid = child.id();
    {
        let mut guard = current_pid.lock().unwrap();
        *guard = Some(pid);
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed to wait for {}: {}", program.display(), e));

    {
        let mut guard = current_pid.lock().unwrap();
        *guard = None;
    }

    let output = output?;
    if !output.status.success() {
        let err = decode_stderr(&output.stderr);
        let trimmed = err.trim();
        if !trimmed.is_empty() {
            return Err(trimmed.to_string());
        }
        let code = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        return Err(format!("xdelta3 exited with code {}", code));
    }
    Ok(())
}

pub fn force_kill_current(current_pid: &CurrentPid) {
    let pid = {
        let guard = current_pid.lock().unwrap();
        *guard
    };
    let Some(pid) = pid else { return };
    kill_pid(pid);
}

#[cfg(windows)]
fn kill_pid(pid: u32) {
    let mut cmd = Command::new("taskkill");
    cmd.args(["/F", "/PID", &pid.to_string()]);
    cmd.creation_flags(CREATE_NO_WINDOW);
    let _ = cmd.output();
}

#[cfg(not(windows))]
fn kill_pid(pid: u32) {
    let _ = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .output();
}

pub fn create_patch_to_dir(
    old_dir: &Path,
    new_dir: &Path,
    work_dir: &Path,
    meta: &PatchMeta,
    xdelta_exe: &Path,
    tx: std::sync::mpsc::Sender<ProgressMsg>,
    cancel_flag: Arc<AtomicBool>,
    current_pid: CurrentPid,
    errors: &mut Vec<FileError>,
) -> bool {
    if let Err(e) = check_exes(xdelta_exe) {
        let _ = tx.send(ProgressMsg::Done(Err(e)));
        return false;
    }

    if meta.operations.is_empty() {
        let _ = tx.send(ProgressMsg::Done(Err(
            "Patch metadata contains no operations (empty patch)".into(),
        )));
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

        let res = run_cmd_with_pid(
            xdelta_exe,
            &[
                "-e", "-9", "-S", "lzma", "-f",
                "-s", &source_path.to_string_lossy(),
                &new_file.to_string_lossy(),
                &out_file.to_string_lossy(),
            ],
            &current_pid,
        );

        if let Err(e) = res {
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMsg::Done(Err("Cancelled by user".into())));
                return false;
            }
            errors.push(FileError { details: e });
            let _ = tx.send(ProgressMsg::ErrorCount(errors.len()));
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

    let zip_name = zip_path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "patch.zip".to_string());
    let _ = tx.send(ProgressMsg::CurrentFile(zip_name));

    let zip_file = std::fs::File::create(zip_path)
        .map_err(|e| format!("Failed to create zip: {}", e))?;
    let mut zip = ZipWriter::new(zip_file);
    let opts = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(1))
        .large_file(true);

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
    current_pid: CurrentPid,
) {
    if meta.operations.is_empty() {
        let _ = tx.send(ProgressMsg::Done(Err(
            "Patch metadata contains no operations (empty patch)".into(),
        )));
        return;
    }

    let work_dir = temp_dir_for_zip(zip_path);

    let _ = std::fs::remove_dir_all(&work_dir);

    let total_ops = meta.operations.len();
    let mut errors: Vec<FileError> = Vec::new();

    let ok = create_patch_to_dir(
        old_dir, new_dir, &work_dir, meta, xdelta_exe,
        tx.clone(), cancel_flag.clone(), current_pid.clone(), &mut errors,
    );
    if !ok {
        let _ = std::fs::remove_dir_all(&work_dir);
        return;
    }

    match pack_dir_to_zip(&work_dir, zip_path, tx.clone(), cancel_flag.clone()) {
        Ok(true) => {
            let _ = std::fs::remove_dir_all(&work_dir);
            if errors.is_empty() {
                let _ = tx.send(ProgressMsg::Done(Ok(())));
            } else {
                let _ = tx.send(ProgressMsg::DoneWithErrors { total_ops, errors });
            }
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
    current_pid: CurrentPid,
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

    let meta = match read_meta_from_archive(&mut archive) {
        Ok(m) => m,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&unpack_dir);
            let _ = tx.send(ProgressMsg::Done(Err(e)));
            return;
        }
    };

    if meta.operations.is_empty() {
        let _ = std::fs::remove_dir_all(&unpack_dir);
        let _ = tx.send(ProgressMsg::Done(Err(
            "Patch metadata contains no operations (empty patch)".into(),
        )));
        return;
    }

    let total_ops = meta.operations.len();
    let mut errors: Vec<FileError> = Vec::new();

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

        let extract_res: Result<(), String> = (|| {
            let mut entry = archive.by_name(&entry_name).map_err(|e| {
                format!("zip entry {} not found: {}", entry_name, e)
            })?;
            let mut out = std::fs::File::create(&delta_temp)
                .map_err(|e| format!(
                    "Failed to create temp delta {}: {}",
                    delta_temp.display(),
                    e
                ))?;
            std::io::copy(&mut entry, &mut out).map_err(|e| {
                format!("Failed to extract {} from zip: {}", entry_name, e)
            })?;
            Ok(())
        })();

        if let Err(e) = extract_res {
            errors.push(FileError { details: e });
            let _ = tx.send(ProgressMsg::ErrorCount(errors.len()));
            let _ = std::fs::remove_file(&delta_temp);
            let _ = tx.send(ProgressMsg::Overall((i + 1) as f32 / total as f32));
            continue;
        }

        let res = run_cmd_with_pid(
            xdelta_exe,
            &[
                "-d", "-f",
                "-s", &source_path.to_string_lossy(),
                &delta_temp.to_string_lossy(),
                &target_file.to_string_lossy(),
            ],
            &current_pid,
        );

        let _ = std::fs::remove_file(&delta_temp);

        if let Err(e) = res {
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = std::fs::remove_dir_all(&unpack_dir);
                let _ = tx.send(ProgressMsg::Done(Err("Cancelled by user".into())));
                return;
            }
            errors.push(FileError { details: e });
            let _ = tx.send(ProgressMsg::ErrorCount(errors.len()));
        }

        let _ = tx.send(ProgressMsg::Overall((i + 1) as f32 / total as f32));
    }

    let _ = std::fs::remove_dir_all(&unpack_dir);

    if errors.is_empty() {
        let _ = tx.send(ProgressMsg::Done(Ok(())));
    } else {
        let _ = tx.send(ProgressMsg::DoneWithErrors { total_ops, errors });
    }
}
