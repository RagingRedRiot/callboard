//! The desktop window: resource sidebar, layout bar, and panel tree.
//! The only write is saving layouts; feeds, boards, and items are read-only.
use crate::{
    backend::{self, Contents, Fetched, ListData, Request, Target},
    events::{self, Signal},
    sync::{Link, Outcome, POLL_INTERVAL, Scheduler},
    workspace::{self, LayoutEntry, LayoutKey, Layouts, Pane, Placement},
};
use callboard::lifecycle::Paths;
use callboard_core::{
    layout::{NamedLayout, Panel},
    store::{BoardContents, BoardInfo, Feed, FeedInfo, ResolvedReference},
};
use eframe::egui;
use egui_tiles::{Behavior, TileId, Tiles, UiResponse};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Last known service state. Contents are kept only for placed targets, so
/// every cached entry is covered by event invalidation.
#[derive(Default)]
pub struct Cache {
    pub feeds: Vec<FeedInfo>,
    pub boards: Vec<BoardInfo>,
    pub feeds_loaded: bool,
    pub boards_loaded: bool,
    pub contents: BTreeMap<Target, Entry>,
    /// The last failure to reach the service or read resource lists.
    pub error: Option<String>,
    pub refreshed_at: Option<Instant>,
}

/// A placed target's last loaded contents and its latest fetch failure. A
/// failed refetch keeps the old contents so an outage does not blank panels.
#[derive(Default)]
pub struct Entry {
    pub contents: Option<Contents>,
    pub error: Option<String>,
}

/// Visible and snoozed item counts for loaded contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub shown: usize,
    pub snoozed: usize,
}

impl Cache {
    pub fn lists_loaded(&self) -> bool {
        self.feeds_loaded && self.boards_loaded
    }

    /// Whether loaded board contents reference `feed`. Unloaded targets answer
    /// true, so a notice is never dropped for want of data.
    pub fn references(&self, target: &Target, feed: &str) -> bool {
        match self.contents.get(target).and_then(|e| e.contents.as_ref()) {
            Some(Contents::Board(board)) => {
                board
                    .todos
                    .iter()
                    .any(|t| t.item.reference.as_ref().is_some_and(|r| r.feed == feed))
                    || board
                        .notes
                        .iter()
                        .any(|n| n.item.reference.as_ref().is_some_and(|r| r.feed == feed))
            }
            Some(_) => false,
            None => true,
        }
    }

    fn feed(&self, name: &str) -> Option<&FeedInfo> {
        self.feeds.iter().find(|f| f.name == name)
    }

    fn board(&self, id: i64) -> Option<&BoardInfo> {
        self.boards.iter().find(|b| b.id == id)
    }

    /// Whether the lists show the target as deleted (not merely unloaded).
    pub fn gone(&self, target: &Target) -> bool {
        matches!(
            self.contents.get(target).and_then(|e| e.contents.as_ref()),
            Some(Contents::Missing)
        ) || match target {
            Target::Feed(name) => self.feeds_loaded && self.feed(name).is_none(),
            Target::Board(id) => self.boards_loaded && self.board(*id).is_none(),
            Target::Archive => false,
        }
    }

    pub fn title(&self, target: &Target) -> String {
        match target {
            Target::Feed(name) => self
                .feed(name)
                .map_or_else(|| name.clone(), |f| f.title.clone()),
            Target::Board(id) => self
                .board(*id)
                .map_or_else(|| format!("Board {id}"), |b| b.name.clone()),
            Target::Archive => "Deleted-board archive".into(),
        }
    }

    pub fn counts(&self, target: &Target) -> Option<Counts> {
        match self.contents.get(target)?.contents.as_ref()? {
            Contents::Feed(feed) => {
                let snoozed = feed
                    .items
                    .iter()
                    .filter(|i| feed.view_state.get(&i.key).is_some_and(|s| s.snoozed))
                    .count();
                Some(Counts {
                    shown: feed.items.len() - snoozed,
                    snoozed,
                })
            }
            Contents::Board(board) => Some(Counts {
                shown: board.todos.len() + board.notes.len(),
                snoozed: 0,
            }),
            Contents::Missing => None,
        }
    }

    /// A short status marker: deleted, error, or stale.
    pub fn marker(&self, target: &Target) -> Option<&'static str> {
        if self.gone(target) {
            return Some("deleted");
        }
        let Target::Feed(name) = target else {
            return None;
        };
        let feed = self.feed(name)?;
        if feed.error.is_some() {
            Some("error")
        } else if stale(feed) {
            Some("stale")
        } else {
            None
        }
    }

    fn tab_title(&self, target: &Target) -> String {
        let mut title = self.title(target);
        if let Some(counts) = self.counts(target) {
            title.push_str(&format!(" ({})", counts.shown));
        }
        if let Some(marker) = self.marker(target) {
            title.push_str(&format!(" · {marker}"));
        }
        title
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Reveal a placed target, or open it as a tab.
    Show(Target),
    Place(Target, Placement),
    /// Tile actions name the tree they came from: tile IDs restart in each
    /// tree, so an action queued before a layout switch or revert in the same
    /// frame must not hit a panel of the new tree.
    Retarget {
        tree: egui::Id,
        tile: TileId,
        target: Target,
    },
    Close {
        tree: egui::Id,
        tile: TileId,
    },
    Switch(LayoutKey),
    /// Replace the active layout's working copy with its saved version.
    Revert,
    Refresh,
    Prompt(PromptKind),
}

