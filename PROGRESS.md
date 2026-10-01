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

## Session 6

- Added the `0002_boards_items.sql` migration for named boards, todos, and notes.
- Added typed core models and SQLx operations for board create/list/rename/delete;
  todo/note create, archive, restore, move, and permanent deletion; plus todo
  completion state and source references.
- A source reference is stored as feed/key text without a foreign key, so it
  survives feed item removal as designed.
- A confirmed non-empty board deletion moves its items into a private system
  archive board, preserving item IDs and source references. Normal board lists
  hide this archive; archive listing and restore operations expose its contents.
- HTTP/CLI wiring, item editing/reordering, view state, events, GUI, and MCP
  remain future slices.

## Session 7

- Added local HTTP routes for board listing/creation/renaming/deletion, board
  contents and archives, todo/note creation, completion, archive/restore/move,
  permanent deletion, and the archive for items whose board was deleted.
- Board deletion accepts `{"archive_contents":true}` as the explicit
  confirmation for preserving non-empty board contents in the private archive.
- Board and item JSON requests have a 64 KiB body limit. Store errors map to
  client status codes for missing objects, invalid names, duplicate names, and
  non-empty board deletion without confirmation.
- Editing item text and changing manual order remain unimplemented; the current
  todo PATCH operation changes completion state.
- `cargo check -p callboard --locked` passed. Tests were not run in this slice.

## Session 8

- Added store coverage for unique board names, lifecycle operations, stable
  source references after feed deletion, item archive/restore/move, explicit
  board deletion archival, and persistence after reopening the database.
- Added service API coverage for board and item creation, duplicate-name and
  missing-board responses, completion state, board/item archives, confirmed
  board deletion, restore, and move routes.
- Item-field editing and manual ordering remain out of scope because the design
  docs do not settle their patch/request shape yet.
- Core tests, service library tests, and service integration tests pass;
  Clippy passes with warnings denied. The service tests needed an unsandboxed
  run because the sandbox reports `/` as UID 65534 and the service correctly
  rejects that untrusted ancestor in its path-security checks.

## Session 9

- Added SQLx PATCH operations for editing todo/note fields, moving between
  boards, completing todos, archiving/restoring, and changing zero-based order.
- Item PATCHes accept only changed fields; JSON null clears optional text,
  color, and reference fields. A restore from a deleted board's archive requires
  a destination `board_id`.
- Exposed the PATCH contract at `/todos/{id}` and `/notes/{id}`. Existing action
  routes remain available for compatibility.
- Updated DESIGN.md §8.1 with the field and ordering semantics.
- Added tests for editing, clearing nullable values, reordering, moving,
  archiving/restoring, invalid positions, and the HTTP PATCH routes.
- Core tests, service integration tests, and Clippy with warnings denied pass.

## Session 10

- Added migration `0003_feed_view_state.sql` for item snooze state, manual feed
  order, and an explicit manual-order mode that remains enabled when a feed is
  temporarily empty.
- Feed reads now return display order, manual-order status, and per-key snooze
  state. Time snoozes wake when their UTC Unix millisecond instant passes;
  wake-on-update snoozes clear only when submitted content changes.
- Manual order inserts newly submitted items at the top, prunes removed keys,
  survives snapshots and restarts, and resets to tool order on request.
- Added `PATCH /feeds/{name}/items/{key}` for snooze state, zero-based position,
  and `reset_order`; documented those request semantics in DESIGN.md §8.1.
- Added core and API tests for persistence, update wake behavior, insertion,
  pruning, reset, missing items, and invalid positions. Core and service tests
  pass; Clippy with warnings denied, formatting, and whitespace checks pass.

## Session 11

- Added atomic store promotion operations and `POST /feeds/{name}/items/{key}/promote`
  with `board_id` plus `kind: "todo" | "note"`. Promotion reads and copies the
  current source item while holding the SQLite write transaction, so source
  submissions cannot race copied fields.
- Promoted todos and notes copy title, optional body, and URL, with a durable
  `{feed,key}` reference that has no source foreign key. Missing items return
  404; deleting the source leaves promoted objects intact.
- Added `0004_note_url.sql` because the design requires copied URLs on both
  promoted resource types, while notes previously had no URL field. Note create
  and PATCH APIs now accept that optional field.
- Updated DESIGN.md §§5.2, 5.3, and 8.1 and added core/API tests for promotion,
  copied values, missing sources, and source deletion.
- Core tests, service integration tests, Clippy with warnings denied, format,
  and whitespace checks pass.

## Suggested session 12 (completed below)

Display-time reference resolution for board reads.

## Audit of sessions 6–11

Reviewed the accumulated storage, migrations, board/item API, snooze/order, and
promotion work against DESIGN.md. Found and corrected:

