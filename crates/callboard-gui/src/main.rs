use callboard::lifecycle::{Environment, Paths};
use callboard_core::store::{BoardContents, Feed, ResolvedReference};
use callboard_gui::{
    backend::{self, Contents, Snapshot, Target},
    find_service_executable,
};
use eframe::egui;
use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

type Loaded = (Option<Target>, Result<Snapshot, String>);
struct App {
    auto_start: Option<PathBuf>,
    auto_start_disabled: bool,
    requests: mpsc::SyncSender<Option<Target>>,
    responses: mpsc::Receiver<Loaded>,
    selected: Option<Target>,
    snapshot: Option<Snapshot>,
    error: Option<String>,
    busy: bool,
    refresh_at: Instant,
    show_snoozed: bool,
}

impl App {
    fn new(
        paths: Paths,
        auto_start: Option<PathBuf>,
        auto_start_disabled: bool,
        ctx: egui::Context,
    ) -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let (requests, incoming) = mpsc::sync_channel::<Option<Target>>(1);
        let (outgoing, responses) = mpsc::sync_channel(1);
        let worker_auto_start = auto_start.clone();
        std::thread::Builder::new()
            .name("callboard-service-client".into())
            .spawn(move || {
                while let Ok(target) = incoming.recv() {
                    let result = runtime.block_on(backend::load(
                        &paths,
                        worker_auto_start.as_deref(),
                        target.as_ref(),
                    ));
                    if outgoing.send((target, result)).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            auto_start,
            auto_start_disabled,
            requests,
            responses,
            selected: None,
            snapshot: None,
            error: None,
            busy: false,
            refresh_at: Instant::now(),
            show_snoozed: false,
        })
    }

    fn refresh(&mut self) {
        if !self.busy {
            match self.requests.try_send(self.selected.clone()) {
                Ok(()) => self.busy = true,
                Err(_) => {
                    self.error = Some(
                        "The service connection worker stopped. Restart the application.".into(),
                    )
                }
            }
        }
    }

    fn select(&mut self, target: Target) {
        self.selected = Some(target);
        self.error = None;
        if let Some(snapshot) = &mut self.snapshot {
            snapshot.contents = None;
        }
        self.refresh_at = Instant::now();
    }
}