#[derive(Debug, Clone, PartialEq)]
pub enum SavePurpose {
    /// Auto-save of an existing layout's arrangement.
    Auto,
    /// "Save as…": store the arrangement of `from` under a new name.
    SaveAs { from: LayoutKey },
    /// "New layout": an empty layout.
    New,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SaveJob {
    pub name: String,
    pub tree: Panel,
    pub purpose: SavePurpose,
}

pub struct SaveDone {
    pub job: SaveJob,
    pub result: Result<NamedLayout, String>,
}

pub struct Channels {
    pub requests: mpsc::SyncSender<Request>,
    pub responses: mpsc::Receiver<Fetched>,
    pub signals: mpsc::Receiver<Signal>,
    pub saves: mpsc::SyncSender<SaveJob>,
    pub saved: mpsc::Receiver<SaveDone>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    SaveAs,
    New,
}

/// The layout-name dialog for "Save as…" and "New layout…".
pub struct NamePrompt {
    pub kind: PromptKind,
    pub text: String,
    pub error: Option<String>,
    /// Submitted; waiting for the service to store it.
    pub waiting: bool,
}

pub struct App {
    auto_start: Option<PathBuf>,
    auto_start_disabled: bool,
    channels: Channels,
    pub scheduler: Scheduler,
    pub layouts: Layouts,
    pub cache: Cache,
    pub prompt: Option<NamePrompt>,
    worker_error: Option<String>,
}

impl App {
    /// Start the fetch worker and the event subscriber, each on its own thread.
    pub fn new(
        paths: Paths,
        auto_start: Option<PathBuf>,
        auto_start_disabled: bool,
        ctx: egui::Context,
    ) -> Result<Self, String> {
        let runtime = || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())
        };
        let (requests, incoming) = mpsc::sync_channel::<Request>(1);
        let (outgoing, responses) = mpsc::sync_channel(1);
        let fetch_runtime = runtime()?;
        let fetch_paths = paths.clone();
        let fetch_auto_start = auto_start.clone();
        let fetch_ctx = ctx.clone();
        std::thread::Builder::new()
            .name("callboard-service-client".into())
            .spawn(move || {
                while let Ok(request) = incoming.recv() {
                    let fetched = fetch_runtime.block_on(backend::fetch(
                        &fetch_paths,
                        fetch_auto_start.as_deref(),
                        &request,
                    ));
                    if outgoing.send(fetched).is_err() {
                        break;
                    }
                    fetch_ctx.request_repaint();
                }
            })
            .map_err(|e| e.to_string())?;
        // Layout saves run on their own worker so a slow fetch batch never
        // delays them (and vice versa).
        let (saves, save_jobs) = mpsc::sync_channel::<SaveJob>(8);
        let (save_done, saved) = mpsc::sync_channel(8);
        let save_runtime = runtime()?;
        let save_paths = paths.clone();
        let save_auto_start = auto_start.clone();
        let save_ctx = ctx.clone();
        std::thread::Builder::new()
            .name("callboard-layout-saver".into())
            .spawn(move || {
                while let Ok(job) = save_jobs.recv() {
                    let result = save_runtime.block_on(backend::save_layout(
                        &save_paths,
                        save_auto_start.as_deref(),
                        &job.name,
                        &job.tree,
                    ));
                    if save_done.send(SaveDone { job, result }).is_err() {
                        break;
                    }
                    save_ctx.request_repaint();
                }
            })
            .map_err(|e| e.to_string())?;
        // Bounded: if the window stops draining, the subscriber blocks, the
        // service marks it lagged, and a resync follows once it catches up.
        let (signal_tx, signals) = mpsc::sync_channel(256);
        let events_runtime = runtime()?;
        std::thread::Builder::new()
            .name("callboard-events".into())
            .spawn(move || {
                events_runtime.block_on(events::run(&paths, events::Policy::default(), |signal| {
                    let open = signal_tx.send(signal).is_ok();
                    ctx.request_repaint();
                    open
                }))
            })
            .map_err(|e| e.to_string())?;
        Ok(Self::with_channels(
            auto_start,
            auto_start_disabled,
            Channels {
                requests,
                responses,
                signals,
                saves,
                saved,
            },
        ))
    }

    pub fn with_channels(
        auto_start: Option<PathBuf>,
        auto_start_disabled: bool,
        channels: Channels,
    ) -> Self {
        Self {
            auto_start,
            auto_start_disabled,
            channels,
            scheduler: Scheduler::new(Instant::now()),
            layouts: Layouts::new(),
            cache: Cache::default(),
            prompt: None,
            worker_error: None,
        }
    }

    fn receive(&mut self, now: Instant) {
        while let Ok(signal) = self.channels.signals.try_recv() {
            self.scheduler.signal(signal, now);
        }
        while let Ok(fetched) = self.channels.responses.try_recv() {
            self.apply_fetched(fetched, now);
        }
        while let Ok(done) = self.channels.saved.try_recv() {
            self.apply_saved(done, now);
        }
    }

    fn apply_fetched(&mut self, fetched: Fetched, now: Instant) {
        let mut outcome = Outcome::default();
        let mut succeeded = false;
        let mut list_error = None;
        let lists_fetched = !fetched.lists.is_empty();
        for (list, result) in fetched.lists {
            match result {
                Ok(data) => {
                    succeeded = true;
                    match data {
                        ListData::Feeds(feeds) => {
                            self.cache.feeds = feeds;
                            self.cache.feeds_loaded = true;
                        }
                        ListData::Boards(boards) => {
                            self.cache.boards = boards;
                            self.cache.boards_loaded = true;
                        }
                        ListData::Layouts(layouts) => self.layouts.sync_saved(&layouts),
                    }
                }
                Err(error) => {
                    outcome.lists_failed.push(list);
                    list_error = Some(error);
                }
            }
        }
        if lists_fetched {
            self.cache.error = list_error;
        }
        let open = self.layouts.active().targets();
        for (target, result) in fetched.targets {
            if let Ok(Contents::Feed(feed)) = &result {
                self.schedule_snooze_wake(&target, feed, now);
            }
            if result.is_err() {
                outcome.failed.push(target.clone());
            }
            // Responses for panels closed meanwhile would never be invalidated.
            if !open.contains(&target) {
                continue;
            }
            let entry = self.cache.contents.entry(target).or_default();
            match result {
                Ok(contents) => {
                    succeeded = true;
                    *entry = Entry {
                        contents: Some(contents),
                        error: None,
                    };
                }
                Err(error) => entry.error = Some(error),
            }
        }
        if succeeded {
            self.cache.refreshed_at = Some(now);
        }
        self.scheduler.finished(now, &outcome);
    }

    fn apply_saved(&mut self, done: SaveDone, now: Instant) {
        let SaveJob { name, purpose, .. } = done.job;
        let stored = done.result.map(|layout| layout.tree);
        match purpose {
            SavePurpose::Auto => self.layouts.save_finished(&name, stored, now),
            SavePurpose::SaveAs { from } => self.finish_prompt(Some(&from), &name, stored),
            SavePurpose::New => self.finish_prompt(None, &name, stored),
        }
    }

    fn finish_prompt(
        &mut self,
        from: Option<&LayoutKey>,
        name: &str,
        stored: Result<Panel, String>,
    ) {
        match stored {
            Ok(tree) => {
                self.layouts.adopt(from, name, &tree);
                self.prompt = None;
            }
            Err(error) => {
                if let Some(prompt) = &mut self.prompt {
                    prompt.waiting = false;
                    prompt.error = Some(error);
                }
            }
        }
    }

    /// Validate and submit the name dialog.
    pub fn submit_prompt(&mut self) {
        let Some(prompt) = &mut self.prompt else {
            return;
        };
        let name = prompt.text.trim().to_owned();
        if let Err(e) = callboard_core::layout::validate_name(&name) {
            prompt.error = Some(e.to_string());
            return;
        }
        if self.layouts.exists(&name) {
            prompt.error = Some(format!("A layout named “{name}” already exists"));
            return;
        }
        let job = match prompt.kind {
            PromptKind::SaveAs => SaveJob {
                name,
                tree: self.layouts.active().panel(),
                purpose: SavePurpose::SaveAs {
                    from: self.layouts.active_key().clone(),
                },
            },
            PromptKind::New => SaveJob {
                name,
                tree: Panel::Empty {},
                purpose: SavePurpose::New,
            },
        };
        match self.channels.saves.try_send(job) {
            Ok(()) => {
                prompt.waiting = true;
                prompt.error = None;
            }
            Err(_) => prompt.error = Some("The layout saver is busy or stopped; try again".into()),
        }
    }

    /// Snooze expiry emits no notice (DESIGN.md §8.2); refetch at the deadline.
    fn schedule_snooze_wake(&mut self, target: &Target, feed: &Feed, now: Instant) {
        let now_ms = now_ms();
        if let Some(until) = feed
            .view_state
            .values()
            .filter(|s| s.snoozed)
            .filter_map(|s| s.snoozed_until_ms)
            .filter(|until| *until > now_ms)
            .min()
        {
            let wait = Duration::from_millis((until - now_ms) as u64 + 50);
            self.scheduler.wake_at(target.clone(), now + wait);
        }
    }

    pub fn apply(&mut self, action: Action) {
        let workspace = self.layouts.active_mut();
        match action {
            Action::Show(target) => {
                workspace.show(target);
            }
            Action::Place(target, placement) => {
                workspace.open(target, placement);
            }
            Action::Retarget { tree, tile, target } if tree == workspace.tree.id() => {
                workspace.retarget(tile, target);
                workspace.focused = Some(tile);
            }
            Action::Close { tree, tile } if tree == workspace.tree.id() => workspace.close(tile),
            Action::Retarget { .. } | Action::Close { .. } => (),
            Action::Switch(key) => self.layouts.switch(key),
            Action::Revert => self.layouts.revert(),
            Action::Refresh => self.scheduler.refresh_all(),
            Action::Prompt(kind) => {
                self.prompt = Some(NamePrompt {
                    kind,
                    text: String::new(),
                    error: None,
                    waiting: false,
                })
            }
        }
    }

    /// Process incoming data and issue the next fetch, if any. Returns the
    /// targets placed in the active layout.
    pub fn tick(&mut self, now: Instant) -> BTreeSet<Target> {
        self.receive(now);
        self.layouts.active_mut().fix_focus();
        let open = self.layouts.active().targets();
        self.cache.contents.retain(|t, _| open.contains(t));
        for target in &open {
            if !self.cache.contents.contains_key(target) {
                self.scheduler.want(target.clone());
            }
        }
        let cache = &self.cache;
        if let Some(request) = self
            .scheduler
            .poll(now, &open, |t, feed| cache.references(t, feed))
            && let Err(error) = self.channels.requests.try_send(request)
        {
            let request = match error {
                mpsc::TrySendError::Full(r) | mpsc::TrySendError::Disconnected(r) => r,
            };
            self.scheduler.finished(now, &Outcome::all_failed(&request));
            self.worker_error =
                Some("The service connection worker stopped. Restart the application.".into());
        }
        for (name, tree) in self.layouts.due_saves(now) {
            let job = SaveJob {
                name: name.clone(),
                tree,
                purpose: SavePurpose::Auto,
            };
            if self.channels.saves.try_send(job).is_err() {
                let error = "The layout saver is busy or stopped".to_string();
                self.layouts.save_finished(&name, Err(error), now);
            }
        }
        open
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        let now = Instant::now();
        // Computed once per frame: both walk every working tree.
        let placed = self.tick(now);
        let entries = self.layouts.entries();
        let wait = self
            .scheduler
            .next_deadline(now)
            .into_iter()
            .chain(self.layouts.next_save_deadline(now))
            .min()
            .unwrap_or(Duration::MAX)
            .min(Duration::from_secs(1)); // Ages and stale markers tick.
        ui.ctx().request_repaint_after(wait);
        let mut actions = Vec::new();
        egui::Panel::top("layout_bar")
            .show(ui, |ui| self.layout_bar(ui, now, &entries, &mut actions));
        if let Some(error) = self.worker_error.clone().or(self.cache.error.clone()) {
            egui::Panel::top("error_bar").show(ui, |ui| self.error_bar(ui, &error));
        }
        egui::Panel::left("resources")
            .default_size(240.0)
            .show(ui, |ui| self.sidebar(ui, &entries, &placed, &mut actions));
        egui::CentralPanel::default().show(ui, |ui| self.panels(ui, &mut actions));
        self.prompt_window(ui);
        for action in actions {
            self.apply(action);
        }
    }

    fn layout_bar(
        &self,
        ui: &mut egui::Ui,
        now: Instant,
        entries: &[LayoutEntry],
        actions: &mut Vec<Action>,
    ) {
        ui.horizontal(|ui| {
            let entry = entries.iter().find(|e| e.active);
            ui.strong(format!("Layout: {}", self.layouts.active_key().label()));
            if let Some(entry) = entry {
                layout_status(ui, entry, actions);
            }
            ui.separator();
            if ui
                .small_button("Save as…")
                .on_hover_text("Save this arrangement as a new named layout")
                .clicked()
            {
                actions.push(Action::Prompt(PromptKind::SaveAs));
            }
            if ui
                .small_button("New layout…")
                .on_hover_text("Create an empty named layout")
                .clicked()
            {
                actions.push(Action::Prompt(PromptKind::New));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(!self.scheduler.busy(), egui::Button::new("Refresh"))
                    .clicked()
                {
                    actions.push(Action::Refresh);
                }
                if self.scheduler.busy() {
                    ui.spinner();
                }
                if let Some(at) = self.cache.refreshed_at {
                    ui.weak(format!(
                        "updated {} ago",
                        humantime::format_duration(Duration::from_secs(
                            now.duration_since(at).as_secs()
                        ))
                    ));
                }
                let (text, hover) = match self.scheduler.link(now) {
                    Link::Live => ("Live".to_string(), "Receiving change notices.".to_string()),
                    Link::Connecting => ("Connecting…".into(), "Opening the event stream.".into()),
                    Link::Polling => (
                        format!("Polling every {} s", POLL_INTERVAL.as_secs()),
                        format!(
                            "The event stream is unavailable{}; refreshing on a timer until it reconnects.",
                            self.scheduler
                                .stream_error()
                                .map(|e| format!(" ({e})"))
                                .unwrap_or_default()
                        ),
                    ),
                };
                ui.label(text).on_hover_text(hover);
                ui.separator();
                ui.weak("Read-only");
            });
        });
    }

    /// A dialog for naming a layout. Enter submits, Escape cancels.
    fn prompt_window(&mut self, ui: &mut egui::Ui) {
        let Some(prompt) = &mut self.prompt else {
            return;
        };
        let title = match prompt.kind {
            PromptKind::SaveAs => "Save layout as",
            PromptKind::New => "New layout",
        };
        let mut submit = false;
        let mut cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                let label = ui.label("Layout name");
                let edit = ui.add_enabled(
                    !prompt.waiting,
                    egui::TextEdit::singleline(&mut prompt.text).desired_width(260.0),
                );
                let edit = edit.labelled_by(label.id);
                if !prompt.waiting && !edit.has_focus() && prompt.text.is_empty() {
                    edit.request_focus();
                }
                if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
                if let Some(error) = &prompt.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                ui.horizontal(|ui| {
                    if prompt.waiting {
                        ui.spinner();
                        ui.label("Saving…");
                    } else {
                        submit |= ui.button("Save").clicked();
                        cancel |= ui.button("Cancel").clicked();
                    }
                });
            });
        if cancel && !prompt.waiting {
            self.prompt = None;
        } else if submit {
            self.submit_prompt();
        }
    }

    fn sidebar(
        &self,
        ui: &mut egui::Ui,
        entries: &[LayoutEntry],
        placed: &BTreeSet<Target>,
        actions: &mut Vec<Action>,
    ) {
        let workspace = self.layouts.active();
        let focused = workspace.focused_target().cloned();
        let has_focus = focused.is_some();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Layouts");
            for entry in entries {
                let mut text = entry.key.label().to_string();
                if entry.key == LayoutKey::Unsaved {
                    text.push_str(" (not saved)");
                } else if entry.save_error.is_some() {
                    text.push_str(" · save failed");
                }
                if ui.selectable_label(entry.active, text).clicked() && !entry.active {
                    actions.push(Action::Switch(entry.key.clone()));
                }
            }
            ui.separator();
            ui.heading("Feeds");
            if !self.cache.lists_loaded() {
                ui.weak("Loading…");
            } else if self.cache.feeds.is_empty() {
                ui.weak("No feeds yet");
            }
            for feed in &self.cache.feeds {
                let target = Target::Feed(feed.name.clone());
                self.entry(ui, &target, placed, &focused, has_focus, actions, |ui| {
                    ui.label(format!("Feed name: {}", feed.name));
                    ui.label(format!(
                        "Last submitted {} ago",
                        humantime::format_duration(age(feed.last_submitted_at_ms))
                    ));
                    if let Some(limit) = &feed.stale_after {
                        ui.label(format!("Stale after {limit}"));
                    }
                    if let Some(error) = &feed.error {
                        ui.colored_label(ui.visuals().error_fg_color, &error.message);
                    }
                });
            }
            ui.separator();
            ui.heading("Boards");
            if self.cache.lists_loaded() && self.cache.boards.is_empty() {
                ui.weak("No boards yet");
            }
            for board in &self.cache.boards {
                let target = Target::Board(board.id);
                self.entry(ui, &target, placed, &focused, has_focus, actions, |ui| {
                    ui.label(format!("Board ID {}", board.id));
                });
            }
            ui.separator();
            self.entry(
                ui,
                &Target::Archive,
                placed,
                &focused,
                has_focus,
                actions,
                |ui| {
                    ui.label("Items archived from deleted boards. Not saved in layouts.");
                },
            );
            ui.separator();
            ui.weak("Click to show. Right-click to open in a new tab or split.");
            ui.weak("Counts appear for placed feeds and boards.");
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn entry(
        &self,
        ui: &mut egui::Ui,
        target: &Target,
        placed: &BTreeSet<Target>,
        focused: &Option<Target>,
        has_focus: bool,
        actions: &mut Vec<Action>,
        details: impl FnOnce(&mut egui::Ui),
    ) {
        ui.horizontal(|ui| {
            let title = self.cache.title(target);
            let text = if placed.contains(target) {
                egui::RichText::new(title).strong()
            } else {
                egui::RichText::new(title)
            };
            let response = ui
                .selectable_label(focused.as_ref() == Some(target), text)
                .on_hover_ui(|ui| {
                    details(ui);
                    if placed.contains(target) {
                        ui.weak("Placed in this layout");
                    }
                });
            if response.clicked() {
                actions.push(Action::Show(target.clone()));
            }
            response.context_menu(|ui| {
                for (label, placement, enabled) in [
                    ("Open in new tab", Placement::Tab, true),
                    ("Split right", Placement::Right, true),
                    ("Split below", Placement::Below, true),
                    ("Show in focused panel", Placement::Replace, has_focus),
                ] {
                    if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                        actions.push(Action::Place(target.clone(), placement));
                        ui.close();
                    }
                }
            });
            if let Some(counts) = self.cache.counts(target) {
                ui.weak(counts.shown.to_string());
                if counts.snoozed > 0 {
                    ui.weak(format!("+{} snoozed", counts.snoozed));
                }
            }
            match self.cache.marker(target) {
                Some(marker @ "error") => {
                    ui.colored_label(ui.visuals().error_fg_color, marker);
                }
                Some(marker) => {
                    ui.colored_label(ui.visuals().warn_fg_color, marker);
                }
                None => (),
            }
        });
    }

    /// One line under the layout bar, so an outage does not shift the panels
    /// around; details and setup hints are on hover.
    fn error_bar(&self, ui: &mut egui::Ui, error: &str) {
        let hint = if self.auto_start_disabled {
            "No service is running and auto-start is disabled. Start `callboard serve` or restart the GUI without `--no-auto-start`."
        } else if self.auto_start.is_none() {
            "The callboard service binary was not found. Install callboard next to callboard-gui or put it on PATH, then refresh."
        } else {
            "Retrying automatically."
        };
        ui.horizontal(|ui| {
            ui.colored_label(ui.visuals().error_fg_color, "Unable to refresh:");
            let first_line = error.lines().next().unwrap_or_default();
            ui.add(egui::Label::new(first_line).truncate())
                .on_hover_text(format!("{error}\n\n{hint}"));
            if self.cache.lists_loaded() {
                ui.weak("showing the last loaded data");
            }
        });
    }

    fn panels(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let workspace = self.layouts.active_mut();
        if workspace.tree.is_empty() {
            ui.heading("No panels in this layout");
            ui.label("Click a feed or board in the sidebar to show it here. Right-click an entry to open it in a new tab or split.");
            return;
        }
        let tree_id = workspace.tree.id();
        let mut behavior = Panes {
            cache: &self.cache,
            focused: workspace.focused,
            clicked: None,
            actions,
            tree_id,
        };
        workspace.tree.ui(&mut behavior, ui);
        if let Some(tile) = behavior.clicked {
            workspace.focused = Some(tile);
        }
        workspace.fix_focus();
    }
}

