# callboard

A Linux per-user bulletin board for tool-submitted feeds, todos, and notes.
[DESIGN.md](DESIGN.md) describes the intended application.

The Linux service and CLI support feeds, boards, todos, and notes, backed by
SQLite through SQLx. The API also provides layouts, promotion, feed view state,
and change events. A read-only desktop GUI is available; MCP is not implemented
yet.

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

Board and item commands return JSON:

```sh
callboard board add inbox
callboard boards
callboard todo add inbox 'Reply about the migration' --body 'Check the rollout plan'
callboard note add inbox 'Release freeze starts Thursday' --title 'Release'
callboard board get inbox
# Use the item ID returned by add:
callboard todo done 1
callboard todo done 1 --undone
printf '%s' '{"body":null,"title":"Updated title"}' | callboard todo patch 1
callboard note archive 1
callboard board archive inbox
callboard note restore 1 inbox
```

`board get/rename/rm/archive`, item `add`, and item `move/restore` accept a board
ID or exact name. Numeric selectors mean IDs; use `boards` to find the ID of a
board with a numeric name. Item IDs are always positive integers. Both `todo`
and `note` support `patch`, `archive`, `restore`, `move`, and `rm`. PATCH reads a
JSON object from stdin; omitted fields are preserved and nullable fields can be
cleared with `null`. The API's field names and limits apply.

`board rm BOARD` refuses nonempty boards; `--archive-contents` preserves their
items in the system archive, available through `callboard archive`. Item `rm`
permanently deletes that item. `board rename BOARD NAME` changes a board name.

Feed view controls and layouts are also available from the CLI:

```sh
printf '%s' '{"wake_on_update":true}' | callboard feed patch reviews 'item-key'
printf '%s' '{"position":0}' | callboard feed patch reviews 'item-key'
printf '%s' '{"reset_order":true}' | callboard feed patch reviews 'item-key'
callboard feed promote reviews 'item-key' inbox
callboard feed promote reviews 'item-key' inbox --kind note
printf '%s' '{"tree":{"kind":"feed","name":"reviews"}}' | callboard layout save 'Review day'
callboard layouts
```

Pass literal item keys and layout names, including URLs, Unicode, and `%`;
the CLI encodes them for the API. Feed PATCH accepts `snoozed_until_ms` (UTC Unix
milliseconds or null), `wake_on_update`, `position`, and `reset_order`. To clear
both snooze conditions, send `{"snoozed_until_ms":null,"wake_on_update":false}`.
Feed patch input is limited to 16 KiB. `layout save NAME` accepts a `{"tree":...}`
object up to 64 KiB and replaces that layout atomically. `layouts` returns all
saved trees. See DESIGN.md for the panel-tree schema. These commands support
`--no-auto-start` and the same JSON output/error conventions as other commands.

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
UID. It exposes health, feeds, boards/items, layouts, and SSE change events;
see DESIGN.md for the API contract.
HTTP snapshots require an object containing `items`; array input is a CLI feature.
Snapshot bodies are capped at 4 MiB, failure reports at 16 KiB, and client
responses at 16 MiB. Connections have deadlines and a 64-connection limit.

Lifecycle tests bind local Unix sockets and spawn a child process to verify
lock contention and crash recovery. Run them with normal Linux filesystem
ownership and Unix-socket access; restrictive sandboxes may deny these operations.

The eframe/egui desktop preview lists feeds and boards and displays their
contents, feed errors/staleness, snoozed items, and source-reference status.
The GUI starts `callboard serve` on demand when it cannot connect, then leaves
the service running if the window closes. It locates the service executable as
`callboard` beside the GUI binary or on `PATH`. Set `CALLBOARD_EXECUTABLE` to a
specific executable path to override lookup. For example, during development:

```sh
cargo build -p callboard -p callboard-gui
target/debug/callboard-gui
```

`callboard-gui --no-auto-start` requires an existing service. Both processes
must use the same XDG and CALLBOARD_SOCKET_DIR settings. Auto-start uses the
same kernel UID checks, data lock, and process lifecycle as CLI requests. The
GUI refreshes every five seconds and has a Refresh button. HTTP(S) links open only when clicked; other URL schemes display as text.
This preview is read-only: editing, draggable/saved layouts, and event-stream
refresh are subsequent work. Graphics dependencies are confined to the GUI
crate; building `callboard` alone does not build eframe. A Wayland or X11 desktop
with OpenGL support is required to launch the window.
See [PROGRESS.md](PROGRESS.md) for session scope and the next step.
