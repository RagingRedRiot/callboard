# Canvas GUI: implementation plan

**Status: implemented.** All phases landed by 30 September 2026; DESIGN.md §6
describes the canvas as built. This plan is kept as a record of how it was
sequenced.

Replaces the tiled split/tab window (`egui_tiles`) with the canvas of cards
specified in DESIGN.md §6.1–6.4 and §6.6. Decisions already made: cards overlap
with click-to-front, the canvas pans (no zoom, no snapping yet), cards collapse
to their title bar, a layout holds one card per feed or board, and existing
saved layouts are dropped rather than converted (callboard is unreleased).

Everything below the arrangement layer stays: the service, feeds and boards,
live sync and the scheduler, counts, snooze and promote, layout save, rename,
delete and preferences, the ordered write worker, and close-time save flushing.

## Phase 0 — Prototype card rendering (decision gate)

Build a throwaway canvas in a test binary or behind a scratch module before
touching the real code. It must show, in `egui_kittest` and on the desktop:

1. Two overlapping cards drawn back to front inside the central area, clipped
   to it (never over the sidebar or layout bar).
2. Clicks and drags hit only the topmost card where they overlap.
3. Title-bar drag moves a card; edge and corner drags resize it with a minimum
   size; a collapse button folds it to the title bar.
4. A `ScrollArea` inside a card scrolls its contents; the wheel over empty
   canvas pans instead, and Shift + wheel pans horizontally.
5. Dragging empty canvas pans every card together.

Approach to try first: draw each card with a child `Ui` at its screen rect
(`view` offset applied), clip rect = card ∩ canvas, in back-to-front order;
sense title-bar and edge drags with `interact`. egui's `Window` is the fallback
only if it can be clipped to the canvas and its stacking order read back
reliably; the requirement is that the GUI owns geometry and order.

Exit: a short note in the progress log ([progress.md](progress.md)) recording the chosen approach and anything
egui made awkward (hit-testing overlap, wheel routing, focus).

## Phase 1 — Layout format in core, service, and CLI

`callboard-core/src/layout.rs`:

- Replace `Panel`/`Axis` and the tree with `Layout { view: View, cards: Vec<Card> }`,
  `View { x, y }`, `Card { target: Target, x, y, width, height, collapsed }`,
  and `Target::{Feed { name }, Board { id }}`; `NamedLayout { name, view,
  cards, updated_at_ms }` (serde-flattened `Layout`). All `deny_unknown_fields`.
- Validation per §6.4: finite coordinates with |value| ≤ 1,000,000; width and
  height finite in 1–100,000; ≤ 256 cards; duplicate targets rejected; feed
  names valid; board IDs > 1; body ≤ 64 KiB. Drop the depth/node-count limits.

Storage and service:

- Migration `0007_canvas_layouts.sql`: `DELETE FROM layouts` (the preference
  clears through its foreign key) and rename `tree_json` to `layout_json`.
- `Store::save_layout`/`list_layouts`/`rename_layout` store and return the new
  shape. `PUT /layouts/{name}` accepts the layout object directly (no `tree`
  wrapper). Routes, notices, rename/delete, and preferences are unchanged.
- CLI `layout save` validates the new object before connecting.

Tests: rewrite `tests/layouts.rs` validation cases (limits at the boundaries,
duplicates, unknown fields, empty layout), keep the persistence/rename/delete/
preference tests on the new shape, add an upgrade test showing 0007 empties
layouts and clears the preference, and update the service and CLI layout
tests. Update README examples.

## Phase 2 — GUI model (`workspace.rs`)

Replace the tile tree with a canvas model that has no egui dependency beyond
geometry types:

- `Canvas { view: Pos2, cards: Vec<CardState> /* back to front */, focused }`
  where `CardState { target, rect (canvas units), collapsed }`. The archive is
  allowed as a target but omitted from saved layouts (`has_unsaveable`).
- Operations: `reveal(target, viewport) -> bool` (pan to it, bring to front,
  expand), `place(target, at: Option<Pos2>, viewport)` (default size, centred
  in the view and cascaded off any card already there, clamped to the view),
  `move_to`, `resize` (minimum size), `toggle_collapsed`, `bring_to_front`,
  `close`, `retarget` (refused when the target already has a card),
  `pan_by`, `show_all(viewport)`, `targets()`, and
  `to_layout()`/`from_layout()`.
- Constants: default card 420 × 520, minimum 220 × 120, cascade offset 32.
- `Layouts` (working copies, sync, debounced saves, rename/delete, preference)
  keeps its logic; it compares `Layout` values instead of trees, and
  `normalize` becomes rounding coordinates to whole units so float noise from
  drags never reads as a change. `Placement` and the tile helpers go away.

Unit tests: placement and cascade, reveal pans to and expands, one card per
target (place reveals instead of duplicating; retarget refuses), close,
collapse keeps height, bring-to-front order, show-all, layout round trip, the
archive left out of saves, and the existing save/rename/delete/preference
tests ported.

## Phase 3 — GUI rendering and interaction (`app.rs`)

- Central area becomes the canvas: pan on empty-canvas drag and wheel, then
  draw visible cards back to front (cull those outside the view). Cards reuse
  `show_feed`/`show_board` inside a per-card `ScrollArea`.
- Title bar: title with counts and error/stale marker, **Show…** (placed
  targets disabled), collapse, close. Clicking a card brings it to front and
  focuses it.
- Sidebar: click reveals or places; **Add card…** replaces **Add panel…** (no
  split choices); right-click offers the same; drag an entry onto the canvas
  to place it at the drop point.
- Layout bar gains **Show all**. Empty-canvas hint replaces "No panels".
- Actions: `Place(Target, Placement)` becomes `Place(Target, Option<Pos2>)`;
  tile actions become card actions keyed by target (unique per layout), which
  also removes the stale-tile-ID guard.
- Remove `egui_tiles` from `callboard-gui/Cargo.toml` and `Cargo.lock`.

## Phase 4 — Tests

Port or replace the `egui_kittest` interaction tests: sidebar click places then
reveals (with pan), Add card and right-click placement, title-bar drag moves
and auto-saves once, edge drag resizes to no less than the minimum, collapse
and expand, overlapping cards where a click brings the lower one to front and
the saved order changes, empty-canvas drag pans and saves the view, wheel over
a card scrolls it but does not pan, Show… retargets and disables placed
targets, Show all, drag from sidebar to canvas, deleted-target placeholder
cards. Keep the rename/delete/preference, snooze/promote, counts, and
close-flush tests. Run the GUI suite repeatedly for stability, as before.

## Phase 5 — Desktop verification and docs

- Click through on the desktop with the absolute uinput pointer and
  `cosmic-screenshot` against an isolated seeded service: place, move, resize,
  overlap and raise, collapse, pan, scroll a long feed, Show all, drag from the
  sidebar, and restart the GUI to confirm the layout (including view and order)
  is restored.
- Update README (GUI section, layout CLI example) and the progress log; run the full
  local workflow.

## Order and review points

Phases run in order; each ends with the full local workflow passing and a
commit. Review points with the user: after Phase 0 (does the prototype feel
right on the desktop?) and after Phase 3 (first real use), before polishing
tests and docs.

Restarting the user's own service after Phase 1 applies migration 0007, which
deletes their saved layouts (agreed).
