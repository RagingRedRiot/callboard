//! A single-request Unix HTTP client. Requests are never replayed after sending.
use crate::{
    Error,
    lifecycle::{LifecycleError, Paths},
    server::authenticate,
};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Request, StatusCode, body::Bytes};
use hyper_util::rt::TokioIo;
use std::{io, path::Path, process::Stdio, time::Duration};
use tokio::net::UnixStream;

async fn connect(paths: &Paths) -> Result<UnixStream, Error> {
    paths.check_socket()?;
    let stream =
        tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(paths.socket())).await??;
    authenticate(&stream, rustix::process::geteuid().as_raw())?;
    Ok(stream)
}
fn absent(e: &Error) -> bool {
    let io = e
        .downcast_ref::<io::Error>()
        .or_else(|| match e.downcast_ref::<LifecycleError>() {
            Some(LifecycleError::Io { source, .. }) => Some(source),
            _ => None,
        });
    io.is_some_and(|e| {
        matches!(
            e.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        )
    })
}

async fn connection(paths: &Paths, auto_start: Option<&Path>) -> Result<UnixStream, Error> {
    match connect(paths).await {
        Ok(stream) => return Ok(stream),
        Err(e) if auto_start.is_some() && absent(&e) => (),
        Err(e) => return Err(e),
    }
    let mut child = tokio::process::Command::new(auto_start.unwrap())
        .arg("serve")
        .env("XDG_DATA_HOME", paths.data_dir().parent().unwrap())
        .env("XDG_CONFIG_HOME", paths.config_dir().parent().unwrap())
        .env("CALLBOARD_SOCKET_DIR", paths.socket_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    // A racing child may lose the lock while the winner is still initializing.
    // Wait for the socket even after our child exits; never retry an HTTP write.
    let wait = async {
        loop {
            match connect(paths).await {
                Ok(stream) => return Ok(stream),
                Err(e) if absent(&e) => (),
                // bind() and chmod() are separate syscalls. Never connect with
                // unsafe permissions, but allow our starting child to finish.
                Err(e)
                    if matches!(e.downcast_ref::<LifecycleError>(),
                    Some(LifecycleError::UnsafePath { path, .. }) if *path == paths.socket()) => {}
                Err(e) => return Err(e),
            }
            let _ = child.try_wait()?;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    match tokio::time::timeout(Duration::from_secs(5), wait).await {
        Ok(result) => result,
        Err(_) => Err("service did not start within 5 seconds; run `callboard serve` to see the startup error (check for a service using the same data directory and a different socket)".into()),
    }
}

pub async fn request(
    paths: &Paths,
    method: &str,
    resource: &str,
    body: Vec<u8>,
    auto_start: Option<&Path>,
) -> Result<(StatusCode, Vec<u8>), Error> {
    let stream = connection(paths, auto_start).await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        let task = tokio::spawn(async move {
            let _ = connection.await;
        });
        // Abort the connection driver on all exits, including timeout/cancellation.
        struct Driver(tokio::task::JoinHandle<()>);
        impl Drop for Driver {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _driver = Driver(task);
        let request = Request::builder()
            .method(method)
            .uri(resource)
            .header("host", "localhost")
            .header("content-type", "application/json")
            .header("connection", "close")
            .body(Full::new(Bytes::from(body)))?;
        let response = sender.send_request(request).await?;
        let status = response.status();
        let body = Limited::new(response.into_body(), 16 * 1024 * 1024)
            .collect()
            .await?
            .to_bytes();
        Ok::<_, Error>((status, body.to_vec()))
    })
    .await?
}
