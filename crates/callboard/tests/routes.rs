#![cfg(target_os = "linux")]
//! The API contract (DESIGN.md §8.1). Every route that exists today is
//! listed here with the shape of its response, so changing an existing
//! route's path, method, status, or response fields fails this test, and
//! fixing it means editing a reviewed file. New routes are free to add: the
//! table may grow, and new route files bring their own tests.
use callboard_service::{
    client,
    lifecycle::{Environment, Paths},
    server,
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, os::unix::fs::PermissionsExt, time::Duration};

/// Routes that must exist unchanged (method, pattern).
const FROZEN: &[(&str, &str)] = &[
    ("GET", "/feeds"),
    ("GET", "/feeds/{feed}"),
    ("PUT", "/feeds/{feed}"),
    ("DELETE", "/feeds/{feed}"),
    ("POST", "/feeds/{feed}/error"),
    ("PATCH", "/feeds/{feed}/items/{key}"),
    ("POST", "/feeds/{feed}/items/{key}/promote"),
    ("GET", "/archive"),
    ("GET", "/boards"),
    ("POST", "/boards"),
    ("GET", "/boards/{id}"),
    ("PATCH", "/boards/{id}"),
    ("DELETE", "/boards/{id}"),
    ("GET", "/boards/{id}/archive"),
    ("POST", "/boards/{id}/todos"),
    ("POST", "/boards/{id}/notes"),
    ("PATCH", "/todos/{id}"),
    ("DELETE", "/todos/{id}"),
    ("POST", "/todos/{id}/archive"),
    ("POST", "/todos/{id}/restore"),
    ("POST", "/todos/{id}/move"),
    ("PATCH", "/notes/{id}"),
    ("DELETE", "/notes/{id}"),
    ("POST", "/notes/{id}/archive"),
    ("POST", "/notes/{id}/restore"),
    ("POST", "/notes/{id}/move"),
    ("GET", "/layouts"),
    ("PUT", "/layouts/{name}"),
    ("PATCH", "/layouts/{name}"),
    ("DELETE", "/layouts/{name}"),
    ("GET", "/preferences"),
    ("PATCH", "/preferences"),
];

#[test]
fn every_frozen_route_is_still_registered() {
    let table: BTreeSet<_> = callboard_service::api::list()
        .unwrap()
        .into_iter()
        .collect();
    for route in FROZEN {
        assert!(
            table.contains(route),
            "route {route:?} was removed or changed"
        );
    }
}

/// The top-level shape of a JSON value: object keys, and for arrays the
/// keys of their first element. Field types are pinned by the store tests.
fn shape(value: &Value) -> String {
    match value {
        Value::Object(map) => format!("{{{}}}", map.keys().cloned().collect::<Vec<_>>().join(",")),
        Value::Array(items) => match items.first() {
            Some(first) => format!("[{}]", shape(first)),
            None => "[]".into(),
        },
        other => other.to_string(),
    }
}

struct Api {
    paths: Paths,
    seen: BTreeSet<(&'static str, &'static str)>,
}

impl Api {
    /// Call `route` at `path`, check the status and response shape, and
    /// return the response.
    async fn call(
        &mut self,
        route: (&'static str, &'static str),
        path: &str,
        body: Option<Value>,
        status: u16,
        expected: &str,
    ) -> Value {
        let body = body
            .map(|b| serde_json::to_vec(&b).unwrap())
            .unwrap_or_default();
        let (got, bytes) = client::request(&self.paths, route.0, path, body, None)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(got.as_u16(), status, "{} {path}: {value}", route.0);
        assert_eq!(shape(&value), expected, "{} {path} changed shape", route.0);
        self.seen.insert(route);
        value
    }
}

