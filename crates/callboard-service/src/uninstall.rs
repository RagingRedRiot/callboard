//! `callboard uninstall` (DESIGN.md §7.4): remove the systemd unit, stop the
//! service, and delete its data, socket, and launcher entry; config only
//! with `--purge`.
use crate::{
    Error, client,
    desktop::{self, Entry},
    lifecycle::{Paths, try_lock},
    setup::{self, Unit},
    upgrade,
};
use rustix::{
    event::{PollFd, PollFlags, Timespec},
    process::{Pid, PidfdFlags, Signal},
};
use std::{
    io::{IsTerminal, Write},
    os::{fd::OwnedFd, unix::fs::FileTypeExt},
    path::Path,
    time::{Duration, Instant},
};

/// How long the data lock may stay held with no identified service to wait
/// for, such as one that exited between our look and its unlock.
const UNIDENTIFIED_LOCK_WAIT: Duration = Duration::from_secs(10);
/// How long a signalled service gets to stop. Shutdown drains for at most
/// 5 seconds, so a service still alive after this is wedged.
const STOP_WAIT: Duration = Duration::from_secs(30);

pub async fn run(paths: &Paths, purge: bool, yes: bool) -> Result<(), Error> {
    let unit = setup::unit_path(paths);
    let generated = match setup::read_unit(paths)? {
        Unit::Absent => false,
        Unit::Generated(_) => true,
        Unit::Custom => {
            return Err(format!(
                "{} is a custom unit, not one setup generated; disable and remove it \
                 first, or it may restart the service",
                unit.display()
            )
            .into());
        }
    };
    // Never auto-start: starting a service to delete it would create the
    // very store being removed.
    let pid = client::serving_pid(paths).await.ok();
    let count = async |resource: &str| -> Option<usize> {
        let (status, body) = client::request(paths, "GET", resource, vec![], None)
            .await
            .ok()?;
        status
            .is_success()
            .then(|| serde_json::from_slice::<Vec<serde_json::Value>>(&body).ok())
            .flatten()
            .map(|list| list.len())
    };
    let (feeds, boards) = match pid {
        Some(_) => (count("/feeds").await, count("/boards").await),
        None => (None, None),
    };
    let config = paths.config_dir();
    let launcher = matches!(desktop::read_entry(paths)?, Entry::Generated);
    let guis = upgrade::guis();

    println!("This removes callboard from this account:");
    if generated {
        println!("  - the systemd unit {}", unit.display());
    }
    if let Some(pid) = pid {
        println!("  - the running service (pid {pid})");
    }
    if paths.data_dir().exists() {
        let contents = match (feeds, boards) {
            (Some(feeds), Some(boards)) => format!(
                "{feeds} feed{}, {boards} board{} with their todos and notes, layouts, and the archive",
                plural(feeds),
                plural(boards)
            ),
            _ => "feeds, boards, todos, notes, layouts, and the archive".into(),
        };
        println!(
            "  - {} ({}): {contents}",
            paths.data_dir().display(),
            human_size(dir_size(paths.data_dir()))
        );
    }
    if launcher {
        println!(
            "  - the launcher entry {} and its icons",
            desktop::entry_path(paths).display()
        );
    }
    if !paths.socket_dir().starts_with(paths.data_dir()) && paths.socket().exists() {
        println!("  - the socket {}", paths.socket().display());
    }
    if config.exists() {
        if purge {
            println!("  - {} (your config)", config.display());
        } else {
            println!(
                "Kept:\n  - {} (your config; --purge removes it)",
                config.display()
            );
        }
    }
    if !guis.is_empty() {
        let pids: Vec<_> = guis.iter().map(|gui| gui.pid.to_string()).collect();
        println!(
            "callboard-gui is open (pid {}). Close it first: it starts a fresh, empty service \
             when it next refreshes.",
            pids.join(", ")
        );
    }

    if !yes {
        if !std::io::stdin().is_terminal() {
            return Err("not a terminal; rerun with --yes to confirm removing the above".into());
        }
        print!("\nRemove all of this? It cannot be undone. [y/N] ");
        std::io::stdout().flush()?;
        let answer = tokio::task::spawn_blocking(|| {
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer).map(|_| answer)
        })
        .await??;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("Nothing removed.");
            return Ok(());
        }
    }

    // The unit first: systemd stops a supervised service itself, and
    // nothing restarts it.
    if let Some(removed) = setup::remove(paths, true).await? {
        println!("removed {}", removed.display());
    }
    // Identified afresh rather than from the PID shown above: that service
    // may have exited since (systemd stops one itself), and its PID been reused.
    let service = Service::find(paths).await;
    if let Some(service) = &service {
        service.terminate()?;
    }
    // Holding the lock proves no service is left, and keeps one a stray
    // client auto-starts meanwhile from opening the store under us.
    let lock = if paths.data_dir().exists() {
        Some(hold_lock(paths, service.as_ref()).await?)
    } else {
        None
    };
    remove_socket(&paths.socket())?;
    if remove_owned_dir(paths.data_dir())? {
        println!("removed {}", paths.data_dir().display());
    }
    drop(lock);
    if let Some(removed) = desktop::remove(paths)? {
        println!("removed {} and its icons", removed.display());
    }
    if purge && remove_owned_dir(config)? {
        println!("removed {}", config.display());
    }

    println!("\ncallboard is uninstalled.");
    if let Some(binary) = upgrade::running_executable() {
        let gui = binary.with_file_name("callboard-gui");
        let mut binaries = binary.display().to_string();
        if gui.exists() {
            binaries.push_str(&format!(" and {}", gui.display()));
        }
        // A release archive's binaries are the user's to delete; Cargo
        // tracks only what `cargo install` put in its bin directory.
        let remove = if binary
            .parent()
            .is_some_and(|dir| dir.ends_with(".cargo/bin"))
        {
            "`cargo uninstall callboard` removes them"
        } else {
            "delete them to finish"
        };
        println!("The binaries remain at {binaries}; {remove}.");
    }
    println!("Running any callboard command starts afresh with an empty store.");
    Ok(())
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// The process serving the socket right now, held by pidfd so that
/// signalling it can never reach a process that has since taken its PID.
struct Service {
    pid: i32,
    pidfd: OwnedFd,
}

