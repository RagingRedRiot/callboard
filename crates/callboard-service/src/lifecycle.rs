//! Filesystem and socket startup for a single-user Linux service.
//!
//! Same-user processes and root are trusted. Paths reject symlinks and unsafe
//! ancestors so another local user cannot replace a checked directory. No
//! process-wide environment or umask changes are made.

use std::fs::{self, DirBuilder, File, Metadata, OpenOptions, Permissions, TryLockError};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Component, Path, PathBuf};

use rustix::fs::OFlags;
use rustix::net::{self, AddressFamily, SocketAddrUnix, SocketFlags, SocketType};

#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("unsafe path {path}: {reason}")]
    UnsafePath { path: PathBuf, reason: &'static str },
    #[error("an absolute HOME is required when XDG data/config defaults are used")]
    MissingHome,
    #[error("another service holds the data-directory lock: {0}")]
    AlreadyRunning(PathBuf),
    #[error("socket is already in use: {0}")]
    SocketInUse(PathBuf),
}

fn io_error(path: &Path, source: impl Into<io::Error>) -> LifecycleError {
    LifecycleError::Io {
        path: path.to_owned(),
        source: source.into(),
    }
}

fn unsafe_path(path: &Path, reason: &'static str) -> LifecycleError {
    LifecycleError::UnsafePath {
        path: path.to_owned(),
        reason,
    }
}

/// Explicit environment input keeps resolution testable without mutating env.
#[derive(Debug, Default)]
pub struct Environment {
    pub home: Option<PathBuf>,
    pub data_home: Option<PathBuf>,
    pub config_home: Option<PathBuf>,
    pub runtime_dir: Option<PathBuf>,
    pub socket_dir: Option<PathBuf>,
}

impl Environment {
    pub fn current() -> Self {
        let get = |name| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        Self {
            home: get("HOME"),
            data_home: get("XDG_DATA_HOME"),
            config_home: get("XDG_CONFIG_HOME"),
            runtime_dir: get("XDG_RUNTIME_DIR"),
            socket_dir: get("CALLBOARD_SOCKET_DIR"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    data: PathBuf,
    config: PathBuf,
    socket_dir: PathBuf,
}

impl Paths {
    /// Resolve without creating anything. Relative/empty XDG values are ignored.
    /// Missing runtime directories fall back to data/run; unsafe ones are errors.
    pub fn resolve(env: &Environment) -> Result<Self, LifecycleError> {
        let uid = rustix::process::geteuid().as_raw();
        let absolute = |p: &Option<PathBuf>| p.as_ref().filter(|p| p.is_absolute()).cloned();
        let home_default = |suffix: &str| {
            absolute(&env.home)
                .map(|p| p.join(suffix))
                .ok_or(LifecycleError::MissingHome)
        };
        let data = match absolute(&env.data_home) {
            Some(p) => p,
            None => home_default(".local/share")?,
        }
        .join("callboard");
        let config = match absolute(&env.config_home) {
            Some(p) => p,
            None => home_default(".config")?,
        }
        .join("callboard");
        validate_absolute(&data)?;
        validate_absolute(&config)?;
        let socket_dir = if let Some(path) = env
            .socket_dir
            .as_ref()
            .filter(|p| !p.as_os_str().is_empty())
        {
            validate_absolute(path)?;
            path.clone()
        } else {
            let candidate = absolute(&env.runtime_dir)
                .unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}")));
            match check_directory(&candidate, uid, false) {
                Ok(()) => candidate,
                Err(LifecycleError::Io { source, .. })
                    if source.kind() == io::ErrorKind::NotFound =>
                {
                    data.join("run")
                }
                Err(error) => return Err(error),
            }
        };
        // Validate socket address length before startup creates files.
        SocketAddrUnix::new(socket_dir.join("callboard.sock"))
            .map_err(|e| io_error(&socket_dir, e))?;
        Ok(Self {
            data,
            config,
            socket_dir,
        })
    }

    pub fn data_dir(&self) -> &Path {
        &self.data
    }
    pub fn config_dir(&self) -> &Path {
        &self.config
    }
    pub fn socket_dir(&self) -> &Path {
        &self.socket_dir
    }
    pub fn database(&self) -> PathBuf {
        self.data.join("callboard.sqlite3")
    }
    pub fn socket(&self) -> PathBuf {
        self.socket_dir.join("callboard.sock")
    }
    pub fn lock(&self) -> PathBuf {
        self.data.join("service.lock")
    }

    /// Validate a client endpoint before connecting (never creates files).
    pub fn check_socket(&self) -> Result<(), LifecycleError> {
        let uid = rustix::process::geteuid().as_raw();
        check_directory(&self.socket_dir, uid, false)?;
        let path = self.socket();
        let metadata = fs::symlink_metadata(&path).map_err(|e| io_error(&path, e))?;
        if !metadata.file_type().is_socket()
            || metadata.uid() != uid
            || metadata.mode() & 0o777 != 0o600
        {
            return Err(unsafe_path(
                &path,
                "expected an owned socket with mode 0600",
            ));
        }
        Ok(())
    }

    fn prepare(&self) -> Result<(), LifecycleError> {
        let uid = rustix::process::geteuid().as_raw();
        for path in [&self.data, &self.config, &self.socket_dir] {
            check_directory(path, uid, true)?;
        }
        Ok(())
    }
}

fn validate_absolute(path: &Path) -> Result<(), LifecycleError> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(unsafe_path(
            path,
            "requires an absolute path without '..' components",
        ));
    }
    Ok(())
}

