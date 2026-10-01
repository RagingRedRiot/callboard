#![cfg(target_os = "linux")]
//! `callboard upgrade` and `callboard uninstall` against real processes
//! (DESIGN.md §7.4), each in an isolated deployment with its own copy of the
//! binary so it can be replaced.
use callboard::{
    client,
    lifecycle::{Environment, Paths},
};
use serde_json::Value;
use std::{
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Deployment {
    root: tempfile::TempDir,
    paths: Paths,
}

impl Deployment {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        std::fs::create_dir(root.path().join("bin")).unwrap();
        let paths = Paths::resolve(&Environment {
            data_home: Some(root.path().join("data")),
            config_home: Some(root.path().join("config")),
            socket_dir: Some(root.path().join("run")),
            ..Default::default()
        })
        .unwrap();
        let deployment = Self { root, paths };
        deployment.install_build();
        deployment
    }

    fn binary(&self) -> PathBuf {
        self.root.path().join("bin/callboard")
    }

    /// Install a fresh copy of the real binary by rename, as `cargo install`
    /// does: a new inode at the same path.
    fn install_build(&self) {
        let staged = self.root.path().join("bin/.callboard.new");
        std::fs::copy(env!("CARGO_BIN_EXE_callboard"), &staged).unwrap();
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&staged, self.binary()).unwrap();
    }

    fn install_script(&self, script: &str) {
        let staged = self.root.path().join("bin/.callboard.new");
        std::fs::write(&staged, script).unwrap();
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&staged, self.binary()).unwrap();
    }

    fn command(&self, program: &Path) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(program);
        cmd.env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("CALLBOARD_SOCKET_DIR", self.root.path().join("run"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }

    /// Run the test build of the CLI, which is not the deployment's binary.
    async fn cli(&self, args: &[&str]) -> Output {
        let child = self
            .command(Path::new(env!("CARGO_BIN_EXE_callboard")))
            .args(args)
            .spawn()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
            .await
            .unwrap()
            .unwrap()
    }

    async fn serve(&self) -> tokio::process::Child {
        let child = self
            .command(&self.binary())
            .arg("serve")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        for _ in 0..250 {
            if self.health().await.is_some() {
                return child;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("service did not start");
    }

    async fn health(&self) -> Option<Value> {
        let (status, body) = client::request(&self.paths, "GET", "/health", vec![], None)
            .await
            .ok()?;
        status
            .is_success()
            .then(|| serde_json::from_slice(&body).unwrap())
    }

    async fn api(&self, method: &str, path: &str, body: &str) -> Value {
        let (status, body) =
            client::request(&self.paths, method, path, body.as_bytes().to_vec(), None)
                .await
                .unwrap();
        assert!(status.is_success(), "{status}");
        serde_json::from_slice(&body).unwrap()
    }
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap()
}

fn same_file(a: impl AsRef<Path>, b: impl AsRef<Path>) -> bool {
    let (a, b) = (std::fs::metadata(a).unwrap(), std::fs::metadata(b).unwrap());
    (a.dev(), a.ino()) == (b.dev(), b.ino())
}

#[tokio::test]
async fn upgrade_replaces_the_image_in_place_without_dropping_requests() {
    let deployment = Deployment::new();
    let mut service = deployment.serve().await;
    let pid = service.id().unwrap() as i32;
    let health = deployment.health().await.unwrap();
    assert_eq!(health["version"], callboard::VERSION);
    assert_eq!(health["build"], callboard::BUILD);
    deployment
        .api("POST", "/boards", r#"{"name":"Inbox"}"#)
        .await;

    let current = deployment.cli(&["upgrade"]).await;
    assert!(current.status.success(), "{}", text(&current.stderr));
    assert!(text(&current.stdout).contains("already running"));

    // Clients keep working throughout: connections made during the drain
    // wait in the backlog for the new image.
    let stop = Arc::new(AtomicBool::new(false));
    let (served, failed) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let traffic = tokio::spawn({
        let (paths, stop) = (deployment.paths.clone(), stop.clone());
        let (served, failed) = (served.clone(), failed.clone());
        async move {
            while !stop.load(Ordering::SeqCst) {
                match client::request(&paths, "GET", "/boards", vec![], None).await {
                    Ok((status, _)) if status.is_success() => served.fetch_add(1, Ordering::SeqCst),
                    _ => failed.fetch_add(1, Ordering::SeqCst),
                };
            }
        }
    });
    let (_, mut events) = client::stream(&deployment.paths, "/events", None)
        .await
        .unwrap();
    events.chunk().await.unwrap().unwrap();

    deployment.install_build();
    let upgraded = deployment.cli(&["upgrade"]).await;
    assert!(upgraded.status.success(), "{}", text(&upgraded.stderr));
    assert!(
        text(&upgraded.stdout).contains(&format!("Upgraded the service (pid {pid})")),
        "{}",
        text(&upgraded.stdout)
    );
    // The same process now runs the newly installed file.
    assert!(same_file(format!("/proc/{pid}/exe"), deployment.binary()));
    // Open event streams end promptly rather than at their 25-second rotation.
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while events.chunk().await.unwrap().is_some() {}
    })
    .await;
    assert!(ended.is_ok(), "event stream outlived the upgrade");

    tokio::time::sleep(Duration::from_millis(200)).await;
    stop.store(true, Ordering::SeqCst);
    traffic.await.unwrap();
    assert_eq!(failed.load(Ordering::SeqCst), 0);
    assert!(served.load(Ordering::SeqCst) > 0);
    let boards = deployment.api("GET", "/boards", "").await;
    assert!(boards.to_string().contains("Inbox"), "{boards}");

    // A second upgrade works from the re-executed image too.
    deployment.install_build();
    let again = deployment.cli(&["upgrade"]).await;
    assert!(again.status.success(), "{}", text(&again.stderr));
    assert!(same_file(format!("/proc/{pid}/exe"), deployment.binary()));

    // A binary that cannot run is refused, and the service carries on.
    deployment.install_script("#!/bin/sh\necho broken >&2\nexit 3\n");
    let broken = deployment.cli(&["upgrade"]).await;
    assert!(!broken.status.success());
    let stderr = text(&broken.stderr);
    assert!(
        stderr.contains("upgrade abandoned") && stderr.contains("broken"),
        "{stderr}"
    );
    assert!(deployment.health().await.is_some());

    // Stopping still cleans up after re-executions.
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(pid).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    let status = tokio::time::timeout(Duration::from_secs(10), service.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    assert!(!deployment.paths.socket().exists());
}

#[tokio::test]
async fn upgrade_without_a_service_starts_nothing() {
    let deployment = Deployment::new();
    let output = deployment.cli(&["upgrade"]).await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(text(&output.stdout).contains("No service is running"));
    assert!(!deployment.paths.socket().exists());
    assert!(!deployment.paths.data_dir().exists());
}

#[tokio::test]
async fn forged_handoff_descriptors_are_rejected() {
    let deployment = Deployment::new();
    for fds in ["99,98", "1,2", "x", "5,5"] {
        let output = deployment
            .command(&deployment.binary())
            .args(["serve", "--handoff", fds])
            .output()
            .await
            .unwrap();
        assert!(!output.status.success(), "accepted --handoff {fds}");
    }
    assert!(!deployment.paths.socket().exists());
}

#[tokio::test]
async fn uninstall_removes_service_and_data_and_keeps_config_unless_purged() {
    let deployment = Deployment::new();
    let mut service = deployment.serve().await;
    deployment
        .api("POST", "/boards", r#"{"name":"Inbox"}"#)
        .await;
    let config = deployment.paths.config_dir().to_path_buf();
    std::fs::write(config.join("mcp.toml"), "").unwrap();

    // Off a terminal, nothing happens without --yes.
    let refused = deployment.cli(&["uninstall"]).await;
    assert!(!refused.status.success());
    assert!(text(&refused.stderr).contains("--yes"));
    assert!(text(&refused.stdout).contains("1 board"));
    assert!(deployment.health().await.is_some());

    let output = deployment.cli(&["uninstall", "--yes"]).await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    let status = tokio::time::timeout(Duration::from_secs(10), service.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    assert!(!deployment.paths.data_dir().exists());
    assert!(!deployment.paths.socket().exists());
    assert!(config.join("mcp.toml").exists());

    // Nothing left to stop; --purge takes the config too.
    let purged = deployment.cli(&["uninstall", "--yes", "--purge"]).await;
    assert!(purged.status.success(), "{}", text(&purged.stderr));
    assert!(!config.exists());
    assert!(
        !deployment.paths.data_dir().exists(),
        "uninstall auto-started a service"
    );
}

#[tokio::test]
async fn uninstall_refuses_a_custom_unit() {
    let deployment = Deployment::new();
    let units = deployment.root.path().join("config/systemd/user");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&units)
        .unwrap();
    std::fs::write(
        units.join("callboard.service"),
        "[Service]\nExecStart=/bin/true\n",
    )
    .unwrap();
    let mut service = deployment.serve().await;
    let output = deployment.cli(&["uninstall", "--yes"]).await;
    assert!(!output.status.success());
    assert!(text(&output.stderr).contains("custom unit"));
    assert!(deployment.health().await.is_some());
    assert!(units.join("callboard.service").exists());
    let status = deployment.cli(&["setup", "--status"]).await;
    assert!(text(&status.stdout).contains("custom unit"));
    service.kill().await.unwrap();
}

#[tokio::test]
async fn setup_status_without_a_unit_names_on_demand_startup() {
    let deployment = Deployment::new();
    let status = deployment.cli(&["setup", "--status"]).await;
    assert!(status.status.success(), "{}", text(&status.stderr));
    assert!(text(&status.stdout).contains("starts on demand"));
    let removed = deployment.cli(&["setup", "--uninstall"]).await;
    assert!(removed.status.success(), "{}", text(&removed.stderr));
    assert!(text(&removed.stdout).contains("No unit is installed"));
}
