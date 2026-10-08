#![cfg(target_os = "linux")]
//! Watches over HTTP and the CLI (DESIGN.md §4a, §8.1, §9.1): every watch
//! route's status and response shape, its errors, and the CLI's exit codes.
use callboard_service::{
    client,
    lifecycle::{Environment, Paths},
};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, process::Stdio, time::Duration};
use tokio::io::AsyncWriteExt;

struct Fixture {
    root: tempfile::TempDir,
    paths: Paths,
}

impl Fixture {
    fn new() -> Self {
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
        Self { root, paths }
    }

    fn command(&self) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_callboard"));
        cmd.env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("CALLBOARD_SOCKET_DIR", self.root.path().join("run"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }

    /// Run the CLI with `input` on stdin: (exit code, JSON output or null).
    async fn cli(&self, args: &[&str], input: &str) -> (Option<i32>, Value) {
        let mut child = self.command().args(args).spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input.as_bytes()).await.unwrap();
        drop(stdin);
        let out = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        let value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
        (out.status.code(), value)
    }

    async fn api(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        let body = if body.is_null() {
            vec![]
        } else {
            serde_json::to_vec(&body).unwrap()
        };
        let (status, bytes) = client::request(&self.paths, method, path, body, None)
            .await
            .unwrap();
        (status.as_u16(), serde_json::from_slice(&bytes).unwrap())
    }

