use callboard_core::{layout::*, store::Store};
use serde_json::{Value, json};

fn card(target: Target, x: f64) -> Card {
    Card {
        target,
        x,
        y: 0.0,
        width: 420.0,
        height: 560.0,
        collapsed: false,
    }
}

fn feed(name: &str) -> Target {
    Target::Feed { name: name.into() }
}

fn layout(cards: Vec<Card>) -> Layout {
    Layout {
        view: View { x: -40.0, y: 0.0 },
        cards,
    }
}

fn leaf() -> Layout {
    layout(vec![card(feed("work"), 0.0)])
}

#[test]
fn parses_the_documented_body_and_rejects_unknown_or_missing_fields() {
    let body = json!({
        "view": {"x": -40.0, "y": 0.0},
        "cards": [
            {"target": {"kind": "feed", "name": "reviews"},
             "x": 0.0, "y": 0.0, "width": 420.0, "height": 560.0, "collapsed": false},
            {"target": {"kind": "board", "id": 2},
             "x": 440.0, "y": 0.0, "width": 360.0, "height": 300.0, "collapsed": true}
        ]
    });
    let parsed: Layout = serde_json::from_value(body.clone()).unwrap();
    assert!(parsed.validate().is_ok());
    assert_eq!(parsed.cards[1].target, Target::Board { id: 2 });
    assert_eq!(serde_json::to_value(&parsed).unwrap(), body);
    // Integer coordinates, as in DESIGN.md, parse too.
    let integers = json!({"view":{"x":-40,"y":0},"cards":[{"target":{"kind":"board","id":2},
        "x":0,"y":0,"width":420,"height":560,"collapsed":false}]});
    assert!(serde_json::from_value::<Layout>(integers).is_ok());

    let with = |edit: fn(&mut Value)| {
        let mut value = body.clone();
        edit(&mut value);
        serde_json::from_value::<Layout>(value)
    };
    for edit in [
        (|v: &mut Value| v["typo"] = json!(1)) as fn(&mut Value),
        |v| v["view"]["z"] = json!(1),
        |v| v["cards"][0]["z"] = json!(1),
        |v| v["cards"][0]["target"]["extra"] = json!(1),
        |v| v["cards"][0]["target"]["kind"] = json!("tabs"),
        |v| {
            v.as_object_mut().unwrap().remove("view");
        },
        |v| {
            v["cards"][0].as_object_mut().unwrap().remove("collapsed");
        },
        |v| v["cards"][0]["width"] = json!("wide"),
    ] {
        assert!(with(edit).is_err());
    }
    // The old tree shape is no longer a layout.
    assert!(serde_json::from_value::<Layout>(json!({"tree":{"kind":"empty"}})).is_err());
    // An empty canvas is valid.
    let empty: Layout = serde_json::from_value(json!({"view":{"x":0,"y":0},"cards":[]})).unwrap();
    assert!(empty.validate().is_ok());
}