- Combined snoozes did not expire at their deadline when wake-on-update was set.
  Normalize expired state before applying later patches to prevent revival.
- Item routes used raw URI segments, making URL-shaped keys inaccessible.
  Decode percent-encoded UTF-8 segments once, preserving literal plus signs.
- Negative board positions reached SQL constraints (HTTP 500); archived-item
  reorders were silently ignored. Reject both atomically as client errors.
- Snooze-only updates returned submitted positions during manual ordering.
- Board item creation/edits lacked the documented title/body content limits.
- Moving via PATCH could silently restore archived items. Move now preserves
  archive state, and archive plus destination is accepted consistently.
- Action and PATCH implementations diverged; move/restore responses were read
  after commit. Shared transactional mutations now return their own result.
- Board/archive HTTP reads assembled multiple database snapshots; these now
  read metadata and both item lists in one transaction. Rename responses no
  longer re-read a potentially deleted or renamed board after committing.
- PATCH typos and nulls for required values were silently ignored. Reject them;
  nullable fields retain explicit clearing. Reject reset-order plus position.
- Corrected board-route 404/405 handling and empty-object deletion requests.

Added six core audit tests and an HTTP regression covering encoded keys,
validation, and routing. Four initial core regressions were observed failing
before fixes. Upgrade tests preserve existing data from migration versions 1
and 3, including notes created before the URL column existed.

Validation: `python3 scripts/checks.py --local` passed: 56 Rust tests, 15 Python
gate tests, formatting, Clippy with warnings denied, build, and mandatory Docker
cross-UID checks in both directions. Foreign reads/writes were rejected even
with filesystem socket permissions relaxed; owners retained access. No UID
authentication, dependency, or workflow-control changes were made. This is
local development validation, not trusted-base merge approval.

Remaining verification caveat: the existing
`stale_owned_socket_is_replaced_but_live_socket_is_preserved` test failed once
with SocketInUse after dropping its fixture listener. It passed on the earlier
workspace run, an isolated rerun, and the full workflow rerun. Cause remains
unconfirmed; neither the test nor production lifecycle code was weakened.

No new feature session was started. Display-time source-reference resolution
remains the next planned feature. Audit changes remain uncommitted.


## Session 12

- Board and archive GET responses now include `resolved_reference`: null for
  unlinked objects, live with the full current source item, or source_gone.
- Resolution happens in the same read transaction as board metadata and items,
  using one batch lookup of distinct references rather than a query per object.
- Stored references and user-edited/copied fields remain unchanged. Removed
  sources resolve gone; reappearing feed/key pairs resolve live again.
- Added HTTP coverage for live updates, copied-content preservation, unlinked
  notes, key removal/reappearance, board archives, deleted-board archives, and
  feed deletion. Updated the API contract in DESIGN.md §5.3.

- Validation: full `python3 scripts/checks.py --local` passed: 57 Rust tests,
  15 workflow tests, formatting, Clippy, build, and both Docker UID-isolation
  cases. The previously intermittent stale-socket test passed this session.

## Session 13

- Added migration 0005 for named layouts, with atomic replacement and ordered
  reads through SQLx. Layout targets have no foreign keys, retaining placeholders
  for absent/deleted feeds and boards.
- Added toolkit-independent typed panel trees: empty root, feed/board leaves,
  weighted splits, and tabs with an active index. Validation bounds names, body
  size, depth, node count, and shape; invalid saves leave prior state intact.
- Added GET /layouts and PUT /layouts/{name}, including decoded Unicode/path
  names and the existing bounded request-body reader. Documented the contract
  in DESIGN.md §6.4. No dependencies were added.
- Tests cover shape/size limits, target deletion, restart persistence, atomic
  replacement, encoded names, and API error handling.

- Validation: full `python3 scripts/checks.py --local` passed: 60 Rust tests,
  15 workflow tests, formatting, Clippy, build, and Docker UID isolation in both
  directions. This is development validation, not merge approval.

## Session 14

- Added a shared, bounded Store notification bus. Successful mutations emit
  feed, board, or layout invalidations after committing. Failed mutations and
  reads emit nothing; moves notify both boards and deleted-board contents
  invalidate the system archive.
- Added authenticated GET /events SSE with initial/lag resync, heartbeat
  comments, 128 buffered notices, and a 16-subscriber cap. Streams rotate after
  25 seconds to retain the existing 30-second connection bound; reconnect and
  refetch behavior is documented in DESIGN.md §8.2.
- Stream bodies own their receivers and subscription permits without detached
  forwarding tasks. Slow consumers do not block writes; disconnects release
  resources. Enabled the existing Tokio sync feature; no new crate/version.
