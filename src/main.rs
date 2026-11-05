#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod meta;
mod runner;
mod scan;
mod ui;

fn main() {
    let state = std::sync::Arc::new(std::sync::Mutex::new(ui::AppState::new()));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([900.0, 700.0]),
        ..Default::default()
    };

    let _ = eframe::run_native(
        "xdelta3 GUI",
        options,
        Box::new(move |_cc| Ok(Box::new(App { state }))),
    );
}

struct App {
    state: std::sync::Arc<std::sync::Mutex<ui::AppState>>,
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let mut s = self.state.lock().unwrap();
        ui::draw(ui, &mut s);
    }
}