use callboard_core::feed::{ChangeSummary, MAX_SNAPSHOT_BYTES, Snapshot, parse_submission};
use callboard_core::store::{
    FeedItemPatch, NotePatch, SourceReference, Store, StoreError, TodoPatch,
};
use serde_json::{Value, json};
use sqlx::Connection;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};
use tempfile::TempDir;

fn snapshot(value: Value) -> Snapshot {
    parse_submission(&serde_json::to_vec(&value).unwrap()).unwrap()
}

async fn setup() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("callboard.sqlite3"))
        .await
        .unwrap();
    (dir, store)
}

async fn inspect(dir: &TempDir) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(dir.path().join("callboard.sqlite3"))
            .foreign_keys(true),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn migrations_wal_and_data_survive_reopening() {
    let (dir, store) = setup().await;
    let submission = snapshot(json!({
        "title":"Reviews", "source_url":"https://example.com", "stale_after":"1h",
        "items":[{"key":"z", "title":"First", "meta":{"ready":true}}, {"key":"a", "title":"Second"}]
    }));
    let changes = store.submit("reviews", &submission).await.unwrap();
    assert_eq!(changes.added_keys, ["z", "a"]);
    let saved = store.feed("reviews").await.unwrap().unwrap();
    assert_eq!(saved.items, submission.items);
    assert_eq!(saved.info.title, "Reviews");
    assert_eq!(saved.info.source_url, submission.source_url);
    assert_eq!(saved.info.stale_after, submission.stale_after);
    assert!(saved.info.last_submitted_at_ms > 0);
    store.close().await;

    let reopened = Store::open(dir.path().join("callboard.sqlite3"))
        .await
        .unwrap();
    assert_eq!(reopened.feed("reviews").await.unwrap(), Some(saved.clone()));
    assert_eq!(reopened.list_feeds().await.unwrap(), [saved.info]);
    let mut conn = inspect(&dir).await;
    assert_eq!(
        sqlx::query_scalar::<_, String>("PRAGMA journal_mode")
            .fetch_one(&mut conn)
            .await
            .unwrap(),
        "wal"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM _sqlx_migrations WHERE success = 1")
            .fetch_one(&mut conn)
            .await
            .unwrap(),
        5
    );
    conn.close().await.unwrap();
    reopened.close().await;
}

#[tokio::test]
async fn board_items_archive_move_restore_and_deleted_board_archive_survive_reopen() {
    let (dir, store) = setup().await;
    let first = store.create_board("First").await.unwrap();
    let second = store.create_board("Second").await.unwrap();
    let source = SourceReference {
        feed: "news".into(),
        key: "story-1".into(),
    };
    let todo = store
        .add_todo(
            first.id,
            "Review",
            Some("details"),
            Some("https://example.com"),
            Some(&source),
        )
        .await
        .unwrap();
    let note = store
        .add_note(
            first.id,
            Some("Context"),
            "remember this",
            Some("blue"),
            Some(&source),
        )
        .await
        .unwrap();
    store.set_todo_done(todo.id, true).await.unwrap();
    store.archive_todo(todo.id).await.unwrap();
    assert!(store.board_todos(first.id, false).await.unwrap().is_empty());
    assert!(
        store.board_todos(first.id, true).await.unwrap()[0]
            .archived_at_ms
            .is_some()
    );
    store.restore_todo(todo.id, first.id).await.unwrap();
    let moved = store.move_note(note.id, second.id).await.unwrap();
    assert_eq!(moved.board_id, second.id);
    assert_eq!(moved.reference, Some(source.clone()));

    // Source references are descriptive snapshots; source removal does not
    // cascade into the user's promoted object.
    store
        .submit(
            "news",
            &snapshot(json!([{"key":"story-1","title":"Story"}])),
        )
        .await
        .unwrap();
    store.delete_feed("news").await.unwrap();
    assert_eq!(
        store.board_todos(first.id, true).await.unwrap()[0].reference,
        Some(source.clone())
    );

    assert!(matches!(
        store.delete_board(first.id, false).await,
        Err(StoreError::BoardNotEmpty(_))
    ));
    store.delete_board(first.id, true).await.unwrap();
    let archived = store.archived_todos().await.unwrap();
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].id, todo.id);
    assert_eq!(archived[0].archived_from_board.as_deref(), Some("First"));
    store.close().await;

    let reopened = Store::open(dir.path().join("callboard.sqlite3"))
        .await
        .unwrap();
    assert_eq!(reopened.archived_todos().await.unwrap(), archived);
    let restored = reopened.restore_todo(todo.id, second.id).await.unwrap();
    assert_eq!(restored.board_id, second.id);
    assert!(restored.archived_at_ms.is_none());
    assert_eq!(
        reopened.board_notes(second.id, false).await.unwrap(),
        [moved]
    );
    reopened.close().await;
}

