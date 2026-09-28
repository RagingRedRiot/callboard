# Progress

## Session 1: foundation

- Created the three-crate Cargo workspace from DESIGN.md §7.
- Added feed item and snapshot types, scalar metadata, feed-name validation,
  and JSON submission parsing for arrays and snapshot objects.
- Enforced snapshot size, item count, title/body, tag-count, and metadata-count
  limits. Duplicate item keys and malformed durations are rejected.
- Added boundary tests and explicit-empty versus missing-input coverage.
- At the end of session 1: no persistence, IPC, working CLI, MCP, or GUI.
- Verified with Rust 1.98.1: all 7 integration tests pass, formatting passes,
  and workspace Clippy passes with warnings denied. Cargo.lock records the
  resolved dependencies.

## Session 2: SQLite feed storage

- Added SQLx 0.8.6 with SQLite, Tokio runtime support, and embedded migrations.
  Only the SQLite driver is enabled; runtime SQL requires no build-time database.
- Added `Store::open`, `submit`, `feed`, `list_feeds`, `report_error`,
  `delete_feed`, and `close`. Databases use WAL, full synchronous durability,
  foreign keys, and a five-second busy timeout.
- Submissions validate before writing and use `BEGIN IMMEDIATE` before reading
  previous state. Concurrent submissions compare against committed state.
- Metadata and item changes commit together; reads use a transaction so their
  metadata and items come from one snapshot. Feed deletion cascades to items.
- SHA-256 hashes typed item JSON, including ordered tags and sorted metadata
  keys. Submitted position and feed metadata are outside the hash. Explicit
  null/empty defaults normalize to omitted optional fields. Numeric forms follow
  serde_json's representation (integer `1` and float `1.0` remain distinct).
  Changes to this serialization in future versions need a hash migration.
- Identical submissions refresh submission time and clear errors. Fetch failures
  preserve content and submission time. Unknown-feed failures return not-found.
- Retained keys use updates, preserving row identity for future view state.
  View state itself remains unimplemented.
- Added 11 storage tests covering reopen/migrations, summaries, normalization,
  content fields, metadata defaults, errors, removal/reappearance, validation,
  database-triggered rollback, competing writers, and consistent readers.
- Verified on Rust 1.98.1: `cargo test --workspace --offline` passes all 18 tests;
  formatting and workspace Clippy with warnings denied also pass.

## Session 3: service lifecycle foundation

- Added Linux-only `callboard::lifecycle` with explicit environment inputs,
  XDG/default path resolution, and `ServiceGuard` owning the listener and lock.
- Missing runtime directories fall back to the data directory's `run` folder.
  Unsafe existing runtime directories are errors. Relative XDG values are
  ignored; explicit socket overrides require absolute paths.
- Created directories as 0700 and lock/database files as 0600. Existing
  directories, files, ancestors, and SQLite sidecars are checked before use;
  symlinks, unsafe owners/modes, and hard-linked state files are rejected.
  No process-wide umask or environment changes are used.
- An exclusive nonblocking file lock protects the data directory, including
  when another process selects a different socket directory. The lock file
  remains on disk across restarts so all contenders lock the same inode.
- Under that lock, only owned stale sockets reporting connection refused are
  removed. Socket probes are nonblocking; live sockets and unexpected entries
  are preserved. Drop removes only the guard's own socket inode.
- Tests exercise separate-process contention, forced process death and restart,
  SQLx persistence through the guard, path/permission rejection, simulated
  foreign ownership, and preservation of unrelated files and replacement sockets.
- Socket operations require execution outside the current restricted sandbox,
  which also remaps ownership. Tests use explicitly private temporary roots.
- Verified on Rust 1.98.1: all 35 workspace tests pass outside the sandbox;
  formatting and Clippy with warnings denied pass. This includes the subprocess
  helper test, which is inert when run without its dedicated test environment.
- HTTP, peer credentials, service commands, auto-start, and systemd setup are
  still unimplemented; the binary remains a placeholder.

## Session 4: authenticated HTTP and feed CLI

- Implemented `serve`, `put`, `feeds`, `get`, `fail`, and `feed rm` with Clap.
  JSON goes to stdout; diagnostics go to stderr. Metadata options override
  stdin snapshot metadata. Added-item exit codes take precedence over changed.
- Added Hyper HTTP/1 over the guarded Unix socket and SQLx store. Routes cover
  health, feed list/read/delete, snapshot submission, and fetch errors.
