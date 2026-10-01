#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod devices;
mod export;
mod format;
mod model;
mod overlay;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("PowerMeter")
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native("PowerMeter", options, Box::new(|cc| Ok(Box::new(app::PowerMeterApp::new(cc)))))
}
