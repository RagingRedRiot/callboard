//! The desktop window: resource sidebar, layout bar, and canvas of cards.
//! Writes manage layouts, snooze or promote feed items, and edit boards.
use crate::{
    backend::{self, Contents, Fetched, ListData, PromoteKind, Request, Target},
    board::{self, BoardOp, Kind},
    events::{self, Signal},
    feed,
    quick::{self, Choice, QuickOpen},
    sync::{Link, Outcome, POLL_INTERVAL, Scheduler},
    theme::{self, Palette, icon},
    workspace::{self, CardState, LayoutEntry, LayoutKey, Layouts, TITLE_HEIGHT},
};
use callboard_core::{
    layout::{Layout, NamedLayout},
    store::{BoardInfo, BoardSummary, Feed, FeedInfo, FeedSummary},
};
use callboard_service::lifecycle::Paths;
use eframe::egui;
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
    pub feeds: Vec<FeedSummary>,
    pub boards: Vec<BoardSummary>,
    pub feeds_loaded: bool,
    pub boards_loaded: bool,
    pub contents: BTreeMap<Target, Entry>,
    /// The last failure to reach the service or read resource lists.
    pub error: Option<String>,
    pub refreshed_at: Option<Instant>,
}

/// A placed target's last loaded contents and its latest fetch failure. A
/// failed refetch keeps the old contents so an outage does not blank cards.
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
    /// New or updated now (DESIGN.md §4.3).
    pub marked: usize,
}

impl Cache {
    pub fn lists_loaded(&self) -> bool {
        self.feeds_loaded && self.boards_loaded
    }

    /// Whether loaded board contents reference `feed`. Unloaded targets answer
    /// true, so a notice is never dropped for want of data.
    pub fn references(&self, target: &Target, feed: &str) -> bool {
        match self.contents.get(target).and_then(|e| e.contents.as_ref()) {
            Some(Contents::Board { items, archived }) => {
                std::iter::once(items).chain(archived).any(|board| {
                    board
                        .todos
                        .iter()
                        .any(|t| t.item.reference.as_ref().is_some_and(|r| r.feed == feed))
                        || board
                            .notes
                            .iter()
                            .any(|n| n.item.reference.as_ref().is_some_and(|r| r.feed == feed))
                })
            }
            Some(_) => false,
            None => true,
        }
    }

    fn feed(&self, name: &str) -> Option<&FeedInfo> {
        self.feeds.iter().map(|f| &f.info).find(|f| f.name == name)
    }

    fn feed_summary(&self, name: &str) -> Option<&FeedSummary> {
        self.feeds.iter().find(|f| f.info.name == name)
    }

    fn board(&self, id: i64) -> Option<&BoardSummary> {
        self.boards.iter().find(|b| b.info.id == id)
    }

