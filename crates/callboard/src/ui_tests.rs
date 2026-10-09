//! Interaction tests: drive the real window code through egui's accessibility
//! tree and pointer events (clicks, right-clicks, typing, drags, the wheel)
//! with a fake service.
use crate::{
    app::{App, OpDone, Reply, SaveDone, SavePurpose, TestEnds, WriteOp},
    backend::{Contents, Fetched, List, ListData, PromoteKind, Request, Target},
    watch::WatchOp,
    workspace::{Canvas, LayoutKey, MIN_CARD, SAVE_DELAY, TITLE_HEIGHT},
};
use callboard_core::{
    feed::Item,
    layout::{NamedLayout, Preferences},
    store::{
        BoardContents, BoardInfo, BoardSummary, ChangeStatus, Feed, FeedInfo, FeedSummary,
        ItemChange, ItemViewState, LastChange, RemovedItem,
    },
    watch::{Watch, WatchInfo, WatchItem, WatchSummary},
};
use eframe::egui::{self, Pos2, Rect, Vec2};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use serde_json::json;
use std::time::Duration;

/// Ten minutes ago, in Unix milliseconds.
fn minutes_ago(minutes: i64) -> i64 {
    crate::app::now_ms() - minutes * 60 * 1000
}

/// Feed "a" has a description, a one-hour `new_for` window, and a last change:
/// item 1 (green) added and "Gone item" removed, ten minutes ago.
fn feed_info(name: &str) -> FeedInfo {
    let a = name == "a";
    FeedInfo {
        name: name.into(),
        title: format!("{name} title"),
        description: a.then(|| "Open reviews waiting on me".into()),
        source_url: None,
        stale_after: None,
        new_for: a.then(|| "1h".into()),
        last_submitted_at_ms: 0,
        error: None,
        last_change: a.then(|| LastChange {
            at_ms: minutes_ago(10),
            added: 1,
            updated: 0,
            removed: 1,
            removed_items: vec![RemovedItem {
                key: "9".into(),
                title: "Gone item".into(),
            }],
        }),
    }
}

fn feed(name: &str) -> Target {
    Target::Feed(name.into())
}

const TICKETS: &str = "Tickets I'm waiting on";

fn watch_info() -> WatchInfo {
    WatchInfo {
        name: "tickets".into(),
        title: TICKETS.into(),
        description: None,
        source_url: None,
        stale_after: None,
        waiting_after: "1d".into(),
        quiet_after: "7d".into(),
        last_reported_at_ms: minutes_ago(5),
        error: None,
    }
}

/// Watch "tickets" in queue order: one item in each state.
fn watch() -> Watch {
    let item = |id: i64, state: &str, label: Option<&str>, minutes: i64| -> WatchItem {
        serde_json::from_value(json!({
            "id": id, "url": format!("https://example.com/ticket/{id}"), "label": label,
            "added_at_ms": minutes_ago(minutes), "fingerprint": "1",
            "content": {"title": format!("Ticket {id}")},
            "reported_at_ms": minutes_ago(5), "changed_at_ms": null, "error": null,
            "attention_since_ms": (state == "attention").then(|| minutes_ago(30)),
            "state": state, "state_since_ms": minutes_ago(30),
            "waiting_since_ms": matches!(state, "quiet" | "waiting").then(|| minutes_ago(minutes)),
        }))
        .unwrap()
    };
    Watch {
        info: watch_info(),
        items: vec![
            item(1, "attention", Some("Refund request"), 60),
            item(2, "quiet", None, 20_000),
            item(3, "new", None, 10),
            item(4, "waiting", None, 3_000),
        ],
        next_wake_at_ms: None,
    }
}

/// A saved layout with one card per target, placed as the sidebar would.
fn layout(name: &str, targets: &[Target]) -> NamedLayout {
    let mut canvas = Canvas::new(egui::Id::NULL);
    for target in targets {
        canvas.place(target.clone(), None, Vec2::new(900.0, 680.0));
    }
    NamedLayout {
        name: name.into(),
        layout: canvas.to_layout(),
        updated_at_ms: 0,
    }
}

/// The fake service's layout state, which tests change as the service would.
#[derive(Clone)]
struct Saved {
    layouts: Vec<NamedLayout>,
    preferences: Preferences,
}

impl Default for Saved {
    fn default() -> Self {
        Self {
            layouts: vec![
                layout("Day", &[feed("a")]),
                layout("Ops", &[Target::Board(2)]),
            ],
            preferences: Preferences::default(),
        }
    }
}

/// Board contents from JSON todos and notes, which need only the fields a
/// test cares about.
fn board(
    id: i64,
    name: Option<&str>,
    todos: &[serde_json::Value],
    notes: &[serde_json::Value],
) -> BoardContents {
    let item = |fields: &serde_json::Value, defaults: serde_json::Value| {
        let mut value = defaults;
        for (k, v) in fields.as_object().unwrap() {
            value[k] = v.clone();
        }
        value["resolved_reference"] = json!(null);
        value
    };
    let common = json!({"board_id": id, "body": null, "url": null, "reference": null,
        "position": 0, "created_at_ms": 0, "updated_at_ms": 0, "archived_at_ms": null,
        "archived_from_board": null});
    let todo = |t: &serde_json::Value| {
        let mut d = common.clone();
        d["title"] = json!("");
        d["done"] = json!(false);
        item(t, d)
    };
    let note = |n: &serde_json::Value| {
        let mut d = common.clone();
        d["title"] = json!(null);
        d["color"] = json!(null);
        item(n, d)
    };
    serde_json::from_value(json!({
        // Inbox is blue, as in the board list.
        "board": name.map(|name| json!({
            "id": id, "name": name, "color": (id == 2).then_some("blue")
        })),
        "todos": todos.iter().map(todo).collect::<Vec<_>>(),
        "notes": notes.iter().map(note).collect::<Vec<_>>(),
    }))
    .unwrap()
}

/// Answer a fetch the way the service would for the seeded data. Feed "a" has
/// one item; "b" has seven, two snoozed. Inbox has an open and a done todo and
/// a note, and one archived todo; Later is empty; the deleted-board archive
/// holds one todo. Loaded feed contents always hold item 1 and item 2,
/// snoozed; feed "long" has 200 visible items.
fn answer(request: Request, saved: &Saved) -> Fetched {
    let lists = request
        .lists
        .iter()
        .map(|list| {
            let data = match list {
                List::Feeds => ListData::Feeds(
                    [("a", 1, 0), ("b", 7, 2), ("long", 200, 0)]
                        .map(|(name, items, snoozed)| FeedSummary {
                            info: feed_info(name),
                            item_count: items,
                            snoozed_count: snoozed,
                            new_count: usize::from(name == "a"),
                            next_wake_at_ms: None,
                        })
                        .into(),
                ),
                List::Boards => ListData::Boards(vec![
                    BoardSummary {
                        info: BoardInfo {
                            id: 2,
                            name: "Inbox".into(),
                            color: Some("blue".into()),
                        },
                        todo_count: 2,
                        open_todo_count: 1,
                        note_count: 1,
                    },
                    BoardSummary {
                        info: BoardInfo {
                            id: 3,
                            name: "Later".into(),
                            color: None,
                        },
                        todo_count: 0,
                        open_todo_count: 0,
                        note_count: 0,
                    },
                ]),
                List::Layouts => {
                    ListData::Layouts(saved.layouts.clone(), saved.preferences.clone())
                }
                List::Watches => ListData::Watches(vec![WatchSummary {
                    info: watch_info(),
                    item_count: 4,
                    attention_count: 1,
                    quiet_count: 1,
                    new_count: 1,
                    next_wake_at_ms: None,
                }]),
            };
            (*list, Ok(data))
        })
        .collect();
    let targets = request
        .targets
        .into_iter()
        .map(|target| {
            let contents = match &target {
                Target::Feed(name) if name == "long" => Contents::Feed(Feed {
                    info: feed_info(name),
                    items: (0..200)
                        .map(|i| {
                            serde_json::from_value::<Item>(
                                json!({"key":i.to_string(),"title":format!("Long {i}")}),
                            )
                            .unwrap()
                        })
                        .collect(),
                    manual_order: false,
                    view_state: Default::default(),
                    changes: Default::default(),
                }),
                Target::Feed(name) => Contents::Feed(Feed {
                    info: feed_info(name),
                    items: vec![
                        serde_json::from_value::<Item>(json!({
                            "key": "1", "title": "Item", "url": "https://example.com/1",
                            "body": "Body text", "tags": ["review"],
                            "meta": {"author": "sam", "checks": 42},
                            "color": "green"
                        }))
                        .unwrap(),
                        serde_json::from_value::<Item>(json!({"key":"2","title":"Later"})).unwrap(),
                    ],
                    // Feed "b" has been reordered by hand.
                    manual_order: name == "b",
                    view_state: [(
                        "2".to_owned(),
                        ItemViewState {
                            snoozed_until_ms: None,
                            wake_on_update: true,
                            snoozed: true,
                        },
                    )]
                    .into(),
                    // Item 1 is new in feed "a"; item 2 predates tracking.
                    changes: [
                        (
                            "1".to_owned(),
                            ItemChange {
                                added_at_ms: minutes_ago(10),
                                changed_at_ms: minutes_ago(10),
                                status: (name == "a").then_some(ChangeStatus::New),
                            },
                        ),
                        (
                            "2".to_owned(),
                            ItemChange {
                                added_at_ms: 0,
                                changed_at_ms: 0,
                                status: None,
                            },
                        ),
                    ]
                    .into(),
                }),
                Target::Board(2) => Contents::Board {
                    items: board(
                        2,
                        Some("Inbox"),
                        &[
                            json!({"id": 10, "title": "Write notes"}),
                            json!({"id": 11, "title": "Ship it", "done": true, "position": 1}),
                        ],
                        &[json!({"id": 20, "body": "Remember the milk", "color": "yellow"})],
                    ),
                    archived: Some(board(
                        2,
                        Some("Inbox"),
                        &[json!({"id": 12, "title": "Old task", "archived_at_ms": 1})],
                        &[],
                    )),
                },
                Target::Watch(name) if name == "tickets" => Contents::Watch(watch()),
                Target::Board(3) => Contents::Board {
                    items: board(3, Some("Later"), &[], &[]),
                    archived: Some(board(3, Some("Later"), &[], &[])),
                },
                Target::Archive => Contents::Board {
                    items: board(
                        1,
                        None,
                        &[json!({"id": 13, "title": "Orphan", "archived_at_ms": 1,
                            "archived_from_board": "Gone"})],
                        &[],
                    ),
                    archived: None,
                },
                _ => Contents::Missing,
            };
            (target, Ok(contents))
        })
        .collect();
    Fetched { lists, targets }
}

struct Ui {
    harness: Harness<'static, App>,
    ends: TestEnds,
    saved: Saved,
}

impl Ui {
    /// A window on the seeded service, with its first loads answered.
    fn new() -> Self {
        Self::with(Saved::default())
    }

