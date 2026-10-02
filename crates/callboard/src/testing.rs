//! An isolated in-process service for backend and event tests.
use callboard_service::{
    client,
    lifecycle::{Environment, Paths},
    server,
};
use std::{os::unix::fs::PermissionsExt, time::Duration};

pub struct Service {
    _root: tempfile::TempDir,
    pub paths: Paths,
}

pub struct Running {
    paths: Paths,
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Service {
    pub fn new() -> Self {
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
        Self { _root: root, paths }
    }

    /// Serve until [`Running::stop`]; can be called again after stopping.
    pub async fn start(&self) -> Running {
        let (stop, shutdown) = tokio::sync::oneshot::channel();
        let paths = self.paths.clone();
        let task = tokio::spawn(async move {
            server::serve(paths, server::Start::default(), async {
                let _ = shutdown.await;
            })
            .await
            .unwrap();
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            while client::request(&self.paths, "GET", "/health", vec![], None)
                .await
                .is_err()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        Running {
            paths: self.paths.clone(),
            stop,
            task,
        }
    }
}

impl Running {
    pub async fn send(
        &self,
        method: &str,
        path: &str,
        value: serde_json::Value,
    ) -> serde_json::Value {
        let (status, bytes) = client::request(
            &self.paths,
            method,
            path,
            serde_json::to_vec(&value).unwrap(),
            None,
        )
        .await
        .unwrap();
        assert!(status.is_success(), "{method} {path}: {status}");
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    pub async fn stop(self) {
        self.stop.send(()).unwrap();
        self.task.await.unwrap();
    }
}

/// A minimal HTTP responder on the service socket, for service versions the
/// real server no longer matches. `route` maps a path to (status, body).
pub async fn serve_fake(
    paths: &Paths,
    route: fn(&str) -> (u16, &'static str),
) -> tokio::task::JoinHandle<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = paths.socket_dir();
    std::fs::create_dir_all(dir).unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let listener = tokio::net::UnixListener::bind(paths.socket()).unwrap();
    std::fs::set_permissions(paths.socket(), std::fs::Permissions::from_mode(0o600)).unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut request = Vec::new();
            let mut buf = [0; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            let line = String::from_utf8_lossy(&request);
            let path = line.split_whitespace().nth(1).unwrap_or("/");
            let (status, body) = route(path);
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    })
}
