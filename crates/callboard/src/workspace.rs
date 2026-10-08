//! Card arrangement on the canvas and named layouts (DESIGN.md §6.1, §6.4).
//!
//! A [`Canvas`] holds cards in canvas units, back to front, and the view
//! offset; it converts to and from the service's toolkit-independent
//! [`Layout`]. Each named layout has a working copy; arrangement changes
//! auto-save to it (§6.4). The Unsaved arrangement has no name until
//! "Save as…".
use crate::backend::Target;
use callboard_core::layout::{self, Layout, NamedLayout, View};
use eframe::egui::{self, Pos2, Rect, Vec2};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

/// Size of a newly placed card, shrunk to fit the view when needed.
pub const DEFAULT_CARD: Vec2 = Vec2::new(420.0, 520.0);
/// Smallest card; smaller saved sizes are enlarged when shown.
pub const MIN_CARD: Vec2 = Vec2::new(220.0, 120.0);
/// Height of a card's title bar, which is all a collapsed card shows.
pub const TITLE_HEIGHT: f32 = 34.0;
/// Offset between cards placed at nearly the same spot: more than a title
/// bar, so the card underneath keeps its title and buttons in view.
pub const CASCADE: f32 = TITLE_HEIGHT + 6.0;
/// Space kept between a revealed card and the edge of the view.
pub const MARGIN: f32 = 24.0;

#[derive(Debug, Clone, PartialEq)]
pub struct CardState {
    pub target: Target,
    /// Canvas units; the height is the expanded height, kept while collapsed.
    pub rect: Rect,
    pub collapsed: bool,
}

impl CardState {
    /// The rect the card occupies: its title bar alone when collapsed.
    pub fn shown(&self) -> Rect {
        if self.collapsed {
            Rect::from_min_size(self.rect.min, Vec2::new(self.rect.width(), TITLE_HEIGHT))
        } else {
            self.rect
        }
    }
}

/// One arrangement of cards. The last card is in front and has focus.
#[derive(Debug, Clone, PartialEq)]
pub struct Canvas {
    /// Fresh for each rebuilt canvas, so per-card UI state (scroll, toggles)
    /// never carries over from a previous version of the arrangement.
    pub id: egui::Id,
    /// The canvas point shown at the top-left of the canvas area.
    pub view: Pos2,
    /// Back to front.
    pub cards: Vec<CardState>,
}

impl Canvas {
    pub fn new(id: egui::Id) -> Self {
        Self {
            id,
            view: Pos2::ZERO,
            cards: Vec::new(),
        }
    }

    pub fn from_layout(id: egui::Id, layout: &Layout) -> Self {
        let cards = layout
            .cards
            .iter()
            .map(|card| CardState {
                target: match &card.target {
                    layout::Target::Feed { name } => Target::Feed(name.clone()),
                    layout::Target::Board { id } => Target::Board(*id),
                    layout::Target::Watch { name } => Target::Watch(name.clone()),
                },
                rect: Rect::from_min_size(
                    Pos2::new(card.x as f32, card.y as f32),
                    Vec2::new(card.width as f32, card.height as f32).max(MIN_CARD),
                ),
                collapsed: card.collapsed,
            })
            .collect();
        Self {
            id,
            view: Pos2::new(layout.view.x as f32, layout.view.y as f32),
            cards,
        }
    }

    /// The service form. Archive cards are omitted; coordinates are whole
    /// canvas units, so sub-pixel drag noise is never a change.
    pub fn to_layout(&self) -> Layout {
        let unit = |v: f32| f64::from(v.round());
        Layout {
            view: View {
                x: unit(self.view.x),
                y: unit(self.view.y),
            },
            cards: self
                .cards
                .iter()
                .filter_map(|card| {
                    let target = match &card.target {
                        Target::Feed(name) => layout::Target::Feed { name: name.clone() },
                        Target::Board(id) => layout::Target::Board { id: *id },
                        Target::Watch(name) => layout::Target::Watch { name: name.clone() },
                        Target::Archive => return None,
                    };
                    Some(layout::Card {
                        target,
                        x: unit(card.rect.min.x),
                        y: unit(card.rect.min.y),
                        width: unit(card.rect.width()).max(1.0),
                        height: unit(card.rect.height()).max(1.0),
                        collapsed: card.collapsed,
                    })
                })
                .collect(),
        }
    }

    pub fn targets(&self) -> BTreeSet<Target> {
        self.cards.iter().map(|c| c.target.clone()).collect()
    }

    /// Whether any card cannot be saved in a layout (the archive).
    pub fn has_unsaveable(&self) -> bool {
        self.cards.iter().any(|c| c.target == Target::Archive)
    }

    /// The front card, which has focus.
    pub fn focused_target(&self) -> Option<&Target> {
        self.cards.last().map(|c| &c.target)
    }

    pub fn find(&self, target: &Target) -> Option<usize> {
        self.cards.iter().position(|c| &c.target == target)
    }

    pub fn card(&self, target: &Target) -> Option<&CardState> {
        self.cards.iter().find(|c| &c.target == target)
    }

    fn card_mut(&mut self, target: &Target) -> Option<&mut CardState> {
        self.cards.iter_mut().find(|c| &c.target == target)
    }

    /// Move a card to the front. Returns false if it is not on the canvas.
    pub fn raise(&mut self, target: &Target) -> bool {
        let Some(index) = self.find(target) else {
            return false;
        };
        let card = self.cards.remove(index);
        self.cards.push(card);
        true
    }