#[test]
fn validates_targets_geometry_duplicates_and_card_count_at_the_boundaries() {
    let valid = |layout: &Layout| layout.validate().is_ok();
    let with_card = |edit: fn(&mut Card)| {
        let mut c = card(feed("work"), 0.0);
        edit(&mut c);
        layout(vec![c])
    };
    // Exact limits are accepted.
    assert!(valid(&with_card(|c| c.x = MAX_COORDINATE)));
    assert!(valid(&with_card(|c| c.y = -MAX_COORDINATE)));
    assert!(valid(&with_card(|c| c.width = 1.0)));
    assert!(valid(&with_card(|c| c.height = MAX_CARD_SIZE)));
    for edit in [
        (|c: &mut Card| c.x = MAX_COORDINATE + 1.0) as fn(&mut Card),
        |c| c.y = f64::NAN,
        |c| c.x = f64::INFINITY,
        |c| c.width = 0.5,
        |c| c.width = 0.0,
        |c| c.height = MAX_CARD_SIZE + 1.0,
        |c| c.height = f64::NAN,
        |c| c.target = Target::Board { id: 1 },
        |c| c.target = Target::Board { id: 0 },
        |c| c.target = feed("bad/name"),
    ] {
        assert!(!valid(&with_card(edit)));
    }
    let mut view = leaf();
    view.view.x = f64::NAN;
    assert!(!valid(&view));
    view.view.x = -MAX_COORDINATE - 1.0;
    assert!(!valid(&view));

    // One card per feed or board; a feed and a board never collide.
    assert!(!valid(&layout(vec![
        card(feed("work"), 0.0),
        card(feed("work"), 500.0)
    ])));
    assert!(!valid(&layout(vec![
        card(Target::Board { id: 2 }, 0.0),
        card(Target::Board { id: 2 }, 500.0)
    ])));
    assert!(valid(&layout(vec![
        card(feed("work"), 0.0),
        card(Target::Board { id: 2 }, 500.0)
    ])));

    let many = |n: usize| {
        layout(
            (0..n)
                .map(|i| card(Target::Board { id: i as i64 + 2 }, 0.0))
                .collect(),
        )
    };
    assert!(valid(&many(MAX_CARDS)));
    assert!(!valid(&many(MAX_CARDS + 1)));
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
    let mut collapsed = card(Target::Board { id: board.id }, 440.0);
    collapsed.collapsed = true;
    let arranged = layout(vec![card(feed("work"), 0.0), collapsed]);
    let saved = store.save_layout("Work / day", &arranged).await.unwrap();
    assert_eq!(saved.layout, arranged);
    store.delete_board(board.id, false).await.unwrap();
    assert_eq!(store.list_layouts().await.unwrap(), vec![saved.clone()]);
    let invalid = layout(vec![card(feed("work"), 0.0), card(feed("work"), 1.0)]);
    assert!(store.save_layout("Work / day", &invalid).await.is_err());
    assert_eq!(store.list_layouts().await.unwrap(), vec![saved]);
    let replacement = store
        .save_layout("Work / day", &Layout::default())
        .await
        .unwrap();
    store.save_layout("Another", &arranged).await.unwrap();
    store.close().await;
    let store = Store::open(&path).await.unwrap();
    let layouts = store.list_layouts().await.unwrap();
    assert_eq!(layouts.len(), 2);
    assert_eq!(layouts[0].name, "Another");
    assert_eq!(layouts[0].layout, arranged, "order and geometry persist");
    assert_eq!(layouts[1], replacement);
    // A named layout serializes flat: name, view, cards, updated_at_ms.
    let json = serde_json::to_value(&layouts[0]).unwrap();
    assert_eq!(json["name"], "Another");
    assert_eq!(json["cards"][1]["collapsed"], true);
    assert_eq!(
        serde_json::from_value::<NamedLayout>(json).unwrap(),
        layouts[0]
    );
    assert!(store.list_boards().await.unwrap().is_empty());
    store.close().await;
}

