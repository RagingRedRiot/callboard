#![cfg(target_os = "linux")]
use callboard::{
    client,
    lifecycle::{Environment, Paths},
    server,
};
use serde_json::{Value, json};
use std::{
    os::unix::{fs::PermissionsExt, net::UnixStream},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
        let env = Environment {
            data_home: Some(root.path().join("data")),
            config_home: Some(root.path().join("config")),
            socket_dir: Some(root.path().join("run")),
            ..Default::default()
        };
        let paths = Paths::resolve(&env).unwrap();
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
    async fn cli(&self, args: &[&str], input: &str) -> std::process::Output {
        let mut child = self.command().args(args).spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
            .await
            .unwrap()
            .unwrap()
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
    async fn api(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        let (status, bytes) = client::request(
            &self.paths,
            method,
            path,
            serde_json::to_vec(&body).unwrap(),
            None,
        )
        .await
        .unwrap();
        (status.as_u16(), serde_json::from_slice(&bytes).unwrap())
    }
    fn signal(&self) {
        if let Ok(stream) = UnixStream::connect(self.paths.socket())
            && let Ok(cred) = rustix::net::sockopt::socket_peercred(&stream)
        {
            let _ = rustix::process::kill_process(cred.pid, rustix::process::Signal::TERM);
        }
    }
    async fn stop(&self) {
        self.signal();
        for _ in 0..350 {
            if !self.paths.socket().exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("service did not stop");
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.signal();
    }
}
fn json_output(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap()
}

#[tokio::test]
async fn foreground_cli_roundtrip_errors_exit_codes_and_restart() {
    let f = Fixture::new();
    let mut service = f.command().arg("serve").spawn().unwrap();
    f.wait_ready().await;
    let out = f
        .cli(
            &[
                "--no-auto-start",
                "put",
                "work",
                "--title",
                "Reviews",
                "--exit-added",
                "10",
                "--exit-changed",
                "11",
            ],
            r#"[{"key":"a","title":"A"}]"#,
        )
        .await;
    assert_eq!(
        out.status.code(),
        Some(10),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(json_output(&out)["added"], 1);
    let out = f
        .cli(
            &["put", "work", "--exit-added", "10", "--exit-changed", "11"],
            r#"[{"key":"a","title":"B"}]"#,
        )
        .await;
    assert_eq!(out.status.code(), Some(11));
    let out = f
        .cli(
            &["put", "work", "--exit-changed", "11"],
            r#"[{"key":"a","title":"B"}]"#,
        )
        .await;
    assert!(out.status.success());
    assert_eq!(json_output(&out)["unchanged"], 1);
    assert!(
        f.cli(&["fail", "work", "upstream failed"], "")
            .await
            .status
            .success()
    );
    let before = json_output(&f.cli(&["get", "work"], "").await);
    assert_eq!(before["info"]["error"]["message"], "upstream failed");
    for input in [
        "",
        "{}",
        r#"[{"key":"a","title":"A"},{"key":"a","title":"B"}]"#,
    ] {
        assert!(!f.cli(&["put", "work"], input).await.status.success());
    }
    assert_eq!(json_output(&f.cli(&["get", "work"], "").await), before);
    let cleared = f.cli(&["put", "work", "--exit-changed", "12"], "[]").await;
    assert_eq!(cleared.status.code(), Some(12));
    assert_eq!(json_output(&cleared)["removed"], 1);
    f.stop().await;
    assert!(service.wait().await.unwrap().success());
    let mut service = f.command().arg("serve").spawn().unwrap();
    f.wait_ready().await;
    assert_eq!(json_output(&f.cli(&["feeds"], "").await)[0]["name"], "work");
    assert_eq!(
        json_output(&f.cli(&["get", "work"], "").await)["items"],
        json!([])
    );
    assert!(f.cli(&["feed", "rm", "work"], "").await.status.success());
    assert!(!f.cli(&["get", "work"], "").await.status.success());
    f.stop().await;
    assert!(service.wait().await.unwrap().success());
}

#[tokio::test]
async fn http_limits_chunking_and_routes_do_not_mutate_on_rejection() {
    let f = Fixture::new();
    let mut service = f.command().arg("serve").spawn().unwrap();
    f.wait_ready().await;
    assert_eq!(
        f.api(
            "PUT",
            "/feeds/work",
            json!({"items":[{"key":"a","title":"A"}]})
        )
        .await
        .0,
        200
    );
    let before = f.api("GET", "/feeds/work", json!(null)).await.1;
    assert_eq!(f.api("PUT", "/feeds/work", json!({})).await.0, 400);
    assert_eq!(f.api("PATCH", "/feeds/work", json!({})).await.0, 405);
    assert_eq!(f.api("GET", "/missing", json!({})).await.0, 404);
    assert_eq!(
        f.api("POST", "/feeds/absent/error", json!({"message":"failed"}))
            .await
            .0,
        404
    );
    // Exercise a large declared length without transmitting that body.
    let mut socket = tokio::net::UnixStream::connect(f.paths.socket())
        .await
        .unwrap();
    socket
        .write_all(
            b"PUT /feeds/work HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4194305\r\n\r\n",
        )
        .await
        .unwrap();
    let mut reply = Vec::new();
    socket.read_to_end(&mut reply).await.unwrap();
    assert!(String::from_utf8_lossy(&reply).starts_with("HTTP/1.1 413"));
    // Chunked transfer must obey the same limit without Content-Length.
    let mut socket = tokio::net::UnixStream::connect(f.paths.socket())
        .await
        .unwrap();
    socket.write_all(b"PUT /feeds/work HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n400001\r\n").await.unwrap();
    let payload = vec![b' '; 4194305];
    let _ = socket.write_all(&payload).await;
    let _ = socket.write_all(b"\r\n0\r\n\r\n").await;
    let mut reply = Vec::new();
    let _ = socket.read_to_end(&mut reply).await;
    assert!(String::from_utf8_lossy(&reply).starts_with("HTTP/1.1 413"));
    assert_eq!(f.api("GET", "/feeds/work", json!(null)).await.1, before);
    f.stop().await;
    service.wait().await.unwrap();
}

#[tokio::test]
async fn board_api_lifecycle_errors_archive_and_restore() {
    let f = Fixture::new();
    let mut service = f.command().arg("serve").spawn().unwrap();
    f.wait_ready().await;

    let (status, created) = f.api("POST", "/boards", json!({"name":"Inbox"})).await;
    assert_eq!(status, 201);
    let board = created["id"].as_i64().unwrap();
    assert_eq!(
        f.api("POST", "/boards", json!({"name":"Inbox"})).await.0,
        409
    );
    assert_eq!(
        f.api("GET", "/boards", json!(null))
            .await
            .1
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let (status, todo) = f
        .api(
            "POST",
            &format!("/boards/{board}/todos"),
            json!({
                "title":"Call back", "body":"Before lunch", "reference":{"feed":"calls","key":"one"}
            }),
        )
        .await;
    assert_eq!(status, 201);
    let todo_id = todo["id"].as_i64().unwrap();
    let (status, note) = f
        .api(
            "POST",
            &format!("/boards/{board}/notes"),
            json!({
                "title":"Details", "body":"Keep this", "color":"blue"
            }),
        )
        .await;
    assert_eq!(status, 201);
    let note_id = note["id"].as_i64().unwrap();

    let (status, patched_todo) = f
        .api(
            "PATCH",
            &format!("/todos/{todo_id}"),
            json!({
                "title":"Call back soon", "body":null, "done":true, "position":0
            }),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(patched_todo["title"], "Call back soon");
    assert_eq!(patched_todo["body"], Value::Null);
    assert_eq!(
        f.api(
            "PATCH",
            &format!("/notes/{note_id}"),
            json!({"title":null,"body":"Changed","color":null})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        f.api(
            "PATCH",
            &format!("/notes/{note_id}"),
            json!({"archived":true})
        )
        .await
        .0,
        200
    );
    let archive = f
        .api("GET", &format!("/boards/{board}/archive"), json!(null))
        .await
        .1;
    assert_eq!(archive["notes"][0]["id"], note_id);
    assert_eq!(
        f.api(
            "PATCH",
            &format!("/notes/{note_id}"),
            json!({"archived":false,"board_id":board})
        )
        .await
        .0,
        200
    );

    assert_eq!(
        f.api(
            "DELETE",
            &format!("/boards/{board}"),
            json!({"archive_contents":false})
        )
        .await
        .0,
        409
    );
    assert_eq!(
        f.api(
            "DELETE",
            &format!("/boards/{board}"),
            json!({"archive_contents":true})
        )
        .await
        .0,
        200
    );
    let global_archive = f.api("GET", "/archive", json!(null)).await.1;
    assert_eq!(global_archive["todos"][0]["id"], todo_id);
    assert_eq!(global_archive["notes"][0]["id"], note_id);

    let destination = f.api("POST", "/boards", json!({"name":"Later"})).await.1["id"]
        .as_i64()
        .unwrap();
    assert_eq!(
        f.api(
            "PATCH",
            &format!("/todos/{todo_id}"),
            json!({"board_id":destination,"archived":false})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        f.api(
            "PATCH",
            &format!("/notes/{note_id}"),
            json!({"board_id":destination,"archived":false})
        )
        .await
        .0,
        200
    );
    let moved_to = f.api("POST", "/boards", json!({"name":"Moved"})).await.1["id"]
        .as_i64()
        .unwrap();
    assert_eq!(
        f.api(
            "PATCH",
            &format!("/notes/{note_id}"),
            json!({"board_id":moved_to})
        )
        .await
        .0,
        200
    );
    let contents = f
        .api("GET", &format!("/boards/{destination}"), json!(null))
        .await
        .1;
    assert_eq!(contents["todos"][0]["done"], true);
    assert!(contents["notes"].as_array().unwrap().is_empty());
    let moved = f
        .api("GET", &format!("/boards/{moved_to}"), json!(null))
        .await
        .1;
    assert_eq!(moved["notes"][0]["body"], "Changed");
    assert_eq!(f.api("GET", "/boards/999999", json!(null)).await.0, 404);

    f.stop().await;
    assert!(service.wait().await.unwrap().success());
}

#[tokio::test]
async fn feed_item_api_patches_snooze_manual_order_and_reset() {
    let f = Fixture::new();
    let mut service = f.command().arg("serve").spawn().unwrap();
    f.wait_ready().await;
    assert_eq!(
        f.api(
            "PUT",
            "/feeds/work",
            json!({"items":[
                {"key":"a","title":"A"}, {"key":"b","title":"B"}, {"key":"c","title":"C"}
            ]})
        )
        .await
        .0,
        200
    );

    let (status, changed) = f
        .api(
            "PATCH",
            "/feeds/work/items/b",
            json!({
                "position":0, "snoozed_until_ms":i64::MAX, "wake_on_update":true
            }),
        )
        .await;
    assert_eq!(status, 200, "feed item PATCH response: {changed}");
    assert_eq!(changed["manual_order"], true);
    assert_eq!(changed["state"]["snoozed"], true);
    let (status, feed) = f.api("GET", "/feeds/work", json!(null)).await;
    assert_eq!(status, 200);
    assert_eq!(feed["items"][0]["key"], "b");
    assert_eq!(feed["view_state"]["b"]["wake_on_update"], true);

    assert_eq!(
        f.api(
            "PUT",
            "/feeds/work",
            json!({"items":[
                {"key":"c","title":"C"}, {"key":"d","title":"D"},
                {"key":"b","title":"B changed"}, {"key":"a","title":"A"}
            ]})
        )
        .await
        .0,
        200
    );
    let (_, after) = f.api("GET", "/feeds/work", json!(null)).await;
    assert_eq!(after["items"][0]["key"], "d");
    assert_eq!(after["items"][1]["key"], "b");
    assert!(after["view_state"]["b"].is_null());
    assert_eq!(
        f.api("PATCH", "/feeds/work/items/missing", json!({"position":0}))
            .await
            .0,
        404
    );
    assert_eq!(
        f.api("PATCH", "/feeds/work/items/b", json!({"position":99}))
            .await
            .0,
        400
    );

    assert_eq!(
        f.api("PATCH", "/feeds/work/items/b", json!({"reset_order":true}))
            .await
            .0,
        200
    );
    let (_, reset) = f.api("GET", "/feeds/work", json!(null)).await;
    assert_eq!(reset["manual_order"], false);
    assert_eq!(reset["items"][0]["key"], "c");
    assert_eq!(reset["items"][1]["key"], "d");
    f.stop().await;
    assert!(service.wait().await.unwrap().success());
}

#[tokio::test]
async fn feed_item_promotion_copies_fields_and_retains_source_reference() {
    let f = Fixture::new();
    let mut service = f.command().arg("serve").spawn().unwrap();
    f.wait_ready().await;
    let board = f.api("POST", "/boards", json!({"name":"Inbox"})).await.1["id"]
        .as_i64()
        .unwrap();
    assert_eq!(f.api("PUT", "/feeds/alerts", json!({"items":[{
        "key":"alert-1", "title":"Review this", "url":"https://example.com/pr", "body":"Details"
    }]})).await.0, 200);

    let (status, todo) = f
        .api(
            "POST",
            "/feeds/alerts/items/alert-1/promote",
            json!({
                "board_id":board, "kind":"todo"
            }),
        )
        .await;
    assert_eq!(status, 201);
    assert_eq!(todo["title"], "Review this");
    assert_eq!(todo["url"], "https://example.com/pr");
    assert_eq!(todo["reference"], json!({"feed":"alerts","key":"alert-1"}));

    let (status, note) = f
        .api(
            "POST",
            "/feeds/alerts/items/alert-1/promote",
            json!({
                "board_id":board, "kind":"note"
            }),
        )
        .await;
    assert_eq!(status, 201);
    assert_eq!(note["title"], "Review this");
    assert_eq!(note["url"], "https://example.com/pr");
    assert_eq!(note["body"], "Details");
    assert_eq!(note["reference"]["key"], "alert-1");

    assert_eq!(
        f.api(
            "POST",
            "/feeds/alerts/items/missing/promote",
            json!({
                "board_id":board, "kind":"todo"
            })
        )
        .await
        .0,
        404
    );
    assert_eq!(f.api("DELETE", "/feeds/alerts", json!(null)).await.0, 200);
    let contents = f
        .api("GET", &format!("/boards/{board}"), json!(null))
        .await
        .1;
    assert_eq!(contents["todos"][0]["reference"]["feed"], "alerts");
    assert_eq!(contents["notes"][0]["url"], "https://example.com/pr");
    f.stop().await;
    assert!(service.wait().await.unwrap().success());
}

#[tokio::test]
async fn peer_authentication_uses_kernel_identity() {
    let (a, _b) = tokio::net::UnixStream::pair().unwrap();
    let uid = rustix::process::geteuid().as_raw();
    assert!(server::authenticate(&a, uid).is_ok());
    assert_eq!(
        server::authenticate(&a, uid.wrapping_add(1))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
}

#[tokio::test]
async fn automatic_start_races_and_invalid_input_does_not_start_service() {
    let f = Fixture::new();
    assert!(
        !f.cli(&["--no-auto-start", "feeds"], "")
            .await
            .status
            .success()
    );
    assert!(!f.cli(&["put", "work"], "").await.status.success());
    assert!(!f.paths.socket().exists());
    let (a, b) = tokio::join!(f.cli(&["put", "a"], "[]"), f.cli(&["put", "b"], "[]"));
    for out in [a, b] {
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let out = f.cli(&["--no-auto-start", "feeds"], "").await;
    assert_eq!(json_output(&out).as_array().unwrap().len(), 2);
    f.stop().await;
    // Start again from persisted state.
    assert_eq!(
        json_output(&f.cli(&["feeds"], "").await)
            .as_array()
            .unwrap()
            .len(),
        2
    );
    f.stop().await;
}

#[tokio::test]
async fn setup_print_install_repeat_and_refuse_custom_unit() {
    let f = Fixture::new();
    let preview = f.cli(&["setup", "--print"], "").await;
    assert!(preview.status.success());
    assert!(String::from_utf8_lossy(&preview.stdout).contains("ExecStart="));
    assert!(!f.paths.config_dir().parent().unwrap().exists());
    // A fake systemctl confines the integration test to a temporary directory.
    let bin = f.root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let tool = bin.join("systemctl");
    std::fs::write(
        &tool,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CALLBOARD_TEST_LOG\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
    let log = f.root.path().join("systemctl.log");
    let unit = f.root.path().join("config/systemd/user/callboard.service");
    std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
    // Existing systemd config directories need not be private, just non-writable
    // by other users. Do not chmod unrelated systemd directories.
    for dir in [
        f.root.path().join("config"),
        f.root.path().join("config/systemd"),
        unit.parent().unwrap().to_owned(),
    ] {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    for _ in 0..2 {
        let out = f
            .command()
            .args(["setup"])
            .env("PATH", &bin)
            .env("CALLBOARD_TEST_LOG", &log)
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(std::fs::read(&unit).unwrap(), preview.stdout);
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "--user daemon-reload\n--user enable callboard.service\n".repeat(2)
    );
    std::fs::write(&unit, "# custom unit\n").unwrap();
    let out = f
        .command()
        .arg("setup")
        .env("PATH", &bin)
        .env("CALLBOARD_TEST_LOG", &log)
        .output()
        .await
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(std::fs::read_to_string(&unit).unwrap(), "# custom unit\n");
}

#[tokio::test]
async fn audited_item_routes_decode_keys_and_reject_invalid_mutations() {
    let f = Fixture::new();
    let mut service = f.command().arg("serve").spawn().unwrap();
    f.wait_ready().await;
    let (_, board) = f.api("POST", "/boards", json!({"name":"Audit"})).await;
    let board_id = board["id"].as_i64().unwrap();
    for (key, encoded) in [
        (
            "https://example.test/a?q=1#x",
            "https%3A%2F%2Fexample.test%2Fa%3Fq%3D1%23x",
        ),
        ("%2F+é", "%252F+%C3%A9"),
    ] {
        assert_eq!(
            f.api(
                "PUT",
                "/feeds/audit",
                json!({"items":[{"key":key,"title":"Source"}]})
            )
            .await
            .0,
            200
        );
        let path = format!("/feeds/audit/items/{encoded}");
        assert_eq!(
            f.api("PATCH", &path, json!({"wake_on_update":true}))
                .await
                .0,
            200
        );
        let (status, todo) = f
            .api(
                "POST",
                &format!("{path}/promote"),
                json!({"kind":"todo","board_id":board_id}),
            )
            .await;
        assert_eq!(status, 201, "{todo}");
        assert_eq!(todo["reference"]["key"], key);
        let path = format!("/todos/{}", todo["id"]);
        for invalid in [
            json!({"position":-1}),
            json!({"done":null}),
            json!({"title":null}),
            json!({"typo":true}),
            json!({"title":"a".repeat(501)}),
        ] {
            assert_eq!(f.api("PATCH", &path, invalid).await.0, 400);
        }
        assert_eq!(f.api("GET", &path, json!(null)).await.0, 405);
        assert_eq!(
            f.api("POST", &format!("{path}/unknown"), json!({})).await.0,
            404
        );
    }
    for encoded in ["%GG", "%FF", "%"] {
        assert_eq!(
            f.api("PATCH", &format!("/feeds/audit/items/{encoded}"), json!({}))
                .await
                .0,
            400
        );
    }
    assert_eq!(
        f.api("DELETE", &format!("/boards/{board_id}"), json!({}))
            .await
            .0,
        409
    );
    let (_, empty) = f.api("POST", "/boards", json!({"name":"Empty"})).await;
    assert_eq!(
        f.api("DELETE", &format!("/boards/{}", empty["id"]), json!({}))
            .await
            .0,
        200
    );
    f.stop().await;
    assert!(service.wait().await.unwrap().success());
}

#[tokio::test]
async fn board_reads_resolve_sources_without_changing_copied_content() {
    let f = Fixture::new();
    let mut service = f.command().arg("serve").spawn().unwrap();
    f.wait_ready().await;
    let (_, board) = f.api("POST", "/boards", json!({"name":"References"})).await;
    let board_id = board["id"].as_i64().unwrap();
    let board_path = format!("/boards/{board_id}");
    let original =
        json!({"key":"a","title":"Original","body":"Copied body","url":"https://example.test/old"});
    assert_eq!(
        f.api("PUT", "/feeds/source", json!({"items":[original]}))
            .await
            .0,
        200
    );
    let mut ids = Vec::new();
    for kind in ["todo", "note"] {
        let (status, item) = f
            .api(
                "POST",
                "/feeds/source/items/a/promote",
                json!({"kind":kind,"board_id":board_id}),
            )
            .await;
        assert_eq!(status, 201);
        ids.push(item["id"].as_i64().unwrap());
    }
    assert_eq!(
        f.api(
            "POST",
            &format!("{board_path}/notes"),
            json!({"body":"Unlinked"})
        )
        .await
        .0,
        201
    );
    let (_, contents) = f.api("GET", &board_path, json!(null)).await;
    assert_eq!(contents["notes"][1]["resolved_reference"], Value::Null);
    assert_eq!(contents["todos"][0]["resolved_reference"]["item"], original);
    assert_eq!(
        f.api(
            "PATCH",
            &format!("/todos/{}", ids[0]),
            json!({"title":"User edit"})
        )
        .await
        .0,
        200
    );
    let current = json!({"key":"a","title":"Current","body":"Live body","url":"https://example.test/new","tags":["new"],"meta":{"count":2}});
    assert_eq!(
        f.api("PUT", "/feeds/source", json!({"items":[current]}))
            .await
            .0,
        200
    );
    let (_, contents) = f.api("GET", &board_path, json!(null)).await;
    for kind in ["todos", "notes"] {
        assert_eq!(
            contents[kind][0]["resolved_reference"],
            json!({"status":"live","item":current})
        );
        assert_eq!(contents[kind][0]["body"], "Copied body");
        assert_eq!(contents[kind][0]["url"], "https://example.test/old");
    }
    assert_eq!(contents["todos"][0]["title"], "User edit");
    assert_eq!(contents["notes"][0]["title"], "Original");
    // Removing just the key resolves as gone; reappearance resolves live again.
    assert_eq!(
        f.api("PUT", "/feeds/source", json!({"items":[]})).await.0,
        200
    );
    let (_, contents) = f.api("GET", &board_path, json!(null)).await;
    assert_eq!(
        contents["todos"][0]["resolved_reference"],
        json!({"status":"source_gone"})
    );
    assert_eq!(
        f.api("PUT", "/feeds/source", json!({"items":[current]}))
            .await
            .0,
        200
    );
    for (kind, id) in ["todos", "notes"].into_iter().zip(ids) {
        assert_eq!(
            f.api("POST", &format!("/{kind}/{id}/archive"), json!({}))
                .await
                .0,
            200
        );
    }
    let (_, archive) = f
        .api("GET", &format!("{board_path}/archive"), json!(null))
        .await;
    assert_eq!(archive["notes"][0]["resolved_reference"]["status"], "live");
    assert_eq!(
        f.api("DELETE", &board_path, json!({"archive_contents":true}))
            .await
            .0,
        200
    );
    let (_, archive) = f.api("GET", "/archive", json!(null)).await;
    assert_eq!(archive["todos"][0]["resolved_reference"]["item"], current);
    assert_eq!(f.api("DELETE", "/feeds/source", json!(null)).await.0, 200);
    let (_, archive) = f.api("GET", "/archive", json!(null)).await;
    for kind in ["todos", "notes"] {
        assert_eq!(
            archive[kind][0]["resolved_reference"],
            json!({"status":"source_gone"})
        );
        assert_eq!(
            archive[kind][0]["reference"],
            json!({"feed":"source","key":"a"})
        );
    }
    f.stop().await;
    assert!(service.wait().await.unwrap().success());
}

#[tokio::test]
async fn layout_api_saves_replaces_and_rejects_invalid_requests_atomically() {
    let f = Fixture::new();
    let mut service = f.command().arg("serve").spawn().unwrap();
    f.wait_ready().await;
    assert_eq!(
        f.api("GET", "/layouts", json!(null)).await,
        (200, json!([]))
    );
    let path = "/layouts/Work%20%2F%20day";
    let tree = json!({"kind":"tabs","active":1,"children":[{"kind":"feed","name":"missing"},{"kind":"board","id":999}]});
    let (status, saved) = f.api("PUT", path, json!({"tree":tree})).await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(saved["name"], "Work / day");
    assert_eq!(saved["tree"], tree);
    for invalid in [
        json!({}),
        json!({"tree":{"kind":"tabs","active":2,"children":[]}}),
        json!({"tree":{"kind":"board","id":1}}),
    ] {
        assert_eq!(f.api("PUT", path, invalid).await.0, 400);
        assert_eq!(
            f.api("GET", "/layouts", json!(null)).await.1,
            json!([saved])
        );
    }
    assert_eq!(
        f.api("PUT", "/layouts/%FF", json!({"tree":{"kind":"empty"}}))
            .await
            .0,
        400
    );
    assert_eq!(
        f.api("PUT", "/layouts/%20", json!({"tree":{"kind":"empty"}}))
            .await
            .0,
        400
    );
    assert_eq!(f.api("POST", "/layouts", json!({})).await.0, 405);
    assert_eq!(f.api("PUT", "/layouts/a/b", json!({})).await.0, 404);
    assert_eq!(
        f.api(
            "PUT",
            path,
            json!({"tree":{"kind":"feed","name":"x".repeat(65536)}})
        )
        .await
        .0,
        413
    );
    let (status, replaced) = f.api("PUT", path, json!({"tree":{"kind":"empty"}})).await;
    assert_eq!(status, 200);
    assert_eq!(
        f.api("GET", "/layouts", json!(null)).await.1,
        json!([replaced])
    );
    f.stop().await;
    assert!(service.wait().await.unwrap().success());
}

#[tokio::test]
async fn layout_rename_delete_preferences_and_list_counts() {
    let f = Fixture::new();
    let mut service = f.command().arg("serve").spawn().unwrap();
    f.wait_ready().await;
    let empty = json!({"tree":{"kind":"empty"}});
    assert_eq!(f.api("PUT", "/layouts/Day", empty.clone()).await.0, 200);
    assert_eq!(f.api("PUT", "/layouts/Night", empty.clone()).await.0, 200);
    assert_eq!(
        f.api("GET", "/preferences", json!(null)).await,
        (200, json!({"last_layout":null}))
    );
    assert_eq!(
        f.api("PATCH", "/preferences", json!({"last_layout":"Missing"}))
            .await
            .0,
        404
    );
    assert_eq!(
        f.api("PATCH", "/preferences", json!({"typo":1})).await.0,
        400
    );
    assert_eq!(f.api("PUT", "/preferences", json!({})).await.0, 405);
    assert_eq!(
        f.api("PATCH", "/preferences", json!({"last_layout":"Day"}))
            .await,
        (200, json!({"last_layout":"Day"}))
    );

    let path = "/layouts/Work%20%2F%20day";
    assert_eq!(
        f.api("PATCH", "/layouts/Day", json!({"name":"Night"}))
            .await
            .0,
        409
    );
    assert_eq!(
        f.api("PATCH", "/layouts/Missing", json!({"name":"x"}))
            .await
            .0,
        404
    );
    for invalid in [
        json!({}),
        json!({"name":" x"}),
        json!({"name":"x","typo":1}),
    ] {
        assert_eq!(f.api("PATCH", "/layouts/Day", invalid).await.0, 400);
    }
    let (status, renamed) = f
        .api("PATCH", "/layouts/Day", json!({"name":"Work / day"}))
        .await;
    assert_eq!(status, 200, "{renamed}");
    assert_eq!(renamed["name"], "Work / day");
    assert_eq!(renamed["tree"], empty["tree"]);
    assert_eq!(
        f.api("GET", "/preferences", json!(null)).await.1,
        json!({"last_layout":"Work / day"})
    );
    assert_eq!(
        f.api("DELETE", path, json!(null)).await,
        (200, json!({"deleted":true}))
    );
    assert_eq!(
        f.api("DELETE", path, json!(null)).await,
        (200, json!({"deleted":false}))
    );
    assert_eq!(
        f.api("GET", "/preferences", json!(null)).await.1,
        json!({"last_layout":null})
    );
    let names: Vec<Value> = f
        .api("GET", "/layouts", json!(null))
        .await
        .1
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["name"].clone())
        .collect();
    assert_eq!(names, [json!("Night")]);

    assert_eq!(
        f.api(
            "PUT",
            "/feeds/work",
            json!({"items":[{"key":"a","title":"A"},{"key":"b","title":"B"}]})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        f.api(
            "PATCH",
            "/feeds/work/items/a",
            json!({"snoozed_until_ms": 4_000_000_000_000_i64})
        )
        .await
        .0,
        200
    );
    let feeds = f.api("GET", "/feeds", json!(null)).await.1;
    assert_eq!(feeds[0]["name"], "work");
    assert_eq!(feeds[0]["item_count"], 2);
    assert_eq!(feeds[0]["snoozed_count"], 1);
    assert_eq!(feeds[0]["next_wake_at_ms"], 4_000_000_000_000_i64);
    let (_, board) = f.api("POST", "/boards", json!({"name":"Work"})).await;
    let id = board["id"].as_i64().unwrap();
    f.api("POST", &format!("/boards/{id}/todos"), json!({"title":"T"}))
        .await;
    f.api("POST", &format!("/boards/{id}/notes"), json!({"body":"N"}))
        .await;
    let boards = f.api("GET", "/boards", json!(null)).await.1;
    assert_eq!(
        boards,
        json!([{"id":id,"name":"Work","todo_count":1,"open_todo_count":1,"note_count":1}])
    );
    f.stop().await;
    assert!(service.wait().await.unwrap().success());
}

#[tokio::test]
async fn event_stream_delivers_committed_changes_and_reconnects_with_resync() {
    use http_body_util::{BodyExt, Full};
    use hyper::{Request, body::Bytes};
    use hyper_util::rt::TokioIo;
    let f = Fixture::new();
    let mut service = f.command().arg("serve").spawn().unwrap();
    f.wait_ready().await;
    assert_eq!(f.api("POST", "/events", json!({})).await.0, 405);
    for _ in 0..2 {
        let stream = tokio::net::UnixStream::connect(f.paths.socket())
            .await
            .unwrap();
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        let driver = tokio::spawn(connection);
        let response = sender
            .send_request(
                Request::builder()
                    .uri("/events")
                    .body(Full::new(Bytes::new()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let mut body = response.into_body();
        let first = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        assert!(
            std::str::from_utf8(&first)
                .unwrap()
                .contains("event: resync")
        );
        assert_eq!(
            f.api("PUT", "/layouts/Day", json!({"tree":{"kind":"empty"}}))
                .await
                .0,
            200
        );
        let changed = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        assert_eq!(
            changed,
            "event: change\ndata: {\"resource\":\"layout\",\"name\":\"Day\"}\n\n"
        );
        assert_eq!(
            f.api("GET", "/layouts", json!(null)).await.1[0]["name"],
            "Day"
        );
        drop(body);
        driver.abort();
        let _ = driver.await;
    }
    f.stop().await;
    assert!(service.wait().await.unwrap().success());
}

#[tokio::test]
async fn board_and_item_cli_roundtrip_names_patches_archives_and_errors() {
    let f = Fixture::new();
    let out = f.cli(&["board", "add", "inbox"], "").await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let board = json_output(&out)["id"].as_i64().unwrap().to_string();
    let out = f.cli(&["board", "add", "Next / day"], "").await;
    assert!(out.status.success());
    let next = json_output(&out)["id"].as_i64().unwrap().to_string();
    assert_eq!(
        json_output(&f.cli(&["boards"], "").await)
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let out = f
        .cli(
            &[
                "todo",
                "add",
                "inbox",
                "Reply",
                "--body",
                "Details",
                "--url",
                "https://example.test",
            ],
            "",
        )
        .await;
    assert!(out.status.success());
    let todo = json_output(&out)["id"].as_i64().unwrap().to_string();
    let out = f
        .cli(
            &[
                "note", "add", &board, "Body", "--title", "Note", "--color", "blue",
            ],
            "",
        )
        .await;
    assert!(out.status.success());
    let note = json_output(&out)["id"].as_i64().unwrap().to_string();
    assert_eq!(
        json_output(&f.cli(&["todo", "done", &todo], "").await)["done"],
        true
    );
    assert_eq!(
        json_output(&f.cli(&["todo", "done", &todo, "--undone"], "").await)["done"],
        false
    );
    let edited = f
        .cli(
            &["todo", "patch", &todo],
            r#"{"title":"Edited","body":null}"#,
        )
        .await;
    assert!(edited.status.success());
    assert_eq!(json_output(&edited)["body"], Value::Null);
    assert_eq!(json_output(&edited)["url"], "https://example.test");
    let edited = f
        .cli(
            &["note", "patch", &note],
            r#"{"title":null,"body":"Changed"}"#,
        )
        .await;
    assert!(edited.status.success());
    assert_eq!(json_output(&edited)["title"], Value::Null);
    assert!(
        f.cli(&["todo", "move", &todo, "Next / day"], "")
            .await
            .status
            .success()
    );
    assert!(
        f.cli(&["note", "archive", &note], "")
            .await
            .status
            .success()
    );
    let archive = json_output(&f.cli(&["board", "archive", "inbox"], "").await);
    assert_eq!(archive["notes"][0]["body"], "Changed");
    assert!(
        f.cli(&["note", "restore", &note, &next], "")
            .await
            .status
            .success()
    );
    assert!(
        f.cli(&["board", "rename", "inbox", "Renamed"], "")
            .await
            .status
            .success()
    );
    assert!(
        f.cli(&["board", "rm", "Renamed"], "")
            .await
            .status
            .success()
    );
    let failed = f.cli(&["board", "rm", &next], "").await;
    assert!(!failed.status.success());
    assert!(failed.stdout.is_empty());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("409"));
    let failed = f.cli(&["board", "get", "missing"], "").await;
    assert!(!failed.status.success());
    let before = json_output(&f.cli(&["board", "get", &next], "").await);
    assert_eq!(before["todos"][0]["title"], "Edited");
    assert!(
        f.cli(&["board", "rm", &next, "--archive-contents"], "")
            .await
            .status
            .success()
    );
    let archive = json_output(&f.cli(&["archive"], "").await);
    assert_eq!(archive["todos"][0]["title"], "Edited");
    assert!(f.cli(&["todo", "rm", &todo], "").await.status.success());
    assert!(f.cli(&["note", "rm", &note], "").await.status.success());
    f.stop().await;
    assert!(
        !f.cli(&["--no-auto-start", "boards"], "")
            .await
            .status
            .success()
    );
    assert!(!f.paths.socket().exists());
}

#[tokio::test]
async fn invalid_board_cli_input_does_not_start_service() {
    let f = Fixture::new();
    for (args, input) in [
        (vec!["board", "add", "  "], String::new()),
        (vec!["board", "get", "1"], String::new()),
        (vec!["todo", "done", "0"], String::new()),
        (vec!["todo", "patch", "1"], "{\"done\":null}".into()),
        (vec!["note", "patch", "1"], "{\"unknown\":true}".into()),
        (vec!["todo", "patch", "1"], "{\"position\":-1}".into()),
        (vec!["todo", "patch", "1"], "x".repeat(65537)),
        (
            vec!["todo", "patch", "1"],
            json!({"title":"x".repeat(501)}).to_string(),
        ),
    ] {
        let out = f.cli(&args, &input).await;
        assert!(!out.status.success(), "{args:?}");
        assert!(out.stdout.is_empty());
        assert!(!f.paths.socket().exists());
    }
    let title = "x".repeat(501);
    assert!(
        !f.cli(&["todo", "add", "inbox", &title], "")
            .await
            .status
            .success()
    );
    assert!(!f.paths.socket().exists());
}

#[tokio::test]
async fn feed_controls_promotion_and_layout_cli_roundtrip_literal_keys() {
    let f = Fixture::new();
    let out = f.cli(&["board", "add", "Inbox / work"], "").await;
    assert!(out.status.success());
    let board = json_output(&out)["id"].as_i64().unwrap();
    let key = "https://example.test/a?q=é+%2F#frag";
    let snapshot = json!({"items":[{"key":"first","title":"First"},{"key":key,"title":"Source","body":"Copied"}]}).to_string();
    assert!(f.cli(&["put", "work"], &snapshot).await.status.success());
    let patched = f
        .cli(
            &["feed", "patch", "work", key],
            r#"{"position":0,"wake_on_update":true}"#,
        )
        .await;
    assert!(
        patched.status.success(),
        "{}",
        String::from_utf8_lossy(&patched.stderr)
    );
    let update = json_output(&patched);
    assert_eq!(update["manual_order"], true);
    assert_eq!(update["state"]["snoozed"], true);
    assert_eq!(
        json_output(&f.cli(&["get", "work"], "").await)["items"][0]["key"],
        key
    );
    let patched = f
        .cli(
            &["feed", "patch", "work", key],
            r#"{"snoozed_until_ms":null,"wake_on_update":false,"reset_order":true}"#,
        )
        .await;
    assert!(patched.status.success());
    assert_eq!(json_output(&patched)["state"]["snoozed"], false);
    assert_eq!(json_output(&patched)["manual_order"], false);
    for args in [
        vec!["feed", "promote", "work", key, "Inbox / work"],
        vec![
            "feed",
            "promote",
            "work",
            key,
            "Inbox / work",
            "--kind",
            "note",
        ],
    ] {
        let out = f.cli(&args, "").await;
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(json_output(&out)["reference"]["key"], key);
        assert_eq!(json_output(&out)["board_id"], board);
    }
    let failed = f
        .cli(&["feed", "promote", "work", "missing", "Inbox / work"], "")
        .await;
    assert!(!failed.status.success());
    assert!(failed.stdout.is_empty());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("404"));
    let name = "Day / é+%2F?#";
    let layout = json!({"tree":{"kind":"board","id":board}}).to_string();
    let out = f.cli(&["layout", "save", name], &layout).await;
    assert!(out.status.success());
    assert_eq!(json_output(&out)["name"], name);
    assert_eq!(
        json_output(&f.cli(&["layouts"], "").await)[0]["tree"]["id"],
        board
    );
    assert!(
        f.cli(&["layout", "save", name], r#"{"tree":{"kind":"empty"}}"#)
            .await
            .status
            .success()
    );
    let layouts = json_output(&f.cli(&["layouts"], "").await);
    assert_eq!(layouts.as_array().unwrap().len(), 1);
    assert_eq!(layouts[0]["tree"]["kind"], "empty");
    assert!(
        !f.cli(&["layout", "save", name], "{}")
            .await
            .status
            .success()
    );
    assert_eq!(json_output(&f.cli(&["layouts"], "").await), layouts);
    let spare = f.cli(&["layout", "save", "Spare"], &layout).await;
    assert!(spare.status.success());
    let taken = f.cli(&["layout", "rename", name, "Spare"], "").await;
    assert!(!taken.status.success());
    assert!(String::from_utf8_lossy(&taken.stderr).contains("409"));
    let evening = "Evening %/?";
    let out = f.cli(&["layout", "rename", name, evening], "").await;
    assert!(out.status.success());
    assert_eq!(json_output(&out)["name"], evening);
    let out = f.cli(&["layout", "rm", evening], "").await;
    assert_eq!(json_output(&out), json!({"deleted":true}));
    let out = f.cli(&["layout", "rm", "Spare"], "").await;
    assert_eq!(json_output(&out), json!({"deleted":true}));
    assert_eq!(json_output(&f.cli(&["layouts"], "").await), json!([]));
    f.stop().await;
    assert!(
        !f.cli(&["--no-auto-start", "layouts"], "")
            .await
            .status
            .success()
    );
    // A valid save can also start the service and recover the existing database.
    assert!(
        f.cli(&["layout", "save", name], &layout)
            .await
            .status
            .success()
    );
    f.stop().await;
}

#[tokio::test]
async fn invalid_view_cli_input_never_starts_service() {
    let f = Fixture::new();
    for (args, input) in [
        (
            vec!["feed", "patch", "work", "key"],
            r#"{"position":-1}"#.to_owned(),
        ),
        (
            vec!["feed", "patch", "work", "key"],
            r#"{"position":0,"reset_order":true}"#.to_owned(),
        ),
        (
            vec!["feed", "patch", "work", "key"],
            r#"{"wake_on_update":null}"#.to_owned(),
        ),
        (
            vec!["feed", "patch", "work", "key"],
            r#"{"typo":true}"#.to_owned(),
        ),
        (vec!["feed", "patch", "work", "key"], " ".repeat(16385)),
        (vec!["feed", "patch", "bad/name", "key"], "{}".to_owned()),
        (vec!["feed", "promote", "work", "key", "1"], String::new()),
        (
            vec!["feed", "promote", "work", "key", "2", "--kind", "invalid"],
            String::new(),
        ),
        (vec!["layout", "save", " bad"], String::new()),
        (vec!["layout", "rename", "Day", " bad"], String::new()),
        (vec!["layout", "rename", "", "Day"], String::new()),
        (vec!["layout", "rm", "bad\n"], String::new()),
        (vec!["layout", "save", "Day"], "{}".to_owned()),
        (
            vec!["layout", "save", "Day"],
            r#"{"tree":{"kind":"tabs","children":[],"active":0}}"#.to_owned(),
        ),
        (vec!["layout", "save", "Day"], " ".repeat(65537)),
    ] {
        let out = f.cli(&args, &input).await;
        assert!(!out.status.success(), "{args:?}");
        assert!(out.stdout.is_empty());
        assert!(!f.paths.socket().exists());
    }
}