    /// A board target's color, from the board list.
    pub fn board_color(&self, target: &Target) -> Option<&str> {
        match target {
            Target::Board(id) => self.board(*id)?.info.color.as_deref(),
            _ => None,
        }
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
                .map_or_else(|| format!("Board {id}"), |b| b.info.name.clone()),
            Target::Archive => "Deleted-board archive".into(),
        }
    }

    /// Counts from loaded contents when placed, otherwise from the lists.
    pub fn counts(&self, target: &Target) -> Option<Counts> {
        let Some(contents) = self.contents.get(target).and_then(|e| e.contents.as_ref()) else {
            return match target {
                Target::Feed(name) => self.feed_summary(name).map(|f| Counts {
                    shown: f.item_count.saturating_sub(f.snoozed_count),
                    snoozed: f.snoozed_count,
                    marked: f.new_count,
                }),
                Target::Board(id) => self.board(*id).map(|b| Counts {
                    shown: b.todo_count + b.note_count,
                    snoozed: 0,
                    marked: 0,
                }),
                Target::Archive => None,
            };
        };
        match contents {
            Contents::Feed(feed) => {
                let snoozed = feed
                    .items
                    .iter()
                    .filter(|i| feed.view_state.get(&i.key).is_some_and(|s| s.snoozed))
                    .count();
                Some(Counts {
                    shown: feed.items.len() - snoozed,
                    snoozed,
                    marked: feed::marked(feed).count(),
                })
            }
            Contents::Board { items: board, .. } => Some(Counts {
                shown: board.todos.len() + board.notes.len(),
                snoozed: 0,
                marked: 0,
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

    /// The card's title as drawn: name, a muted count, and a status mark.
    /// [`Cache::card_title`] is the same as plain text, its accessible name.
    fn card_heading(&self, target: &Target, p: &Palette) -> egui::text::LayoutJob {
        let mut job = egui::text::LayoutJob::default();
        let font = |size, family| egui::FontId::new(size, family);
        let format = |font, color| egui::TextFormat::simple(font, color);
        job.append(
            &self.title(target),
            0.0,
            format(font(14.0, theme::medium()), p.text),
        );
        if let Some(counts) = self.counts(target) {
            job.append(
                &counts.shown.to_string(),
                8.0,
                format(font(13.0, egui::FontFamily::Proportional), p.faint),
            );
        }
        if let Some(marker) = self.marker(target) {
            let color = if marker == "error" {
                p.danger
            } else {
                p.warning
            };
            job.append(marker, 8.0, format(font(12.0, theme::medium()), color));
        }
        job
    }

    fn card_title(&self, target: &Target) -> String {
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

/// A change to one card, from its own controls on the canvas.
#[derive(Debug, Clone, PartialEq)]
pub enum CardOp {
    Raise,
    Move(egui::Vec2),
    /// The card's new expanded size; the minimum is enforced.
    Resize(egui::Vec2),
    ToggleCollapsed,
    Close,
    Retarget(Target),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Reveal the target's card, or place one in the middle of the view.
    Show(Target),
    /// Place the target's card with its top-left at a canvas point (a drop
    /// from the sidebar); an existing card moves there.
    PlaceAt(Target, egui::Pos2),
    /// Canvas actions name the canvas they came from, so one queued before a
    /// layout switch or revert in the same frame never hits the new canvas.
    Card {
        canvas: egui::Id,
        target: Target,
        op: CardOp,
    },
    Pan {
        canvas: egui::Id,
        delta: egui::Vec2,
    },
    ShowAll,
    Switch(LayoutKey),
    /// Replace the active layout's working copy with its saved version.
    Revert,
    Refresh,
    Write(WriteOp),
    DismissWriteError,
    Prompt(PromptKind),
    /// Ask to confirm a deletion.
    ConfirmDelete(DeleteSubject),
    QuickOpen,
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
    pub layout: Layout,
    pub purpose: SavePurpose,
}

pub struct SaveDone {
    pub job: SaveJob,
    pub result: Result<NamedLayout, String>,
}

/// Writes other than layout saves, sent in order on their own worker.
#[derive(Debug, Clone, PartialEq)]
pub enum WriteOp {
    Rename {
        from: String,
        to: String,
    },
    Delete {
        name: String,
    },
    /// The layout to open at next startup; `None` for the Unsaved arrangement.
    Remember {
        name: Option<String>,
    },
    /// Set both snooze conditions; neither set unsnoozes the item.
    Snooze {
        feed: String,
        key: String,
        until_ms: Option<i64>,
        on_update: bool,
    },
    Promote {
        feed: String,
        key: String,
        board_id: i64,
        kind: PromoteKind,
    },
    /// Move an item to `position` in the feed's whole order (§4.2).
    Reorder {
        feed: String,
        key: String,
        position: usize,
    },
    /// Return the feed to the tool's order; `key` is any of its items.
    ResetOrder {
        feed: String,
        key: String,
    },
    Board(BoardOp),
}

/// What a successful write returned, where the window uses it.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Done,
    /// A renamed layout.
    Layout(NamedLayout),
    /// A created board.
    Board(BoardInfo),
}

pub struct OpDone {
    pub op: WriteOp,
    pub result: Result<Reply, String>,
}

pub struct Channels {
    pub requests: mpsc::SyncSender<Request>,
    pub responses: mpsc::Receiver<Fetched>,
    pub signals: mpsc::Receiver<Signal>,
    pub saves: mpsc::SyncSender<SaveJob>,
    pub saved: mpsc::Receiver<SaveDone>,
    pub ops: mpsc::SyncSender<WriteOp>,
    pub ops_done: mpsc::Receiver<OpDone>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    SaveAs,
    New,
    Rename,
    NewBoard,
    RenameBoard(i64),
}

impl PromptKind {
    fn board(self) -> bool {
        matches!(self, PromptKind::NewBoard | PromptKind::RenameBoard(_))
    }
}

/// Something deleted only after confirmation.
#[derive(Debug, Clone, PartialEq)]
pub enum DeleteSubject {
    /// A saved layout.
    Layout(String),
    Board {
        id: i64,
        name: String,
    },
    /// A todo or note on `board` (1 for the deleted-board archive).
    Item {
        kind: Kind,
        id: i64,
        board: i64,
        name: String,
    },
}

pub struct DeletePrompt {
    pub subject: DeleteSubject,
    pub error: Option<String>,
    pub waiting: bool,
}

/// The name dialog: "Save as…", "New layout…", and "Rename…" for layouts,
/// "New board…" and "Rename…" for boards.
pub struct NamePrompt {
    pub kind: PromptKind,
    pub text: String,
    pub error: Option<String>,
    /// Submitted; waiting for the service to store it.
    pub waiting: bool,
    /// Select the whole name on the next frame, so typing replaces it.
    pub select_all: bool,
}

pub struct App {
    auto_start: Option<PathBuf>,
    auto_start_disabled: bool,
    channels: Channels,
    pub scheduler: Scheduler,
    pub layouts: Layouts,
    pub cache: Cache,
    pub prompt: Option<NamePrompt>,
    pub delete_prompt: Option<DeletePrompt>,
    pub quick: Option<QuickOpen>,
    pub ticks: board::Ticks,
    /// The layout last sent as the startup preference (or read from it).
    remembered: Option<LayoutKey>,
    /// The last failed snooze or promotion, until dismissed.
    pub write_error: Option<String>,
    /// The canvas area as last drawn: its size places new cards in view, and
    /// sidebar drops map from the screen through it.
    canvas_area: egui::Rect,
    worker_error: Option<String>,
    // A save completed while the current fetch might still contain an older
    // layout list. Discard that list and fetch again after the batch completes.
    discard_layout_list: bool,
    pending_saves: usize,
    closing: bool,
    close_error: Option<String>,
    allow_close: bool,
    /// The connected service's build, when it reports one (DESIGN.md §7.4).
    service_build: Option<String>,
}

impl App {
    /// Start the fetch worker and the event subscriber, each on its own thread.
    pub fn new(
        paths: Paths,
        auto_start: Option<PathBuf>,
        auto_start_disabled: bool,
        ctx: egui::Context,
    ) -> Result<Self, String> {
        theme::install(&ctx);
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
                        &job.layout,
                    ));
                    if save_done.send(SaveDone { job, result }).is_err() {
                        break;
                    }
                    save_ctx.request_repaint();
                }
            })
            .map_err(|e| e.to_string())?;
        let (ops, op_jobs) = mpsc::sync_channel::<WriteOp>(8);
        let (op_done, ops_done) = mpsc::sync_channel(8);
        let op_runtime = runtime()?;
        let op_paths = paths.clone();
        let op_auto_start = auto_start.clone();
        let op_ctx = ctx.clone();
        std::thread::Builder::new()
            .name("callboard-layout-ops".into())
            .spawn(move || {
                while let Ok(op) = op_jobs.recv() {
                    let (paths, auto) = (&op_paths, op_auto_start.as_deref());
                    let result = op_runtime.block_on(async {
                        match &op {
                            WriteOp::Rename { from, to } => {
                                backend::rename_layout(paths, auto, from, to)
                                    .await
                                    .map(Reply::Layout)
                            }
                            WriteOp::Delete { name } => backend::delete_layout(paths, auto, name)
                                .await
                                .map(|_| Reply::Done),
                            WriteOp::Remember { name } => {
                                backend::set_last_layout(paths, auto, name.as_deref())
                                    .await
                                    .map(|_| Reply::Done)
                            }
                            WriteOp::Snooze {
                                feed,
                                key,
                                until_ms,
                                on_update,
                            } => {
                                let patch = serde_json::json!({
                                    "snoozed_until_ms": until_ms,
                                    "wake_on_update": on_update,
                                });
                                backend::patch_feed_item(paths, auto, feed, key, &patch)
                                    .await
                                    .map(|_| Reply::Done)
                            }
                            WriteOp::Promote {
                                feed,
                                key,
                                board_id,
                                kind,
                            } => backend::promote(paths, auto, feed, key, *board_id, *kind)
                                .await
                                .map(|_| Reply::Done),
                            WriteOp::Reorder {
                                feed,
                                key,
                                position,
                            } => {
                                let patch = serde_json::json!({ "position": position });
                                backend::patch_feed_item(paths, auto, feed, key, &patch)
                                    .await
                                    .map(|_| Reply::Done)
                            }
                            WriteOp::ResetOrder { feed, key } => {
                                let patch = serde_json::json!({ "reset_order": true });
                                backend::patch_feed_item(paths, auto, feed, key, &patch)
                                    .await
                                    .map(|_| Reply::Done)
                            }
                            WriteOp::Board(op) => {
                                let mut reply = Reply::Done;
                                for (method, resource, body) in op.requests() {
                                    let value =
                                        backend::send(paths, auto, method, &resource, &body)
                                            .await?;
                                    if let BoardOp::Create { .. } = op {
                                        reply =
                                            Reply::Board(serde_json::from_value(value).map_err(
                                                |e| format!("Invalid service response: {e}"),
                                            )?);
                                    }
                                }
                                Ok(reply)
                            }
                        }
                    });
                    if op_done.send(OpDone { op, result }).is_err() {
                        break;
                    }
                    op_ctx.request_repaint();
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
                ops,
                ops_done,
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
            delete_prompt: None,
            quick: None,
            ticks: board::Ticks::new(),
            remembered: None,
            write_error: None,
            canvas_area: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0)),
            worker_error: None,
            discard_layout_list: false,
            pending_saves: 0,
            closing: false,
            close_error: None,
            allow_close: false,
            service_build: None,
        }
    }

    fn receive(&mut self, now: Instant) {
        while let Ok(signal) = self.channels.signals.try_recv() {
            if let Signal::Build(build) = &signal {
                self.service_build = Some(build.clone());
            }
            self.scheduler.signal(signal, now);
        }
        while let Ok(done) = self.channels.saved.try_recv() {
            self.apply_saved(done, now);
        }
        while let Ok(done) = self.channels.ops_done.try_recv() {
            self.apply_op(done);
        }
        while let Ok(fetched) = self.channels.responses.try_recv() {
            self.apply_fetched(fetched, now);
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
                            // Snooze expiry changes counts without a notice.
                            let now_ms = now_ms();
                            let wake = feeds
                                .iter()
                                .filter_map(|f| f.next_wake_at_ms)
                                .filter(|at| *at > now_ms)
                                .min()
                                .map(|at| now + Duration::from_millis((at - now_ms) as u64 + 50));
                            self.scheduler.wake_feed_list_at(wake);
                            self.cache.feeds = feeds;
                            self.cache.feeds_loaded = true;
                        }
                        ListData::Boards(boards) => {
                            self.cache.boards = boards;
                            self.cache.boards_loaded = true;
                        }
                        ListData::Layouts(layouts, preferences) => {
                            if !self.discard_layout_list {
                                if !self.layouts.loaded() {
                                    self.remembered = Some(
                                        preferences
                                            .last_layout
                                            .clone()
                                            .map_or(LayoutKey::Unsaved, LayoutKey::Saved),
                                    );
                                    self.layouts.prefer(preferences.last_layout);
                                }
                                self.layouts.sync_saved(&layouts);
                            }
                        }
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
            // Responses for cards closed meanwhile would never be invalidated.
            if !open.contains(&target) {
                continue;
            }
            if let (Target::Board(id), Ok(Contents::Board { items, .. })) = (&target, &result) {
                self.reconcile_ticks(*id, items);
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
        self.discard_layout_list = false;
    }

    fn apply_saved(&mut self, done: SaveDone, now: Instant) {
        self.pending_saves = self.pending_saves.saturating_sub(1);
        if done.result.is_ok() {
            self.discard_layout_list |= self.scheduler.busy();
            self.scheduler.refresh_layouts();
        } else if self.closing {
            self.close_error = done.result.as_ref().err().cloned();
        }
        let SaveJob { name, purpose, .. } = done.job;
        let stored = done.result.map(|named| named.layout);
        match purpose {
            SavePurpose::Auto => self.layouts.save_finished(&name, stored, now),
            SavePurpose::SaveAs { from } => self.finish_prompt(Some(&from), &name, stored),
            SavePurpose::New => self.finish_prompt(None, &name, stored),
        }
    }

    fn apply_op(&mut self, done: OpDone) {
        let layout_op = matches!(done.op, WriteOp::Rename { .. } | WriteOp::Delete { .. });
        if done.result.is_ok() && layout_op {
            // A list fetched before this commit would bring the old name back.
            self.discard_layout_list |= self.scheduler.busy();
            self.scheduler.refresh_layouts();
        }
        match (done.op, done.result) {
            (WriteOp::Rename { from, .. }, Ok(Reply::Layout(layout))) => {
                self.layouts.renamed(&from, &layout);
                self.prompt = None;
            }
            (WriteOp::Delete { name }, Ok(_)) => {
                self.layouts.deleted(&name);
                self.delete_prompt = None;
            }
            (WriteOp::Rename { from, .. }, result) => {
                self.layouts.end_op(&from);
                if let Some(prompt) = &mut self.prompt {
                    prompt.waiting = false;
                    prompt.error = Some(
                        result
                            .err()
                            .unwrap_or_else(|| "Invalid service response".into()),
                    );
                }
            }
            (WriteOp::Delete { name }, Err(error)) => {
                self.layouts.end_op(&name);
                if let Some(prompt) = &mut self.delete_prompt {
                    prompt.waiting = false;
                    prompt.error = Some(error);
                }
            }
            // The preference only picks the startup layout; a failure is not
            // worth interrupting the user for.
            (WriteOp::Remember { .. }, _) => (),
            // The change notice also triggers these refetches; asking directly
            // keeps the card current while the event stream is down.
            (
                WriteOp::Snooze { feed, .. }
                | WriteOp::Reorder { feed, .. }
                | WriteOp::ResetOrder { feed, .. },
                Ok(_),
            ) => self.scheduler.want(Target::Feed(feed)),
            (WriteOp::Reorder { .. } | WriteOp::ResetOrder { .. }, Err(error)) => {
                self.write_error = Some(format!("Could not reorder the feed: {error}"));
            }
            (WriteOp::Promote { board_id, .. }, Ok(_)) => {
                self.scheduler.want(Target::Board(board_id))
            }
            (WriteOp::Snooze { .. }, Err(error)) => {
                self.write_error = Some(format!("Could not snooze the item: {error}"));
            }
            (WriteOp::Promote { .. }, Err(error)) => {
                self.write_error = Some(format!("Could not promote the item: {error}"));
            }
            (WriteOp::Board(op), result) => self.board_done(op, result),
        }
    }

    /// Drop confirmed ticks that a reload of `board` shows, or that two
    /// reloads after confirmation did not (another window changed it back).
    fn reconcile_ticks(&mut self, board: i64, items: &callboard_core::store::BoardContents) {
        self.ticks.retain(|id, tick| {
            if tick.board != board || !tick.confirmed {
                return true;
            }
            let stored = items.todos.iter().find(|t| t.item.id == *id);
            if stored.is_none_or(|t| t.item.done == tick.done) {
                return false;
            }
            tick.stale_reloads += 1;
            tick.stale_reloads < 2
        });
    }

    fn board_done(&mut self, op: BoardOp, result: Result<Reply, String>) {
        if let BoardOp::Patch {
            kind: Kind::Todo,
            id,
            ..
        } = &op
        {
            match &result {
                Ok(_) => {
                    if let Some(tick) = self.ticks.get_mut(id) {
                        tick.confirmed = true;
                    }
                }
                Err(_) => {
                    self.ticks.remove(id);
                }
            }
        }
        let error = match result {
            Ok(reply) => {
                for id in op.boards() {
                    self.scheduler.board_changed(id);
                }
                match (&op, reply) {
                    (BoardOp::Create { .. }, Reply::Board(created)) => {
                        self.scheduler.board_changed(created.id);
                        self.prompt = None;
                        self.apply(Action::Show(Target::Board(created.id)));
                    }
                    (BoardOp::Rename { .. }, _) => self.prompt = None,
                    (BoardOp::Delete { id }, _) => {
                        self.delete_prompt = None;
                        let canvas = self.layouts.active_mut();
                        canvas.close(&Target::Board(*id));
                    }
                    (BoardOp::DeleteItem { .. }, _) => self.delete_prompt = None,
                    _ => (),
                }
                return;
            }
            Err(error) => error,
        };
        // Dialog writes report in their dialog; the rest in the error bar.
        match op {
            BoardOp::Create { .. } | BoardOp::Rename { .. } if self.prompt.is_some() => {
                let prompt = self.prompt.as_mut().expect("checked");
                prompt.waiting = false;
                prompt.error = Some(error);
            }
            BoardOp::Delete { .. } | BoardOp::DeleteItem { .. } if self.delete_prompt.is_some() => {
                let prompt = self.delete_prompt.as_mut().expect("checked");
                prompt.waiting = false;
                prompt.error = Some(error);
            }
            _ => self.write_error = Some(format!("{}: {error}", op.failure())),
        }
    }

    /// Delete the layout named in the confirmation dialog.
    pub fn confirm_delete(&mut self) {
        let Some(prompt) = &mut self.delete_prompt else {
            return;
        };
        let op = match &prompt.subject {
            DeleteSubject::Layout(name) => {
                if !self.layouts.begin_op(name) {
                    prompt.error = Some("Wait for the current save to finish".into());
                    return;
                }
                WriteOp::Delete { name: name.clone() }
            }
            DeleteSubject::Board { id, .. } => WriteOp::Board(BoardOp::Delete { id: *id }),
            DeleteSubject::Item {
                kind, id, board, ..
            } => WriteOp::Board(BoardOp::DeleteItem {
                kind: *kind,
                id: *id,
                board: *board,
            }),
        };
        match self.channels.ops.try_send(op) {
            Ok(()) => {
                prompt.waiting = true;
                prompt.error = None;
            }
            Err(_) => {
                if let DeleteSubject::Layout(name) = &prompt.subject {
                    self.layouts.end_op(name);
                }
                prompt.error = Some("The service writer is busy or stopped; try again".into());
            }
        }
    }

    fn finish_prompt(
        &mut self,
        from: Option<&LayoutKey>,
        name: &str,
        stored: Result<Layout, String>,
    ) {
        match stored {
            Ok(layout) => {
                self.layouts.adopt(from, name, &layout);
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
        if prompt.kind.board() {
            let current = match prompt.kind {
                PromptKind::RenameBoard(id) => self.cache.board(id).map(|b| b.info.name.as_str()),
                _ => None,
            };
            if name.is_empty() {
                prompt.error = Some("Enter a board name".into());
                return;
            }
            if current == Some(name.as_str()) {
                self.prompt = None;
                return;
            }
            if self.cache.boards.iter().any(|b| b.info.name == name) {
                prompt.error = Some(format!("A board named “{name}” already exists"));
                return;
            }
            let op = match prompt.kind {
                PromptKind::RenameBoard(id) => BoardOp::Rename { id, name },
                _ => BoardOp::Create { name },
            };
            match self.channels.ops.try_send(WriteOp::Board(op)) {
                Ok(()) => {
                    prompt.waiting = true;
                    prompt.error = None;
                }
                Err(_) => {
                    prompt.error = Some("The service writer is busy or stopped; try again".into())
                }
            }
            return;
        }
        if let Err(e) = callboard_core::layout::validate_name(&name) {
            prompt.error = Some(e.to_string());
            return;
        }
        if prompt.kind == PromptKind::Rename {
            let LayoutKey::Saved(from) = self.layouts.active_key().clone() else {
                self.prompt = None;
                return;
            };
            if from == name {
                self.prompt = None;
                return;
            }
            if self.layouts.exists(&name) {
                prompt.error = Some(format!("A layout named “{name}” already exists"));
                return;
            }
            if !self.layouts.begin_op(&from) {
                prompt.error = Some("Wait for the current save to finish".into());
                return;
            }
            match self.channels.ops.try_send(WriteOp::Rename {
                from: from.clone(),
                to: name,
            }) {
                Ok(()) => {
                    prompt.waiting = true;
                    prompt.error = None;
                }
                Err(_) => {
                    self.layouts.end_op(&from);
                    prompt.error = Some("The layout worker is busy or stopped; try again".into());
                }
            }
            return;
        }
        if self.layouts.exists(&name) {
            prompt.error = Some(format!("A layout named “{name}” already exists"));
            return;
        }
        let job = match prompt.kind {
            PromptKind::SaveAs => SaveJob {
                name,
                layout: self.layouts.active().to_layout(),
                purpose: SavePurpose::SaveAs {
                    from: self.layouts.active_key().clone(),
                },
            },
            PromptKind::New => SaveJob {
                name,
                layout: Layout::default(),
                purpose: SavePurpose::New,
            },
            PromptKind::Rename | PromptKind::NewBoard | PromptKind::RenameBoard(_) => {
                unreachable!("handled above")
            }
        };
        match self.channels.saves.try_send(job) {
            Ok(()) => {
                self.pending_saves += 1;
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
        let viewport = self.canvas_area.size();
        let canvas = self.layouts.active_mut();
        match action {
            Action::Show(target) => canvas.place(target, None, viewport),
            Action::PlaceAt(target, at) => {
                if canvas.find(&target).is_some() {
                    let from = canvas.card(&target).expect("found").rect.min;
                    canvas.move_by(&target, at - from);
                    canvas.reveal(&target, viewport);
                } else {
                    canvas.place(target, Some(at), viewport);
                }
            }
            Action::Card {
                canvas: id,
                target,
                op,
            } if id == canvas.id => match op {
                CardOp::Raise => {
                    canvas.raise(&target);
                }
                CardOp::Move(delta) => canvas.move_by(&target, delta),
                CardOp::Resize(size) => canvas.resize(&target, size),
                CardOp::ToggleCollapsed => canvas.toggle_collapsed(&target),
                CardOp::Close => canvas.close(&target),
                CardOp::Retarget(to) => {
                    canvas.retarget(&target, to);
                }
            },
            Action::Pan { canvas: id, delta } if id == canvas.id => canvas.pan_by(delta),
            Action::Card { .. } | Action::Pan { .. } => (),
            Action::ShowAll => canvas.show_all(),
            Action::Switch(key) => self.layouts.switch(key),
            Action::Revert => self.layouts.revert(),
            Action::Refresh => self.scheduler.refresh_all(),
            Action::Write(op) => {
                if let WriteOp::Board(BoardOp::Patch {
                    kind: Kind::Todo,
                    id,
                    board,
                    patch,
                }) = &op
                    && let Some(done) = patch.get("done").and_then(serde_json::Value::as_bool)
                {
                    self.ticks.insert(
                        *id,
                        board::Tick {
                            done,
                            board: *board,
                            confirmed: false,
                            stale_reloads: 0,
                        },
                    );
                }
                if self.channels.ops.try_send(op).is_err() {
                    self.write_error =
                        Some("The service writer is busy or stopped; try again".into());
                }
            }
            Action::DismissWriteError => self.write_error = None,
            Action::Prompt(kind) => {
                let text = match (kind, self.layouts.active_key()) {
                    (PromptKind::Rename, LayoutKey::Saved(name)) => name.clone(),
                    (PromptKind::RenameBoard(id), _) => self.cache.title(&Target::Board(id)),
                    _ => String::new(),
                };
                self.prompt = Some(NamePrompt {
                    kind,
                    text,
                    error: None,
                    waiting: false,
                    select_all: true,
                })
            }
            Action::QuickOpen => self.quick = Some(QuickOpen::default()),
            Action::ConfirmDelete(subject) => {
                self.delete_prompt = Some(DeletePrompt {
                    subject,
                    error: None,
                    waiting: false,
                });
            }
        }
    }

    /// Process incoming data and issue the next fetch, if any. Returns the
    /// targets placed in the active layout.
    pub fn tick(&mut self, now: Instant) -> BTreeSet<Target> {
        self.receive(now);
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
        let saves = if self.closing {
            if self.close_error.is_none() {
                self.layouts.flush_saves(now)
            } else {
                Vec::new()
            }
        } else {
            self.layouts.due_saves(now)
        };
        for (name, layout) in saves {
            let job = SaveJob {
                name: name.clone(),
                layout,
                purpose: SavePurpose::Auto,
            };
            if self.channels.saves.try_send(job).is_err() {
                let error = "The layout saver is busy or stopped".to_string();
                if self.closing {
                    self.close_error = Some(error.clone());
                }
                self.layouts.save_finished(&name, Err(error), now);
            } else {
                self.pending_saves += 1;
            }
        }
        self.remember_active();
        open
    }

    /// Record the active layout as the startup preference when it changes:
    /// on switching, Save as, New layout, rename, or deleting the active one.
    fn remember_active(&mut self) {
        if !self.layouts.loaded() || self.remembered.as_ref() == Some(self.layouts.active_key()) {
            return;
        }
        let key = self.layouts.active_key().clone();
        let name = match &key {
            LayoutKey::Saved(name) => Some(name.clone()),
            LayoutKey::Unsaved => None,
        };
        // Not retried: at worst the next startup opens a different layout.
        let _ = self.channels.ops.try_send(WriteOp::Remember { name });
        self.remembered = Some(key);
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        theme::install(ui.ctx());
        if !theme::ready(ui.ctx()) {
            return;
        }
        let now = Instant::now();
        if ui.input(|i| i.viewport().close_requested()) && !self.allow_close {
            self.closing = true;
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        // Computed once per frame: both convert every working canvas.
        let placed = self.tick(now);
        let entries = self.layouts.entries();
        let wait = self
            .scheduler
            .next_deadline(now)
            .into_iter()
            .chain(
                (!self.closing)
                    .then(|| self.layouts.next_save_deadline(now))
                    .flatten(),
            )
            .min()
            .unwrap_or(Duration::MAX)
            .min(Duration::from_secs(1)); // Ages and stale markers tick.
        ui.ctx().request_repaint_after(wait);
        let mut actions = Vec::new();
        let dialog = self.closing || self.prompt.is_some() || self.delete_prompt.is_some();
        if !dialog && ui.input_mut(|i| i.consume_shortcut(&quick::SHORTCUT)) {
            actions.push(Action::QuickOpen);
        }
        egui::Panel::top("layout_bar")
            .frame(
                egui::Frame::new()
                    .fill(ui.visuals().panel_fill)
                    .inner_margin(egui::Margin::symmetric(10, 6)),
            )
            .show(ui, |ui| self.layout_bar(ui, now, &entries, &mut actions));
        if let Some(error) = self.worker_error.clone().or(self.cache.error.clone()) {
            egui::Panel::top("error_bar").show(ui, |ui| self.error_bar(ui, &error));
        }
        if let Some(error) = &self.write_error {
            egui::Panel::top("write_error_bar").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        theme::glyph(icon::WARNING_CIRCLE).color(Palette::of(ui.visuals()).danger),
                    );
                    let first_line = error.lines().next().unwrap_or_default();
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(first_line).color(ui.visuals().error_fg_color),
                        )
                        .truncate(),
                    )
                    .on_hover_text(error);
                    if ui.small_button("Dismiss").clicked() {
                        actions.push(Action::DismissWriteError);
                    }
                });
            });
        }
        egui::Panel::left("resources")
            .default_size(264.0)
            .frame(
                egui::Frame::new()
                    .fill(ui.visuals().panel_fill)
                    .inner_margin(egui::Margin::symmetric(10, 10)),
            )
            .show(ui, |ui| self.sidebar(ui, &entries, &placed, &mut actions));
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(canvas_fill(ui.visuals())))
            .show(ui, |ui| self.canvas(ui, &mut actions));
        if self.closing {
            self.close_window(ui);
        } else {
            self.prompt_window(ui);
            self.delete_window(ui);
            self.quick_open_window(ui, &entries, &placed, &mut actions);
            for action in actions {
                self.apply(action);
            }
        }
    }

    fn quick_open_window(
        &mut self,
        ui: &mut egui::Ui,
        entries: &[LayoutEntry],
        placed: &BTreeSet<Target>,
        actions: &mut Vec<Action>,
    ) {
        let Some(state) = &mut self.quick else {
            return;
        };
        let target = |kind, title, target: Target| {
            let placed = placed.contains(&target);
            Choice::target(kind, title, target, placed)
        };
        let choices: Vec<Choice> = self
            .cache
            .feeds
            .iter()
            .map(|f| {
                target(
                    "Feed",
                    f.info.title.clone(),
                    Target::Feed(f.info.name.clone()),
                )
            })
            .chain(
                self.cache
                    .boards
                    .iter()
                    .map(|b| target("Board", b.info.name.clone(), Target::Board(b.info.id))),
            )
            .chain([target(
                "Archive",
                "Deleted-board archive".into(),
                Target::Archive,
            )])
            .chain(
                entries
                    .iter()
                    .map(|e| Choice::layout(e.key.clone(), e.active)),
            )
            .collect();
        if !quick::show(ui, state, &choices, actions) {
            self.quick = None;
        }
    }

    fn close_window(&mut self, ui: &mut egui::Ui) {
        let pending =
            self.pending_saves > 0 || self.layouts.entries().iter().any(|e| e.pending || e.saving);
        if !pending && self.close_error.is_none() {
            self.allow_close = true;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        egui::Modal::new(egui::Id::new("save_before_close")).show(ui.ctx(), |ui| {
            ui.heading("Saving layouts before closing");
            if let Some(error) = &self.close_error {
                ui.colored_label(ui.visuals().error_fg_color, error);
                if ui.button("Retry saving").clicked() {
                    self.close_error = None;
                    // A failed Save as/New request is not an auto-save.
                    if self
                        .prompt
                        .as_ref()
                        .is_some_and(|p| !p.waiting && p.error.is_some())
                    {
                        self.submit_prompt();
                        if let Some(error) = self.prompt.as_ref().and_then(|p| p.error.clone()) {
                            self.close_error = Some(error);
                        }
                    }
                }
            } else {
                ui.spinner();
                ui.label("Waiting for the service to confirm your changes…");
            }
            if ui.button("Keep window open").clicked() {
                self.closing = false;
                self.close_error = None;
            }
            if ui.button("Close without waiting").clicked() {
                self.allow_close = true;
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }

    fn layout_bar(
        &self,
        ui: &mut egui::Ui,
        now: Instant,
        entries: &[LayoutEntry],
        actions: &mut Vec<Action>,
    ) {
        let p = Palette::of(ui.visuals());
        ui.horizontal(|ui| {
            let active = self.layouts.active_key();
            let muted = Palette::of(ui.visuals()).muted;
            let menu = ui.menu_button(
                (
                    theme::glyph(icon::SQUARES_FOUR).color(muted),
                    theme::strong(active.label()),
                    theme::glyph(icon::CARET_DOWN).size(11.0).color(muted),
                ),
                |ui| self.layout_menu(ui, entries, actions),
            );
            theme::name(&menu.response, &format!("Layout: {}", active.label()));
            menu.response
                .on_hover_text("Switch, save, rename, or delete layouts");
            if let Some(entry) = entries.iter().find(|e| e.active) {
                layout_status(ui, entry, actions);
            }
            ui.add_space(12.0);
            let search = ui
                .add(
                    egui::Button::new((
                        theme::glyph(icon::MAGNIFYING_GLASS).color(p.muted),
                        egui::RichText::new("Search or jump to…").color(p.muted),
                    ))
                    .shortcut_text(egui::RichText::new("Ctrl K").size(11.0).color(p.faint))
                    .fill(p.raised)
                    .min_size(egui::vec2(280.0, 26.0)),
                )
                .on_hover_text("Find a feed, board, or layout by name (Ctrl+K)");
            theme::name(&search, "Open…");
            if search.clicked() {
                actions.push(Action::QuickOpen);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let refresh = ui.add_enabled_ui(!self.scheduler.busy(), |ui| {
                    theme::icon_button(
                        ui,
                        icon::ARROWS_CLOCKWISE,
                        "Refresh",
                        "Refetch everything now",
                    )
                });
                if refresh.inner.clicked() {
                    actions.push(Action::Refresh);
                }
                if self.scheduler.busy() {
                    ui.spinner();
                }
                if let Some(at) = self.cache.refreshed_at {
                    ui.label(
                        egui::RichText::new(format!(
                            "updated {} ago",
                            coarse(Duration::from_secs(now.duration_since(at).as_secs()))
                        ))
                        .size(12.0)
                        .color(p.faint),
                    );
                }
                let (text, dot, hover) = match self.scheduler.link(now) {
                    Link::Live => (
                        "Live".to_string(),
                        p.success,
                        "Receiving change notices.".to_string(),
                    ),
                    Link::Connecting => (
                        "Connecting…".into(),
                        p.faint,
                        "Opening the event stream.".into(),
                    ),
                    Link::Polling => (
                        format!("Polling every {} s", POLL_INTERVAL.as_secs()),
                        p.warning,
                        format!(
                            "The event stream is unavailable{}; refreshing on a timer until it reconnects.",
                            self.scheduler
                                .stream_error()
                                .map(|e| format!(" ({e})"))
                                .unwrap_or_default()
                        ),
                    ),
                };
                ui.label(egui::RichText::new(text).size(12.0).color(p.muted))
                    .on_hover_text(hover);
                let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 3.5, dot);
                if let Some(build) = self.service_build.as_deref()
                    && build != callboard_service::BUILD
                {
                    theme::pill(ui, "Version mismatch", p.warning_soft, p.warning)
                        .on_hover_text(build_mismatch(build));
                }
            });
        });
    }

    /// The layout menu: switch layouts, and save, create, rename, or delete.
    fn layout_menu(&self, ui: &mut egui::Ui, entries: &[LayoutEntry], actions: &mut Vec<Action>) {
        ui.set_min_width(220.0);
        ui.label(theme::eyebrow(ui, "Layouts"));
        for entry in entries {
            let mut text = entry.key.label().to_string();
            if entry.key == LayoutKey::Unsaved {
                text.push_str(" (not saved)");
            }
            if ui
                .add(egui::Button::selectable(entry.active, text))
                .clicked()
            {
                if !entry.active {
                    actions.push(Action::Switch(entry.key.clone()));
                }
                ui.close();
            }
        }
        ui.separator();
        let mut item = |ui: &mut egui::Ui, text: &str, hint: &str, action: Action| {
            if ui.button(text).on_hover_text(hint).clicked() {
                actions.push(action);
                ui.close();
            }
        };
        item(
            ui,
            "Save as…",
            "Save this arrangement as a new named layout",
            Action::Prompt(PromptKind::SaveAs),
        );
        item(
            ui,
            "New layout…",
            "Create an empty named layout",
            Action::Prompt(PromptKind::New),
        );
        if let LayoutKey::Saved(name) = self.layouts.active_key() {
            item(
                ui,
                "Rename…",
                "Rename this layout",
                Action::Prompt(PromptKind::Rename),
            );
            item(
                ui,
                "Delete…",
                "Delete this saved layout",
                Action::ConfirmDelete(DeleteSubject::Layout(name.clone())),
            );
        }
        ui.separator();
        if ui
            .add_enabled(
                !self.layouts.active().cards.is_empty(),
                egui::Button::new("Show all"),
            )
            .on_hover_text("Pan to the cards, including any panned out of sight")
            .clicked()
        {
            actions.push(Action::ShowAll);
            ui.close();
        }
    }

    /// A dialog for naming a layout. Enter submits, Escape cancels.
    fn prompt_window(&mut self, ui: &mut egui::Ui) {
        let Some(prompt) = &mut self.prompt else {
            return;
        };
        let title = match prompt.kind {
            PromptKind::SaveAs => "Save layout as",
            PromptKind::New => "New layout",
            PromptKind::Rename => "Rename layout",
            PromptKind::NewBoard => "New board",
            PromptKind::RenameBoard(_) => "Rename board",
        };
        let mut submit = false;
        let mut cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                let label = ui.label(if prompt.kind.board() {
                    "Board name"
                } else {
                    "Layout name"
                });
                let mut output = ui
                    .add_enabled_ui(!prompt.waiting, |ui| {
                        egui::TextEdit::singleline(&mut prompt.text)
                            .desired_width(260.0)
                            .show(ui)
                    })
                    .inner;
                if std::mem::take(&mut prompt.select_all) {
                    let all = egui::text_selection::CCursorRange::select_all(&output.galley);
                    output.state.cursor.set_char_range(Some(all));
                    output.state.store(ui.ctx(), output.response.response.id);
                }
                let edit = output.response.response.clone().labelled_by(label.id);
                // Read before refocusing: lost_focus() reflects focus now.
                if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
                // Keep typing in the field: on open (including a prefilled
                // rename), and after Enter submits a name that is refused.
                if !prompt.waiting && !edit.has_focus() {
                    edit.request_focus();
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

    /// Confirms a deletion. Escape cancels.
    fn delete_window(&mut self, ui: &mut egui::Ui) {
        let Some(prompt) = &self.delete_prompt else {
            return;
        };
        let mut confirm = false;
        let mut cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
        let (title, question, detail) = match &prompt.subject {
            DeleteSubject::Layout(name) => (
                "Delete layout".to_owned(),
                format!("Delete the layout “{name}”?"),
                "Feeds and boards are not affected.",
            ),
            DeleteSubject::Board { name, .. } => (
                "Delete board".to_owned(),
                format!("Delete the board “{name}”?"),
                "Its todos and notes, archived ones included, move to the deleted-board archive, where they can be restored.",
            ),
            DeleteSubject::Item { kind, name, .. } => (
                format!("Delete {}", kind.noun()),
                format!("Delete the {} “{name}”?", kind.noun()),
                "It cannot be restored. Archive it instead to keep it.",
            ),
        };
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                ui.label(question);
                ui.weak(detail);
                if let Some(error) = &prompt.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                ui.horizontal(|ui| {
                    if prompt.waiting {
                        ui.spinner();
                        ui.label("Deleting…");
                    } else {
                        confirm |= ui.button("Delete").clicked();
                        cancel |= ui.button("Cancel").clicked();
                    }
                });
            });
        if cancel && !prompt.waiting {
            self.delete_prompt = None;
        } else if confirm {
            self.confirm_delete();
        }
    }

    fn sidebar(
        &self,
        ui: &mut egui::Ui,
        entries: &[LayoutEntry],
        placed: &BTreeSet<Target>,
        actions: &mut Vec<Action>,
    ) {
        let p = Palette::of(ui.visuals());
        let canvas = self.layouts.active();
        let front = canvas.focused_target().cloned();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            let add = ui.menu_button(
                (
                    theme::glyph(icon::PLUS).color(p.muted),
                    egui::RichText::new("Add card…").color(p.muted),
                ),
                |ui| {
                    // Prefixed: a feed and a board may share a title.
                    let choices =
                        self.cache
                            .feeds
                            .iter()
                            .map(|f| {
                                (
                                    Target::Feed(f.info.name.clone()),
                                    format!("Feed: {}", f.info.title),
                                )
                            })
                            .chain(self.cache.boards.iter().map(|b| {
                                (Target::Board(b.info.id), format!("Board: {}", b.info.name))
                            }))
                            .chain([(Target::Archive, "Deleted-board archive".to_owned())]);
                    for (target, mut title) in choices {
                        if placed.contains(&target) {
                            title.push_str(" (on canvas)");
                        }
                        if ui.button(title).clicked() {
                            actions.push(Action::Show(target));
                            ui.close();
                        }
                    }
                },
            );
            theme::name(&add.response, "Add card…");
            let section = |ui: &mut egui::Ui, title: &str| {
                ui.add_space(12.0);
                ui.label(theme::eyebrow(ui, title));
                ui.add_space(2.0);
            };
            section(ui, "Layouts");
            for entry in entries {
                let mut text = entry.key.label().to_string();
                if entry.key == LayoutKey::Unsaved {
                    text.push_str(" (not saved)");
                } else if entry.save_error.is_some() {
                    text.push_str(" · save failed");
                }
                let row = theme::list_row(ui, entry.active, |ui| {
                    ui.label(theme::glyph(icon::SQUARES_FOUR).color(p.faint));
                    theme::row_text(ui, &text);
                });
                theme::name(&row, &text);
                if row.clicked() && !entry.active {
                    actions.push(Action::Switch(entry.key.clone()));
                }
            }
            section(ui, "Feeds");
            if !self.cache.lists_loaded() {
                ui.weak("Loading…");
            } else if self.cache.feeds.is_empty() {
                ui.weak("No feeds yet");
            }
            for feed in self.cache.feeds.iter().map(|f| &f.info) {
                let target = Target::Feed(feed.name.clone());
                self.entry(ui, &target, placed, &front, actions, |ui| {
                    ui.label(format!("Feed name: {}", feed.name));
                    ui.label(format!(
                        "Last submitted {} ago",
                        coarse(age(feed.last_submitted_at_ms))
                    ));
                    if let Some(limit) = &feed.stale_after {
                        ui.label(format!("Stale after {limit}"));
                    }
                    if let Some(error) = &feed.error {
                        ui.colored_label(ui.visuals().error_fg_color, &error.message);
                    }
                });
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.label(theme::eyebrow(ui, "Boards"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::icon_button(
                        ui,
                        icon::PLUS,
                        "New board…",
                        "Create a board and place its card",
                    )
                    .clicked()
                    {
                        actions.push(Action::Prompt(PromptKind::NewBoard));
                    }
                });
            });
            if self.cache.lists_loaded() && self.cache.boards.is_empty() {
                ui.weak("No boards yet");
            }
            for board in self.cache.boards.iter().map(|b| &b.info) {
                let target = Target::Board(board.id);
                self.entry(ui, &target, placed, &front, actions, |ui| {
                    ui.label(format!("Board ID {}", board.id));
                });
            }
            ui.add_space(6.0);
            self.entry(ui, &Target::Archive, placed, &front, actions, |ui| {
                ui.label("Items archived from deleted boards. Not saved in layouts.");
            });
            ui.add_space(16.0);
            ui.label(
                egui::RichText::new(
                    "Click to show a card. Drag an entry onto the canvas to place it there.",
                )
                .size(11.5)
                .color(p.faint),
            );
        });
    }

    fn entry(
        &self,
        ui: &mut egui::Ui,
        target: &Target,
        placed: &BTreeSet<Target>,
        front: &Option<Target>,
        actions: &mut Vec<Action>,
        details: impl FnOnce(&mut egui::Ui),
    ) {
        let p = Palette::of(ui.visuals());
        let title = self.cache.title(target);
        let on_canvas = placed.contains(target);
        let response = theme::list_row(ui, front.as_ref() == Some(target), |ui| {
            // Counts and markers on the right, then the icon and the name.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                match self.cache.marker(target) {
                    Some("error") => {
                        ui.label(theme::glyph(icon::WARNING_CIRCLE).color(p.danger));
                    }
                    Some(marker) => {
                        ui.label(egui::RichText::new(marker).size(11.0).color(p.warning));
                    }
                    None => (),
                }
                if let Some(counts) = self.cache.counts(target) {
                    if counts.snoozed > 0 {
                        let snoozed = ui.label(
                            egui::RichText::new(format!("+{}", counts.snoozed))
                                .size(11.5)
                                .color(p.faint),
                        );
                        let name = format!("+{} snoozed", counts.snoozed);
                        snoozed.widget_info(|| {
                            egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &name)
                        });
                    }
                    if counts.marked > 0 {
                        theme::pill(
                            ui,
                            &format!("{} new", counts.marked),
                            p.accent_soft,
                            p.accent_text,
                        );
                    }
                    ui.label(
                        egui::RichText::new(counts.shown.to_string())
                            .size(12.0)
                            .color(p.faint),
                    );
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    match (target, self.cache.board_color(target)) {
                        (Target::Board(_), Some(color)) => {
                            let (rect, _) = ui
                                .allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                            if let Some(mark) = theme::color_mark(color) {
                                ui.painter().circle_filled(rect.center(), 4.5, mark);
                            }
                        }
                        (Target::Board(_), None) => {
                            ui.label(theme::glyph(icon::KANBAN).color(p.faint));
                        }
                        (Target::Feed(_), _) => {
                            ui.label(theme::glyph(icon::RSS_SIMPLE).color(p.faint));
                        }
                        (Target::Archive, _) => {
                            ui.label(theme::glyph(icon::ARCHIVE).color(p.faint));
                        }
                    }
                    let color = if on_canvas { p.text } else { p.muted };
                    theme::row_text(ui, egui::RichText::new(&title).color(color));
                });
            });
        });
        theme::name(&response, &title);
        let response = response.on_hover_ui(|ui| {
            details(ui);
            if on_canvas {
                ui.weak("On the canvas");
            }
        });
        if response.clicked() {
            actions.push(Action::Show(target.clone()));
        }
        if response.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            if let Some(pos) = ui.ctx().pointer_interact_pos() {
                drag_ghost(ui.ctx(), pos, &title);
            }
        }
        if response.drag_stopped()
            && let Some(pos) = ui.ctx().pointer_interact_pos()
            && self.canvas_area.contains(pos)
        {
            // The pointer lands on the new card's title bar.
            let at = self.layouts.active().view + (pos - self.canvas_area.min)
                - egui::vec2(DROP_GRAB.x, DROP_GRAB.y);
            actions.push(Action::PlaceAt(target.clone(), at.round()));
        }
        response.context_menu(|ui| {
            if ui.button("Show card").clicked() {
                actions.push(Action::Show(target.clone()));
                ui.close();
            }
            if on_canvas && ui.button("Remove card").clicked() {
                actions.push(Action::Card {
                    canvas: self.layouts.active().id,
                    target: target.clone(),
                    op: CardOp::Close,
                });
                ui.close();
            }
            if let Target::Board(id) = target {
                ui.separator();
                if ui.button("Rename board…").clicked() {
                    actions.push(Action::Prompt(PromptKind::RenameBoard(*id)));
                    ui.close();
                }
                if ui.button("Delete board…").clicked() {
                    actions.push(Action::ConfirmDelete(DeleteSubject::Board {
                        id: *id,
                        name: title.clone(),
                    }));
                    ui.close();
                }
            }
        });
    }

    /// One line under the layout bar, so an outage does not shift the canvas
    /// around; details and setup hints are on hover.
    fn error_bar(&self, ui: &mut egui::Ui, error: &str) {
        // A missing service binary will not fix itself, so say so up front
        // rather than showing the socket error it causes.
        let missing = !self.auto_start_disabled && self.auto_start.is_none();
        let hint = if self.auto_start_disabled {
            "No service is running and auto-start is disabled. Start `callboard serve` or restart the GUI without `--no-auto-start`."
        } else if self.auto_start.is_none() {
            "The callboard service binary was not found. Install callboard next to callboard-gui or put it on PATH, then refresh."
        } else {
            "Retrying automatically."
        };
        ui.horizontal(|ui| {
            let p = Palette::of(ui.visuals());
            ui.label(theme::glyph(icon::WARNING_CIRCLE).color(p.danger));
            ui.colored_label(p.danger, "Unable to refresh:");
            let first_line = if missing {
                "the callboard service binary is not installed beside callboard-gui or on PATH"
            } else {
                error.lines().next().unwrap_or_default()
            };
            ui.add(egui::Label::new(first_line).truncate())
                .on_hover_text(format!("{error}\n\n{hint}"));
            if self.cache.lists_loaded() {
                ui.weak("showing the last loaded data");
            }
        });
    }

    /// The canvas area as last drawn, in screen coordinates.
    pub fn canvas_area(&self) -> egui::Rect {
        self.canvas_area
    }

    /// Where a card is drawn, in screen coordinates (unclipped).
    pub fn card_screen_rect(&self, target: &Target) -> Option<egui::Rect> {
        let canvas = self.layouts.active();
        let card = canvas.card(target)?;
        Some(
            card.shown()
                .translate(self.canvas_area.min.to_vec2() - canvas.view.to_vec2()),
        )
    }

    /// The canvas: cards drawn back to front, clipped to the central area.
    /// Nothing changes while drawing; every change is an action.
    fn canvas(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let area = ui.max_rect();
        self.canvas_area = area;
        let canvas = self.layouts.active();
        let offset = area.min.to_vec2() - canvas.view.to_vec2();
        let screen = |card: &CardState| card.shown().translate(offset);
        let topmost_at = |pos: egui::Pos2| {
            area.contains(pos)
                .then(|| canvas.cards.iter().rposition(|c| screen(c).contains(pos)))
                .flatten()
        };
        let card_action = |target: &Target, op| Action::Card {
            canvas: canvas.id,
            target: target.clone(),
            op,
        };

        // A press anywhere on a card raises it, including on its buttons.
        // Read presses from the events: a quick click can press and release
        // within one frame, and then `press_origin()` is already cleared.
        let presses: Vec<egui::Pos2> = ui.input(|i| {
            i.events
                .iter()
                .filter_map(|event| match event {
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        ..
                    } => Some(*pos),
                    _ => None,
                })
                .collect()
        });
        let keep = board::take_keep_in_place(ui.ctx());
        for pos in presses {
            if let Some(i) = topmost_at(pos)
                && i + 1 < canvas.cards.len()
                && !keep
                    .iter()
                    .any(|(rect, card)| rect.contains(pos) && *card == canvas.cards[i].target)
            {
                actions.push(card_action(&canvas.cards[i].target, CardOp::Raise));
            }
        }

        // Registered first, so every card widget sits above it.
        dot_grid(ui, area, canvas.view);
        let background = ui.interact(
            area,
            ui.id().with(("canvas", canvas.id)),
            egui::Sense::click_and_drag(),
        );
        if background.dragged() && background.drag_delta() != egui::Vec2::ZERO {
            actions.push(Action::Pan {
                canvas: canvas.id,
                delta: -background.drag_delta(),
            });
        }
        if canvas.cards.is_empty() {
            let p = Palette::of(ui.visuals());
            let rect = egui::Rect::from_center_size(area.center(), egui::vec2(420.0, 120.0));
            let mut empty = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(rect)
                    .layout(egui::Layout::top_down(egui::Align::Center)),
            );
            empty.label(theme::glyph(icon::SQUARES_FOUR).size(28.0).color(p.faint));
            empty.label(theme::strong("No cards in this layout").size(16.0));
            empty.add(
                egui::Label::new(
                    egui::RichText::new("Click a feed or board in the sidebar, or drag one here.")
                        .color(p.muted),
                )
                .selectable(false),
            );
        }

        let under = ui.input(|i| i.pointer.hover_pos()).and_then(topmost_at);
        let front = canvas.cards.len().saturating_sub(1);
        for (i, card) in canvas.cards.iter().enumerate() {
            let rect = screen(card);
            if rect.intersects(area) {
                self.card(ui, area, card, rect, i == front, under == Some(i), actions);
            }
        }

        // The wheel over empty canvas pans; over a card, its list used it.
        if under.is_none()
            && ui
                .input(|i| i.pointer.hover_pos())
                .is_some_and(|p| area.contains(p))
        {
            let delta = ui.input_mut(|i| std::mem::take(&mut i.smooth_scroll_delta));
            if delta != egui::Vec2::ZERO {
                actions.push(Action::Pan {
                    canvas: canvas.id,
                    delta: -delta,
                });
            }
        }
    }

    /// One card. Registration order is stacking order within the card: a
    /// full-card blocker (so lower cards never react through it), the title
    /// bar, its buttons, the body, and last the resize grips.
    #[allow(clippy::too_many_arguments)]
    fn card(
        &self,
        ui: &mut egui::Ui,
        area: egui::Rect,
        card: &CardState,
        rect: egui::Rect,
        front: bool,
        scroll: bool,
        actions: &mut Vec<Action>,
    ) {
        let canvas = self.layouts.active();
        let target = &card.target;
        let act = |op| Action::Card {
            canvas: canvas.id,
            target: target.clone(),
            op,
        };
        // Keyed by canvas and target: a rebuilt or retargeted card starts fresh.
        let salt = canvas.id.with(target);
        let clip = rect.intersect(area);
        // An explicit id, not a salt: egui mixes a salted child's position in
        // the parent into its widget ids, so raising a card (drawing it later)
        // would change its buttons' ids between press and release.
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).id(salt));
        child.set_clip_rect(clip);
        child.interact(rect, salt.with("blocker"), egui::Sense::click_and_drag());

        let visuals = child.visuals().clone();
        let p = Palette::of(&visuals);
        let radius = egui::CornerRadius::same(theme::RADIUS_LG);
        child
            .painter()
            .add(visuals.window_shadow.as_shape(rect, radius));
        let stroke = if front {
            egui::Stroke::new(1.5, p.accent)
        } else {
            egui::Stroke::new(1.0, p.hairline)
        };
        child
            .painter()
            .rect(rect, radius, p.surface, stroke, egui::StrokeKind::Inside);
        let title_rect =
            egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), TITLE_HEIGHT));
        // A board's color runs along the card's top edge (DESIGN.md §6.3).
        if let Some(mark) = self.cache.board_color(target).and_then(theme::color_mark) {
            let band = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), 4.0));
            child.painter().rect_filled(
                band,
                egui::CornerRadius {
                    nw: theme::RADIUS_LG,
                    ne: theme::RADIUS_LG,
                    sw: 0,
                    se: 0,
                },
                mark,
            );
        }
        if !card.collapsed {
            child.painter().hline(
                rect.x_range().shrink(1.0),
                title_rect.bottom(),
                egui::Stroke::new(1.0, p.hairline),
            );
        }
        let title = child.interact(
            title_rect,
            salt.with("title"),
            egui::Sense::click_and_drag(),
        );
        if title.dragged() && title.drag_delta() != egui::Vec2::ZERO {
            actions.push(act(CardOp::Move(title.drag_delta())));
        }
        if title.double_clicked() {
            actions.push(act(CardOp::ToggleCollapsed));
        }
        let mut bar = child.new_child(
            egui::UiBuilder::new()
                .max_rect(title_rect.shrink2(egui::vec2(10.0, 4.0)))
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        bar.set_clip_rect(title_rect.intersect(area));
        bar.spacing_mut().item_spacing.x = 2.0;
        if theme::icon_button(
            &mut bar,
            icon::X,
            "Close card",
            "Remove this card from the layout",
        )
        .clicked()
        {
            actions.push(act(CardOp::Close));
        }
        let (fold, name, hint) = if card.collapsed {
            (icon::PLUS, "Expand card", "Expand")
        } else {
            (icon::MINUS, "Collapse card", "Collapse to the title bar")
        };
        if theme::icon_button(&mut bar, fold, name, hint).clicked() {
            actions.push(act(CardOp::ToggleCollapsed));
        }
        self.retarget_menu(&mut bar, canvas, target, actions);
        bar.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let glyph = match target {
                Target::Feed(_) => icon::RSS_SIMPLE,
                Target::Board(_) => icon::KANBAN,
                Target::Archive => icon::ARCHIVE,
            };
            ui.label(theme::glyph(glyph).color(p.faint));
            let label = ui.add(
                egui::Label::new(self.cache.card_heading(target, p))
                    .truncate()
                    .selectable(false),
            );
            label.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Label,
                    true,
                    self.cache.card_title(target),
                )
            });
        });

        if card.collapsed {
            return;
        }
        let body = egui::Rect::from_min_max(
            egui::pos2(rect.min.x + 12.0, rect.min.y + TITLE_HEIGHT + 8.0),
            rect.max - egui::vec2(10.0, 10.0),
        );
        let mut body_ui = child.new_child(egui::UiBuilder::new().max_rect(body));
        body_ui.set_clip_rect(body.intersect(area));
        self.card_body(&mut body_ui, target, salt, scroll, actions);

        // Resize from the right edge, the bottom edge, and the corner.
        const GRIP: f32 = 6.0;
        let grips = [
            (
                "right",
                egui::Rect::from_min_max(
                    egui::pos2(rect.max.x - GRIP, rect.min.y + TITLE_HEIGHT),
                    rect.max,
                ),
                egui::vec2(1.0, 0.0),
                egui::CursorIcon::ResizeHorizontal,
            ),
            (
                "bottom",
                egui::Rect::from_min_max(egui::pos2(rect.min.x, rect.max.y - GRIP), rect.max),
                egui::vec2(0.0, 1.0),
                egui::CursorIcon::ResizeVertical,
            ),
            (
                "corner",
                egui::Rect::from_min_max(rect.max - egui::Vec2::splat(GRIP * 2.5), rect.max),
                egui::vec2(1.0, 1.0),
                egui::CursorIcon::ResizeNwSe,
            ),
        ];
        for (name, grip_rect, axes, cursor) in grips {
            let grip = child
                .interact(grip_rect, salt.with(name), egui::Sense::drag())
                .on_hover_cursor(cursor);
            if grip.dragged() && grip.drag_delta() != egui::Vec2::ZERO {
                actions.push(act(CardOp::Resize(
                    card.rect.size() + grip.drag_delta() * axes,
                )));
            }
        }
        // A visible corner mark: three short diagonal strokes.
        let mark = egui::Stroke::new(1.0, Palette::of(&visuals).grid);
        for step in [4.0, 8.0, 12.0] {
            child.painter().line_segment(
                [
                    rect.max - egui::vec2(step, 3.0),
                    rect.max - egui::vec2(3.0, step),
                ],
                mark,
            );
        }
    }

    /// **Show…**: point the card at another feed or board. Targets already
    /// on the canvas are disabled (one card per feed or board).
    fn retarget_menu(
        &self,
        ui: &mut egui::Ui,
        canvas: &workspace::Canvas,
        current: &Target,
        actions: &mut Vec<Action>,
    ) {
        let mut chosen = None;
        let muted = Palette::of(ui.visuals()).muted;
        let menu =
            ui.menu_button(
                theme::glyph(icon::ARROWS_LEFT_RIGHT)
                    .size(15.0)
                    .color(muted),
                |ui| {
                    ui.set_min_width(220.0);
                    // Prefixed: a feed and a board may share a title.
                    let choices =
                        self.cache
                            .feeds
                            .iter()
                            .map(|f| {
                                (
                                    Target::Feed(f.info.name.clone()),
                                    format!("Feed: {}", f.info.title),
                                )
                            })
                            .chain(self.cache.boards.iter().map(|b| {
                                (Target::Board(b.info.id), format!("Board: {}", b.info.name))
                            }))
                            .chain([(Target::Archive, "Deleted-board archive".to_string())]);
                    for (target, title) in choices {
                        let placed = canvas.find(&target).is_some();
                        let response = ui.add_enabled(
                            !placed || &target == current,
                            egui::Button::selectable(&target == current, title),
                        );
                        if response.clicked() {
                            chosen = Some(target);
                            ui.close();
                        }
                    }
                },
            );
        theme::name(&menu.response, "Show…");
        menu.response
            .on_hover_text("Point this card at another feed or board");
        if let Some(target) = chosen.filter(|t| t != current) {
            actions.push(Action::Card {
                canvas: canvas.id,
                target: current.clone(),
                op: CardOp::Retarget(target),
            });
        }
    }

    fn card_body(
        &self,
        ui: &mut egui::Ui,
        target: &Target,
        salt: egui::Id,
        scroll: bool,
        actions: &mut Vec<Action>,
    ) {
        let entry = self.cache.contents.get(target);
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
            Some(Contents::Missing) => placeholder(ui, target),
            Some(Contents::Feed(feed)) => {
                feed::show(ui, feed, salt, scroll, &self.cache.boards, actions)
            }
            Some(Contents::Board { items, archived }) => board::show(
                ui,
                target,
                items,
                archived.as_ref(),
                &self.cache.boards,
                &self.ticks,
                salt,
                scroll,
                actions,
            ),
        }
    }
}