- Added tests across store mutation families, rejection/rollback silence,
  notification visibility, lag, stream expiry/permit cleanup, and HTTP delivery
  and reconnect resync.
- Full `python3 scripts/checks.py --local` passed: 63 Rust tests, 15 workflow
  tests, formatting, Clippy, build, and both Docker UID-isolation cases. Local
  development validation only; changes remain uncommitted.

## Session 15

- Added board list/create/read/rename/delete/archive CLI commands, plus todo
  and note creation, bounded JSON PATCH input, archive/restore/move/delete,
  and todo completion/undo. Board selectors accept an ID or exact name.
- Commands use the existing service API, JSON stdout, stderr errors, and global
  --no-auto-start behavior. Invalid typed patches, IDs, and content limits are
  checked before connecting; nullable fields preserve PATCH clearing semantics.
- Added CLI integration tests for automatic startup, names and IDs, edits,
  moves, archives, explicit nonempty-board deletion, error output, and rejecting
  invalid input without starting the service. Updated README and DESIGN.md.
- Final full `python3 scripts/checks.py --local` passed: 65 Rust tests, 15
  workflow tests, formatting, Clippy, build, and both Docker UID-isolation cases.
- Verification caveat: the existing non_socket_and_symlink_entries_are_never_removed
  lifecycle test failed twice at its second bind assertion before CLI tests ran.
  It passed in isolation, in a diagnostic lifecycle run, and in the final full
  workflow. Added the actual result to its failure message; cause remains
  unresolved. No lifecycle production behavior or assertion was weakened.
- Changes remain uncommitted; local development checks do not approve a merge.

## Session 16

- Added feed patch NAME KEY for bounded, typed JSON snooze/order updates and
  feed promote NAME KEY BOARD [--kind todo|note], accepting board IDs or names.
- Added layouts and layout save NAME with local panel-tree validation and
  bounded stdin. Invalid inputs are rejected before service access; patch
  bodies preserve explicit nulls and omitted fields.
- Added byte-wise URI segment encoding for literal keys/names, including URL
  keys, Unicode, plus signs, percent signs, slashes, and query/fragment markers.
- Integration tests cover snooze/reset/reorder, both promotion kinds, source
  references, layout replacement, auto-start/no-auto-start, failed saves,
  missing sources, malformed/oversized input, and validation before startup.
- The existing stale-socket test failed again in the first workflow run. Moved
  the subprocess lock/crash tests unchanged to lifecycle_process.rs so fork/exec
  cannot temporarily retain parallel lifecycle tests' socket/lock descriptors.
  This removes a plausible source of the recurring lifetime-test interference;
  the final full workflow passed. No production lifecycle checks were changed.
- Full local workflow passed: 67 Rust tests, 15 workflow tests, formatting,
  Clippy, build, and Docker UID isolation in both directions. Updated README
  and DESIGN.md. Changes remain uncommitted; no merge approval claimed.

## Session 17

- Replaced the GUI placeholder with an eframe/egui 0.36.2 desktop preview using
  the glow renderer and Wayland/X11 support. Graphics dependencies remain in
  callboard-gui; the service/CLI dependency graph does not include eframe.
- Added a read-only backend using the existing authenticated Unix-socket client,
  requiring an already-running service. It never opens SQLite or auto-starts a
  service. A bounded worker handles requests off the UI thread; old-selection
  responses are discarded. Refresh is manual and every five seconds for now.
- Added feed/board sidebar selection, deleted-board archive, content display,
  feed age/error/stale status, snoozed visibility, disabled todo checkboxes,
  source reference status, deleted-resource placeholders, and connection errors.
  Only explicit HTTP(S) links are clickable; bodies are rendered as plain text.
- Added deserialization for shared board display models and tests against a
  real authenticated service, including source updates/deletion, missing
  resources, disconnects, headless UI rendering, and late-response rejection.
- Updated README with launch instructions and preview limitations. No editing,
  saved/draggable panels, or SSE client was added in this session.

- Validation: full local workflow passed with 69 Rust tests, 15 workflow tests,
  formatting, Clippy, build, and both Docker UID-isolation cases. The GUI binary
  built and ran for a 12-second desktop launch smoke check without diagnostics,
  then timeout closed it. Visual interaction was not manually inspected (the
  Wayland window did not appear in the X11 window list). Headless rendering is
  covered by the test suite; saved layouts and editing remain unimplemented.
- Changes remain uncommitted; no merge approval claimed.

## Session 18

- The GUI backend now passes the sibling/PATH `callboard` executable through
  the existing authenticated client. Lookup order is `CALLBOARD_EXECUTABLE`,
  sibling executable, then PATH. Invalid/missing candidates fall back to the
  existing service-only behavior with a visible setup hint.