    fn with(saved: Saved) -> Self {
        let (app, ends) = App::for_tests();
        let harness = Harness::builder()
            .with_size([1200.0, 760.0])
            .build_ui_state(|ui, app: &mut App| app.show(ui), app);
        let mut ui = Self {
            harness,
            ends,
            saved,
        };
        ui.settle();
        ui
    }

    /// Run frames, answering fetches as they are issued.
    fn settle(&mut self) {
        for _ in 0..8 {
            self.harness.step();
            if let Ok(request) = self.ends.requests.try_recv() {
                self.ends
                    .responses
                    .send(answer(request, &self.saved))
                    .unwrap();
            }
        }
    }

    /// Layout operations sent so far, oldest first.
    fn ops(&self) -> Vec<WriteOp> {
        self.ends.ops.try_iter().collect()
    }

    fn clear_field(&mut self, label: &str) {
        self.harness.get_by_label(label).focus();
        self.harness
            .key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
        self.harness.key_press(egui::Key::Backspace);
        self.harness.step();
    }

    fn app(&self) -> &App {
        self.harness.state()
    }

    fn canvas(&self) -> &Canvas {
        self.app().layouts.active()
    }

    /// Back to front.
    fn order(&self) -> Vec<Target> {
        self.canvas()
            .cards
            .iter()
            .map(|c| c.target.clone())
            .collect()
    }

    /// Where a card is drawn now, in screen coordinates.
    fn card(&self, target: &Target) -> Rect {
        self.app()
            .card_screen_rect(target)
            .unwrap_or_else(|| panic!("{target:?} is not on the canvas"))
    }

    /// A point on a card's title bar, clear of its buttons.
    fn title_bar(&self, target: &Target) -> Pos2 {
        self.card(target).left_top() + Vec2::new(24.0, TITLE_HEIGHT / 2.0)
    }

    fn press(&mut self, pos: Pos2, pressed: bool) {
        self.harness.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
    }

    fn click_at(&mut self, pos: Pos2) {
        self.harness.hover_at(pos);
        self.harness.step();
        self.press(pos, true);
        self.harness.step();
        self.press(pos, false);
        self.settle();
    }

    /// A pointer drag in small steps, as a hand would make it.
    fn drag(&mut self, from: Pos2, to: Pos2) {
        self.harness.hover_at(from);
        self.harness.step();
        self.press(from, true);
        self.harness.step();
        for step in 1..=8 {
            self.harness
                .hover_at(from + (to - from) * (step as f32 / 8.0));
            self.harness.step();
        }
        self.press(to, false);
        self.settle();
    }

    fn wheel(&mut self, at: Pos2, delta: Vec2) {
        self.harness.hover_at(at);
        self.harness.step();
        self.harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta,
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        });
        // Smooth scrolling spreads the delta over several frames.
        for _ in 0..30 {
            self.harness.step();
        }
        self.settle();
    }

    /// A point on the canvas no card covers.
    fn empty_canvas(&self) -> Pos2 {
        let area = self.app().canvas_area();
        let covered = |p: Pos2| {
            self.canvas()
                .cards
                .iter()
                .any(|c| self.card(&c.target).expand(8.0).contains(p))
        };
        [
            area.right_bottom() - Vec2::splat(20.0),
            area.left_bottom() + Vec2::new(20.0, -20.0),
        ]
        .into_iter()
        .find(|p| !covered(*p))
        .expect("an empty corner")
    }

    /// Wait out the auto-save debounce and return the one save it sends.
    fn auto_save(&mut self) -> crate::app::SaveJob {
        assert!(
            self.ends.saves.try_recv().is_err(),
            "waits for changes to settle"
        );
        std::thread::sleep(SAVE_DELAY + Duration::from_millis(100));
        self.settle();
        let job = self.ends.saves.try_recv().expect("an auto-save");
        assert_eq!(job.purpose, SavePurpose::Auto);
        job
    }

    /// Confirm every auto-save as the service would until none is pending,
    /// returning the last layout saved. Slow interactions (smooth scrolling
    /// over many frames) may save part-way through.
    fn saves_until_idle(&mut self) -> callboard_core::layout::Layout {
        let mut last = None;
        loop {
            std::thread::sleep(SAVE_DELAY + Duration::from_millis(100));
            self.settle();
            let Ok(job) = self.ends.saves.try_recv() else {
                return last.expect("at least one auto-save");
            };
            assert_eq!(job.purpose, SavePurpose::Auto);
            last = Some(job.layout.clone());
            self.confirm_save(job);
        }
    }

    fn confirm_save(&mut self, job: crate::app::SaveJob) {
        self.ends
            .saved
            .send(SaveDone {
                result: Ok(NamedLayout {
                    name: job.name.clone(),
                    layout: job.layout.clone(),
                    updated_at_ms: 1,
                }),
                job,
            })
            .unwrap();
        self.settle();
    }
}

#[test]
fn opens_the_first_saved_layout_with_loaded_cards() {
    let ui = Ui::new();
    assert_eq!(
        ui.app().layouts.active_key(),
        &LayoutKey::Saved("Day".into())
    );
    ui.harness.get_by_label("a title (1)");
    ui.harness.get_by_label("saved");
    assert!(ui.app().canvas_area().contains_rect(ui.card(&feed("a"))));
}

#[test]
fn add_card_places_it_in_view_and_again_only_reveals_it() {
    let mut ui = Ui::new();
    ui.harness.get_by_label("Add card…").click();
    ui.settle();
    ui.harness.get_by_label("Board: Inbox").click();
    ui.settle();
    assert_eq!(ui.order(), [feed("a"), Target::Board(2)]);
    ui.harness.get_by_label("Inbox (3)");
    assert!(
        ui.app()
            .canvas_area()
            .contains_rect(ui.card(&Target::Board(2)))
    );
    // Offset from the card already in the middle of the view.
    assert_ne!(ui.card(&Target::Board(2)).min, ui.card(&feed("a")).min);
    ui.harness.get_by_label("Add card…").click();
    ui.settle();
    ui.harness.get_by_label("Feed: a title (on canvas)").click();
    ui.settle();
    assert_eq!(
        ui.order(),
        [Target::Board(2), feed("a")],
        "raised, not added"
    );
}

#[test]
fn closing_after_a_failed_save_can_retry_and_wait_for_confirmation() {
    let mut ui = Ui::new();
    ui.harness.get_by_label("Inbox").click();
    ui.settle();
    ui.harness
        .input_mut()
        .viewports
        .get_mut(&egui::ViewportId::ROOT)
        .unwrap()
        .events
        .push(egui::ViewportEvent::Close);
    ui.harness.step();
    let job = ui.ends.saves.try_recv().expect("close flushes debounce");
    ui.ends
        .saved
        .send(SaveDone {
            job,
            result: Err("Service unavailable".into()),
        })
        .unwrap();
    ui.settle();
    ui.harness.get_by_label("Service unavailable");
    assert!(
        !ui.harness.output().viewport_output[&egui::ViewportId::ROOT]
            .commands
            .contains(&egui::ViewportCommand::Close)
    );
    ui.harness.get_by_label("Retry saving").click();
    ui.settle();
    let job = ui
        .ends
        .saves
        .try_recv()
        .expect("retry bypasses normal backoff");
    ui.ends
        .saved
        .send(SaveDone {
            result: Ok(NamedLayout {
                name: job.name.clone(),
                layout: job.layout.clone(),
                updated_at_ms: 2,
            }),
            job,
        })
        .unwrap();
    ui.harness.step();
    assert!(
        ui.harness.output().viewport_output[&egui::ViewportId::ROOT]
            .commands
            .contains(&egui::ViewportCommand::Close)
    );
}

#[test]
fn a_sidebar_click_places_a_card_then_pans_back_to_it() {
    let mut ui = Ui::new();
    ui.harness.get_by_label("b title").click();
    ui.settle();
    assert_eq!(ui.order(), [feed("a"), feed("b")]);
    ui.harness.get_by_label("b title (1)");
    // Pan far away, then a click on the entry brings the card back into view.
    let empty = ui.empty_canvas();
    ui.drag(empty, empty - Vec2::new(900.0, 690.0));
    assert!(!ui.app().canvas_area().intersects(ui.card(&feed("b"))));
    ui.harness.get_by_label("a title").click();
    ui.settle();
    assert!(ui.app().canvas_area().contains_rect(ui.card(&feed("a"))));
    assert_eq!(ui.order(), [feed("b"), feed("a")]);
}

#[test]
fn the_sidebar_context_menu_shows_and_removes_cards() {
    let mut ui = Ui::new();
    ui.harness.get_by_label("Inbox").click_secondary();
    ui.settle();
    assert!(
        ui.harness.query_by_label("Remove card").is_none(),
        "not placed"
    );
    ui.harness.get_by_label("Show card").click();
    ui.settle();
    assert_eq!(ui.order(), [feed("a"), Target::Board(2)]);
    ui.harness.get_by_label("Inbox").click_secondary();
    ui.settle();
    ui.harness.get_by_label("Remove card").click();
    ui.settle();
    assert_eq!(ui.order(), [feed("a")]);
}

#[test]
fn clicking_a_layout_switches_to_it() {
    let mut ui = Ui::new();
    ui.harness.get_by_label("Ops").click();
    ui.settle();
    assert_eq!(
        ui.app().layouts.active_key(),
        &LayoutKey::Saved("Ops".into())
    );
    ui.harness.get_by_label("Inbox (3)");
    assert!(ui.harness.query_by_label("a title (1)").is_none());
}

#[test]
fn a_card_close_button_removes_it_and_shows_the_empty_canvas_hint() {
    let mut ui = Ui::new();
    ui.harness.get_by_label("Close card").click();
    ui.settle();
    assert!(ui.canvas().cards.is_empty());
    ui.harness.get_by_label_contains("No cards in this layout");
}

#[test]
fn show_retargets_a_card_in_place_and_disables_placed_targets() {
    let mut ui = Ui::new();
    ui.harness.get_by_label("b title").click();
    ui.settle();
    let b = ui.card(&feed("b"));
    // Show… on b's card, the front one: the last drawn.
    ui.harness
        .query_all_by_label("Show…")
        .last()
        .unwrap()
        .click();
    ui.settle();
    assert!(
        ui.harness
            .get_by_label("Feed: a title")
            .accesskit_node()
            .is_disabled(),
        "a already has a card"
    );
    ui.harness.get_by_label("Board: Inbox").click();
    ui.settle();
    assert_eq!(ui.order(), [feed("a"), Target::Board(2)]);
    assert_eq!(ui.card(&Target::Board(2)), b, "same place and size");
    ui.harness.get_by_label("Inbox (3)");
}

#[test]
fn dragging_a_title_bar_moves_the_card_and_saves_once() {
    let mut ui = Ui::new();
    let before = ui.canvas().card(&feed("a")).unwrap().rect;
    let grab = ui.title_bar(&feed("a"));
    ui.drag(grab, grab + Vec2::new(120.0, 80.0));
    let after = ui.canvas().card(&feed("a")).unwrap().rect;
    assert_eq!(after.min, before.min + Vec2::new(120.0, 80.0));
    assert_eq!(after.size(), before.size());
    ui.harness.get_by_label_contains("saving");
    let job = ui.auto_save();
    assert_eq!(job.name, "Day");
    assert_eq!(job.layout.cards[0].x, f64::from(after.min.x));
    ui.confirm_save(job);
    ui.harness.get_by_label("saved");
    assert!(ui.ends.saves.try_recv().is_err(), "saved once");
}