/// Follows the pointer while a sidebar entry is dragged toward the canvas.
pub(crate) fn drag_ghost(ctx: &egui::Context, pos: egui::Pos2, title: &str) {
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("card_drag_ghost"),
    ));
    let visuals = ctx.global_style().visuals.clone();
    let p = Palette::of(&visuals);
    let rect = egui::Rect::from_min_size(pos - DROP_GRAB, egui::vec2(220.0, TITLE_HEIGHT));
    let radius = egui::CornerRadius::same(theme::RADIUS_LG);
    painter.add(visuals.window_shadow.as_shape(rect, radius));
    painter.rect(
        rect,
        radius,
        p.surface.gamma_multiply(0.96),
        egui::Stroke::new(1.5, p.accent),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.left_center() + egui::vec2(12.0, 0.0),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::new(13.5, theme::medium()),
        p.text,
    );
}

/// Where the pointer holds a dropped card: on its title bar, near the left.
const DROP_GRAB: egui::Vec2 = egui::vec2(40.0, 14.0);

/// The other ends of an app's worker channels, for tests that play the
/// service's part.
#[cfg(test)]
pub(crate) struct TestEnds {
    pub requests: mpsc::Receiver<Request>,
    pub responses: mpsc::SyncSender<Fetched>,
    pub signals: mpsc::SyncSender<Signal>,
    pub saves: mpsc::Receiver<SaveJob>,
    pub saved: mpsc::SyncSender<SaveDone>,
    pub ops: mpsc::Receiver<WriteOp>,
    pub ops_done: mpsc::SyncSender<OpDone>,
}

