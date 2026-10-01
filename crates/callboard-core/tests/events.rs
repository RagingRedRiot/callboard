use callboard_core::{feed::parse_submission, layout::Layout, store::*};
use tokio::sync::broadcast::{Receiver, error::TryRecvError};

fn take(rx: &mut Receiver<Change>, expected: Change) {
    assert_eq!(rx.try_recv().unwrap(), expected);
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
}
fn board(id: i64) -> Change {
    Change::Board { id }
}
fn feed() -> Change {
    Change::Feed {
        name: "work".into(),
    }
}

#[tokio::test]
async fn all_mutations_notify_after_success_and_failed_writes_stay_silent() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("events.sqlite3"))
        .await
        .unwrap();
    let mut rx = store.subscribe();
    let snapshot = parse_submission(br#"[{"key":"a","title":"A"}]"#).unwrap();
    store.clone().submit("work", &snapshot).await.unwrap();
    take(&mut rx, feed());
    assert_eq!(store.feed("work").await.unwrap().unwrap().items.len(), 1);
    store.report_error("work", "failed fetch").await.unwrap();
    take(&mut rx, feed());
    store
        .patch_feed_item(
            "work",
            "a",
            FeedItemPatch {
                wake_on_update: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    take(&mut rx, feed());
    let a = store.create_board("A").await.unwrap().id;
    take(&mut rx, board(a));
    let b = store.create_board("B").await.unwrap().id;
    take(&mut rx, board(b));
    store.rename_board(a, "Renamed").await.unwrap();
    take(&mut rx, board(a));
    let todo = store.add_todo(a, "Todo", None, None, None).await.unwrap();
    take(&mut rx, board(a));
    let note = store.add_note(a, None, "Note", None, None).await.unwrap();
    take(&mut rx, board(a));
    store.set_todo_done(todo.id, true).await.unwrap();
    take(&mut rx, board(a));
    store.archive_note(note.id).await.unwrap();
    take(&mut rx, board(a));
    store.restore_note(note.id, a).await.unwrap();
    take(&mut rx, board(a));
    store.move_todo(todo.id, b).await.unwrap();
    assert_eq!(rx.try_recv().unwrap(), board(b));
    take(&mut rx, board(a));
    assert!(
        store
            .patch_note(
                note.id,
                NotePatch {
                    body: Some("rollback".into()),
                    position: Some(500),
                    ..Default::default()
                }
            )
            .await
            .is_err()
    );
    assert!(store.delete_board(a, false).await.is_err());
    assert!(store.report_error("missing", "error").await.is_err());
    assert!(store.create_board("B").await.is_err());
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(store.board_notes(a, false).await.unwrap()[0].body, "Note");
    store.delete_note(note.id).await.unwrap();
    take(&mut rx, board(a));
    store.delete_todo(todo.id).await.unwrap();
    take(&mut rx, board(b));
    store.promote_todo("work", "a", a).await.unwrap();
    take(&mut rx, board(a));
    store.promote_note("work", "a", a).await.unwrap();
    take(&mut rx, board(a));
    store.delete_board(a, true).await.unwrap();
    assert_eq!(rx.try_recv().unwrap(), board(a));
    take(&mut rx, board(1));
    store.save_layout("Day", &Layout::default()).await.unwrap();
    take(&mut rx, Change::Layout { name: "Day".into() });
    store.delete_feed("work").await.unwrap();
    take(&mut rx, feed());
    assert!(!store.delete_feed("work").await.unwrap());
    store.list_layouts().await.unwrap();
    store.list_boards().await.unwrap();
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    store.close().await;
}
