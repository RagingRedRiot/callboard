//! Interaction tests: drive the real window code through egui's accessibility
//! tree (clicks, right-clicks, typing, tab drags) with a fake service.
use crate::{
    app::{App, SaveDone, SavePurpose, TestEnds},
    backend::{Contents, Fetched, List, ListData, Request, Target},
    workspace::{LayoutKey, SAVE_DELAY},
};
use callboard_core::{
    feed::Item,
    layout::{Axis, NamedLayout, Panel},
    store::{BoardContents, BoardInfo, Feed, FeedInfo},
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

/// Answer a fetch the way the service would for the seeded data.
fn answer(request: Request) -> Fetched {
    let lists = request
        .lists
        .iter()
        .map(|list| {
            let data = match list {
                List::Feeds => ListData::Feeds(vec![feed_info("a"), feed_info("b")]),
                List::Boards => ListData::Boards(vec![BoardInfo {
                    id: 2,
                    name: "Inbox".into(),
                }]),
                List::Layouts => ListData::Layouts(vec![
                    layout("Day", json!({"kind":"feed","name":"a"})),
                    layout("Ops", json!({"kind":"board","id":2})),
                ]),
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
}

impl Ui {
    /// A window on the seeded service, with its first loads answered.
    fn new() -> Self {
        let (app, ends) = App::for_tests();
        let harness = Harness::builder()
            .with_size([1200.0, 760.0])
            .build_ui_state(|ui, app: &mut App| app.show(ui), app);
        let mut ui = Self { harness, ends };
        ui.settle();
        ui
    }

    /// Run frames, answering fetches as they are issued.
    fn settle(&mut self) {
        for _ in 0..8 {
            self.harness.step();
            if let Ok(request) = self.ends.requests.try_recv() {
                self.ends.responses.send(answer(request)).unwrap();
            }
        }
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
