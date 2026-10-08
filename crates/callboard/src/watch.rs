//! Watch cards: the user's items in queue order, with what the watch's
//! script last reported on each (DESIGN.md §6.7).
use crate::{
    app::{Action, DeleteSubject, WriteOp, age, coarse, link},
    board,
    feed::{Rest, short_link, tags_and_meta, wheel},
    theme::{self, Palette, icon},
};
use callboard_core::watch::{ItemState, Watch, WatchInfo, WatchItem};
use eframe::egui;
use serde_json::{Value, json};

/// A write to a watch's items (DESIGN.md §8.1).
#[derive(Debug, Clone, PartialEq)]
pub enum WatchOp {
    Add {
        watch: String,
        url: String,
        label: Option<String>,
    },
    Acknowledge {
        watch: String,
        id: i64,
    },
    KeepWaiting {
        watch: String,
        id: i64,
    },
    Remove {
        watch: String,
        id: i64,
    },
}

impl WatchOp {
    pub fn watch(&self) -> &str {
        match self {
            WatchOp::Add { watch, .. }
            | WatchOp::Acknowledge { watch, .. }
            | WatchOp::KeepWaiting { watch, .. }
            | WatchOp::Remove { watch, .. } => watch,
        }
    }

    pub fn request(&self) -> (&'static str, String, Value) {
        let items = format!("/watches/{}/items", self.watch());
        match self {
            WatchOp::Add { url, label, .. } => {
                ("POST", items, json!({ "url": url, "label": label }))
            }
            WatchOp::Acknowledge { id, .. } => (
                "PATCH",
                format!("{items}/{id}"),
                json!({ "acknowledge": true }),
            ),
            WatchOp::KeepWaiting { id, .. } => (
                "PATCH",
                format!("{items}/{id}"),
                json!({ "keep_waiting": true }),
            ),
            WatchOp::Remove { id, .. } => ("DELETE", format!("{items}/{id}"), Value::Null),
        }
    }

    /// Shown with the service's error.
    pub fn failure(&self) -> &'static str {
        match self {
            WatchOp::Add { .. } => "Could not add the item",
            WatchOp::Acknowledge { .. } => "Could not acknowledge the item",
            WatchOp::KeepWaiting { .. } => "Could not keep the item waiting",
            WatchOp::Remove { .. } => "Could not remove the item",
        }
    }
}

fn write(op: WatchOp) -> Action {
    Action::Write(WriteOp::Watch(op))
}

/// What a row is called: the user's label, else the reported title, else
/// the URL until the first report.
pub(crate) fn item_name(item: &WatchItem) -> &str {
    item.label
        .as_deref()
        .or(item.content.as_ref().map(|c| c.title.as_str()))
        .unwrap_or(&item.url)
}

fn section_name(state: ItemState) -> &'static str {
    match state {
        ItemState::Attention => "Needs attention",
        ItemState::Quiet => "Quiet",
        ItemState::New => "New",
        ItemState::Waiting => "Waiting",
    }
}

/// How long the item has been in its state: "changed 2h ago", "waiting 12d".
fn state_text(item: &WatchItem) -> String {
    let since = coarse(age(item.state_since_ms));
    match item.state {
        ItemState::Attention => format!("changed {since} ago"),
        ItemState::New => format!("added {since} ago"),
        ItemState::Waiting | ItemState::Quiet => {
            let waiting = item.waiting_since_ms.unwrap_or(item.state_since_ms);
            format!("waiting {}", coarse(age(waiting)))
        }
    }
}

pub(crate) fn stale(info: &WatchInfo) -> bool {
    info.stale_after
        .as_deref()
        .and_then(|v| humantime::parse_duration(v).ok())
        .is_some_and(|limit| {
            (crate::app::now_ms()
                .saturating_sub(info.last_reported_at_ms)
                .max(0) as u128)
                >= limit.as_millis()
        })
}

/// The field that adds a URL, with an optional label. Enter in either adds.
fn add_fields(ui: &mut egui::Ui, watch: &str, salt: egui::Id, actions: &mut Vec<Action>) {
    let (url_id, label_id) = (salt.with("new_url"), salt.with("new_label"));
    let mut url: String = ui.data(|d| d.get_temp(url_id)).unwrap_or_default();
    let mut label: String = ui.data(|d| d.get_temp(label_id)).unwrap_or_default();
    let mut submit = false;
    ui.horizontal(|ui| {
        let width = ui.available_width();
        let url_field = ui.add(
            egui::TextEdit::singleline(&mut url)
                .id(url_id)
                .hint_text("Add a URL to watch")
                .desired_width(width * 0.6),
        );
        let label_field = ui.add(
            egui::TextEdit::singleline(&mut label)
                .id(label_id)
                .hint_text("Label (optional)")
                .desired_width(f32::INFINITY),
        );
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
        for field in [&url_field, &label_field] {
            if field.lost_focus() && enter {
                submit = true;
                url_field.request_focus();
            }
        }
    });
    if submit && !url.trim().is_empty() {
        let trimmed = label.trim();
        actions.push(write(WatchOp::Add {
            watch: watch.to_owned(),
            url: url.trim().to_owned(),
            label: (!trimmed.is_empty()).then(|| trimmed.to_owned()),
        }));
        url.clear();
        label.clear();
    }
    ui.data_mut(|d| {
        d.insert_temp(url_id, url);
        d.insert_temp(label_id, label);
    });
}

