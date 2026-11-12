use crate::runner::{self, FileError, ProgressMsg};
use crate::scan;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FORCE_CANCEL_DELAY: Duration = Duration::from_secs(5);
const MAX_SHOWN_ERRORS: usize = 5;

pub struct AppState {
    pub create_old: String,
    pub create_new: String,
    pub create_out: String,
    pub apply_old: String,
    pub apply_out: String,
    pub apply_patch: String,
    pub status: String,

    pub is_running: bool,
    pub overall_progress: f32,
    pub current_file_name: String,
    pub error_count: usize,
    pub cancel_flag: Arc<AtomicBool>,
    pub current_pid: runner::CurrentPid,
    pub rx: Option<Receiver<ProgressMsg>>,
    pub show_cancel_dialog: bool,
    pub pending_cleanup_path: Option<PathBuf>,
    pub pending_cleanup_is_zip: bool,

    pub cancel_started_at: Option<Instant>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            create_old: String::new(),
            create_new: String::new(),
            create_out: String::new(),
            apply_old: String::new(),
            apply_out: String::new(),
            apply_patch: String::new(),
            status: "Ready".into(),
            is_running: false,
            overall_progress: 0.0,
            current_file_name: String::new(),
            error_count: 0,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            current_pid: Arc::new(Mutex::new(None)),
            rx: None,
            show_cancel_dialog: false,
            pending_cleanup_path: None,
            pending_cleanup_is_zip: false,
            cancel_started_at: None,
        }
    }
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    fn reset_progress(&mut self) {
        self.overall_progress = 0.0;
        self.current_file_name.clear();
        self.error_count = 0;
        self.cancel_started_at = None;
        self.cancel_flag.store(false, Ordering::Relaxed);
    }
}

fn folder_picker(ui: &mut egui::Ui, label: &str, value: &mut String, enabled: bool) {
    ui.horizontal(|ui| {
        ui.label(label);
        let available_width = ui.available_width();
        let button_width = 40.0;
        let text_width = (available_width - button_width).max(100.0);

        ui.add_enabled_ui(enabled, |ui| {
            ui.add_sized([text_width, 20.0], egui::TextEdit::singleline(value));
            if ui.button("...").clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                    *value = p.display().to_string();
                }
            }
        });
    });
}

fn zip_save_picker(ui: &mut egui::Ui, label: &str, value: &mut String, enabled: bool) {
    ui.horizontal(|ui| {
        ui.label(label);
        let available_width = ui.available_width();
        let button_width = 40.0;
        let text_width = (available_width - button_width).max(100.0);

        ui.add_enabled_ui(enabled, |ui| {
            ui.add_sized([text_width, 20.0], egui::TextEdit::singleline(value));
            if ui.button("...").clicked() {
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter("Patch zip", &["zip"])
                    .set_file_name("patch.zip")
                    .save_file()
                {
                    *value = p.display().to_string();
                }
            }
        });
    });
}

fn zip_open_picker(ui: &mut egui::Ui, label: &str, value: &mut String, enabled: bool) {
    ui.horizontal(|ui| {
        ui.label(label);
        let available_width = ui.available_width();
        let button_width = 40.0;
        let text_width = (available_width - button_width).max(100.0);

        ui.add_enabled_ui(enabled, |ui| {
            ui.add_sized([text_width, 20.0], egui::TextEdit::singleline(value));
            if ui.button("...").clicked() {
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter("Patch zip", &["zip"])
                    .pick_file()
                {
                    *value = p.display().to_string();
                }
            }
        });
    });
}

fn to_path(s: &str) -> Result<PathBuf, String> {
    let trimmed = s.trim().trim_matches('"');
    if trimmed.is_empty() {
        return Err("Error: Path is empty".into());
    }
    let p = PathBuf::from(trimmed);
    if p.is_absolute() {
        Ok(p)
    } else {
        Err(format!("Path must be absolute (e.g. C:\\folder), but got: {}", s))
    }
}

fn normalize_for_compare(p: &PathBuf) -> String {
    let s = p.to_string_lossy().replace('/', "\\");
    let s = s.trim_end_matches('\\');
    s.to_lowercase()
}

