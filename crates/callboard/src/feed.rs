//! Feed cards: items in display order, with snooze, promote, and drag to
//! reorder or promote (DESIGN.md §6.2).
use crate::{
    app::{Action, WriteOp, age, coarse, drag_ghost, link, now_ms, stale},
    backend::{PromoteKind, Target},
    board,
    theme::{self, Palette, icon},
};
use callboard_core::{
    feed::{Item, MetaValue},
    store::{BoardSummary, ChangeStatus, Feed, ItemChange, ItemViewState, LastChange, window_ms},
};
use eframe::egui;
use std::time::Duration;

/// When a snoozed item wakes (DESIGN.md §4.1: whichever condition comes first).
pub(crate) fn snooze_text(state: &ItemViewState) -> String {
    let until = state.snoozed_until_ms.map(|at| {
        // Whole minutes, rounded up: "wakes in 1h", not "59m 59s".
        let left_ms = at.saturating_sub(now_ms()).max(0) as u64;
        let left = Duration::from_secs(left_ms.div_ceil(60_000) * 60);
        // The two largest units are precise enough: "3days 4h", not "… 12m".
        format!("wakes in {}", coarse(left))
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

/// An item's … menu: snooze (or unsnooze) and promote.
fn item_menu(
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
    let faint = Palette::of(ui.visuals()).faint;
    let menu = ui.menu_button(
        theme::glyph(icon::DOTS_THREE).size(15.0).color(faint),
        |ui| {
            if snoozed {
                if ui.button("Unsnooze").clicked() {
                    actions.push(snooze(None, false));
                    ui.close();
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
        },
    );
    theme::name(&menu.response, "Item menu");
}

/// A link shortened to fit one line: no scheme, truncated with an ellipsis.
/// Only HTTP(S) links open, and only when clicked (DESIGN.md §10.4).
/// Quiet until hovered, so a list of links does not read as a wall of blue.
pub(crate) fn short_link(ui: &mut egui::Ui, url: &str) {
    let p = Palette::of(ui.visuals());
    let web = url.starts_with("https://") || url.starts_with("http://");
    let shown = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let id = ui.next_auto_id();
    let hovered = web && ui.ctx().read_response(id).is_some_and(|r| r.hovered());
    let mut text =
        egui::RichText::new(shown)
            .size(11.5)
            .color(if hovered { p.accent_text } else { p.faint });
    if hovered {
        text = text.underline();
    }
    let label = egui::Label::new(text).truncate();
    if web {
        let response = ui.add(label.sense(egui::Sense::click()));
        if response
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
        {
            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
        }
    } else {
        ui.add(label);
    }
}

/// How long the pointer rests on an item before its details show, and when
/// the row starts showing that the wait is under way.
pub const DWELL: f64 = 1.0;
const CUE_AFTER: f64 = 0.2;

/// The row the pointer rests on in a card, and since when.
#[derive(Clone, Copy, PartialEq)]
struct Dwell {
    row: egui::Id,
    since: f64,
}

/// Details on rest for one card's rows: [`Rest::lit`] while drawing them,
/// then [`Rest::end`] with the row the pointer rests on, if any.
pub(crate) struct Rest {
    id: egui::Id,
    dwell: Option<Dwell>,
    now: f64,
}

impl Rest {
    pub fn begin(ui: &egui::Ui, salt: egui::Id) -> Self {
        let id = salt.with("dwell");
        Self {
            id,
            dwell: ui.data(|d| d.get_temp(id)),
            now: ui.input(|i| i.time),
        }
    }

    /// Whether `row` is highlighted: the pointer rests on it.
    pub fn lit(&self, row: egui::Id) -> bool {
        self.dwell.is_some_and(|d| d.row == row)
    }

    /// Whether the pointer can rest on `rect`: the topmost card (`scroll`),
    /// nothing dragged or open.
    pub fn resting(ui: &egui::Ui, rect: egui::Rect, scroll: bool) -> bool {
        scroll
            && ui.rect_contains_pointer(rect)
            && !ui.input(|i| i.pointer.any_down())
            && !egui::Popup::is_any_open(ui.ctx())
    }

    /// Show the details of the row resting since [`DWELL`], or the cue that
    /// the wait is under way.
    pub fn end(
        self,
        ui: &egui::Ui,
        resting: Option<(egui::Id, egui::Rect)>,
        details: impl FnOnce(&mut egui::Ui),
    ) {
        let p = Palette::of(ui.visuals());
        let next = resting.map(|(row, rect)| {
            let since = self
                .dwell
                .filter(|d| d.row == row)
                .map_or(self.now, |d| d.since);
            let waited = self.now - since;
            if waited >= DWELL {
                show_details(ui, row, rect, details);
            } else {
                ui.ctx().request_repaint_after_secs((DWELL - waited) as f32);
                // A line fills along the row's foot while the pointer rests.
                if waited >= CUE_AFTER {
                    let progress = ((waited - CUE_AFTER) / (DWELL - CUE_AFTER)) as f32;
                    let x = rect.left()..=rect.left() + rect.width() * progress;
                    ui.painter()
                        .hline(x, rect.bottom() - 1.0, egui::Stroke::new(2.0, p.accent));
                }
            }
            Dwell { row, since }
        });
        if next != self.dwell {
            ui.data_mut(|d| match next {
                Some(next) => {
                    d.insert_temp(self.id, next);
                }
                None => d.remove::<Dwell>(self.id),
            });
            // Highlight (or unhighlight) the row on the next frame.
            ui.ctx().request_repaint();
        }
    }
}

/// Everything about an item, shown after the pointer rests on its row.
fn details(ui: &mut egui::Ui, feed: &Feed, item: &Item) {
    ui.set_max_width(440.0);
    ui.label(egui::RichText::new(&item.title).strong());
    if let Some(url) = &item.url {
        link(ui, Some(url));
    }
    if let Some(change) = feed.changes.get(&item.key)
        && change.added_at_ms > 0
    {
        ui.horizontal_wrapped(|ui| {
            if let Some(status) = status(feed, &item.key) {
                badge(ui, status);
            }
            if let Some(text) = change_text(change) {
                ui.weak(text);
            }
        });
    }
    if let Some(state) = feed.view_state.get(&item.key).filter(|s| s.snoozed) {
        ui.weak(snooze_text(state));
    }
    if let Some(body) = item.body.as_deref().filter(|b| !b.is_empty()) {
        ui.add_space(4.0);
        ui.label(body);
    }
    tags_and_meta(ui, &item.tags, &item.meta);
    ui.add_space(4.0);
    let key = match &item.color {
        Some(color) => format!("Key: {} · color: {color}", item.key),
        None => format!("Key: {}", item.key),
    };
    ui.weak(egui::RichText::new(key).small());
}

/// An item's tags as chips, then its `meta` as a key/value grid.
pub(crate) fn tags_and_meta(
    ui: &mut egui::Ui,
    tags: &[String],
    meta: &std::collections::BTreeMap<String, MetaValue>,
) {
    if !tags.is_empty() {
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            for tag in tags {
                egui::Frame::new()
                    .fill(ui.visuals().faint_bg_color)
                    .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::symmetric(6, 1))
                    .show(ui, |ui| ui.small(tag));
            }
        });
    }
    if !meta.is_empty() {
        ui.add_space(4.0);
        egui::Grid::new("meta")
            .num_columns(2)
            .spacing([12.0, 2.0])
            .show(ui, |ui| {
                for (key, value) in meta {
                    ui.weak(key);
                    ui.label(meta_text(value));
                    ui.end_row();
                }
            });
    }
}

fn meta_text(value: &MetaValue) -> String {
    match value {
        MetaValue::String(s) => s.clone(),
        MetaValue::Number(n) => n.to_string(),
        MetaValue::Bool(b) => b.to_string(),
    }
}

/// Only the topmost card under the pointer scrolls with the wheel: with a
/// multiplier of 0 a covered card neither scrolls nor consumes the wheel.
pub(crate) fn wheel(scroll: bool) -> egui::Vec2 {
    egui::Vec2::splat(if scroll { 1.0 } else { 0.0 })
}

pub fn show(
    ui: &mut egui::Ui,
    feed: &Feed,
    salt: egui::Id,
    scroll: bool,
    boards: &[BoardSummary],
    actions: &mut Vec<Action>,
) {
    let p = Palette::of(ui.visuals());
    if let Some(description) = &feed.info.description {
        ui.add(egui::Label::new(egui::RichText::new(description).color(p.muted)).truncate())
            .on_hover_text(description);
    }
    // One line that truncates rather than widening the card.
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some(url) = feed.info.source_url.as_deref() {
                let web = url.starts_with("https://") || url.starts_with("http://");
                let open = ui.add_enabled_ui(web, |ui| {
                    theme::icon_button(ui, icon::ARROW_SQUARE_OUT, "Open source", url)
                });
                if open.inner.clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                }
            }
            if stale(&feed.info) {
                theme::pill(ui, "stale", p.warning_soft, p.warning);
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                let mut meta = format!(
                    "{} · submitted {} ago",
                    board::count(feed.items.len(), "item"),
                    coarse(age(feed.info.last_submitted_at_ms))
                );
                if let Some(change) = &feed.info.last_change {
                    meta.push_str(" · ");
                    meta.push_str(&last_change_text(change));
                }
                let response = ui.add(
                    egui::Label::new(egui::RichText::new(&meta).size(12.0).color(p.faint))
                        .truncate(),
                );
                if let Some(change) = &feed.info.last_change {
                    gone_on_hover(response, change);
                }
            });
        });
    });
    if let Some(error) = &feed.info.error {
        ui.horizontal(|ui| {
            ui.label(theme::glyph(icon::WARNING_CIRCLE).color(p.danger));
            ui.add(egui::Label::new(egui::RichText::new(&error.message).color(p.danger)).wrap());
        });
    }
    let snoozed = feed
        .items
        .iter()
        .filter(|i| feed.view_state.get(&i.key).is_some_and(|s| s.snoozed))
        .count();
    let toggle = salt.with("show_snoozed");
    let mut show_snoozed = ui.data(|d| d.get_temp::<bool>(toggle)).unwrap_or(false);
    if snoozed > 0 || feed.manual_order {
        ui.horizontal(|ui| {
            if snoozed > 0
                && ui
                    .add(egui::Button::selectable(
                        show_snoozed,
                        egui::RichText::new(format!("Show snoozed ({snoozed})")).size(12.0),
                    ))
                    .clicked()
            {
                show_snoozed = !show_snoozed;
                ui.data_mut(|d| d.insert_temp(toggle, show_snoozed));
            }
            if feed.manual_order
                && let Some(first) = feed.items.first()
                && ui
                    .add(egui::Button::new(
                        egui::RichText::new("Reset order").size(12.0),
                    ))
                    .on_hover_text("Return to the order the feed submits")
                    .clicked()
            {
                actions.push(Action::Write(WriteOp::ResetOrder {
                    feed: feed.info.name.clone(),
                    key: first.key.clone(),
                }));
            }
        });
    }
    ui.add_space(4.0);
    egui::ScrollArea::vertical()
        .id_salt(salt.with("feed_items"))
        .auto_shrink([false, false])
        .wheel_scroll_multiplier(wheel(scroll))
        .show(ui, |ui| {
            if feed.items.is_empty() {
                ui.label(egui::RichText::new("This feed is empty.").color(p.faint));
            }
            // Shown items: their keys, rows, and the handle being dragged.
            let mut shown: Vec<&str> = Vec::new();
            let mut rects = Vec::new();
            let mut dragging = None;
            let rest = Rest::begin(ui, salt);
            let mut resting = None;
            for item in &feed.items {
                let state = feed.view_state.get(&item.key).filter(|s| s.snoozed);
                if state.is_some() && !show_snoozed {
                    continue;
                }
                let row_id = salt.with(("row", &item.key));
                // Highlighted while the pointer rests on it.
                let lit = rest.lit(row_id);
                let mut frame = egui::Frame::new()
                    .inner_margin(egui::Margin {
                        left: 10,
                        right: 4,
                        top: 5,
                        bottom: 5,
                    })
                    .corner_radius(theme::RADIUS_MD)
                    .begin(ui);
                let mut grip = None;
                // A color marks the row with a bar at its left edge.
                let mark = item.color.as_deref().and_then(theme::color_mark);
                {
                    let ui = &mut frame.content_ui;
                    ui.set_width(ui.available_width());
                    ui.push_id(&item.key, |ui| {
                        ui.horizontal_top(|ui| {
                            let handle = board::handle(ui, "Drag to reorder or promote");
                            board::keep_in_place(
                                ui,
                                handle.rect,
                                &Target::Feed(feed.info.name.clone()),
                            );
                            if handle.dragged() {
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
                            grip = Some(handle);
                            // Room for the … button and the spacing around it,
                            // so rows never grow wider than the list.
                            let menu = ui.spacing().interact_size.y
                                + 2.0 * ui.spacing().item_spacing.x
                                + 8.0;
                            let width = (ui.available_width() - menu).max(40.0);
                            ui.allocate_ui_with_layout(
                                egui::vec2(width, 0.0),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    ui.set_width(width);
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    if let Some(status) = status(feed, &item.key) {
                                        ui.horizontal_wrapped(|ui| {
                                            badge(ui, status);
                                            ui.label(&item.title);
                                        });
                                    } else {
                                        ui.label(&item.title);
                                    }
                                    if let Some(url) = &item.url {
                                        short_link(ui, url);
                                    }
                                    if let Some(state) = state {
                                        ui.label(
                                            egui::RichText::new(snooze_text(state))
                                                .size(11.5)
                                                .color(p.faint),
                                        );
                                    }
                                },
                            );
                            item_menu(
                                ui,
                                &feed.info.name,
                                &item.key,
                                state.is_some(),
                                boards,
                                actions,
                            );
                        });
                    });
                }
                let hovered = ui.rect_contains_pointer(frame.content_ui.min_rect().expand(6.0));
                let hover =
                    ui.ctx()
                        .animate_bool_with_time(row_id.with("hover"), lit || hovered, 0.12);
                frame.frame.fill = egui::Color32::TRANSPARENT.lerp_to_gamma(p.hover, hover);
                let rect = frame.end(ui).rect;
                if let Some(mark) = mark {
                    let bar = egui::Rect::from_min_max(
                        rect.left_top() + egui::vec2(2.0, 5.0),
                        rect.left_bottom() + egui::vec2(5.0, -5.0),
                    );
                    ui.painter().rect_filled(bar, 2.0, mark);
                }
                if Rest::resting(ui, rect, scroll) {
                    resting = Some((row_id, rect, item));
                }
                ui.add_space(2.0);
                if let Some(grip) = grip
                    && (grip.dragged() || grip.drag_stopped())
                {
                    dragging = Some((shown.len(), grip));
                }
                shown.push(&item.key);
                rects.push(rect);
            }
            let item = resting.map(|(_, _, item)| item);
            rest.end(ui, resting.map(|(row, rect, _)| (row, rect)), |ui| {
                if let Some(item) = item {
                    details(ui, feed, item);
                }
            });
            if let Some((from, grip)) = dragging
                && let Some(to) = board::reorder_drop(ui, &rects, from, &grip)
            {
                // Dropped here, it is not a promotion.
                egui::DragAndDrop::clear_payload(ui.ctx());
                let all: Vec<&str> = feed.items.iter().map(|i| i.key.as_str()).collect();
                let position = feed_position(&all, &shown, from, to);
                actions.push(Action::Write(WriteOp::Reorder {
                    feed: feed.info.name.clone(),
                    key: shown[from].to_owned(),
                    position,
                }));
            }
        });
}