    /// Bring a placed card to the front, expand it, and pan it into view.
    pub fn reveal(&mut self, target: &Target, viewport: Vec2) -> bool {
        if !self.raise(target) {
            return false;
        }
        let card = self.cards.last_mut().expect("raised");
        card.collapsed = false;
        let rect = card.rect;
        self.pan_into_view(rect, viewport);
        true
    }

    /// Pan by the least amount that shows `rect` with a margin; a rect too
    /// big for the view is aligned at its top-left.
    fn pan_into_view(&mut self, rect: Rect, viewport: Vec2) {
        let axis = |view: f32, min: f32, max: f32, extent: f32| {
            if max - min + 2.0 * MARGIN > extent || min - MARGIN < view {
                min - MARGIN
            } else if max + MARGIN > view + extent {
                max + MARGIN - extent
            } else {
                view
            }
        };
        self.view = Pos2::new(
            axis(self.view.x, rect.min.x, rect.max.x, viewport.x),
            axis(self.view.y, rect.min.y, rect.max.y, viewport.y),
        );
    }

    /// Reveal the target's card, or place a new one: at `at` (canvas units,
    /// its top-left) or centred in the view, cascaded off any card whose
    /// title bar it would cover. Never adds a second card for a target.
    pub fn place(&mut self, target: Target, at: Option<Pos2>, viewport: Vec2) {
        if self.reveal(&target, viewport) {
            return;
        }
        let size = DEFAULT_CARD
            .min(viewport - Vec2::splat(2.0 * MARGIN))
            .max(MIN_CARD);
        let mut min = at
            .unwrap_or_else(|| self.view + (viewport - size) / 2.0)
            .round();
        while self.cards.iter().any(|c| {
            let offset = c.rect.min - min;
            offset.x.abs() < CASCADE && offset.y.abs() < CASCADE
        }) {
            min += Vec2::splat(CASCADE);
        }
        self.cards.push(CardState {
            target,
            rect: Rect::from_min_size(min, size),
            collapsed: false,
        });
    }

    pub fn move_by(&mut self, target: &Target, delta: Vec2) {
        if let Some(card) = self.card_mut(target) {
            card.rect = card.rect.translate(delta);
        }
    }

    /// Resize from the top-left corner, never below [`MIN_CARD`].
    pub fn resize(&mut self, target: &Target, size: Vec2) {
        if let Some(card) = self.card_mut(target) {
            card.rect = Rect::from_min_size(card.rect.min, size.max(MIN_CARD));
        }
    }

    pub fn toggle_collapsed(&mut self, target: &Target) {
        if let Some(card) = self.card_mut(target) {
            card.collapsed = !card.collapsed;
        }
    }

    pub fn close(&mut self, target: &Target) {
        self.cards.retain(|c| &c.target != target);
    }

    /// Point a card at another target, keeping its geometry. Refused when
    /// the new target already has a card (one card per feed, board, or watch).
    pub fn retarget(&mut self, from: &Target, to: Target) -> bool {
        if self.find(&to).is_some() {
            return false;
        }
        match self.card_mut(from) {
            Some(card) => {
                card.target = to;
                true
            }
            None => false,
        }
    }

    pub fn pan_by(&mut self, delta: Vec2) {
        self.view += delta;
    }

