//! Colors on todos, notes, and boards, set from the GUI (DESIGN.md §5, §6.3).
use callboard_core::store::{BoardPatch, NotePatch, Store, StoreError, TodoPatch};
use serde_json::json;

fn invalid(result: Result<impl std::fmt::Debug, StoreError>) -> bool {
    matches!(result, Err(StoreError::Validation(e)) if e.to_string().starts_with("color must be"))
}

#[tokio::test]
async fn todos_notes_and_boards_store_and_validate_colors() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db.sqlite3")).await.unwrap();
    let board = store.create_board("Home").await.unwrap();
    assert_eq!(board.color, None);

    // Todos: on creation and by patch; null clears.
    let todo = store
        .add_todo_with(board.id, "Paint", None, None, Some("orange"), None)
        .await
        .unwrap();
    assert_eq!(todo.color.as_deref(), Some("orange"));
    let patch: TodoPatch = serde_json::from_value(json!({"color": null})).unwrap();
    assert_eq!(store.patch_todo(todo.id, patch).await.unwrap().color, None);
    assert!(invalid(
        store
            .add_todo_with(board.id, "Bad", None, None, Some("teal"), None)
            .await
    ));
    let patch: TodoPatch = serde_json::from_value(json!({"color": "teal"})).unwrap();
    assert!(invalid(store.patch_todo(todo.id, patch).await));

    // Notes now share the palette.
    let note = store
        .add_note(board.id, None, "Idea", Some("gray"), None)
        .await
        .unwrap();
    assert_eq!(note.color.as_deref(), Some("gray"));
    assert!(invalid(
        store
            .add_note(board.id, None, "Idea", Some("beige"), None)
            .await
    ));
    let patch: NotePatch = serde_json::from_value(json!({"color": "beige"})).unwrap();
    assert!(invalid(store.patch_note(note.id, patch).await));

    // Boards: color alone, name alone, or both; list and contents carry it.
    let patch: BoardPatch = serde_json::from_value(json!({"color": "green"})).unwrap();
    let board = store.patch_board(board.id, patch).await.unwrap();
    assert_eq!(
        (board.name.as_str(), board.color.as_deref()),
        ("Home", Some("green"))
    );
    store.rename_board(board.id, " House ").await.unwrap();
    let summary = &store.board_summaries().await.unwrap()[0];
    assert_eq!(summary.info.name, "House");
    assert_eq!(summary.info.color.as_deref(), Some("green"));
    let contents = store.board_contents(Some(board.id), false).await.unwrap();
    assert_eq!(contents.board.unwrap().color.as_deref(), Some("green"));
    let patch: BoardPatch = serde_json::from_value(json!({"color": "teal"})).unwrap();
    assert!(invalid(store.patch_board(board.id, patch).await));
    let patch: BoardPatch = serde_json::from_value(json!({"color": null})).unwrap();
    assert_eq!(
        store.patch_board(board.id, patch).await.unwrap().color,
        None
    );
    assert!(serde_json::from_value::<BoardPatch>(json!({"colour": "red"})).is_err());
    store.close().await;
}
