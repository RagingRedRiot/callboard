# Command line

Every command prints JSON on stdout and exits nonzero with the error on stderr.
Commands start the service on demand (see [Running the service](#running-the-service)).

## Feeds

`put` accepts arrays or full snapshot objects; `--title`, `--description`,
`--source-url`, `--stale-after`, and `--new-for` override object metadata. Empty input
fails; `[]` clears the feed.

A full snapshot object for a tracking script, with a description saying what
the feed is and per-item details for the GUI's hover card:

```json
{"title": "myrepo — review requests",
 "description": "Open PRs in org/myrepo where I'm a requested reviewer",
 "source_url": "https://github.com/org/myrepo/pulls",
 "stale_after": "1h",
 "new_for": "24h",
 "items": [{"key": "https://github.com/org/myrepo/pull/42",
            "title": "Fix auth race",
            "url": "https://github.com/org/myrepo/pull/42",
            "body": "Fixes the token refresh race in the session store.",
            "tags": ["review-requested"],
            "meta": {"author": "sam", "checks": "failing", "opened": "2026-09-27"},
            "color": "red"}]}
```

`new_for` sets how long items show as **new** after they first appear, or
**updated** after their content last changed; without it nothing is marked.
The marks follow time alone and end by themselves. `color` (red, orange,
yellow, green, blue, purple, pink, or gray) tints the item's row, so a script
can color by state (failing checks red). The feed also keeps its last change
with the titles of removed items (DESIGN.md §4.3). Every field, `color`
included, counts as content, so send stable values (an "opened" date, not an
age) or items show as updated on every run. A feed's first submission is its
baseline: its items are not marked new.
`--exit-added CODE` takes precedence over `--exit-changed CODE` when both match.
Responses are JSON on stdout; errors go to stderr with a nonzero exit status.

## Boards, todos, and notes

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

## Feed view state and layouts

Feed view controls and layouts are also available from the CLI:

```sh
printf '%s' '{"wake_on_update":true}' | callboard feed patch reviews 'item-key'
printf '%s' '{"position":0}' | callboard feed patch reviews 'item-key'
printf '%s' '{"reset_order":true}' | callboard feed patch reviews 'item-key'
callboard feed promote reviews 'item-key' inbox
callboard feed promote reviews 'item-key' inbox --kind note
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

## Running the service

Commands start the service automatically when needed. Use `--no-auto-start` to
require an existing service, or `callboard serve` to run it in the foreground.
SIGINT/SIGTERM drains requests and closes SQLite before releasing the data lock.
To test in isolation, set absolute `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, and
`CALLBOARD_SOCKET_DIR` paths under a private directory.

From a stable installed binary path, `callboard setup --print` previews the
systemd unit. `callboard setup` installs it and enables it for future logins;
it does not take over an already running on-demand service. If no service is
running, start the unit with `systemctl --user start callboard.service`.
The unit records the executable and resolved data/config/socket paths. Keep
the executable at that location, and rerun setup after moving it. Existing
custom units are preserved. Custom XDG config locations must also be visible
to your systemd user manager. `callboard setup --status` reports the unit and
its systemd state; `callboard setup --uninstall` disables and deletes it.

## Upgrade

Install the new build over the old one, then move the running service onto it:

```sh
cargo install --path crates/callboard --locked
callboard upgrade
```

The service finishes in-flight requests and re-executes the installed binary in
place, keeping its PID, socket, systemd supervision, and store; migrations run
as it starts. Requests made meanwhile wait and are then served. A binary that
fails `--version` is refused and nothing changes. Reopen any open
`callboard-gui` window to load the new GUI; the status bar shows "Version
mismatch" while a window and the service are different builds.

## Uninstall

```sh
callboard uninstall          # add --purge to remove ~/.config/callboard too
cargo uninstall callboard    # or delete the binaries, for a release install
```

`uninstall` lists what it will remove and asks first: the systemd unit, the
running service, and the data directory with every feed, board, todo, note,
layout, and the archive, plus the socket and the launcher entry from
`callboard-gui --install-desktop`. Config is kept unless you pass
`--purge`. Close `callboard-gui` first; an open window would start a fresh,
empty service. Off a terminal, `--yes` is required.
