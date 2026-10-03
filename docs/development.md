# Development

Use a current stable Rust toolchain (tested with Rust 1.98.1):

```sh
python3 scripts/checks.py --local
```

This runs all local checks, including container tests with two real Linux UIDs.
Docker is required; missing prerequisites fail rather than skip. For the trusted
base runner and base-branch control gate required before merging, see
[Local merge checks](merge-checks.md). Individual Rust checks remain:

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
UID. It exposes health, feeds, boards/items, layouts, and SSE change events;
see DESIGN.md for the API contract.
HTTP snapshots require an object containing `items`; array input is a CLI feature.
Snapshot bodies are capped at 4 MiB, failure reports at 16 KiB, and client
responses at 16 MiB. Connections have deadlines and a 64-connection limit.

Lifecycle tests bind local Unix sockets and spawn a child process to verify
lock contention and crash recovery. Run them with normal Linux filesystem
ownership and Unix-socket access; restrictive sandboxes may deny these operations.

To run the desktop app from a checkout:

```sh
cargo build -p callboard
target/debug/callboard-gui
```

See [PROGRESS.md](../PROGRESS.md) for what each session built and what comes next.

## Releasing

Pushing a version tag publishes a GitHub release
(`.github/workflows/release.yml`):

```sh
git tag v0.2.0 && git push origin v0.2.0
```

The tag must equal `v` plus the version in `Cargo.toml` and point at a commit
on `main`. The workflow builds a static `callboard` (musl), runs the
cross-user security fixture against that exact binary, builds `callboard-gui`
on Ubuntu 22.04 (glibc 2.35), checks that both carry the tagged commit's
build ID, and attaches `callboard-x86_64-linux.tar.gz` and its SHA-256
checksum to the release. It never replaces an existing release. Bump the
workspace version in `Cargo.toml` before tagging a new release.

## The README demo

`docs/assets/demo-light.gif` and `demo-dark.gif` were recorded from the real
desktop app and service in an isolated session, driven by a script, with a
demo recorder that lives outside this repository. Re-recording means running
that recorder against a release build.
