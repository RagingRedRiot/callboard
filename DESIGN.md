# callboard design

callboard is a Linux per-user bulletin board implemented in Rust. A background
service stores feeds, todos, and notes; a desktop GUI pins them as cards on a
canvas; scripts submit feeds through the CLI, and a stdio MCP server
lets AI clients read the board and add todos and notes. This document describes
the intended design before implementation; the schema, API, and CLI are not yet
stable interfaces.

## 1. Purpose

Centralize the queues a workstation user watches — review requests, issues,
tickets, alerts — alongside the todos and notes that come out of them. Tools
decide what to fetch and submit the results; callboard stores and shows them.

callboard is deliberately not an integration. It never contacts GitHub, JIRA, or
any other source, holds no credentials for them, and does not interpret what
tools submit beyond a small common item shape.

callboard does not raise desktop notifications. Scheduling, fetching, and popups
belong to the submitting tool; the intended pairing is cued (§9), which runs the
fetch on a schedule and notifies when a submission reports new items.

### 1.1 Scope

One user on one Linux workstation. There is no network listener, no sync, and no
multi-user sharing. macOS is possible future work (§12) and does not shape the
alpha.

## 2. Concepts

| Term | Owner | Lifetime |
|---|---|---|
| **Feed** | a submitting tool | until the user deletes it |
| **Item** | the feed's tool | until a snapshot omits it |
| **Board** | the user | until the user deletes it |
| **Todo** | the user | until archived or deleted |
| **Note** | the user | until archived or deleted |
| **Layout** | the user | until the user deletes it |

A feed is the section a tool declares by name. Each submission to a feed is a
complete snapshot of its items. Boards hold the user's todos and notes; feeds
never contain todos or notes, and boards never contain feed items. A todo or
note can reference a feed item it was promoted from (§5.3). A layout arranges
feeds and boards as cards on a canvas (§6).

The service owns all persistent state, including layouts. The GUI is a client
and holds only window geometry.

## 3. Feeds

### 3.1 Identity

A feed is identified by its name, chosen by the submitting tool. Names match
`[a-z0-9][a-z0-9._-]{0,63}`. The first submission to an unknown name creates the
feed; later submissions replace its items. Any same-user tool can submit to any
feed name; callboard does not track which tool owns a feed.

Feed metadata travels with each submission and replaces the stored values:

- `title` — display name; defaults to the feed name.
- `description` — optional plain text, at most 1000 characters, saying what
  the feed tracks ("Open PRs in org/repo awaiting my review").
- `source_url` — optional link to the queue the feed mirrors.
- `stale_after` — optional duration after which the feed is marked stale (§3.5).
- `new_for` — optional duration (`"24h"`) for which items show as new or
  updated (§4.3). Without it, items are never marked.

### 3.2 Items

An item has a `key`, unique within its feed and stable across submissions; a
URL is the usual choice. Other fields:

- `title` — required, plain text.
- `url` — optional link opened from the GUI (§10.4).
- `body` — optional plain text.
- `tags` — optional list of short strings.
- `color` — optional: `red`, `orange`, `yellow`, `green`, `blue`, `purple`,
  `pink`, or `gray`. The GUI tints the item's row with it. Any other value is
  rejected, so a misspelling fails the submission rather than vanishing.
- `meta` — optional flat object of string, number, or boolean values, shown as
  key/value pairs and otherwise uninterpreted.

The submitted order is the tool's order and the default display order (§4.2).

### 3.3 Snapshot semantics

A submission replaces the feed's items atomically. Comparing by key, the service
classifies each item:

- **added** — key not present in the previous snapshot.
- **removed** — key absent from this snapshot; the item and its view state
  (§4) are deleted.
- **updated** — key present in both, content hash differs.
- **unchanged** — key present in both, content identical.

The response reports the counts and the added, removed, and updated keys.
Content comparison uses decoded item fields; JSON whitespace and object-key
order do not matter. Item order and feed metadata do not count as item updates.
Added and updated keys follow the submitted order; removed keys follow the
previous snapshot's order.

Resubmitting an identical snapshot leaves item content and view state unchanged.
Every accepted snapshot refreshes the last-submitted time and clears any fetch
error, even when all items are unchanged. An explicitly empty snapshot (`[]`)
is valid and clears the feed. Submissions to one feed are serialized; the last
accepted submission wins.

A key that disappears and later returns is a new item: its earlier view state is
gone. References from todos and notes (§5.3) resolve by key, so they become live
again when it returns.

### 3.4 Failure reporting

A tool whose fetch fails must not submit an empty snapshot, which would clear
the feed. Instead it reports the failure: the service records the message and
time, marks the feed as errored, and leaves the items unchanged. The next
accepted snapshot clears the error.

Reporting failure for an unknown feed returns an error; the first accepted
snapshot creates the feed (§3.1).

`callboard put` rejects empty standard input, so a failed upstream command in a
pipeline cannot clear a feed by producing no output. Clearing requires the
literal `[]`.

### 3.5 Staleness

