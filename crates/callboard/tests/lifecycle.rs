#![cfg(target_os = "linux")]

use std::fs::{self, DirBuilder, Permissions};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use callboard_service::lifecycle::{Environment, LifecycleError, Paths, ServiceGuard};

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

fn private_dir(path: &Path) {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .unwrap();
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o777
}

#[test]
fn default_paths_and_runtime_fallback_follow_xdg() {
    let root = private_tempdir();
    let env = Environment {
        home: Some(root.path().to_owned()),
        runtime_dir: Some(root.path().join("missing")),
        ..Default::default()
    };
    let paths = Paths::resolve(&env).unwrap();
    assert_eq!(paths.data_dir(), root.path().join(".local/share/callboard"));
    assert_eq!(paths.config_dir(), root.path().join(".config/callboard"));
    assert_eq!(paths.socket(), paths.data_dir().join("run/callboard.sock"));
    assert!(
        !paths.data_dir().exists(),
        "resolving must not create files"
    );
}

#[test]
fn existing_runtime_and_explicit_socket_directory_take_precedence() {
    let root = private_tempdir();
    let runtime = root.path().join("runtime");
    private_dir(&runtime);
    let mut env = environment(root.path());
    env.runtime_dir = Some(runtime.clone());
    env.socket_dir = None;
    let runtime_paths = Paths::resolve(&env).unwrap();
    assert_eq!(runtime_paths.socket_dir(), runtime);
    env.socket_dir = Some(root.path().join("custom"));
    let custom = Paths::resolve(&env).unwrap();
    assert_eq!(custom.socket_dir(), root.path().join("custom"));
    assert_eq!(custom.data_dir(), runtime_paths.data_dir());
    assert_eq!(custom.config_dir(), runtime_paths.config_dir());
}

#[test]
fn relative_xdg_values_are_ignored_and_explicit_bad_paths_rejected() {
    let root = private_tempdir();
    let mut env = environment(root.path());
    env.data_home = Some("relative".into());
    env.config_home = Some("".into());
    let defaults = Paths::resolve(&env).unwrap();
    assert_eq!(
        defaults.data_dir(),
        root.path().join("home/.local/share/callboard")
    );
    assert_eq!(
        defaults.config_dir(),
        root.path().join("home/.config/callboard")
    );
    env.home = None;
    assert!(matches!(
        Paths::resolve(&env),
        Err(LifecycleError::MissingHome)
    ));
    env = environment(root.path());
    env.home = None;
    assert!(
        Paths::resolve(&env).is_ok(),
        "HOME is unnecessary with explicit XDG bases"
    );
    for path in ["relative".into(), root.path().join("a/../b")] {
        env.socket_dir = Some(path);
        assert!(matches!(
            Paths::resolve(&env),
            Err(LifecycleError::UnsafePath { .. })
        ));
    }
}

#[test]
fn unsafe_runtime_is_rejected_without_falling_back_or_chmod() {
    let root = private_tempdir();
    let runtime = root.path().join("runtime");
    private_dir(&runtime);
    fs::set_permissions(&runtime, Permissions::from_mode(0o755)).unwrap();
    let mut env = environment(root.path());
    env.socket_dir = None;
    env.runtime_dir = Some(runtime.clone());
    assert!(matches!(
        Paths::resolve(&env),
        Err(LifecycleError::UnsafePath { .. })
    ));
    assert_eq!(mode(&runtime), 0o755);
}

#[test]
fn overlong_socket_path_is_rejected_before_creating_directories() {
    let root = private_tempdir();
    let mut env = environment(root.path());
    env.socket_dir = Some(root.path().join("x".repeat(108)));
    assert!(Paths::resolve(&env).is_err());
    assert!(!env.data_home.unwrap().exists());
    assert!(!env.socket_dir.unwrap().exists());
}

#[test]
fn startup_creates_private_files_and_drop_cleans_only_socket() {
    let root = private_tempdir();
    let paths = Paths::resolve(&environment(root.path())).unwrap();
    let guard = ServiceGuard::bind(paths.clone()).unwrap();
    for dir in [paths.data_dir(), paths.config_dir(), paths.socket_dir()] {
        assert_eq!(mode(dir), 0o700);
    }
    for file in [paths.database(), paths.socket(), paths.lock()] {
        assert_eq!(mode(&file), 0o600);
    }
    assert!(UnixStream::connect(paths.socket()).is_ok());
    let lock_inode = fs::metadata(paths.lock()).unwrap().ino();
    drop(guard);
    assert!(!paths.socket().exists());
    assert!(paths.database().exists());
    assert_eq!(fs::metadata(paths.lock()).unwrap().ino(), lock_inode);
    let restarted = ServiceGuard::bind(paths.clone()).unwrap();
    assert_eq!(fs::metadata(paths.lock()).unwrap().ino(), lock_inode);
    drop(restarted);
}