fn same_path(a: &PathBuf, b: &PathBuf) -> bool {
    normalize_for_compare(a) == normalize_for_compare(b)
}

fn is_inside(child: &PathBuf, parent: &PathBuf) -> bool {
    let child_str = normalize_for_compare(child);
    let mut parent_str = normalize_for_compare(parent);
    if !parent_str.ends_with('\\') {
        parent_str.push('\\');
    }
    child_str.starts_with(&parent_str) && child_str != parent_str.trim_end_matches('\\')
}

fn check_no_nesting(a: &PathBuf, b: &PathBuf, a_name: &str, b_name: &str) -> Result<(), String> {
    if is_inside(a, b) {
        return Err(format!("Error: {} cannot be inside {}", a_name, b_name));
    }
    if is_inside(b, a) {
        return Err(format!("Error: {} cannot be inside {}", b_name, a_name));
    }
    Ok(())
}

fn require_dir(p: &PathBuf, name: &str) -> Result<(), String> {
    if !p.exists() {
        return Err(format!("Error: {} does not exist: {}", name, p.display()));
    }
    if !p.is_dir() {
        return Err(format!("Error: {} is not a folder: {}", name, p.display()));
    }
    Ok(())
}

fn format_errors_for_status(errors: &[FileError], max_items: usize) -> String {
    let sep = "\n──────────────\n";
    let mut s = String::new();
    let n = errors.len().min(max_items);
    for (i, e) in errors.iter().take(n).enumerate() {
        if i > 0 {
            s.push_str(sep);
        }
        let lines: Vec<&str> = e
            .details
            .lines()
            .map(|l| l.trim_end())
            .filter(|l| !l.trim().is_empty())
            .collect();
        if !lines.is_empty() {
            s.push_str(lines[0]);
            for l in &lines[1..] {
                s.push('\n');
                s.push_str("    ");
                s.push_str(l);
            }
        }
    }
    s
}