#[test]
fn the_corner_grip_resizes_down_to_the_minimum() {
    let mut ui = Ui::new();
    let card = ui.card(&feed("a"));
    let corner = card.right_bottom() - Vec2::splat(4.0);
    ui.drag(corner, corner + Vec2::new(60.0, 40.0));
    let rect = ui.canvas().card(&feed("a")).unwrap().rect;
    assert_eq!(rect.size(), card.size() + Vec2::new(60.0, 40.0));
    let corner = ui.card(&feed("a")).right_bottom() - Vec2::splat(4.0);
    ui.drag(corner, corner - Vec2::splat(700.0));
    let rect = ui.canvas().card(&feed("a")).unwrap().rect;
    assert_eq!(rect.size(), MIN_CARD);
    assert_eq!(rect.min, ui.canvas().card(&feed("a")).unwrap().rect.min);
}

#[test]
fn collapse_folds_a_card_to_its_title_bar_and_expand_restores_it() {
    let mut ui = Ui::new();
    let height = ui.card(&feed("a")).height();
    ui.harness.get_by_label("Collapse card").click();
    ui.settle();
    assert_eq!(ui.card(&feed("a")).height(), TITLE_HEIGHT);
    // The title still shows counts while collapsed.
    ui.harness.get_by_label("a title (1)");
    assert!(ui.harness.query_by_label("Item").is_none(), "body hidden");
    ui.harness.get_by_label("Expand card").click();
    ui.settle();
    assert_eq!(ui.card(&feed("a")).height(), height);
}

#[test]
fn a_press_raises_the_topmost_card_under_the_pointer_only() {
    let mut ui = Ui::new();
    ui.harness.get_by_label("b title").click();
    ui.settle();
    // b cascades onto a: pressing where they overlap hits b, the top card.
    let (a, b) = (ui.card(&feed("a")), ui.card(&feed("b")));
    let overlap = a.intersect(b).center();
    ui.click_at(overlap);
    assert_eq!(ui.order(), [feed("a"), feed("b")]);
    // a's visible strip above b raises a.
    ui.click_at(ui.title_bar(&feed("a")));
    assert_eq!(ui.order(), [feed("b"), feed("a")]);
    // Press and release within one frame, as a quick click can arrive, on
    // b's strip that a (now in front) leaves uncovered.
    let (a, b) = (ui.card(&feed("a")), ui.card(&feed("b")));
    let strip = [
        Pos2::new(b.right() - 10.0, b.center().y),
        Pos2::new(b.center().x, b.bottom() - 10.0),
        Pos2::new(b.left() + 10.0, b.center().y),
    ]
    .into_iter()
    .find(|p| !a.contains(*p))
    .expect("part of b is uncovered");
    ui.harness.hover_at(strip);
    ui.harness.step();
    ui.press(strip, true);
    ui.press(strip, false);
    ui.settle();
    assert_eq!(ui.order(), [feed("a"), feed("b")]);
    // The order is part of the layout.
    let saved = ui.saves_until_idle();
    assert!(matches!(
        &saved.cards[1].target,
        callboard_core::layout::Target::Feed { name } if name == "b"
    ));
}

#[test]
fn empty_canvas_drags_and_wheels_pan_but_a_card_wheel_scrolls_it() {
    let mut ui = Ui::new();
    ui.harness.get_by_label("long title").click();
    ui.settle();
    let empty = ui.empty_canvas();
    ui.drag(empty, empty + Vec2::new(-100.0, -50.0));
    assert_eq!(ui.canvas().view, Pos2::new(100.0, 50.0));
    let empty = ui.empty_canvas();
    ui.wheel(empty, Vec2::new(0.0, -40.0));
    let view = ui.canvas().view;
    assert!((view.y - 90.0).abs() < 1.0, "wheel pans: {view:?}");
    // Over the long feed the wheel scrolls its items and leaves the view.
    let long = ui.card(&feed("long"));
    let first = ui.harness.get_by_label("Long 0").rect().top();
    ui.wheel(long.center(), Vec2::new(0.0, -600.0));
    assert_eq!(ui.canvas().view, view, "view unchanged");
    let scrolled = ui.harness.get_by_label("Long 0").rect().top();
    assert!(
        scrolled < first - 500.0,
        "items scrolled: {first} → {scrolled}"
    );
    // Panning is part of the layout.
    let saved = ui.saves_until_idle();
    assert_eq!((saved.view.x, saved.view.y), (100.0, 90.0));
}

#[test]
fn show_all_pans_back_to_the_cards() {
    let mut ui = Ui::new();
    let empty = ui.empty_canvas();
    ui.drag(empty, empty - Vec2::new(800.0, 600.0));
    assert!(!ui.app().canvas_area().intersects(ui.card(&feed("a"))));
    ui.layout_action("Show all");
    ui.settle();
    assert!(ui.app().canvas_area().intersects(ui.card(&feed("a"))));
}

#[test]
fn dragging_a_sidebar_entry_onto_the_canvas_places_it_there() {
    let mut ui = Ui::new();
    let entry = ui.harness.get_by_label("Inbox").rect().center();
    let drop = ui.app().canvas_area().right_bottom() - Vec2::new(300.0, 200.0);
    ui.drag(entry, drop);
    assert_eq!(ui.order(), [feed("a"), Target::Board(2)]);
    // The pointer lands on the new card's title bar.
    let card = ui.card(&Target::Board(2));
    assert!(
        Rect::from_min_size(card.min, Vec2::new(card.width(), TITLE_HEIGHT)).contains(drop),
        "{card:?} under {drop:?}"
    );
    // Dragging a placed entry moves its card.
    let entry = ui.harness.get_by_label("a title").rect().center();
    let drop = ui.app().canvas_area().left_top() + Vec2::new(200.0, 150.0);
    ui.drag(entry, drop);
    assert_eq!(ui.order(), [Target::Board(2), feed("a")]);
    assert!(ui.card(&feed("a")).contains(drop));
}

#[test]
fn save_as_names_the_arrangement_and_switches_to_it() {
    let mut ui = Ui::new();
    ui.layout_action("Save as…");
    ui.settle();
    // An existing name is refused before anything is sent.
    ui.harness.get_by_label("Layout name").type_text("Ops");
    ui.harness.get_by_label("Save").click();
    ui.settle();
    ui.harness.get_by_label_contains("already exists");
    assert!(ui.ends.saves.try_recv().is_err());
    let field = ui.harness.get_by_label("Layout name");
    field.focus();
    for _ in 0..3 {
        ui.harness.key_press(egui::Key::Backspace);
    }
    ui.harness.step();
    ui.harness.get_by_label("Layout name").type_text("Focus");
    ui.harness.get_by_label("Save").click();
    ui.settle();
    let job = ui.ends.saves.try_recv().expect("a save-as request");
    assert_eq!(job.name, "Focus");
    assert_eq!(
        job.purpose,
        SavePurpose::SaveAs {
            from: LayoutKey::Saved("Day".into())
        }
    );
    let stored = job.layout.clone();
    ui.ends
        .saved
        .send(SaveDone {
            result: Ok(NamedLayout {
                name: "Focus".into(),
                layout: stored,
                updated_at_ms: 1,
            }),
            job,
        })
        .unwrap();
    ui.settle();
    assert_eq!(
        ui.app().layouts.active_key(),
        &LayoutKey::Saved("Focus".into())
    );
    assert!(
        ui.harness.query_by_label("Layout name").is_none(),
        "dialog closed"
    );
}

fn remember(name: Option<&str>) -> WriteOp {
    WriteOp::Remember {
        name: name.map(str::to_owned),
    }
}

#[test]
fn unplaced_sidebar_entries_show_counts_from_the_lists() {
    let ui = Ui::new();
    // Feed "b" is not placed: 7 items, 2 snoozed. Inbox: 1 todo + 2 notes.
    ui.harness.get_by_label("5");
    ui.harness.get_by_label("+2 snoozed");
    ui.harness.get_by_label("3");
}

#[test]
fn startup_opens_the_preferred_layout_and_switching_remembers_it() {
    let mut ui = Ui::with(Saved {
        preferences: Preferences {
            last_layout: Some("Ops".into()),
        },
        ..Saved::default()
    });
    assert_eq!(
        ui.app().layouts.active_key(),
        &LayoutKey::Saved("Ops".into())
    );
    assert!(ui.ops().is_empty(), "already the preference");
    ui.harness.get_by_label("Day").click();
    ui.settle();
    assert_eq!(ui.ops(), [remember(Some("Day"))]);
    ui.harness.get_by_label("Unsaved (not saved)").click();
    ui.settle();
    assert_eq!(ui.ops(), [remember(None)]);
}

#[test]
fn without_a_preference_the_first_layout_opens_and_is_remembered() {
    let ui = Ui::new();
    assert_eq!(
        ui.app().layouts.active_key(),
        &LayoutKey::Saved("Day".into())
    );
    assert_eq!(ui.ops(), [remember(Some("Day"))]);
}

#[test]
fn rename_refuses_taken_names_then_renames_the_active_layout() {
    let mut ui = Ui::new();
    ui.ops();
    ui.layout_action("Rename…");
    ui.settle();
    ui.harness.get_by_label("Rename layout");
    assert_eq!(
        ui.harness.get_by_label("Layout name").value().as_deref(),
        Some("Day"),
        "prefilled with the current name"
    );
    ui.clear_field("Layout name");
    ui.harness.get_by_label("Layout name").type_text("Ops");
    ui.harness.get_by_label("Save").click();
    ui.settle();
    ui.harness.get_by_label_contains("already exists");
    assert!(ui.ops().is_empty());
    ui.clear_field("Layout name");
    ui.harness.get_by_label("Layout name").type_text("Morning");
    ui.harness.get_by_label("Save").click();
    ui.settle();
    let op = WriteOp::Rename {
        from: "Day".into(),
        to: "Morning".into(),
    };
    assert_eq!(ui.ops(), std::slice::from_ref(&op));
    ui.harness.get_by_label("Saving…");
    // The service refuses (another window took the name meanwhile).
    ui.ends
        .ops_done
        .send(OpDone {
            op: op.clone(),
            result: Err("HTTP 409 Conflict: layout name is already in use".into()),
        })
        .unwrap();
    ui.settle();
    ui.harness.get_by_label_contains("already in use");
    assert_eq!(
        ui.app().layouts.active_key(),
        &LayoutKey::Saved("Day".into())
    );
    // Retry succeeds.
    ui.harness.get_by_label("Save").click();
    ui.settle();
    assert_eq!(ui.ops(), std::slice::from_ref(&op));
    let renamed = layout("Morning", &[feed("a")]);
    ui.saved.layouts[0] = renamed.clone();
    ui.saved.layouts.sort_by(|a, b| a.name.cmp(&b.name));
    ui.ends
        .ops_done
        .send(OpDone {
            op,
            result: Ok(Reply::Layout(renamed)),
        })
        .unwrap();
    ui.settle();
    assert_eq!(
        ui.app().layouts.active_key(),
        &LayoutKey::Saved("Morning".into())
    );
    assert!(ui.harness.query_by_label("Rename layout").is_none());
    assert!(ui.harness.query_by_label("Day").is_none());
    ui.harness.get_by_label("Layout: Morning");
    assert_eq!(ui.ops(), [remember(Some("Morning"))]);
}

