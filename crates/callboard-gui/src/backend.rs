//! Read-only service access. Never opens SQLite or starts a second service.
use callboard::{client, lifecycle::Paths};
use callboard_core::store::{BoardContents, BoardInfo, Feed, FeedInfo};
use serde::de::DeserializeOwned;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Feed(String),
    Board(i64),
    Archive,
}

pub enum Contents {
    Feed(Feed),
    Board(BoardContents),
    Missing,
}

pub struct Snapshot {
    pub feeds: Vec<FeedInfo>,
    pub boards: Vec<BoardInfo>,
    pub contents: Option<Contents>,
}

async fn get<T: DeserializeOwned>(
    paths: &Paths,
    auto: Option<&Path>,
    resource: &str,
) -> Result<T, String> {
    let (status, body) = client::request(paths, "GET", resource, vec![], auto)
        .await
        .map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", String::from_utf8_lossy(&body)));
    }
    serde_json::from_slice(&body).map_err(|e| format!("Invalid service response: {e}"))
}

pub async fn load(
    paths: &Paths,
    auto: Option<&Path>,
    target: Option<&Target>,
) -> Result<Snapshot, String> {
    let feeds: Vec<FeedInfo> = get(paths, auto, "/feeds").await?;
    let boards: Vec<BoardInfo> = get(paths, auto, "/boards").await?;
    let contents = match target {
        Some(Target::Feed(name)) if !feeds.iter().any(|f| &f.name == name) => {
            Some(Contents::Missing)
        }
        Some(Target::Board(id)) if !boards.iter().any(|b| &b.id == id) => Some(Contents::Missing),
        Some(Target::Feed(name)) => {
            callboard_core::feed::validate_feed_name(name).map_err(|e| e.to_string())?;
            Some(Contents::Feed(
                get(paths, auto, &format!("/feeds/{name}")).await?,
            ))
        }
        Some(Target::Board(id)) => Some(Contents::Board(
            get(paths, auto, &format!("/boards/{id}")).await?,
        )),
        Some(Target::Archive) => Some(Contents::Board(get(paths, auto, "/archive").await?)),
        None => None,
    };
    Ok(Snapshot {
        feeds,
        boards,
        contents,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use callboard::{lifecycle::Environment, server};
    use serde_json::json;
    use std::{os::unix::fs::PermissionsExt, time::Duration};

    #[tokio::test]
    async fn reads_authenticated_service_and_retains_deleted_target_placeholders() {
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
        assert!(load(&paths, None, None).await.is_err());
        assert!(!paths.socket().exists());
        let (stop, shutdown) = tokio::sync::oneshot::channel();
        let service_paths = paths.clone();
        let task = tokio::spawn(async move {
            server::serve(service_paths, async {
                let _ = shutdown.await;
            })
            .await
            .unwrap();
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            while client::request(&paths, "GET", "/health", vec![], None)
                .await
                .is_err()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        async fn send(
            paths: &Paths,
            method: &str,
            path: &str,
            value: serde_json::Value,
        ) -> serde_json::Value {
            let (status, bytes) = client::request(
                paths,
                method,
                path,
                serde_json::to_vec(&value).unwrap(),
                None,
            )
            .await
            .unwrap();
            assert!(status.is_success());
            serde_json::from_slice(&bytes).unwrap()
        }
        send(
            &paths,
            "PUT",
            "/feeds/work",
            json!({"items":[{"key":"a","title":"Original"}]}),
        )
        .await;
        let id = send(&paths, "POST", "/boards", json!({"name":"Inbox"})).await["id"]
            .as_i64()
            .unwrap();
        send(
            &paths,
            "POST",
            "/feeds/work/items/a/promote",
            json!({"board_id":id,"kind":"todo"}),
        )
        .await;
        send(
            &paths,
            "PUT",
            "/feeds/work",
            json!({"items":[{"key":"a","title":"Current"}]}),
        )
        .await;
        let view = load(&paths, None, Some(&Target::Board(id))).await.unwrap();
        assert_eq!(view.feeds.len(), 1);
        assert_eq!(view.boards.len(), 1);
        let Some(Contents::Board(board)) = view.contents else {
            panic!("missing board")
        };
        assert_eq!(board.todos[0].item.title, "Original");
        assert!(
            matches!(&board.todos[0].resolved_reference,Some(callboard_core::store::ResolvedReference::Live {item}) if item.title == "Current")
        );
        let view = load(&paths, None, Some(&Target::Feed("work".into())))
            .await
            .unwrap();
        assert!(
            matches!(view.contents,Some(Contents::Feed(feed)) if feed.items[0].title == "Current")
        );
        send(
            &paths,
            "DELETE",
            &format!("/boards/{id}"),
            json!({"archive_contents":true}),
        )
        .await;
        let view = load(&paths, None, Some(&Target::Board(id))).await.unwrap();
        assert!(view.boards.is_empty());
        assert!(matches!(view.contents, Some(Contents::Missing)));
        send(&paths, "DELETE", "/feeds/work", json!(null)).await;
        let view = load(&paths, None, Some(&Target::Archive)).await.unwrap();
        let Some(Contents::Board(board)) = view.contents else {
            panic!("missing archive")
        };
        assert!(matches!(
            board.todos[0].resolved_reference,
            Some(callboard_core::store::ResolvedReference::SourceGone)
        ));
        stop.send(()).unwrap();
        task.await.unwrap();
        assert!(load(&paths, None, None).await.is_err());
    }
}