When a feed declares `stale_after` and no snapshot has arrived within that
duration, the GUI marks it stale with the time of the last submission. Staleness
is display only: it never removes items or the feed. It exists so a broken or
disabled tool is visible instead of silently showing old data.

### 3.6 Deletion

Feeds do not expire. The user deletes a feed from the GUI or CLI, removing its
items and view state. References to its items become "source gone" (§5.3). A
later submission to the same name creates a new feed.

## 4. Item view state

The user's state for a feed item is stored apart from the submitted content,
keyed by feed and item key, so snapshots never overwrite it. It is deleted when
the item is removed.

### 4.1 Snooze

A snoozed item is hidden from its feed card, which shows a count of snoozed
items. A snooze ends:

- at a chosen instant, or
- when the item is next **updated** (§3.3) — useful for "wake me when this PR
  changes" — or
- at whichever of the two comes first.

Waking is silent; callboard does not notify (§1). An item removed while snoozed
is simply gone.

### 4.2 Order

A feed displays in the tool's order until the user drags an item. From then the
feed has a manual order: the service stores the ordered keys, inserts newly added
items at the top, and prunes removed keys. Resetting returns the feed to the
tool's order.

There is no dismiss state. Snooze covers "not now"; the source covers "done" by
dropping the item from the next snapshot.

The service stores an optional snooze-until instant as UTC Unix milliseconds and
a `wake_on_update` flag. Either condition can end a snooze, and both may be set.
Manual positions are zero-based. Newly submitted items are inserted before the
existing manual order; reset removes manual order and returns to submitted order.

### 4.3 Changes

The service records, per item, when it was added and when its content last
changed (an **updated** submission, §3.3), and per feed the last submission
that added, updated, or removed anything: its time, counts, and the keys and
titles of up to 20 removed items.

A feed's `new_for` window decides how items are marked, from these times
alone: an item is **new** while less than `new_for` has passed since it was
added, and otherwise **updated** while less than `new_for` has passed since its
content last changed. Viewing items changes nothing; marks end when the window
does. Without `new_for` nothing is marked.

A feed's first submission is its baseline: it is not reported as a change, and
its items have no added time, so they are never new (a newly set-up feed is
not all badges). Items that existed before change tracking are the same. Both
are marked updated when their content later changes.

Content includes every item field, `color` included, so a tool that puts a
volatile value (an age, a relative time) in `meta` or the body marks the item
updated on each change of that value. Tools should send stable values
(timestamps, not ages).

## 5. Boards, todos, and notes

### 5.1 Boards

A board is a named, user-created collection of todos and notes, with an
optional color. Boards are created, renamed, recolored, and deleted from the
GUI or CLI. Deleting a board requires it to be empty or an explicit
confirmation that archives its contents.

### 5.2 Todos and notes

A todo has a title, optional body, URL, and color, a done flag, an optional
reference (§5.3), and a position in its board. A note has a body, an optional
title, URL, color, and reference, and a position in its board; the GUI renders
notes as sticky notes beside the board's todo list. Colors on boards, todos,
and notes are set by the user and come from the feed item palette (§3.2):
`red`, `orange`, `yellow`, `green`, `blue`, `purple`, `pink`, or `gray`; any
other value is rejected.

Todos and notes never expire. Marking a todo done keeps it visible until it is
archived. Archiving hides an item into the board's archive, from which it can be
restored; deleting is permanent. Todos and notes can be moved between boards.

Tools can create todos and notes through the API (§8), for example an MCP client
recording a follow-up. Once created, they are the user's: no later submission
replaces them.

### 5.3 Promotion and references

Promoting a feed item creates a todo or note in a chosen board. The new object
copies the item's title and URL, copies its body when present, and stores a
reference `{feed, key}`. For a note, the copied URL is kept in the note's URL
field and the copied body becomes its note body.

References resolve at display time:

- **live** — the feed holds an item with that key. The GUI shows its current
  title and can reveal it in its feed card.
- **source gone** — the item or its feed no longer exists. The GUI shows the
  copied title and URL with a "source gone" marker.

Board reads (`GET /boards/{id}`, `GET /boards/{id}/archive`, and `GET /archive`)
include a display-only `resolved_reference` on each todo and note:

- `null` when the object has no reference;
- `{"status":"live","item":{...}}` with the current full feed item;
- `{"status":"source_gone"}` when the referenced feed/key is absent.

The stored `reference` and copied fields remain unchanged. If the same feed/key
reappears, it resolves live again. Resolution uses the board read's database
snapshot, including archived objects. Mutation responses contain stored fields;
clients refetch the board to obtain current display-time resolution.

A reference never keeps a feed item alive, and removing an item never alters the
todo or note that references it. "Source gone" is itself useful: the tracked
thing has resolved.

## 6. GUI

### 6.1 Canvas and cards

The window is a canvas — a callboard — with a sidebar beside it. Each feed or
board placed on the canvas is a **card**: a free-floating, resizable window with
a title bar. Cards may overlap; clicking anywhere on a card brings it to the
front. The user arranges cards however suits the work: a wide feed next to a
narrow board, a stack of small status cards in a corner.