#[test]
fn delete_asks_for_confirmation_then_opens_the_next_layout() {
    let mut ui = Ui::new();
    ui.ops();
    ui.layout_action("Delete…");
    ui.settle();
    ui.harness.get_by_label("Delete the layout “Day”?");
    ui.harness.get_by_label("Cancel").click();
    ui.settle();
    assert!(
        ui.harness
            .query_by_label("Delete the layout “Day”?")
            .is_none()
    );
    assert!(ui.ops().is_empty());

    ui.layout_action("Delete…");
    ui.settle();
    ui.harness.get_by_label("Delete").click();
    ui.settle();
    let op = WriteOp::Delete { name: "Day".into() };
    assert_eq!(ui.ops(), std::slice::from_ref(&op));
    ui.harness.get_by_label("Deleting…");
    ui.saved.layouts.remove(0);
    ui.ends
        .ops_done
        .send(OpDone {
            op,
            result: Ok(Reply::Done),
        })
        .unwrap();
    ui.settle();
    assert_eq!(
        ui.app().layouts.active_key(),
        &LayoutKey::Saved("Ops".into())
    );
    assert!(
        ui.harness
            .query_by_label("Delete the layout “Day”?")
            .is_none()
    );
    assert!(ui.harness.query_by_label("Day").is_none());
    assert_eq!(ui.ops(), [remember(Some("Ops"))]);
}

#[test]
fn feed_items_snooze_for_a_duration_or_until_they_change() {
    let mut ui = Ui::new();
    ui.ops();
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    ui.feed_menu(0);
    ui.harness.get_by_label_contains("Snooze").click();
    ui.settle();
    ui.harness.get_by_label("For 1 hour").click();
    ui.settle();
    let ops = ui.ops();
    let [
        WriteOp::Snooze {
            feed,
            key,
            until_ms: Some(until),
            on_update: false,
        },
    ] = ops.as_slice()
    else {
        panic!("expected one timed snooze, got {ops:?}")
    };
    assert_eq!((feed.as_str(), key.as_str()), ("a", "1"));
    let hour = 60 * 60 * 1000;
    assert!((before + hour..before + hour + 60_000).contains(until));
    // Success refetches the feed without waiting for the change notice.
    ui.ends
        .ops_done
        .send(OpDone {
            op: ops[0].clone(),
            result: Ok(Reply::Done),
        })
        .unwrap();
    ui.harness.step();
    ui.harness.step();
    let request = ui.ends.requests.try_recv().expect("a refetch");
    assert_eq!(request.targets, [Target::Feed("a".into())]);
    ui.ends.responses.send(answer(request, &ui.saved)).unwrap();
    ui.settle();

    ui.feed_menu(0);
    ui.harness.get_by_label_contains("Snooze").click();
    ui.settle();
    ui.harness.get_by_label("Until it changes").click();
    ui.settle();
    assert_eq!(
        ui.ops(),
        [WriteOp::Snooze {
            feed: "a".into(),
            key: "1".into(),
            until_ms: None,
            on_update: true,
        }]
    );
}

#[test]
fn a_snoozed_item_can_be_unsnoozed_once_shown() {
    let mut ui = Ui::new();
    ui.ops();
    assert!(ui.harness.query_by_label("Unsnooze").is_none());
    ui.harness.get_by_label("Show snoozed (1)").click();
    ui.settle();
    ui.feed_menu(1);
    ui.harness.get_by_label("Unsnooze").click();
    ui.settle();
    assert_eq!(
        ui.ops(),
        [WriteOp::Snooze {
            feed: "a".into(),
            key: "2".into(),
            until_ms: None,
            on_update: false,
        }]
    );
}

#[test]
fn promoting_an_item_names_the_board_and_kind_and_reports_failure() {
    let mut ui = Ui::new();
    ui.ops();
    ui.feed_menu(0);
    ui.harness.get_by_label_contains("Promote").click();
    ui.settle();
    // The submenu label carries an arrow; the sidebar has an "Inbox" entry too.
    ui.harness
        .query_all_by_label_contains("Inbox")
        .find(|n| n.rect().min.x > 300.0)
        .expect("the board submenu")
        .click();
    ui.settle();
    ui.harness.get_by_label("As note").click();
    ui.settle();
    let op = WriteOp::Promote {
        feed: "a".into(),
        key: "1".into(),
        board_id: 2,
        kind: PromoteKind::Note,
    };
    assert_eq!(ui.ops(), std::slice::from_ref(&op));
    ui.ends
        .ops_done
        .send(OpDone {
            op,
            result: Err("HTTP 404 Not Found: feed item not found: a/1".into()),
        })
        .unwrap();
    ui.settle();
    ui.harness
        .get_by_label_contains("Could not promote the item: HTTP 404");
    ui.harness.get_by_label("Dismiss").click();
    ui.settle();
    assert!(
        ui.harness
            .query_by_label_contains("Could not promote")
            .is_none()
    );
}

/// Type as a keyboard would, into whatever has focus.
fn type_keys(ui: &mut Ui, text: &str) {
    ui.harness
        .input_mut()
        .events
        .push(egui::Event::Text(text.into()));
    ui.harness.step();
}

#[test]
fn rename_typing_replaces_the_selected_name_and_refusals_keep_focus() {
    let mut ui = Ui::new();
    ui.ops();
    ui.layout_action("Rename…");
    ui.settle();
    // No click into the field: it has focus with "Day" selected.
    type_keys(&mut ui, "Ops");
    ui.harness.key_press(egui::Key::Enter);
    ui.settle();
    ui.harness.get_by_label_contains("already exists");
    // Still focused after the refusal: keep typing.
    type_keys(&mut ui, "2");
    ui.harness.key_press(egui::Key::Enter);
    ui.settle();
    assert_eq!(
        ui.ops(),
        [WriteOp::Rename {
            from: "Day".into(),
            to: "Ops2".into(),
        }]
    );
}

#[test]
fn buttons_on_a_card_behind_others_work_on_the_first_click() {
    let mut ui = Ui::new();
    ui.harness.get_by_label("b title").click();
    ui.settle();
    // a is behind b; its title bar strip above b stays visible.
    let a = ui.card(&feed("a"));
    let collapse = ui
        .harness
        .query_all_by_label("Collapse card")
        .map(|n| n.rect().center())
        .find(|p| a.contains(*p) && !ui.card(&feed("b")).contains(*p))
        .expect("a's collapse button is visible");
    ui.click_at(collapse);
    assert_eq!(ui.order(), [feed("b"), feed("a")], "raised");
    assert!(
        ui.canvas().card(&feed("a")).unwrap().collapsed,
        "and collapsed"
    );
}

#[test]
fn the_right_and_bottom_edges_resize_one_dimension_each() {
    let mut ui = Ui::new();
    let card = ui.card(&feed("a"));
    let right = Pos2::new(card.right() - 2.0, card.center().y);
    ui.drag(right, right + Vec2::new(50.0, 30.0));
    let rect = ui.canvas().card(&feed("a")).unwrap().rect;
    assert_eq!(rect.size(), card.size() + Vec2::new(50.0, 0.0));
    let card = ui.card(&feed("a"));
    let bottom = Pos2::new(card.center().x, card.bottom() - 2.0);
    ui.drag(bottom, bottom + Vec2::new(30.0, -1000.0));
    let rect = ui.canvas().card(&feed("a")).unwrap().rect;
    assert_eq!(rect.size(), Vec2::new(card.width(), MIN_CARD.y));
    assert_eq!(rect.min, ui.canvas().card(&feed("a")).unwrap().rect.min);
}

#[test]
fn a_deleted_target_stays_as_a_placeholder_until_retargeted() {
    let mut ui = Ui::with(Saved {
        layouts: vec![layout("Day", &[Target::Board(9)])],
        ..Saved::default()
    });
    let before = ui.card(&Target::Board(9));
    ui.harness.get_by_label("Board 9 · deleted");
    ui.harness.get_by_label("Board 9 no longer exists");
    ui.harness.get_by_label("Show…").click();
    ui.settle();
    ui.harness.get_by_label("Feed: a title").click();
    ui.settle();
    assert_eq!(ui.order(), [feed("a")]);
    assert_eq!(ui.card(&feed("a")), before, "same place and size");
    ui.harness.get_by_label("a title (1)");
}

// Board editing (DESIGN.md §6.3). Inbox holds todo 10 "Write notes", done
// todo 11 "Ship it", note 20 "Remember the milk", and archived todo 12.

use crate::board::{BoardOp, Kind};
use egui_kittest::kittest::by;

/// A window with the Inbox card placed and the startup writes drained.
fn inbox() -> Ui {
    let mut ui = Ui::new();
    ui.harness.get_by_label("Inbox").click();
    ui.settle();
    ui.ops();
    ui
}

fn board_op(op: BoardOp) -> WriteOp {
    WriteOp::Board(op)
}

fn patch(kind: Kind, id: i64, patch: serde_json::Value) -> WriteOp {
    board_op(BoardOp::Patch {
        kind,
        id,
        board: 2,
        patch,
    })
}