- Added `--no-auto-start` for GUI users who require an already-running service.
  Auto-start uses the CLI client lifecycle, XDG paths and socket directory, so
  the service lock prevents duplicates and the daemon remains independent of
  the GUI window and continues serving CLI/feed jobs after it closes.
- Added executable lookup tests and an integration test that starts an isolated
  daemon through the GUI backend, proves later refreshes retain the same daemon
  PID, verifies the daemon serves after the first client request returns, and
  stops it cleanly. Updated README and DESIGN.md.
- Full local workflow passed: 71 Rust tests, 15 workflow tests, formatting,
  Clippy, build, and both Docker UID isolation cases. No CI, workflow,
  dependency, database, or service authentication changes were made beyond the
  existing eframe dependencies and GUI lifecycle path. Changes remain
  uncommitted; this is development validation, not merge approval.

## Session 19

- Replaced the single selected-resource view with an `egui_tiles` 0.17.1 panel
  tree (built for egui 0.36; only that crate was added to Cargo.lock). Panels
  sit in splits and tab groups, can be dragged, retargeted (**Show…**), and
  closed. Sidebar clicks reveal a placed target or open a tab; the context menu
  opens a new tab, splits right/below, or retargets the focused panel.
- Saved layouts load from `GET /layouts` and convert both ways between the
  service `Panel` tree and the tile tree (weights, active tab, nested splits).
  The first layout by name opens at startup unless panels were already
  arranged. Each layout keeps a session-local working copy, so switching back
  preserves rearrangement. Unmodified copies follow newer saves; rearranged
  copies are flagged "rearranged" (with Revert) and "newer saved version".
  Deleted feed/board targets remain as placeholder panels. The deleted-board
  archive can be shown but is omitted from layout trees. DESIGN.md §6.4 now
  states that a horizontal split places children left to right.
- Read-only preserved: no layout or other mutation is sent. Rearrangement is
  never saved.
- Added `client::stream`, a streaming GET that reuses the existing
  authenticated connection path; the buffered `request` is unchanged. The GUI
  subscribes to `/events` on its own thread with an incremental SSE parser,
  a 30-second idle bound, and backoff (immediate after healthy rotation,
  otherwise 1 s doubling to 30 s, honoring `retry:`). The subscriber never
  auto-starts; fetches keep that role.
- A scheduler refetches lists plus only affected open targets per notice (feed
  notices also refresh open boards/archive for references), everything on
  initial or lag resync, one batch in flight, failed batches after 5 s, snooze
  deadlines at expiry, and polls every 5 s only after the stream has been down
  for 2 s. Contents are cached only for placed targets.
- Sidebar shows feed error/stale markers with hover details and visible/snoozed
  counts for placed feeds and boards. List endpoints carry no counts, so
  unplaced entries show none.
- Fixed `--no-auto-start`: it previously skipped only the
  `CALLBOARD_EXECUTABLE` override and still auto-started a sibling/PATH binary.
- Tests: layout round trip, placements, reveal, close/focus, layout switching
  and save-following; SSE parsing, backoff, scheduler resync/fallback/retry/
  snooze; headless app flows (saved layout, placeholders, resync, sidebar
  actions); a real-service subscription through restart; and the autostart
  test, now also proving the subscriber does not start the daemon.
- Full local workflow passed: 84 Rust tests, 15 workflow tests, formatting,
  locked Clippy, build, and both Docker UID isolation cases. An X11 launch
  against an isolated seeded service showed the saved layout, placeholders,
  live updates without refresh, and Live → Polling → Live across a daemon kill
  with GUI auto-restart. Changes remain uncommitted; not merge approval.

### Session 19 review fixes

A high-effort code review found nine issues; all are fixed with regression
tests:

- Saved layouts that the tile tree simplifies (one-child tabs, nested same-axis
  splits) no longer show "rearranged" on load; the base is the normalized form.
- Switching tabs no longer counts as rearranging. A service change to the
  active tab alone is still detected.
- A dropped active archive tab saves as its left neighbour.
- A failed refetch keeps the panel's last contents and shows the error above
  them.
- Only failed batch parts retry, after 5 s. Lists and each target retry
  separately, and nothing else is held back.
- A 404 from `/layouts` (a service predating layouts) yields no layouts instead
  of blocking feeds and boards.
- Backoff with a maximum below 100 ms no longer panics.
- Rebuilt trees get fresh IDs, and per-panel state is keyed by target, so
  scroll and snoozed toggles do not leak between panels.
- Routine rotation no longer refetches everything every 25 s (DESIGN.md §6.5,
  with a 5-minute full-refresh cap). Startup waits up to 1 s for the stream, so
  the first load happens once.
