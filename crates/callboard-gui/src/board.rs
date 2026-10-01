//! Board cards: todos, notes, and the writes that edit them (DESIGN.md §6.3).
use crate::{
    app::{Action, DeleteSubject, PromptKind, WriteOp},
    backend::{PromoteKind, Target},
};
use callboard_core::store::{
    BoardContents, BoardItem, BoardSummary, Note, ResolvedReference, Todo,
};
use eframe::egui;
use serde_json::{Value, json};

/// The deleted-board archive's board ID (DESIGN.md §8.2).
pub const ARCHIVE_BOARD: i64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Todo,
    Note,
}

impl Kind {
    fn resource(self) -> &'static str {
        match self {
            Kind::Todo => "todos",
            Kind::Note => "notes",
        }
    }

    pub fn noun(self) -> &'static str {
        match self {
            Kind::Todo => "todo",
            Kind::Note => "note",
        }
    }
}

/// A write to a board or its items.
#[derive(Debug, Clone, PartialEq)]
pub enum BoardOp {
    Create {
        name: String,
    },
    Rename {
        id: i64,
        name: String,
    },
    /// Deletes the board; its items, archived ones included, move to the
    /// deleted-board archive.
    Delete {
        id: i64,
    },
    AddTodo {
        board: i64,
        title: String,
    },
    AddNote {
        board: i64,
        body: String,
    },
    /// Change an item on `board` (the archive for restores from it). The
    /// patch holds only changed fields (DESIGN.md §8.1).
    Patch {
        kind: Kind,
        id: i64,
        board: i64,
        patch: Value,
    },
    ArchiveDone {
        board: i64,
        ids: Vec<i64>,
    },
    DeleteItem {
        kind: Kind,
        id: i64,
        board: i64,
    },
}

impl BoardOp {
    /// The requests to send, in order; the first failure stops the rest.
    pub fn requests(&self) -> Vec<(&'static str, String, Value)> {
        match self {
            BoardOp::Create { name } => vec![("POST", "/boards".into(), json!({ "name": name }))],
            BoardOp::Rename { id, name } => {
                vec![("PATCH", format!("/boards/{id}"), json!({ "name": name }))]
            }
            BoardOp::Delete { id } => vec![(
                "DELETE",
                format!("/boards/{id}"),
                json!({ "archive_contents": true }),
            )],
            BoardOp::AddTodo { board, title } => vec![(
                "POST",
                format!("/boards/{board}/todos"),
                json!({ "title": title }),
            )],
            BoardOp::AddNote { board, body } => vec![(
                "POST",
                format!("/boards/{board}/notes"),
                json!({ "body": body }),
            )],
            BoardOp::Patch {
                kind, id, patch, ..
            } => vec![("PATCH", format!("/{}/{id}", kind.resource()), patch.clone())],
            BoardOp::ArchiveDone { ids, .. } => ids
                .iter()
                .map(|id| ("PATCH", format!("/todos/{id}"), json!({ "archived": true })))
                .collect(),
            BoardOp::DeleteItem { kind, id, .. } => {
                vec![("DELETE", format!("/{}/{id}", kind.resource()), json!(null))]
            }
        }
    }

    /// Boards whose contents change, refetched on success without waiting
    /// for the change notice. A created board's ID comes from the reply.
    pub fn boards(&self) -> Vec<i64> {
        match self {
            BoardOp::Create { .. } => vec![],
            BoardOp::Rename { id, .. } => vec![*id],
            BoardOp::Delete { id } => vec![*id, ARCHIVE_BOARD],
            BoardOp::AddTodo { board, .. }
            | BoardOp::AddNote { board, .. }
            | BoardOp::ArchiveDone { board, .. }
            | BoardOp::DeleteItem { board, .. } => vec![*board],
            BoardOp::Patch { board, patch, .. } => {
                let mut boards = vec![*board];
                boards.extend(patch.get("board_id").and_then(Value::as_i64));
                boards
            }
        }
    }

    /// Shown with the service's error.
    pub fn failure(&self) -> String {
        match self {
            BoardOp::Create { .. } => "Could not create the board".into(),
            BoardOp::Rename { .. } => "Could not rename the board".into(),
            BoardOp::Delete { .. } => "Could not delete the board".into(),
            BoardOp::AddTodo { .. } => "Could not add the todo".into(),
            BoardOp::AddNote { .. } => "Could not add the note".into(),
            BoardOp::Patch { kind, .. } => format!("Could not change the {}", kind.noun()),
            BoardOp::ArchiveDone { .. } => "Could not archive the done todos".into(),
            BoardOp::DeleteItem { kind, .. } => format!("Could not delete the {}", kind.noun()),
        }
    }
}