    /// Pan so the top-left of the cards' bounding box is in view.
    pub fn show_all(&mut self) {
        if let Some(bounds) = self
            .cards
            .iter()
            .map(CardState::shown)
            .reduce(|a, b| a.union(b))
        {
            self.view = bounds.min - Vec2::splat(MARGIN);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LayoutKey {
    Saved(String),
    /// A session-only arrangement, used before any layout is saved.
    Unsaved,
}

impl LayoutKey {
    pub fn label(&self) -> &str {
        match self {
            Self::Unsaved => "Unsaved",
            Self::Saved(name) => name,
        }
    }
}

/// Arrangement changes save to the active layout after this quiet period
/// (DESIGN.md §6.4), so a drag or resize saves once when it ends.
pub const SAVE_DELAY: Duration = Duration::from_secs(1);
/// A failed save is retried after this long.
pub const SAVE_RETRY: Duration = Duration::from_secs(5);

#[derive(Default)]
struct SaveState {
    /// The last arrangement seen differing from `base`, and since when.
    last: Option<Layout>,
    changed_at: Option<Instant>,
    in_flight: bool,
    /// A rename or delete is in flight; saving now could recreate the old name.
    locked: bool,
    error: Option<String>,
    retry_at: Option<Instant>,
}

struct Working {
    canvas: Canvas,
    /// The saved layout (normalized) this copy matches when nothing is
    /// pending; `None` for `Unsaved`, which has no name to save under.
    base: Option<Layout>,
    /// The service has a newer saved version that was not applied.
    outdated: bool,
    save: SaveState,
}

impl Working {
    /// Local changes not yet saved.
    fn pending(&self) -> bool {
        self.base
            .as_ref()
            .is_some_and(|base| &self.canvas.to_layout() != base)
    }

    /// Following a newer save would lose something: unsaved changes, a save
    /// in flight, or an archive card (never part of a saved layout).
    fn keep_local(&self) -> bool {
        self.pending() || self.save.in_flight || self.canvas.has_unsaveable()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutEntry {
    pub key: LayoutKey,
    pub active: bool,
    /// Changed locally and waiting to be saved (or being saved).
    pub pending: bool,
    pub saving: bool,
    /// The last save failed; it is retried.
    pub save_error: Option<String>,
    /// Contains an archive card, which layouts cannot store.
    pub unsaveable: bool,
    /// Saved in another window while this one kept its own arrangement.
    pub outdated: bool,
    /// No longer present in the service's layout list.
    pub gone: bool,
}

/// Saved layouts from the service plus session-local working copies.
/// Switching layouts never changes feeds or boards.
pub struct Layouts {
    saved: BTreeMap<String, Layout>,
    working: BTreeMap<LayoutKey, Working>,
    active: LayoutKey,
    loaded: bool,
    /// The layout to open on first load, from the startup preference.
    preferred: Option<String>,
    /// Bumped per rebuilt canvas, giving each a fresh [`Canvas::id`].
    generation: u64,
}

impl Default for Layouts {
    fn default() -> Self {
        Self::new()
    }
}

/// A saved layout as the GUI shows it: whole units and cards no smaller than
/// the minimum. Comparing against this form keeps layouts saved elsewhere
/// (the CLI, another window) from looking changed on load.
pub fn normalize(layout: &Layout) -> Layout {
    Canvas::from_layout(egui::Id::NULL, layout).to_layout()
}

impl Layouts {
    pub fn new() -> Self {
        let mut layouts = Self {
            saved: BTreeMap::new(),
            working: BTreeMap::new(),
            active: LayoutKey::Unsaved,
            loaded: false,
            preferred: None,
            generation: 0,
        };
        let unsaved = layouts.build(&LayoutKey::Unsaved, &Layout::default());
        layouts.working.insert(LayoutKey::Unsaved, unsaved);
        layouts
    }

    fn build(&mut self, key: &LayoutKey, layout: &Layout) -> Working {
        self.generation += 1;
        let canvas = Canvas::from_layout(
            egui::Id::new(("callboard-layout", key, self.generation)),
            layout,
        );
        let base = matches!(key, LayoutKey::Saved(_)).then(|| canvas.to_layout());
        Working {
            canvas,
            base,
            outdated: false,
            save: SaveState::default(),
        }
    }

    pub fn active_key(&self) -> &LayoutKey {
        &self.active
    }

    pub fn active(&self) -> &Canvas {
        &self.working[&self.active].canvas
    }

    pub fn active_mut(&mut self) -> &mut Canvas {
        &mut self
            .working
            .get_mut(&self.active)
            .expect("active layout has a working copy")
            .canvas
    }

    /// Whether the service's layout list has been applied.
    pub fn loaded(&self) -> bool {
        self.loaded
    }

    /// Open `name` on first load instead of the first layout by name.
    pub fn prefer(&mut self, name: Option<String>) {
        self.preferred = name;
    }

    /// Hold auto-saves of `name` while a rename or delete is in flight.
    /// Refused while a save of it is in flight: the service would apply the
    /// two in either order.
    pub fn begin_op(&mut self, name: &str) -> bool {
        match self.working.get_mut(&LayoutKey::Saved(name.to_owned())) {
            Some(working) if working.save.in_flight => false,
            Some(working) => {
                working.save.locked = true;
                true
            }
            None => true,
        }
    }

    /// The rename or delete of `name` failed; resume auto-saving.
    pub fn end_op(&mut self, name: &str) {
        if let Some(working) = self.working.get_mut(&LayoutKey::Saved(name.to_owned())) {
            working.save.locked = false;
        }
    }

    /// The service renamed `from`. The working copy keeps its arrangement,
    /// and any unsaved changes then save under the new name.
    pub fn renamed(&mut self, from: &str, layout: &NamedLayout) {
        let old = LayoutKey::Saved(from.to_owned());
        let new = LayoutKey::Saved(layout.name.clone());
        self.saved.remove(from);
        self.saved
            .insert(layout.name.clone(), layout.layout.clone());
        if let Some(mut working) = self.working.remove(&old) {
            working.save.locked = false;
            self.working.insert(new.clone(), working);
        }
        if self.active == old {
            self.active = new;
        }
    }

    /// The service deleted `name`. If it was active, open the first saved
    /// layout, or the Unsaved arrangement when none remain.
    pub fn deleted(&mut self, name: &str) {
        let key = LayoutKey::Saved(name.to_owned());
        self.saved.remove(name);
        self.working.remove(&key);
        if self.active == key {
            let next = self
                .saved
                .keys()
                .next()
                .cloned()
                .map_or(LayoutKey::Unsaved, LayoutKey::Saved);
            self.switch(next);
        }
    }

    /// Whether `name` is taken, saved or not yet listed.
    pub fn exists(&self, name: &str) -> bool {
        self.saved.contains_key(name)
            || self
                .working
                .contains_key(&LayoutKey::Saved(name.to_owned()))
    }

    /// Apply the service's layout list. Working copies follow the saved
    /// version unless that would lose local state (see [`Working::keep_local`]).
    pub fn sync_saved(&mut self, layouts: &[NamedLayout]) {
        self.saved = layouts
            .iter()
            .map(|l| (l.name.clone(), l.layout.clone()))
            .collect();
        let keys: Vec<LayoutKey> = self.working.keys().cloned().collect();
        for key in keys {
            let LayoutKey::Saved(name) = &key else {
                continue;
            };
            let Some(saved) = self.saved.get(name).map(normalize) else {
                continue;
            };
            let working = &self.working[&key];
            let Some(base) = &working.base else { continue };
            if &saved == base {
                self.working.get_mut(&key).expect("present").outdated = false;
            } else if !working.keep_local() {
                let rebuilt = self.build(&key, &saved);
                self.working.insert(key, rebuilt);
            } else if !working.save.in_flight {
                // In flight: our own save is about to become the saved version.
                self.working.get_mut(&key).expect("present").outdated = true;
            }
        }
        // Start in the preferred (else first) saved layout unless the user
        // already placed cards.
        if !self.loaded {
            self.loaded = true;
            let untouched = self.active == LayoutKey::Unsaved && self.active().cards.is_empty();
            let start = self
                .preferred
                .take()
                .filter(|name| self.saved.contains_key(name))
                .or_else(|| self.saved.keys().next().cloned());
            if untouched && let Some(name) = start {
                self.switch(LayoutKey::Saved(name));
            }
        }
    }

    /// Saved layouts with changes that have settled for [`SAVE_DELAY`]. Each
    /// returned save is in flight until [`Self::save_finished`].
    pub fn due_saves(&mut self, now: Instant) -> Vec<(String, Layout)> {
        self.collect_saves(now, false)
    }

    /// Flush pending arrangements on close, bypassing debounce and retry delays.
    pub fn flush_saves(&mut self, now: Instant) -> Vec<(String, Layout)> {
        self.collect_saves(now, true)
    }

    fn collect_saves(&mut self, now: Instant, flush: bool) -> Vec<(String, Layout)> {
        let mut due = Vec::new();
        for (key, working) in &mut self.working {
            let LayoutKey::Saved(name) = key else {
                continue;
            };
            if working.save.in_flight || working.save.locked {
                continue;
            }
            let layout = working.canvas.to_layout();
            let Some(base) = &working.base else { continue };
            if &layout == base {
                working.save.last = None;
                working.save.changed_at = None;
                continue;
            }
            if working.save.last.as_ref() != Some(&layout) {
                working.save.last = Some(layout.clone());
                working.save.changed_at = Some(now);
            }
            let settled = working
                .save
                .changed_at
                .is_some_and(|t| now >= t + SAVE_DELAY);
            let retry_ok = working.save.retry_at.is_none_or(|t| now >= t);
            if flush || (settled && retry_ok) {
                working.save.in_flight = true;
                due.push((name.clone(), layout));
            }
        }
        due
    }

    /// When [`Self::due_saves`] could next return something.
    pub fn next_save_deadline(&self, now: Instant) -> Option<Duration> {
        self.working
            .values()
            .filter(|w| !w.save.in_flight && !w.save.locked)
            .filter_map(|w| {
                let changed = w.save.changed_at? + SAVE_DELAY;
                Some(w.save.retry_at.map_or(changed, |r| r.max(changed)))
            })
            .min()
            .map(|t| t.saturating_duration_since(now))
    }

    /// Record the outcome of an auto-save. On success the layout as stored by
    /// the service becomes the base; a failure is retried after [`SAVE_RETRY`].
    pub fn save_finished(&mut self, name: &str, result: Result<Layout, String>, now: Instant) {
        let key = LayoutKey::Saved(name.to_owned());
        if let Ok(layout) = &result {
            self.saved.insert(name.to_owned(), layout.clone());
        }
        let Some(working) = self.working.get_mut(&key) else {
            return;
        };
        working.save.in_flight = false;
        match result {
            Ok(layout) => {
                working.base = Some(normalize(&layout));
                working.outdated = false;
                working.save.error = None;
                working.save.retry_at = None;
            }
            Err(error) => {
                working.save.error = Some(error);
                working.save.retry_at = Some(now + SAVE_RETRY);
            }
        }
    }

    /// A layout created by "Save as…" or "New layout": the service stored
    /// `layout` under `name`. Saving the Unsaved arrangement moves it (cards,
    /// order, and archive cards intact); others start from the saved layout.
    pub fn adopt(&mut self, from: Option<&LayoutKey>, name: &str, layout: &Layout) {
        let key = LayoutKey::Saved(name.to_owned());
        self.saved.insert(name.to_owned(), layout.clone());
        let working = match from {
            Some(LayoutKey::Unsaved) => {
                let fresh = self.build(&LayoutKey::Unsaved, &Layout::default());
                let mut moved = self
                    .working
                    .insert(LayoutKey::Unsaved, fresh)
                    .expect("Unsaved always has a working copy");
                moved.base = Some(normalize(layout));
                moved
            }
            _ => self.build(&key, layout),
        };
        self.working.insert(key.clone(), working);
        self.active = key;
    }

    pub fn switch(&mut self, key: LayoutKey) {
        if !self.working.contains_key(&key) {
            let layout = match &key {
                LayoutKey::Saved(name) => self.saved.get(name).cloned(),
                LayoutKey::Unsaved => None,
            };
            let Some(layout) = layout else { return };
            let working = self.build(&key, &layout);
            self.working.insert(key.clone(), working);
        }
        self.active = key;
    }

    /// Replace the active working copy with the saved version.
    pub fn revert(&mut self) {
        let key = self.active.clone();
        let layout = match &key {
            LayoutKey::Saved(name) => self.saved.get(name).cloned(),
            LayoutKey::Unsaved => Some(Layout::default()),
        };
        if let Some(layout) = layout {
            let working = self.build(&key, &layout);
            self.working.insert(key, working);
        }
    }

    pub fn entries(&self) -> Vec<LayoutEntry> {
        let mut keys: BTreeSet<LayoutKey> =
            self.saved.keys().cloned().map(LayoutKey::Saved).collect();
        keys.extend(self.working.keys().cloned());
        keys.into_iter()
            .map(|key| {
                let working = self.working.get(&key);
                let gone = matches!(&key, LayoutKey::Saved(n) if !self.saved.contains_key(n));
                LayoutEntry {
                    active: key == self.active,
                    pending: working.is_some_and(Working::pending),
                    saving: working.is_some_and(|w| w.save.in_flight),
                    save_error: working.and_then(|w| w.save.error.clone()),
                    unsaveable: working.is_some_and(|w| w.canvas.has_unsaveable()),
                    outdated: working.is_some_and(|w| w.outdated),
                    gone,
                    key,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const VIEWPORT: Vec2 = Vec2::new(1000.0, 700.0);

    fn feed(name: &str) -> Target {
        Target::Feed(name.into())
    }

    fn layout(value: serde_json::Value) -> Layout {
        serde_json::from_value(value).unwrap()
    }

    /// A layout with one card per target, 500 apart along a row at y 100.
    fn cards(targets: &[serde_json::Value]) -> Layout {
        let cards: Vec<_> = targets
            .iter()
            .enumerate()
            .map(|(i, target)| {
                json!({"target":target,"x":i as f64 * 500.0,"y":100,"width":420,"height":520,"collapsed":false})
            })
            .collect();
        layout(json!({"view":{"x":0,"y":0},"cards":cards}))
    }

    fn named(name: &str, layout: Layout) -> NamedLayout {
        NamedLayout {
            name: name.into(),
            layout,
            updated_at_ms: 0,
        }
    }

    fn one(name: &str) -> Layout {
        cards(&[json!({"kind":"feed","name":name})])
    }

    fn entry(layouts: &Layouts, name: &str) -> LayoutEntry {
        layouts
            .entries()
            .into_iter()
            .find(|e| e.key == LayoutKey::Saved(name.into()))
            .unwrap()
    }

    #[test]
    fn layouts_round_trip_with_order_geometry_and_whole_units() {
        let saved = layout(json!({"view":{"x":-40,"y":12},"cards":[
            {"target":{"kind":"feed","name":"a"},"x":0,"y":0,"width":420,"height":560,"collapsed":false},
            {"target":{"kind":"board","id":9},"x":440,"y":-8,"width":360,"height":300,"collapsed":true}
        ]}));
        let canvas = Canvas::from_layout(egui::Id::new("c"), &saved);
        assert_eq!(canvas.cards[1].target, Target::Board(9));
        assert_eq!(canvas.focused_target(), Some(&Target::Board(9)));
        assert_eq!(canvas.cards[1].shown().height(), TITLE_HEIGHT);
        assert_eq!(canvas.to_layout(), saved);
        // Fractions from drags round away; small saved cards grow to the minimum.
        let mut moved = canvas.clone();
        moved.move_by(&feed("a"), Vec2::new(0.3, -0.4));
        assert_eq!(moved.to_layout(), saved);
        let tiny = layout(json!({"view":{"x":0,"y":0},"cards":[
            {"target":{"kind":"board","id":2},"x":0,"y":0,"width":1,"height":1,"collapsed":false}]}));
        let grown = normalize(&tiny);
        assert_eq!(
            (grown.cards[0].width, grown.cards[0].height),
            (f64::from(MIN_CARD.x), f64::from(MIN_CARD.y))
        );
        assert_eq!(normalize(&grown), grown, "normal form is stable");
    }

    #[test]
    fn the_archive_can_be_placed_but_is_never_saved() {
        let mut canvas = Canvas::new(egui::Id::new("c"));
        canvas.place(Target::Archive, None, VIEWPORT);
        canvas.place(feed("a"), None, VIEWPORT);
        assert!(canvas.has_unsaveable());
        let saved = canvas.to_layout();
        assert_eq!(saved.cards.len(), 1);
        assert!(saved.validate().is_ok());
    }

    #[test]
    fn a_card_placed_near_another_never_covers_its_title_bar() {
        let mut canvas = Canvas::new(egui::Id::new("c"));
        canvas.place(feed("a"), Some(Pos2::new(100.0, 100.0)), VIEWPORT);
        // Off by a few points: still on a's title bar, so it cascades.
        canvas.place(feed("b"), Some(Pos2::new(110.0, 113.0)), VIEWPORT);
        let (a, b) = (canvas.cards[0].rect, canvas.cards[1].rect);
        assert!(b.top() - a.top() > TITLE_HEIGHT, "{a:?} {b:?}");
        // Clear of every title bar, it stays where it was put.
        canvas.place(feed("c"), Some(Pos2::new(100.0, 400.0)), VIEWPORT);
        assert_eq!(canvas.cards[2].rect.min, Pos2::new(100.0, 400.0));
    }

    #[test]
    fn placing_centres_in_the_view_cascades_and_never_duplicates() {
        let mut canvas = Canvas::new(egui::Id::new("c"));
        canvas.view = Pos2::new(100.0, 50.0);
        canvas.place(feed("a"), None, VIEWPORT);
        let first = canvas.cards[0].rect;
        assert_eq!(first.size(), DEFAULT_CARD);
        assert_eq!(first.center(), (canvas.view + VIEWPORT / 2.0).round());
        canvas.place(feed("b"), None, VIEWPORT);
        assert_eq!(canvas.cards[1].rect.min, first.min + Vec2::splat(CASCADE));
        canvas.place(feed("c"), None, VIEWPORT);
        assert_eq!(
            canvas.cards[2].rect.min,
            first.min + Vec2::splat(2.0 * CASCADE)
        );
        // Placing an existing target reveals it instead.
        canvas.place(feed("a"), None, VIEWPORT);
        assert_eq!(canvas.cards.len(), 3);
        assert_eq!(canvas.focused_target(), Some(&feed("a")));
        // A drop point places the card's top-left there.
        canvas.place(Target::Board(2), Some(Pos2::new(-300.0, 900.0)), VIEWPORT);
        assert_eq!(
            canvas.card(&Target::Board(2)).unwrap().rect.min,
            Pos2::new(-300.0, 900.0)
        );
        // A small view shrinks new cards, but never below the minimum.
        let mut small = Canvas::new(egui::Id::new("s"));
        small.place(feed("a"), None, Vec2::new(300.0, 100.0));
        assert_eq!(small.cards[0].rect.size(), Vec2::new(252.0, MIN_CARD.y));
    }

    #[test]
    fn revealing_raises_expands_and_pans_by_the_least_amount() {
        let mut canvas = Canvas::from_layout(
            egui::Id::new("c"),
            &cards(&[
                json!({"kind":"feed","name":"a"}),
                json!({"kind":"feed","name":"b"}),
                json!({"kind":"feed","name":"c"}),
            ]),
        );
        // b sits at x 500..920: visible in a 1000-wide view, so no pan.
        canvas.toggle_collapsed(&feed("b"));
        assert!(canvas.reveal(&feed("b"), VIEWPORT));
        assert_eq!(canvas.view, Pos2::ZERO);
        assert_eq!(canvas.focused_target(), Some(&feed("b")));
        assert!(!canvas.card(&feed("b")).unwrap().collapsed);
        // c sits at x 1000..1420: pan right just enough, keeping a margin.
        assert!(canvas.reveal(&feed("c"), VIEWPORT));
        assert_eq!(canvas.view, Pos2::new(1420.0 + MARGIN - 1000.0, 0.0));
        // a is now off to the left: its left edge comes into view.
        assert!(canvas.reveal(&feed("a"), VIEWPORT));
        assert_eq!(canvas.view, Pos2::new(-MARGIN, 0.0));
        // Too tall for the view: aligned at its top.
        assert!(canvas.reveal(&feed("a"), Vec2::new(1000.0, 300.0)));
        assert_eq!(canvas.view.y, 100.0 - MARGIN);
        assert!(!canvas.reveal(&feed("missing"), VIEWPORT));
    }

    #[test]
    fn moving_resizing_collapsing_closing_and_retargeting() {
        let mut canvas = Canvas::from_layout(
            egui::Id::new("c"),
            &cards(&[
                json!({"kind":"feed","name":"a"}),
                json!({"kind":"board","id":2}),
            ]),
        );
        canvas.move_by(&feed("a"), Vec2::new(10.0, 20.0));
        assert_eq!(
            canvas.card(&feed("a")).unwrap().rect.min,
            Pos2::new(10.0, 120.0)
        );
        canvas.resize(&feed("a"), Vec2::new(50.0, 900.0));
        assert_eq!(
            canvas.card(&feed("a")).unwrap().rect.size(),
            Vec2::new(MIN_CARD.x, 900.0)
        );
        canvas.toggle_collapsed(&feed("a"));
        let card = canvas.card(&feed("a")).unwrap();
        assert_eq!(
            (card.shown().height(), card.rect.height()),
            (TITLE_HEIGHT, 900.0)
        );
        // One card per target: retargeting onto a placed target is refused.
        assert!(!canvas.retarget(&feed("a"), Target::Board(2)));
        assert!(canvas.retarget(&feed("a"), feed("z")));
        assert_eq!(
            canvas.card(&feed("z")).unwrap().rect.min,
            Pos2::new(10.0, 120.0)
        );
        assert!(canvas.raise(&feed("z")));
        assert_eq!(canvas.focused_target(), Some(&feed("z")));
        canvas.close(&feed("z"));
        assert_eq!(canvas.targets(), BTreeSet::from([Target::Board(2)]));
    }

    #[test]
    fn show_all_pans_to_the_top_left_of_every_card() {
        let mut canvas = Canvas::from_layout(
            egui::Id::new("c"),
            &cards(&[
                json!({"kind":"feed","name":"a"}),
                json!({"kind":"feed","name":"b"}),
            ]),
        );
        canvas.move_by(&feed("b"), Vec2::new(0.0, -300.0));
        canvas.pan_by(Vec2::new(5000.0, 5000.0));
        canvas.show_all();
        assert_eq!(canvas.view, Pos2::new(-MARGIN, -200.0 - MARGIN));
        let mut empty = Canvas::new(egui::Id::new("e"));
        empty.pan_by(Vec2::new(7.0, 7.0));
        empty.show_all();
        assert_eq!(empty.view, Pos2::new(7.0, 7.0), "nothing to show");
    }

    #[test]
    fn layouts_switch_keep_working_copies_and_follow_unmodified_saves() {
        let mut layouts = Layouts::new();
        assert_eq!(layouts.active_key(), &LayoutKey::Unsaved);
        layouts.sync_saved(&[named("Day", one("reviews")), named("Ops", one("ops"))]);
        // The first saved layout opens automatically when nothing was placed.
        assert_eq!(layouts.active_key(), &LayoutKey::Saved("Day".into()));
        layouts.active_mut().place(feed("alerts"), None, VIEWPORT);
        assert!(layouts.entries()[0].pending);
        layouts.switch(LayoutKey::Saved("Ops".into()));
        assert_eq!(layouts.active().targets(), BTreeSet::from([feed("ops")]));
        layouts.switch(LayoutKey::Saved("Day".into()));
        assert_eq!(layouts.active().targets().len(), 2, "arrangement kept");
        // A newer save: the unchanged copy follows; the one with unsaved
        // changes keeps them (its auto-save will win) and is flagged.
        layouts.sync_saved(&[named("Day", one("changed")), named("Ops", one("ops2"))]);
        let entries = layouts.entries();
        assert!(entries[0].pending && entries[0].outdated);
        assert!(!entries[1].pending && !entries[1].outdated);
        layouts.revert();
        assert_eq!(
            layouts.active().targets(),
            BTreeSet::from([feed("changed")])
        );
        layouts.switch(LayoutKey::Saved("Ops".into()));
        assert_eq!(layouts.active().targets(), BTreeSet::from([feed("ops2")]));
        // A layout missing from the list keeps its working copy but is marked.
        layouts.sync_saved(&[named("Day", one("changed"))]);
        let ops = entry(&layouts, "Ops");
        assert!(ops.gone && ops.active);
        layouts.switch(LayoutKey::Unsaved);
        assert!(layouts.active().cards.is_empty());
    }

    #[test]
    fn placed_unsaved_cards_are_not_replaced_by_the_first_saved_layout() {
        let mut layouts = Layouts::new();
        layouts.active_mut().place(feed("a"), None, VIEWPORT);
        layouts.sync_saved(&[named("Day", one("b"))]);
        assert_eq!(layouts.active_key(), &LayoutKey::Unsaved);
        assert_eq!(layouts.entries().len(), 2);
    }

    #[test]
    fn layouts_saved_elsewhere_in_other_units_are_not_reported_as_changed() {
        let fractional = layout(json!({"view":{"x":0.4,"y":0},"cards":[
            {"target":{"kind":"feed","name":"a"},"x":10.2,"y":0,"width":2,"height":2,"collapsed":false}]}));
        let mut layouts = Layouts::new();
        layouts.sync_saved(&[named("A", fractional.clone())]);
        assert!(!entry(&layouts, "A").pending);
        assert!(
            layouts
                .due_saves(Instant::now() + SAVE_DELAY * 2)
                .is_empty()
        );
        layouts.sync_saved(&[named("A", fractional)]);
        assert!(!entry(&layouts, "A").outdated);
    }

    #[test]
    fn changes_auto_save_once_settled_and_failures_retry() {
        let mut layouts = Layouts::new();
        layouts.sync_saved(&[named("Day", one("a"))]);
        let t0 = Instant::now();
        assert!(layouts.due_saves(t0).is_empty(), "nothing changed");
        // Panning is part of the layout, so it saves.
        layouts.active_mut().pan_by(Vec2::new(0.0, 40.0));
        assert!(entry(&layouts, "Day").pending);
        assert!(
            layouts.due_saves(t0).is_empty(),
            "waits for changes to settle"
        );
        assert_eq!(layouts.next_save_deadline(t0), Some(SAVE_DELAY));
        // A further change restarts the wait (a drag saves once).
        let t1 = t0 + Duration::from_millis(600);
        layouts
            .active_mut()
            .move_by(&feed("a"), Vec2::new(30.0, 0.0));
        assert!(layouts.due_saves(t1).is_empty());
        assert!(
            layouts
                .due_saves(t1 + Duration::from_millis(500))
                .is_empty()
        );
        let due = layouts.due_saves(t1 + SAVE_DELAY);
        assert_eq!(due.len(), 1);
        let (name, saved) = due[0].clone();
        assert_eq!(name, "Day");
        assert_eq!(saved.view.y, 40.0);
        assert_eq!(saved.cards[0].x, 30.0);
        assert!(entry(&layouts, "Day").saving);
        assert!(
            layouts.due_saves(t1 + SAVE_DELAY * 3).is_empty(),
            "one save at a time"
        );
        // Failure: shown, retried after SAVE_RETRY, not before.
        let t2 = t1 + SAVE_DELAY;
        layouts.save_finished("Day", Err("HTTP 500".into()), t2);
        assert_eq!(
            entry(&layouts, "Day").save_error.as_deref(),
            Some("HTTP 500")
        );
        assert!(layouts.due_saves(t2 + Duration::from_secs(1)).is_empty());
        let retry = layouts.due_saves(t2 + SAVE_RETRY);
        assert_eq!(retry, vec![(name.clone(), saved.clone())]);
        // The service's notice for our own save arrives before the response:
        // not treated as a change from another window.
        layouts.sync_saved(&[named("Day", saved.clone())]);
        assert!(!entry(&layouts, "Day").outdated);
        layouts.save_finished("Day", Ok(saved.clone()), t2 + SAVE_RETRY);
        let day = entry(&layouts, "Day");
        assert!(!day.pending && !day.saving && day.save_error.is_none() && !day.outdated);
        assert_eq!(
            layouts.active().view,
            Pos2::new(0.0, 40.0),
            "arrangement kept"
        );
        // Raising a card changes the saved order, so it saves too.
        layouts.active_mut().place(feed("b"), None, VIEWPORT);
        layouts.save_finished("Day", Ok(layouts.active().to_layout()), t2);
        layouts.active_mut().raise(&feed("a"));
        assert!(entry(&layouts, "Day").pending);
        // Unchanged copies still follow saves made elsewhere.
        layouts.revert();
        layouts.sync_saved(&[named("Day", one("z"))]);
        assert_eq!(layouts.active().targets(), BTreeSet::from([feed("z")]));
    }

    #[test]
    fn saving_the_unsaved_arrangement_moves_it_under_the_new_name() {
        let mut layouts = Layouts::new();
        layouts.active_mut().place(feed("a"), None, VIEWPORT);
        layouts.active_mut().place(Target::Archive, None, VIEWPORT);
        assert!(!layouts.exists("Focus"));
        assert!(
            layouts
                .due_saves(Instant::now() + SAVE_DELAY * 2)
                .is_empty(),
            "no name"
        );
        let saved = layouts.active().to_layout();
        layouts.adopt(Some(&LayoutKey::Unsaved), "Focus", &saved);
        assert_eq!(layouts.active_key(), &LayoutKey::Saved("Focus".into()));
        assert!(layouts.exists("Focus"));
        // Cards, including the unsaveable archive card, came along.
        assert!(layouts.active().targets().contains(&Target::Archive));
        let focus = entry(&layouts, "Focus");
        assert!(!focus.pending && focus.unsaveable && !focus.gone);
        layouts.switch(LayoutKey::Unsaved);
        assert!(layouts.active().cards.is_empty(), "Unsaved starts over");
        // "New layout" starts empty.
        layouts.adopt(None, "Blank", &Layout::default());
        assert_eq!(layouts.active_key(), &LayoutKey::Saved("Blank".into()));
        assert!(layouts.active().cards.is_empty());
    }

    #[test]
    fn the_preferred_layout_opens_first_unless_it_is_gone() {
        let list = [named("Day", one("a")), named("Ops", one("b"))];
        let mut layouts = Layouts::new();
        layouts.prefer(Some("Ops".into()));
        layouts.sync_saved(&list);
        assert_eq!(layouts.active_key(), &LayoutKey::Saved("Ops".into()));
        // Only the first load follows the preference.
        layouts.switch(LayoutKey::Saved("Day".into()));
        layouts.sync_saved(&list);
        assert_eq!(layouts.active_key(), &LayoutKey::Saved("Day".into()));

        let mut layouts = Layouts::new();
        layouts.prefer(Some("Missing".into()));
        layouts.sync_saved(&list);
        assert_eq!(layouts.active_key(), &LayoutKey::Saved("Day".into()));
    }

    #[test]
    fn renaming_moves_the_working_copy_and_holds_saves_until_done() {
        let mut layouts = Layouts::new();
        layouts.sync_saved(&[named("Day", one("a")), named("Ops", one("b"))]);
        let t0 = Instant::now();
        layouts.active_mut().place(feed("c"), None, VIEWPORT);
        assert!(layouts.due_saves(t0).is_empty());
        assert!(layouts.begin_op("Day"));
        assert!(
            layouts.due_saves(t0 + SAVE_DELAY).is_empty(),
            "a save under the old name could recreate it"
        );
        assert_eq!(layouts.next_save_deadline(t0), None);
        let arranged = layouts.active().to_layout();
        layouts.renamed("Day", &named("Morning", one("a")));
        assert_eq!(layouts.active_key(), &LayoutKey::Saved("Morning".into()));
        assert_eq!(layouts.active().to_layout(), arranged, "arrangement kept");
        assert!(!layouts.exists("Day") && layouts.exists("Morning"));
        // The pending change now saves under the new name.
        let due = layouts.due_saves(t0 + SAVE_DELAY);
        assert_eq!(due, vec![("Morning".to_owned(), arranged.clone())]);
        assert!(!layouts.begin_op("Morning"), "refused while saving");
        layouts.save_finished("Morning", Ok(arranged), t0 + SAVE_DELAY);
        assert!(layouts.begin_op("Morning"));
        layouts.end_op("Morning");
        layouts.active_mut().place(feed("d"), None, VIEWPORT);
        assert!(layouts.due_saves(t0 + SAVE_DELAY * 2).is_empty());
        assert_eq!(layouts.due_saves(t0 + SAVE_DELAY * 3).len(), 1, "resumed");
    }

    #[test]
    fn deleting_the_active_layout_opens_another_or_unsaved() {
        let mut layouts = Layouts::new();
        layouts.sync_saved(&[named("Day", one("a")), named("Ops", one("b"))]);
        layouts.active_mut().place(feed("c"), None, VIEWPORT);
        assert!(layouts.begin_op("Day"));
        layouts.deleted("Day");
        assert_eq!(layouts.active_key(), &LayoutKey::Saved("Ops".into()));
        assert!(!layouts.exists("Day"));
        assert!(
            layouts
                .due_saves(Instant::now() + SAVE_DELAY * 2)
                .is_empty(),
            "the deleted layout's pending change is dropped"
        );
        // Deleting an inactive layout leaves the active one alone.
        layouts.switch(LayoutKey::Unsaved);
        layouts.deleted("Ops");
        assert_eq!(layouts.active_key(), &LayoutKey::Unsaved);
        layouts.adopt(None, "Last", &Layout::default());
        layouts.deleted("Last");
        assert_eq!(layouts.active_key(), &LayoutKey::Unsaved);
        assert_eq!(layouts.entries().len(), 1);
    }

    #[test]
    fn rebuilt_canvases_get_fresh_ids_so_card_state_does_not_carry_over() {
        let mut layouts = Layouts::new();
        layouts.sync_saved(&[named("Day", one("a"))]);
        let before = layouts.active().id;
        layouts.active_mut().place(feed("b"), None, VIEWPORT);
        layouts.revert();
        assert_ne!(layouts.active().id, before);
        let reverted = layouts.active().id;
        layouts.sync_saved(&[named("Day", one("c"))]);
        assert_ne!(layouts.active().id, reverted);
    }

    #[test]
    fn an_added_archive_card_survives_newer_saves_made_elsewhere() {
        let mut layouts = Layouts::new();
        layouts.sync_saved(&[named("Day", one("a"))]);
        layouts.active_mut().place(Target::Archive, None, VIEWPORT);
        let day = entry(&layouts, "Day");
        assert!(day.unsaveable && !day.pending, "nothing else to save");
        layouts.sync_saved(&[named("Day", one("b"))]);
        assert!(entry(&layouts, "Day").outdated);
        assert!(layouts.active().targets().contains(&Target::Archive));
    }
}