    async fn wait_ready(&self) {
        for _ in 0..100 {
            if let Ok((status, _)) =
                client::request(&self.paths, "GET", "/health", vec![], None).await
                && status.is_success()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("service did not become ready");
    }

    fn stop(&self) {
        if let Ok(stream) = std::os::unix::net::UnixStream::connect(self.paths.socket())
            && let Ok(cred) = rustix::net::sockopt::socket_peercred(&stream)
        {
            let _ = rustix::process::kill_process(cred.pid, rustix::process::Signal::TERM);
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Object keys, sorted, to pin a response's shape.
fn keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap_or_else(|| panic!("not an object: {value}"))
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    keys
}

#[tokio::test]
async fn watch_routes_report_add_acknowledge_and_delete() {
    let f = Fixture::new();
    let mut service = f.command().arg("serve").spawn().unwrap();
    f.wait_ready().await;

    // A watch exists only once its script reports.
    let (status, _) = f
        .api(
            "POST",
            "/watches/tickets/items",
            json!({"url": "https://example.com/1"}),
        )
        .await;
    assert_eq!(status, 404);
    let (status, summary) = f
        .api(
            "POST",
            "/watches/tickets/report",
            json!({"title": "Tickets", "quiet_after": "never"}),
        )
        .await;
    assert_eq!(status, 200, "{summary}");
    assert_eq!(
        keys(&summary),
        [
            "attention_ids",
            "created",
            "ignored_ids",
            "quiet_ids",
            "reported"
        ]
    );
    assert_eq!(summary["created"], true);

    let (status, added) = f
        .api(
            "POST",
            "/watches/tickets/items",
            json!({"url": "https://example.com/1", "label": "Refund"}),
        )
        .await;
    assert_eq!(status, 201, "{added}");
    assert_eq!(keys(&added), ["created", "item"]);
    let id = added["item"]["id"].as_i64().unwrap();
    let (status, again) = f
        .api(
            "POST",
            "/watches/tickets/items",
            json!({"url": "https://example.com/1"}),
        )
        .await;
    assert_eq!((status, again["item"]["id"].as_i64()), (200, Some(id)));

    let report = |fingerprint: &str| {
        json!({"quiet_after": "never",
               "items": [{"id": id, "title": "Ticket", "fingerprint": fingerprint}]})
    };
    f.api("POST", "/watches/tickets/report", report("1")).await;
    let (_, summary) = f.api("POST", "/watches/tickets/report", report("2")).await;
    assert_eq!(summary["attention_ids"], json!([id]));

    let (status, list) = f.api("GET", "/watches", Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(
        keys(&list[0]),
        [
            "attention_count",
            "description",
            "error",
            "item_count",
            "last_reported_at_ms",
            "name",
            "new_count",
            "next_wake_at_ms",
            "quiet_after",
            "quiet_count",
            "source_url",
            "stale_after",
            "title",
            "waiting_after",
        ]
    );
    assert_eq!(list[0]["attention_count"], 1);
    let (status, watch) = f.api("GET", "/watches/tickets", Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(keys(&watch), ["info", "items", "next_wake_at_ms"]);
    assert_eq!(watch["items"][0]["state"], "attention");
    assert_eq!(watch["items"][0]["label"], "Refund");

    let item = format!("/watches/tickets/items/{id}");
    let (status, acked) = f.api("PATCH", &item, json!({"acknowledge": true})).await;
    assert_eq!((status, acked["state"].as_str()), (200, Some("waiting")));
    for (body, expected) in [
        (json!({}), 400),
        (json!({"acknowledge": true, "keep_waiting": true}), 400),
        (json!({"snooze": true}), 400),
    ] {
        assert_eq!(f.api("PATCH", &item, body).await.0, expected);
    }
    assert_eq!(
        f.api(
            "PATCH",
            "/watches/tickets/items/999",
            json!({"acknowledge": true})
        )
        .await
        .0,
        404
    );

    let (status, recorded) = f
        .api(
            "POST",
            "/watches/tickets/error",
            json!({"id": id, "message": "could not read the page"}),
        )
        .await;
    assert_eq!((status, recorded), (200, json!({"recorded": true})));
    assert_eq!(
        f.api("POST", "/watches/missing/error", json!({"message": "x"}))
            .await
            .0,
        404
    );
    // Invalid reports change nothing.
    let (status, _) = f
        .api(
            "POST",
            "/watches/tickets/report",
            json!({"items": [{"id": id, "title": "No fingerprint"}]}),
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(f.api("GET", "/watches/Bad", Value::Null).await.0, 404);

    assert_eq!(
        f.api("DELETE", &item, Value::Null).await,
        (200, json!({"deleted": true}))
    );
    assert_eq!(
        f.api("DELETE", "/watches/tickets", Value::Null).await,
        (200, json!({"deleted": true}))
    );
    assert_eq!(f.api("GET", "/watches/tickets", Value::Null).await.0, 404);
    f.stop();
    assert!(service.wait().await.unwrap().success());
}

#[tokio::test]
async fn the_cli_reports_with_exit_codes_and_manages_items() {
    let f = Fixture::new();
    let mut service = f.command().arg("serve").spawn().unwrap();
    f.wait_ready().await;

    // Empty input reports metadata only, and creates the watch.
    let (code, summary) = f
        .cli(
            &[
                "watch",
                "report",
                "tickets",
                "--title",
                "Tickets",
                "--quiet-after",
                "never",
            ],
            "",
        )
        .await;
    assert_eq!((code, summary["created"].clone()), (Some(0), json!(true)));
    let (code, added) = f
        .cli(
            &[
                "watch",
                "item",
                "add",
                "tickets",
                "https://example.com/1",
                "--label",
                "Refund",
            ],
            "",
        )
        .await;
    assert_eq!(code, Some(0));
    let id = added["item"]["id"].as_i64().unwrap();
    let ids = id.to_string();

    let report =
        |fingerprint: &str| format!(r#"[{{"id":{id},"title":"T","fingerprint":"{fingerprint}"}}]"#);
    let args = [
        "watch",
        "report",
        "tickets",
        "--quiet-after",
        "never",
        "--exit-attention",
        "10",
        "--exit-quiet",
        "11",
    ];
    assert_eq!(f.cli(&args, &report("1")).await.0, Some(0), "baseline");
    assert_eq!(f.cli(&args, &report("2")).await.0, Some(10));
    assert_eq!(f.cli(&args, &report("2")).await.0, Some(0));
    let (_, watch) = f.cli(&["watch", "items", "tickets"], "").await;
    assert_eq!(watch["items"][0]["state"], "attention");
    let (code, acked) = f.cli(&["watch", "ack", "tickets", &ids], "").await;
    assert_eq!((code, acked["state"].as_str()), (Some(0), Some("waiting")));

    // Quiet at once with zero windows; attention wins when both apply.
    let quiet = [
        "watch",
        "report",
        "tickets",
        "--waiting-after",
        "0s",
        "--quiet-after",
        "0s",
        "--exit-attention",
        "10",
        "--exit-quiet",
        "11",
    ];
    assert_eq!(f.cli(&quiet, "").await.0, Some(11));
    assert_eq!(f.cli(&quiet, "").await.0, Some(0), "listed once");
    assert_eq!(
        f.cli(&["watch", "keep-waiting", "tickets", &ids], "")
            .await
            .0,
        Some(0)
    );
    assert_eq!(f.cli(&quiet, &report("3")).await.0, Some(10));

    assert_eq!(
        f.cli(&["watch", "fail", "tickets", "rate limited"], "")
            .await
            .0,
        Some(0)
    );
    assert_eq!(
        f.cli(&["watch", "fail", "tickets", &ids, "gone"], "")
            .await
            .0,
        Some(0)
    );
    let (_, watch) = f.cli(&["watch", "items", "tickets"], "").await;
    assert_eq!(watch["info"]["error"]["message"], "rate limited");
    assert_eq!(watch["items"][0]["error"]["message"], "gone");
    let (_, list) = f.cli(&["watches"], "").await;
    assert_eq!(list[0]["name"], "tickets");

    // Invalid input fails before the service is asked.
    for bad in [
        vec!["watch", "report", "Bad Name"],
        vec!["watch", "ack", "tickets", "0"],
        vec!["watch", "item", "add", "tickets", " "],
        vec!["watch", "report", "tickets", "--quiet-after", "soon"],
    ] {
        assert_ne!(f.cli(&bad, "").await.0, Some(0), "{bad:?}");
    }
    assert_ne!(
        f.cli(&["watch", "report", "tickets"], "{nope").await.0,
        Some(0)
    );

    assert_eq!(
        f.cli(&["watch", "item", "rm", "tickets", &ids], "").await,
        (Some(0), json!({"deleted": true}))
    );
    assert_eq!(
        f.cli(&["watch", "rm", "tickets"], "").await,
        (Some(0), json!({"deleted": true}))
    );
    assert_ne!(f.cli(&["watch", "items", "tickets"], "").await.0, Some(0));
    f.stop();
    assert!(service.wait().await.unwrap().success());
}