/// A feed item being dragged toward a board card (DESIGN.md §6.3).
#[derive(Debug, Clone, PartialEq)]
pub struct FeedDrag {
    pub feed: String,
    pub key: String,
    pub title: String,
}

/// Feed item handles drawn in the last frame, with their cards. Pressing one
/// starts a drag toward another card, so it does not raise its own card,
/// which could then cover the board it is dragged to.
#[derive(Clone, Default)]
struct KeepInPlace(Vec<(egui::Rect, Target)>);

fn keep_in_place_id() -> egui::Id {
    egui::Id::new("callboard_keep_in_place")
}

/// Record a visible handle that does not raise `card` when pressed.
pub(crate) fn keep_in_place(ui: &egui::Ui, rect: egui::Rect, card: &Target) {
    let rect = rect.intersect(ui.clip_rect());
    ui.ctx().data_mut(|d| {
        d.get_temp_mut_or_default::<KeepInPlace>(keep_in_place_id())
            .0
            .push((rect, card.clone()))
    });
}

/// The handles recorded since the last call (the previous frame's).
pub(crate) fn take_keep_in_place(ctx: &egui::Context) -> Vec<(egui::Rect, Target)> {
    ctx.data_mut(|d| {
        std::mem::take(
            &mut d
                .get_temp_mut_or_default::<KeepInPlace>(keep_in_place_id())
                .0,
        )
    })
}

/// A todo ticked or unticked here, shown at once until a reload agrees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tick {
    pub done: bool,
    pub board: i64,
    /// The service stored it.
    pub confirmed: bool,
    /// Reloads since then that still showed the old state.
    pub stale_reloads: u8,
}

/// Ticks by todo ID.
pub type Ticks = std::collections::BTreeMap<i64, Tick>;

fn write(op: BoardOp) -> Action {
    Action::Write(WriteOp::Board(op))
}

/// Sticky-note colors: stored value and menu label.
pub const COLORS: [(&str, &str); 5] = [
    ("yellow", "Yellow"),
    ("green", "Green"),
    ("blue", "Blue"),
    ("pink", "Pink"),
    ("purple", "Purple"),
];

fn note_fill(color: Option<&str>, visuals: &egui::Visuals) -> egui::Color32 {
    color
        .and_then(|c| color_fill(c, visuals))
        .unwrap_or(visuals.faint_bg_color)
}

/// A named color's fill for notes and feed items (DESIGN.md §3.2): muted in
/// the dark theme, pastel in the light one, so text reads on both. `None`
/// for a name this version does not know.
pub(crate) fn color_fill(color: &str, visuals: &egui::Visuals) -> Option<egui::Color32> {
    let (dark, light) = match color {
        "red" => ([88, 34, 34], [255, 210, 206]),
        "orange" => ([86, 52, 24], [255, 224, 190]),
        "yellow" => ([74, 66, 28], [255, 244, 179]),
        "green" => ([34, 62, 40], [208, 238, 208]),
        "blue" => ([30, 50, 78], [208, 226, 255]),
        "purple" => ([54, 42, 80], [230, 216, 255]),
        "pink" => ([78, 38, 56], [255, 216, 230]),
        "gray" => ([58, 58, 62], [226, 226, 230]),
        _ => return None,
    };
    let [r, g, b] = if visuals.dark_mode { dark } else { light };
    Some(egui::Color32::from_rgb(r, g, b))
}

/// An item being edited in place, kept in egui's memory per card.
#[derive(Debug, Clone, PartialEq)]
struct Draft {
    kind: Kind,
    id: i64,
    title: String,
    body: String,
    url: String,
    color: Option<String>,
    error: Option<String>,
    /// Focus the first field on the next frame.
    focus: bool,
}

impl Draft {
    fn todo(todo: &Todo) -> Self {
        Self {
            kind: Kind::Todo,
            id: todo.id,
            title: todo.title.clone(),
            body: todo.body.clone().unwrap_or_default(),
            url: todo.url.clone().unwrap_or_default(),
            color: None,
            error: None,
            focus: false,
        }
    }

    fn note(note: &Note) -> Self {
        Self {
            kind: Kind::Note,
            id: note.id,
            title: note.title.clone().unwrap_or_default(),
            body: note.body.clone(),
            url: note.url.clone().unwrap_or_default(),
            color: note.color.clone(),
            error: None,
            focus: false,
        }
    }

