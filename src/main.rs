#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod chart;
mod icon;
mod icon_art;
mod sampler;
mod series;
mod theme;
mod tray;
mod util;
mod win_sensors;

use eframe::egui;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1240.0, 780.0])
            // Snapping to a screen quarter is a resize like any other, and
            // Windows refuses a snap smaller than the window's minimum. A
            // 1080p screen at 125 % gives a quarter of 768x432 points, so the
            // floor has to sit below that or the tile simply does not happen.
            // The layout is checked down to this size; below it the chart says
            // "window too small" and the panels still work.
            .with_min_inner_size([600.0, 330.0])
            .with_title("FXViewer")
            // The title bar is ours to draw (like Task Manager / VS Code).
            // winit still keeps WS_CAPTION | WS_SIZEBOX underneath, so the
            // native modal drag loop — Aero snap included — keeps working;
            // winit just eats the non-client area via WM_NCCALCSIZE.
            .with_decorations(false)
            .with_icon(icon::app_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "FXViewer",
        options,
        Box::new(|cc| Ok(Box::new(app::FxApp::new(cc)))),
    )
}