- **Move** by dragging the title bar; **resize** by dragging an edge or corner.
  Cards have a minimum size that keeps the title bar and a few rows readable.
- **Collapse** folds a card to its title bar, which keeps showing the target's
  name, counts, and error/stale marker, so a collapsed card works as a compact
  status tile. Expanding restores its previous height.
- **Scroll** within a card: contents never grow a card, so a feed of hundreds of
  items stays the size the user gave it and scrolls inside.
- **Close** removes the card from the layout; the feed or board is unaffected.
- **Show…** (the swap button in the title bar) points the card at a different
  feed or board.

A layout holds at most one card per feed or board. Targets already on the
canvas are shown but disabled in **Show…**.

The canvas is unbounded and pans: drag empty canvas, or scroll the wheel or
trackpad over empty canvas (Shift + wheel pans horizontally). Over a card, the
wheel scrolls that card's contents. There is no zoom. Resizing the window shows
more or less of the canvas and never moves cards. **Show all** pans so the
top-left of the cards' bounding box is in view, recovering cards panned out of
sight.

A sidebar lists every feed and board with item counts and error/stale markers.
Clicking an entry reveals its card (panning to it, bringing it to the front, and
expanding it if collapsed) or, when it has none, places a new card in the middle
of the view, offset from any card already there. **Add card…** offers the same.
Dragging an entry onto the canvas places its card at the drop point.

Feeds that are not placed still accept submissions and stay current.

**Quick open** (Ctrl+K, or **Open…** in the layout bar) finds a feed, board,
the deleted-board archive, or a saved layout by name. Typing narrows the list;
names that start with the text rank first, then word starts, then matches
anywhere, then the typed letters in order. ↑/↓ move the selection; Enter or a
click opens it: a feed or board card is revealed or placed as from the
sidebar, and a layout is switched to. Escape or a click outside closes it.

### 6.2 Feed cards

A feed card shows the feed's title, source link, last-submitted time, and any
error or stale marker, then its visible items in display order (§4.2). Each
item is a compact row: its title, its link (without the scheme, clicked to
open), and its snooze state if snoozed. The row's **…** menu snoozes,
unsnoozes, or promotes it to a board as a todo or note. **Show snoozed**
reveals snoozed items. A row with a `color` is tinted with it, like a sticky
note. New and updated items carry a **new** or **updated** badge (§4.3).
Under the feed's counts the card shows its description and its last change
("Changed 10m ago: 2 new · 1 updated · 1 gone", the gone titles on hover). The
sidebar shows each feed's count of new and updated items.

**Details**: resting the pointer on a row highlights it, a line fills along its
foot, and after one second a card beside the pointer shows everything about
the item: title, full link, new/updated status with when it was added and last
changed, snooze state, body, tags, every `meta` key and value, its color, and
its key. Passing over rows, dragging, or an open menu shows no card; moving
off the row hides it.

Each item has a drag handle. Released within its own feed's list, the item
moves there (giving the feed a manual order, §4.2); released on a board card,
it is promoted (§6.3). Snoozed items keep their places when hidden: a drop
between two shown items puts it just after the upper one. **Reset order**,
shown while the feed has a manual order, returns it to the tool's order.
Pressing the handle does not bring its card to the front, so the card never
covers the board it is being dragged to.

### 6.3 Board cards

A board card shows the board's todos as a checklist and its notes as sticky
notes, below the todos or beside them when the card is wide enough. Edits go
to the service at once; the card refetches and shows what was stored.

- **Add**: a field above each list. Enter adds a todo with that title, or a
  note with that text, and keeps the field ready for the next one.
- **Complete**: a todo's checkbox marks it done or not done, showing the new
  state at once and reverting if the service refuses. Done todos stay
  until archived; **Archive done** in the board menu archives them together.
- **Edit**: an item's **…** menu opens an editor in its place for the title,
  body, link, and color. A colored todo's row is tinted, like a sticky note. Save (or Enter in a single-line
  field) stores only the fields that changed; Cancel or Escape discards.
- **Reorder** by dragging the handle at an item's left within its list.
- **Move to** another board, **Archive**, or **Delete…** (permanent, after
  confirmation) from the same menu.
- **Show archived (n)** lists the board's archived items with **Restore** and
  **Delete…**.