    /// The fields that differ from `original`; an empty optional field
    /// clears it. `None` when the draft cannot be saved (`error` says why).
    fn patch(&mut self, original: &Draft) -> Option<Value> {
        let title = self.title.trim();
        if self.kind == Kind::Todo && title.is_empty() {
            self.error = Some("A todo needs a title".into());
            return None;
        }
        let optional = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_owned());
        let mut patch = serde_json::Map::new();
        if title != original.title {
            patch.insert(
                "title".into(),
                match self.kind {
                    Kind::Todo => json!(title),
                    Kind::Note => json!(optional(title)),
                },
            );
        }
        if self.body != original.body {
            patch.insert(
                "body".into(),
                match self.kind {
                    Kind::Todo => json!(optional(&self.body)),
                    Kind::Note => json!(self.body),
                },
            );
        }
        if self.url.trim() != original.url {
            patch.insert("url".into(), json!(optional(&self.url)));
        }
        if self.color != original.color {
            patch.insert("color".into(), json!(self.color));
        }
        Some(Value::Object(patch))
    }
}

/// Per-card state in egui's memory: survives frames, not restarts.
struct Memory {
    salt: egui::Id,
}

impl Memory {
    fn get<T: Clone + Send + Sync + 'static>(&self, ui: &egui::Ui, key: &str) -> Option<T> {
        ui.data(|d| d.get_temp(self.salt.with(key)))
    }

    fn set<T: Clone + Send + Sync + 'static>(&self, ui: &egui::Ui, key: &str, value: Option<T>) {
        let id = self.salt.with(key);
        ui.data_mut(|d| match value {
            Some(value) => {
                d.insert_temp(id, value);
            }
            None => d.remove::<T>(id),
        });
    }
}

/// What every row needs to know about the card it is on.
struct Card<'a> {
    /// The board's ID, or [`ARCHIVE_BOARD`] for the deleted-board archive.
    board: i64,
    boards: &'a [BoardSummary],
    memory: Memory,
    ticks: &'a Ticks,
}

impl Card<'_> {
    fn done(&self, todo: &Todo) -> bool {
        self.ticks.get(&todo.id).map_or(todo.done, |t| t.done)
    }

    fn archive(&self) -> bool {
        self.board == ARCHIVE_BOARD
    }

    fn patch(&self, kind: Kind, id: i64, patch: Value) -> Action {
        write(BoardOp::Patch {
            kind,
            id,
            board: self.board,
            patch,
        })
    }
}

/// Draw a board card's body: a user board (`archived` holds its archived
/// items) or the deleted-board archive. `topmost` says the card is the top
/// one under the pointer: it scrolls with the wheel and takes drops.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut egui::Ui,
    target: &Target,
    items: &BoardContents,
    archived: Option<&BoardContents>,
    boards: &[BoardSummary],
    ticks: &Ticks,
    salt: egui::Id,
    topmost: bool,
    actions: &mut Vec<Action>,
) {
    let card = Card {
        ticks,
        board: match target {
            Target::Board(id) => *id,
            _ => ARCHIVE_BOARD,
        },
        boards,
        memory: Memory { salt },
    };
    let open = items.todos.iter().filter(|t| !card.done(&t.item)).count();
    let done: Vec<i64> = items
        .todos
        .iter()
        .filter(|t| card.done(&t.item))
        .map(|t| t.item.id)
        .collect();
    ui.horizontal(|ui| {
        // Archived todos are neither open nor done.
        ui.label(if card.archive() {
            format!(
                "{} · {}",
                count(items.todos.len(), "todo"),
                count(items.notes.len(), "note")
            )
        } else {
            format!(
                "{} ({open} open) · {}",
                count(items.todos.len(), "todo"),
                count(items.notes.len(), "note")
            )
        });
        if card.archive() {
            return;
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.menu_button("Board", |ui| {
                if ui.button("Rename…").clicked() {
                    actions.push(Action::Prompt(PromptKind::RenameBoard(card.board)));
                    ui.close();
                }
                if ui
                    .add_enabled(!done.is_empty(), egui::Button::new("Archive done"))
                    .clicked()
                {
                    actions.push(write(BoardOp::ArchiveDone {
                        board: card.board,
                        ids: done.clone(),
                    }));
                    ui.close();
                }
                if ui.button("Delete board…").clicked() {
                    let name = items.board.as_ref().map(|b| b.name.clone());
                    actions.push(Action::ConfirmDelete(DeleteSubject::Board {
                        id: card.board,
                        name: name.unwrap_or_default(),
                    }));
                    ui.close();
                }
            });
        });
    });
    // A solid bar keeps its own space, so it never covers the item menus.
    ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
    egui::ScrollArea::vertical()
        .id_salt(salt.with("board_items"))
        .auto_shrink([false, false])
        .wheel_scroll_multiplier(egui::Vec2::splat(if topmost { 1.0 } else { 0.0 }))
        .show(ui, |ui| {
            if card.archive() {
                ui.weak("Items from deleted boards. Restore one to a board, or delete it.");
                if items.todos.is_empty() && items.notes.is_empty() {
                    ui.weak("The archive is empty.");
                }
                archived_items(ui, &card, items, actions);
                return;
            }
            // Notes beside the todos when there is room for both.
            let (todo_list, note_list) = if ui.available_width() >= 600.0 {
                ui.columns(2, |columns| {
                    (
                        todos(&mut columns[0], &card, &items.todos, actions),
                        notes(&mut columns[1], &card, &items.notes, actions),
                    )
                })
            } else {
                let todo_list = todos(ui, &card, &items.todos, actions);
                ui.add_space(8.0);
                (todo_list, notes(ui, &card, &items.notes, actions))
            };
            if topmost {
                promote_drop(ui, &card, todo_list, note_list, actions);
            }
            if let Some(archived) = archived {
                ui.add_space(8.0);
                let n = archived.todos.len() + archived.notes.len();
                let mut show = card.memory.get(ui, "show_archived").unwrap_or(false);
                if ui
                    .checkbox(&mut show, format!("Show archived ({n})"))
                    .changed()
                {
                    card.memory.set(ui, "show_archived", show.then_some(true));
                }
                if show {
                    archived_items(ui, &card, archived, actions);
                }
            }
        });
}

