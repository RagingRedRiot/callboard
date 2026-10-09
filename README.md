<div align="center">

# callboard

**A bulletin board for your scripts, todos, and notes, on your Linux desktop.**

Scripts post what they find. callboard tracks what's new, and keeps it on a canvas beside your boards.

[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Rust 1.98+](https://img.shields.io/badge/rust-1.98%2B-orange)](https://www.rust-lang.org/)
[![Platform: Linux](https://img.shields.io/badge/platform-Linux-lightgrey)](#quick-start)

[Quick start](#quick-start) · [How it works](#how-it-works) · [Command line](docs/cli.md) · [Desktop app](docs/gui.md) · [Design](DESIGN.md) · [Development](docs/development.md)

</div>

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/demo-dark.gif">
  <img alt="A script runs callboard put and a new pull request appears live in the review feed; its details show on hover, it is dragged onto the Inbox board as a todo, a todo is ticked, a nightly build turns green, and Ctrl+K jumps back to the Inbox after panning away. The canvas then zooms out until every card fits, bringing a watch of support tickets into view; a script reports that one ticket changed, it is flagged as needing attention, acknowledged, and the zoom is reset to 100%" src="docs/assets/demo-light.gif" width="100%">
</picture>

Anything a script can fetch can be a feed: pull requests waiting on you, last
night's builds, new releases of the tools you pin. Each run submits the whole
list, and callboard works out what changed. Beside the feeds you keep boards of
todos and notes, and the desktop app lays them all out as cards you arrange
and save.

- **Feeds from any script.** Pipe a JSON list to `callboard put` and callboard
  diffs it against the last run: new and updated items are marked, removed ones
  remembered, and an exit code tells your scheduler when something arrived.
- **Watches for what you're waiting on.** Add the URLs you care about; a
  script reports on each, and an item needs your attention only when its
  fingerprint changes, until you or the script acknowledge it. Items that sit
  unchanged too long turn quiet.
- **Boards for the rest.** Todos and notes, with colors, archives, and promotion
  from any feed item in one drag.
- **A canvas you arrange.** Cards overlap, resize, and collapse; zoom out for
  the whole picture and back in to work. Layouts save automatically, zoom
  included, and Ctrl+K finds any feed, watch, board, or layout.
- **Live everywhere.** Every write reaches every open window at once, from the
  CLI, a script, or another window.
- **Private to your account.** A per-user service on a Unix socket checks each
  peer's UID with the kernel. There is no network listener.
- **Upgrades in place.** `callboard upgrade` moves the running service onto a new
  build without a restart or a dropped request.

## Quick start

Download the latest release for x86_64 Linux:

```sh
base=https://github.com/RagingRedRiot/callboard/releases/latest/download
curl -fLO "$base/callboard-x86_64-linux.tar.gz" -O "$base/callboard-x86_64-linux.tar.gz.sha256"
sha256sum -c callboard-x86_64-linux.tar.gz.sha256
tar -xzf callboard-x86_64-linux.tar.gz callboard callboard-gui
install -D -m 755 -t ~/.local/bin callboard callboard-gui
callboard-gui --install-desktop   # add Callboard to your applications list and dock
```

The archive holds both binaries from one build: `callboard` (the service and
CLI) and `callboard-gui` (the desktop app), which starts the service from
beside it. `callboard` is static and runs on any x86_64 Linux; the app needs a
Wayland or X11 desktop with OpenGL and glibc 2.35 or newer.

Or build from source with Rust 1.98 or newer:

```sh
git clone https://github.com/RagingRedRiot/callboard.git
cd callboard
cargo install --path crates/callboard --locked   # installs callboard and callboard-gui to ~/.cargo/bin
callboard-gui --install-desktop
```

`--install-desktop` adds a launcher entry and icons under `~/.local/share`,
which GNOME, KDE, COSMIC, and other freedesktop desktops read. The entry runs the
app by its full path, so it works even when the folder you installed into isn't
on your desktop session's `PATH`. Run it again if you move the binary. If the
launcher or dock doesn't show it straight away, log out and back in.
`callboard-gui --uninstall-desktop` removes it. See [Desktop app](docs/gui.md)
for details.

Post a feed and open the board:

```sh
printf '%s' '[{"key":"pr-42","title":"Fix auth race","url":"https://github.com/org/repo/pull/42"}]' \
  | callboard put reviews --title "Review requests" --new-for 24h
callboard board add Inbox
callboard todo add Inbox "Reply about the migration"
callboard-gui
```

The service starts on demand and keeps running after the window closes. To start
it at login instead, run `callboard setup` (a systemd user unit).

## How it works

A small service owns a SQLite store under `~/.local/share/callboard` and serves
a JSON API on a private Unix socket. The CLI, your scripts, and the desktop app
are all clients of it, and the app follows a stream of change events, so what
you see is always current.

A feed is identified by name and replaced as a whole on each submission. A
tracking script is usually a fetch piped into `put`:

```sh
gh search prs --review-requested=@me --state open --json url,title \
    --jq 'map({key: .url, title, url})' \
  | callboard put reviews --title "Review requests" --stale-after 1h --exit-added 10
```

`--exit-added 10` exits with 10 when new items arrived, so a scheduler such as
[cued](https://github.com/RagingRedRiot/cued) can notify you only then. If the
fetch fails, `callboard fail reviews "message"` marks the feed without touching
its items.

## Documentation

| | |
|---|---|
| [Command line](docs/cli.md) | Feeds, snapshot format, boards and items, layouts, the service, upgrade and uninstall |
| [Desktop app](docs/gui.md) | The canvas, cards, live updates, and saved layouts |
| [Design](DESIGN.md) | Concepts, API, events, security model, and lifecycle |
| [Development](docs/development.md) | Building, tests, local checks, and the [merge gate](docs/merge-checks.md) |

## Upgrade and uninstall

Install the new release over the old binaries (or `git pull && cargo install
--path crates/callboard --locked` from source), then:

```sh
callboard upgrade            # switch the running service to the new build
callboard uninstall          # remove the service, unit, launcher, and all data (--purge for config too)
```

Uninstall leaves the binaries: delete them from `~/.local/bin`, or run
`cargo uninstall callboard` for a source install.

Upgrading from a build where the app was a separate `callboard-gui` package?
Run `cargo uninstall callboard-gui` once before installing; otherwise Cargo
refuses to replace the old binary.

## Status

callboard is alpha software. The API and storage may
change between versions; `callboard upgrade` migrates your data in place. MCP
access for AI assistants is designed (DESIGN.md §9.3) but not implemented yet.

## License

[MIT](LICENSE)