#[cfg(test)]
impl App {
    /// As if launched without `--no-auto-start` and with no service binary found.
    pub(crate) fn without_service_binary(mut self) -> Self {
        (self.auto_start, self.auto_start_disabled) = (None, false);
        self
    }

    pub(crate) fn for_tests() -> (Self, TestEnds) {
        let (requests, incoming) = mpsc::sync_channel(1);
        let (outgoing, responses) = mpsc::sync_channel(1);
        let (signal_tx, signals) = mpsc::sync_channel(16);
        let (saves, save_jobs) = mpsc::sync_channel(8);
        let (save_done, saved) = mpsc::sync_channel(8);
        let (ops, op_jobs) = mpsc::sync_channel(8);
        let (op_done, ops_done) = mpsc::sync_channel(8);
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
                ops,
                ops_done,
            },
        );
        let ends = TestEnds {
            requests: incoming,
            responses: outgoing,
            signals: signal_tx,
            saves: save_jobs,
            saved: save_done,
            ops: op_jobs,
            ops_done: op_done,
        };
        (app, ends)
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

/// Save state of the active layout, for the layout bar.
fn layout_status(ui: &mut egui::Ui, entry: &LayoutEntry, actions: &mut Vec<Action>) {
    let p = Palette::of(ui.visuals());
    let quiet = |ui: &mut egui::Ui, text: &str| {
        ui.label(egui::RichText::new(text).size(12.0).color(p.faint))
    };
    if entry.key == LayoutKey::Unsaved {
        quiet(ui, "not saved; use Save as… to keep it");
        return;
    }
    if let Some(error) = &entry.save_error {
        ui.colored_label(p.danger, "not saved (retrying)")
            .on_hover_text(error);
    } else if entry.saving || entry.pending {
        quiet(ui, "saving…");
    } else {
        quiet(ui, "saved");
    }
    if entry.unsaveable {
        quiet(ui, "· the archive card is not saved in layouts");
    }
    if entry.outdated && !entry.pending {
        ui.colored_label(p.warning, "changed in another window");
        if ui
            .small_button("Load saved version")
            .on_hover_text("Replace this window's arrangement with the saved one")
            .clicked()
        {
            actions.push(Action::Revert);
        }
    }
}

