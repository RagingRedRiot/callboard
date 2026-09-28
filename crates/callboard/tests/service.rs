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