/// Where an item dropped at index `to` among the other shown items goes in
/// the feed's whole order (`all`, which includes hidden snoozed items): just
/// after the shown item above it, or before the first shown item.
pub(crate) fn feed_position(all: &[&str], shown: &[&str], from: usize, to: usize) -> usize {
    let moved = shown[from];
    let rest: Vec<&str> = all.iter().copied().filter(|k| *k != moved).collect();
    let others: Vec<&str> = shown.iter().copied().filter(|k| *k != moved).collect();
    let index = |key: &str| rest.iter().position(|k| *k == key).unwrap_or(0);
    match to.checked_sub(1).and_then(|i| others.get(i)) {
        Some(above) => index(above) + 1,
        None => others.first().map_or(0, |first| index(first)),
    }
}

/// The details card beside a row, above everything else.
fn show_details(
    ui: &egui::Ui,
    row: egui::Id,
    rect: egui::Rect,
    details: impl FnOnce(&mut egui::Ui),
) {
    let pointer = ui.ctx().pointer_hover_pos().unwrap_or(rect.right_top());
    egui::Area::new(row.with("details"))
        .order(egui::Order::Tooltip)
        .fixed_pos(pointer + egui::vec2(16.0, 12.0))
        .constrain(true)
        .interactable(false)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, details);
        });
}