/// The other ends of an app's worker channels, for tests that play the
/// service's part.
#[cfg(test)]
pub(crate) struct TestEnds {
    pub requests: mpsc::Receiver<Request>,
    pub responses: mpsc::SyncSender<Fetched>,
    pub signals: mpsc::SyncSender<Signal>,
    pub saves: mpsc::Receiver<SaveJob>,
    pub saved: mpsc::SyncSender<SaveDone>,
}

#[cfg(test)]
impl App {
    pub(crate) fn for_tests() -> (Self, TestEnds) {
        let (requests, incoming) = mpsc::sync_channel(1);
        let (outgoing, responses) = mpsc::sync_channel(1);
        let (signal_tx, signals) = mpsc::sync_channel(16);
        let (saves, save_jobs) = mpsc::sync_channel(8);
        let (save_done, saved) = mpsc::sync_channel(8);
        // No stream in tests: fetch at once instead of waiting for it.
        signal_tx
            .send(Signal::Disconnected(Some("no stream in tests".into())))
            .unwrap();
        let app = Self::with_channels(
            None,
            true,
            Channels {
                requests,
                responses,
                signals,
                saves,
                saved,
            },
        );
        let ends = TestEnds {
            requests: incoming,
            responses: outgoing,
            signals: signal_tx,
            saves: save_jobs,
            saved: save_done,
        };
        (app, ends)
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

struct Panes<'a> {
    cache: &'a Cache,
    focused: Option<TileId>,
    clicked: Option<TileId>,
    actions: &'a mut Vec<Action>,
    tree_id: egui::Id,
}

impl Behavior<Pane> for Panes<'_> {
    fn tab_title_for_pane(&mut self, pane: &Pane) -> egui::WidgetText {
        self.cache.tab_title(&pane.target).into()
    }

