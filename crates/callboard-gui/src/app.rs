//! The desktop window: resource sidebar, layout bar, and panel tree.
//! Writes manage layouts and snooze or promote feed items; boards and their
//! items are not yet editable.
use crate::{
    backend::{self, Contents, Fetched, ListData, PromoteKind, Request, Target},
    events::{self, Signal},
    sync::{Link, Outcome, POLL_INTERVAL, Scheduler},
    workspace::{self, LayoutEntry, LayoutKey, Layouts, Pane, Placement},
};
use callboard::lifecycle::Paths;
use callboard_core::{
    layout::{NamedLayout, Panel},
    store::{BoardContents, BoardSummary, Feed, FeedInfo, FeedSummary, ResolvedReference},
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
    Write(WriteOp),
    DismissWriteError,
    Prompt(PromptKind),
    /// Ask to confirm deleting the active saved layout.
    ConfirmDelete,
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
}

pub struct OpDone {
    pub op: WriteOp,
    /// The renamed layout for a rename; `None` otherwise.
    pub result: Result<Option<NamedLayout>, String>,
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
}

/// Confirmation for deleting a saved layout.
pub struct DeletePrompt {
    pub name: String,
    pub error: Option<String>,
    pub waiting: bool,
}

/// The layout-name dialog for "Save as…", "New layout…", and "Rename…".
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
    pub delete_prompt: Option<DeletePrompt>,
    /// The layout last sent as the startup preference (or read from it).
    remembered: Option<LayoutKey>,
    /// The last failed snooze or promotion, until dismissed.
    pub write_error: Option<String>,
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
                        &job.tree,
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
                                    .map(Some)
                            }
                            WriteOp::Delete { name } => backend::delete_layout(paths, auto, name)
                                .await
                                .map(|_| None),
                            WriteOp::Remember { name } => {
                                backend::set_last_layout(paths, auto, name.as_deref())
                                    .await
                                    .map(|_| None)
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
                                    .map(|_| None)
                            }
                            WriteOp::Promote {
                                feed,
                                key,
                                board_id,
                                kind,
                            } => backend::promote(paths, auto, feed, key, *board_id, *kind)
                                .await
                                .map(|_| None),
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
        let stored = done.result.map(|layout| layout.tree);
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
            (WriteOp::Rename { from, .. }, Ok(Some(layout))) => {
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
            // keeps the panel current while the event stream is down.
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
        }
    }

    /// Delete the layout named in the confirmation dialog.
    pub fn confirm_delete(&mut self) {
        let Some(prompt) = &mut self.delete_prompt else {
            return;
        };
        if !self.layouts.begin_op(&prompt.name) {
            prompt.error = Some("Wait for the current save to finish".into());
            return;
        }
        let op = WriteOp::Delete {
            name: prompt.name.clone(),
        };
        match self.channels.ops.try_send(op) {
            Ok(()) => {
                prompt.waiting = true;
                prompt.error = None;
            }
            Err(_) => {
                self.layouts.end_op(&prompt.name);
                prompt.error = Some("The layout worker is busy or stopped; try again".into());
            }
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
            PromptKind::Rename => unreachable!("handled above"),
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
                    _ => String::new(),
                };
                self.prompt = Some(NamePrompt {
                    kind,
                    text,
                    error: None,
                    waiting: false,
                })
            }
            Action::ConfirmDelete => {
                if let LayoutKey::Saved(name) = self.layouts.active_key() {
                    self.delete_prompt = Some(DeletePrompt {
                        name: name.clone(),
                        error: None,
                        waiting: false,
                    });
                }
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
        let saves = if self.closing {
            if self.close_error.is_none() {
                self.layouts.flush_saves(now)
            } else {
                Vec::new()
            }
        } else {
            self.layouts.due_saves(now)
        };
        for (name, tree) in saves {
            let job = SaveJob {
                name: name.clone(),
                tree,
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
        // Computed once per frame: both walk every working tree.
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
        egui::CentralPanel::default().show(ui, |ui| self.panels(ui, &mut actions));
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
            if matches!(self.layouts.active_key(), LayoutKey::Saved(_)) {
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
                    actions.push(Action::ConfirmDelete);
                }
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

    /// Confirms deleting a saved layout. Escape cancels.
    fn delete_window(&mut self, ui: &mut egui::Ui) {
        let Some(prompt) = &self.delete_prompt else {
            return;
        };
        let mut confirm = false;
        let mut cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
        egui::Window::new("Delete layout")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                ui.label(format!("Delete the layout “{}”?", prompt.name));
                ui.weak("Feeds and boards are not affected.");
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
        let workspace = self.layouts.active();
        let focused = workspace.focused_target().cloned();
        let has_focus = focused.is_some();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.menu_button("Add panel…", |ui| {
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
                for (target, title) in choices {
                    ui.push_id(&target, |ui| {
                        ui.menu_button(title, |ui| {
                            placement_menu(ui, &target, has_focus, actions);
                        });
                    });
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
            for board in self.cache.boards.iter().map(|b| &b.info) {
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
            ui.weak(
                "Click to show. Use Add panel… for a new tab or split, or right-click an entry.",
            );
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
                placement_menu(ui, target, has_focus, actions);
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
            ui.label("Click a feed or board in the sidebar to show it here. Use Add panel… to open a new tab or split.");
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
                Some(Contents::Feed(feed)) => {
                    show_feed(ui, feed, salt, &self.cache.boards, self.actions)
                }
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

/// Shared by the visible Add panel menu and resource context menus.
fn placement_menu(ui: &mut egui::Ui, target: &Target, has_focus: bool, actions: &mut Vec<Action>) {
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

fn show_feed(
    ui: &mut egui::Ui,
    feed: &Feed,
    salt: egui::Id,
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
                    ui.push_id(&item.key, |ui| {
                        item_actions(ui, &feed.info.name, &item.key, snoozed, boards, actions);
                    });
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
    fn a_list_overlapping_a_save_is_discarded_then_refetched() {
        for same_frame in [false, true] {
            let (mut app, ends) = App::for_tests();
            let now = Instant::now();
            let old = NamedLayout {
                name: "Day".into(),
                tree: Panel::Feed { name: "a".into() },
                updated_at_ms: 1,
            };
            app.layouts.sync_saved(std::slice::from_ref(&old));
            app.apply(Action::Place(Target::Board(2), Placement::Right));
            app.tick(now);
            ends.requests.try_recv().unwrap(); // Leave the old fetch in flight.
            app.tick(now + workspace::SAVE_DELAY);
            let job = ends.saves.try_recv().unwrap();
            let tree = job.tree.clone();
            ends.saved
                .send(SaveDone {
                    result: Ok(NamedLayout {
                        name: job.name.clone(),
                        tree: tree.clone(),
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
            assert_eq!(app.layouts.active().panel(), tree);
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
                            tree: Panel::Board { id: 3 },
                            updated_at_ms: 3,
                        }])),
                    )],
                    targets: vec![],
                })
                .unwrap();
            app.tick(now + workspace::SAVE_DELAY);
            assert_eq!(app.layouts.active().panel(), Panel::Board { id: 3 });
        }
    }

    #[test]
    fn native_close_flushes_all_layouts_and_waits_for_confirmation() {
        let (mut app, ends) = App::for_tests();
        app.layouts.sync_saved(&[NamedLayout {
            name: "Day".into(),
            tree: Panel::Feed { name: "a".into() },
            updated_at_ms: 1,
        }]);
        app.apply(Action::Place(Target::Board(2), Placement::Right));
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
                    tree: job.tree.clone(),
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