#[test]
fn changing_socket_directory_does_not_bypass_data_lock() {
    let root = private_tempdir();
    let mut env = environment(root.path());
    let paths = Paths::resolve(&env).unwrap();
    let first = ServiceGuard::bind(paths.clone()).unwrap();
    env.socket_dir = Some(root.path().join("run2"));
    let second_paths = Paths::resolve(&env).unwrap();
    assert!(matches!(
        ServiceGuard::bind(second_paths.clone()),
        Err(LifecycleError::AlreadyRunning(_))
    ));
    assert!(!second_paths.socket().exists());
    assert!(UnixStream::connect(paths.socket()).is_ok());
    drop(first);
}

#[test]
fn stale_owned_socket_is_replaced_but_live_socket_is_preserved() {
    let root = private_tempdir();
    let paths = Paths::resolve(&environment(root.path())).unwrap();
    private_dir(paths.socket_dir());
    let active = UnixListener::bind(paths.socket()).unwrap();
    let inode = fs::metadata(paths.socket()).unwrap().ino();
    assert!(matches!(
        ServiceGuard::bind(paths.clone()),
        Err(LifecycleError::SocketInUse(_))
    ));
    assert_eq!(fs::metadata(paths.socket()).unwrap().ino(), inode);
    drop(active);
    let service = ServiceGuard::bind(paths.clone()).unwrap();
    assert!(UnixStream::connect(paths.socket()).is_ok());
    drop(service);
}

#[test]
fn non_socket_and_symlink_entries_are_never_removed() {
    let root = private_tempdir();
    let paths = Paths::resolve(&environment(root.path())).unwrap();
    private_dir(paths.socket_dir());
    fs::write(paths.socket(), "keep me").unwrap();
    assert!(matches!(
        ServiceGuard::bind(paths.clone()),
        Err(LifecycleError::UnsafePath { .. })
    ));
    assert_eq!(fs::read_to_string(paths.socket()).unwrap(), "keep me");
    fs::remove_file(paths.socket()).unwrap();
    let target = root.path().join("target");
    fs::write(&target, "untouched").unwrap();
    symlink(&target, paths.socket()).unwrap();
    let result = ServiceGuard::bind(paths.clone()).map(|_| ());
    assert!(
        matches!(result, Err(LifecycleError::UnsafePath { .. })),
        "{result:?}"
    );
    assert!(fs::symlink_metadata(paths.socket()).unwrap().is_symlink());
    assert_eq!(fs::read_to_string(target).unwrap(), "untouched");
}

#[test]
fn shutdown_does_not_remove_a_replacement_socket() {
    let root = private_tempdir();
    let paths = Paths::resolve(&environment(root.path())).unwrap();
    let guard = ServiceGuard::bind(paths.clone()).unwrap();
    // Keep the old inode alive under another name to avoid inode reuse.
    let displaced = paths.socket_dir().join("displaced.sock");
    fs::rename(paths.socket(), &displaced).unwrap();
    let replacement = UnixListener::bind(paths.socket()).unwrap();
    drop(guard);
    assert!(UnixStream::connect(paths.socket()).is_ok());
    drop(replacement);
}