pub fn draw(ui: &mut egui::Ui, state: &mut AppState) {
    ui.ctx().set_zoom_factor(1.5);

    while let Some(rx) = state.rx.take() {
        let mut done = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                ProgressMsg::CurrentFile(name) => {
                    state.current_file_name = name;
                }
                ProgressMsg::Overall(prog) => {
                    state.overall_progress = prog;
                }
                ProgressMsg::ErrorCount(n) => {
                    state.error_count = n;
                }
                ProgressMsg::Done(res) => {
                    done = true;
                    state.is_running = false;
                    state.current_file_name.clear();
                    state.cancel_started_at = None;
                    match res {
                        Ok(_) => {
                            state.overall_progress = 1.0;
                            state.status = "Operation completed".into();
                        }
                        Err(e) => {
                            if e == "Cancelled by user" {
                                state.show_cancel_dialog = true;
                            } else {
                                state.status = format!("Error: {e}");
                            }
                        }
                    }
                }
                ProgressMsg::DoneWithErrors { total_ops, errors } => {
                    done = true;
                    state.is_running = false;
                    state.current_file_name.clear();
                    state.cancel_started_at = None;
                    state.overall_progress = 1.0;
                    state.error_count = errors.len();
                    let total_errors = errors.len();
                    let shown = total_errors.min(MAX_SHOWN_ERRORS);
                    let mut msg = format!(
                        "Operation completed. Total {}, with errors {}, shown {}:\n",
                        total_ops, total_errors, shown
                    );
                    msg.push_str(&format_errors_for_status(&errors, MAX_SHOWN_ERRORS));
                    state.status = msg;
                }
            }
        }
        if !done {
            state.rx = Some(rx);
            ui.ctx().request_repaint();
            break;
        }
    }

    if state.show_cancel_dialog {
        egui::Window::new("Cancelled")
            .collapsible(false)
            .resizable(false)
            .show(ui.ctx(), |ui| {
                let what = if state.pending_cleanup_is_zip {
                    "partially created patch file"
                } else {
                    "partially applied output folder"
                };
                ui.label(format!("Operation cancelled. Do you want to delete the {}?", what));
                ui.horizontal(|ui| {
                    if ui.button("Yes, delete").clicked() {
                        if let Some(p) = &state.pending_cleanup_path {
                            if state.pending_cleanup_is_zip {
                                let _ = std::fs::remove_file(p);
                            } else {
                                let _ = std::fs::remove_dir_all(p);
                            }
                        }
                        state.pending_cleanup_path = None;
                        state.show_cancel_dialog = false;
                        state.status = "Cancelled and cleaned up".into();
                    }
                    if ui.button("No, keep").clicked() {
                        state.pending_cleanup_path = None;
                        state.show_cancel_dialog = false;
                        state.status = "Cancelled".into();
                    }
                });
            });
    }

    egui::CentralPanel::default().show(ui, |ui| {
        ui.heading("xdelta3 GUI");
        ui.label(&state.status);

        if state.is_running {
            let has_errors = state.error_count > 0;
            let text = if has_errors {
                format!("Progress (errors: {})", state.error_count)
            } else {
                "Progress".to_string()
            };
            let fill = if has_errors {
                egui::Color32::from_rgb(180, 90, 20)
            } else {
                egui::Color32::from_rgb(0, 120, 215)
            };
            ui.add(
                egui::ProgressBar::new(state.overall_progress)
                    .text(text)
                    .fill(fill),
            );
            if !state.current_file_name.is_empty() {
                ui.label(format!("{}", state.current_file_name));
            }

            match state.cancel_started_at {
                None => {
                    if ui.button("Cancel").clicked() {
                        state.cancel_started_at = Some(Instant::now());
                        state.cancel_flag.store(true, Ordering::Relaxed);
                    }
                }
                Some(started) => {
                    let elapsed = started.elapsed();
                    if elapsed < FORCE_CANCEL_DELAY {
                        let remaining =
                            (FORCE_CANCEL_DELAY.as_secs_f32() - elapsed.as_secs_f32()).max(0.0);
                        let _ = ui.add_enabled(
                            false,
                            egui::Button::new(format!("Cancel ({:.0})", remaining.ceil())),
                        );
                        ui.ctx()
                            .request_repaint_after(Duration::from_millis(100));
                    } else {
                        if ui
                            .add(
                                egui::Button::new("Force Cancel")
                                    .fill(egui::Color32::from_rgb(200, 60, 60)),
                            )
                            .clicked()
                        {
                            runner::force_kill_current(&state.current_pid);
                            state.cancel_flag.store(true, Ordering::Relaxed);
                            state.cancel_started_at = None;
                        }
                        ui.ctx()
                            .request_repaint_after(Duration::from_millis(100));
                    }
                }
            }
        }

        ui.separator();

        let enabled = !state.is_running;

        let available_height = ui.available_height();
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), available_height / 2.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| ui_child_create(ui, state, enabled),
        );
        ui.separator();
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), available_height / 2.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| ui_child_apply(ui, state, enabled),
        );
    });
}

fn ui_child_create(ui: &mut egui::Ui, state: &mut AppState, enabled: bool) {
    ui.heading("Create Patch");
    folder_picker(ui, "Old Version Folder", &mut state.create_old, enabled);
    folder_picker(ui, "New Version Folder", &mut state.create_new, enabled);
    zip_save_picker(ui, "Patch Output (.zip)", &mut state.create_out, enabled);

    if enabled && ui.button("Create Patch").clicked() {
        start_create(state);
    }
}

fn ui_child_apply(ui: &mut egui::Ui, state: &mut AppState, enabled: bool) {
    ui.heading("Apply Patch");
    folder_picker(ui, "Old Version Folder", &mut state.apply_old, enabled);
    folder_picker(ui, "New Version Folder (empty)", &mut state.apply_out, enabled);
    zip_open_picker(ui, "Patch (.zip)", &mut state.apply_patch, enabled);

    if enabled && ui.button("Apply Patch").clicked() {
        start_apply(state);
    }
}

