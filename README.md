# callboard

A Linux per-user bulletin board for tool-submitted feeds, todos, and notes.
[DESIGN.md](DESIGN.md) describes the intended application.

The feed service and CLI work on Linux, backed by SQLite through SQLx. Boards,
todos, notes, MCP, and the desktop GUI are not implemented yet.

```sh
cargo build -p callboard
printf '%s' '[{"key":"example","title":"Review this"}]' | target/debug/callboard put reviews
target/debug/callboard feeds
target/debug/callboard get reviews
target/debug/callboard fail reviews 'Fetch failed'
target/debug/callboard feed rm reviews
```

Commands start the service automatically when needed. Use `--no-auto-start` to
require an existing service, or `callboard serve` to run it in the foreground.
SIGINT/SIGTERM drains requests and closes SQLite before releasing the data lock.
To test in isolation, set absolute `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, and
`CALLBOARD_SOCKET_DIR` paths under a private directory.

`put` accepts arrays or full snapshot objects; `--title`, `--source-url`, and
`--stale-after` override object metadata. Empty input fails; `[]` clears the feed.
`--exit-added CODE` takes precedence over `--exit-changed CODE` when both match.
Responses are JSON on stdout; errors go to stderr with a nonzero exit status.

From a stable installed binary path, `callboard setup --print` previews the
systemd unit. `callboard setup` installs it and enables it for future logins;
it does not take over an already running on-demand service. If no service is
running, start the unit with `systemctl --user start callboard.service`.
The unit records the executable and resolved data/config/socket paths. Keep
the executable at that location, and rerun setup after moving it. Existing
custom units are preserved. Custom XDG config locations must also be visible
to your systemd user manager.

Use a current stable Rust toolchain (tested with Rust 1.98.1):

```sh
python3 scripts/checks.py --local
```

This runs all local checks, including container tests with two real Linux UIDs.
Docker is required; missing prerequisites fail rather than skip. For the trusted
base runner and base-branch control gate required before merging, see
[Local merge checks](docs/merge-checks.md). Individual Rust checks remain:

```sh
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

`callboard_core::store::Store::open(path)` opens a file-backed database in WAL
mode and applies embedded SQLx migrations. Run it inside a Tokio runtime and
provide an existing private parent directory. The store supports submitting,
reading, listing, deleting, and recording fetch errors for feeds. Snapshot
replacement is atomic and returns added/removed/updated/unchanged counts and
changed keys. Call `close().await` to release its connection pool explicitly.

No database server, `DATABASE_URL`, or SQLx CLI is required to build or test.
Storage tests use temporary SQLite files, including rollback, reopen, and
concurrent access checks. Runtime queries are checked by integration tests;
migrations are embedded at compile time.

On Linux, `callboard::lifecycle` provides environment-based `Paths` resolution
and `ServiceGuard::bind(paths)`. The guard creates private directories, holds the
data lock, prepares the database file, and binds a nonblocking Unix listener.
Keep the guard alive until the SQLx pool and request handlers have shut down.
Dropping it removes its socket and releases the lock. The HTTP service checks
the kernel peer UID before reading requests, and the client checks the server's
UID. It exposes `GET /health`, feed list/read/delete, snapshot PUT, and error POST.
HTTP snapshots require an object containing `items`; array input is a CLI feature.
Snapshot bodies are capped at 4 MiB, failure reports at 16 KiB, and client
responses at 16 MiB. Connections have deadlines and a 64-connection limit.

Lifecycle tests bind local Unix sockets and spawn a child process to verify
lock contention and crash recovery. Run them with normal Linux filesystem
ownership and Unix-socket access; restrictive sandboxes may deny these operations.

The GUI crate is reserved for eframe/egui and egui_tiles, as specified in the
design. Graphics dependencies will be added when GUI implementation starts.
See [PROGRESS.md](PROGRESS.md) for session scope and the next step.
