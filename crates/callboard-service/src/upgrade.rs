//! `callboard upgrade` (DESIGN.md §7.4): move the running service onto the
//! binary installed at its path, in place. The service side lives in
//! `server::serve`; this module holds its preflight and the client.
use crate::{Error, client, lifecycle::Paths};
use std::{
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// How long the candidate binary gets to answer `--version`.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the client waits for the re-executed service, which runs
/// migrations at startup.
const RETURN_TIMEOUT: Duration = Duration::from_secs(120);

/// The path this process was started from, which an upgrade re-executes.
/// After a replaced binary's fallback exec through `/proc/self/exe`, the
/// path reads "… (deleted)"; the install still lives at the path without it.
pub fn running_executable() -> Option<PathBuf> {
    let path = std::env::current_exe().ok()?;
    let path = match path.as_os_str().as_bytes().strip_suffix(b" (deleted)") {
        Some(path) => PathBuf::from(std::ffi::OsStr::from_bytes(path)),
        None => path,
    };
    path.canonicalize().ok()
}

pub(crate) enum Preflight {
    /// The installed binary is the image already running.
    Current,
    /// There is nothing runnable to become; nothing was changed.
    Unusable(String),
}

/// Before stopping anything: is there a different, runnable binary to become?
pub(crate) async fn preflight(executable: &Path) -> Result<(), Preflight> {
    use std::os::unix::fs::PermissionsExt;
    let installed = std::fs::metadata(executable).map_err(|e| {
        Preflight::Unusable(format!(
            "nothing to upgrade to at {}: {e}",
            executable.display()
        ))
    })?;
    if !installed.is_file() || installed.permissions().mode() & 0o111 == 0 {
        return Err(Preflight::Unusable(format!(
            "{} is not an executable file",
            executable.display()
        )));
    }
    // The magic link names the running image even after its path was replaced.
    if let Ok(running) = std::fs::metadata("/proc/self/exe")
        && (running.dev(), running.ino()) == (installed.dev(), installed.ino())
    {
        return Err(Preflight::Current);
    }
    // Wrong architecture, missing libraries, a half-written file: find out
    // while backing off costs nothing.
    let probe = tokio::process::Command::new(executable)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    let failure = match tokio::time::timeout(PREFLIGHT_TIMEOUT, probe).await {
        Ok(Ok(output)) if output.status.success() => return Ok(()),
        Ok(Ok(output)) => format!(
            "{} ({})",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Ok(Err(e)) => e.to_string(),
        Err(_) => format!("no answer within {} s", PREFLIGHT_TIMEOUT.as_secs()),
    };
    Err(Preflight::Unusable(format!(
        "{} does not run: `--version` failed: {failure}",
        executable.display()
    )))
}

#[derive(serde::Deserialize)]
struct Reply {
    executable: Option<PathBuf>,
    reason: Option<String>,
}

pub async fn run(paths: &Paths) -> Result<(), Error> {
    // Never auto-start: a fresh service would already be the installed build.
    let (status, body) =
        match client::request(paths, "POST", "/service/upgrade", vec![], None).await {
            Ok(response) => response,
            Err(e) if client::absent(&e) => {
                println!(
                    "No service is running; the next callboard command starts the installed binary."
                );
                report_guis();
                return Ok(());
            }
            Err(e) => return Err(e),
        };
    let reply: Option<Reply> = serde_json::from_slice(&body).ok();
    let executable = reply.as_ref().and_then(|r| r.executable.clone());
    match (status.as_u16(), executable) {
        (404, _) => {
            return Err(
                "the running service predates `callboard upgrade`, so it cannot be \
                 upgraded in place. Restart it once (`systemctl --user restart \
                 callboard.service` with the systemd unit, otherwise stop `callboard serve` \
                 and run any callboard command); later upgrades will work"
                    .into(),
            );
        }
        (409, _) => {
            let reason = reply.and_then(|r| r.reason).unwrap_or_default();
            return Err(format!("upgrade abandoned: {reason}").into());
        }
        (200, Some(executable)) => {
            println!(
                "The service is already running the binary installed at {}.",
                executable.display()
            );
        }
        (202, Some(executable)) => {
            let pid = await_return(paths).await?;
            // Answering is not enough: a service that could not execute the
            // new build falls back to its old image, which answers too.
            if !runs_binary(pid, &executable) {
                return Err(format!(
                    "the service could not start {} and is still running the previous build; \
                     run `callboard serve` with that binary to see why",
                    executable.display()
                )
                .into());
            }
            println!(
                "Upgraded the service (pid {pid}) in place to {}.",
                executable.display()
            );
            if let Some(ours) = running_executable()
                && ours != executable
            {
                println!(
                    "Note: this CLI is {}, not the service's binary.",
                    ours.display()
                );
            }
        }
        _ => {
            return Err(format!("HTTP {status}: {}", String::from_utf8_lossy(&body)).into());
        }
    }
    report_guis();
    Ok(())
}

/// The listener never closed, so this connects at once and gets an answer
/// when the new image is serving. Returns the serving process's PID.
async fn await_return(paths: &Paths) -> Result<i32, Error> {
    let deadline = Instant::now() + RETURN_TIMEOUT;
    loop {
        let attempt = async {
            let pid = client::serving_pid(paths).await?;
            let (status, _) = client::request(paths, "GET", "/health", vec![], None).await?;
            if !status.is_success() {
                return Err::<_, Error>(format!("/health returned HTTP {status}").into());
            }
            Ok(pid)
        };
        match attempt.await {
            Ok(pid) => return Ok(pid),
            Err(e) if Instant::now() >= deadline => {
                return Err(format!(
                    "the service did not come back within {} s: {e}",
                    RETURN_TIMEOUT.as_secs()
                )
                .into());
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// Whether `pid` executes the file now at `executable`, compared by inode,
/// since a replaced binary keeps running under its old path.
fn runs_binary(pid: i32, executable: &Path) -> bool {
    match (
        std::fs::metadata(format!("/proc/{pid}/exe")),
        std::fs::metadata(executable),
    ) {
        (Ok(running), Ok(installed)) => {
            (running.dev(), running.ino()) == (installed.dev(), installed.ino())
        }
        _ => false,
    }
}

/// An open `callboard-gui` window of this user.
pub(crate) struct Gui {
    pub pid: i32,
    /// Its binary was replaced since it started.
    pub outdated: bool,
}

/// Open GUI processes of this user, found through `/proc`.
pub(crate) fn guis() -> Vec<Gui> {
    let uid = rustix::process::geteuid().as_raw();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return vec![];
    };
    let mut found: Vec<Gui> = entries
        .flatten()
        .filter_map(|entry| {
            let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
            if entry.metadata().ok()?.uid() != uid {
                return None;
            }
            let exe = std::fs::read_link(entry.path().join("exe")).ok()?;
            let name = exe.file_name()?.as_bytes();
            let (name, outdated) = match name.strip_suffix(b" (deleted)") {
                Some(name) => (name, true),
                None => (name, false),
            };
            (name == b"callboard-gui").then_some(Gui { pid, outdated })
        })
        .collect();
    found.sort_by_key(|gui| gui.pid);
    found
}

fn report_guis() {
    let guis = guis();
    if guis.is_empty() {
        return;
    }
    let pids: Vec<_> = guis.iter().map(|gui| gui.pid.to_string()).collect();
    println!(
        "callboard-gui is open (pid {}); reopen it to load a new GUI build{}.",
        pids.join(", "),
        if guis.iter().any(|gui| gui.outdated) {
            " (its binary has been replaced)"
        } else {
            ""
        }
    );
}