- A second review found ten more issues; all are fixed with tests:
  - newly opened panels are no longer fetched twice;
  - an added archive panel counts as rearranged, so newer saves no longer
    drop it;
  - only a clean end after at least 20 s counts as rotation, so a quick
    service restart still refetches;
  - split weights are normalized before narrowing to f32;
  - tile actions carry their tree ID and are ignored after a switch or revert;
  - only a resource-level 404 means deleted, and the generic
    `unknown resource` 404 is an error;
  - lists refetch per kind;
  - feed changes refetch only boards that reference the feed;
  - targets fetch with bounded concurrency after a probe;
  - `client::request` and `client::stream` share one send helper;
  - scheduler stream state is a single enum;
  - layout entries and targets are computed once per frame, with a
    single-pass tree walk.
- Full local workflow passed again: 98 Rust tests, 15 workflow tests,
  formatting, locked Clippy, build, and both Docker UID isolation cases. An X11
  launch confirmed the cold-start auto-start and load, and no full refetch
  across a stream rotation.

## Session 20: layout saving, panel chrome, interaction tests

Scope agreed: layout saving, panel chrome cleanup, and interaction tests; no
API changes. Feeds, boards, and items stay read-only.

- Layouts auto-save (DESIGN.md §6.4) through the existing `PUT /layouts/{name}`
  after a 1-second pause, so a resize drag saves once. Failed saves show in the
  layout bar and retry after 5 s. A save runs on its own worker; its own change
  notice is not mistaken for another window's save.
- Working copies follow saves made elsewhere unless that would lose unsaved
  changes, a save in flight, or an archive panel. In that last case the bar
  offers "Load saved version". With concurrent edits the last save wins.
- **Save as…** (moves the Unsaved arrangement, or copies a saved one) and
  **New layout…** use a name dialog with core name validation; existing names
  are refused, so nothing is overwritten.
- Panel chrome: removed the duplicate header row and Close button. **Show…**
  sits in each tab bar; its entries are prefixed Feed:/Board:. Connection
  errors are one line under the layout bar, with details and setup hints on
  hover, so panels no longer shift.
- Added `egui_kittest` 0.36.2 as a dev-dependency (6 dev-only crates, no egui
  change). Nine interaction tests drive the real window through the
  accessibility tree: sidebar click and reveal, context-menu split, layout
  switch, tab close, tab-bar retarget, tab drag to split, auto-save, and Save
  as (including a refused duplicate name). They were stable over five
  repeated runs.
- Not done (need API sign-off): layout delete/rename, per-user
  last-layout preference, and sidebar counts for unplaced resources.

### Session 20 follow-ups

- Closing the window flushes pending named-layout saves, bypassing the debounce
  and retry delays, and waits for the service to confirm them. A failed save
  offers Retry, Keep window open, or Close without waiting.
- **Add panel…** in the sidebar offers each feed, board, and the deleted-board
  archive with the same placement choices as the right-click menu.
- A layout list fetched while a save is in flight is discarded and refetched,
  so it cannot revert the working copy to the pre-save tree.
- The scheduler no longer reports expired deadlines while a batch is in flight,
  which had let the UI spin until the batch finished.
- Full local workflow passed; the GUI suite was stable over five repeated runs.
  The close flow is covered by headless tests; not manually exercised in X11.

## Session 21: layout management API, list counts, startup layout

Scope agreed: all three pending API additions, then GUI snooze and promote.
No backward compatibility is kept: callboard is unreleased, so the GUI assumes
a matching service.

- Added `PATCH /layouts/{name}` (rename; 404 missing, 409 taken, never
  replaces) and `DELETE /layouts/{name}` (`{"deleted": bool}`), both emitting
  layout notices, with `layout rename OLD NEW` and `layout rm NAME` CLI commands.
- Added migration 0006 and `GET`/`PATCH /preferences` with `last_layout`. Its
  foreign key to `layouts(name)` follows renames and clears on deletion, so the
  preference never names a missing layout. Preferences emit no notice.
- `GET /feeds` adds `item_count`, `snoozed_count`, and `next_wake_at_ms`;
  `GET /boards` adds `todo_count`, `open_todo_count`, and `note_count` (active
  items only). Counts are read in one snapshot. DESIGN.md §6.4 and §8.1 updated.
- GUI: the sidebar shows counts for every feed and board, preferring loaded
  panel contents; the feed list is refetched at the next snooze deadline.
  **Rename…** and **Delete…** (with confirmation) act on the active saved
  layout. Auto-saves of that layout are held while the request is in flight,
  so a debounced save cannot recreate the old name; a rename keeps unsaved
  changes, which then save under the new name. Deleting the active layout
  opens the next saved one. The GUI opens the last active layout at startup
  and records each change of active layout.
