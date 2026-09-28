# callboard design

callboard is a Linux per-user bulletin board implemented in Rust. A background
service stores feeds, todos, and notes; a desktop GUI arranges them into
draggable panels; scripts submit feeds through the CLI, and a stdio MCP server
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
feeds and boards into panels on screen (§6).

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
- `source_url` — optional link to the queue the feed mirrors.
- `stale_after` — optional duration after which the feed is marked stale (§3.5).

### 3.2 Items

An item has a `key`, unique within its feed and stable across submissions; a
URL is the usual choice. Other fields:

- `title` — required, plain text.
- `url` — optional link opened from the GUI (§10.4).
- `body` — optional plain text.
- `tags` — optional list of short strings.
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

A snoozed item is hidden from its feed panel, which shows a count of snoozed
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

## 5. Boards, todos, and notes

### 5.1 Boards

A board is a named, user-created collection of todos and notes. Boards are
created, renamed, and deleted from the GUI or CLI. Deleting a board requires it
to be empty or an explicit confirmation that archives its contents.

### 5.2 Todos and notes

A todo has a title, optional body and URL, a done flag, an optional reference
(§5.3), and a position in its board. A note has a body, an optional title,
color, and reference, and a position in its board; the GUI renders notes as
sticky-note cards beside the board's todo list.

Todos and notes never expire. Marking a todo done keeps it visible until it is
archived. Archiving hides an item into the board's archive, from which it can be
restored; deleting is permanent. Todos and notes can be moved between boards.

Tools can create todos and notes through the API (§8), for example an MCP client
recording a follow-up. Once created, they are the user's: no later submission
replaces them.

### 5.3 Promotion and references

Promoting a feed item creates a todo or note in a chosen board. The new object
copies the item's title and URL, and stores a reference `{feed, key}`.

References resolve at display time:

- **live** — the feed holds an item with that key. The GUI shows its current
  title and can reveal it in its feed panel.
- **source gone** — the item or its feed no longer exists. The GUI shows the
  copied title and URL with a "source gone" marker.

A reference never keeps a feed item alive, and removing an item never alters the
todo or note that references it. "Source gone" is itself useful: the tracked
thing has resolved.

## 6. GUI

### 6.1 Panels

The window is a tree of panels: splits and tab groups whose leaves each show one
feed or one board. The user drags panels to rearrange, split, or stack them, and
can point an existing panel at a different feed or board. A sidebar lists every
feed and board with item counts and error/stale markers; dragging an entry into
the window places it.

Feeds that are not placed still accept submissions and stay current.

### 6.2 Feed panels

A feed panel shows the feed's title, source link, last-submitted time, and any
error or stale marker, then its visible items in display order (§4.2). Item
actions: open link, snooze, drag to reorder, promote to todo or note, reveal
snoozed.

### 6.3 Board panels

A board panel shows the board's todos and notes. Todos are a checklist; notes
are cards. Both reorder by dragging and accept drags from feed panels as
promotion (§5.3).

### 6.4 Layouts

A layout is the panel tree plus each leaf's target. The GUI saves changes to the
active layout through the API as they happen. The user can keep several named
layouts — one per project or kind of day — and switch between them. Switching
layouts never changes feeds or boards.

A layout leaf whose feed or board has been deleted shows an empty placeholder
until the user retargets or closes it.

### 6.5 Live updates

The GUI subscribes to the service's event stream (§8.2) and refetches whatever
changed. Submissions appear without refresh; GUI edits made in one window apply
to others.

### 6.6 Toolkit

egui via eframe, with `egui_tiles` for the draggable panel tree. It provides
splits, tabs, and drag rearrangement with little custom code, which is the GUI's
central requirement.

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
Without a running service, the CLI starts it on demand (the GUI will use the
same client support). `--no-auto-start` requires an existing service. A
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

## 8. API

### 8.1 Resources

| Method and path | Purpose |
|---|---|
| `GET /health` | Identify the service and API version |
| `PUT /feeds/{name}` | Submit a snapshot (§3.3); returns the change summary |
| `POST /feeds/{name}/error` | Report a failed fetch (§3.4) |
| `GET /feeds`, `GET /feeds/{name}` | List feeds; read one with items and view state |
| `DELETE /feeds/{name}` | Delete a feed (§3.6) |
| `PATCH /feeds/{name}/items/{key}` | Set snooze or position |
| `GET /boards`, `POST /boards` | List or create boards |
| `PATCH /boards/{id}`, `DELETE /boards/{id}` | Rename or delete a board |
| `POST /boards/{id}/todos`, `POST /boards/{id}/notes` | Create a todo or note |
| `PATCH /todos/{id}`, `PATCH /notes/{id}` | Edit, move, complete, archive, restore |
| `DELETE /todos/{id}`, `DELETE /notes/{id}` | Delete permanently |
| `POST /feeds/{name}/items/{key}/promote` | Promote to a todo or note (§5.3) |
| `GET /layouts`, `PUT /layouts/{name}` | Read or save layouts |
| `GET /events` | Change stream (§8.2) |

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

### 8.2 Events

`GET /events` is a server-sent event stream of change notices naming the feed,
board, or layout that changed. Notices carry identifiers, not content; clients
refetch.

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
- **New and updated markers.** Whether items show "new" or "updated" badges until
  seen, and what counts as seen.
- **Free-form note placement.** Notes are ordered cards in a board; placing them
  freely on a canvas is possible later.
- **Due dates.** Whether todos carry due dates, and whether cued reminders are
  created for them. callboard itself stays silent (§1).
- **Search** across feeds, todos, and notes.
- **macOS port** of callboard and cued together, once both are stable on Linux.
