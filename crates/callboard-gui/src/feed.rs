//! Feed cards: items in display order, with snooze, promote, and drag to
//! reorder or promote (DESIGN.md §6.2).
use crate::{
    app::{Action, WriteOp, age, coarse, drag_ghost, link, now_ms, stale},
    backend::{PromoteKind, Target},
    board,
};
use callboard_core::store::{BoardSummary, Feed, ItemViewState};
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

pub fn show(
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
        coarse(age(feed.info.last_submitted_at_ms)),
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
    ui.horizontal(|ui| {
        if ui
            .checkbox(&mut show_snoozed, format!("Show snoozed ({snoozed})"))
            .changed()
        {
            ui.data_mut(|d| d.insert_temp(toggle, show_snoozed));
        }
        if feed.manual_order
            && let Some(first) = feed.items.first()
            && ui
                .small_button("Reset order")
                .on_hover_text("Return to the order the feed submits")
                .clicked()
        {
            actions.push(Action::Write(WriteOp::ResetOrder {
                feed: feed.info.name.clone(),
                key: first.key.clone(),
            }));
        }
    });
    egui::ScrollArea::vertical()
        .id_salt(salt.with("feed_items"))
        .auto_shrink([false, false])
        .wheel_scroll_multiplier(wheel(scroll))
        .show(ui, |ui| {
            if feed.items.is_empty() {
                ui.label("This feed is empty.");
            }
            // Shown items: their keys, rows, and the handle being dragged.
            let mut shown: Vec<&str> = Vec::new();
            let mut rects = Vec::new();
            let mut dragging = None;
            for item in &feed.items {
                let snoozed = feed.view_state.get(&item.key).is_some_and(|s| s.snoozed);
                if snoozed && !show_snoozed {
                    continue;
                }
                let mut grip = None;
                let row = ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    ui.push_id(&item.key, |ui| {
                        ui.horizontal(|ui| {
                            let handle = board::handle(ui, "Drag to reorder or promote")
                                .on_hover_text(
                                    "Drag within the feed to reorder, or onto a board card to promote",
                                );
                            ui.strong(&item.title);
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
                if let Some(grip) = grip
                    && (grip.dragged() || grip.drag_stopped())
                {
                    dragging = Some((shown.len(), grip));
                }
                shown.push(&item.key);
                rects.push(row.response.rect);
            }
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