- Removed the GUI's tolerance of a service without `/layouts`.
- Tests: store rename/delete/preference/summary behaviour, HTTP and CLI
  routes, backend round trips, workspace rename/delete/preference, the
  feed-list wake, and five interaction tests (list counts, startup preference,
  remembering, rename with refusal and retry, delete with confirmation). A
  mutation check confirmed the counts and remembering tests fail without the
  feature. Full local workflow passed; GUI suite stable over four runs.

### Session 21, part 2: snooze and promote in the GUI

- Feed items have **Snooze** (1 hour, 4 hours, 1 day, 1 week, or until the
  content changes) and **Promote** (board, then todo or note); shown snoozed
  items have **Unsnooze**. Requests use the existing PATCH and promote routes
  on the ordered write worker (formerly layout-only, now `WriteOp`). Success
  refetches the affected feed or board directly, so panels stay current while
  the event stream is down; failures show a dismissible line under the layout
  bar. The layout bar's "Read-only" label is gone.
- Tests: backend snooze/unsnooze/promote round trips with a URL-shaped key
  against a real service, and three interaction tests (timed and until-changed
  snooze with the follow-up refetch, unsnooze of a shown snoozed item, promote
  with a reported failure and dismissal).
- Full local workflow passed; the GUI suite (59 tests) was stable over three
  repeated runs. An X11 launch against an isolated seeded service ran without
  errors and opened the preferred layout "Day" rather than the first by name
  ("Alpha"). A `cosmic-screenshot` capture (after approving the portal's
  access dialog) showed Day open, Rename…/Delete… in the layout bar, sidebar
  counts for unplaced feeds and boards ("alerts 1 +1 snoozed", "Inbox 2"),
  and Snooze/Promote on each visible item. Menus were not opened by hand: no
  Wayland input tool is installed, so interaction relies on the kittest tests.

### Session 21, part 3: desktop click-through