/// The canvas behind the cards: a step darker than the cards in both themes.
fn canvas_fill(visuals: &egui::Visuals) -> egui::Color32 {
    Palette::of(visuals).canvas
}

/// A dot grid that moves with the view, so panning reads as movement.
fn dot_grid(ui: &egui::Ui, area: egui::Rect, view: egui::Pos2) {
    const STEP: f32 = 24.0;
    let color = Palette::of(ui.visuals()).grid;
    let painter = ui.painter_at(area);
    let start = |min: f32, offset: f32| min - offset.rem_euclid(STEP) + STEP / 2.0;
    let mut y = start(area.min.y, view.y);
    while y < area.max.y {
        let mut x = start(area.min.x, view.x);
        while x < area.max.x {
            painter.circle_filled(egui::pos2(x, y), 1.2, color);
            x += STEP;
        }
        y += STEP;
    }
}

fn placeholder(ui: &mut egui::Ui, target: &Target) {
    let p = Palette::of(ui.visuals());
    ui.add_space(8.0);
    ui.label(theme::glyph(icon::WARNING_CIRCLE).size(22.0).color(p.faint));
    ui.heading(match target {
        Target::Feed(name) => format!("Feed “{name}” no longer exists"),
        Target::Board(id) => format!("Board {id} no longer exists"),
        Target::Archive => "The archive is unavailable".into(),
    });
    ui.label(
        egui::RichText::new(
            "This card keeps its place in the layout. Use the swap button in its title bar to point it at another feed or board, or close it.",
        )
        .color(p.muted),
    );
    if let Target::Feed(_) = target {
        ui.label(
            egui::RichText::new("A new submission under the same name will appear here.")
                .color(p.faint),
        );
    }
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

pub(crate) fn age(at_ms: i64) -> Duration {
    Duration::from_secs(now_ms().saturating_sub(at_ms).max(0) as u64 / 1000)
}

pub(crate) fn stale(feed: &FeedInfo) -> bool {
    feed.stale_after
        .as_deref()
        .and_then(|v| humantime::parse_duration(v).ok())
        .is_some_and(|limit| {
            (now_ms().saturating_sub(feed.last_submitted_at_ms).max(0) as u128) >= limit.as_millis()
        })
}

/// Why the service's build differs from this window's, and what to do.
pub(crate) fn build_mismatch(service: &str) -> String {
    format!(
        "The service is build {service}; this window is build {}. One of them is out of date. \
         After installing an update, run `callboard upgrade` and reopen this window.",
        callboard_service::BUILD
    )
}

pub(crate) fn link(ui: &mut egui::Ui, url: Option<&str>) {
    if let Some(url) = url {
        if url.starts_with("https://") || url.starts_with("http://") {
            ui.hyperlink_to(url, url);
        } else {
            ui.label(url);
        }
    }
}

/// A duration in its two largest units: "3days 4h", not "3days 4h 12m 5s".
pub(crate) fn coarse(duration: Duration) -> String {
    let text = humantime::format_duration(duration).to_string();
    text.split(' ').take(2).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        backend::List,
        events::Notice,
        feed::{feed_position, snooze_text},
    };
    use callboard_core::store::ItemViewState;

    struct Lists {
        feeds: Vec<FeedInfo>,
        boards: Vec<BoardInfo>,
        layouts: Vec<NamedLayout>,
    }

    fn all_lists(lists: Lists) -> Vec<(List, Result<ListData, String>)> {
        vec![
            (
                List::Feeds,
                Ok(ListData::Feeds(
                    lists.feeds.into_iter().map(feed_summary).collect(),
                )),
            ),
            (
                List::Boards,
                Ok(ListData::Boards(
                    lists.boards.into_iter().map(board_summary).collect(),
                )),
            ),
            (List::Layouts, Ok(layouts(lists.layouts))),
        ]
    }

    fn feed_summary(info: FeedInfo) -> FeedSummary {
        FeedSummary {
            info,
            item_count: 0,
            snoozed_count: 0,
            new_count: 0,
            next_wake_at_ms: None,
        }
    }

    fn board_summary(info: BoardInfo) -> BoardSummary {
        BoardSummary {
            info,
            todo_count: 0,
            open_todo_count: 0,
            note_count: 0,
        }
    }

    fn layouts(layouts: Vec<NamedLayout>) -> ListData {
        ListData::Layouts(layouts, Default::default())
    }
    use callboard_core::{feed::Item, layout::NamedLayout, store::BoardInfo};

    /// A saved layout with one card per target, placed as the sidebar would.
    fn canvas(targets: &[Target]) -> Layout {
        let mut canvas = workspace::Canvas::new(egui::Id::NULL);
        for target in targets {
            canvas.place(target.clone(), None, egui::vec2(1000.0, 700.0));
        }
        canvas.to_layout()
    }
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
                ctx: {
                    // As the real app does before its first frame.
                    let ctx = egui::Context::default();
                    theme::install(&ctx);
                    let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
                    output.textures_delta.clear();
                    ctx
                },
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

    #[test]
    fn ages_keep_their_two_largest_units() {
        assert_eq!(coarse(Duration::from_secs(13 * 3600 + 61)), "13h 1m");
        assert_eq!(coarse(Duration::from_secs(5)), "5s");
        assert_eq!(coarse(Duration::ZERO), "0s");
    }

    #[test]
    fn a_drop_among_shown_items_maps_to_the_whole_order() {
        // b is snoozed and hidden: shown a, c, d.
        let all = ["a", "b", "c", "d"];
        let shown = ["a", "c", "d"];
        // d dropped first: before a.
        assert_eq!(feed_position(&all, &shown, 2, 0), 0);
        // a dropped below c: just after c, past the hidden b.
        assert_eq!(feed_position(&all, &shown, 0, 1), 2);
        // a dropped last.
        assert_eq!(feed_position(&all, &shown, 0, 2), 3);
        // d dropped between a and c: after a, before the hidden b.
        assert_eq!(feed_position(&all, &shown, 2, 1), 1);
        // With the first item hidden, a drop at the top lands before the
        // first shown item and after the hidden one.
        assert_eq!(feed_position(&["x", "y", "z"], &["y", "z"], 1, 0), 1);
    }

    #[test]
    fn snooze_text_names_the_wake_condition() {
        let hour = now_ms() + 60 * 60 * 1000;
        let state = |until: Option<i64>, on_update| ItemViewState {
            snoozed_until_ms: until,
            wake_on_update: on_update,
            snoozed: true,
        };
        assert_eq!(
            snooze_text(&state(Some(hour), false)),
            "Snoozed · wakes in 1h"
        );
        assert_eq!(
            snooze_text(&state(Some(hour), true)),
            "Snoozed · wakes in 1h or when it changes"
        );
        assert_eq!(snooze_text(&state(None, true)), "Snoozed until it changes");
        let later = now_ms() + ((3 * 24 + 4) * 60 + 12) * 60 * 1000 + 30_000;
        assert_eq!(
            snooze_text(&state(Some(later), false)),
            "Snoozed · wakes in 3days 4h"
        );
        assert_eq!(board::count(1, "note"), "1 note");
        assert_eq!(board::count(0, "todo"), "0 todos");
    }

    #[test]
    fn a_list_overlapping_a_save_is_discarded_then_refetched() {
        for same_frame in [false, true] {
            let (mut app, ends) = App::for_tests();
            let now = Instant::now();
            let old = NamedLayout {
                name: "Day".into(),
                layout: canvas(&[Target::Feed("a".into())]),
                updated_at_ms: 1,
            };
            app.layouts.sync_saved(std::slice::from_ref(&old));
            app.apply(Action::Show(Target::Board(2)));
            app.tick(now);
            ends.requests.try_recv().unwrap(); // Leave the old fetch in flight.
            app.tick(now + workspace::SAVE_DELAY);
            let job = ends.saves.try_recv().unwrap();
            let saved = job.layout.clone();
            ends.saved
                .send(SaveDone {
                    result: Ok(NamedLayout {
                        name: job.name.clone(),
                        layout: saved.clone(),
                        updated_at_ms: 2,
                    }),
                    job,
                })
                .unwrap();
            if !same_frame {
                app.tick(now + workspace::SAVE_DELAY);
            }
            ends.responses
                .send(Fetched {
                    lists: vec![(backend::List::Layouts, Ok(layouts(vec![old])))],
                    targets: vec![],
                })
                .unwrap();
            app.tick(now + workspace::SAVE_DELAY);
            assert_eq!(app.layouts.active().to_layout(), saved);
            let next = ends
                .requests
                .try_recv()
                .expect("fresh layout read after save");
            assert!(next.lists.contains(&backend::List::Layouts));
            // A later read is accepted, so edits from other windows still work.
            ends.responses
                .send(Fetched {
                    lists: vec![(
                        backend::List::Layouts,
                        Ok(layouts(vec![NamedLayout {
                            name: "Day".into(),
                            layout: canvas(&[Target::Board(3)]),
                            updated_at_ms: 3,
                        }])),
                    )],
                    targets: vec![],
                })
                .unwrap();
            app.tick(now + workspace::SAVE_DELAY);
            assert_eq!(
                app.layouts.active().targets(),
                BTreeSet::from([Target::Board(3)])
            );
        }
    }

    #[test]
    fn native_close_flushes_all_layouts_and_waits_for_confirmation() {
        let (mut app, ends) = App::for_tests();
        app.layouts.sync_saved(&[NamedLayout {
            name: "Day".into(),
            layout: canvas(&[Target::Feed("a".into())]),
            updated_at_ms: 1,
        }]);
        app.apply(Action::Show(Target::Board(2)));
        // Pending changes in an inactive layout must also be flushed.
        app.apply(Action::Switch(LayoutKey::Unsaved));
        // As the real app does before its first frame.
        let ctx = egui::Context::default();
        theme::install(&ctx);
        ctx.run_ui(egui::RawInput::default(), |_| {})
            .textures_delta
            .clear();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let mut output = ctx.run_ui(input, |ui| app.show(ui));
        output.textures_delta.clear();
        let commands = &output.viewport_output[&egui::ViewportId::ROOT].commands;
        assert!(commands.contains(&egui::ViewportCommand::CancelClose));
        assert!(!commands.contains(&egui::ViewportCommand::Close));
        let job = ends
            .saves
            .try_recv()
            .expect("flush without waiting one second");
        assert_eq!(job.name, "Day");
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| app.show(ui));
        output.textures_delta.clear();
        assert!(
            !output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::Close)
        );
        ends.saved
            .send(SaveDone {
                result: Ok(NamedLayout {
                    name: job.name.clone(),
                    layout: job.layout.clone(),
                    updated_at_ms: 2,
                }),
                job,
            })
            .unwrap();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| app.show(ui));
        output.textures_delta.clear();
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::Close)
        );
    }

    fn feed_info(name: &str) -> FeedInfo {
        FeedInfo {
            name: name.into(),
            title: format!("{name} title"),
            description: None,
            source_url: None,
            stale_after: None,
            new_for: None,
            last_submitted_at_ms: now_ms(),
            error: None,
            last_change: None,
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
            changes: Default::default(),
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
                        layout: canvas(&[Target::Feed("reviews".into()), Target::Board(4)]),
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
                snoozed: 0,
                marked: 0,
            })
        );
        assert_eq!(cache.marker(&Target::Board(4)), Some("deleted"));
        assert_eq!(cache.card_title(&Target::Board(4)), "Board 4 · deleted");
        // The deleted board's card survives in the layout.
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
    fn sidebar_actions_place_cards_and_drop_contents_of_closed_ones() {
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
                        color: None,
                    }],
                    layouts: vec![],
                }),
                targets: vec![],
            })
            .unwrap();
        h.frame();
        assert_eq!(h.app.layouts.active_key(), &LayoutKey::Unsaved);
        h.app.apply(Action::Show(Target::Feed("a".into())));
        h.app.apply(Action::Show(Target::Board(2)));
        h.app.apply(Action::Show(Target::Feed("b".into())));
        // Showing a placed target again reveals it instead of adding a card.
        h.app.apply(Action::Show(Target::Board(2)));
        h.app.apply(Action::Show(Target::Feed("b".into())));
        h.frame();
        let request = h.request();
        assert_eq!(request.targets.len(), 3);
        let canvas = h.app.layouts.active();
        assert_eq!(canvas.cards.len(), 3);
        assert_eq!(canvas.focused_target(), Some(&Target::Feed("b".into())));
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
        // A failed refetch keeps what was loaded and retries only that card.
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
            "failed card waits for its retry"
        );
        let id = h.app.layouts.active().id;
        h.app.apply(Action::Card {
            canvas: id,
            target: Target::Feed("b".into()),
            op: CardOp::Close,
        });
        h.frame();
        assert_eq!(h.app.cache.contents.len(), 2, "closed card's data dropped");
        assert_eq!(
            h.app.layouts.active().focused_target(),
            Some(&Target::Board(2))
        );
        // Retargeting fetches the new target for the same card.
        let rect = h.app.layouts.active().card(&Target::Board(2)).unwrap().rect;
        h.app.apply(Action::Card {
            canvas: id,
            target: Target::Board(2),
            op: CardOp::Retarget(Target::Archive),
        });
        h.frame();
        assert_eq!(h.request().targets, vec![Target::Archive]);
        assert_eq!(
            h.app.layouts.active().card(&Target::Archive).unwrap().rect,
            rect
        );
        // One card per target: retargeting onto a placed target is refused.
        h.app.apply(Action::Card {
            canvas: id,
            target: Target::Archive,
            op: CardOp::Retarget(Target::Feed("a".into())),
        });
        assert!(h.app.layouts.active().find(&Target::Archive).is_some());
    }

    #[test]
    fn card_actions_from_a_replaced_canvas_are_ignored() {
        let mut h = Harness::new();
        h.app.layouts.sync_saved(&[NamedLayout {
            name: "Day".into(),
            layout: canvas(&[Target::Feed("a".into())]),
            updated_at_ms: 0,
        }]);
        let id = h.app.layouts.active().id;
        // Same frame: a revert rebuilds the canvas, then queued actions arrive.
        h.app.apply(Action::Revert);
        h.app.apply(Action::Card {
            canvas: id,
            target: Target::Feed("a".into()),
            op: CardOp::Close,
        });
        h.app.apply(Action::Pan {
            canvas: id,
            delta: egui::vec2(50.0, 0.0),
        });
        let canvas = h.app.layouts.active();
        assert_eq!(canvas.targets().len(), 1, "card kept");
        assert_eq!(canvas.view, egui::Pos2::ZERO, "not panned");
    }

    #[test]
    fn arrangement_changes_auto_save_through_the_saver_and_settle() {
        let mut h = Harness::new();
        h.app.layouts.sync_saved(&[NamedLayout {
            name: "Day".into(),
            layout: canvas(&[Target::Feed("a".into())]),
            updated_at_ms: 0,
        }]);
        h.app.apply(Action::Show(Target::Board(2)));
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
        assert_eq!(job.layout.cards.len(), 2);
        h.saved
            .send(SaveDone {
                result: Ok(NamedLayout {
                    name: job.name.clone(),
                    layout: job.layout.clone(),
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