impl Ui {
    fn field(&self, placeholder: &'static str) -> egui_kittest::Node<'_> {
        self.harness
            .get(by().predicate(move |n| n.placeholder() == Some(placeholder)))
    }

    /// Open the n-th item menu (…) on the canvas, top to bottom.
    /// Open the n-th item menu (…) on Inbox's card, top to bottom. Feed
    /// items have menus too; Inbox is drawn last, so its three come last.
    fn item_menu(&mut self, n: usize) {
        let inbox = self.card(&Target::Board(2));
        let menus: Vec<Pos2> = self
            .harness
            .query_all_by_label("Item menu")
            .map(|node| node.rect().center())
            .filter(|p| inbox.contains(*p))
            .collect();
        let board = &menus[menus.len() - 3..];
        self.click_at(board[n]);
    }

    /// Open feed "a"'s item menu (…) for its n-th shown item.
    fn feed_menu(&mut self, n: usize) {
        let card = self.card(&feed("a"));

        let at = self
            .harness
            .query_all_by_label("Item menu")
            .map(|node| node.rect().center())
            .filter(|p| card.contains(*p))
            .nth(n)
            .expect("a feed item menu");
        self.click_at(at);
    }

    /// Focus a labelled field and type into it.
    fn type_into(&mut self, label: &str, text: &str) {
        self.harness.get_by_label(label).focus();
        self.harness.step();
        type_keys(self, text);
    }

    /// Choose an entry from the layout menu in the layout bar.
    fn layout_action(&mut self, label: &str) {
        self.harness.get_by_label_contains("Layout: ").click();
        self.settle();
        self.harness.get_by_label(label).click();
    }

    /// A menu entry on the canvas, not the sidebar or layout bar entry
    /// with the same label.
    fn menu_item(&mut self, label: &str) {
        let area = self.app().canvas_area();
        self.harness
            .query_all_by_label(label)
            .find(|n| area.contains(n.rect().center()))
            .unwrap_or_else(|| panic!("no “{label}” menu entry on the canvas"))
            .click();
        self.settle();
    }

    fn finish(&mut self, op: WriteOp, result: Result<Reply, String>) {
        self.ends.ops_done.send(OpDone { op, result }).unwrap();
        self.settle();
    }
}

#[test]
fn enter_adds_a_todo_and_keeps_the_field_ready() {
    let mut ui = inbox();
    ui.field("Add a todo").focus();
    ui.harness.step();
    type_keys(&mut ui, "Buy milk");
    ui.harness.key_press(egui::Key::Enter);
    ui.settle();
    type_keys(&mut ui, "Call Sam");
    ui.harness.key_press(egui::Key::Enter);
    ui.settle();
    assert_eq!(
        ui.ops(),
        [
            board_op(BoardOp::AddTodo {
                board: 2,
                title: "Buy milk".into()
            }),
            board_op(BoardOp::AddTodo {
                board: 2,
                title: "Call Sam".into()
            }),
        ]
    );
    assert_eq!(ui.field("Add a todo").value().as_deref(), Some(""));
    // Blank entries add nothing; notes have their own field.
    ui.harness.key_press(egui::Key::Enter);
    ui.settle();
    ui.field("Add a note").focus();
    ui.harness.step();
    type_keys(&mut ui, "Idea");
    ui.harness.key_press(egui::Key::Enter);
    ui.settle();
    assert_eq!(
        ui.ops(),
        [board_op(BoardOp::AddNote {
            board: 2,
            body: "Idea".into()
        })]
    );
}

#[test]
fn a_todo_checkbox_marks_it_done_or_not() {
    let mut ui = inbox();
    ui.harness.get_by_label("Write notes").click();
    ui.settle();
    ui.harness.get_by_label("Ship it").click();
    ui.settle();
    assert_eq!(
        ui.ops(),
        [
            patch(Kind::Todo, 10, json!({"done": true})),
            patch(Kind::Todo, 11, json!({"done": false})),
        ]
    );
}

#[test]
fn the_editor_saves_changed_fields_only_and_escape_discards() {
    let mut ui = inbox();
    ui.item_menu(0);
    ui.harness.get_by_label("Edit…").click();
    ui.settle();
    // An empty title is refused before anything is sent.
    ui.clear_field("Title");
    ui.harness.get_by_label("Save").click();
    ui.settle();
    ui.harness.get_by_label("A todo needs a title");
    ui.type_into("Title", "Write the notes");
    ui.type_into("Link", "https://example.com");
    ui.harness.get_by_label("Save").click();
    ui.settle();
    assert_eq!(
        ui.ops(),
        [patch(
            Kind::Todo,
            10,
            json!({"title": "Write the notes", "url": "https://example.com"})
        )]
    );
    assert!(ui.harness.query_by_label("Title").is_none(), "closed");
    // The note editor also offers colors; Escape discards.
    ui.item_menu(2);
    ui.harness.get_by_label("Edit…").click();
    ui.settle();
    ui.harness.get_by_label("Green").click();
    ui.settle();
    ui.harness.get_by_label("Text").focus();
    ui.harness.step();
    ui.harness.key_press(egui::Key::Escape);
    ui.settle();
    assert!(ui.harness.query_by_label("Text").is_none(), "discarded");
    assert!(ui.ops().is_empty());
    ui.item_menu(2);
    ui.harness.get_by_label("Edit…").click();
    ui.settle();
    ui.harness.get_by_label("None").click();
    ui.harness.get_by_label("Save").click();
    ui.settle();
    assert_eq!(ui.ops(), [patch(Kind::Note, 20, json!({"color": null}))]);
}

#[test]
fn dragging_a_handle_moves_the_item_within_its_list() {
    let mut ui = inbox();
    let handles: Vec<Pos2> = ui
        .harness
        .query_all_by_label("Drag to reorder")
        .map(|n| n.rect().center())
        .collect();
    assert_eq!(handles.len(), 3, "two todos and a note");
    // Below "Ship it": the first todo becomes the second.
    ui.drag(handles[0], handles[1] + Vec2::new(0.0, 20.0));
    assert_eq!(ui.ops(), [patch(Kind::Todo, 10, json!({"position": 1}))]);
    // Dropped where it started: nothing to send.
    ui.drag(handles[1], handles[1] + Vec2::new(0.0, 4.0));
    assert!(ui.ops().is_empty());
}

#[test]
fn the_item_menu_moves_archives_and_deletes_after_confirming() {
    let mut ui = inbox();
    ui.item_menu(0);
    ui.harness.get_by_label_contains("Move to").click();
    ui.settle();
    ui.menu_item("Later");
    ui.item_menu(0);
    ui.harness.get_by_label("Archive").click();
    ui.settle();
    assert_eq!(
        ui.ops(),
        [
            patch(Kind::Todo, 10, json!({"board_id": 3})),
            patch(Kind::Todo, 10, json!({"archived": true})),
        ]
    );
    ui.item_menu(2);
    ui.menu_item("Delete…");
    ui.harness
        .get_by_label("Delete the note “Remember the milk”?");
    ui.harness.get_by_label("Delete").click();
    ui.settle();
    let op = board_op(BoardOp::DeleteItem {
        kind: Kind::Note,
        id: 20,
        board: 2,
    });
    assert_eq!(ui.ops(), std::slice::from_ref(&op));
    ui.harness.get_by_label("Deleting…");
    ui.finish(op, Ok(Reply::Done));
    assert!(
        ui.harness
            .query_by_label_contains("Delete the note")
            .is_none()
    );
}

#[test]
fn archived_items_show_on_request_and_restore() {
    let mut ui = inbox();
    assert!(ui.harness.query_by_label_contains("Old task").is_none());
    ui.harness.get_by_label("Show archived (1)").click();
    ui.settle();
    ui.harness.get_by_label("Old task");
    ui.harness.get_by_label("Restore").click();
    ui.settle();
    assert_eq!(
        ui.ops(),
        [patch(Kind::Todo, 12, json!({"archived": false}))]
    );
}

#[test]
fn the_deleted_board_archive_restores_to_a_chosen_board() {
    let mut ui = Ui::new();
    ui.harness.get_by_label("Deleted-board archive").click();
    ui.settle();
    ui.ops();
    ui.harness.get_by_label("From “Gone”");
    ui.harness.get_by_label_contains("Restore to").click();
    ui.settle();
    ui.menu_item("Inbox");
    assert_eq!(
        ui.ops(),
        [board_op(BoardOp::Patch {
            kind: Kind::Todo,
            id: 13,
            board: 1,
            patch: json!({"archived": false, "board_id": 2}),
        })]
    );
}

#[test]
fn the_board_menu_renames_archives_done_and_deletes_the_board() {
    let mut ui = inbox();
    ui.harness.get_by_label("Board").click();
    ui.settle();
    ui.menu_item("Rename…");
    ui.harness.get_by_label("Rename board");
    assert_eq!(
        ui.harness.get_by_label("Board name").value().as_deref(),
        Some("Inbox")
    );
    type_keys(&mut ui, "Later");
    ui.harness.key_press(egui::Key::Enter);
    ui.settle();
    ui.harness
        .get_by_label("A board named “Later” already exists");
    type_keys(&mut ui, " on");
    ui.harness.key_press(egui::Key::Enter);
    ui.settle();
    let op = board_op(BoardOp::Rename {
        id: 2,
        name: "Later on".into(),
    });
    assert_eq!(ui.ops(), std::slice::from_ref(&op));
    ui.finish(op, Ok(Reply::Done));
    assert!(ui.harness.query_by_label("Rename board").is_none());

    ui.harness.get_by_label("Board").click();
    ui.settle();
    ui.harness.get_by_label("Archive done").click();
    ui.settle();
    assert_eq!(
        ui.ops(),
        [board_op(BoardOp::ArchiveDone {
            board: 2,
            ids: vec![11]
        })]
    );

    ui.harness.get_by_label("Board").click();
    ui.settle();
    ui.harness.get_by_label("Delete board…").click();
    ui.settle();
    ui.harness.get_by_label("Delete the board “Inbox”?");
    ui.harness.get_by_label("Delete").click();
    ui.settle();
    let op = board_op(BoardOp::Delete { id: 2 });
    assert_eq!(ui.ops(), std::slice::from_ref(&op));
    // A refusal stays in the dialog.
    ui.finish(op.clone(), Err("HTTP 503 Service Unavailable: busy".into()));
    ui.harness.get_by_label_contains("HTTP 503");
    ui.harness.get_by_label("Delete").click();
    ui.settle();
    assert_eq!(ui.ops(), std::slice::from_ref(&op));
    ui.finish(op, Ok(Reply::Done));
    assert_eq!(ui.order(), [feed("a")], "the card closes");
}

#[test]
fn new_board_creates_it_and_places_its_card() {
    let mut ui = Ui::new();
    ui.ops();
    ui.harness.get_by_label("New board…").click();
    ui.settle();
    ui.harness.get_by_label("New board");
    type_keys(&mut ui, "Ideas");
    ui.harness.key_press(egui::Key::Enter);
    ui.settle();
    let op = board_op(BoardOp::Create {
        name: "Ideas".into(),
    });
    assert_eq!(ui.ops(), std::slice::from_ref(&op));
    ui.harness.get_by_label("Saving…");
    ui.finish(
        op,
        Ok(Reply::Board(BoardInfo {
            id: 4,
            name: "Ideas".into(),
            color: None,
        })),
    );
    assert!(ui.harness.query_by_label("New board").is_none());
    assert_eq!(ui.order(), [feed("a"), Target::Board(4)]);
}

#[test]
fn a_board_entry_menu_offers_rename_and_delete() {
    let mut ui = Ui::new();
    ui.harness.get_by_label("Later").click_secondary();
    ui.settle();
    ui.harness.get_by_label("Delete board…").click();
    ui.settle();
    ui.harness.get_by_label("Delete the board “Later”?");
    ui.harness.get_by_label("Cancel").click();
    ui.settle();
    assert!(ui.app().delete_prompt.is_none());
    ui.harness.get_by_label("Later").click_secondary();
    ui.settle();
    ui.harness.get_by_label("Rename board…").click();
    ui.settle();
    assert_eq!(
        ui.harness.get_by_label("Board name").value().as_deref(),
        Some("Later")
    );
    assert!(matches!(
        ui.app().prompt.as_ref().map(|p| p.kind),
        Some(crate::app::PromptKind::RenameBoard(3))
    ));
}

#[test]
fn board_writes_refetch_on_success_and_report_failures() {
    let mut ui = inbox();
    let op = patch(Kind::Todo, 10, json!({"done": true}));
    ui.ends
        .ops_done
        .send(OpDone {
            op: op.clone(),
            result: Ok(Reply::Done),
        })
        .unwrap();
    ui.harness.step();
    ui.harness.step();
    let request = ui.ends.requests.try_recv().expect("a refetch");
    assert!(request.targets.contains(&Target::Board(2)));
    assert!(request.lists.contains(&crate::backend::List::Boards));
    ui.ends.responses.send(answer(request, &ui.saved)).unwrap();
    ui.settle();
    ui.finish(op, Err("HTTP 404 Not Found: todo not found".into()));
    ui.harness
        .get_by_label_contains("Could not change the todo: HTTP 404");
}

/// Feed "a" and Inbox side by side, so neither covers the other.
fn side_by_side() -> Ui {
    let layout = serde_json::from_value(json!({
        "view": {"x": 0, "y": 0},
        "cards": [
            {"target": {"kind": "feed", "name": "a"},
             "x": 10, "y": 10, "width": 400, "height": 500, "collapsed": false},
            {"target": {"kind": "board", "id": 2},
             "x": 430, "y": 10, "width": 420, "height": 600, "collapsed": false},
        ],
    }))
    .unwrap();
    let ui = Ui::with(Saved {
        layouts: vec![NamedLayout {
            name: "Day".into(),
            layout,
            updated_at_ms: 0,
        }],
        ..Saved::default()
    });
    ui.ops();
    ui
}

fn promote(kind: PromoteKind) -> WriteOp {
    WriteOp::Promote {
        feed: "a".into(),
        key: "1".into(),
        board_id: 2,
        kind,
    }
}

#[test]
fn dragging_a_feed_item_onto_a_board_promotes_it_into_the_list_under_it() {
    let mut ui = side_by_side();
    let grip = ui
        .harness
        .get_by_label("Drag to reorder or promote")
        .rect()
        .center();
    let todos = ui.harness.get_by_label("Todos").rect().center();
    let notes = ui.harness.get_by_label("Notes").rect().center();
    ui.drag(grip, todos);
    assert_eq!(ui.ops(), [promote(PromoteKind::Todo)]);
    ui.drag(grip, notes);
    assert_eq!(ui.ops(), [promote(PromoteKind::Note)]);
    // The board's title bar counts as the card: a todo.
    ui.drag(grip, ui.title_bar(&Target::Board(2)));
    assert_eq!(ui.ops(), [promote(PromoteKind::Todo)]);
}

#[test]
fn a_feed_item_dropped_off_a_board_or_cancelled_promotes_nothing() {
    let mut ui = side_by_side();
    let grip = ui
        .harness
        .get_by_label("Drag to reorder or promote")
        .rect()
        .center();
    ui.drag(grip, ui.empty_canvas());
    ui.drag(grip, grip + Vec2::new(40.0, 60.0));
    assert!(ui.ops().is_empty());
    // Escape mid-drag cancels even over the board.
    let todos = ui.harness.get_by_label("Todos").rect().center();
    ui.harness.hover_at(grip);
    ui.harness.step();
    ui.press(grip, true);
    ui.harness.step();
    for step in 1..=8 {
        ui.harness
            .hover_at(grip + (todos - grip) * (step as f32 / 8.0));
        ui.harness.step();
    }
    ui.harness.key_press(egui::Key::Escape);
    ui.press(todos, false);
    ui.settle();
    assert!(ui.ops().is_empty());
    // The deleted-board archive takes no drops.
    ui.harness.get_by_label("Deleted-board archive").click();
    ui.settle();
    assert_eq!(ui.order().last(), Some(&Target::Archive), "in front");
    let drop = ui.card(&Target::Archive).center();
    ui.drag(grip, drop);
    assert!(
        ui.ops()
            .iter()
            .all(|op| !matches!(op, WriteOp::Promote { .. })),
        "no promotion"
    );
}

// Quick open (DESIGN.md §6.1).

impl Ui {
    fn quick_open(&mut self) {
        self.harness
            .key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::K);
        self.settle();
        self.harness.get_by_label("Open a feed, board, or layout");
    }