Drove the real window with an absolute-position virtual pointer and keyboard
(uinput via python-evdev; ydotool 0.1.8 only moves relatively, which pointer
acceleration distorts) and checked each step with `cosmic-screenshot` against
an isolated seeded service. Snooze (menu, timed, until changed), Show snoozed,
Unsnooze, Promote (board submenu, as note), Rename (refusal and success,
preference following), Delete (cancel and confirm, switching layouts and
preference), New layout, Add panel (tab, split right, disabled "focused
panel"), the sidebar context menu, and Show… retargeting all worked, with the
service state confirmed over the socket after each write. Fixed what it found:

- The layout-name field now has focus whenever the dialog is open, including
  a prefilled rename (with the name selected, so typing replaces it) and after
  Enter submits a refused name. The Enter check runs before refocusing,
  because `lost_focus()` reflects the current focus.
- Snoozed items say when they wake ("wakes in 1h", "… or when it changes",
  "until it changes"), to the two largest units.
- A note with an empty body no longer shows a blank line; the board summary
  says "1 note".
- Remaining: disabled todo checkboxes dim their titles; left for board editing.

Tests: snooze and plural text, and an interaction test that renames by typing
alone, through a refusal. Full local workflow passed.

## Canvas Phase 0: rendering prototype

`callboard-gui/src/canvas_proto.rs` (run with `cargo run -p callboard-gui
--example canvas_proto`; removed in Phase 3) draws three overlapping cards on a
pannable canvas beside a sidebar. All five gate checks passed in
`egui_kittest` (7 tests) and on the desktop, driven by the absolute uinput
pointer with screenshots: overlap and click-to-front, clipping to the canvas,
title-bar move, corner/edge resize with a minimum, collapse, wheel scrolling a
card versus panning empty canvas (vertical, horizontal wheel, Shift + wheel),
and empty-canvas drag panning.

Chosen approach: the GUI draws cards itself; `egui::Window` is not needed.

- Each card is a child `Ui` at its screen rect (canvas rect offset by the
  view), clipped to card ∩ canvas, drawn back to front. egui's hit-testing
  honours the clip rect, so cards panned under the sidebar never take its
  clicks.
- Registration order is the stacking order: the canvas background first, then
  per card a full-card blocker widget, the title-bar drag area, its buttons,
  the body, and last the resize grips. The blocker stops lower cards from
  reacting through upper ones.
- Raising reads primary presses from the frame's input events. On the real
  desktop a quick click delivered press and release in one frame, and
  `press_origin()` was already cleared, so the first version never raised;
  kittest did not show this until a test sent both events in one frame.
- Wheel routing: only the topmost card under the pointer keeps a wheel
  multiplier of 1 (`ScrollArea` has no on/off switch in 0.36; with 0 a covered
  card neither scrolls nor consumes the delta). Leftover `smooth_scroll_delta`
  over empty canvas pans.

For Phase 3: resize grips are invisible (only the cursor changes) and need a
visible corner mark; the front or focused card needs a highlight; the title
bar needs its own background. Test clicks must use positions from current
geometry — a pan or resize moves every later target.

## Canvas Phases 1–3 (merged)

Merged at the user's choice: the layout format change breaks the tiled GUI, so
format, canvas model, and drawing landed together and were committed only when
everything passed. Stopped at the Phase 3 review point.

- Core: `Layout { view, cards }` with `Card { target, x, y, width, height,
  collapsed }`, back to front; validation of finite coordinates (≤ 1,000,000),
  sizes (1–100,000), ≤ 256 cards, and one card per target. `NamedLayout`
  flattens the layout. Migration 0007 deletes saved trees, clears the
  preference, and renames `tree_json` to `layout_json`; an upgrade test starts
  from a version-6 database with a saved tree and a preference.
- Service and CLI accept the new body unchanged in shape of routes; tests
  cover validation errors, persistence, and the CLI round trip.
- GUI model (`workspace.rs`): `Canvas` with place (centred, cascaded, never a
  second card), reveal (raise, expand, least pan), move, resize (minimum
  220 × 120), collapse, close, retarget (refused onto a placed target), pan,
  and show all. Saved coordinates are whole units; smaller saved cards grow to
  the minimum, and comparisons use that normal form. `Layouts` logic is
  unchanged apart from comparing layouts.
- GUI drawing (`app.rs`): cards drawn back to front inside the canvas area
  with a full-card blocker, title bar (title with counts, Show…, −/+, ×),
  body, resize grips, a corner mark, and a front-card highlight. Raising reads
  presses from events; the wheel scrolls only the topmost card; empty-canvas
  drags and wheels pan. Sidebar: Add card…, click to place or reveal,
  right-click Show/Remove card, drag onto the canvas to place at the drop
  point. Layout bar: Show all. `egui_tiles` and the Phase 0 prototype removed.
- Found on the desktop and fixed with regression tests: ▾ and ✕ are missing
  from egui's fonts (now −, +, ×); and a button on a card behind others needed
  two clicks, because egui mixes a salted child's position into its widget
  ids, so raising a card changed its buttons' ids between press and release.
  Cards now use explicit ids (`UiBuilder::id`).
- Tests: 68 GUI tests, including 13 new canvas interaction tests (placement,
  reveal with pan, context menu, close, Show… with disabled targets, title
  drag saving once, corner resize to the minimum, collapse, overlap raise
  including a one-frame click, pan by drag and wheel versus card scrolling,
  Show all, sidebar drag-to-place, background-card buttons).
- Desktop: placed cards by click and drag, raised, moved, collapsed, panned,
  saved as a layout, and restarted the GUI to confirm view, positions, order,
  and collapse state restore. Full local workflow passed.

## Canvas Phases 4–5: remaining tests and desktop checks

- Tests: resizing by the right and bottom edges (one dimension each, down to
  the minimum), and a deleted board kept as a placeholder card (title
  "· deleted", explanation in the body) that Show… retargets in place. The
  GUI suite passed 15 consecutive runs.
- Desktop, against an isolated seeded service with the uinput pointer:
  resized a card by both edges (saved), scrolled an 80-item feed inside its
  card with the wheel (view unchanged), panned away, and Show all brought the
  cards back. Together with the Phase 3 checks this covers the Phase 5 list.
- Replaced leftover "panel" wording in comments and the `layouts` help text.
  Full local workflow passed.

## Board editing in the GUI

DESIGN.md §6.3 now specifies board cards; the service API was already
complete, so this is GUI-only (new `board.rs`).

- Writes: one `WriteOp::Board(BoardOp)` covers create, rename, and delete
  board, add todo or note, patch an item (done, fields, position, move,
  archive, restore), archive done todos, and delete an item. Each op lists
  its requests and the boards it changes; success refetches those boards and
  the board list without waiting for the notice. Write replies are now a
  `Reply` enum (a renamed layout, a created board, or nothing).
- Board targets fetch `/boards/{id}` and `/boards/{id}/archive` together
  (`Contents::Board { items, archived }`).
- Card: add fields (Enter adds and keeps focus), enabled checkboxes, an item
  menu (…) with Edit… (in-place editor sending only changed fields; notes get
  a color), Move to, Archive, Delete… (confirmed); dotted drag handles that
  reorder within a list; Show archived with Restore; a Board menu (Rename…,
  Archive done, Delete board…). Notes render as colored sticky notes, beside
  the todos when the card is at least 600 wide. The deleted-board archive
  card restores to a chosen board. Board cards use a solid scroll bar so it
  never covers the item menus.
- Sidebar: New board… (creates and places the card) and Rename/Delete in a
  board entry's context menu. The name and delete dialogs are shared with
  layouts (`PromptKind::NewBoard/RenameBoard`, `DeleteSubject`). Deleting a
  board from the GUI closes its card in the active layout.
- Tests: 11 interaction tests (adding, done, editor save/refusal/Escape and
  note colors, drag reorder, move/archive/delete with confirmation, archived
  restore, archive-card restore, board menu rename/archive-done/delete with a
  refusal, new board placement, entry context menu, refetch and error bar),
  draft and op unit tests, and a test that runs every op against the real
  service.
- Desktop: added todos by typing, ticked one, reordered by dragging, edited a
  title (only the title was sent), created a board (card placed) and deleted
  it (card closed, layout saved), with service state checked over the socket.
- Known: a ticked checkbox shows its old state for the moment until the
  refetch arrives (no optimistic update).

## Drag-to-promote

- Feed items have a dotted handle. Dragging it carries the item (egui's
  drag-and-drop payload) with a ghost of its title; the topmost board card
  under the pointer highlights the list it would join (notes over the note
  list, todos anywhere else on the card) and promotes on release. Escape or a
  release elsewhere cancels; the deleted-board archive takes no drops.
- Tests: drops onto the todo list, the note list, and the title bar; drops on
  empty canvas, the feed itself, the archive, and after Escape send nothing.
- Desktop: dragged a feed item over a board's notes (list highlighted, ghost
  under the pointer) and released; the service holds the new note with its
  source reference.