pub(crate) fn check_directory(path: &Path, uid: u32, create: bool) -> Result<(), LifecycleError> {
    check_directory_policy(path, uid, create, true)
}

pub(crate) fn check_shared_directory(path: &Path, uid: u32) -> Result<(), LifecycleError> {
    check_directory_policy(path, uid, true, false)
}

fn check_directory_policy(
    path: &Path,
    uid: u32,
    create: bool,
    private: bool,
) -> Result<(), LifecycleError> {
    validate_absolute(path)?;
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(e) if create && e.kind() == io::ErrorKind::NotFound => {
                match DirBuilder::new().mode(0o700).create(&current) {
                    Ok(()) => (),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
                    Err(e) => return Err(io_error(&current, e)),
                }
                fs::symlink_metadata(&current).map_err(|e| io_error(&current, e))?
            }
            Err(e) => return Err(io_error(&current, e)),
        };
        validate_directory(&current, &metadata, uid, private && current == path)?;
    }
    Ok(())
}

fn validate_directory(
    path: &Path,
    metadata: &Metadata,
    uid: u32,
    private: bool,
) -> Result<(), LifecycleError> {
    if !metadata.is_dir() {
        return Err(unsafe_path(
            path,
            "expected a directory, not a symlink or other file",
        ));
    }
    if private {
        if metadata.uid() != uid {
            return Err(unsafe_path(path, "directory belongs to another user"));
        }
        if metadata.mode() & 0o777 != 0o700 {
            return Err(unsafe_path(path, "directory must have mode 0700"));
        }
    } else {
        if metadata.uid() != uid && metadata.uid() != 0 {
            return Err(unsafe_path(path, "ancestor belongs to an untrusted user"));
        }
        // A root-owned sticky directory (e.g. /tmp) prevents other users from
        // renaming/removing our entries and supports isolated test deployments.
        let sticky_root = metadata.uid() == 0 && metadata.mode() & 0o1000 != 0;
        if metadata.mode() & 0o022 != 0 && !sticky_root {
            return Err(unsafe_path(path, "ancestor is writable by other users"));
        }
    }
    Ok(())
}

fn validate_file(path: &Path, metadata: &Metadata, uid: u32) -> Result<(), LifecycleError> {
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.nlink() != 1
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(unsafe_path(
            path,
            "expected an owned, single-link regular file with mode 0600",
        ));
    }
    Ok(())
}

pub(crate) fn private_file(path: &Path, uid: u32) -> Result<File, LifecycleError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_file(path, &metadata, uid)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        Err(e) => return Err(io_error(path, e)),
    }
    // O_NONBLOCK prevents a pre-existing FIFO from hanging startup; O_NOFOLLOW
    // prevents symlink traversal. Opening never truncates an existing file.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32)
        .open(path)
        .map_err(|e| io_error(path, e))?;
    validate_file(path, &file.metadata().map_err(|e| io_error(path, e))?, uid)?;
    Ok(file)
}

/// Owns the listener and data lock for the service lifetime. Drop removes only
/// its own socket inode, then releases the lock. The lock file is never unlinked.
pub struct ServiceGuard {
    listener: UnixListener,
    paths: Paths,
    socket_identity: (u64, u64),
    _lock: File,
}