    fn quick_open_is_closed(&self) -> bool {
        self.harness
            .query_by_label("Open a feed, board, or layout")
            .is_none()
    }
}

#[test]
fn ctrl_k_finds_a_board_by_name_and_enter_places_its_card() {
    let mut ui = Ui::new();
    ui.quick_open();
    type_keys(&mut ui, "inb");
    ui.settle();
    ui.harness.get_by_label("Board: Inbox");
    assert!(ui.harness.query_by_label("Feed: a title").is_none());
    ui.harness.key_press(egui::Key::Enter);
    ui.settle();
    assert!(ui.quick_open_is_closed());
    assert_eq!(ui.order(), [feed("a"), Target::Board(2)]);
}

#[test]
fn arrows_move_the_selection_and_layouts_switch() {
    let mut ui = Ui::new();
    ui.quick_open();
    // Empty: every choice, in sidebar order. Down twice then up once.
    ui.harness.get_by_label("Feed: a title (on canvas)");
    for key in [
        egui::Key::ArrowDown,
        egui::Key::ArrowDown,
        egui::Key::ArrowUp,
    ] {
        ui.harness.key_press(key);
        ui.settle();
    }
    // Enter opens the second choice.
    ui.harness.key_press(egui::Key::Enter);
    ui.settle();
    assert_eq!(ui.order(), [feed("a"), feed("b")]);
    ui.quick_open();
    type_keys(&mut ui, "ops");
    ui.settle();
    ui.harness.get_by_label("Layout: Ops").click();
    ui.settle();
    assert_eq!(
        ui.app().layouts.active_key(),
        &LayoutKey::Saved("Ops".into())
    );
}

#[test]
fn quick_open_closes_on_escape_or_a_click_outside_and_says_when_nothing_matches() {
    let mut ui = Ui::new();
    ui.quick_open();
    type_keys(&mut ui, "zzz");
    ui.settle();
    ui.harness.get_by_label("No matches");
    ui.harness.key_press(egui::Key::Enter);
    ui.settle();
    ui.harness.key_press(egui::Key::Escape);
    ui.settle();
    assert!(ui.quick_open_is_closed());
    assert_eq!(ui.order(), [feed("a")], "nothing opened");
    // The layout bar button opens it too; a click elsewhere closes it.
    ui.harness.get_by_label("Open…").click();
    ui.settle();
    ui.harness.get_by_label("Open a feed, board, or layout");
    ui.click_at(ui.empty_canvas());
    assert!(ui.quick_open_is_closed());
}

// Feed item reordering (DESIGN.md §6.2).

#[test]
fn dragging_a_feed_item_within_its_feed_moves_it() {
    let mut ui = side_by_side();
    ui.harness.get_by_label("Show snoozed (1)").click();
    ui.settle();
    let handles: Vec<Pos2> = ui
        .harness
        .query_all_by_label("Drag to reorder or promote")
        .map(|n| n.rect().center())
        .collect();
    assert_eq!(handles.len(), 2);
    ui.drag(handles[0], handles[1] + Vec2::new(0.0, 40.0));
    assert_eq!(
        ui.ops(),
        [WriteOp::Reorder {
            feed: "a".into(),
            key: "1".into(),
            position: 1,
        }]
    );
    // Released where it started: nothing is sent, and no promotion either.
    ui.drag(handles[1], handles[1] + Vec2::new(0.0, 4.0));
    assert!(ui.ops().is_empty());
}

#[test]
fn reset_order_shows_only_for_a_manually_ordered_feed() {
    let mut ui = Ui::new();
    assert!(ui.harness.query_by_label("Reset order").is_none());
    ui.harness.get_by_label("b title").click();
    ui.settle();
    ui.ops();
    ui.harness.get_by_label("Reset order").click();
    ui.settle();
    assert_eq!(
        ui.ops(),
        [WriteOp::ResetOrder {
            feed: "b".into(),
            key: "1".into(),
        }]
    );
}

// Polish: ticks show at once; feed handles do not raise their card.

fn ticked(ui: &Ui, title: &str) -> bool {
    ui.harness.get_by_label(title).accesskit_node().toggled()
        == Some(egui::accesskit::Toggled::True)
}

#[test]
fn a_tick_shows_at_once_reverts_on_failure_and_yields_to_the_service() {
    let mut ui = inbox();
    ui.harness.get_by_label("Write notes").click();
    ui.settle();
    assert!(
        ticked(&ui, "Write notes"),
        "shown before the service answers"
    );
    // The open count follows the tick too.
    ui.harness.get_by_label("2 todos (0 open) · 1 note");
    let op = patch(Kind::Todo, 10, json!({"done": true}));
    assert_eq!(ui.ops(), std::slice::from_ref(&op));
    ui.finish(op.clone(), Err("HTTP 503 Service Unavailable: busy".into()));
    assert!(!ticked(&ui, "Write notes"), "reverted");
    ui.harness
        .get_by_label_contains("Could not change the todo");

    // Stored, but reloads keep showing it open (changed back elsewhere):
    // the tick holds through one reload, then the service's state wins.
    ui.harness.get_by_label("Write notes").click();
    ui.settle();
    assert_eq!(ui.ops(), std::slice::from_ref(&op));
    ui.finish(op, Ok(Reply::Done));
    assert!(ticked(&ui, "Write notes"));
    ui.harness.get_by_label("Refresh").click();
    ui.settle();
    assert!(!ticked(&ui, "Write notes"));
}

#[test]
fn pressing_a_feed_handle_leaves_a_covering_board_in_front() {
    let mut ui = inbox();
    assert_eq!(ui.order(), [feed("a"), Target::Board(2)]);
    let grip = ui
        .harness
        .get_by_label("Drag to reorder or promote")
        .rect()
        .center();
    assert!(!ui.card(&Target::Board(2)).contains(grip), "handle visible");
    let todos = ui.harness.get_by_label("Todos").rect().center();
    ui.drag(grip, todos);
    assert_eq!(ui.order(), [feed("a"), Target::Board(2)], "not raised");
    assert_eq!(ui.ops(), [promote(PromoteKind::Todo)]);
    // A click elsewhere on the feed card still raises it.
    ui.click_at(ui.title_bar(&feed("a")));
    assert_eq!(ui.order(), [Target::Board(2), feed("a")]);
}

// Feed item details (DESIGN.md §6.2).

impl Ui {
    /// Rest the pointer at `at` for `seconds` of harness time.
    fn rest(&mut self, at: Pos2, seconds: f32) {
        self.harness.hover_at(at);
        let steps = (seconds / 0.25).round() as usize;
        for _ in 0..steps.max(1) {
            self.harness.step();
        }
    }

    fn details_shown(&self) -> bool {
        self.harness.query_by_label_contains("Key: 1").is_some()
    }
}

#[test]
fn resting_on_a_feed_item_for_a_second_shows_everything_about_it() {
    let mut ui = side_by_side();
    // The row shows the title and the link without its scheme.
    ui.harness.get_by_label("example.com/1");
    assert!(ui.harness.query_by_label("Body text").is_none());
    let row = ui.harness.get_by_label("Item").rect().center();
    ui.rest(row, 0.5);
    assert!(!ui.details_shown(), "not yet");
    ui.rest(row, 0.75);
    assert!(ui.details_shown());
    for text in ["Body text", "review", "author", "sam", "checks", "42"] {
        ui.harness.get_by_label(text);
    }
    ui.harness.get_by_label("https://example.com/1");
    // Moving off hides it.
    ui.rest(ui.empty_canvas(), 0.25);
    assert!(!ui.details_shown());
}