/// Everything about an item, shown after the pointer rests on its row.
fn details(ui: &mut egui::Ui, item: &WatchItem) {
    ui.set_max_width(440.0);
    ui.label(egui::RichText::new(item_name(item)).strong());
    if let Some(content) = &item.content
        && item.label.is_some()
    {
        ui.label(&content.title);
    }
    link(ui, Some(&item.url));
    ui.horizontal_wrapped(|ui| {
        state_pill(ui, item.state);
        ui.weak(state_text(item));
    });
    let ago = |at: i64| format!("{} ago", coarse(age(at)));
    let mut times = vec![format!("Added {}", ago(item.added_at_ms))];
    if let Some(at) = item.changed_at_ms {
        times.push(format!("last needed attention {}", ago(at)));
    }
    match item.reported_at_ms {
        Some(at) => times.push(format!("reported {}", ago(at))),
        None => times.push("not reported yet".into()),
    }
    ui.weak(times.join(" · "));
    if let Some(error) = &item.error {
        ui.colored_label(Palette::of(ui.visuals()).danger, &error.message);
    }
    if let Some(content) = &item.content {
        if let Some(body) = content.body.as_deref().filter(|b| !b.is_empty()) {
            ui.add_space(4.0);
            ui.label(body);
        }
        tags_and_meta(ui, &content.tags, &content.meta);
    }
    ui.add_space(4.0);
    let mut key = format!("ID {}", item.id);
    if let Some(fingerprint) = &item.fingerprint {
        key.push_str(&format!(" · fingerprint: {fingerprint}"));
    }
    if let Some(color) = item.content.as_ref().and_then(|c| c.color.as_deref()) {
        key.push_str(&format!(" · color: {color}"));
    }
    ui.weak(egui::RichText::new(key).small());
}

fn state_pill(ui: &mut egui::Ui, state: ItemState) {
    let p = Palette::of(ui.visuals());
    match state {
        ItemState::Attention => theme::pill(ui, "needs attention", p.accent_soft, p.accent_text),
        ItemState::Quiet => theme::pill(ui, "quiet", p.warning_soft, p.warning),
        ItemState::New => theme::pill(ui, "new", p.raised, p.muted),
        ItemState::Waiting => theme::pill(ui, "waiting", p.raised, p.muted),
    };
}

/// An item's … menu: open it, or remove it after confirmation.
fn item_menu(ui: &mut egui::Ui, watch: &str, item: &WatchItem, actions: &mut Vec<Action>) {
    let faint = Palette::of(ui.visuals()).faint;
    let menu = ui.menu_button(
        theme::glyph(icon::DOTS_THREE).size(15.0).color(faint),
        |ui| {
            if ui.button("Remove…").clicked() {
                actions.push(Action::ConfirmDelete(DeleteSubject::WatchItem {
                    watch: watch.to_owned(),
                    id: item.id,
                    name: item_name(item).to_owned(),
                }));
                ui.close();
            }
        },
    );
    theme::name(&menu.response, "Item menu");
}