#[tokio::test]
async fn board_names_are_unique_and_empty_board_deletion_is_supported() {
    let (_dir, store) = setup().await;
    let board = store.create_board("Work").await.unwrap();
    assert!(matches!(
        store.create_board("Work").await,
        Err(StoreError::BoardNameTaken)
    ));
    assert!(matches!(store.rename_board(board.id, "Work").await, Ok(())));
    assert!(matches!(
        store.rename_board(board.id, " ").await,
        Err(StoreError::EmptyBoardName)
    ));
    store.delete_board(board.id, false).await.unwrap();
    assert!(store.list_boards().await.unwrap().is_empty());
    store.close().await;
}

#[tokio::test]
async fn todo_and_note_patches_edit_order_move_archive_and_clear_optional_fields() {
    let (_dir, store) = setup().await;
    let source = store.create_board("Source").await.unwrap();
    let destination = store.create_board("Destination").await.unwrap();
    let first = store
        .add_todo(
            source.id,
            "First",
            Some("body"),
            Some("https://example.com"),
            None,
        )
        .await
        .unwrap();
    let second = store
        .add_todo(source.id, "Second", None, None, None)
        .await
        .unwrap();
    let third = store
        .add_todo(source.id, "Third", None, None, None)
        .await
        .unwrap();
    let edited = store
        .patch_todo(
            first.id,
            serde_json::from_value::<TodoPatch>(json!({
                "title":"Updated", "body":null, "url":null, "done":true, "position":2
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(edited.title, "Updated");
    assert!(edited.body.is_none() && edited.url.is_none());
    assert!(edited.done);
    assert_eq!(
        store
            .board_todos(source.id, false)
            .await
            .unwrap()
            .iter()
            .map(|t| t.id)
            .collect::<Vec<_>>(),
        [second.id, third.id, first.id]
    );

    let moved = store
        .patch_todo(
            first.id,
            TodoPatch {
                board_id: Some(destination.id),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(moved.board_id, destination.id);
    let archived = store
        .patch_todo(
            first.id,
            TodoPatch {
                archived: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(archived.archived_at_ms.is_some());
    let restored = store
        .patch_todo(
            first.id,
            TodoPatch {
                archived: Some(false),
                board_id: Some(source.id),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(restored.board_id, source.id);
    assert!(restored.archived_at_ms.is_none());

    let note = store
        .add_note(source.id, Some("Memo"), "old", Some("yellow"), None)
        .await
        .unwrap();
    let patched = store
        .patch_note(
            note.id,
            serde_json::from_value::<NotePatch>(json!({
                "title":null, "body":"new", "color":null, "position":0
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(patched.title.is_none() && patched.color.is_none());
    assert_eq!(patched.body, "new");
    assert!(matches!(
        store
            .patch_note(
                note.id,
                NotePatch {
                    position: Some(2),
                    ..Default::default()
                }
            )
            .await,
        Err(StoreError::InvalidPosition(2))
    ));
    assert_eq!(
        store.board_notes(source.id, false).await.unwrap()[0].body,
        "new"
    );
    store.close().await;
}

#[tokio::test]
async fn feed_view_state_manual_order_and_snapshot_wake_rules_persist() {
    let (dir, store) = setup().await;
    let original = snapshot(json!([
        {"key":"a","title":"A"}, {"key":"b","title":"B"}, {"key":"c","title":"C"}
    ]));
    store.submit("work", &original).await.unwrap();
    let reorder = FeedItemPatch {
        position: Some(0),
        snoozed_until_ms: serde_json::from_value(json!(i64::MAX)).unwrap(),
        wake_on_update: Some(true),
        ..Default::default()
    };
    let state = store.patch_feed_item("work", "b", reorder).await.unwrap();
    assert!(state.manual_order && state.state.snoozed);
    assert_eq!(state.position, 0);
    let timed = FeedItemPatch {
        snoozed_until_ms: serde_json::from_value(json!(i64::MAX)).unwrap(),
        ..Default::default()
    };
    store.patch_feed_item("work", "a", timed).await.unwrap();
    let before = store.feed("work").await.unwrap().unwrap();
    assert!(before.manual_order);
    assert_eq!(
        before
            .items
            .iter()
            .map(|item| item.key.as_str())
            .collect::<Vec<_>>(),
        ["b", "a", "c"]
    );
    assert_eq!(before.view_state.len(), 2);
    store.close().await;

    let reopened = Store::open(dir.path().join("callboard.sqlite3"))
        .await
        .unwrap();
    let persisted = reopened.feed("work").await.unwrap().unwrap();
    assert_eq!(persisted.items, before.items);
    assert_eq!(persisted.view_state, before.view_state);

    let add_and_update = snapshot(json!([
        {"key":"c","title":"C"}, {"key":"d","title":"D"},
        {"key":"b","title":"B changed"}, {"key":"a","title":"A"}
    ]));
    let changes = reopened.submit("work", &add_and_update).await.unwrap();
    assert_eq!(changes.added_keys, ["d"]);
    let after = reopened.feed("work").await.unwrap().unwrap();
    assert_eq!(
        after
            .items
            .iter()
            .map(|item| item.key.as_str())
            .collect::<Vec<_>>(),
        ["d", "b", "a", "c"]
    );
    assert!(
        !after.view_state.contains_key("b"),
        "a content update wakes wake-on-update snoozes"
    );
    assert!(
        after.view_state["a"].snoozed,
        "time snooze survives unrelated updates"
    );

    let remove_a = snapshot(json!([
        {"key":"c","title":"C"}, {"key":"d","title":"D"}, {"key":"b","title":"B changed"}
    ]));
    reopened.submit("work", &remove_a).await.unwrap();
    let pruned = reopened.feed("work").await.unwrap().unwrap();
    assert!(!pruned.view_state.contains_key("a"));
    assert_eq!(
        pruned
            .items
            .iter()
            .map(|item| item.key.as_str())
            .collect::<Vec<_>>(),
        ["d", "b", "c"]
    );
    reopened
        .patch_feed_item(
            "work",
            "b",
            FeedItemPatch {
                reset_order: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let reset = reopened.feed("work").await.unwrap().unwrap();
    assert!(!reset.manual_order);
    assert_eq!(
        reset
            .items
            .iter()
            .map(|item| item.key.as_str())
            .collect::<Vec<_>>(),
        ["c", "d", "b"]
    );
    reopened.close().await;
}

#[tokio::test]
async fn promotion_copies_current_source_and_keeps_objects_after_source_removal() {
    let (_dir, store) = setup().await;
    let board = store.create_board("Inbox").await.unwrap();
    store
        .submit(
            "alerts",
            &snapshot(json!([{
                "key":"a-1", "title":"Original", "url":"https://example.com/old", "body":"Details"
            }])),
        )
        .await
        .unwrap();
    let todo = store.promote_todo("alerts", "a-1", board.id).await.unwrap();
    let note = store.promote_note("alerts", "a-1", board.id).await.unwrap();
    assert_eq!(todo.title, "Original");
    assert_eq!(todo.body.as_deref(), Some("Details"));
    assert_eq!(todo.url.as_deref(), Some("https://example.com/old"));
    assert_eq!(todo.reference.as_ref().unwrap().feed, "alerts");
    assert_eq!(note.title.as_deref(), Some("Original"));
    assert_eq!(note.body, "Details");
    assert_eq!(note.url.as_deref(), Some("https://example.com/old"));
    assert_eq!(note.reference.as_ref().unwrap().key, "a-1");

    store
        .submit(
            "alerts",
            &snapshot(json!([{
                "key":"a-1", "title":"Current", "url":"https://example.com/new"
            }])),
        )
        .await
        .unwrap();
    let current = store.promote_todo("alerts", "a-1", board.id).await.unwrap();
    assert_eq!(current.title, "Current");
    assert_eq!(current.url.as_deref(), Some("https://example.com/new"));

    store.submit("alerts", &snapshot(json!([]))).await.unwrap();
    assert!(matches!(
        store.promote_todo("alerts", "a-1", board.id).await,
        Err(StoreError::FeedItemNotFound { .. })
    ));
    assert_eq!(store.board_todos(board.id, false).await.unwrap()[0], todo);
    assert_eq!(store.board_notes(board.id, false).await.unwrap()[0], note);
    store.close().await;
}

#[tokio::test]
async fn summaries_and_order_follow_complete_snapshots() {
    let (_dir, store) = setup().await;
    store
        .submit(
            "work",
            &snapshot(json!([
                {"key":"gone-z", "title":"Gone"}, {"key":"keep", "title":"Keep"},
                {"key":"gone-a", "title":"Gone"}, {"key":"update", "title":"Before"}
            ])),
        )
        .await
        .unwrap();
    let next = snapshot(json!([
        {"key":"new-z", "title":"New"}, {"key":"update", "title":"After"},
        {"key":"keep", "title":"Keep"}, {"key":"new-a", "title":"New"}
    ]));
    let changes = store.submit("work", &next).await.unwrap();
    assert_eq!(
        changes,
        ChangeSummary {
            added: 2,
            removed: 2,
            updated: 1,
            unchanged: 1,
            added_keys: vec!["new-z".into(), "new-a".into()],
            removed_keys: vec!["gone-z".into(), "gone-a".into()],
            updated_keys: vec!["update".into()],
        }
    );
    assert_eq!(store.feed("work").await.unwrap().unwrap().items, next.items);
    assert_eq!(
        store.submit("work", &next).await.unwrap(),
        ChangeSummary {
            unchanged: 4,
            ..Default::default()
        }
    );
    store.close().await;
}

#[tokio::test]
async fn normalization_and_reordering_do_not_mark_content_updated() {
    let (dir, store) = setup().await;
    let first = parse_submission(
        br#"[
        {"key":"b", "title":"B", "meta":{"z":true, "a":2}},
        {"key":"a", "title":"A"}
    ]"#,
    )
    .unwrap();
    store.submit("work", &first).await.unwrap();
    let mut conn = inspect(&dir).await;
    let before =
        sqlx::query_as::<_, (String, i64)>("SELECT key, rowid FROM feed_items ORDER BY key")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    let reordered = parse_submission(
        br#"[
        {"title":"A", "key":"a", "url":null, "body":null, "tags":[], "meta":{}},
        {"title":"B", "key":"b", "meta":{"a":2, "z":true}}
    ]"#,
    )
    .unwrap();
    assert_eq!(
        store.submit("work", &reordered).await.unwrap(),
        ChangeSummary {
            unchanged: 2,
            ..Default::default()
        }
    );
    assert_eq!(
        store.feed("work").await.unwrap().unwrap().items,
        reordered.items
    );
    let after =
        sqlx::query_as::<_, (String, i64)>("SELECT key, rowid FROM feed_items ORDER BY key")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    assert_eq!(before, after, "retained keys must keep their rows");
    conn.close().await.unwrap();
    store.close().await;
}

#[tokio::test]
async fn all_content_fields_participate_in_updates() {
    let (_dir, store) = setup().await;
    let mut value = json!({"key":"a", "title":"A"});
    store
        .submit("work", &snapshot(json!([value.clone()])))
        .await
        .unwrap();
    for (field, replacement) in [
        ("title", json!("Changed")),
        ("body", json!("Details")),
        ("url", json!("https://example.com")),
        ("tags", json!(["review"])),
        ("meta", json!({"ready":true})),
    ] {
        value[field] = replacement;
        let changes = store
            .submit("work", &snapshot(json!([value.clone()])))
            .await
            .unwrap();
        assert_eq!(
            changes,
            ChangeSummary {
                updated: 1,
                updated_keys: vec!["a".into()],
                ..Default::default()
            },
            "{field}"
        );
    }
    store.close().await;
}

#[tokio::test]
async fn failures_preserve_items_and_identical_submissions_refresh_status() {
    let (dir, store) = setup().await;
    let submission = snapshot(json!([{"key":"a", "title":"A"}]));
    store.submit("work", &submission).await.unwrap();
    // Set a known older timestamp, avoiding sleeps and clock-resolution races.
    let mut conn = inspect(&dir).await;
    sqlx::query("UPDATE feeds SET last_submitted_at_ms = 1 WHERE name = 'work'")
        .execute(&mut conn)
        .await
        .unwrap();
    store.report_error("work", "upstream failed").await.unwrap();
    let failed = store.feed("work").await.unwrap().unwrap();
    assert_eq!(failed.items, submission.items);
    assert_eq!(failed.info.last_submitted_at_ms, 1);
    let error = failed.info.error.unwrap();
    assert_eq!(error.message, "upstream failed");
    assert!(error.at_ms > 1);
    store.report_error("work", "still failing").await.unwrap();
    assert_eq!(
        store
            .feed("work")
            .await
            .unwrap()
            .unwrap()
            .info
            .error
            .unwrap()
            .message,
        "still failing"
    );
    assert_eq!(
        store.submit("work", &submission).await.unwrap().unchanged,
        1
    );
    let refreshed = store.feed("work").await.unwrap().unwrap();
    assert!(refreshed.info.error.is_none());
    assert!(refreshed.info.last_submitted_at_ms > 1);
    assert_eq!(refreshed.items, submission.items);
    conn.close().await.unwrap();
    store.close().await;
}

#[tokio::test]
async fn metadata_replaces_and_defaults_without_content_updates() {
    let (_dir, store) = setup().await;
    store.submit("work", &snapshot(json!({"title":"Title", "source_url":"https://example.com", "stale_after":"1h", "items":[]}))).await.unwrap();
    assert_eq!(
        store.submit("work", &snapshot(json!([]))).await.unwrap(),
        ChangeSummary::default()
    );
    let info = store.feed("work").await.unwrap().unwrap().info;
    assert_eq!(info.title, "work");
    assert_eq!(info.source_url, None);
    assert_eq!(info.stale_after, None);
    store.close().await;
}

#[tokio::test]
async fn clearing_deleting_and_reappearing_keys_are_new() {
    let (dir, store) = setup().await;
    let one = snapshot(json!([{"key":"a", "title":"A"}]));
    for name in ["work", "other"] {
        store.submit(name, &one).await.unwrap();
    }
    let empty = store.submit("work", &snapshot(json!([]))).await.unwrap();
    assert_eq!(empty.removed_keys, ["a"]);
    assert_eq!(empty.removed, 1);
    assert!(store.feed("work").await.unwrap().unwrap().items.is_empty());
    assert_eq!(store.submit("work", &one).await.unwrap().added, 1);
    assert!(store.delete_feed("work").await.unwrap());
    assert!(!store.delete_feed("work").await.unwrap());
    assert!(store.feed("work").await.unwrap().is_none());
    let mut conn = inspect(&dir).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM feed_items WHERE feed = 'work'")
            .fetch_one(&mut conn)
            .await
            .unwrap(),
        0
    );
    assert_eq!(store.feed("other").await.unwrap().unwrap().items, one.items);
    assert_eq!(store.submit("work", &one).await.unwrap().added_keys, ["a"]);
    assert_eq!(
        store
            .list_feeds()
            .await
            .unwrap()
            .iter()
            .map(|info| info.name.as_str())
            .collect::<Vec<_>>(),
        ["other", "work"]
    );
    conn.close().await.unwrap();
    store.close().await;
}

#[tokio::test]
async fn invalid_submissions_leave_existing_state_untouched() {
    let (_dir, store) = setup().await;
    let valid = snapshot(json!([{"key":"a", "title":"A"}]));
    store.submit("work", &valid).await.unwrap();
    store.report_error("work", "fetch failed").await.unwrap();
    let before = store.feed("work").await.unwrap();
    let mut duplicate = valid.clone();
    duplicate.items.push(duplicate.items[0].clone());
    let mut too_large = valid.clone();
    too_large.items[0].url = Some("x".repeat(MAX_SNAPSHOT_BYTES));
    let mut bad_title = valid.clone();
    bad_title.items[0].title = "x".repeat(501);
    for invalid in [duplicate, too_large, bad_title] {
        assert!(store.submit("work", &invalid).await.is_err());
        assert!(store.submit("new", &invalid).await.is_err());
        assert_eq!(store.feed("work").await.unwrap(), before);
        assert!(store.feed("new").await.unwrap().is_none());
    }
    assert!(matches!(
        store.report_error("unknown", "failed").await,
        Err(StoreError::FeedNotFound(_))
    ));
    assert!(store.feed("unknown").await.unwrap().is_none());
    assert!(store.submit("BAD", &valid).await.is_err());
    assert!(store.feed("BAD").await.is_err());
    assert!(store.report_error("BAD", "failed").await.is_err());
    assert!(store.delete_feed("BAD").await.is_err());
    store.close().await;
}

#[tokio::test]
async fn database_failure_rolls_back_partial_item_and_metadata_writes() {
    let (dir, store) = setup().await;
    store
        .submit(
            "work",
            &snapshot(json!({"title":"Original", "items":[{"key":"old", "title":"Old"}]})),
        )
        .await
        .unwrap();
    store
        .report_error("work", "preserve this error")
        .await
        .unwrap();
    let before = store.feed("work").await.unwrap();
    let mut conn = inspect(&dir).await;
    sqlx::query("CREATE TRIGGER fail_item BEFORE INSERT ON feed_items WHEN NEW.key = 'fail' BEGIN SELECT RAISE(ABORT, 'injected failure'); END")
        .execute(&mut conn).await.unwrap();
    let next = snapshot(json!({"title":"Changed", "items":[
        {"key":"first", "title":"Inserted before failure"}, {"key":"fail", "title":"Abort"}
    ]}));
    assert!(matches!(
        store.submit("work", &next).await,
        Err(StoreError::Database(_))
    ));
    assert_eq!(store.feed("work").await.unwrap(), before);
    assert!(store.submit("new", &next).await.is_err());
    assert!(store.feed("new").await.unwrap().is_none());
    sqlx::query("DROP TRIGGER fail_item")
        .execute(&mut conn)
        .await
        .unwrap();
    assert_eq!(store.submit("work", &next).await.unwrap().added, 2);
    assert_eq!(store.feed("work").await.unwrap().unwrap().items, next.items);
    conn.close().await.unwrap();
    store.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_writers_compare_against_last_committed_snapshot() {
    let (dir, first) = setup().await;
    let second = Store::open(dir.path().join("callboard.sqlite3"))
        .await
        .unwrap();
    let submission = snapshot(json!([{"key":"shared", "title":"Shared"}]));
    let (a, b) = tokio::join!(
        first.submit("work", &submission),
        second.submit("work", &submission)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.added + b.added, 1);
    assert_eq!(a.unchanged + b.unchanged, 1);
    let left = snapshot(json!([{"key":"left", "title":"Left"}]));
    let right = snapshot(json!([{"key":"right", "title":"Right"}]));
    let (a, b) = tokio::join!(first.submit("work", &left), second.submit("work", &right));
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!((a.added, a.removed, b.added, b.removed), (1, 1, 1, 1));
    let final_items = first.feed("work").await.unwrap().unwrap().items;
    if a.removed_keys == ["shared"] {
        assert_eq!(b.removed_keys, ["left"]);
        assert_eq!(final_items, right.items);
    } else {
        assert_eq!(b.removed_keys, ["shared"]);
        assert_eq!(a.removed_keys, ["right"]);
        assert_eq!(final_items, left.items);
    }
    first.close().await;
    second.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readers_never_mix_metadata_and_items_from_different_snapshots() {
    let (_dir, store) = setup().await;
    let make = |n| {
        snapshot(json!({"title":format!("v{n}"), "items":[{"key":"a", "title":format!("v{n}")}]}))
    };
    store.submit("work", &make(0)).await.unwrap();
    let writer = async {
        for n in 1..=30 {
            store.submit("work", &make(n)).await.unwrap();
        }
    };
    let reader = async {
        for _ in 0..60 {
            let feed = store.feed("work").await.unwrap().unwrap();
            assert_eq!(feed.items.len(), 1);
            assert_eq!(feed.info.title, feed.items[0].title);
        }
    };
    tokio::join!(writer, reader);
    store.close().await;
}