#[test]
fn unsafe_directories_and_ancestors_are_rejected_unchanged() {
    let root = private_tempdir();
    let env = environment(root.path());
    let paths = Paths::resolve(&env).unwrap();
    private_dir(paths.data_dir());
    fs::set_permissions(paths.data_dir(), Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        ServiceGuard::bind(paths.clone()),
        Err(LifecycleError::UnsafePath { .. })
    ));
    assert_eq!(mode(paths.data_dir()), 0o755);
    fs::set_permissions(paths.data_dir(), Permissions::from_mode(0o700)).unwrap();
    let parent = paths.data_dir().parent().unwrap();
    fs::set_permissions(parent, Permissions::from_mode(0o777)).unwrap();
    assert!(matches!(
        ServiceGuard::bind(paths.clone()),
        Err(LifecycleError::UnsafePath { .. })
    ));
    assert!(!paths.lock().exists());
    fs::set_permissions(parent, Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn symlink_ancestors_are_rejected_before_creating_children() {
    let root = private_tempdir();
    let target = root.path().join("target");
    private_dir(&target);
    let env = environment(root.path());
    symlink(&target, env.data_home.as_ref().unwrap()).unwrap();
    let paths = Paths::resolve(&env).unwrap();
    assert!(matches!(
        ServiceGuard::bind(paths),
        Err(LifecycleError::UnsafePath { .. })
    ));
    assert!(!target.join("callboard").exists());
}

#[test]
fn lock_database_and_sidecars_reject_symlinks_and_hard_links() {
    for name in [
        "service.lock",
        "callboard.sqlite3",
        "callboard.sqlite3-wal",
        "callboard.sqlite3-shm",
        "callboard.sqlite3-journal",
    ] {
        let root = private_tempdir();
        let paths = Paths::resolve(&environment(root.path())).unwrap();
        private_dir(paths.data_dir());
        let target = root.path().join("target");
        fs::write(&target, "preserved").unwrap();
        fs::set_permissions(&target, Permissions::from_mode(0o600)).unwrap();
        let entry = paths.data_dir().join(name);
        symlink(&target, &entry).unwrap();
        assert!(
            matches!(
                ServiceGuard::bind(paths.clone()),
                Err(LifecycleError::UnsafePath { .. })
            ),
            "{name}"
        );
        fs::remove_file(&entry).unwrap();
        fs::hard_link(&target, &entry).unwrap();
        assert!(
            matches!(
                ServiceGuard::bind(paths),
                Err(LifecycleError::UnsafePath { .. })
            ),
            "{name}"
        );
        assert_eq!(fs::read_to_string(target).unwrap(), "preserved");
    }
}

#[tokio::test]
async fn prepared_database_works_with_sqlx_and_restarts() {
    let root = private_tempdir();
    let paths = Paths::resolve(&environment(root.path())).unwrap();
    let guard = ServiceGuard::bind(paths.clone()).unwrap();
    let store = callboard_core::store::Store::open(paths.database())
        .await
        .unwrap();
    let snapshot = callboard_core::feed::parse_submission(br#"[{"key":"a","title":"A"}]"#).unwrap();
    store.submit("work", &snapshot).await.unwrap();
    for suffix in ["", "-wal", "-shm"] {
        let path = paths.data_dir().join(format!("callboard.sqlite3{suffix}"));
        if path.exists() {
            assert_eq!(mode(&path), 0o600);
        }
    }
    store.close().await;
    drop(guard);
    let restarted = ServiceGuard::bind(paths.clone()).unwrap();
    let store = callboard_core::store::Store::open(paths.database())
        .await
        .unwrap();
    assert_eq!(
        store.feed("work").await.unwrap().unwrap().items,
        snapshot.items
    );
    store.close().await;
    drop(restarted);
}

#[test]
fn handoff_adopts_only_this_deployments_lock_and_socket() {
    use std::os::fd::IntoRawFd;
    let root = private_tempdir();
    let paths = Paths::resolve(&environment(root.path())).unwrap();
    for dir in [paths.data_dir(), paths.config_dir(), paths.socket_dir()] {
        private_dir(dir);
    }
    let open_lock = || {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(paths.lock())
            .unwrap()
    };
    let listener = UnixListener::bind(paths.socket()).unwrap();
    fs::set_permissions(paths.socket(), Permissions::from_mode(0o600)).unwrap();
    let elsewhere = UnixListener::bind(root.path().join("other.sock")).unwrap();
    let stray = fs::File::create(root.path().join("stray")).unwrap();
    let handoff = |lock: i32, listener: i32| format!("{lock},{listener}");

    // Adopt takes ownership of what it is given, so each attempt gets dups.
    let dup = |fd: &dyn std::os::fd::AsFd| fd.as_fd().try_clone_to_owned().unwrap().into_raw_fd();
    let not_the_lock = ServiceGuard::adopt(paths.clone(), &handoff(dup(&stray), dup(&listener)));
    assert!(matches!(
        not_the_lock,
        Err(LifecycleError::UnsafePath { .. })
    ));
    let not_the_socket = ServiceGuard::adopt(
        paths.clone(),
        &handoff(open_lock().into_raw_fd(), dup(&elsewhere)),
    );
    assert!(matches!(
        not_the_socket,
        Err(LifecycleError::UnsafePath { .. })
    ));

    let guard = ServiceGuard::adopt(
        paths.clone(),
        &handoff(open_lock().into_raw_fd(), dup(&listener)),
    )
    .unwrap();
    UnixStream::connect(paths.socket()).unwrap();
    // The adopted lock is held: neither a fresh service nor a second
    // handoff with a different open of the lock file can take it.
    assert!(matches!(
        ServiceGuard::bind(paths.clone()),
        Err(LifecycleError::AlreadyRunning(_))
    ));
    assert!(matches!(
        ServiceGuard::adopt(
            paths.clone(),
            &handoff(open_lock().into_raw_fd(), dup(&listener))
        ),
        Err(LifecycleError::AlreadyRunning(_))
    ));
    drop(guard);
    assert!(
        !paths.socket().exists(),
        "an adopted guard still owns its socket"
    );
}