    fn pane_ui(&mut self, ui: &mut egui::Ui, tile: TileId, pane: &mut Pane) -> UiResponse {
        if ui.ui_contains_pointer() && ui.input(|i| i.pointer.primary_pressed()) {
            self.clicked = Some(tile);
        }
        egui::Frame::new().inner_margin(6.0).show(ui, |ui| {
            // Keyed by target too: a retargeted panel starts with fresh state.
            let salt = tile.egui_id(self.tree_id).with(&pane.target);
            let entry = self.cache.contents.get(&pane.target);
            let contents = entry.and_then(|e| e.contents.as_ref());
            if let Some(error) = entry.and_then(|e| e.error.as_ref()) {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    if contents.is_some() {
                        "Unable to refresh; showing the last loaded contents"
                    } else {
                        "Unable to load"
                    },
                );
                ui.label(error);
                ui.separator();
            }
            match contents {
                None if entry.is_some_and(|e| e.error.is_some()) => (),
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Loading…");
                    });
                }
                Some(Contents::Missing) => placeholder(ui, &pane.target),
                Some(Contents::Feed(feed)) => show_feed(ui, feed, salt),
                Some(Contents::Board(board)) => show_board(ui, board, salt),
            }
        });
        UiResponse::None
    }

    fn is_tab_closable(&self, tiles: &Tiles<Pane>, tile: TileId) -> bool {
        tiles.get(tile).is_some_and(|t| t.is_pane())
    }

    fn on_tab_close(&mut self, _tiles: &mut Tiles<Pane>, tile: TileId) -> bool {
        // Close through the workspace so focus and containers stay consistent.
        self.actions.push(Action::Close {
            tree: self.tree_id,
            tile,
        });
        false
    }

    fn on_tab_button(
        &mut self,
        tiles: &mut Tiles<Pane>,
        tile: TileId,
        response: egui::Response,
    ) -> egui::Response {
        if response.clicked() && tiles.get(tile).is_some_and(|t| t.is_pane()) {
            self.clicked = Some(tile);
        }
        response
    }

    fn paint_on_top_of_tile(
        &self,
        painter: &egui::Painter,
        style: &egui::Style,
        tile: TileId,
        rect: egui::Rect,
    ) {
        if self.focused == Some(tile) {
            painter.rect_stroke(
                rect,
                0.0,
                style.visuals.selection.stroke,
                egui::StrokeKind::Inside,
            );
        }
    }

    fn top_bar_right_ui(
        &mut self,
        tiles: &Tiles<Pane>,
        ui: &mut egui::Ui,
        _tile: TileId,
        tabs: &egui_tiles::Tabs,
        _scroll_offset: &mut f32,
    ) {
        let Some(active) = tabs.active else { return };
        if let Some(egui_tiles::Tile::Pane(pane)) = tiles.get(active) {
            let target = pane.target.clone();
            self.retarget_menu(ui, active, &target);
        }
    }

    fn simplification_options(&self) -> egui_tiles::SimplificationOptions {
        workspace::simplification()
    }
}