fn start_create(state: &mut AppState) {
    state.status = "Processing...".into();

    let old = match to_path(&state.create_old) {
        Ok(p) => p,
        Err(e) => { state.status = e; return; }
    };
    let new = match to_path(&state.create_new) {
        Ok(p) => p,
        Err(e) => { state.status = e; return; }
    };
    let out = match to_path(&state.create_out) {
        Ok(p) => p,
        Err(e) => { state.status = e; return; }
    };

    if let Err(e) = require_dir(&old, "Old version folder") {
        state.status = e;
        return;
    }
    if let Err(e) = require_dir(&new, "New version folder") {
        state.status = e;
        return;
    }

    if out.is_dir() {
        state.status = "Error: Patch output must be a .zip file path, not a folder".into();
        return;
    }
    if same_path(&old, &new) {
        state.status = "Error: Old and New version folders must be different".into();
        return;
    }

    if let Err(e) = check_no_nesting(&old, &new, "Old version folder", "New version folder") {
        state.status = e;
        return;
    }
    if let Err(e) = check_no_nesting(&out, &old, "Patch zip", "Old version folder") {
        state.status = e;
        return;
    }
    if let Err(e) = check_no_nesting(&out, &new, "Patch zip", "New version folder") {
        state.status = e;
        return;
    }

    let meta = scan::build_plan(&old, &new);
    if meta.operations.is_empty() {
        state.status = "Error: New version folder contains no files; nothing to patch".into();
        return;
    }

    if let Some(parent) = out.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            state.status = format!("Error creating output dir: {}", e);
            return;
        }
    }

    let xdelta = match runner::ensure_xdelta_exe() {
        Ok(p) => p,
        Err(e) => {
            state.status = format!("Error preparing xdelta3.exe: {}", e);
            return;
        }
    };

    state.is_running = true;
    state.reset_progress();
    state.pending_cleanup_path = Some(out.clone());
    state.pending_cleanup_is_zip = true;

    let (tx, rx) = std::sync::mpsc::channel();
    state.rx = Some(rx);
    let cancel = state.cancel_flag.clone();
    let pid = state.current_pid.clone();

    std::thread::spawn(move || {
        runner::create_patch(&old, &new, &out, &meta, &xdelta, tx, cancel, pid);
    });
}

fn start_apply(state: &mut AppState) {
    state.status = "Processing...".into();

    let patch_zip = match to_path(&state.apply_patch) {
        Ok(p) => p,
        Err(e) => { state.status = e; return; }
    };
    let old = match to_path(&state.apply_old) {
        Ok(p) => p,
        Err(e) => { state.status = e; return; }
    };
    let out = match to_path(&state.apply_out) {
        Ok(p) => p,
        Err(e) => { state.status = e; return; }
    };

    if !patch_zip.exists() {
        state.status = format!("Error: Patch file does not exist: {}", patch_zip.display());
        return;
    }
    if !patch_zip.is_file() {
        state.status = format!(
            "Error: Patch path must point to a .zip file: {}",
            patch_zip.display()
        );
        return;
    }

    if let Err(e) = require_dir(&old, "Old version folder") {
        state.status = e;
        return;
    }

    if out.exists() && !out.is_dir() {
        state.status = format!(
            "Error: Output path exists and is not a folder: {}",
            out.display()
        );
        return;
    }
    if same_path(&old, &out) {
        state.status = "Error: Old Version and New Version (empty) folders must be different".into();
        return;
    }

    if let Err(e) = check_no_nesting(
        &old,
        &out,
        "Old version folder",
        "New version (empty) folder",
    ) {
        state.status = e;
        return;
    }

    if let Err(e) = runner::validate_patch_zip(&patch_zip) {
        state.status = format!("Error: {}", e);
        return;
    }

    let xdelta = match runner::ensure_xdelta_exe() {
        Ok(p) => p,
        Err(e) => {
            state.status = format!("Error preparing xdelta3.exe: {}", e);
            return;
        }
    };

    state.is_running = true;
    state.reset_progress();
    state.pending_cleanup_path = Some(out.clone());
    state.pending_cleanup_is_zip = false;

    let (tx, rx) = std::sync::mpsc::channel();
    state.rx = Some(rx);
    let cancel = state.cancel_flag.clone();
    let pid = state.current_pid.clone();

    std::thread::spawn(move || {
        runner::apply_patch(&old, &out, &patch_zip, &xdelta, tx, cancel, pid);
    });
}
