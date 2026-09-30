//! Phase 0 canvas prototype on the desktop (docs/canvas-plan.md).
use callboard_gui::canvas_proto::Proto;
use eframe::egui;

struct App(Proto);

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.0.show(ui);
    }
}

fn main() -> eframe::Result {
    eframe::run_native(
        "Canvas prototype",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([1200.0, 700.0]),
            ..Default::default()
        },
        Box::new(|_| Ok(Box::new(App(Proto::default())))),
    )
}