impl Panes<'_> {
    /// The retarget menu for a tab group's active panel, in its tab bar.
    fn retarget_menu(&mut self, ui: &mut egui::Ui, tile: TileId, current: &Target) {
        let mut chosen = None;
        egui::ComboBox::from_id_salt(("retarget", self.tree_id, tile))
            .selected_text("Show…")
            .show_ui(ui, |ui| {
                // Prefixed: a feed and a board may share a title.
                let choices = self
                    .cache
                    .feeds
                    .iter()
                    .map(|f| (Target::Feed(f.name.clone()), format!("Feed: {}", f.title)))
                    .chain(
                        self.cache
                            .boards
                            .iter()
                            .map(|b| (Target::Board(b.id), format!("Board: {}", b.name))),
                    )
                    .chain([(Target::Archive, "Deleted-board archive".to_string())]);
                for (target, title) in choices {
                    if ui.selectable_label(&target == current, title).clicked() {
                        chosen = Some(target);
                    }
                }
            })
            .response
            .on_hover_text("Point this panel at another feed or board");
        if let Some(target) = chosen.filter(|t| t != current) {
            self.actions.push(Action::Retarget {
                tree: self.tree_id,
                tile,
                target,
            });
        }
    }
}

/// Save state of the active layout, for the layout bar.
fn layout_status(ui: &mut egui::Ui, entry: &LayoutEntry, actions: &mut Vec<Action>) {
    if entry.key == LayoutKey::Unsaved {
        ui.weak("not saved; use Save as… to keep it");
        return;
    }
    if let Some(error) = &entry.save_error {
        ui.colored_label(ui.visuals().error_fg_color, "not saved (retrying)")
            .on_hover_text(error);
    } else if entry.saving || entry.pending {
        ui.weak("saving…");
    } else {
        ui.weak("saved");
    }
    if entry.unsaveable {
        ui.weak("· the archive panel is not saved in layouts");
    }
    if entry.outdated && !entry.pending {
        ui.colored_label(ui.visuals().warn_fg_color, "changed in another window");
        if ui
            .small_button("Load saved version")
            .on_hover_text("Replace this window's arrangement with the saved one")
            .clicked()
        {
            actions.push(Action::Revert);
        }
    }
}

