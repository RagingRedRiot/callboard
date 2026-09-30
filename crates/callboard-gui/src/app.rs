//! The desktop window: resource sidebar, layout bar, and canvas of cards.
//! Writes manage layouts, snooze or promote feed items, and edit boards.
use crate::{
    backend::{self, Contents, Fetched, ListData, PromoteKind, Request, Target},
    board::{self, BoardOp, Kind},
    events::{self, Signal},
    sync::{Link, Outcome, POLL_INTERVAL, Scheduler},
    workspace::{self, CardState, LayoutEntry, LayoutKey, Layouts, TITLE_HEIGHT},
};
use callboard::lifecycle::Paths;
use callboard_core::{
    layout::{Layout, NamedLayout},
    store::{BoardInfo, BoardSummary, Feed, FeedInfo, FeedSummary, ItemViewState},
};
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
                }),
                Target::Board(id) => self.board(*id).map(|b| Counts {
                    shown: b.todo_count + b.note_count,
                    snoozed: 0,
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
                })
            }
            Contents::Board { items: board, .. } => Some(Counts {
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
            remembered: None,
            write_error: None,
            canvas_area: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0)),
            worker_error: None,
            discard_layout_list: false,
            pending_saves: 0,
            closing: false,
            close_error: None,
            allow_close: false,
        }
    }

    fn receive(&mut self, now: Instant) {
        while let Ok(signal) = self.channels.signals.try_recv() {
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
            (WriteOp::Snooze { feed, .. }, Ok(_)) => self.scheduler.want(Target::Feed(feed)),
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

    fn board_done(&mut self, op: BoardOp, result: Result<Reply, String>) {
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
        egui::Panel::top("layout_bar")
            .show(ui, |ui| self.layout_bar(ui, now, &entries, &mut actions));
        if let Some(error) = self.worker_error.clone().or(self.cache.error.clone()) {
            egui::Panel::top("error_bar").show(ui, |ui| self.error_bar(ui, &error));
        }
        if let Some(error) = &self.write_error {
            egui::Panel::top("write_error_bar").show(ui, |ui| {
                ui.horizontal(|ui| {
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
            .default_size(240.0)
            .show(ui, |ui| self.sidebar(ui, &entries, &placed, &mut actions));
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(ui.visuals().extreme_bg_color))
            .show(ui, |ui| self.canvas(ui, &mut actions));
        if self.closing {
            self.close_window(ui);
        } else {
            self.prompt_window(ui);
            self.delete_window(ui);
            for action in actions {
                self.apply(action);
            }
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
            if let LayoutKey::Saved(name) = self.layouts.active_key() {
                if ui
                    .small_button("Rename…")
                    .on_hover_text("Rename this layout")
                    .clicked()
                {
                    actions.push(Action::Prompt(PromptKind::Rename));
                }
                if ui
                    .small_button("Delete…")
                    .on_hover_text("Delete this saved layout")
                    .clicked()
                {
                    actions.push(Action::ConfirmDelete(DeleteSubject::Layout(name.clone())));
                }
            }
            ui.separator();
            if ui
                .add_enabled(
                    !self.layouts.active().cards.is_empty(),
                    egui::Button::new("Show all").small(),
                )
                .on_hover_text("Pan to the cards, including any panned out of sight")
                .clicked()
            {
                actions.push(Action::ShowAll);
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
        let canvas = self.layouts.active();
        let front = canvas.focused_target().cloned();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.menu_button("Add card…", |ui| {
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
                        .chain(
                            self.cache.boards.iter().map(|b| {
                                (Target::Board(b.info.id), format!("Board: {}", b.info.name))
                            }),
                        )
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
            });
            ui.separator();
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
            for feed in self.cache.feeds.iter().map(|f| &f.info) {
                let target = Target::Feed(feed.name.clone());
                self.entry(ui, &target, placed, &front, actions, |ui| {
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
            ui.horizontal(|ui| {
                ui.heading("Boards");
                if ui
                    .small_button("New board…")
                    .on_hover_text("Create a board and place its card")
                    .clicked()
                {
                    actions.push(Action::Prompt(PromptKind::NewBoard));
                }
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
            ui.separator();
            self.entry(ui, &Target::Archive, placed, &front, actions, |ui| {
                ui.label("Items archived from deleted boards. Not saved in layouts.");
            });
            ui.separator();
            ui.weak("Click to show a card. Drag an entry onto the canvas to place it there.");
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
        ui.horizontal(|ui| {
            let title = self.cache.title(target);
            let text = if placed.contains(target) {
                egui::RichText::new(&title).strong()
            } else {
                egui::RichText::new(&title)
            };
            let response = ui
                .selectable_label(front.as_ref() == Some(target), text)
                .interact(egui::Sense::click_and_drag())
                .on_hover_ui(|ui| {
                    details(ui);
                    if placed.contains(target) {
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
                if placed.contains(target) && ui.button("Remove card").clicked() {
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

    /// One line under the layout bar, so an outage does not shift the canvas
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
        for pos in presses {
            if let Some(i) = topmost_at(pos)
                && i + 1 < canvas.cards.len()
            {
                actions.push(card_action(&canvas.cards[i].target, CardOp::Raise));
            }
        }

        // Registered first, so every card widget sits above it.
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
            ui.put(
                egui::Rect::from_center_size(area.center(), egui::vec2(560.0, 60.0)),
                egui::Label::new(
                    egui::RichText::new(
                        "No cards in this layout. Click a feed or board in the sidebar, or drag one here.",
                    )
                    .weak(),
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
        let stroke = if front {
            visuals.selection.stroke
        } else {
            visuals.window_stroke
        };
        child.painter().rect(
            rect,
            6.0,
            visuals.window_fill,
            stroke,
            egui::StrokeKind::Inside,
        );
        let title_rect =
            egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), TITLE_HEIGHT));
        child.painter().rect_filled(
            title_rect.shrink(1.0),
            egui::CornerRadius {
                nw: 5,
                ne: 5,
                sw: if card.collapsed { 5 } else { 0 },
                se: if card.collapsed { 5 } else { 0 },
            },
            visuals.faint_bg_color,
        );
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
                .max_rect(title_rect.shrink2(egui::vec2(8.0, 3.0)))
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        bar.set_clip_rect(title_rect.intersect(area));
        if bar
            .small_button("×")
            .on_hover_text("Remove this card from the layout")
            .clicked()
        {
            actions.push(act(CardOp::Close));
        }
        let (fold, hint) = if card.collapsed {
            ("+", "Expand")
        } else {
            ("−", "Collapse to the title bar")
        };
        if bar.small_button(fold).on_hover_text(hint).clicked() {
            actions.push(act(CardOp::ToggleCollapsed));
        }
        self.retarget_menu(&mut bar, canvas, target, actions);
        bar.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            let title = self.cache.card_title(target);
            ui.add(
                egui::Label::new(egui::RichText::new(title).strong())
                    .truncate()
                    .selectable(false),
            );
        });

        if card.collapsed {
            return;
        }
        let body = egui::Rect::from_min_max(
            egui::pos2(rect.min.x + 8.0, rect.min.y + TITLE_HEIGHT + 6.0),
            rect.max - egui::vec2(8.0, 8.0),
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
        let mark = egui::Stroke::new(1.0, visuals.weak_text_color());
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
        egui::ComboBox::from_id_salt(canvas.id.with(("retarget", current)))
            .selected_text("Show…")
            .width(64.0)
            .show_ui(ui, |ui| {
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
                        .chain(
                            self.cache.boards.iter().map(|b| {
                                (Target::Board(b.info.id), format!("Board: {}", b.info.name))
                            }),
                        )
                        .chain([(Target::Archive, "Deleted-board archive".to_string())]);
                for (target, title) in choices {
                    let placed = canvas.find(&target).is_some();
                    let response = ui.add_enabled(
                        !placed || &target == current,
                        egui::Button::selectable(&target == current, title),
                    );
                    if response.clicked() {
                        chosen = Some(target);
                    }
                }
            })
            .response
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
                show_feed(ui, feed, salt, scroll, &self.cache.boards, actions)
            }
            Some(Contents::Board { items, archived }) => board::show(
                ui,
                target,
                items,
                archived.as_ref(),
                &self.cache.boards,
                salt,
                scroll,
                actions,
            ),
        }
    }
}

/// Follows the pointer while a sidebar entry is dragged toward the canvas.
fn drag_ghost(ctx: &egui::Context, pos: egui::Pos2, title: &str) {
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("card_drag_ghost"),
    ));
    let visuals = ctx.global_style().visuals.clone();
    let rect = egui::Rect::from_min_size(pos - DROP_GRAB, egui::vec2(200.0, TITLE_HEIGHT));
    painter.rect(
        rect,
        5.0,
        visuals.window_fill.gamma_multiply(0.9),
        visuals.selection.stroke,
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.left_center() + egui::vec2(8.0, 0.0),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::proportional(13.0),
        visuals.strong_text_color(),
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
        ui.weak("· the archive card is not saved in layouts");
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
        "This card keeps its place in the layout. Use Show… in its title bar to point it at another feed or board, or close it.",
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

pub(crate) fn link(ui: &mut egui::Ui, url: Option<&str>) {
    if let Some(url) = url {
        if url.starts_with("https://") || url.starts_with("http://") {
            ui.hyperlink_to(url, url);
        } else {
            ui.label(url);
        }
    }
}

/// When a snoozed item wakes (DESIGN.md §4.1: whichever condition comes first).
fn snooze_text(state: &ItemViewState) -> String {
    let until = state.snoozed_until_ms.map(|at| {
        // Whole minutes, rounded up: "wakes in 1h", not "59m 59s".
        let left_ms = at.saturating_sub(now_ms()).max(0) as u64;
        let left = Duration::from_secs(left_ms.div_ceil(60_000) * 60);
        // The two largest units are precise enough: "3days 4h", not "… 12m".
        let text = humantime::format_duration(left).to_string();
        let coarse: Vec<&str> = text.split(' ').take(2).collect();
        format!("wakes in {}", coarse.join(" "))
    });
    match (until, state.wake_on_update) {
        (Some(until), true) => format!("Snoozed · {until} or when it changes"),
        (Some(until), false) => format!("Snoozed · {until}"),
        (None, _) => "Snoozed until it changes".into(),
    }
}

/// Snooze choices: label and duration.
const SNOOZE_FOR: [(&str, Duration); 4] = [
    ("For 1 hour", Duration::from_secs(60 * 60)),
    ("For 4 hours", Duration::from_secs(4 * 60 * 60)),
    ("For 1 day", Duration::from_secs(24 * 60 * 60)),
    ("For 1 week", Duration::from_secs(7 * 24 * 60 * 60)),
];

/// Snooze and promote actions for one feed item.
fn item_actions(
    ui: &mut egui::Ui,
    feed: &str,
    key: &str,
    snoozed: bool,
    boards: &[BoardSummary],
    actions: &mut Vec<Action>,
) {
    let snooze = |until_ms: Option<i64>, on_update: bool| {
        Action::Write(WriteOp::Snooze {
            feed: feed.to_owned(),
            key: key.to_owned(),
            until_ms,
            on_update,
        })
    };
    ui.horizontal(|ui| {
        if snoozed {
            if ui.small_button("Unsnooze").clicked() {
                actions.push(snooze(None, false));
            }
        } else {
            ui.menu_button("Snooze", |ui| {
                for (label, duration) in SNOOZE_FOR {
                    if ui.button(label).clicked() {
                        let until = now_ms().saturating_add(duration.as_millis() as i64);
                        actions.push(snooze(Some(until), false));
                        ui.close();
                    }
                }
                if ui
                    .button("Until it changes")
                    .on_hover_text("Wake when the feed submits different content for it")
                    .clicked()
                {
                    actions.push(snooze(None, true));
                    ui.close();
                }
            });
        }
        ui.menu_button("Promote", |ui| {
            if boards.is_empty() {
                ui.weak("No boards yet");
            }
            for board in boards {
                ui.push_id(board.info.id, |ui| {
                    ui.menu_button(&board.info.name, |ui| {
                        for (label, kind) in [
                            ("As todo", PromoteKind::Todo),
                            ("As note", PromoteKind::Note),
                        ] {
                            if ui.button(label).clicked() {
                                actions.push(Action::Write(WriteOp::Promote {
                                    feed: feed.to_owned(),
                                    key: key.to_owned(),
                                    board_id: board.info.id,
                                    kind,
                                }));
                                ui.close();
                            }
                        }
                    });
                });
            }
        });
    });
}

/// Only the topmost card under the pointer scrolls with the wheel: with a
/// multiplier of 0 a covered card neither scrolls nor consumes the wheel.
fn wheel(scroll: bool) -> egui::Vec2 {
    egui::Vec2::splat(if scroll { 1.0 } else { 0.0 })
}

fn show_feed(
    ui: &mut egui::Ui,
    feed: &Feed,
    salt: egui::Id,
    scroll: bool,
    boards: &[BoardSummary],
    actions: &mut Vec<Action>,
) {
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
        .wheel_scroll_multiplier(wheel(scroll))
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
                    ui.push_id(&item.key, |ui| {
                        ui.horizontal(|ui| {
                            let grip = board::handle(ui, "Drag onto a board to promote")
                                .on_hover_text("Drag onto a board card to promote it");
                            ui.strong(&item.title);
                            if grip.dragged() {
                                egui::DragAndDrop::set_payload(
                                    ui.ctx(),
                                    board::FeedDrag {
                                        feed: feed.info.name.clone(),
                                        key: item.key.clone(),
                                        title: item.title.clone(),
                                    },
                                );
                                if let Some(pos) = ui.ctx().pointer_interact_pos() {
                                    drag_ghost(ui.ctx(), pos, &item.title);
                                }
                            }
                        });
                    });
                    if let Some(state) = feed.view_state.get(&item.key).filter(|s| s.snoozed) {
                        ui.weak(snooze_text(state));
                    }
                    if let Some(body) = &item.body {
                        ui.label(body);
                    }
                    link(ui, item.url.as_deref());
                    if !item.tags.is_empty() {
                        ui.weak(item.tags.join(" · "));
                    }
                    ui.push_id(&item.key, |ui| {
                        item_actions(ui, &feed.info.name, &item.key, snoozed, boards, actions);
                    });
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
        let ctx = egui::Context::default();
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
                snoozed: 0
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