fn row(
    ui: &mut egui::Ui,
    watch: &Watch,
    item: &WatchItem,
    lit: bool,
    row_id: egui::Id,
    actions: &mut Vec<Action>,
) -> egui::Rect {
    let p = Palette::of(ui.visuals());
    let name = &watch.info.name;
    let mut frame = egui::Frame::new()
        .inner_margin(egui::Margin {
            left: 10,
            right: 4,
            top: 5,
            bottom: 5,
        })
        .corner_radius(theme::RADIUS_MD)
        .begin(ui);
    let content = item.content.as_ref();
    let mark = content
        .and_then(|c| c.color.as_deref())
        .and_then(theme::color_mark);
    {
        let ui = &mut frame.content_ui;
        ui.set_width(ui.available_width());
        ui.push_id(item.id, |ui| {
            ui.horizontal_top(|ui| {
                let menu = ui.spacing().interact_size.y + 2.0 * ui.spacing().item_spacing.x + 8.0;
                let width = (ui.available_width() - menu).max(40.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(width, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_width(width);
                        ui.spacing_mut().item_spacing.y = 2.0;
                        let strong = item.state == ItemState::Attention;
                        let title = egui::RichText::new(item_name(item));
                        ui.label(if strong { title.strong() } else { title });
                        if let (Some(content), Some(_)) = (content, &item.label) {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&content.title).color(p.muted),
                                )
                                .truncate(),
                            );
                        }
                        short_link(ui, &item.url);
                        ui.horizontal_wrapped(|ui| {
                            if item.state == ItemState::Quiet {
                                state_pill(ui, item.state);
                            }
                            ui.label(
                                egui::RichText::new(state_text(item))
                                    .size(11.5)
                                    .color(p.faint),
                            );
                            let button = |ui: &mut egui::Ui, text: &str, hint: &str| {
                                ui.add(egui::Button::new(egui::RichText::new(text).size(12.0)))
                                    .on_hover_text(hint)
                                    .clicked()
                            };
                            let id = item.id;
                            let watch = name.clone();
                            match item.state {
                                ItemState::Attention
                                    if button(
                                        ui,
                                        "Acknowledge",
                                        "Clear its attention until it changes again",
                                    ) =>
                                {
                                    actions.push(write(WatchOp::Acknowledge { watch, id }));
                                }
                                ItemState::Quiet
                                    if button(
                                        ui,
                                        "Keep waiting",
                                        "Restart its waiting time; it becomes quiet again if nothing changes",
                                    ) =>
                                {
                                    actions.push(write(WatchOp::KeepWaiting { watch, id }));
                                }
                                _ => (),
                            }
                        });
                        if let Some(error) = &item.error {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&error.message)
                                        .size(11.5)
                                        .color(p.danger),
                                )
                                .truncate(),
                            );
                        }
                    },
                );
                item_menu(ui, name, item, actions);
            });
        });
    }
    let hovered = ui.rect_contains_pointer(frame.content_ui.min_rect().expand(6.0));
    let hover = ui
        .ctx()
        .animate_bool_with_time(row_id.with("hover"), lit || hovered, 0.12);
    // Items that need attention stand out (DESIGN.md §6.7).
    let base = if item.state == ItemState::Attention {
        p.accent_soft
    } else {
        egui::Color32::TRANSPARENT
    };
    frame.frame.fill = base.lerp_to_gamma(p.hover, hover);
    let rect = frame.end(ui).rect;
    if let Some(mark) = mark {
        let bar = egui::Rect::from_min_max(
            rect.left_top() + egui::vec2(2.0, 5.0),
            rect.left_bottom() + egui::vec2(5.0, -5.0),
        );
        ui.painter().rect_filled(bar, 2.0, mark);
    }
    rect
}

pub fn show(
    ui: &mut egui::Ui,
    watch: &Watch,
    salt: egui::Id,
    scroll: bool,
    actions: &mut Vec<Action>,
) {
    let p = Palette::of(ui.visuals());
    let info = &watch.info;
    if let Some(description) = &info.description {
        ui.add(egui::Label::new(egui::RichText::new(description).color(p.muted)).truncate())
            .on_hover_text(description);
    }
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some(url) = info.source_url.as_deref() {
                let web = url.starts_with("https://") || url.starts_with("http://");
                let open = ui.add_enabled_ui(web, |ui| {
                    theme::icon_button(ui, icon::ARROW_SQUARE_OUT, "Open source", url)
                });
                if open.inner.clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                }
            }
            if stale(info) {
                theme::pill(ui, "stale", p.warning_soft, p.warning);
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                let meta = format!(
                    "{} · reported {} ago",
                    board::count(watch.items.len(), "item"),
                    coarse(age(info.last_reported_at_ms))
                );
                ui.add(
                    egui::Label::new(egui::RichText::new(&meta).size(12.0).color(p.faint))
                        .truncate(),
                )
                .on_hover_text(format!(
                    "New for {} after an item is added; quiet after waiting {}.",
                    info.waiting_after, info.quiet_after
                ));
            });
        });
    });
    if let Some(error) = &info.error {
        ui.horizontal(|ui| {
            ui.label(theme::glyph(icon::WARNING_CIRCLE).color(p.danger));
            ui.add(egui::Label::new(egui::RichText::new(&error.message).color(p.danger)).wrap());
        });
    }
    add_fields(ui, &info.name, salt, actions);
    ui.add_space(4.0);
    egui::ScrollArea::vertical()
        .id_salt(salt.with("watch_items"))
        .auto_shrink([false, false])
        .wheel_scroll_multiplier(wheel(scroll))
        .show(ui, |ui| {
            if watch.items.is_empty() {
                ui.label(
                    egui::RichText::new("Nothing watched yet. Add a URL above.").color(p.faint),
                );
            }
            let rest = Rest::begin(ui, salt);
            let mut resting = None;
            let mut section = None;
            for item in &watch.items {
                if section != Some(item.state) {
                    section = Some(item.state);
                    let n = watch.items.iter().filter(|i| i.state == item.state).count();
                    let heading = format!("{} ({n})", section_name(item.state));
                    ui.add_space(4.0);
                    let label = ui.label(theme::eyebrow(ui, &heading));
                    label.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &heading)
                    });
                }
                let row_id = salt.with(("row", item.id));
                let rect = row(ui, watch, item, rest.lit(row_id), row_id, actions);
                if Rest::resting(ui, rect, scroll) {
                    resting = Some((row_id, rect, item));
                }
                ui.add_space(2.0);
            }
            let item = resting.map(|(_, _, item)| item);
            rest.end(ui, resting.map(|(row, rect, _)| (row, rect)), |ui| {
                if let Some(item) = item {
                    details(ui, item);
                }
            });
        });
}