#[test]
fn passing_over_items_or_using_their_menu_shows_no_details() {
    let mut ui = side_by_side();
    ui.harness.get_by_label("Show snoozed (1)").click();
    ui.settle();
    let first = ui.harness.get_by_label("Item").rect().center();
    let card = ui.card(&feed("a"));
    let second = ui
        .harness
        .query_all_by_label("Later")
        .map(|n| n.rect().center())
        .find(|p| card.contains(*p))
        .expect("the snoozed item");
    // Half a second on each, back and forth: the wait restarts per row.
    for at in [first, second, first, second] {
        ui.rest(at, 0.5);
        assert!(!ui.details_shown());
    }
    // An open menu suppresses the card.
    ui.feed_menu(0);
    ui.rest(first, 2.0);
    assert!(!ui.details_shown());
}

// Feed descriptions and change tracking (DESIGN.md §4.3, §6.2).

#[test]
fn a_feed_card_shows_its_description_last_change_and_marked_items() {
    let mut ui = side_by_side();
    ui.harness.get_by_label("Open reviews waiting on me");
    ui.harness
        .get_by_label_contains("changed 10m ago: 1 new, 1 gone");
    // Item 1 was added ten minutes ago, inside feed "a"'s one-hour window.
    ui.harness.get_by_label("new");
    ui.harness.get_by_label("1 new");
    assert!(ui.harness.query_by_label("Mark all seen").is_none());
    ui.harness
        .get_by_label_contains("changed 10m ago: 1 new, 1 gone")
        .hover();
    for _ in 0..4 {
        ui.harness.step();
    }
    ui.harness.get_by_label("Gone item");
}

#[test]
fn details_show_when_an_item_was_added_its_color_and_change_nothing() {
    let mut ui = side_by_side();
    let row = ui.harness.get_by_label("Item").rect().center();
    ui.rest(row, 1.25);
    ui.harness.get_by_label("Added 10m ago");
    ui.harness.get_by_label("Key: 1 · color: green");
    assert!(ui.ops().is_empty(), "looking marks nothing");
    // Items from before change tracking show no times.
    ui.rest(ui.empty_canvas(), 0.25);
    ui.harness.get_by_label("Show snoozed (1)").click();
    ui.settle();
    let card = ui.card(&feed("a"));
    let later = ui
        .harness
        .query_all_by_label("Later")
        .map(|n| n.rect().center())
        .find(|p| card.contains(*p))
        .unwrap();
    ui.rest(later, 1.25);
    ui.harness.get_by_label("Key: 2");
    assert!(ui.harness.query_by_label_contains("Added").is_none());
}

#[test]
fn a_mark_ends_when_the_window_does_without_a_refetch() {
    let mut ui = side_by_side();
    ui.harness.get_by_label("new");
    // Item 1's hour runs out in 300 ms.
    let ends_soon = crate::app::now_ms() - 3_600_000 + 300;
    match &mut ui
        .harness
        .state_mut()
        .cache
        .contents
        .get_mut(&feed("a"))
        .unwrap()
        .contents
    {
        Some(Contents::Feed(a)) => {
            let change = a.changes.get_mut("1").unwrap();
            change.added_at_ms = ends_soon;
            change.changed_at_ms = ends_soon;
        }
        _ => panic!("feed a is loaded"),
    }
    ui.harness.step();
    ui.harness.get_by_label("new");
    std::thread::sleep(Duration::from_millis(400));
    ui.harness.step();
    assert!(ui.harness.query_by_label("new").is_none());
    assert!(ui.ends.requests.try_recv().is_err(), "no refetch needed");
}

// Colors set in the GUI (DESIGN.md §6.3).

#[test]
fn the_board_menu_recolors_the_board() {
    let mut ui = inbox();
    ui.harness.get_by_label("Board").click();
    ui.settle();
    ui.harness.get_by_label_contains("Color").hover();
    ui.settle();
    // The current color does nothing; another color or None recolors.
    ui.harness.get_by_label("Blue").click();
    ui.settle();
    assert!(ui.ops().is_empty());
    for (label, color) in [("Green", Some("green")), ("None", None)] {
        ui.harness.get_by_label("Board").click();
        ui.settle();
        ui.harness.get_by_label_contains("Color").hover();
        ui.settle();
        ui.harness.get_by_label(label).click();
        ui.settle();
        assert_eq!(
            ui.ops(),
            [board_op(BoardOp::Recolor {
                id: 2,
                color: color.map(str::to_owned),
            })]
        );
    }
}

#[test]
fn todos_and_notes_take_any_of_the_eight_colors() {
    let mut ui = inbox();
    ui.item_menu(0);
    ui.harness.get_by_label("Edit…").click();
    ui.settle();
    ui.harness.get_by_label("Red").click();
    ui.harness.get_by_label("Save").click();
    ui.settle();
    assert_eq!(ui.ops(), [patch(Kind::Todo, 10, json!({"color": "red"}))]);
    ui.item_menu(2);
    ui.harness.get_by_label("Edit…").click();
    ui.settle();
    for label in ["Red", "Orange", "Gray"] {
        ui.harness.get_by_label(label);
    }
    ui.harness.get_by_label("Gray").click();
    ui.harness.get_by_label("Save").click();
    ui.settle();
    assert_eq!(ui.ops(), [patch(Kind::Note, 20, json!({"color": "gray"}))]);
}

#[test]
fn a_service_of_another_build_shows_a_version_mismatch() {
    let mut ui = Ui::new();
    ui.ends
        .signals
        .send(crate::events::Signal::Connected)
        .unwrap();
    ui.ends
        .signals
        .send(crate::events::Signal::Build(
            callboard_service::BUILD.into(),
        ))
        .unwrap();
    ui.settle();
    assert!(ui.harness.query_by_label("Version mismatch").is_none());
    ui.ends
        .signals
        .send(crate::events::Signal::Build("0123456789ab".into()))
        .unwrap();
    ui.settle();
    ui.harness.get_by_label("Version mismatch");
    assert!(crate::app::build_mismatch("0123456789ab").contains("callboard upgrade"));
}

#[test]
fn a_missing_service_binary_is_named_instead_of_the_socket_error() {
    let (app, ends) = App::for_tests();
    let mut harness = Harness::builder()
        .with_size([1200.0, 760.0])
        .build_ui_state(
            |ui, app: &mut App| app.show(ui),
            app.without_service_binary(),
        );
    let unreachable = "/run/user/1000/callboard.sock: No such file or directory (os error 2)";
    for _ in 0..8 {
        harness.step();
        if let Ok(request) = ends.requests.try_recv() {
            let lists = request
                .lists
                .into_iter()
                .map(|list| (list, Err(unreachable.to_owned())))
                .collect();
            ends.responses
                .send(Fetched {
                    lists,
                    targets: vec![],
                })
                .unwrap();
        }
    }
    harness.get_by_label_contains("service binary is not installed");
    assert!(harness.query_by_label_contains("No such file").is_none());
}

/// A window with the "tickets" watch card placed and startup writes drained.
fn tickets() -> Ui {
    let mut ui = Ui::new();
    ui.harness.get_by_label(TICKETS).click();
    ui.settle();
    ui.ops();
    ui
}

fn watch_op(op: WatchOp) -> WriteOp {
    WriteOp::Watch(op)
}

#[test]
fn a_watch_card_shows_its_queue_and_counts() {
    let mut ui = tickets();
    assert!(ui.order().contains(&Target::Watch("tickets".into())));
    for heading in ["Needs attention (1)", "Quiet (1)", "New (1)", "Waiting (1)"] {
        ui.harness.get_by_label(heading);
    }
    // The label names the row; the reported title follows it.
    ui.harness.get_by_label("Refund request");
    ui.harness.get_by_label("Ticket 1");
    // Counts in the card's title bar and the sidebar, collapsed or not.
    let title = format!("{TICKETS} (4) · 1 need attention · 1 quiet");
    ui.harness.get_by_label(&title);
    ui.harness.get_by_label("1 attention");
    ui.harness.get_by_label("1 quiet");
    let card = ui.card(&Target::Watch("tickets".into()));
    ui.harness
        .query_all_by_label("Collapse card")
        .find(|n| card.contains(n.rect().center()))
        .expect("the watch card's collapse button")
        .click();
    ui.settle();
    assert!(
        ui.canvas()
            .card(&Target::Watch("tickets".into()))
            .unwrap()
            .collapsed
    );
    ui.harness.get_by_label(&title);
}

#[test]
fn acknowledge_and_keep_waiting_act_on_their_items() {
    let mut ui = tickets();
    ui.harness.get_by_label("Acknowledge").click();
    ui.settle();
    ui.harness.get_by_label("Keep waiting").click();
    ui.settle();
    let ops = ui.ops();
    assert_eq!(
        ops,
        [
            watch_op(WatchOp::Acknowledge {
                watch: "tickets".into(),
                id: 1
            }),
            watch_op(WatchOp::KeepWaiting {
                watch: "tickets".into(),
                id: 2
            }),
        ]
    );
    // Success refetches the watch without waiting for the change notice.
    ui.finish(ops[0].clone(), Ok(Reply::Done));
    // A failure shows in the error bar.
    ui.finish(ops[1].clone(), Err("HTTP 500: storage failure".into()));
    ui.harness
        .get_by_label_contains("Could not keep the item waiting");
}

#[test]
fn the_add_fields_add_a_url_with_an_optional_label() {
    let mut ui = tickets();
    ui.field("Add a URL to watch").focus();
    ui.harness.step();
    type_keys(&mut ui, "https://example.com/ticket/9");
    ui.harness.key_press(egui::Key::Enter);
    ui.settle();
    ui.field("Add a URL to watch").focus();
    ui.harness.step();
    type_keys(&mut ui, " https://example.com/ticket/10 ");
    ui.field("Label (optional)").focus();
    ui.harness.step();
    type_keys(&mut ui, "Waiting on legal");
    ui.harness.key_press(egui::Key::Enter);
    ui.settle();
    assert_eq!(
        ui.ops(),
        [
            watch_op(WatchOp::Add {
                watch: "tickets".into(),
                url: "https://example.com/ticket/9".into(),
                label: None,
            }),
            watch_op(WatchOp::Add {
                watch: "tickets".into(),
                url: "https://example.com/ticket/10".into(),
                label: Some("Waiting on legal".into()),
            }),
        ]
    );
    assert_eq!(ui.field("Add a URL to watch").value().as_deref(), Some(""));
    assert_eq!(ui.field("Label (optional)").value().as_deref(), Some(""));
}