/// "1 note", "2 notes".
pub(crate) fn count(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

/// A single-line field that adds an item on Enter and stays focused.
fn add_field(ui: &mut egui::Ui, card: &Card, kind: Kind, actions: &mut Vec<Action>) {
    let key = match kind {
        Kind::Todo => "new_todo",
        Kind::Note => "new_note",
    };
    let mut text: String = card.memory.get(ui, key).unwrap_or_default();
    let response = ui.add(
        egui::TextEdit::singleline(&mut text)
            .id(card.memory.salt.with(key))
            .hint_text(match kind {
                Kind::Todo => "Add a todo",
                Kind::Note => "Add a note",
            })
            .desired_width(f32::INFINITY),
    );
    if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        let entered = text.trim().to_owned();
        if !entered.is_empty() {
            actions.push(write(match kind {
                Kind::Todo => BoardOp::AddTodo {
                    board: card.board,
                    title: entered,
                },
                Kind::Note => BoardOp::AddNote {
                    board: card.board,
                    body: entered,
                },
            }));
            text.clear();
        }
        response.request_focus();
    }
    card.memory.set(ui, key, (!text.is_empty()).then_some(text));
}

/// The todo list; returns its area.
fn todos(
    ui: &mut egui::Ui,
    card: &Card,
    todos: &[BoardItem<Todo>],
    actions: &mut Vec<Action>,
) -> egui::Rect {
    ui.scope(|ui| todo_list(ui, card, todos, actions))
        .response
        .rect
}

fn todo_list(ui: &mut egui::Ui, card: &Card, todos: &[BoardItem<Todo>], actions: &mut Vec<Action>) {
    ui.strong("Todos");
    add_field(ui, card, Kind::Todo, actions);
    if todos.is_empty() {
        ui.weak("No todos");
    }
    reorderable(
        ui,
        card,
        Kind::Todo,
        todos,
        |t| t.item.id,
        actions,
        |ui, todo, actions| {
            let draft = Draft::todo(&todo.item);
            if let Some(rect) = editor(ui, card, &draft, actions) {
                return (rect, None);
            }
            let row = ui.group(|ui| {
                ui.set_width(ui.available_width());
                item_row(
                    ui,
                    |ui, actions| {
                        let mut done = card.done(&todo.item);
                        // Done todos are struck through and dimmed.
                        let title = if done {
                            egui::RichText::new(&todo.item.title)
                                .strikethrough()
                                .color(ui.visuals().weak_text_color())
                        } else {
                            egui::RichText::new(&todo.item.title)
                        };
                        if ui.checkbox(&mut done, title).changed() {
                            actions.push(card.patch(
                                Kind::Todo,
                                todo.item.id,
                                json!({ "done": done }),
                            ));
                        }
                        details(
                            ui,
                            todo.item.body.as_deref(),
                            todo.item.url.as_deref(),
                            todo,
                        );
                    },
                    |ui, actions| item_menu(ui, card, draft, &todo.item.title, actions),
                    actions,
                )
            });
            (row.response.rect, Some(row.inner))
        },
    );
}

/// The note list; returns its area.
fn notes(
    ui: &mut egui::Ui,
    card: &Card,
    notes: &[BoardItem<Note>],
    actions: &mut Vec<Action>,
) -> egui::Rect {
    ui.scope(|ui| note_list(ui, card, notes, actions))
        .response
        .rect
}

