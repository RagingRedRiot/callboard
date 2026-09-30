//! Interaction tests: drive the real window code through egui's accessibility
//! tree and pointer events (clicks, right-clicks, typing, drags, the wheel)
//! with a fake service.
use crate::{
    app::{App, OpDone, SaveDone, SavePurpose, TestEnds, WriteOp},
    backend::{Contents, Fetched, List, ListData, PromoteKind, Request, Target},
    workspace::{Canvas, LayoutKey, MIN_CARD, SAVE_DELAY, TITLE_HEIGHT},
};
use callboard_core::{
    feed::Item,
    layout::{NamedLayout, Preferences},
    store::{BoardContents, BoardInfo, BoardSummary, Feed, FeedInfo, FeedSummary, ItemViewState},
};
use eframe::egui::{self, Pos2, Rect, Vec2};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use serde_json::json;
use std::time::Duration;

fn feed_info(name: &str) -> FeedInfo {
    FeedInfo {
        name: name.into(),
        title: format!("{name} title"),
        source_url: None,
        stale_after: None,
        last_submitted_at_ms: 0,
        error: None,
    }
}

fn feed(name: &str) -> Target {
    Target::Feed(name.into())
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

/// Answer a fetch the way the service would for the seeded data. Feed "a" has
/// one item; "b" has seven, two snoozed. Inbox has one todo and two notes.
/// Loaded feed contents always hold item 1 and item 2, snoozed; feed "long"
/// has 200 visible items.
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
                            next_wake_at_ms: None,
                        })
                        .into(),
                ),
                List::Boards => ListData::Boards(vec![BoardSummary {
                    info: BoardInfo {
                        id: 2,
                        name: "Inbox".into(),
                    },
                    todo_count: 1,
                    open_todo_count: 1,
                    note_count: 2,
                }]),
                List::Layouts => {
                    ListData::Layouts(saved.layouts.clone(), saved.preferences.clone())
                }
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
                }),
                Target::Feed(name) => Contents::Feed(Feed {
                    info: feed_info(name),
                    items: vec![
                        serde_json::from_value::<Item>(json!({"key":"1","title":"Item"})).unwrap(),
                        serde_json::from_value::<Item>(json!({"key":"2","title":"Later"})).unwrap(),
                    ],
                    manual_order: false,
                    view_state: [(
                        "2".to_owned(),
                        ItemViewState {
                            snoozed_until_ms: None,
                            wake_on_update: true,
                            snoozed: true,
                        },
                    )]
                    .into(),
                }),
                Target::Board(2) => Contents::Board(BoardContents {
                    board: Some(BoardInfo {
                        id: 2,
                        name: "Inbox".into(),
                    }),
                    todos: vec![],
                    notes: vec![],
                }),
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
    ui.harness.get_by_label("Inbox (0)");
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
    ui.harness.get_by_label("Inbox (0)");
    assert!(ui.harness.query_by_label("a title (1)").is_none());
}

#[test]
fn a_card_close_button_removes_it_and_shows_the_empty_canvas_hint() {
    let mut ui = Ui::new();
    ui.harness.get_by_label("×").click();
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
        .query_all_by_value("Show…")
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
    ui.harness.get_by_label("Inbox (0)");
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
    ui.harness.get_by_label("−").click();
    ui.settle();
    assert_eq!(ui.card(&feed("a")).height(), TITLE_HEIGHT);
    // The title still shows counts while collapsed.
    ui.harness.get_by_label("a title (1)");
    assert!(ui.harness.query_by_label("Item").is_none(), "body hidden");
    ui.harness.get_by_label("+").click();
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
    ui.harness.get_by_label("Show all").click();
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
    ui.harness.get_by_label("Save as…").click();
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
    ui.harness.get_by_label("Rename…").click();
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
            result: Ok(Some(renamed)),
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
    ui.harness.get_by_label("Delete…").click();
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

    ui.harness.get_by_label("Delete…").click();
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
            result: Ok(None),
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
    ui.harness.get_by_label("Snooze").click();
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
            result: Ok(None),
        })
        .unwrap();
    ui.harness.step();
    ui.harness.step();
    let request = ui.ends.requests.try_recv().expect("a refetch");
    assert_eq!(request.targets, [Target::Feed("a".into())]);
    ui.ends.responses.send(answer(request, &ui.saved)).unwrap();
    ui.settle();

    ui.harness.get_by_label("Snooze").click();
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
    ui.harness.get_by_label("Promote").click();
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
    ui.harness.get_by_label("Rename…").click();
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
        .query_all_by_label("−")
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