impl App {
    fn show(&mut self, ui: &mut egui::Ui) {
        while let Ok((target, result)) = self.responses.try_recv() {
            self.busy = false;
            if target != self.selected {
                self.refresh_at = Instant::now();
                continue;
            }
            match result {
                Ok(snapshot) => {
                    self.snapshot = Some(snapshot);
                    self.error = None;
                }
                Err(error) => {
                    self.error = Some(error);
                    if let Some(snapshot) = &mut self.snapshot {
                        snapshot.contents = None;
                    }
                }
            }
            self.refresh_at = Instant::now() + Duration::from_secs(5);
        }
        if Instant::now() >= self.refresh_at {
            self.refresh();
        }
        ui.ctx().request_repaint_after(Duration::from_millis(250));
        let mut selection = None;
        egui::Panel::left("resources")
            .default_size(230.0)
            .show(ui, |ui| {
                ui.heading("Callboard");
                ui.label("Read-only preview");
                if ui
                    .add_enabled(!self.busy, egui::Button::new("Refresh"))
                    .clicked()
                {
                    self.refresh();
                }
                if self.busy {
                    ui.spinner();
                }
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.heading("Feeds");
                    if let Some(snapshot) = &self.snapshot {
                        if snapshot.feeds.is_empty() {
                            ui.label("No feeds yet");
                        }
                        for feed in &snapshot.feeds {
                            let target = Target::Feed(feed.name.clone());
                            let marker = if feed.error.is_some() {
                                " · error"
                            } else if stale(feed) {
                                " · stale"
                            } else {
                                ""
                            };
                            if ui
                                .selectable_label(
                                    self.selected.as_ref() == Some(&target),
                                    format!("{}{marker}", feed.title),
                                )
                                .on_hover_text(&feed.name)
                                .clicked()
                            {
                                selection = Some(target);
                            }
                        }
                        ui.separator();
                        ui.heading("Boards");
                        if snapshot.boards.is_empty() {
                            ui.label("No boards yet");
                        }
                        for board in &snapshot.boards {
                            let target = Target::Board(board.id);
                            if ui
                                .selectable_label(
                                    self.selected.as_ref() == Some(&target),
                                    &board.name,
                                )
                                .clicked()
                            {
                                selection = Some(target);
                            }
                        }
                    }
                    ui.separator();
                    if ui
                        .selectable_label(
                            self.selected == Some(Target::Archive),
                            "Deleted-board archive",
                        )
                        .clicked()
                    {
                        selection = Some(Target::Archive);
                    }
                });
            });
        if let Some(target) = selection {
            self.select(target);
        }
        egui::CentralPanel::default().show(ui,|ui| {
            if let Some(error) = &self.error {
                ui.colored_label(egui::Color32::LIGHT_RED,"Unable to refresh");
                ui.label(error);
                if self.auto_start_disabled {
                    ui.label("No service is running and auto-start is disabled. Start `callboard serve` or restart the GUI without `--no-auto-start`.");
                } else if self.auto_start.is_none() {
                    ui.label("The callboard service binary was not found. Install callboard next to callboard-gui or put it on PATH, then refresh.");
                } else {
                    ui.label("If the selected resource was deleted, select another entry.");
                }
                return;
            }
            match self.snapshot.as_ref().and_then(|s|s.contents.as_ref()) {
                Some(Contents::Feed(feed)) => show_feed(ui,feed,&mut self.show_snoozed),
                Some(Contents::Board(board)) => show_board(ui,board),
                Some(Contents::Missing) => { ui.heading("Source no longer exists"); ui.label("Select another feed or board."); },
                None => { ui.heading(if self.selected.is_some() {"Loading…"} else {"Select a feed or board"}); }
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
fn stale(feed: &callboard_core::store::FeedInfo) -> bool {
    feed.stale_after
        .as_deref()
        .and_then(|v| humantime::parse_duration(v).ok())
        .is_some_and(|limit| {
            (now_ms().saturating_sub(feed.last_submitted_at_ms).max(0) as u128) >= limit.as_millis()
        })
}
fn link(ui: &mut egui::Ui, url: Option<&str>) {
    if let Some(url) = url {
        if url.starts_with("https://") || url.starts_with("http://") {
            ui.hyperlink_to(url, url);
        } else {
            ui.label(url);
        }
    }
}
fn reference(ui: &mut egui::Ui, resolved: Option<&ResolvedReference>) {
    match resolved {
        Some(ResolvedReference::Live { item }) => {
            ui.label(format!("Current source: {}", item.title));
            link(ui, item.url.as_deref());
        }
        Some(ResolvedReference::SourceGone) => {
            ui.weak("Source gone");
        }
        None => (),
    }
}
fn show_feed(ui: &mut egui::Ui, feed: &Feed, show_snoozed: &mut bool) {
    ui.heading(&feed.info.title);
    let age = now_ms()
        .saturating_sub(feed.info.last_submitted_at_ms)
        .max(0) as u64;
    ui.label(format!(
        "{} items · submitted {} ago{}",
        feed.items.len(),
        humantime::format_duration(Duration::from_secs(age / 1000)),
        if stale(&feed.info) { " · stale" } else { "" }
    ));
    link(ui, feed.info.source_url.as_deref());
    if let Some(error) = &feed.info.error {
        ui.colored_label(egui::Color32::LIGHT_RED, &error.message);
    }
    let snoozed = feed.view_state.values().filter(|s| s.snoozed).count();
    ui.checkbox(show_snoozed, format!("Show snoozed ({snoozed})"));
    egui::ScrollArea::vertical()
        .id_salt("feed_items")
        .show(ui, |ui| {
            if feed.items.is_empty() {
                ui.label("This feed is empty.");
            }
            for item in &feed.items {
                let snoozed = feed.view_state.get(&item.key).is_some_and(|s| s.snoozed);
                if snoozed && !*show_snoozed {
                    continue;
                }
                ui.group(|ui| {
                    ui.strong(&item.title);
                    if snoozed {
                        ui.weak("Snoozed");
                    }
                    if let Some(body) = &item.body {
                        ui.label(body);
                    }
                    link(ui, item.url.as_deref());
                    if !item.tags.is_empty() {
                        ui.weak(item.tags.join(" · "));
                    }
                });
            }
        });
}
fn show_board(ui: &mut egui::Ui, board: &BoardContents) {
    ui.heading(
        board
            .board
            .as_ref()
            .map_or("Deleted-board archive", |b| b.name.as_str()),
    );
    ui.label(format!(
        "{} todos · {} notes",
        board.todos.len(),
        board.notes.len()
    ));
    egui::ScrollArea::vertical()
        .id_salt("board_items")
        .show(ui, |ui| {
            ui.heading("Todos");
            for todo in &board.todos {
                ui.group(|ui| {
                    let mut done = todo.item.done;
                    ui.add_enabled(false, egui::Checkbox::new(&mut done, &todo.item.title));
                    if let Some(body) = &todo.item.body {
                        ui.label(body);
                    }
                    link(ui, todo.item.url.as_deref());
                    reference(ui, todo.resolved_reference.as_ref());
                });
            }
            ui.heading("Notes");
            for note in &board.notes {
                ui.group(|ui| {
                    if let Some(title) = &note.item.title {
                        ui.strong(title);
                    }
                    ui.label(&note.item.body);
                    link(ui, note.item.url.as_deref());
                    reference(ui, note.resolved_reference.as_ref());
                });
            }
        });
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let paths = Paths::resolve(&Environment::current())?;
    let no_auto_start = std::env::args_os().any(|arg| arg == "--no-auto-start");
    let override_path = std::env::var_os("CALLBOARD_EXECUTABLE").map(PathBuf::from);
    let search_path = std::env::var_os("PATH").unwrap_or_default();
    let auto_start = find_service_executable(
        &std::env::current_exe()?,
        if no_auto_start {
            None
        } else {
            override_path.as_deref()
        },
        &search_path,
    );
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1000.0, 700.0]),
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_discards_late_responses_and_renders_without_a_display() {
        let (requests, incoming) = mpsc::sync_channel(1);
        let (outgoing, responses) = mpsc::sync_channel(1);
        let mut app = App {
            auto_start: None,
            auto_start_disabled: true,
            requests,
            responses,
            selected: Some(Target::Board(2)),
            snapshot: None,
            error: None,
            busy: true,
            refresh_at: Instant::now(),
            show_snoozed: false,
        };
        outgoing
            .send((Some(Target::Feed("old".into())), Err("stale error".into())))
            .unwrap();
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| app.show(ui));
        output.textures_delta.clear(); // Headless test has no texture renderer.
        assert!(!output.shapes.is_empty());
        assert!(app.error.is_none());
        assert!(app.busy);
        assert_eq!(incoming.try_recv().unwrap(), Some(Target::Board(2)));
        outgoing
            .send((
                Some(Target::Board(2)),
                Ok(Snapshot {
                    feeds: vec![],
                    boards: vec![],
                    contents: Some(Contents::Missing),
                }),
            ))
            .unwrap();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| app.show(ui));
        output.textures_delta.clear(); // Headless test has no texture renderer.
        assert!(!output.shapes.is_empty());
        assert!(!app.busy);
        assert!(matches!(
            app.snapshot.unwrap().contents,
            Some(Contents::Missing)
        ));
    }
}