fn note_list(ui: &mut egui::Ui, card: &Card, notes: &[BoardItem<Note>], actions: &mut Vec<Action>) {
    ui.strong("Notes");
    add_field(ui, card, Kind::Note, actions);
    if notes.is_empty() {
        ui.weak("No notes");
    }
    reorderable(
        ui,
        card,
        Kind::Note,
        notes,
        |n| n.item.id,
        actions,
        |ui, note, actions| {
            let draft = Draft::note(&note.item);
            if let Some(rect) = editor(ui, card, &draft, actions) {
                return (rect, None);
            }
            let row = sticky(ui, &note.item, |ui| {
                item_row(
                    ui,
                    |ui, _| {
                        if let Some(title) = &note.item.title {
                            ui.strong(title);
                        }
                        details(ui, Some(&note.item.body), note.item.url.as_deref(), note);
                    },
                    |ui, actions| item_menu(ui, card, draft, &note_name(&note.item), actions),
                    actions,
                )
            });
            (row.response.rect, Some(row.inner))
        },
    );
}

fn sticky<R>(
    ui: &mut egui::Ui,
    note: &Note,
    contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    egui::Frame::new()
        .fill(note_fill(note.color.as_deref(), ui.visuals()))
        .corner_radius(4.0)
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Full-contrast text on the colored note.
            ui.visuals_mut().override_text_color = Some(ui.visuals().strong_text_color());
            contents(ui)
        })
}

/// An item's drag handle, its contents, and its menu, side by side.
/// Returns the handle.
fn item_row(
    ui: &mut egui::Ui,
    contents: impl FnOnce(&mut egui::Ui, &mut Vec<Action>),
    menu: impl FnOnce(&mut egui::Ui, &mut Vec<Action>),
    actions: &mut Vec<Action>,
) -> egui::Response {
    ui.horizontal_top(|ui| {
        let grip = handle(ui, "Drag to reorder");
        const MENU: f32 = 28.0;
        let width = (ui.available_width() - MENU).max(40.0);
        ui.allocate_ui_with_layout(
            egui::vec2(width, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_width(width);
                contents(ui, actions);
            },
        );
        menu(ui, actions);
        grip
    })
    .inner
}

/// How a note is named in confirmations: its title or its first line.
fn note_name(note: &Note) -> String {
    note.title.clone().unwrap_or_else(|| {
        let first = note.body.lines().next().unwrap_or_default();
        let mut name: String = first.chars().take(40).collect();
        if name.len() < first.len() {
            name.push('…');
        }
        name
    })
}

fn details<T>(ui: &mut egui::Ui, body: Option<&str>, url: Option<&str>, item: &BoardItem<T>) {
    if let Some(body) = body.filter(|b| !b.is_empty()) {
        ui.label(body);
    }
    crate::app::link(ui, url);
    match &item.resolved_reference {
        Some(ResolvedReference::Live { item }) => {
            ui.weak(format!("Source: {}", item.title));
        }
        Some(ResolvedReference::SourceGone) => {
            ui.weak("Source gone");
        }
        None => (),
    }
}

/// While a feed item is dragged over this card: highlight the list it would
/// join (notes over the note list, todos anywhere else), and promote it there
/// on release.
fn promote_drop(
    ui: &mut egui::Ui,
    card: &Card,
    todos: egui::Rect,
    notes: egui::Rect,
    actions: &mut Vec<Action>,
) {
    let Some(drag) = egui::DragAndDrop::payload::<FeedDrag>(ui.ctx()) else {
        return;
    };
    let Some(pointer) = ui.ctx().pointer_interact_pos() else {
        return;
    };
    let (kind, list) = if notes.contains(pointer) {
        (PromoteKind::Note, notes)
    } else {
        (PromoteKind::Todo, todos)
    };
    let selection = ui.visuals().selection;
    let shown = list.expand(3.0).intersect(ui.clip_rect());
    ui.painter()
        .rect_filled(shown, 4.0, selection.bg_fill.gamma_multiply(0.2));
    ui.painter()
        .rect_stroke(shown, 4.0, selection.stroke, egui::StrokeKind::Inside);
    if ui.input(|i| i.pointer.any_released()) {
        egui::DragAndDrop::clear_payload(ui.ctx());
        actions.push(Action::Write(WriteOp::Promote {
            feed: drag.feed.clone(),
            key: drag.key.clone(),
            board_id: card.board,
            kind,
        }));
    }
}