- Known: pressing a handle raises the feed card, which can cover part of an
  overlapping board card during the drag.

## Quick open (Ctrl+K)

- DESIGN.md §6.1 specifies it. `quick.rs` ranks feeds (by title or name),
  boards, the deleted-board archive, and layouts: prefix, then word start,
  then substring, then the typed letters in order; ties keep sidebar order,
  and at most 12 are listed. Up/Down select, Enter or a click opens (the same
  `Show`/`Switch` actions as the sidebar), Escape or a click outside closes.
  The shortcut is consumed before widgets draw, so it works from a text
  field; it is ignored while a dialog is open. **Open…** in the layout bar
  opens it too.
- Tests: ranking and alias matching; Ctrl+K typing then Enter places a card,
  arrow selection, switching layouts by click, no matches, Escape, the
  button, and a click outside.
- Desktop: opened it from a board's add field, filtered, moved with Down, and
  Enter revealed the card. Fixed from that: entries are left-aligned, and the
  hint says Up/Down (egui's fonts lack ↑↓).

## Feed item reordering

- DESIGN.md §6.2 specifies it. The feed item handle now does both: released
  within its own feed's list it reorders (`WriteOp::Reorder`, `PATCH
  .../items/{key}` with `position`); released on a board card it promotes.
  The insertion line and drop logic are shared with board lists
  (`board::reorder_drop`), which now also cancel a reorder released outside
  the list. Hidden snoozed items keep their places: a drop goes just after
  the shown item above it (`feed_position`). **Reset order** appears while
  the feed has a manual order (`reset_order: true`).
- Tests: position mapping with hidden items, a drag within a feed, a drop in
  place sending nothing, Reset order shown only for manual order, and the
  reorder/reset bodies against the real service.
- Desktop check not done: `cosmic-screenshot` timed out (the portal
  likely wanted approval while the user was away).

## GUI polish

- Ticks show at once: `App::ticks` overrides a todo's done state from the
  click until a reload agrees (or, after the service confirmed, two reloads
  disagree, meaning another window changed it back); a refusal reverts it.
  The board header's open count follows.
- Feed item handles do not raise their card: each frame records visible feed
  handles with their card (`board::keep_in_place`), and a press there skips
  the raise. Other presses on the card still raise it.
- Tests: tick shown before the reply, reverted on failure, yielding to the
  service after two disagreeing reloads; promoting from a feed covered by a
  board leaves the board in front.

### Visual pass

Rendered every card type, menu, editor, and dialog in dark and light themes
with egui_kittest's wgpu renderer (in a scratch worktree with the `wgpu` and
`snapshot` dev features; the repository's dependencies are unchanged), since
the desktop screenshot portal was unavailable. Fixed what it showed:

- Light theme: the canvas was as pale as the cards; it is now mid-grey.
- Notes use the strong text color, so text reads on colored notes in dark.
- Done todos are struck through and dimmed.
- Ages ("submitted …", "updated …", sidebar hover) keep their two largest
  units, as snooze wake times already did.
- The deleted-board archive's header no longer says "(n open)".

Dialogs, menus, quick open, editors, and the error bar looked right in both
themes.

## Suggested next

Polish from real use. Feed items still show Snooze and Promote on every
item; a … menu like board items have would quiet long feeds.
