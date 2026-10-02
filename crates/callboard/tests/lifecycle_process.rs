#![cfg(target_os = "linux")]
// Keep subprocess creation separate from parallel socket/lock lifetime tests:
// fork can temporarily retain their descriptors until the child reaches exec.
use callboard_service::lifecycle::{Environment, LifecycleError, Paths, ServiceGuard};
use std::fs::Permissions;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};

fn private_tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .permissions(Permissions::from_mode(0o700))
        .tempdir()
        .unwrap()
}

fn environment(root: &Path) -> Environment {
    Environment {
        home: Some(root.join("home")),
        data_home: Some(root.join("data")),
        config_home: Some(root.join("config")),
        runtime_dir: Some(root.join("missing-runtime")),
        socket_dir: Some(root.join("run")),
    }
}

// Invoked as a subprocess by the lock/crash test; inert in the normal test run.
#[test]
fn child_service() {
    let Some(root) = std::env::var_os("CALLBOARD_TEST_ROOT") else {
        return;
    };
    let paths = Paths::resolve(&environment(Path::new(&root))).unwrap();
    if std::env::var_os("CALLBOARD_TEST_CONTEND").is_some() {
        assert!(matches!(
            ServiceGuard::bind(paths),
            Err(LifecycleError::AlreadyRunning(_))
        ));
        return;
    }
    let _guard = ServiceGuard::bind(paths).unwrap();
    println!("READY");
    loop {
        std::thread::park();
    }
}

struct ChildCleanup(Child);
impl Drop for ChildCleanup {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn cross_process_lock_and_crash_recovery() {
    let root = private_tempdir();
    let paths = Paths::resolve(&environment(root.path())).unwrap();
    let executable = std::env::current_exe().unwrap();
    let mut child = ChildCleanup(
        Command::new(&executable)
            .args(["--exact", "child_service", "--nocapture"])
            .env("CALLBOARD_TEST_ROOT", root.path())
            .env_remove("CALLBOARD_TEST_CONTEND")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut reader = BufReader::new(child.0.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        assert_ne!(
            reader.read_line(&mut line).unwrap(),
            0,
            "child exited before startup"
        );
        if line.trim() == "READY" {
            break;
        }
    }
    let contender = Command::new(&executable)
        .args(["--exact", "child_service", "--nocapture"])
        .env("CALLBOARD_TEST_ROOT", root.path())
        .env("CALLBOARD_TEST_CONTEND", "1")
        .output()
        .unwrap();
    assert!(
        contender.status.success(),
        "{}",
        String::from_utf8_lossy(&contender.stderr)
    );
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert!(
        paths.socket().exists(),
        "crash leaves the socket entry behind"
    );
    let recovered = ServiceGuard::bind(paths.clone()).unwrap();
    assert!(UnixStream::connect(paths.socket()).is_ok());
    drop(recovered);
}