/// A drag handle: six dots. `label` names what dragging it does.
pub(crate) fn handle(ui: &mut egui::Ui, label: &'static str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(10.0, 16.0), egui::Sense::drag());
    let color = if response.hovered() || response.dragged() {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    for (x, y) in [
        (3.0, 4.0),
        (7.0, 4.0),
        (3.0, 8.0),
        (7.0, 8.0),
        (3.0, 12.0),
        (7.0, 12.0),
    ] {
        ui.painter()
            .circle_filled(rect.min + egui::vec2(x, y), 1.2, color);
    }
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, label));
    response.on_hover_cursor(egui::CursorIcon::Grab)
}

/// Draw `items` with `row`, which returns the row's rectangle and its drag
/// handle (none while the item is being edited). Dropping a dragged row
/// between others sends its new position.
#[allow(clippy::too_many_arguments)]
fn reorderable<T>(
    ui: &mut egui::Ui,
    card: &Card,
    kind: Kind,
    items: &[T],
    id_of: impl Fn(&T) -> i64,
    actions: &mut Vec<Action>,
    mut row: impl FnMut(&mut egui::Ui, &T, &mut Vec<Action>) -> (egui::Rect, Option<egui::Response>),
) {
    let mut rects = Vec::with_capacity(items.len());
    let mut dragging = None;
    for (i, item) in items.iter().enumerate() {
        let (rect, grip) = ui.push_id(id_of(item), |ui| row(ui, item, actions)).inner;
        rects.push(rect);
        if let Some(grip) = grip
            && (grip.dragged() || grip.drag_stopped())
        {
            dragging = Some((i, grip));
        }
    }
    if let Some((from, grip)) = dragging
        && let Some(to) = reorder_drop(ui, &rects, from, &grip)
    {
        actions.push(card.patch(kind, id_of(&items[from]), json!({ "position": to })));
    }
}

/// While `grip` drags row `from` of a list whose rows are `rects`: show where
/// it would go and, when released within the list's visible area at a new
/// place, return its new index among the rows. Released elsewhere (a board
/// card, say), it stays put.
pub(crate) fn reorder_drop(
    ui: &egui::Ui,
    rects: &[egui::Rect],
    from: usize,
    grip: &egui::Response,
) -> Option<usize> {
    let pointer = ui.ctx().pointer_interact_pos()?;
    if !ui.clip_rect().contains(pointer) {
        return None;
    }
    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    // The new index counts the other rows above the pointer.
    let others: Vec<egui::Rect> = rects
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != from)
        .map(|(_, r)| *r)
        .collect();
    let to = others.iter().filter(|r| r.center().y < pointer.y).count();
    let y = match (
        to.checked_sub(1).and_then(|i| others.get(i)),
        others.get(to),
    ) {
        (Some(above), Some(below)) => (above.bottom() + below.top()) / 2.0,
        (Some(above), None) => above.bottom() + 2.0,
        (None, Some(below)) => below.top() - 2.0,
        (None, None) => rects[from].top(),
    };
    ui.painter()
        .hline(rects[from].x_range(), y, ui.visuals().selection.stroke);
    (grip.drag_stopped() && to != from).then_some(to)
}

/// Edit, move, archive, or delete an active item.
fn item_menu(ui: &mut egui::Ui, card: &Card, draft: Draft, name: &str, actions: &mut Vec<Action>) {
    let (kind, id) = (draft.kind, draft.id);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
        ui.menu_button("…", |ui| {
            if ui.button("Edit…").clicked() {
                card.memory.set(
                    ui,
                    "draft",
                    Some(Draft {
                        focus: true,
                        ..draft
                    }),
                );
                ui.close();
            }
            ui.menu_button("Move to", |ui| {
                let others: Vec<_> = card
                    .boards
                    .iter()
                    .filter(|b| b.info.id != card.board)
                    .collect();
                if others.is_empty() {
                    ui.weak("No other boards");
                }
                for board in others {
                    if ui.button(&board.info.name).clicked() {
                        actions.push(card.patch(kind, id, json!({ "board_id": board.info.id })));
                        ui.close();
                    }
                }
            });
            if ui.button("Archive").clicked() {
                actions.push(card.patch(kind, id, json!({ "archived": true })));
                ui.close();
            }
            if ui.button("Delete…").clicked() {
                actions.push(delete(card, kind, id, name));
                ui.close();
            }
        });
    });
}

fn delete(card: &Card, kind: Kind, id: i64, name: &str) -> Action {
    Action::ConfirmDelete(DeleteSubject::Item {
        kind,
        id,
        board: card.board,
        name: name.to_owned(),
    })
}

