use callboard_core::{feed::parse_submission, store::*};
use serde_json::json;

async fn setup() -> (tempfile::TempDir, Store, i64) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("audit.sqlite3")).await.unwrap();
    let board = store.create_board("Audit").await.unwrap().id;
    (dir, store, board)
}

#[tokio::test]
async fn deadline_ends_combined_snooze_and_clearing_it_does_not_rearm_update_snooze() {
    let (_dir, store, _) = setup().await;
    store
        .submit(
            "work",
            &parse_submission(br#"[{"key":"a","title":"A"}]"#).unwrap(),
        )
        .await
        .unwrap();
    let result = store
        .patch_feed_item(
            "work",
            "a",
            serde_json::from_value(json!({
                "snoozed_until_ms": 1, "wake_on_update": true
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        !result.state.snoozed,
        "time must win even when wake_on_update is set"
    );
    assert!(!store.feed("work").await.unwrap().unwrap().view_state["a"].snoozed);
    let cleared = store
        .patch_feed_item(
            "work",
            "a",
            serde_json::from_value(json!({
                "snoozed_until_ms": null
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        !cleared.state.snoozed,
        "expired snoozes must not be resurrected"
    );
    store.close().await;
}

#[tokio::test]
async fn snooze_only_patch_reports_manual_position() {
    let (_dir, store, _) = setup().await;
    store
        .submit(
            "work",
            &parse_submission(br#"[{"key":"a","title":"A"},{"key":"b","title":"B"}]"#).unwrap(),
        )
        .await
        .unwrap();
    store
        .patch_feed_item(
            "work",
            "b",
            FeedItemPatch {
                position: Some(0),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let result = store
        .patch_feed_item(
            "work",
            "b",
            FeedItemPatch {
                wake_on_update: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(result.manual_order);
    assert_eq!(result.position, 0);
    store.close().await;
}

#[tokio::test]
async fn invalid_reordering_rejects_entire_patch_including_archived_items() {
    let (_dir, store, board) = setup().await;
    let todo = store
        .add_todo(board, "Keep", None, None, None)
        .await
        .unwrap();
    let err = store
        .patch_todo(
            todo.id,
            TodoPatch {
                title: Some("Should roll back".into()),
                position: Some(-1),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidPosition(-1)), "{err}");
    assert_eq!(store.board_todos(board, false).await.unwrap()[0], todo);
    store.archive_todo(todo.id).await.unwrap();
    assert!(
        store
            .patch_todo(
                todo.id,
                TodoPatch {
                    title: Some("Should roll back".into()),
                    position: Some(0),
                    ..Default::default()
                }
            )
            .await
            .is_err()
    );
    assert_eq!(
        store.board_todos(board, true).await.unwrap()[0].title,
        "Keep"
    );
    store.close().await;
}

#[tokio::test]
async fn board_content_limits_apply_to_create_and_patch() {
    let (_dir, store, board) = setup().await;
    assert!(
        store
            .add_todo(board, &"a".repeat(501), None, None, None)
            .await
            .is_err()
    );
    assert!(
        store
            .add_note(board, None, &"x".repeat(16385), None, None)
            .await
            .is_err()
    );
    let note = store
        .add_note(board, Some("Keep"), "Body", None, None)
        .await
        .unwrap();
    assert!(
        store
            .patch_note(
                note.id,
                NotePatch {
                    body: Some("x".repeat(16385)),
                    ..Default::default()
                }
            )
            .await
            .is_err()
    );
    assert_eq!(store.board_notes(board, false).await.unwrap()[0], note);
    store.close().await;
}

#[tokio::test]
async fn moving_archived_items_preserves_archive_until_explicit_restore() {
    let (_dir, store, board) = setup().await;
    let target = store.create_board("Target").await.unwrap().id;
    let todo = store
        .add_todo(board, "Todo", None, None, None)
        .await
        .unwrap();
    let archived = store
        .patch_todo(
            todo.id,
            TodoPatch {
                board_id: Some(target),
                archived: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(archived.board_id, target);
    assert!(archived.archived_at_ms.is_some());
    let moved = store.move_todo(todo.id, board).await.unwrap();
    assert_eq!(moved.archived_at_ms, archived.archived_at_ms);
    assert!(
        store
            .board_contents(Some(board), false)
            .await
            .unwrap()
            .todos
            .is_empty()
    );
    assert_eq!(
        store.board_contents(Some(board), true).await.unwrap().todos[0].item,
        moved
    );
    let restored = store.restore_todo(todo.id, target).await.unwrap();
    assert!(restored.archived_at_ms.is_none());
    assert_eq!(
        store
            .board_contents(Some(target), false)
            .await
            .unwrap()
            .todos[0]
            .item,
        restored
    );
    assert!(matches!(
        store.board_contents(Some(1), true).await,
        Err(StoreError::BoardNotFound(1))
    ));
    store.close().await;
}

#[tokio::test]
async fn upgrades_preserve_existing_feed_and_note_data() {
    use sqlx::{Connection, sqlite::SqliteConnectOptions};
    for version in [1, 3] {
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
        ]
        .into_iter()
        .take(version)
        {
            std::fs::write(migrations.join(name), sql).unwrap();
        }
        let path = dir.path().join("legacy.sqlite3");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let mut conn = sqlx::SqliteConnection::connect_with(&options)
            .await
            .unwrap();
        sqlx::migrate::Migrator::new(migrations.as_path())
            .await
            .unwrap()
            .run(&mut conn)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO feeds(name,title,last_submitted_at_ms) VALUES('legacy','Legacy',1)",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        sqlx::query("INSERT INTO feed_items VALUES('legacy','key',0,?,?)")
            .bind(r#"{"key":"key","title":"Preserved"}"#)
            .bind(vec![0u8; 32])
            .execute(&mut conn)
            .await
            .unwrap();
        if version == 3 {
            sqlx::raw_sql("INSERT INTO boards(id,name) VALUES(2,'Legacy'); INSERT INTO notes(board_id,body,position,created_at_ms,updated_at_ms) VALUES(2,'Preserved note',0,1,1)")
                .execute(&mut conn).await.unwrap();
        }
        conn.close().await.unwrap();
        let store = Store::open(&path).await.unwrap();
        assert_eq!(
            store.feed("legacy").await.unwrap().unwrap().items[0].title,
            "Preserved"
        );
        if version == 3 {
            let notes = store.board_notes(2, false).await.unwrap();
            assert_eq!(notes[0].body, "Preserved note");
            assert_eq!(notes[0].url, None);
        }
        store.close().await;
    }
}