#[test]
fn removing_an_item_and_deleting_a_watch_ask_first() {
    let mut ui = tickets();
    let card = ui.card(&Target::Watch("tickets".into()));
    // The menu on the "Refund request" row; the feed card's lie beneath.
    let row = ui.harness.get_by_label("Refund request").rect();
    let menu = ui
        .harness
        .query_all_by_label("Item menu")
        .map(|node| node.rect().center())
        .find(|p| {
            card.contains(*p)
                && p.x > row.right()
                && (row.top() - 4.0..row.top() + 24.0).contains(&p.y)
        })
        .expect("the row's item menu");
    ui.click_at(menu);
    ui.harness.get_by_label("Remove…").click();
    ui.settle();
    ui.harness.get_by_label("Stop watching “Refund request”?");
    ui.harness.get_by_label("Delete").click();
    ui.settle();
    let ops = ui.ops();
    assert_eq!(
        ops,
        [watch_op(WatchOp::Remove {
            watch: "tickets".into(),
            id: 1
        })]
    );
    ui.finish(ops[0].clone(), Ok(Reply::Done));
    assert!(ui.app().delete_prompt.is_none());

    let entry = ui
        .harness
        .query_all_by_label(TICKETS)
        .find(|n| !card.contains(n.rect().center()))
        .expect("the sidebar entry")
        .rect()
        .center();
    ui.harness.hover_at(entry);
    ui.harness.step();
    ui.harness.event(egui::Event::PointerButton {
        pos: entry,
        button: egui::PointerButton::Secondary,
        pressed: true,
        modifiers: egui::Modifiers::NONE,
    });
    ui.harness.event(egui::Event::PointerButton {
        pos: entry,
        button: egui::PointerButton::Secondary,
        pressed: false,
        modifiers: egui::Modifiers::NONE,
    });
    ui.settle();
    ui.harness.get_by_label("Delete watch…").click();
    ui.settle();
    ui.harness.get_by_label("Delete").click();
    ui.settle();
    let ops = ui.ops();
    assert_eq!(
        ops,
        [WriteOp::DeleteWatch {
            name: "tickets".into()
        }]
    );
    ui.finish(ops[0].clone(), Ok(Reply::Done));
    assert!(!ui.order().contains(&Target::Watch("tickets".into())));
}

// Canvas zoom (DESIGN.md §6.1).

impl Ui {
    fn zoom(&self) -> f32 {
        self.canvas().zoom
    }

    /// The canvas point drawn at a screen position.
    fn canvas_point(&self, at: Pos2) -> Pos2 {
        self.canvas()
            .to_screen(self.app().canvas_area().min)
            .inverse()
            * at
    }

    fn ctrl_wheel(&mut self, at: Pos2, delta: Vec2) {
        self.harness.hover_at(at);
        self.harness.step();
        self.harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta,
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..30 {
            self.harness.step();
        }
        self.settle();
    }

    fn zoom_key(&mut self, key: egui::Key) {
        self.harness
            .key_press_modifiers(egui::Modifiers::COMMAND, key);
        self.settle();
    }
}

#[test]
fn ctrl_wheel_zooms_around_the_pointer_even_over_a_card_and_is_saved() {
    let mut ui = side_by_side();
    let at = ui.card(&feed("a")).center();
    let under = ui.canvas_point(at);
    let width = ui.card(&feed("a")).width();
    ui.ctrl_wheel(at, Vec2::new(0.0, -100.0));
    let zoom = ui.zoom();
    assert!(zoom < 0.9, "zoomed out: {zoom}");
    assert!((ui.canvas_point(at) - under).length() < 1.0, "anchored");
    assert!((ui.card(&feed("a")).width() - width * zoom).abs() < 0.5);
    // Positions and sizes stay in canvas units; the zoom is saved with the
    // view in whole percent.
    let saved = ui.saves_until_idle();
    assert_eq!(saved.view.zoom, (f64::from(zoom) * 100.0).round() / 100.0);
    assert_eq!((saved.cards[0].x, saved.cards[0].width), (10.0, 400.0));
    // Pinching zooms too.
    let zoom = ui.zoom();
    ui.harness.hover_at(at);
    ui.harness.event(egui::Event::Zoom(1.5));
    ui.settle();
    assert!((ui.zoom() - zoom * 1.5).abs() < 0.001);
}

#[test]
fn ctrl_up_and_down_step_the_canvas_zoom_and_the_indicator_resets() {
    let mut ui = side_by_side();
    let middle = ui.app().canvas_area().center();
    let under = ui.canvas_point(middle);
    ui.zoom_key(egui::Key::ArrowUp);
    assert_eq!(ui.zoom(), 1.1);
    ui.harness.get_by_label("Zoom 110%");
    assert!((ui.canvas_point(middle) - under).length() < 0.01);
    for _ in 0..3 {
        ui.zoom_key(egui::Key::ArrowDown);
    }
    assert_eq!(ui.zoom(), 0.75);
    ui.harness.get_by_label("Zoom 75%").click();
    ui.settle();
    assert_eq!(ui.zoom(), 1.0);
    assert!((ui.canvas_point(middle) - under).length() < 0.01);

    // In a text field they keep moving the cursor; the canvas stays put.
    ui.field("Add a todo").focus();
    ui.harness.step();
    ui.zoom_key(egui::Key::ArrowDown);
    assert_eq!(ui.zoom(), 1.0);
}

#[test]
fn ctrl_plus_and_minus_scale_the_whole_window_not_the_canvas() {
    let mut ui = side_by_side();
    ui.zoom_key(egui::Key::Equals);
    assert!(ui.harness.ctx.zoom_factor() > 1.0);
    assert_eq!(ui.zoom(), 1.0, "the canvas zoom is unchanged");
    ui.harness.get_by_label("Zoom 100%");
    ui.zoom_key(egui::Key::Num0);
    assert_eq!(ui.harness.ctx.zoom_factor(), 1.0);
}

#[test]
fn resize_grips_keep_their_screen_size_when_zoomed_out() {
    let mut ui = side_by_side();
    while ui.zoom() > 0.25 {
        ui.zoom_key(egui::Key::ArrowDown);
    }
    assert_eq!(ui.zoom(), 0.25);
    let before = ui.canvas().card(&feed("a")).unwrap().rect.size();
    // Four screen points in from the right edge, halfway down.
    let card = ui.card(&feed("a"));
    let edge = egui::pos2(card.right() - 4.0, card.center().y);
    ui.drag(edge, edge + Vec2::new(10.0, 0.0));
    let after = ui.canvas().card(&feed("a")).unwrap().rect.size();
    assert_eq!(after - before, Vec2::new(40.0, 0.0));
}

#[test]
fn cards_move_resize_drop_and_promote_the_same_way_when_zoomed() {
    let mut ui = side_by_side();
    let close = ui.harness.query_all_by_label("Close card").next().unwrap();
    let full = close.rect().width();
    while ui.zoom() > 0.5 {
        ui.zoom_key(egui::Key::ArrowDown);
    }
    assert_eq!(ui.zoom(), 0.5);
    // Accessibility bounds follow the drawn cards.
    let close = ui.harness.query_all_by_label("Close card").next().unwrap();
    assert!((close.rect().width() - full / 2.0).abs() < 0.5);
    let todos = ui.harness.get_by_label("Todos").rect();
    assert!(ui.card(&Target::Board(2)).contains_rect(todos));

    // Drag-to-promote finds the scaled board.
    let grip = ui
        .harness
        .get_by_label("Drag to reorder or promote")
        .rect()
        .center();
    let todos = ui.harness.get_by_label("Todos").rect().center();
    ui.drag(grip, todos);
    assert_eq!(ui.ops(), [promote(PromoteKind::Todo)]);

    // A screen drag moves a card twice as far in canvas units.
    let from = ui.canvas().card(&feed("a")).unwrap().rect;
    let title = ui.title_bar(&feed("a"));
    ui.drag(title, title + Vec2::new(40.0, 30.0));
    let moved = ui.canvas().card(&feed("a")).unwrap().rect;
    assert_eq!(moved.min - from.min, Vec2::new(80.0, 60.0));
    // And resizes it twice as much.
    let corner = ui.card(&feed("a")).right_bottom() - Vec2::splat(3.0);
    ui.drag(corner, corner + Vec2::new(20.0, 10.0));
    let resized = ui.canvas().card(&feed("a")).unwrap().rect;
    assert_eq!(resized.size() - moved.size(), Vec2::new(40.0, 20.0));

    // A sidebar drop still lands the pointer on the card's title bar.
    let entry = ui.harness.get_by_label("Inbox").rect().center();
    let drop = ui.app().canvas_area().left_top() + Vec2::new(200.0, 150.0);
    ui.drag(entry, drop);
    let card = ui.card(&Target::Board(2));
    let bar = Rect::from_min_size(card.min, Vec2::new(card.width(), TITLE_HEIGHT * 0.5));
    assert!(bar.contains(drop), "{card:?} under {drop:?}");
    assert_eq!(ui.zoom(), 0.5, "the drop kept the zoom");
}

#[test]
fn show_all_zooms_out_until_every_card_fits() {
    let layout = serde_json::from_value(json!({
        "view": {"x": 0, "y": 0, "zoom": 1.5},
        "cards": [
            {"target": {"kind": "feed", "name": "a"},
             "x": 0, "y": 0, "width": 400, "height": 500, "collapsed": false},
            {"target": {"kind": "board", "id": 2},
             "x": 2600, "y": 900, "width": 420, "height": 600, "collapsed": false},
        ],
    }))
    .unwrap();
    let mut ui = Ui::with(Saved {
        layouts: vec![NamedLayout {
            name: "Day".into(),
            layout,
            updated_at_ms: 0,
        }],
        ..Saved::default()
    });
    assert_eq!(ui.zoom(), 1.5, "opened at its saved zoom");
    ui.harness.get_by_label("Zoom 150%");
    ui.layout_action("Show all");
    ui.settle();
    assert!(ui.zoom() < 0.5);
    let area = ui.app().canvas_area();
    assert!(area.contains_rect(ui.card(&feed("a"))));
    assert!(area.contains_rect(ui.card(&Target::Board(2))));
}

#[test]
fn item_details_stay_full_size_when_zoomed_out() {
    let mut ui = side_by_side();
    let row = ui.harness.get_by_label("Item").rect().center();
    ui.rest(row, 1.25);
    let full = ui.harness.get_by_label("Body text").rect().height();
    ui.rest(ui.empty_canvas(), 0.25);
    while ui.zoom() > 0.5 {
        ui.zoom_key(egui::Key::ArrowDown);
    }
    let row = ui.harness.get_by_label("Item").rect().center();
    ui.rest(row, 1.25);
    assert!(ui.details_shown());
    assert_eq!(ui.harness.get_by_label("Body text").rect().height(), full);
}

#[test]
fn revealing_a_card_too_big_for_the_view_zooms_out_just_enough() {
    let mut ui = side_by_side();
    ui.zoom_key(egui::Key::ArrowUp);
    ui.zoom_key(egui::Key::ArrowUp);
    ui.zoom_key(egui::Key::ArrowUp);
    assert_eq!(ui.zoom(), 1.5);
    ui.harness.get_by_label("Inbox").click();
    ui.settle();
    let zoom = ui.zoom();
    assert!(zoom < 1.5, "{zoom}");
    let area = ui.app().canvas_area();
    assert!(area.contains_rect(ui.card(&Target::Board(2))));
    // Revealing a card that already fits keeps the zoom.
    ui.harness.get_by_label("a title").click();
    ui.settle();
    assert_eq!(ui.zoom(), zoom);
}