#[tokio::test]
async fn upgrading_drops_saved_trees_and_clears_the_preference() {
    use sqlx::{Connection, sqlite::SqliteConnectOptions};
    let dir = tempfile::tempdir().unwrap();
    let migrations = dir.path().join("migrations");
    std::fs::create_dir(&migrations).unwrap();
    for (name, sql) in [
        (
            "0001_feeds.sql",
            include_str!("../migrations/0001_feeds.sql"),
        ),
        (
            "0002_boards_items.sql",
            include_str!("../migrations/0002_boards_items.sql"),
        ),
        (
            "0003_feed_view_state.sql",
            include_str!("../migrations/0003_feed_view_state.sql"),
        ),
        (
            "0004_note_url.sql",
            include_str!("../migrations/0004_note_url.sql"),
        ),
        (
            "0005_layouts.sql",
            include_str!("../migrations/0005_layouts.sql"),
        ),
        (
            "0006_preferences.sql",
            include_str!("../migrations/0006_preferences.sql"),
        ),
    ] {
        std::fs::write(migrations.join(name), sql).unwrap();
    }
    let path = dir.path().join("before-canvas.sqlite3");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .foreign_keys(true);
    let mut conn = sqlx::SqliteConnection::connect_with(&options)
        .await
        .unwrap();
    sqlx::migrate::Migrator::new(migrations.as_path())
        .await
        .unwrap()
        .run(&mut conn)
        .await
        .unwrap();
    sqlx::raw_sql(
        r#"INSERT INTO layouts(name, tree_json, updated_at_ms)
           VALUES('Day', '{"kind":"feed","name":"reviews"}', 1);
           UPDATE preferences SET last_layout = 'Day';
           INSERT INTO feeds(name,title,last_submitted_at_ms) VALUES('kept','Kept',1);"#,
    )
    .execute(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();

    let store = Store::open(&path).await.unwrap();
    assert!(store.list_layouts().await.unwrap().is_empty());
    assert_eq!(store.preferences().await.unwrap().last_layout, None);
    assert_eq!(store.list_feeds().await.unwrap()[0].name, "kept");
    let saved = store.save_layout("Day", &leaf()).await.unwrap();
    assert_eq!(store.list_layouts().await.unwrap(), vec![saved]);
    store.close().await;
}

#[tokio::test]
async fn rename_delete_and_last_layout_preference() {
    use callboard_core::store::{Change, PreferencesPatch, StoreError};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("layouts.sqlite3");
    let store = Store::open(&path).await.unwrap();
    store.save_layout("Day", &leaf()).await.unwrap();
    store.save_layout("Night", &leaf()).await.unwrap();
    assert_eq!(store.preferences().await.unwrap().last_layout, None);

    let set = |name: Option<&str>| PreferencesPatch {
        last_layout: serde_json::from_value(json!(name)).unwrap(),
    };
    assert!(matches!(
        store.patch_preferences(set(Some("Missing"))).await,
        Err(StoreError::LayoutNotFound(_))
    ));
    assert!(matches!(
        store.patch_preferences(set(Some(" bad"))).await,
        Err(StoreError::Layout(_))
    ));
    let prefs = store.patch_preferences(set(Some("Day"))).await.unwrap();
    assert_eq!(prefs.last_layout.as_deref(), Some("Day"));
    // An empty patch changes nothing.
    let prefs = store
        .patch_preferences(PreferencesPatch::default())
        .await
        .unwrap();
    assert_eq!(prefs.last_layout.as_deref(), Some("Day"));

    let mut changes = store.subscribe();
    assert!(matches!(
        store.rename_layout("Day", "Night").await,
        Err(StoreError::LayoutNameTaken(_))
    ));
    assert!(matches!(
        store.rename_layout("Missing", "Other").await,
        Err(StoreError::LayoutNotFound(_))
    ));
    assert!(store.rename_layout("Day", "").await.is_err());
    assert!(changes.try_recv().is_err(), "failed renames stay silent");
    let same = store.rename_layout("Day", "Day").await.unwrap();
    assert_eq!(same.name, "Day");
    assert!(changes.try_recv().is_err(), "no-op renames stay silent");

    let renamed = store.rename_layout("Day", "Morning").await.unwrap();
    assert_eq!(renamed.name, "Morning");
    assert_eq!(renamed.layout, leaf());
    for name in ["Day", "Morning"] {
        assert_eq!(
            changes.try_recv().unwrap(),
            Change::Layout { name: name.into() }
        );
    }
    let names: Vec<_> = store
        .list_layouts()
        .await
        .unwrap()
        .into_iter()
        .map(|l| l.name)
        .collect();
    assert_eq!(names, ["Morning", "Night"]);
    assert_eq!(
        store.preferences().await.unwrap().last_layout.as_deref(),
        Some("Morning"),
        "the preference follows the rename"
    );

    assert!(!store.delete_layout("Missing").await.unwrap());
    assert!(changes.try_recv().is_err());
    assert!(store.delete_layout("Morning").await.unwrap());
    assert_eq!(
        changes.try_recv().unwrap(),
        Change::Layout {
            name: "Morning".into()
        }
    );
    assert_eq!(store.preferences().await.unwrap().last_layout, None);

    store.patch_preferences(set(Some("Night"))).await.unwrap();
    store.close().await;
    let store = Store::open(&path).await.unwrap();
    assert_eq!(
        store.preferences().await.unwrap().last_layout.as_deref(),
        Some("Night")
    );
    let prefs = store.patch_preferences(set(None)).await.unwrap();
    assert_eq!(prefs.last_layout, None);
    store.close().await;
}
