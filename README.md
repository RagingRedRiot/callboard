# callboard

A Linux per-user bulletin board for tool-submitted feeds, todos, and notes.
[DESIGN.md](DESIGN.md) describes the intended application.

The Linux service and CLI support feeds, boards, todos, and notes, backed by
SQLite through SQLx. The API also provides layouts, promotion, feed view state,
and change events. A desktop GUI (managing layouts, snoozing, and promoting feed items) is available; MCP is not implemented
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

`put` accepts arrays or full snapshot objects; `--title`, `--description`,
`--source-url`, and `--stale-after` override object metadata. Empty input
fails; `[]` clears the feed.

A full snapshot object for a tracking script, with a description saying what
the feed is and per-item details for the GUI's hover card:

```json
{"title": "myrepo — review requests",
 "description": "Open PRs in org/myrepo where I'm a requested reviewer",
 "source_url": "https://github.com/org/myrepo/pulls",
 "stale_after": "1h",
 "items": [{"key": "https://github.com/org/myrepo/pull/42",
            "title": "Fix auth race",
            "url": "https://github.com/org/myrepo/pull/42",
            "body": "Fixes the token refresh race in the session store.",
            "tags": ["review-requested"],
            "meta": {"author": "sam", "checks": "failing", "opened": "2026-09-27"}}]}
```

The service tracks what each submission changed: items are **new** or
**updated** until seen, and the feed keeps its last change with the titles of
removed items (DESIGN.md §4.3). Every field counts as content, so send stable
values (an "opened" date, not an age) or items show as updated on every run.
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
callboard feed seen reviews 'item-key'   # or omit the key to mark all seen
printf '%s' '{"view":{"x":0,"y":0},"cards":[{"target":{"kind":"feed","name":"reviews"},"x":0,"y":0,"width":420,"height":520,"collapsed":false}]}' | callboard layout save 'Review day'
callboard layouts
callboard layout rename 'Review day' 'Reviews'
callboard layout rm 'Reviews'
```

Pass literal item keys and layout names, including URLs, Unicode, and `%`;
the CLI encodes them for the API. Feed PATCH accepts `snoozed_until_ms` (UTC Unix
milliseconds or null), `wake_on_update`, `position`, and `reset_order`. To clear
both snooze conditions, send `{"snoozed_until_ms":null,"wake_on_update":false}`.
Feed patch input is limited to 16 KiB. `layout save NAME` accepts a layout
object (`view` and back-to-front `cards`) up to 64 KiB and replaces that layout
atomically. `layouts` returns all saved layouts. `layout rename` never replaces
an existing layout, and `layout rm` reports whether the layout existed. See
DESIGN.md §6.4 for the layout schema. These commands support
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
same kernel UID checks, data lock, and process lifecycle as CLI requests.

The window is a canvas of cards, one per feed or board (DESIGN.md §6.1).
Cards overlap; clicking anywhere on a card brings it to the front. Drag a
card's title bar to move it, drag its right or bottom edge or corner to resize
it, **−** collapses it to its title bar (which keeps showing counts) and **+**
expands it, and **×** removes it from the layout. **Show…** in the title bar
points the card at another feed or board; targets that already have a card are
disabled. A long feed scrolls inside its card.

Drag empty canvas, or use the wheel over it (Shift + wheel for horizontal), to
pan; over a card, the wheel scrolls that card. **Show all** in the layout bar
pans back to the cards. Clicking a sidebar entry pans to its card or places a
new one in the middle of the view; **Add card…** offers the same, the
right-click menu can also remove a card, and dragging an entry onto the canvas
places its card at the drop point. The sidebar lists saved layouts, feeds with
error/stale markers, and boards, each with its item count (visible and snoozed
for feeds). **Ctrl+K** (or **Open…** in the layout bar) finds a feed,
board, or layout by name: type part of it, pick with Up/Down or the pointer,
and press Enter to reveal or place the card, or to switch layouts. The GUI
opens the layout that was active when it last ran, or else
the first saved layout by name. Cards whose feed or board was deleted stay in
place as placeholders.

The GUI subscribes to `GET /events` and refetches only what changed, handling
the initial resync, lag resyncs, and reconnects with backoff. Routine stream
rotation does not refetch everything; see DESIGN.md §6.5. While the stream is
down for more than two seconds it polls every five seconds instead; the status
bar shows Live, Connecting, or Polling. A failed refetch keeps a card's last
loaded contents and retries that card alone after five seconds. HTTP(S) links
open only when clicked; other URL schemes display as text.

Arrangement changes save to the active layout automatically after a
one-second pause (`PUT /layouts/{name}`); the layout bar shows saving, saved,
or a failed save that is retried. Closing the window flushes pending named-layout
changes and waits for confirmation; if saving fails, you can retry, keep the
window open, or explicitly close without waiting. **Save as…** stores the current arrangement
under a new name and **New layout…** creates an empty one. **Rename…** and
**Delete…** act on the active saved layout; deleting it switches to the next
saved layout. Without any saved
layout the window starts in an unnamed "Unsaved" arrangement. The
deleted-board archive can be shown but is not stored in layouts.

Feed items are compact rows: title and link, with a **new** or **updated**
badge. Rest the pointer on one for a second to see everything about it (body,
tags, `meta` key/values, key, when it was added and changed); that marks it
seen. A feed card shows the feed's description and last change ("Changed 10m
ago: 2 new · 1 gone"; hover for the gone titles) and **Mark all seen**. Its
**…** menu has **Snooze** (for an hour, four hours, a day, a week, or until
its content changes) and **Promote** (to a board as a todo or note). Snoozed
items appear under **Show snoozed**, where the menu offers **Unsnooze**. A failed action shows a
dismissible message under the layout bar.

Board cards are editable (DESIGN.md §6.3). Type into **Add a todo** or **Add a
note** and press Enter; tick a todo's checkbox to mark it done. Each item's
**…** menu edits it in place (title, details, link, and a color for notes),
moves it to another board, archives it, or deletes it after confirmation. Drag
the dotted handle at an item's left to reorder it. **Show archived** lists
archived items with **Restore**. The **Board** menu renames the board,
archives its done todos, or deletes it (its items move to the deleted-board
archive, whose card restores them to a chosen board). **New board…** in the
sidebar creates a board and places its card. Drag a feed item by its dotted
handle within its feed to reorder it (**Reset order** returns to the feed's
own order), or onto a board card to promote it: over the notes it becomes a note,
anywhere else on the card a todo (Escape cancels).

Graphics dependencies are confined to the GUI
crate; building `callboard` alone does not build eframe. A Wayland or X11 desktop
with OpenGL support is required to launch the window.
See [PROGRESS.md](PROGRESS.md) for session scope and the next step.