impl Service {
    async fn find(paths: &Paths) -> Option<Self> {
        let pid = client::serving_pid(paths).await.ok()?;
        let pidfd = rustix::process::pidfd_open(Pid::from_raw(pid)?, PidfdFlags::empty()).ok()?;
        // The PID could have been reused between the two calls; a second
        // look at the socket confirms the pidfd names its server.
        (client::serving_pid(paths).await.ok()? == pid).then_some(Self { pid, pidfd })
    }

    fn terminate(&self) -> Result<(), Error> {
        match rustix::process::pidfd_send_signal(&self.pidfd, Signal::TERM) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(e) => Err(format!("signalling the service (pid {}): {e}", self.pid).into()),
        }
    }

    /// A pidfd turns readable once its process has exited.
    fn alive(&self) -> bool {
        let mut fds = [PollFd::new(&self.pidfd, PollFlags::IN)];
        let now = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        rustix::event::poll(&mut fds, Some(&now)).is_ok_and(|ready| ready == 0)
    }
}

/// Wait for the service to release the data lock, then hold it.
async fn hold_lock(paths: &Paths, service: Option<&Service>) -> Result<std::fs::File, Error> {
    let started = Instant::now();
    let mut gone_since = None;
    loop {
        if let Some(lock) = try_lock(paths)? {
            return Ok(lock);
        }
        let now = Instant::now();
        match service.filter(|service| service.alive()) {
            Some(service) if now - started >= STOP_WAIT => {
                return Err(format!(
                    "the service (pid {}) did not stop within {} s; kill it and rerun \
                     `callboard uninstall` (the systemd unit is already removed)",
                    service.pid,
                    STOP_WAIT.as_secs()
                )
                .into());
            }
            Some(_) => (),
            None => {
                let since = *gone_since.get_or_insert(now);
                if now - since >= UNIDENTIFIED_LOCK_WAIT {
                    return Err(format!(
                        "something still holds {}; stop any running `callboard serve` and \
                         rerun `callboard uninstall` (the systemd unit is already removed)",
                        paths.lock().display()
                    )
                    .into());
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Remove the socket only if it is one: a different file there is not ours.
fn remove_socket(path: &Path) -> Result<(), Error> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => Ok(std::fs::remove_file(path)?),
        Ok(_) => Err(format!("refusing to delete {}: not a socket", path.display()).into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Recursive delete, but only of a directory that is plainly ours: named
/// `callboard`, and a real directory. An unusual XDG value can at worst make
/// this refuse, never widen what it removes. A symlink is removed as a link.
fn remove_owned_dir(dir: &Path) -> Result<bool, Error> {
    if dir.file_name().is_none_or(|name| name != "callboard") {
        return Err(format!(
            "refusing to delete {}: not a callboard directory",
            dir.display()
        )
        .into());
    }
    let metadata = match std::fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if metadata.file_type().is_symlink() {
        std::fs::remove_file(dir)?;
    } else {
        std::fs::remove_dir_all(dir)?;
    }
    Ok(true)
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => dir_size(&entry.path()),
            Ok(kind) if kind.is_file() => entry.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_dir_removal_is_limited_to_callboard_directories() {
        let root = tempfile::tempdir().unwrap();
        let other = root.path().join("other");
        std::fs::create_dir(&other).unwrap();
        assert!(remove_owned_dir(&other).is_err());
        assert!(other.exists());

        // A symlink named callboard goes; its target stays.
        let link = root.path().join("callboard");
        std::os::unix::fs::symlink(&other, &link).unwrap();
        std::fs::write(other.join("keep"), "x").unwrap();
        assert!(remove_owned_dir(&link).unwrap());
        assert!(other.join("keep").exists());
        assert!(!remove_owned_dir(&link).unwrap());

        let real = root.path().join("nested/callboard");
        std::fs::create_dir_all(real.join("run")).unwrap();
        assert!(remove_owned_dir(&real).unwrap());
        assert!(!real.exists());
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MiB");
    }
}