/// A small "new" or "updated" pill.
fn badge(ui: &mut egui::Ui, status: ChangeStatus) {
    let p = Palette::of(ui.visuals());
    match status {
        ChangeStatus::New => theme::pill(ui, "new", p.accent_soft, p.accent_text),
        ChangeStatus::Updated => theme::pill(ui, "updated", p.warning_soft, p.warning),
    };
}

/// "Added 2h ago · changed 10m ago"; none for items that predate change
/// tracking (their times are 0).
pub(crate) fn change_text(change: &ItemChange) -> Option<String> {
    let ago = |at: i64| format!("{} ago", coarse(age(at)));
    match (change.added_at_ms, change.changed_at_ms) {
        (0, _) => None,
        (added, changed) if changed > added => {
            Some(format!("Added {} · changed {}", ago(added), ago(changed)))
        }
        (added, _) => Some(format!("Added {}", ago(added))),
    }
}

/// "changed 10m ago: 2 new, 1 updated, 1 gone".
fn last_change_text(change: &LastChange) -> String {
    let parts: Vec<String> = [
        (change.added, "new"),
        (change.updated, "updated"),
        (change.removed, "gone"),
    ]
    .iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, what)| format!("{n} {what}"))
    .collect();
    format!(
        "changed {} ago: {}",
        coarse(age(change.at_ms)),
        parts.join(", ")
    )
}

/// The titles a change removed, on hover.
fn gone_on_hover(response: egui::Response, change: &LastChange) {
    if change.removed_items.is_empty() {
        return;
    }
    response.on_hover_ui(|ui| {
        ui.strong("Gone in that change");
        for item in &change.removed_items {
            ui.label(&item.title);
        }
        let more = change.removed.saturating_sub(change.removed_items.len());
        if more > 0 {
            ui.weak(format!("and {more} more"));
        }
    });
}

/// An item's mark now, from its times and the feed's `new_for` window
/// (DESIGN.md §4.3). Evaluated each frame, so a mark ends on time.
pub(crate) fn status(feed: &Feed, key: &str) -> Option<ChangeStatus> {
    let window = window_ms(feed.info.new_for.as_deref());
    feed.changes
        .get(key)?
        .status_at(window, now_ms())
        .map(|(status, _)| status)
}

/// Keys of the items marked new or updated now.
pub(crate) fn marked(feed: &Feed) -> impl Iterator<Item = &str> {
    feed.items
        .iter()
        .map(|i| i.key.as_str())
        .filter(|key| status(feed, key).is_some())
}