#[tokio::test]
async fn every_frozen_route_keeps_its_status_and_response_shape() {
    let root = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let paths = Paths::resolve(&Environment {
        data_home: Some(root.path().join("data")),
        config_home: Some(root.path().join("config")),
        socket_dir: Some(root.path().join("run")),
        ..Default::default()
    })
    .unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let service = tokio::spawn(server::serve(
        paths.clone(),
        server::Start::default(),
        async {
            let _ = stopped.await;
        },
    ));
    for _ in 0..200 {
        if client::request(&paths, "GET", "/health", vec![], None)
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut api = Api {
        paths,
        seen: BTreeSet::new(),
    };

    // Feeds.
    let items = json!({"items": [
        {"key": "a/b", "title": "First"},
        {"key": "c", "title": "Second"}
    ]});
    api.call(
        ("PUT", "/feeds/{feed}"),
        "/feeds/reviews",
        Some(items),
        200,
        "{added,added_keys,removed,removed_keys,unchanged,updated,updated_keys}",
    )
    .await;
    api.call(("GET", "/feeds"), "/feeds", None, 200,
        "[{description,error,item_count,last_change,last_submitted_at_ms,name,new_count,new_for,next_wake_at_ms,snoozed_count,source_url,stale_after,title}]").await;
    api.call(
        ("GET", "/feeds/{feed}"),
        "/feeds/reviews",
        None,
        200,
        "{changes,info,items,manual_order,view_state}",
    )
    .await;
    api.call(
        ("POST", "/feeds/{feed}/error"),
        "/feeds/reviews/error",
        Some(json!({"message": "rate limited"})),
        200,
        "{recorded}",
    )
    .await;
    api.call(
        ("PATCH", "/feeds/{feed}/items/{key}"),
        "/feeds/reviews/items/a%2Fb",
        Some(json!({"position": 1})),
        200,
        "{manual_order,position,state}",
    )
    .await;

    // Boards and promotion.
    let board = api
        .call(
            ("POST", "/boards"),
            "/boards",
            Some(json!({"name": "Inbox"})),
            201,
            "{color,id,name}",
        )
        .await;
    let id = board["id"].as_i64().unwrap();
    let other = api
        .call(
            ("POST", "/boards"),
            "/boards",
            Some(json!({"name": "Other"})),
            201,
            "{color,id,name}",
        )
        .await["id"]
        .as_i64()
        .unwrap();
    api.call(
        ("GET", "/boards"),
        "/boards",
        None,
        200,
        "[{color,id,name,note_count,open_todo_count,todo_count}]",
    )
    .await;
    api.call(("POST", "/feeds/{feed}/items/{key}/promote"), "/feeds/reviews/items/a%2Fb/promote",
        Some(json!({"board_id": id, "kind": "todo"})), 201,
        "{archived_at_ms,archived_from_board,board_id,body,color,created_at_ms,done,id,position,reference,title,updated_at_ms,url}").await;
    api.call(("POST", "/feeds/{feed}/items/{key}/promote"), "/feeds/reviews/items/c/promote",
        Some(json!({"board_id": id, "kind": "note"})), 201,
        "{archived_at_ms,archived_from_board,board_id,body,color,created_at_ms,id,position,reference,title,updated_at_ms,url}").await;
    api.call(
        ("PATCH", "/boards/{id}"),
        &format!("/boards/{id}"),
        Some(json!({"name": "Inbox 2"})),
        200,
        "{color,id,name}",
    )
    .await;

    // Todos and notes.
    let todo = api.call(("POST", "/boards/{id}/todos"), &format!("/boards/{id}/todos"),
        Some(json!({"title": "Reply"})), 201,
        "{archived_at_ms,archived_from_board,board_id,body,color,created_at_ms,done,id,position,reference,title,updated_at_ms,url}").await["id"].as_i64().unwrap();
    let note = api.call(("POST", "/boards/{id}/notes"), &format!("/boards/{id}/notes"),
        Some(json!({"body": "Freeze Thursday"})), 201,
        "{archived_at_ms,archived_from_board,board_id,body,color,created_at_ms,id,position,reference,title,updated_at_ms,url}").await["id"].as_i64().unwrap();
    api.call(
        ("GET", "/boards/{id}"),
        &format!("/boards/{id}"),
        None,
        200,
        "{board,notes,todos}",
    )
    .await;
    api.call(("PATCH", "/todos/{id}"), &format!("/todos/{todo}"), Some(json!({"title": "Reply soon"})), 200,
        "{archived_at_ms,archived_from_board,board_id,body,color,created_at_ms,done,id,position,reference,title,updated_at_ms,url}").await;
    api.call(("PATCH", "/notes/{id}"), &format!("/notes/{note}"), Some(json!({"body": "Freeze Friday"})), 200,
        "{archived_at_ms,archived_from_board,board_id,body,color,created_at_ms,id,position,reference,title,updated_at_ms,url}").await;
    api.call(
        ("POST", "/todos/{id}/archive"),
        &format!("/todos/{todo}/archive"),
        None,
        200,
        "{ok}",
    )
    .await;
    api.call(
        ("POST", "/notes/{id}/archive"),
        &format!("/notes/{note}/archive"),
        None,
        200,
        "{ok}",
    )
    .await;
    api.call(
        ("GET", "/boards/{id}/archive"),
        &format!("/boards/{id}/archive"),
        None,
        200,
        "{board,notes,todos}",
    )
    .await;
    api.call(("POST", "/todos/{id}/restore"), &format!("/todos/{todo}/restore"), Some(json!({"board_id": id})), 200,
        "{archived_at_ms,archived_from_board,board_id,body,color,created_at_ms,done,id,position,reference,title,updated_at_ms,url}").await;
    api.call(("POST", "/notes/{id}/restore"), &format!("/notes/{note}/restore"), Some(json!({"board_id": id})), 200,
        "{archived_at_ms,archived_from_board,board_id,body,color,created_at_ms,id,position,reference,title,updated_at_ms,url}").await;
    api.call(("POST", "/todos/{id}/move"), &format!("/todos/{todo}/move"), Some(json!({"board_id": other})), 200,
        "{archived_at_ms,archived_from_board,board_id,body,color,created_at_ms,done,id,position,reference,title,updated_at_ms,url}").await;
    api.call(("POST", "/notes/{id}/move"), &format!("/notes/{note}/move"), Some(json!({"board_id": other})), 200,
        "{archived_at_ms,archived_from_board,board_id,body,color,created_at_ms,id,position,reference,title,updated_at_ms,url}").await;
    api.call(
        ("DELETE", "/todos/{id}"),
        &format!("/todos/{todo}"),
        None,
        200,
        "{ok}",
    )
    .await;
    api.call(
        ("DELETE", "/notes/{id}"),
        &format!("/notes/{note}"),
        None,
        200,
        "{ok}",
    )
    .await;
    api.call(
        ("DELETE", "/boards/{id}"),
        &format!("/boards/{other}"),
        None,
        200,
        "{deleted}",
    )
    .await;
    api.call(
        ("DELETE", "/boards/{id}"),
        &format!("/boards/{id}"),
        Some(json!({"archive_contents": true})),
        200,
        "{deleted}",
    )
    .await;
    api.call(("GET", "/archive"), "/archive", None, 200, "{notes,todos}")
        .await;

    // Layouts and preferences.
    let layout = json!({"view": {"x": 0, "y": 0}, "cards": []});
    api.call(
        ("PUT", "/layouts/{name}"),
        "/layouts/Day%20one",
        Some(layout),
        200,
        "{cards,name,updated_at_ms,view}",
    )
    .await;
    api.call(
        ("GET", "/layouts"),
        "/layouts",
        None,
        200,
        "[{cards,name,updated_at_ms,view}]",
    )
    .await;
    api.call(
        ("PATCH", "/layouts/{name}"),
        "/layouts/Day%20one",
        Some(json!({"name": "Night"})),
        200,
        "{cards,name,updated_at_ms,view}",
    )
    .await;
    api.call(
        ("PATCH", "/preferences"),
        "/preferences",
        Some(json!({"last_layout": "Night"})),
        200,
        "{last_layout}",
    )
    .await;
    api.call(
        ("GET", "/preferences"),
        "/preferences",
        None,
        200,
        "{last_layout}",
    )
    .await;
    api.call(
        ("DELETE", "/layouts/{name}"),
        "/layouts/Night",
        None,
        200,
        "{deleted}",
    )
    .await;
    api.call(
        ("DELETE", "/feeds/{feed}"),
        "/feeds/reviews",
        None,
        200,
        "{deleted}",
    )
    .await;

    // Errors keep one shape.
    api.call(
        ("GET", "/feeds/{feed}"),
        "/feeds/missing",
        None,
        404,
        "{error}",
    )
    .await;
    let (status, _) = client::request(&api.paths, "GET", "/nowhere", vec![], None)
        .await
        .unwrap();
    assert_eq!(status.as_u16(), 404);
    let (status, _) = client::request(&api.paths, "PUT", "/archive", vec![], None)
        .await
        .unwrap();
    assert_eq!(status.as_u16(), 405);
    let (status, _) = client::request(&api.paths, "PUT", "/layouts/%zz", vec![], None)
        .await
        .unwrap();
    assert_eq!(status.as_u16(), 400);

    let frozen: BTreeSet<_> = FROZEN.iter().copied().collect();
    assert_eq!(api.seen, frozen, "every frozen route is exercised");
    let _ = stop.send(());
    service.await.unwrap().unwrap();
}