/// The item's editor, when it is the one being edited: its rectangle.
fn editor(
    ui: &mut egui::Ui,
    card: &Card,
    original: &Draft,
    actions: &mut Vec<Action>,
) -> Option<egui::Rect> {
    let mut draft = card
        .memory
        .get::<Draft>(ui, "draft")
        .filter(|d| d.kind == original.kind && d.id == original.id)?;
    let mut save = false;
    let mut cancel = false;
    let response = ui.group(|ui| {
        ui.set_width(ui.available_width());
        let focus = std::mem::take(&mut draft.focus);
        let mut in_editor = false;
        let mut field = |ui: &mut egui::Ui, label: &str, text: &mut String, multiline: bool| {
            let label = ui.label(label);
            let edit = if multiline {
                egui::TextEdit::multiline(text).desired_rows(3)
            } else {
                egui::TextEdit::singleline(text)
            };
            let response = ui
                .add(edit.desired_width(f32::INFINITY))
                .labelled_by(label.id);
            // Escape makes the field give up focus before this check.
            in_editor |= response.has_focus() || response.lost_focus();
            if !multiline && response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
            {
                save = true;
            }
            response
        };
        let title = field(ui, "Title", &mut draft.title, false);
        if focus {
            title.request_focus();
        }
        let body_label = match draft.kind {
            Kind::Todo => "Details",
            Kind::Note => "Text",
        };
        field(ui, body_label, &mut draft.body, true);
        field(ui, "Link", &mut draft.url, false);
        if draft.kind == Kind::Note {
            ui.horizontal_wrapped(|ui| {
                ui.label("Color");
                ui.selectable_value(&mut draft.color, None, "None");
                for (value, label) in COLORS {
                    ui.selectable_value(&mut draft.color, Some(value.to_owned()), label);
                }
            });
        }
        if let Some(error) = &draft.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        ui.horizontal(|ui| {
            save |= ui.button("Save").clicked();
            cancel |= ui.button("Cancel").clicked();
        });
        cancel |= in_editor && ui.input(|i| i.key_pressed(egui::Key::Escape));
    });
    if cancel {
        card.memory.set::<Draft>(ui, "draft", None);
    } else if save && let Some(patch) = draft.patch(original) {
        if patch.as_object().is_some_and(|p| !p.is_empty()) {
            actions.push(card.patch(draft.kind, draft.id, patch));
        }
        card.memory.set::<Draft>(ui, "draft", None);
    } else {
        card.memory.set(ui, "draft", Some(draft));
    }
    Some(response.response.rect)
}

