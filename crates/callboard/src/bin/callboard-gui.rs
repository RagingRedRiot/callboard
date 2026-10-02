use callboard::{app::App, find_service_executable};
use callboard_service::{
    desktop,
    lifecycle::{Environment, Paths},
    upgrade,
};
use eframe::egui;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let paths = Paths::resolve(&Environment::current())?;
    for flag in ["--install-desktop", "--uninstall-desktop"] {
        if std::env::args_os().any(|arg| arg == flag) {
            return launcher(&paths, flag == "--install-desktop").map_err(|e| e as _);
        }
    }
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
            .with_app_id(desktop::APP_ID)
            .with_icon(callboard::window_icon()),
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

/// `--install-desktop` / `--uninstall-desktop`: the applications-list entry
/// and dock icon.
fn launcher(paths: &Paths, install: bool) -> Result<(), callboard_service::Error> {
    if !install {
        match desktop::remove(paths)? {
            Some(entry) => println!("Removed {} and its icons.", entry.display()),
            None => println!("No launcher entry is installed."),
        }
        return Ok(());
    }
    let executable =
        upgrade::running_executable().ok_or("cannot resolve this executable's path")?;
    let entry = desktop::install(paths, &executable, &callboard::launcher_icons())?;
    println!(
        "Installed {} launching {}. Callboard now appears in your applications list; \
         rerun this after moving the binary.",
        entry.display(),
        executable.display()
    );
    let search_path = std::env::var_os("PATH").unwrap_or_default();
    if find_service_executable(&executable, None, &search_path).is_none() {
        eprintln!(
            "Warning: the callboard service binary is not beside {} or on PATH, so the app \
             cannot start the service. Install it with \
             `cargo install --path crates/callboard --locked`.",
            executable.display()
        );
    }
    Ok(())
}