impl ServiceGuard {
    /// Synchronous startup; run before the service's async request loop. This
    /// prepares a private empty database file but does not open SQLx or serve HTTP.
    pub fn bind(paths: Paths) -> Result<Self, LifecycleError> {
        paths.prepare()?;
        let uid = rustix::process::geteuid().as_raw();
        let lock = try_lock(&paths)?.ok_or_else(|| LifecycleError::AlreadyRunning(paths.lock()))?;
        check_store_files(&paths, uid)?;
        let socket_path = paths.socket();
        remove_stale_socket(&socket_path, uid)?;
        let listener = UnixListener::bind(&socket_path).map_err(|e| io_error(&socket_path, e))?;
        let metadata = fs::symlink_metadata(&socket_path).map_err(|e| io_error(&socket_path, e))?;
        let guard = Self {
            listener,
            paths,
            socket_identity: (metadata.dev(), metadata.ino()),
            _lock: lock,
        };
        // The directory is already 0700, including during bind/chmod.
        fs::set_permissions(&socket_path, Permissions::from_mode(0o600))
            .map_err(|e| io_error(&socket_path, e))?;
        guard
            .listener
            .set_nonblocking(true)
            .map_err(|e| io_error(&socket_path, e))?;
        Ok(guard)
    }

    /// The re-executed side of [`ServiceGuard::exec`]: take ownership of the
    /// inherited lock and listener (`LOCK,LISTENER` descriptor numbers), but
    /// only after checking they are this deployment's lock file and socket. A
    /// wrong descriptor adopted as the lock would let two services share a store.
    pub fn adopt(paths: Paths, handoff: &str) -> Result<Self, LifecycleError> {
        use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd};
        let invalid = |reason| unsafe_path(&paths.lock(), reason);
        let fds: Vec<i32> = handoff
            .split(',')
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(|_| invalid("--handoff takes two descriptor numbers"))?;
        let [lock_fd, listener_fd] = fds[..] else {
            return Err(invalid("--handoff takes two descriptor numbers"));
        };
        if lock_fd <= 2 || listener_fd <= 2 || lock_fd == listener_fd {
            return Err(invalid(
                "--handoff descriptors must be distinct and not stdio",
            ));
        }
        for fd in [lock_fd, listener_fd] {
            // SAFETY: only queried and flagged here; an fd that is not open
            // fails with EBADF. Nothing may inherit these from now on.
            let fd = unsafe { BorrowedFd::borrow_raw(fd) };
            rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)
                .map_err(|e| io_error(&paths.lock(), e))?;
        }
        // SAFETY: both descriptors are open (checked above), distinct, and
        // were handed to this process to own.
        let (lock, listener) = unsafe {
            (
                File::from_raw_fd(lock_fd),
                UnixListener::from(OwnedFd::from_raw_fd(listener_fd)),
            )
        };
        paths.prepare()?;
        let uid = rustix::process::geteuid().as_raw();
        let held = lock.metadata().map_err(|e| io_error(&paths.lock(), e))?;
        let named = private_file(&paths.lock(), uid)?
            .metadata()
            .map_err(|e| io_error(&paths.lock(), e))?;
        if (held.dev(), held.ino()) != (named.dev(), named.ino()) {
            return Err(invalid("inherited lock is not this data directory's lock"));
        }
        // Re-locking through the inherited open file description succeeds;
        // any other holder makes it fail.
        match lock.try_lock() {
            Ok(()) => (),
            Err(TryLockError::WouldBlock) => {
                return Err(LifecycleError::AlreadyRunning(paths.lock()));
            }
            Err(TryLockError::Error(e)) => return Err(io_error(&paths.lock(), e)),
        }
        check_store_files(&paths, uid)?;
        let socket_path = paths.socket();
        let bound = listener
            .local_addr()
            .map_err(|e| io_error(&socket_path, e))?;
        let listening = rustix::net::sockopt::socket_acceptconn(listener.as_fd())
            .map_err(|e| io_error(&socket_path, e))?;
        if bound.as_pathname() != Some(socket_path.as_path()) || !listening {
            return Err(unsafe_path(
                &socket_path,
                "inherited listener is not this deployment's socket",
            ));
        }
        let metadata = fs::symlink_metadata(&socket_path).map_err(|e| io_error(&socket_path, e))?;
        if !metadata.file_type().is_socket() || metadata.uid() != uid {
            return Err(unsafe_path(&socket_path, "expected an owned socket"));
        }
        listener
            .set_nonblocking(true)
            .map_err(|e| io_error(&socket_path, e))?;
        Ok(Self {
            listener,
            paths,
            socket_identity: (metadata.dev(), metadata.ino()),
            _lock: lock,
        })
    }

    /// Become `executable serve --handoff LOCK,LISTENER`, passing the lock and
    /// listener across the exec (see [`ServiceGuard::adopt`]). Destructors do not
    /// run, so the socket stays bound. Returns only if the exec failed, with
    /// both descriptors restored to close-on-exec.
    pub fn exec(&self, executable: &Path) -> io::Error {
        use std::os::fd::{AsFd, AsRawFd};
        use std::os::unix::process::CommandExt;
        let fds = [self._lock.as_fd(), self.listener.as_fd()];
        let set = |flags| {
            fds.iter()
                .try_for_each(|fd| rustix::io::fcntl_setfd(fd, flags))
                .map_err(io::Error::from)
        };
        if let Err(e) = set(rustix::io::FdFlags::empty()) {
            let _ = set(rustix::io::FdFlags::CLOEXEC);
            return e;
        }
        let error = std::process::Command::new(executable)
            .arg("serve")
            .arg("--handoff")
            .arg(format!(
                "{},{}",
                self._lock.as_raw_fd(),
                self.listener.as_raw_fd()
            ))
            .exec();
        let _ = set(rustix::io::FdFlags::CLOEXEC);
        error
    }

    pub fn listener(&self) -> &UnixListener {
        &self.listener
    }
    pub fn paths(&self) -> &Paths {
        &self.paths
    }
}

