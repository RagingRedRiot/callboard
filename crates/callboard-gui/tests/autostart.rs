#![cfg(target_os = "linux")]
use callboard::{
    client,
    lifecycle::{Environment, Paths},
};
use callboard_gui::{
    backend::{self, Contents, List, ListData, Request, Target},
    events::{self, Signal},
};
use std::{
    os::unix::{fs::PermissionsExt, net::UnixStream},
    time::Duration,
};

fn lists_and(targets: &[Target]) -> Request {
    Request {
        lists: List::ALL.into_iter().collect(),
        targets: targets.to_vec(),
    }
}

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
    let unreachable = backend::fetch(&paths, None, &lists_and(&[])).await;
    assert!(unreachable.lists.iter().all(|(_, r)| r.is_err()));
    // The event subscriber never auto-starts; only fetches do.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let events_paths = paths.clone();
    let subscriber = tokio::spawn(async move {
        events::run(&events_paths, events::Policy::default(), move |s| {
            tx.send(s).is_ok()
        })
        .await
    });
    assert!(matches!(
        rx.recv().await,
        Some(Signal::Disconnected(Some(_)))
    ));
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
    let first = backend::fetch(&paths, Some(&binary), &lists_and(&[Target::Archive])).await;
    assert!(first.lists.iter().all(|(_, r)| r.is_ok()));
    assert!(
        first
            .lists
            .iter()
            .any(|(_, r)| matches!(r, Ok(ListData::Boards(b)) if b.is_empty()))
    );
    assert!(matches!(first.targets[0].1, Ok(Contents::Board(_))));
    assert!(paths.socket().exists());
    let stream = UnixStream::connect(paths.socket()).unwrap();
    let pid = rustix::net::sockopt::socket_peercred(&stream).unwrap().pid;
    drop(stream);
    // Once the daemon is up, the subscriber connects on its next retry.
    tokio::time::timeout(Duration::from_secs(10), async {
        while rx.recv().await != Some(Signal::Connected) {}
    })
    .await
    .unwrap();
    drop(rx);
    subscriber.abort();
    let again = backend::fetch(&paths, Some(&binary), &lists_and(&[])).await;
    assert!(again.lists.iter().all(|(_, r)| r.is_ok()));
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