- Server and client verify kernel peer UIDs. No TCP listener is created.
- Bounded snapshot bodies (including chunked transfer) at 4 MiB and failure
  bodies at 16 KiB. Input is bounded before CLI parsing. Client response reads
  cap at 16 MiB. HTTP uses at most 64 connections, 32 headers, a 16 KiB header
  buffer, a 10-second body deadline, and a 30-second connection deadline.
- SIGINT/SIGTERM stops accepting connections, drains for up to five seconds,
  cancels remaining handlers, closes SQLite, then releases the socket and lock.

## Session 5: automatic startup and systemd setup

- Client starts `serve` only for missing/refused sockets; it checks filesystem
  safety and peer credentials before sending. Startup waits at most five seconds
  and tolerates another starter winning the lock. Sent requests are never retried.
- On-demand children get a separate process group and null standard streams,
  with the resolved paths passed explicitly. Startup errors direct the user to
  foreground `serve` for diagnostics. `--no-auto-start` disables this behavior.
- `setup --print` renders a unit; `setup` writes it atomically, reloads systemd,
  and enables it for future logins. It does not interrupt an existing service.
  Existing custom units are preserved. Values are escaped for systemd syntax;
  control characters and non-UTF-8 paths are rejected by unit generation.
- Existing safe shared systemd directories may remain 0755; application state
  directories remain strictly 0700. Generated unit files use 0600.
- End-to-end tests cover CLI/API persistence, exit codes, rejected input,
  chunked/declared request limits, peer UID mismatch, simultaneous auto-start,
  graceful shutdown/restart, and setup using a fake systemctl. No real user
  service was installed or enabled while developing. Generated unit syntax also
  passes the installed `systemd-analyze verify`.
- Final verification on Rust 1.98.1: all 41 workspace tests pass outside the
  socket-restricted sandbox, and formatting/Clippy with warnings denied pass.

## Workflow groundwork after session 5

- Added `scripts/checks.py`: formatting, locked Clippy/tests/build, workflow
  self-tests, and required container authentication checks in one local command.
- Docker tests alternate two real unprivileged UIDs as service owner. They prove
  owner access, filesystem denial, and kernel peer-UID denial for foreign reads
  and writes after deliberately relaxing socket access inside the fixture.
- Added a trusted-base local gate with clean-tree requirements, protected-path
  checks, rename/addition/deletion coverage, and a post-run revision check. The
  initial caller-supplied digest bypass was removed during workflow hardening.
- Added the stethoscope/relay base-branch control workflow: metadata-only, no PR
  checkout/execution, read-only token, and maintainer permission/revision checks
  for fresh control-review labels. Reruns and incomplete API responses fail.
- Added CI and its stable required aggregate.
- Verified the complete local runner: all 41 Rust tests, 15 workflow-gate tests,
  format/Clippy checks, and both real-UID container scenarios pass with no skips.

## Implementation interpretations

- Title lengths count Unicode scalar values; bodies and wire size count bytes.
  The title limit also applies to the feed's display title.
- Omitted metadata stays optional; the store resolves a missing feed title
  to the feed name. An object must include `items`, avoiding accidental clears.
- `stale_after` is validated with humantime and preserved as submitted text.
- No additional restrictions on empty item keys/titles, tag lengths, metadata
  key lengths, or URL schemes are introduced. The design does not specify
  these limits; URL opening restrictions belong in the GUI.
- Metadata uses sorted keys; content hashing is defined in session 2 above.
- Unknown JSON fields are currently ignored by serde. Decide whether to reject
  them when fixing the service wire contract.
- Transport readers must bound input before allocation; the parser accepts an
  already collected slice and checks its size before decoding.
- The store also enforces 4 MiB on serialized snapshot models for callers that
  bypass the parser. This includes the full object envelope; transport limits
  still apply independently to the original wire bytes.
- Store timestamps are UTC Unix milliseconds. The lifecycle guard now provides
  the private database directory/file required by the store. Keep it alive until
  all store connections and request handlers have closed.
- `CALLBOARD_SOCKET_DIR` changes only the socket location. Full isolation also
  requires separate XDG data/config homes. This is explicit in DESIGN.md §7.1.
- Ancestors are owned by the user or root, with no group/world write access;
  root-owned sticky directories such as `/tmp` are allowed. Symlinked paths are
  rejected, even when user-owned. Existing insecure permissions are not repaired.

## Suggested session 6

Implement board/todo/note persistence and migrations in `callboard-core`, with
archive/restore/move semantics and source references. Keep HTTP/CLI wiring for
a subsequent slice. View state, events, GUI, and MCP remain later work.
