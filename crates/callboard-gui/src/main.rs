use callboard::lifecycle::{Environment, Paths};
use callboard_gui::{app::App, find_service_executable};
use eframe::egui;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let paths = Paths::resolve(&Environment::current())?;
    let no_auto_start = std::env::args_os().any(|arg| arg == "--no-auto-start");
    let override_path = std::env::var_os("CALLBOARD_EXECUTABLE").map(PathBuf::from);
    let search_path = std::env::var_os("PATH").unwrap_or_default();
    let auto_start = if no_auto_start {
        None
    } else {
        find_service_executable(
            &std::env::current_exe()?,
            override_path.as_deref(),
            &search_path,
        )
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 760.0])
            .with_app_id("callboard")
            .with_icon(callboard_gui::window_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "Callboard",
        options,
        Box::new(move |cc| {
            App::new(paths, auto_start, no_auto_start, cc.egui_ctx.clone())
                .map(|app| Box::new(app) as Box<dyn eframe::App>)
                .map_err(|e| e.into())
        }),
    )?;
    Ok(())
}
