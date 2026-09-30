//! Interaction tests: drive the real window code through egui's accessibility
//! tree (clicks, right-clicks, typing, tab drags) with a fake service.
use crate::{
    app::{App, LayoutOp, OpDone, SaveDone, SavePurpose, TestEnds},
    backend::{Contents, Fetched, List, ListData, Request, Target},
    workspace::{LayoutKey, SAVE_DELAY},
};
use callboard_core::{
    feed::Item,
    layout::{Axis, NamedLayout, Panel, Preferences},
    store::{BoardContents, BoardInfo, BoardSummary, Feed, FeedInfo, FeedSummary},
};
use eframe::egui;
use egui_kittest::{Harness, kittest::Queryable};
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

fn layout(name: &str, tree: serde_json::Value) -> NamedLayout {
    NamedLayout {
        name: name.into(),
        tree: serde_json::from_value(tree).unwrap(),
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
                layout("Day", json!({"kind":"feed","name":"a"})),
                layout("Ops", json!({"kind":"board","id":2})),
            ],
            preferences: Preferences::default(),
        }
    }
}

/// Answer a fetch the way the service would for the seeded data. Feed "a" has
/// one item; "b" has seven, two snoozed. Inbox has one todo and two notes.
fn answer(request: Request, saved: &Saved) -> Fetched {
    let lists = request
        .lists
        .iter()
        .map(|list| {
            let data = match list {
                List::Feeds => ListData::Feeds(
                    [("a", 1, 0), ("b", 7, 2)]
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
                Target::Feed(name) => Contents::Feed(Feed {
                    info: feed_info(name),
                    items: vec![
                        serde_json::from_value::<Item>(json!({"key":"1","title":"Item"})).unwrap(),
                    ],
                    manual_order: false,
                    view_state: Default::default(),
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
    fn ops(&self) -> Vec<LayoutOp> {
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

    fn panel(&self) -> Panel {
        self.app().layouts.active().panel()
    }
}

#[test]
fn opens_the_first_saved_layout_with_loaded_panels() {
    let ui = Ui::new();
    assert_eq!(
        ui.app().layouts.active_key(),
        &LayoutKey::Saved("Day".into())
    );
    ui.harness.get_by_label("a title (1)");
    ui.harness.get_by_label("saved");
}

#[test]
fn visible_add_panel_menu_splits_without_a_right_click() {
    let mut ui = Ui::new();
    ui.harness.get_by_label("Add panel…").click();
    ui.settle();
    ui.harness.get_by_label_contains("Board: Inbox").click();
    ui.settle();
    ui.harness.get_by_label("Split right").click();
    ui.settle();
    let Panel::Split { axis, children, .. } = ui.panel() else {
        panic!("expected a split")
    };
    assert_eq!(axis, Axis::Horizontal);
    assert_eq!(children[1], Panel::Board { id: 2 });
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
                tree: job.tree.clone(),
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
fn clicking_a_sidebar_entry_opens_it_as_a_tab_and_clicking_again_reveals_it() {
    let mut ui = Ui::new();
    ui.harness.get_by_label("b title").click();
    ui.settle();
    assert_eq!(
        ui.panel(),
        serde_json::from_value(json!({"kind":"tabs","active":1,"children":[
            {"kind":"feed","name":"a"},{"kind":"feed","name":"b"}]}))
        .unwrap()
    );
    ui.harness.get_by_label("b title (1)");
    ui.harness.get_by_label("a title").click();
    ui.settle();
    assert!(matches!(ui.panel(), Panel::Tabs { active: 0, ref children } if children.len() == 2));
}

#[test]
fn the_context_menu_splits_the_focused_panel() {
    let mut ui = Ui::new();
    ui.harness.get_by_label("Inbox").click_secondary();
    ui.settle();
    ui.harness.get_by_label("Split right").click();
    ui.settle();
    let Panel::Split { axis, children, .. } = ui.panel() else {
        panic!("expected a split, got {:?}", ui.panel())
    };
    assert_eq!(axis, Axis::Horizontal);
    assert_eq!(children[1], Panel::Board { id: 2 });
    ui.harness.get_by_label("Inbox (0)");
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
fn a_tab_close_button_closes_the_panel() {
    let mut ui = Ui::new();
    ui.harness.get_by_label("Close").click();
    ui.settle();
    assert_eq!(ui.panel(), Panel::Empty {});
    ui.harness.get_by_label("No panels in this layout");
}

#[test]
fn the_tab_bar_show_menu_retargets_the_panel() {
    let mut ui = Ui::new();
    ui.harness.get_by_value("Show…").click();
    ui.settle();
    ui.harness.get_by_label("Board: Inbox").click();
    ui.settle();
    assert_eq!(ui.panel(), Panel::Board { id: 2 });
    ui.harness.get_by_label("Inbox (0)");
}

#[test]
fn dragging_a_tab_to_a_panel_edge_splits_the_group() {
    let mut ui = Ui::new();
    ui.harness.get_by_label("b title").click();
    ui.settle();
    let tab = ui.harness.get_by_label("b title (1)").rect();
    // Drop near the right edge of the panel body, below the tab bar.
    let target = egui::pos2(1150.0, 400.0);
    ui.harness.drag_at(tab.center());
    ui.harness.step();
    for step in 1..=10 {
        let t = step as f32 / 10.0;
        ui.harness
            .hover_at(tab.center() + (target - tab.center()) * t);
        ui.harness.step();
    }
    ui.harness.drop_at(target);
    ui.settle();
    let Panel::Split { axis, children, .. } = ui.panel() else {
        panic!("expected a split after the drag, got {:?}", ui.panel())
    };
    assert_eq!(axis, Axis::Horizontal);
    assert_eq!(children[1], Panel::Feed { name: "b".into() });
}

#[test]
fn rearranging_auto_saves_the_active_layout() {
    let mut ui = Ui::new();
    ui.harness.get_by_label("Inbox").click_secondary();
    ui.settle();
    ui.harness.get_by_label("Split below").click();
    ui.settle();
    assert!(
        ui.ends.saves.try_recv().is_err(),
        "waits for changes to settle"
    );
    ui.harness.get_by_label_contains("saving");
    std::thread::sleep(SAVE_DELAY + Duration::from_millis(100));
    ui.settle();
    let job = ui.ends.saves.try_recv().expect("an auto-save");
    assert_eq!(job.name, "Day");
    assert_eq!(job.purpose, SavePurpose::Auto);
    assert!(matches!(
        job.tree,
        Panel::Split {
            axis: Axis::Vertical,
            ..
        }
    ));
    let stored = job.tree.clone();
    ui.ends
        .saved
        .send(SaveDone {
            result: Ok(NamedLayout {
                name: job.name.clone(),
                tree: stored,
                updated_at_ms: 1,
            }),
            job,
        })
        .unwrap();
    ui.settle();
    ui.harness.get_by_label("saved");
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
    let stored = job.tree.clone();
    ui.ends
        .saved
        .send(SaveDone {
            result: Ok(NamedLayout {
                name: "Focus".into(),
                tree: stored,
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

fn remember(name: Option<&str>) -> LayoutOp {
    LayoutOp::Remember {
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
    let op = LayoutOp::Rename {
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
    let renamed = layout("Morning", json!({"kind":"feed","name":"a"}));
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
    let op = LayoutOp::Delete { name: "Day".into() };
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