/// Archived items: restore (to a chosen board, from the deleted-board
/// archive) or delete.
fn archived_items(
    ui: &mut egui::Ui,
    card: &Card,
    items: &BoardContents,
    actions: &mut Vec<Action>,
) {
    let rows = items
        .todos
        .iter()
        .map(|t| {
            (
                Kind::Todo,
                t.item.id,
                t.item.title.clone(),
                t.item.archived_from_board.as_deref(),
            )
        })
        .chain(items.notes.iter().map(|n| {
            (
                Kind::Note,
                n.item.id,
                note_name(&n.item),
                n.item.archived_from_board.as_deref(),
            )
        }));
    for (kind, id, name, from) in rows {
        ui.push_id((kind, id), |ui| {
            ui.group(|ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.weak(format!("{}:", capitalized(kind.noun())));
                    ui.label(&name);
                });
                if card.archive()
                    && let Some(from) = from
                {
                    ui.weak(format!("From “{from}”"));
                }
                ui.horizontal(|ui| {
                    if card.archive() {
                        ui.menu_button("Restore to", |ui| {
                            if card.boards.is_empty() {
                                ui.weak("No boards yet");
                            }
                            for board in card.boards {
                                if ui.button(&board.info.name).clicked() {
                                    actions.push(card.patch(
                                        kind,
                                        id,
                                        json!({ "archived": false, "board_id": board.info.id }),
                                    ));
                                    ui.close();
                                }
                            }
                        });
                    } else if ui.small_button("Restore").clicked() {
                        actions.push(card.patch(kind, id, json!({ "archived": false })));
                    }
                    if ui.small_button("Delete…").clicked() {
                        actions.push(delete(card, kind, id, &name));
                    }
                });
            });
        });
    }
}

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Service;

    fn todo_draft() -> Draft {
        Draft {
            kind: Kind::Todo,
            id: 1,
            title: "Title".into(),
            body: "Body".into(),
            url: "https://a".into(),
            color: None,
            error: None,
            focus: false,
        }
    }

    #[test]
    fn a_draft_sends_changed_fields_and_clears_emptied_optional_ones() {
        let original = todo_draft();
        let mut draft = original.clone();
        assert_eq!(draft.patch(&original), Some(json!({})));
        draft.title = "  New  ".into();
        draft.body = String::new();
        draft.url = " ".into();
        assert_eq!(
            draft.patch(&original),
            Some(json!({"title": "New", "body": null, "url": null}))
        );
        draft.title = " ".into();
        assert_eq!(draft.patch(&original), None);
        assert_eq!(draft.error.as_deref(), Some("A todo needs a title"));

        // A note's title is optional and its body is text, never null.
        let note = Draft {
            kind: Kind::Note,
            title: "Title".into(),
            color: Some("blue".into()),
            ..todo_draft()
        };
        let mut draft = note.clone();
        draft.title = String::new();
        draft.body = String::new();
        draft.color = None;
        assert_eq!(
            draft.patch(&note),
            Some(json!({"title": null, "body": "", "color": null}))
        );
    }

    #[test]
    fn ops_name_the_boards_they_change() {
        let moved = BoardOp::Patch {
            kind: Kind::Note,
            id: 5,
            board: 2,
            patch: json!({"board_id": 3}),
        };
        assert_eq!(moved.boards(), [2, 3]);
        assert_eq!(BoardOp::Delete { id: 4 }.boards(), [4, ARCHIVE_BOARD]);
        assert!(BoardOp::Create { name: "x".into() }.boards().is_empty());
        let archive = BoardOp::ArchiveDone {
            board: 2,
            ids: vec![7, 8],
        };
        assert_eq!(archive.requests().len(), 2);
        assert_eq!(moved.failure(), "Could not change the note");
    }

    async fn run(service: &Service, op: &BoardOp) -> Value {
        let mut last = Value::Null;
        for (method, resource, body) in op.requests() {
            last = crate::backend::send(&service.paths, None, method, &resource, &body)
                .await
                .unwrap_or_else(|e| panic!("{op:?}: {e}"));
        }
        last
    }

    /// Every op's requests, against the real service.
    #[tokio::test]
    async fn every_op_is_accepted_by_the_service() {
        let service = Service::new();
        let running = service.start().await;
        let created = run(
            &service,
            &BoardOp::Create {
                name: "Home".into(),
            },
        )
        .await;
        let home: i64 = created["id"].as_i64().unwrap();
        let other = run(
            &service,
            &BoardOp::Create {
                name: "Work".into(),
            },
        )
        .await["id"]
            .as_i64()
            .unwrap();
        let todo = run(
            &service,
            &BoardOp::AddTodo {
                board: home,
                title: "Plan".into(),
            },
        )
        .await["id"]
            .as_i64()
            .unwrap();
        let second = run(
            &service,
            &BoardOp::AddTodo {
                board: home,
                title: "Pack".into(),
            },
        )
        .await["id"]
            .as_i64()
            .unwrap();
        let note = run(
            &service,
            &BoardOp::AddNote {
                board: home,
                body: "Idea".into(),
            },
        )
        .await["id"]
            .as_i64()
            .unwrap();
        let patch = |kind, id, patch| BoardOp::Patch {
            kind,
            id,
            board: home,
            patch,
        };
        for op in [
            patch(
                Kind::Todo,
                todo,
                json!({"done": true, "title": "Plan it", "body": null}),
            ),
            patch(Kind::Todo, second, json!({"position": 0})),
            patch(
                Kind::Note,
                note,
                json!({"title": "T", "color": "green", "url": "https://x"}),
            ),
            BoardOp::ArchiveDone {
                board: home,
                ids: vec![todo],
            },
            patch(Kind::Todo, todo, json!({"archived": false})),
            patch(Kind::Note, note, json!({"board_id": other})),
            BoardOp::Rename {
                id: home,
                name: "House".into(),
            },
            BoardOp::DeleteItem {
                kind: Kind::Todo,
                id: second,
                board: home,
            },
            BoardOp::Delete { id: home },
            BoardOp::Patch {
                kind: Kind::Todo,
                id: todo,
                board: ARCHIVE_BOARD,
                patch: json!({"archived": false, "board_id": other}),
            },
        ] {
            run(&service, &op).await;
        }
        let work: callboard_core::store::BoardContents = serde_json::from_value(
            running
                .send("GET", &format!("/boards/{other}"), json!(null))
                .await,
        )
        .unwrap();
        assert_eq!(work.todos[0].item.title, "Plan it");
        assert!(work.todos[0].item.done);
        assert_eq!(work.notes[0].item.color.as_deref(), Some("green"));
        let boards = running.send("GET", "/boards", json!(null)).await;
        assert_eq!(boards.as_array().unwrap().len(), 1, "House deleted");
        running.stop().await;
    }
}
