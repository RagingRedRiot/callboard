use callboard_core::{layout::*, store::Store};
use serde_json::json;

fn leaf() -> Panel {
    Panel::Feed {
        name: "work".into(),
    }
}

#[test]
fn validates_tree_shapes_and_exact_resource_boundaries() {
    for value in [
        json!({}),
        json!({"tree":null}),
        json!({"tree":{"kind":"empty","typo":true}}),
        json!({"tree":{"kind":"split","axis":"diagonal","children":[],"weights":[]}}),
    ] {
        assert!(serde_json::from_value::<Layout>(value).is_err());
    }
    for tree in [
        Panel::Board { id: 1 },
        Panel::Feed {
            name: "bad/name".into(),
        },
        Panel::Tabs {
            children: vec![],
            active: 0,
        },
        Panel::Tabs {
            children: vec![leaf()],
            active: 1,
        },
        Panel::Tabs {
            children: vec![Panel::Empty {}],
            active: 0,
        },
        Panel::Split {
            axis: Axis::Horizontal,
            children: vec![leaf()],
            weights: vec![1.0],
        },
        Panel::Split {
            axis: Axis::Horizontal,
            children: vec![leaf(), leaf()],
            weights: vec![1.0],
        },
        Panel::Split {
            axis: Axis::Horizontal,
            children: vec![leaf(), leaf()],
            weights: vec![1.0, 0.0],
        },
        Panel::Split {
            axis: Axis::Horizontal,
            children: vec![leaf(), leaf()],
            weights: vec![1.0, f64::NAN],
        },
    ] {
        assert!(Layout { tree }.validate().is_err());
    }
    let mut tree = leaf();
    for _ in 1..MAX_LAYOUT_DEPTH {
        tree = Panel::Tabs {
            children: vec![tree],
            active: 0,
        };
    }
    assert!(Layout { tree: tree.clone() }.validate().is_ok());
    assert!(
        Layout {
            tree: Panel::Tabs {
                children: vec![tree],
                active: 0
            }
        }
        .validate()
        .is_err()
    );
    assert!(
        Layout {
            tree: Panel::Tabs {
                children: vec![leaf(); 255],
                active: 254
            }
        }
        .validate()
        .is_ok()
    );
    assert!(
        Layout {
            tree: Panel::Tabs {
                children: vec![leaf(); 256],
                active: 0
            }
        }
        .validate()
        .is_err()
    );
    assert!(validate_name(&"é".repeat(100)).is_ok());
    for name in ["", " spaced", "line\nbreak", &"a".repeat(101)] {
        assert!(validate_name(name).is_err());
    }
}

#[tokio::test]
async fn layout_replacement_survives_restart_and_target_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("layouts.sqlite3");
    let store = Store::open(&path).await.unwrap();
    assert!(store.list_layouts().await.unwrap().is_empty());
    let board = store.create_board("Work").await.unwrap();
    let layout = Layout {
        tree: Panel::Split {
            axis: Axis::Horizontal,
            weights: vec![2.0, 1.0],
            children: vec![leaf(), Panel::Board { id: board.id }],
        },
    };
    let saved = store.save_layout("Work / day", &layout).await.unwrap();
    store.delete_board(board.id, false).await.unwrap();
    assert_eq!(store.list_layouts().await.unwrap(), vec![saved.clone()]);
    let invalid = Layout {
        tree: Panel::Tabs {
            children: vec![],
            active: 0,
        },
    };
    assert!(store.save_layout("Work / day", &invalid).await.is_err());
    assert_eq!(store.list_layouts().await.unwrap(), vec![saved]);
    let replacement = store
        .save_layout(
            "Work / day",
            &Layout {
                tree: Panel::Empty {},
            },
        )
        .await
        .unwrap();
    store.save_layout("Another", &layout).await.unwrap();
    store.close().await;
    let store = Store::open(&path).await.unwrap();
    let layouts = store.list_layouts().await.unwrap();
    assert_eq!(layouts.len(), 2);
    assert_eq!(layouts[0].name, "Another");
    assert_eq!(layouts[1], replacement);
    assert!(store.list_boards().await.unwrap().is_empty());
    assert!(store.list_feeds().await.unwrap().is_empty());
    store.close().await;
}