- The **Board** menu renames the board, sets its **Color** (a band along the
  card's top edge and a dot on its sidebar entry), archives
  done todos, or deletes the board. The confirmation says that its todos and notes, archived ones
  included, move to the deleted-board archive. Deleting a board from the GUI
  closes its card in the active layout.
- **New board…** in the sidebar creates a board and places its card;
  right-clicking a board entry offers rename and delete too.

The deleted-board archive card lists its items with the board each came from,
**Restore to** a chosen board, and **Delete…**.

Dragging a feed item by its handle onto a board card promotes it (§5.3): over
the notes it becomes a note, anywhere else on the card a todo. The list it
will join is highlighted while the item is over it; releasing elsewhere, or
pressing Escape, cancels. The deleted-board archive card accepts no drops.

### 6.4 Layouts

A layout is the arrangement of cards on the canvas: each card's target,
position, size, and collapsed state, the cards' front-to-back order, and the
canvas view position. The GUI saves changes to the active layout through the
API as they happen, after a one-second pause, so a drag or resize saves once
when it ends; panning and bringing a card to the front are saved the same way.
The user can keep several named layouts — one per project or kind of day — and
switch between them. Switching layouts never changes feeds or boards.

A card whose feed or board has been deleted stays in place as a placeholder
until the user retargets or closes it. The deleted-board archive can be shown
as a card but is not stored in layouts.

The layout API is independent of the GUI toolkit. `GET /layouts` returns all
saved layouts sorted by name, each with `name`, `view`, `cards`, and
`updated_at_ms`. `PUT /layouts/{name}` accepts `{"view": ..., "cards": [...]}`
and atomically creates or replaces that name, returning the saved layout with
HTTP 200. Names are case-sensitive, 1–100 Unicode characters, without
surrounding whitespace or control characters; encode the name as one URI path
segment. An empty list is valid before any save.

`PATCH /layouts/{name}` with `{"name": "New name"}` renames a layout and returns
it with a new `updated_at_ms`; a missing layout is 404, and an existing layout
with the new name is 409 (never replaced). Renaming to the same name changes
nothing. `DELETE /layouts/{name}` returns `{"deleted": true}`, or `false` when
the layout was already absent. Both emit layout notices: a rename names the old
and the new layout.

`GET /preferences` returns `{"last_layout": "Day"}` (or null): the layout the
GUI opens at startup. `PATCH /preferences` sets it; `last_layout` must name an
existing layout (404 otherwise) or be null, and an omitted field is unchanged.
The preference follows layout renames and clears when its layout is deleted.
Preference changes emit no notice. The GUI records the active layout whenever
it changes and opens it at startup, falling back to the first layout by name.

A layout body:

```json
{
  "view": {"x": -40, "y": 0},
  "cards": [
    {"target": {"kind": "feed", "name": "reviews"},
     "x": 0, "y": 0, "width": 420, "height": 560, "collapsed": false},
    {"target": {"kind": "board", "id": 2},
     "x": 440, "y": 0, "width": 360, "height": 300, "collapsed": true}
  ]
}
```

Coordinates are canvas units (the GUI's logical pixels at 100% scale), with y
increasing downward. `view` is the canvas point shown at the top-left of the
canvas area. `cards` is ordered back to front: the last card is drawn on top.
Targets are `{"kind":"feed","name":...}` or `{"kind":"board","id":...}` with a
user board ID greater than 1. `height` is the expanded height, kept while a card
is collapsed. An empty layout has no cards.

Unknown fields, invalid targets, and duplicate targets are rejected.
Coordinates must be finite with magnitude at most 1,000,000; widths and heights
must be finite, from 1 to 100,000 (the GUI enlarges cards below its minimum
size when displaying them). A layout holds at most 256 cards, and save bodies
are limited to 64 KiB. Targets are validated syntactically, not checked for
existence. Invalid saves preserve the previous layout. Saving layouts does not
mutate feeds or boards, and target deletion does not modify stored layouts.

### 6.5 Live updates

The GUI subscribes to the service's event stream (§8.2) and refetches whatever
changed. Submissions appear without refresh; GUI edits made in one window apply
to others.

The GUI refetches on every resync except one: when a stream that lasted at
least 20 seconds ends cleanly (the 25-second rotation) and the next connects
within 2 seconds, it skips that stream's opening resync. Shorter streams that
end cleanly (a service shutting down) always refetch. It refetches everything open at least every 5
minutes, which bounds how long a write landing in that millisecond reconnect
gap can go unseen. A resync mid-stream (lag) always refetches.

### 6.6 Toolkit

egui via eframe. Cards are drawn inside the canvas area in the layout's
back-to-front order and clipped to it, so they never cover the sidebar or the
layout bar; the GUI maps canvas coordinates to the screen with the view offset.
egui supplies the pieces — child areas, scroll areas, and drag sensing — and
the GUI owns card geometry and stacking order, which are exactly what a layout
stores.

The look is one set of design tokens (`callboard-gui/src/theme.rs`) applied to
egui's style for both themes: slate neutrals and a single teal accent taken
from the app icon, a ladder of surface tones with hairline borders and a soft
card shadow, radii of 4, 6, and 10 points, and an 8-point spacing rhythm. Text is
Inter (regular, medium, semibold) and icons are Phosphor, both bundled in the
binary with their licenses. Icons have a font family of their own, never a
fallback for text. Buttons are ghosts until hovered; icon buttons carry
accessible names. Item colors are a bar at a row's edge, not a filled row;
notes keep a tint blended from the same color. A card placed by the GUI
cascades off any card whose title bar it would cover.

## 7. Components

A Cargo workspace:

- `callboard-core` — model, store, validation, API types.
- `callboard` — one binary for the service (`callboard serve`), the CLI, and the
  stdio MCP server (`callboard mcp`).
- `callboard-gui` — the egui desktop app, kept separate so the CLI and service do
  not link graphics dependencies.

### 7.1 IPC

The service speaks HTTP/1.1 with JSON bodies over a Unix socket and verifies
peer credentials supplied by the kernel. There is no TCP listener. Plain HTTP
keeps ad-hoc tools simple: `curl --unix-socket` can submit a feed.

The socket uses `$XDG_RUNTIME_DIR/callboard.sock`, or
`/run/user/<uid>/callboard.sock` when that variable is unset or invalid. If the
chosen runtime directory does not exist, it falls back to
`$XDG_DATA_HOME/callboard/run/callboard.sock` (by default
`~/.local/share/callboard/run/callboard.sock`). An existing but unsafe runtime
directory is rejected. `CALLBOARD_SOCKET_DIR` overrides the socket directory;
use separate `XDG_DATA_HOME` and `XDG_CONFIG_HOME` values as well for a fully
isolated deployment. Socket-directory overrides must be absolute.

### 7.2 Service lifecycle

`callboard setup` installs and enables a systemd user service for future logins;
`setup --print` previews the unit. Setup leaves an already running on-demand
service alone. When no service is running, `systemctl --user start
callboard.service` starts the installed unit immediately. The unit records the
current executable and resolved paths; rerun setup after moving the binary.
Without a running service, the CLI and GUI start it on demand through the same client support; the GUI defaults to auto-start too and offers `--no-auto-start` for a required existing service. `--no-auto-start` requires an existing service. A
data-directory lock prevents competing services;
startup checks ownership before replacing a stale socket. Closing the GUI does
not stop the service.

The data lock is held for the service lifetime, and its file is never unlinked.
While holding it, startup removes an existing socket only if it is owned by the
user and a nonblocking connection attempt reports connection refused. A live
socket, symlink, or other unexpected entry is preserved and startup fails.
Normal shutdown removes only the socket inode created by that service. A crash
releases the kernel lock; the next startup can reclaim the stale socket.

### 7.3 Store

SQLite via SQLx (`sqlx`) in WAL mode under `$XDG_DATA_HOME/callboard/`. Use SQLx
for database access and migrations. Each snapshot is applied in one transaction.
Configuration lives under `$XDG_CONFIG_HOME/callboard/`. Data and config
directories are private to the user.

### 7.4 Upgrade and uninstall

Installing a new build replaces the files but not the running service, which
keeps executing the old image. `callboard upgrade` moves the running service
onto the binary now installed at the path it was started from, in place:

1. The client connects without auto-starting (a fresh service would already be
   the installed build) and sends `POST /service/upgrade`.
2. The service checks the binary first. A path that is missing or not
   executable, or a binary that fails `--version` within 10 seconds, abandons
   the upgrade with nothing changed. A path naming the image already running
   (same inode as `/proc/self/exe`) reports that the service is current.
3. It stops accepting connections but keeps the listening socket open, replies
   that it is upgrading, ends its event streams, waits up to 5 seconds for
   in-flight requests, and closes the store.
4. It re-executes the binary as `serve --handoff LOCK,LISTENER`, passing the
   data lock and the listening socket across the exec. The PID, the lock and the
   socket never go away, so a supervisor sees no restart, no other service can
   take the lock in between, and clients connecting meanwhile wait in the
   socket's backlog. The new image adopts the descriptors only after checking
   they are this deployment's lock file (same inode, lock re-asserted) and
   socket (bound to this socket path). It then starts up as usual, including
   migrations.
5. The client connects again, waits for `/health`, and compares the inode of
   the answering process's `/proc/PID/exe` (PID from peer credentials) with the
   installed binary. If the exec failed, the service re-executes its own running
   image instead, and the client reports the failure.

Only a service started by `callboard serve` can upgrade; an in-process test
service cannot. One upgrade runs at a time. A stop signal during the drain ends
the service rather than re-executing it. The upgrade route and its replies keep
a frozen shape, so any future CLI can ask an older service to upgrade. Their
statuses are 202 `{"upgrade":"started","executable":PATH}`, 200
`{"upgrade":"current","executable":PATH}`, and 409
`{"upgrade":"abandoned","reason":TEXT}`. A service that predates the route
answers 404; restart it once by hand.

`/health` reports the package `version` and a `build` identity: the Git commit,
with `-dirty` and a digest of the uncommitted diff when the tree had changes, or
`unknown` outside Git. The event stream carries the same identity in an
`x-callboard-build` header. The GUI compares it with its own build. A
difference means one of them is out of date, typically an open GUI after an
upgrade, and the status bar says to run `callboard upgrade` and reopen the GUI.
`callboard upgrade` also lists open GUI processes.

`callboard uninstall` removes everything callboard put on the account:

1. It lists what it will remove: the generated systemd unit, the running service
   (by PID), the data directory with its size and feed and board counts, the
   socket, and with `--purge` the config directory. It also lists open GUI
   processes, which would auto-start a fresh, empty service. It removes nothing
   without interactive confirmation or `--yes`.
2. It disables and stops the unit (`systemctl --user disable --now`), deletes the
   unit file, and reloads systemd, so no supervisor restarts the service. A
   custom `callboard.service` is not ours to remove, so uninstall refuses until
   it is gone.
3. It signals whatever process serves the socket at that moment. The process is
   held by pidfd, confirmed by a second peer-credential check, so a reused PID is
   never signalled.
4. It deletes nothing until it holds the data lock, which proves no service
   remains and keeps an auto-started one from opening the store mid-delete. It
   waits up to 30 seconds for the signalled process (shutdown drains for at most
   5), and at most 10 seconds when the lock holder cannot be identified. Either
   failure leaves the unit removed and says to stop the service and rerun.
5. It deletes the socket and the data directory, then the config directory with
   `--purge`. Recursive deletes apply only to real directories named
   `callboard`, so an unusual XDG value can make uninstall refuse but cannot widen
   what it deletes. It never auto-starts the service.

The binaries belong to Cargo; uninstall names them and leaves them in place.

`setup --status` reports the unit file, whether it is generated or custom,
whether it is enabled and active, and its executable. `setup --uninstall`
disables and deletes the generated unit and leaves a running service running,
the counterpart of setup leaving one alone.

## 8. API

### 8.1 Resources

| Method and path | Purpose |
|---|---|
| `GET /health` | Identify the service, API version, package version, and build (§7.4) |
| `POST /service/upgrade` | Re-execute the installed binary in place (§7.4) |
| `PUT /feeds/{name}` | Submit a snapshot (§3.3); returns the change summary |
| `POST /feeds/{name}/error` | Report a failed fetch (§3.4) |
| `GET /feeds`, `GET /feeds/{name}` | List feeds; read one with items and view state |
| `DELETE /feeds/{name}` | Delete a feed (§3.6) |
| `PATCH /feeds/{name}/items/{key}` | Set snooze or position |
| `GET /boards`, `POST /boards` | List or create boards |
| `PATCH /boards/{id}`, `DELETE /boards/{id}` | Rename or recolor (`{"name"}`, `{"color"}`, or both), or delete a board |
| `POST /boards/{id}/todos`, `POST /boards/{id}/notes` | Create a todo or note |
| `PATCH /todos/{id}`, `PATCH /notes/{id}` | Edit, move, complete, archive, restore |
| `DELETE /todos/{id}`, `DELETE /notes/{id}` | Delete permanently |
| `POST /feeds/{name}/items/{key}/promote` | Promote to a todo or note (§5.3) |
| `GET /layouts`, `PUT /layouts/{name}` | Read or save layouts |
| `PATCH /layouts/{name}`, `DELETE /layouts/{name}` | Rename or delete a layout (§6.4) |
| `GET /preferences`, `PATCH /preferences` | Read or set the startup layout (§6.4) |
| `GET /events` | Change stream (§8.2) |

Todo and note PATCH bodies contain only changed fields. JSON `null` clears an
optional field such as a body, URL, title, color, or reference. Their editable
fields also include `done` for todos and a zero-based `position` within the
board's active list of the same item type. `board_id` moves an item; `archived`
archives it when true and restores it when false. Restoring an item from the
archive for a deleted board requires `board_id` to select its destination.
Moving alone preserves archive state; restoring requires `archived: false`.
Archived items cannot be reordered until restored. Unknown PATCH fields and
null values for non-nullable fields are rejected. Board item titles and bodies
use the same content limits as feed items (§8.3).

Feed item PATCH bodies can set `snoozed_until_ms` (UTC Unix milliseconds),
`wake_on_update`, or zero-based `position`. A null `snoozed_until_ms` clears the
time condition; setting both snooze fields uses whichever wake condition comes
first. `reset_order: true` returns the feed to submitted order and cannot be
combined with `position`. Item keys in request paths must be percent-encoded
as a single URI segment (including slashes in URL-shaped keys); the service
decodes that segment exactly once.

Feed metadata includes `description`, `new_for`, and `last_change` (null, or
`{"at_ms", "added", "updated", "removed", "removed_items": [{"key", "title"}]}`).
`GET /feeds/{name}` adds `changes`, mapping each key to `{"added_at_ms",
"changed_at_ms", "status"}`; `status` (`"new"`, `"updated"`, or null) is
evaluated at read time against `new_for`.

`GET /feeds` entries add `item_count`, `snoozed_count`, `new_count` (items new
or updated now), and `next_wake_at_ms` (the earliest future time a snooze or a
new/updated window ends, or null) to the feed metadata; counts are evaluated at
read time, so clients refetch the list at that deadline.
`GET /boards` entries add `todo_count`, `open_todo_count`, and `note_count`,
counting only items that are not archived.

Promotion requests provide `board_id` and `kind` (`todo` or `note`). Promotion
copies the current source title and URL, also copying its body when present;
both resource types retain the source reference. Notes keep the URL in their
optional URL field.

A snapshot body:

```json
{
  "title": "myrepo — review requests",
  "source_url": "https://github.com/org/myrepo/pulls",
  "stale_after": "1h",
  "items": [
    {
      "key": "https://github.com/org/myrepo/pull/42",
      "title": "Fix auth race",
      "url": "https://github.com/org/myrepo/pull/42",
      "tags": ["review"],
      "meta": {"author": "someone", "checks": "passing"}
    }
  ]
}
```

A change summary:

```json
{"added": 1, "removed": 0, "updated": 2, "unchanged": 5,
 "added_keys": ["..."], "removed_keys": [], "updated_keys": ["...", "..."]}
```

Feature routes live in `crates/callboard/src/routes/`, one file per area,
registered by `build.rs`. The dispatcher matches captures (`{feed}` is a valid
feed name, `{id}` an integer, `{key}` and `{name}` decoded once), applies body
limits, and refuses to start if two routes, or a route and a service endpoint
(`/health`, `/events`, `/service/upgrade`), could match the same path.

### 8.2 Events

`GET /events` is a server-sent event stream of change notices naming the feed,
board, or layout that changed. Notices carry identifiers, not content; clients
refetch.

The response uses `text/event-stream`. After each successful committed write,
`event: change` carries a JSON identifier in `data`:

- `{"resource":"feed","name":"reviews"}` for submissions, error status,
  snooze/order changes, or deletion;
- `{"resource":"board","id":2}` for board and contained-item changes;
- `{"resource":"layout","name":"Day"}` for layout saves.

Moves invalidate both origin and destination boards. Board ID 1 identifies the
system archive (`GET /archive`). Feed notices also invalidate display-time
references to that feed; clients refetch any affected board/archive view.
Time-based snooze expiry is computed at read time, so clients schedule their
own refresh at the returned deadline; the passage of time emits no notice.

Every subscription begins with `event: resync` and `data: {}`. Subscribe first,
then refetch current state while continuing to consume notices. `resync` also
means a subscriber fell behind: discard cached assumptions and refetch all
visible state and resource lists. Events are ephemeral invalidations, not an
ordered audit log; duplicates are harmless. There are no replay IDs, and
`Last-Event-ID` does not restore missed history. Reconnect after disconnects or
service restarts and process the new initial resync.

The shared broadcast buffer holds 128 notices, with at most 16 event streams
per service; additional subscriptions receive HTTP 503 and should retry later.
Slow subscribers never block writes. Idle streams send heartbeat comments at
10-second intervals and end after 25 seconds, within the existing 30-second
connection deadline; the initial SSE retry interval is 1 second. Clients must
reconnect. Shutdown retains its bounded grace period. The event route uses the
same kernel UID authentication as all other requests.

The service's Store clones share the in-process notification bus. Independently
opened stores or external database writers do not publish to that bus; clients
must mutate state through the service.


### 8.3 Limits

A snapshot is at most 1,000 items and 4 MiB. Titles are at most 500 characters,
bodies 16 KiB, tags 32 per item, `meta` 32 keys. Oversized submissions are
rejected whole; nothing is truncated.

## 9. Authoring and cued integration

### 9.1 CLI

```sh
some-fetch | callboard put gh-myrepo --title "myrepo reviews" --stale-after 1h
callboard fail gh-myrepo "gh: rate limited"
callboard feeds
callboard feed rm gh-myrepo
callboard todo add inbox "Reply about the migration plan"
callboard note add inbox "Release freeze starts Thursday"
```

`put` reads a JSON array of items, or a full snapshot object, from standard
input. It prints the change summary and exits 0 on acceptance and nonzero on
error. `--exit-added CODE` exits with `CODE` instead of 0 when the snapshot added
at least one item, so a scheduler can branch on new arrivals without parsing
output. `--exit-changed CODE` does the same for any added, removed, or updated
item.

When both exit-code options match, `--exit-added` takes precedence. CLI metadata
options override corresponding fields in a full snapshot object.

Board CLI commands are `boards`, `board add NAME`, `board get BOARD`,
`board rename BOARD NAME`, `board rm BOARD [--archive-contents]`, and
`board archive BOARD`. `archive` reads the system archive for deleted boards.
A BOARD selector is a user board ID or exact name; numeric strings are IDs.
Use `boards` to discover IDs, including for boards with numeric names.

`todo add BOARD TITLE` accepts `--body` and `--url`. `note add BOARD BODY`
accepts `--title`, `--url`, and `--color`. Both item commands support `patch ID`
(JSON PATCH object from stdin), `archive ID`, `restore ID BOARD`, `move ID BOARD`,
and `rm ID`. `todo done ID [--undone]` changes completion. Item IDs are positive
integers. Commands print successful API responses as JSON; failures print to
stderr with a nonzero exit. Patch input is limited to 64 KiB and validated
before contacting the service; omitted fields and explicit nulls retain their
API semantics. `--no-auto-start` applies to all these commands.

`feed patch NAME KEY` reads a feed-item PATCH object from stdin (at most
16 KiB), covering snooze and manual order fields (§8.1). `feed promote NAME KEY
BOARD [--kind todo|note]` copies an item into the selected board; the default
kind is todo. Feed keys are literal strings, encoded exactly once by the CLI.

`upgrade` moves the running service onto the installed binary in place;
`uninstall [--purge] [--yes]` removes the unit, service, and data; `setup
--status` and `setup --uninstall` inspect and remove the unit (§7.4).

`layouts` lists saved layouts with their cards. `layout save NAME` reads and
validates a layout object (§6.4) from stdin, bounded at 64 KiB, and creates or
replaces that layout. `layout rename NAME NEW_NAME` never replaces another
layout, and `layout rm NAME` reports whether the layout existed. Layout names are literal Unicode strings and use the
same name rules as the API. Validations happen before service access; these
commands retain global `--no-auto-start` and JSON response semantics.

### 9.2 cued workflow

A feed script fetches and pipes into `callboard put`. Scripts should run with
`pipefail` so a fetch failure fails the step, and report it with `callboard
fail`:

```sh
#!/usr/bin/env bash
set -euo pipefail
feed=gh-myrepo-reviews
if ! out=$(gh search prs --repo org/myrepo --review-requested=@me --state open \
      --json url,title,author \
      --jq 'map({key: .url, title, url, meta: {author: .author.login}})'); then
  callboard fail "$feed" "gh search failed"
  exit 1
fi
printf '%s' "$out" | callboard put "$feed" --title "myrepo reviews" \
  --stale-after 1h --exit-added 10
```

cued runs it on a schedule and notifies only when items were added:

```toml
name = "feed-gh-myrepo-reviews"
every = "15m"
entry = "fetch"

[[step]]
id  = "fetch"
run = ["/home/me/bin/feed-gh-myrepo-reviews"]
timeout = "2m"
[[step.transition]]
when = { exit = 10 }
then = { goto = "notify" }
[[step.transition]]
when = { exit = 0 }
then = { end = "success" }
[[step.transition]]
when = "always"
then = { end = "failure" }

[[step]]
id     = "notify"
notify = { title = "myrepo", body = "New review requests on the callboard" }
on.always = { end = "success" }
```

cued's notify actions carry a fixed title and body, so the popup cannot yet name
the new items. See §12.

### 9.3 MCP

`callboard mcp` serves stdio only. It exposes:

- **read** — list feeds and boards; read a feed's items and status; read a
  board's todos and notes.
- **create** — add a todo or note to an existing board.

MCP cannot submit snapshots, report feed errors, or create feeds. Feeds are only
useful if they are timely and consistently shaped: a script produces the same
fields every run on a fixed schedule. A model would shape items differently from
one submission to the next, and changed content reads as an update (§3.3). An AI
harness is also the wrong timer: polling a queue through a model spends tokens on
work a scheduled script does for free. Feeds belong to scripts run by cued or
another scheduler; AI clients read them and capture what comes out of them as
todos and notes.

MCP also cannot edit, complete, archive, or delete todos and notes, create or
delete boards, change snooze or order, or read and save layouts. Those remain CLI
and GUI work.

Following cued, the server reads `$XDG_CONFIG_HOME/callboard/mcp.toml`, re-read on
each capability check, with each capability closed by default:

```toml
read   = "on"      # on | off
create = "on"      # on | off
```

Todos and notes created through MCP record their source for display, and are
otherwise the user's like any other (§5.2).

## 10. Security

### 10.1 Threat model

Protect one user's board from other local users. Same-user processes are trusted
with that user's privileges, including submitting to any feed.

### 10.2 Socket authentication

The service checks kernel peer credentials, not identity asserted in requests.

### 10.3 Filesystem permissions

Socket, data, and config directories are private and ownership is checked.
Application directories require mode 0700; the database, lock, and socket use
0600. Existing unsafe permissions are rejected rather than silently changed.
Paths may not contain symlinks or `..` components. Ancestors must be owned by
the user or root and not writable by others, except root-owned sticky
directories such as `/tmp`. Lock/database files and existing SQLite sidecars
must be owned regular files with a single hard link.

### 10.4 Untrusted content

Submitted text is data. The GUI renders titles, bodies, tags, and `meta` as plain
text — no HTML, Markdown, or image loading — so a feed cannot fetch remote
content or inject markup. Only `http` and `https` URLs are opened, through the
desktop's URL handler; other schemes are shown but not opened.

## 11. Platform

Linux only, matching cued: Unix sockets with `SO_PEERCRED`, systemd user
services, XDG base directories, and a Wayland or X11 desktop for the GUI.

## 12. Future work and open questions

- **Dynamic notify bodies in cued.** A cued notify step that takes its body from
  the previous step's output would let popups name new items. This is a cued
  feature; callboard's change summary is already shaped for it.
- **Free-form note placement.** Notes are ordered sticky notes inside a board
  card; placing them freely is possible later.
- **Canvas zoom and snapping.** Zooming out for an overview, and snapping cards
  to each other's edges, can be added without changing the layout format.
- **Due dates.** Whether todos carry due dates, and whether cued reminders are
  created for them. callboard itself stays silent (§1).
- **Search** across feeds, todos, and notes.
- **macOS port** of callboard and cued together, once both are stable on Linux.