/// Take the data lock without waiting: `None` while a service holds it.
/// Uninstall holds it to prove no service remains (DESIGN.md §7.4).
pub fn try_lock(paths: &Paths) -> Result<Option<File>, LifecycleError> {
    let lock = private_file(&paths.lock(), rustix::process::geteuid().as_raw())?;
    match lock.try_lock() {
        Ok(()) => Ok(Some(lock)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(io_error(&paths.lock(), e)),
    }
}

fn check_store_files(paths: &Paths, uid: u32) -> Result<(), LifecycleError> {
    private_file(&paths.database(), uid)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let path = paths.data.join(format!("callboard.sqlite3{suffix}"));
        match fs::symlink_metadata(&path) {
            Ok(metadata) => validate_file(&path, &metadata, uid)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(io_error(&path, e)),
        }
    }
    Ok(())
}

impl Drop for ServiceGuard {
    fn drop(&mut self) {
        let path = self.paths.socket();
        if let Ok(metadata) = fs::symlink_metadata(&path)
            && metadata.file_type().is_socket()
            && (metadata.dev(), metadata.ino()) == self.socket_identity
        {
            let _ = fs::remove_file(path);
        }
    }
}

fn remove_stale_socket(path: &Path, uid: u32) -> Result<(), LifecycleError> {
    let before = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_error(path, e)),
    };
    if !before.file_type().is_socket() || before.uid() != uid || before.nlink() != 1 {
        return Err(unsafe_path(
            path,
            "refusing to replace a non-socket, foreign-owned socket, or hard link",
        ));
    }
    let address = SocketAddrUnix::new(path).map_err(|e| io_error(path, e))?;
    let probe = net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|e| io_error(path, e))?;
    match net::connect(&probe, &address) {
        Err(rustix::io::Errno::CONNREFUSED) => (),
        Ok(()) | Err(rustix::io::Errno::AGAIN) | Err(rustix::io::Errno::INPROGRESS) => {
            return Err(LifecycleError::SocketInUse(path.to_owned()));
        }
        Err(e) => return Err(io_error(path, e)),
    }
    let after = fs::symlink_metadata(path).map_err(|e| io_error(path, e))?;
    if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
        return Err(unsafe_path(path, "socket changed during startup"));
    }
    fs::remove_file(path).map_err(|e| io_error(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_ownership_is_rejected_without_modification() {
        let dir = tempfile::Builder::new()
            .permissions(Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let uid = rustix::process::geteuid().as_raw();
        // A different expected identity exercises foreign-owner checks without
        // requiring root or changing actual filesystem ownership.
        let other = uid.wrapping_add(1);
        let metadata = fs::symlink_metadata(dir.path()).unwrap();
        assert!(validate_directory(dir.path(), &metadata, other, true).is_err());
        let path = dir.path().join("lock");
        let file = private_file(&path, uid).unwrap();
        assert!(validate_file(&path, &file.metadata().unwrap(), other).is_err());
        let socket = dir.path().join("socket");
        let listener = UnixListener::bind(&socket).unwrap();
        drop(listener);
        assert!(matches!(
            remove_stale_socket(&socket, other),
            Err(LifecycleError::UnsafePath { .. })
        ));
        assert!(socket.exists());
        assert_eq!(fs::symlink_metadata(dir.path()).unwrap().uid(), uid);
    }
}