fn placeholder(ui: &mut egui::Ui, target: &Target) {
    ui.heading(match target {
        Target::Feed(name) => format!("Feed “{name}” no longer exists"),
        Target::Board(id) => format!("Board {id} no longer exists"),
        Target::Archive => "The archive is unavailable".into(),
    });
    ui.label(
        "This panel keeps its place in the layout. Use Show… in the tab bar to point it at another feed or board, or close its tab.",
    );
    if let Target::Feed(_) = target {
        ui.weak("A new submission under the same name will appear here.");
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn age(at_ms: i64) -> Duration {
    Duration::from_secs(now_ms().saturating_sub(at_ms).max(0) as u64 / 1000)
}

fn stale(feed: &FeedInfo) -> bool {
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

fn show_feed(ui: &mut egui::Ui, feed: &Feed, salt: egui::Id) {
    ui.label(format!(
        "{} items · submitted {} ago{}",
        feed.items.len(),
        humantime::format_duration(age(feed.info.last_submitted_at_ms)),
        if stale(&feed.info) { " · stale" } else { "" }
    ));
    link(ui, feed.info.source_url.as_deref());
    if let Some(error) = &feed.info.error {
        ui.colored_label(ui.visuals().error_fg_color, &error.message);
    }
    let snoozed = feed
        .items
        .iter()
        .filter(|i| feed.view_state.get(&i.key).is_some_and(|s| s.snoozed))
        .count();
    let toggle = salt.with("show_snoozed");
    let mut show_snoozed = ui.data(|d| d.get_temp::<bool>(toggle)).unwrap_or(false);
    if ui
        .checkbox(&mut show_snoozed, format!("Show snoozed ({snoozed})"))
        .changed()
    {
        ui.data_mut(|d| d.insert_temp(toggle, show_snoozed));
    }
    egui::ScrollArea::vertical()
        .id_salt(salt.with("feed_items"))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if feed.items.is_empty() {
                ui.label("This feed is empty.");
            }
            for item in &feed.items {
                let snoozed = feed.view_state.get(&item.key).is_some_and(|s| s.snoozed);
                if snoozed && !show_snoozed {
                    continue;
                }
                ui.group(|ui| {
                    ui.set_width(ui.available_width());
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

fn show_board(ui: &mut egui::Ui, board: &BoardContents, salt: egui::Id) {
    let open = board.todos.iter().filter(|t| !t.item.done).count();
    ui.label(format!(
        "{} todos ({open} open) · {} notes",
        board.todos.len(),
        board.notes.len()
    ));
    egui::ScrollArea::vertical()
        .id_salt(salt.with("board_items"))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.strong("Todos");
            if board.todos.is_empty() {
                ui.weak("No todos");
            }
            for todo in &board.todos {
                ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    let mut done = todo.item.done;
                    ui.add_enabled(false, egui::Checkbox::new(&mut done, &todo.item.title));
                    if let Some(body) = &todo.item.body {
                        ui.label(body);
                    }
                    link(ui, todo.item.url.as_deref());
                    reference(ui, todo.resolved_reference.as_ref());
                });
            }
            ui.strong("Notes");
            if board.notes.is_empty() {
                ui.weak("No notes");
            }
            for note in &board.notes {
                ui.group(|ui| {
                    ui.set_width(ui.available_width());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{backend::List, events::Notice};

    struct Lists {
        feeds: Vec<FeedInfo>,
        boards: Vec<BoardInfo>,
        layouts: Vec<NamedLayout>,
    }

    fn all_lists(lists: Lists) -> Vec<(List, Result<ListData, String>)> {
        vec![
            (List::Feeds, Ok(ListData::Feeds(lists.feeds))),
            (List::Boards, Ok(ListData::Boards(lists.boards))),
            (List::Layouts, Ok(ListData::Layouts(lists.layouts))),
        ]
    }
    use callboard_core::{feed::Item, layout::NamedLayout};
    use serde_json::json;

    struct Harness {
        app: App,
        requests: mpsc::Receiver<Request>,
        responses: mpsc::SyncSender<Fetched>,
        signals: mpsc::SyncSender<Signal>,
        saves: mpsc::Receiver<SaveJob>,
        saved: mpsc::SyncSender<SaveDone>,
        ctx: egui::Context,
    }

    impl Harness {
        fn new() -> Self {
            let (app, ends) = App::for_tests();
            Self {
                app,
                requests: ends.requests,
                responses: ends.responses,
                signals: ends.signals,
                saves: ends.saves,
                saved: ends.saved,
                ctx: egui::Context::default(),
            }
        }

        fn frame(&mut self) {
            let mut output = self
                .ctx
                .run_ui(egui::RawInput::default(), |ui| self.app.show(ui));
            output.textures_delta.clear(); // Headless test has no texture renderer.
            assert!(!output.shapes.is_empty());
        }

        fn request(&self) -> Request {
            self.requests.try_recv().expect("a fetch request")
        }
    }

    fn feed_info(name: &str) -> FeedInfo {
        FeedInfo {
            name: name.into(),
            title: format!("{name} title"),
            source_url: None,
            stale_after: None,
            last_submitted_at_ms: now_ms(),
            error: None,
        }
    }

    fn feed(name: &str, keys: &[&str]) -> Contents {
        Contents::Feed(Feed {
            info: feed_info(name),
            items: keys
                .iter()
                .map(|k| serde_json::from_value::<Item>(json!({"key":k,"title":k})).unwrap())
                .collect(),
            manual_order: false,
            view_state: Default::default(),
        })
    }

    #[test]
    fn opens_saved_layout_keeps_deleted_targets_as_placeholders_and_refetches_on_resync() {
        let mut h = Harness::new();
        h.frame();
        // Initial load before any stream: lists first (they carry layouts).
        assert_eq!(
            h.request(),
            Request {
                lists: List::ALL.into_iter().collect(),
                targets: vec![]
            }
        );
        h.responses
            .send(Fetched {
                lists: all_lists(Lists {
                    feeds: vec![feed_info("reviews")],
                    boards: vec![],
                    layouts: vec![NamedLayout {
                        name: "Day".into(),
                        tree: serde_json::from_value(json!({"kind":"split","axis":"horizontal",
                            "weights":[1.0,1.0],"children":[{"kind":"feed","name":"reviews"},
                            {"kind":"board","id":4}]}))
                        .unwrap(),
                        updated_at_ms: 0,
                    }],
                }),
                targets: vec![],
            })
            .unwrap();
        h.frame();
        assert_eq!(h.app.layouts.active_key(), &LayoutKey::Saved("Day".into()));
        let first = h.request();
        assert!(first.lists.is_empty());
        assert_eq!(
            first.targets,
            vec![Target::Feed("reviews".into()), Target::Board(4)]
        );
        h.responses
            .send(Fetched {
                lists: vec![],
                targets: vec![
                    (
                        Target::Feed("reviews".into()),
                        Ok(feed("reviews", &["a", "b"])),
                    ),
                    (Target::Board(4), Ok(Contents::Missing)),
                ],
            })
            .unwrap();
        h.frame();
        let cache = &h.app.cache;
        assert_eq!(
            cache.counts(&Target::Feed("reviews".into())),
            Some(Counts {
                shown: 2,
                snoozed: 0
            })
        );
        assert_eq!(cache.marker(&Target::Board(4)), Some("deleted"));
        assert_eq!(cache.tab_title(&Target::Board(4)), "Board 4 · deleted");
        // The deleted board's panel survives in the layout.
        assert!(h.app.layouts.active().targets().contains(&Target::Board(4)));
        assert!(h.requests.try_recv().is_err(), "nothing more to fetch");
        // A mid-stream resync (lag) refetches lists and every placed target.
        h.signals.send(Signal::Connected).unwrap();
        h.signals.send(Signal::Notice(Notice::Resync)).unwrap();
        h.frame();
        let resync = h.request();
        assert_eq!(resync.lists.len(), 3);
        assert_eq!(resync.targets.len(), 2);
    }

    #[test]
    fn sidebar_actions_arrange_panels_and_drop_contents_of_closed_ones() {
        let mut h = Harness::new();
        h.frame();
        h.request();
        h.responses
            .send(Fetched {
                lists: all_lists(Lists {
                    feeds: vec![feed_info("a"), feed_info("b")],
                    boards: vec![BoardInfo {
                        id: 2,
                        name: "Inbox".into(),
                    }],
                    layouts: vec![],
                }),
                targets: vec![],
            })
            .unwrap();
        h.frame();
        assert_eq!(h.app.layouts.active_key(), &LayoutKey::Unsaved);
        h.app.apply(Action::Show(Target::Feed("a".into())));
        h.app
            .apply(Action::Place(Target::Board(2), Placement::Right));
        h.app
            .apply(Action::Place(Target::Feed("b".into()), Placement::Tab));
        h.frame();
        let request = h.request();
        assert_eq!(request.targets.len(), 3);
        let panel = h.app.layouts.active().panel();
        assert_eq!(
            panel,
            serde_json::from_value(
                json!({"kind":"split","axis":"horizontal","weights":[1.0,1.0],
                "children":[{"kind":"feed","name":"a"},{"kind":"tabs","active":1,"children":[
                    {"kind":"board","id":2},{"kind":"feed","name":"b"}]}]})
            )
            .unwrap()
        );
        h.responses
            .send(Fetched {
                lists: vec![],
                targets: request
                    .targets
                    .iter()
                    .map(|t| (t.clone(), Ok(feed("x", &["1"]))))
                    .collect(),
            })
            .unwrap();
        h.frame();
        assert_eq!(h.app.cache.contents.len(), 3);
        // A failed refetch keeps what was loaded and retries only that panel.
        h.signals.send(Signal::Connected).unwrap();
        h.signals.send(Signal::Notice(Notice::Resync)).unwrap();
        h.frame();
        let resync = h.request();
        h.responses
            .send(Fetched {
                lists: vec![],
                targets: resync
                    .targets
                    .iter()
                    .map(|t| {
                        let result = match t {
                            Target::Board(2) => Err("HTTP 500".to_string()),
                            _ => Ok(feed("x", &["1", "2"])),
                        };
                        (t.clone(), result)
                    })
                    .collect(),
            })
            .unwrap();
        h.frame();
        let board = &h.app.cache.contents[&Target::Board(2)];
        assert_eq!(board.error.as_deref(), Some("HTTP 500"));
        assert!(
            matches!(board.contents, Some(Contents::Feed(_))),
            "old data kept"
        );
        assert_eq!(
            h.app.cache.counts(&Target::Feed("a".into())).unwrap().shown,
            2,
            "successful parts still apply"
        );
        assert!(h.app.cache.refreshed_at.is_some());
        assert!(
            h.requests.try_recv().is_err(),
            "failed panel waits for its retry"
        );
        let focused = h.app.layouts.active().focused.unwrap();
        let tree = h.app.layouts.active().tree.id();
        h.app.apply(Action::Close {
            tree,
            tile: focused,
        });
        h.frame();
        assert_eq!(h.app.cache.contents.len(), 2, "closed panel's data dropped");
        assert_eq!(
            h.app.layouts.active().focused_target(),
            Some(&Target::Board(2))
        );
        // Retargeting fetches the new target for the same panel.
        let tile = h.app.layouts.active().focused.unwrap();
        h.app.apply(Action::Retarget {
            tree,
            tile,
            target: Target::Archive,
        });
        h.frame();
        assert_eq!(h.request().targets, vec![Target::Archive]);
    }

    #[test]
    fn tile_actions_from_a_replaced_tree_are_ignored() {
        let mut h = Harness::new();
        h.app.layouts.sync_saved(&[NamedLayout {
            name: "Day".into(),
            tree: serde_json::from_value(json!({"kind":"feed","name":"a"})).unwrap(),
            updated_at_ms: 0,
        }]);
        let tree = h.app.layouts.active().tree.id();
        let tile = h.app.layouts.active().focused.unwrap();
        // Same frame: a revert rebuilds the tree, then a queued close arrives.
        h.app.apply(Action::Revert);
        h.app.apply(Action::Close { tree, tile });
        assert_eq!(h.app.layouts.active().targets().len(), 1, "panel kept");
    }

    #[test]
    fn arrangement_changes_auto_save_through_the_saver_and_settle() {
        let mut h = Harness::new();
        h.app.layouts.sync_saved(&[NamedLayout {
            name: "Day".into(),
            tree: serde_json::from_value(json!({"kind":"feed","name":"a"})).unwrap(),
            updated_at_ms: 0,
        }]);
        h.app
            .apply(Action::Place(Target::Board(2), Placement::Right));
        let t0 = Instant::now();
        h.app.tick(t0);
        assert!(
            h.saves.try_recv().is_err(),
            "waits for the change to settle"
        );
        h.app.tick(t0 + workspace::SAVE_DELAY);
        let job = h.saves.try_recv().expect("an auto-save");
        assert_eq!(job.name, "Day");
        assert_eq!(job.purpose, SavePurpose::Auto);
        assert!(matches!(job.tree, Panel::Split { .. }));
        h.saved
            .send(SaveDone {
                result: Ok(NamedLayout {
                    name: job.name.clone(),
                    tree: job.tree.clone(),
                    updated_at_ms: 1,
                }),
                job,
            })
            .unwrap();
        h.app.tick(t0 + workspace::SAVE_DELAY * 2);
        let day = h
            .app
            .layouts
            .entries()
            .into_iter()
            .find(|e| e.active)
            .unwrap();
        assert!(!day.pending && !day.saving && day.save_error.is_none());
        assert!(h.saves.try_recv().is_err(), "saved once");
        let _ = &h.saved;
    }
}
