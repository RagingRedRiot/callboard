#![cfg(target_os = "linux")]
use callboard::{
    client,
    lifecycle::{Environment, Paths},
};
use callboard_gui::backend::{self, Target};
use std::{
    os::unix::{fs::PermissionsExt, net::UnixStream},
    time::Duration,
};

#[tokio::test]
async fn gui_connection_starts_daemon_once_and_daemon_survives_client_exit() {
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
    assert!(backend::load(&paths, None, None).await.is_err());
    assert!(!paths.socket().exists());
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("callboard");
    assert!(
        binary.is_file(),
        "build callboard before running GUI daemon tests"
    );
    let first = backend::load(&paths, Some(&binary), Some(&Target::Archive))
        .await
        .unwrap();
    assert!(first.boards.is_empty());
    assert!(paths.socket().exists());
    let stream = UnixStream::connect(paths.socket()).unwrap();
    let pid = rustix::net::sockopt::socket_peercred(&stream).unwrap().pid;
    drop(stream);
    let again = backend::load(&paths, Some(&binary), None).await.unwrap();
    assert!(again.contents.is_none());
    let stream = UnixStream::connect(paths.socket()).unwrap();
    assert_eq!(
        rustix::net::sockopt::socket_peercred(&stream).unwrap().pid,
        pid
    );
    drop(stream);
    client::request(&paths, "GET", "/health", vec![], None)
        .await
        .unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    for _ in 0..100 {
        if !paths.socket().exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("auto-started daemon did not stop after SIGTERM");
}
