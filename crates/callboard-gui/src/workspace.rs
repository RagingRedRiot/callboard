//! Panel arrangement and named layouts (DESIGN.md §6.1, §6.4).
//!
//! The service's toolkit-independent [`Panel`] tree is converted to an
//! `egui_tiles` tree for display and back for saving. Each named layout has a
//! working copy; arrangement changes auto-save to it (§6.4). The Unsaved
//! arrangement has no name until "Save as…".
use crate::backend::Target;
use callboard_core::layout::{Axis, NamedLayout, Panel};
use eframe::egui;
use egui_tiles::{
    Container, Linear, LinearDir, SimplificationOptions, Tabs, Tile, TileId, Tiles, Tree,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub target: Target,
}

/// Every pane sits in a tab group, so each has a title bar to drag and close.
pub fn simplification() -> SimplificationOptions {
    SimplificationOptions {
        all_panes_must_have_tabs: true,
        ..Default::default()
    }
}

fn insert(tiles: &mut Tiles<Pane>, panel: &Panel) -> Option<TileId> {
    Some(match panel {
        Panel::Empty {} => return None,
        Panel::Feed { name } => tiles.insert_pane(Pane {
            target: Target::Feed(name.clone()),
        }),
        Panel::Board { id } => tiles.insert_pane(Pane {
            target: Target::Board(*id),
        }),
        Panel::Split {
            axis,
            children,
            weights,
        } => {
            let dir = match axis {
                Axis::Horizontal => LinearDir::Horizontal,
                Axis::Vertical => LinearDir::Vertical,
            };
            let mut linear = Linear::new(dir, vec![]);
            // Normalize in f64 before narrowing: the service accepts weights
            // such as 1e-50 or 1e300 that are 0 or infinite as f32 shares.
            let sum: f64 = weights.iter().sum();
            let count = children.len() as f64;
            for (child, weight) in children.iter().zip(weights) {
                if let Some(id) = insert(tiles, child) {
                    linear.children.push(id);
                    let share = (weight / sum).max(1e-4) * count;
                    linear.shares.set_share(id, share as f32);
                }
            }
            tiles.insert_container(linear)
        }
        Panel::Tabs { children, active } => {
            let ids: Vec<_> = children.iter().filter_map(|c| insert(tiles, c)).collect();
            let mut tabs = Tabs::new(ids.clone());
            if let Some(id) = ids.get(*active) {
                tabs.set_active(*id);
            }
            tiles.insert_container(tabs)
        }
    })
}

pub fn tree_from_panel(id: egui::Id, panel: &Panel) -> Tree<Pane> {
    let mut tiles = Tiles::default();
    let root = insert(&mut tiles, panel);
    let mut tree = match root {
        Some(root) => Tree::new(id, root, tiles),
        None => Tree::empty(id),
    };
    tree.simplify(&simplification());
    tree
}

fn panel_of(tiles: &Tiles<Pane>, id: TileId) -> Option<Panel> {
    match tiles.get(id)? {
        Tile::Pane(pane) => match &pane.target {
            Target::Feed(name) => Some(Panel::Feed { name: name.clone() }),
            Target::Board(id) => Some(Panel::Board { id: *id }),
            // The archive is not a layout target; it is dropped on conversion.
            Target::Archive => None,
        },
        Tile::Container(Container::Tabs(tabs)) => {
            let mut children = Vec::new();
            let mut active = 0;
            for child in &tabs.children {
                let panel = panel_of(tiles, *child);
                if tabs.active == Some(*child) {
                    // A dropped active tab (the archive) saves as its left
                    // neighbour, the tab that stays nearest to it.
                    active = match panel {
                        Some(_) => children.len(),
                        None => children.len().saturating_sub(1),
                    };
                }
                children.extend(panel);
            }
            match children.len() {
                0 => None,
                // Single-pane tab groups exist only to give panes a title bar.
                1 => children.pop(),
                _ => Some(Panel::Tabs { children, active }),
            }
        }
        Tile::Container(Container::Linear(linear)) => {
            let axis = match linear.dir {
                LinearDir::Horizontal => Axis::Horizontal,
                LinearDir::Vertical => Axis::Vertical,
            };
            split(
                axis,
                linear
                    .children
                    .iter()
                    .filter_map(|c| Some((panel_of(tiles, *c)?, f64::from(linear.shares[*c])))),
            )
        }
        // Not produced by this GUI; saved as an equal-weight row.
        Tile::Container(Container::Grid(grid)) => split(
            Axis::Horizontal,
            grid.children()
                .filter_map(|c| Some((panel_of(tiles, *c)?, 1.0))),
        ),
    }
}

fn split(axis: Axis, parts: impl Iterator<Item = (Panel, f64)>) -> Option<Panel> {
    let (mut children, weights): (Vec<_>, Vec<_>) = parts.unzip();
    match children.len() {
        0 => None,
        1 => children.pop(),
        _ => Some(Panel::Split {
            axis,
            children,
            weights,
        }),
    }
}

/// The service representation of a tree. Archive panes are omitted.
pub fn panel_from_tree(tree: &Tree<Pane>) -> Panel {
    tree.root
        .and_then(|root| panel_of(&tree.tiles, root))
        .unwrap_or(Panel::Empty {})
}

/// Whether the user rearranged panels: structure, targets, and split
/// proportions. Which tab is in front is browsing, not rearranging.
pub fn same_arrangement(a: &Panel, b: &Panel) -> bool {
    compare(a, b, false)
}

/// Full equality including active tabs, for detecting newer saved versions.
pub fn same_layout(a: &Panel, b: &Panel) -> bool {
    compare(a, b, true)
}

/// Split weights compare as normalized proportions, so an `f64` → `f32` →
/// `f64` round trip does not count as a change.
fn compare(a: &Panel, b: &Panel, active: bool) -> bool {
    match (a, b) {
        (
            Panel::Split {
                axis: a_axis,
                children: a_children,
                weights: a_weights,
            },
            Panel::Split {
                axis: b_axis,
                children: b_children,
                weights: b_weights,
            },
        ) => {
            let (a_sum, b_sum) = (a_weights.iter().sum::<f64>(), b_weights.iter().sum::<f64>());
            a_axis == b_axis
                && a_children.len() == b_children.len()
                && a_weights.len() == b_weights.len()
                && a_weights
                    .iter()
                    .zip(b_weights)
                    .all(|(x, y)| (x / a_sum - y / b_sum).abs() < 1e-4)
                && a_children
                    .iter()
                    .zip(b_children)
                    .all(|(x, y)| compare(x, y, active))
        }
        (
            Panel::Tabs {
                children: a_children,
                active: a_active,
            },
            Panel::Tabs {
                children: b_children,
                active: b_active,
            },
        ) => {
            (!active || a_active == b_active)
                && a_children.len() == b_children.len()
                && a_children
                    .iter()
                    .zip(b_children)
                    .all(|(x, y)| compare(x, y, active))
        }
        _ => a == b,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Add a tab beside the focused panel.
    Tab,
    /// Split the focused panel's group and place to the right.
    Right,
    /// Split the focused panel's group and place below.
    Below,
    /// Point the focused panel at the target instead.
    Replace,
}

/// One arrangement of panels plus which panel has focus.
pub struct Workspace {
    pub tree: Tree<Pane>,
    pub focused: Option<TileId>,
}

impl Workspace {
    pub fn new(id: egui::Id, panel: &Panel) -> Self {
        let mut workspace = Self {
            tree: tree_from_panel(id, panel),
            focused: None,
        };
        workspace.fix_focus();
        workspace
    }

    pub fn panel(&self) -> Panel {
        panel_from_tree(&self.tree)
    }

    /// Panes reachable from the root, in one walk of the tree.
    fn placed(&self) -> Vec<(TileId, &Pane)> {
        let mut panes = Vec::new();
        let mut stack: Vec<TileId> = self.tree.root.into_iter().collect();
        while let Some(id) = stack.pop() {
            match self.tree.tiles.get(id) {
                Some(Tile::Pane(pane)) => panes.push((id, pane)),
                Some(Tile::Container(container)) => stack.extend(container.children()),
                None => (),
            }
        }
        panes
    }

    /// All targets placed in this arrangement, visible or in background tabs.
    pub fn targets(&self) -> BTreeSet<Target> {
        self.placed()
            .into_iter()
            .map(|(_, p)| p.target.clone())
            .collect()
    }

    /// Whether any panel cannot be saved in a layout (the archive).
    pub fn has_unsaveable(&self) -> bool {
        self.placed()
            .iter()
            .any(|(_, p)| p.target == Target::Archive)
    }

    pub fn target_of(&self, id: TileId) -> Option<&Target> {
        match self.tree.tiles.get(id)? {
            Tile::Pane(pane) => Some(&pane.target),
            Tile::Container(_) => None,
        }
    }

    pub fn focused_target(&self) -> Option<&Target> {
        self.target_of(self.focused?)
    }

    /// Keep focus on an existing pane after closes and drags.
    pub fn fix_focus(&mut self) {
        if self
            .focused
            .is_some_and(|id| self.placed().iter().any(|(p, _)| *p == id))
        {
            return;
        }
        let active = self.tree.active_tiles();
        self.focused = active
            .iter()
            .copied()
            .filter(|id| self.target_of(*id).is_some())
            .min_by_key(|id| id.0);
    }

    fn find(&self, target: &Target) -> Option<TileId> {
        let focused = self
            .focused
            .filter(|id| self.target_of(*id) == Some(target));
        focused.or_else(|| {
            self.placed()
                .into_iter()
                .filter(|(_, p)| &p.target == target)
                .map(|(id, _)| id)
                .min_by_key(|id| id.0)
        })
    }

    /// Bring an already placed target to the front and focus it.
    pub fn reveal(&mut self, target: &Target) -> bool {
        let Some(id) = self.find(target) else {
            return false;
        };
        self.tree.make_active(|tile, _| tile == id);
        self.focused = Some(id);
        true
    }

    /// Reveal the target if placed, otherwise open it as a tab.
    pub fn show(&mut self, target: Target) -> TileId {
        if self.reveal(&target) {
            return self.focused.expect("revealed");
        }
        self.open(target, Placement::Tab)
    }

    pub fn open(&mut self, target: Target, placement: Placement) -> TileId {
        self.fix_focus();
        let Some(anchor) = self.focused else {
            let id = self.tree.tiles.insert_pane(Pane { target });
            if let Some(old) = self.tree.root {
                self.tree.remove_recursively(old);
            }
            self.tree.root = Some(id);
            return self.settle(id);
        };
        if placement == Placement::Replace {
            self.retarget(anchor, target);
            return anchor;
        }
        let id = self.tree.tiles.insert_pane(Pane { target });
        let parent = self.tree.tiles.parent_of(anchor);
        match placement {
            Placement::Tab => {
                if let Some(Tile::Container(Container::Tabs(tabs))) =
                    parent.and_then(|p| self.tree.tiles.get_mut(p))
                {
                    let at = tabs
                        .children
                        .iter()
                        .position(|c| *c == anchor)
                        .map_or(tabs.children.len(), |i| i + 1);
                    tabs.children.insert(at, id);
                    tabs.set_active(id);
                } else {
                    let tabs = self
                        .tree
                        .tiles
                        .insert_container(Tabs::new(vec![anchor, id]));
                    self.replace(parent, anchor, tabs);
                    if let Some(Tile::Container(Container::Tabs(t))) = self.tree.tiles.get_mut(tabs)
                    {
                        t.set_active(id);
                    }
                }
            }
            Placement::Right | Placement::Below => {
                let dir = if placement == Placement::Right {
                    LinearDir::Horizontal
                } else {
                    LinearDir::Vertical
                };
                // Split beside the whole tab group, not inside it.
                let group = match parent.and_then(|p| self.tree.tiles.get(p)) {
                    Some(Tile::Container(Container::Tabs(_))) => parent.expect("tab group"),
                    _ => anchor,
                };
                let outer = self.tree.tiles.parent_of(group);
                match outer.and_then(|p| self.tree.tiles.get_mut(p)) {
                    Some(Tile::Container(Container::Linear(linear))) if linear.dir == dir => {
                        let at = linear
                            .children
                            .iter()
                            .position(|c| *c == group)
                            .map_or(linear.children.len(), |i| i + 1);
                        let half = linear.shares[group] / 2.0;
                        linear.shares.set_share(group, half);
                        linear.shares.set_share(id, half);
                        linear.children.insert(at, id);
                    }
                    _ => {
                        let linear = self.tree.tiles.insert_container(Linear::new_binary(
                            dir,
                            [group, id],
                            0.5,
                        ));
                        self.replace(outer, group, linear);
                    }
                }
            }
            Placement::Replace => unreachable!("handled above"),
        }
        self.settle(id)
    }

    /// Put the wrapper `new` where `old` was under `parent` (looked up before
    /// `new` was created), keeping `old`'s share or active-tab status.
    fn replace(&mut self, parent: Option<TileId>, old: TileId, new: TileId) {
        match parent {
            Some(parent) => {
                if let Some(Tile::Container(container)) = self.tree.tiles.get_mut(parent) {
                    let _ = container.replace_child(old, new);
                }
            }
            None => self.tree.root = Some(new),
        }
    }

    fn settle(&mut self, id: TileId) -> TileId {
        self.tree.simplify(&simplification());
        self.tree.make_active(|tile, _| tile == id);
        self.focused = Some(id);
        id
    }

    pub fn retarget(&mut self, id: TileId, target: Target) {
        if let Some(Tile::Pane(pane)) = self.tree.tiles.get_mut(id) {
            pane.target = target;
        }
    }

    /// The pane a container would show first: its active tab, or first child.
    fn first_pane(&self, id: TileId) -> Option<TileId> {
        match self.tree.tiles.get(id)? {
            Tile::Pane(_) => Some(id),
            Tile::Container(Container::Tabs(tabs)) => tabs
                .active
                .into_iter()
                .chain(tabs.children.iter().copied())
                .find_map(|c| self.first_pane(c)),
            Tile::Container(container) => container.children().find_map(|c| self.first_pane(*c)),
        }
    }

    /// Close a panel; focus moves to a neighbour in the same group.
    pub fn close(&mut self, id: TileId) {
        let mut siblings = Vec::new();
        if let Some(parent) = self.tree.tiles.parent_of(id)
            && let Some(Tile::Container(container)) = self.tree.tiles.get_mut(parent)
        {
            let at = container.remove_child(id).unwrap_or(0);
            siblings = container.children_vec();
            // Prefer the neighbour that took the closed panel's place.
            let neighbour = at.min(siblings.len().saturating_sub(1));
            siblings.rotate_left(neighbour);
        } else if self.tree.root == Some(id) {
            self.tree.root = None;
        }
        self.tree.remove_recursively(id);
        self.tree.simplify(&simplification());
        if self.focused == Some(id) || self.focused.is_none() {
            self.focused = siblings.into_iter().find_map(|s| self.first_pane(s));
        }
        self.fix_focus();
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
/// (DESIGN.md §6.4), so a resize drag saves once when it ends.
pub const SAVE_DELAY: Duration = Duration::from_secs(1);
/// A failed save is retried after this long.
pub const SAVE_RETRY: Duration = Duration::from_secs(5);

#[derive(Default)]
struct SaveState {
    /// The last arrangement seen differing from `base`, and since when.
    last_panel: Option<Panel>,
    changed_at: Option<Instant>,
    in_flight: bool,
    error: Option<String>,
    retry_at: Option<Instant>,
}

struct Working {
    workspace: Workspace,
    /// The saved tree (normalized) this copy matches when nothing is pending;
    /// `None` for `Unsaved`, which has no name to save under.
    base: Option<Panel>,
    /// The service has a newer saved version that was not applied.
    outdated: bool,
    save: SaveState,
}

impl Working {
    /// Local changes not yet saved.
    fn pending(&self) -> bool {
        self.base
            .as_ref()
            .is_some_and(|base| !same_layout(&self.workspace.panel(), base))
    }

    /// Following a newer save would lose something: unsaved changes, a save
    /// in flight, or an archive panel (never part of a saved tree).
    fn keep_local(&self) -> bool {
        self.pending() || self.save.in_flight || self.workspace.has_unsaveable()
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
    /// Contains an archive panel, which layouts cannot store.
    pub unsaveable: bool,
    /// Saved in another window while this one kept its own arrangement.
    pub outdated: bool,
    /// No longer present in the service's layout list.
    pub gone: bool,
}

/// Saved layouts from the service plus session-local working copies.
/// Switching layouts never changes feeds or boards.
pub struct Layouts {
    saved: BTreeMap<String, Panel>,
    working: BTreeMap<LayoutKey, Working>,
    active: LayoutKey,
    loaded: bool,
    /// Bumped per rebuilt tree. Tile IDs restart for each tree, so a fresh
    /// tree ID keeps per-panel UI state (scroll, toggles) from leaking.
    generation: u64,
}

impl Default for Layouts {
    fn default() -> Self {
        Self::new()
    }
}

/// A saved tree as the GUI shows it. The service accepts shapes the tile
/// tree simplifies (one-child tabs, nested same-axis splits); comparing
/// against this form keeps such layouts from looking changed on load.
pub fn normalize(panel: &Panel) -> Panel {
    panel_from_tree(&tree_from_panel(
        egui::Id::new("callboard-normalize"),
        panel,
    ))
}

impl Layouts {
    pub fn new() -> Self {
        let mut layouts = Self {
            saved: BTreeMap::new(),
            working: BTreeMap::new(),
            active: LayoutKey::Unsaved,
            loaded: false,
            generation: 0,
        };
        let unsaved = layouts.build(&LayoutKey::Unsaved, &Panel::Empty {});
        layouts.working.insert(LayoutKey::Unsaved, unsaved);
        layouts
    }

    fn build(&mut self, key: &LayoutKey, panel: &Panel) -> Working {
        self.generation += 1;
        let workspace = Workspace::new(
            egui::Id::new(("callboard-layout", key, self.generation)),
            panel,
        );
        let base = matches!(key, LayoutKey::Saved(_)).then(|| workspace.panel());
        Working {
            workspace,
            base,
            outdated: false,
            save: SaveState::default(),
        }
    }

    pub fn active_key(&self) -> &LayoutKey {
        &self.active
    }

    pub fn active(&self) -> &Workspace {
        &self.working[&self.active].workspace
    }

    pub fn active_mut(&mut self) -> &mut Workspace {
        &mut self
            .working
            .get_mut(&self.active)
            .expect("active layout has a working copy")
            .workspace
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
            .map(|l| (l.name.clone(), l.tree.clone()))
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
            if same_layout(&saved, base) {
                self.working.get_mut(&key).expect("present").outdated = false;
            } else if !working.keep_local() {
                let rebuilt = self.build(&key, &saved);
                self.working.insert(key, rebuilt);
            } else if !working.save.in_flight {
                // In flight: our own save is about to become the saved version.
                self.working.get_mut(&key).expect("present").outdated = true;
            }
        }
        // Start in the first saved layout unless the user already arranged panels.
        if !self.loaded {
            self.loaded = true;
            let untouched = self.active == LayoutKey::Unsaved && self.active().tree.is_empty();
            if untouched && let Some(name) = self.saved.keys().next().cloned() {
                self.switch(LayoutKey::Saved(name));
            }
        }
    }

    /// Saved layouts with changes that have settled for [`SAVE_DELAY`]. Each
    /// returned save is in flight until [`Self::save_finished`].
    pub fn due_saves(&mut self, now: Instant) -> Vec<(String, Panel)> {
        let mut due = Vec::new();
        for (key, working) in &mut self.working {
            let LayoutKey::Saved(name) = key else {
                continue;
            };
            if working.save.in_flight {
                continue;
            }
            let panel = working.workspace.panel();
            let Some(base) = &working.base else { continue };
            if same_layout(&panel, base) {
                working.save.last_panel = None;
                working.save.changed_at = None;
                continue;
            }
            if !working
                .save
                .last_panel
                .as_ref()
                .is_some_and(|last| same_layout(last, &panel))
            {
                working.save.last_panel = Some(panel.clone());
                working.save.changed_at = Some(now);
            }
            let settled = working
                .save
                .changed_at
                .is_some_and(|t| now >= t + SAVE_DELAY);
            let retry_ok = working.save.retry_at.is_none_or(|t| now >= t);
            if settled && retry_ok {
                working.save.in_flight = true;
                due.push((name.clone(), panel));
            }
        }
        due
    }

    /// When [`Self::due_saves`] could next return something.
    pub fn next_save_deadline(&self, now: Instant) -> Option<Duration> {
        self.working
            .values()
            .filter(|w| !w.save.in_flight)
            .filter_map(|w| {
                let changed = w.save.changed_at? + SAVE_DELAY;
                Some(w.save.retry_at.map_or(changed, |r| r.max(changed)))
            })
            .min()
            .map(|t| t.saturating_duration_since(now))
    }

    /// Record the outcome of an auto-save. On success `tree` (as stored by
    /// the service) becomes the base; a failure is retried after [`SAVE_RETRY`].
    pub fn save_finished(&mut self, name: &str, result: Result<Panel, String>, now: Instant) {
        let key = LayoutKey::Saved(name.to_owned());
        if let Ok(tree) = &result {
            self.saved.insert(name.to_owned(), tree.clone());
        }
        let Some(working) = self.working.get_mut(&key) else {
            return;
        };
        working.save.in_flight = false;
        match result {
            Ok(tree) => {
                working.base = Some(normalize(&tree));
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
    /// `tree` under `name`. Saving the Unsaved arrangement moves it (panels,
    /// focus, and archive panels intact); others start from the saved tree.
    pub fn adopt(&mut self, from: Option<&LayoutKey>, name: &str, tree: &Panel) {
        let key = LayoutKey::Saved(name.to_owned());
        self.saved.insert(name.to_owned(), tree.clone());
        let working = match from {
            Some(LayoutKey::Unsaved) => {
                let fresh = self.build(&LayoutKey::Unsaved, &Panel::Empty {});
                let mut moved = self
                    .working
                    .insert(LayoutKey::Unsaved, fresh)
                    .expect("Unsaved always has a working copy");
                moved.base = Some(normalize(tree));
                moved
            }
            _ => self.build(&key, tree),
        };
        self.working.insert(key.clone(), working);
        self.active = key;
    }

    pub fn switch(&mut self, key: LayoutKey) {
        if !self.working.contains_key(&key) {
            let panel = match &key {
                LayoutKey::Saved(name) => self.saved.get(name).cloned(),
                LayoutKey::Unsaved => None,
            };
            let Some(panel) = panel else { return };
            let working = self.build(&key, &panel);
            self.working.insert(key.clone(), working);
        }
        self.active = key;
    }

    /// Replace the active working copy with the saved version.
    pub fn revert(&mut self) {
        let key = self.active.clone();
        let panel = match &key {
            LayoutKey::Saved(name) => self.saved.get(name).cloned(),
            LayoutKey::Unsaved => Some(Panel::Empty {}),
        };
        if let Some(panel) = panel {
            let working = self.build(&key, &panel);
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
                    unsaveable: working.is_some_and(|w| w.workspace.has_unsaveable()),
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

    fn panel(value: serde_json::Value) -> Panel {
        serde_json::from_value(value).unwrap()
    }

    fn feed(name: &str) -> Target {
        Target::Feed(name.into())
    }

    fn named(name: &str, tree: serde_json::Value) -> NamedLayout {
        NamedLayout {
            name: name.into(),
            tree: panel(tree),
            updated_at_ms: 0,
        }
    }

    #[test]
    fn saved_trees_round_trip_including_weights_tabs_and_deleted_targets() {
        let saved = panel(
            json!({"kind":"split","axis":"horizontal","weights":[2.0,1.0],"children":[
            {"kind":"tabs","active":1,"children":[{"kind":"feed","name":"reviews"},{"kind":"board","id":7}]},
            {"kind":"split","axis":"vertical","weights":[0.3,0.7],"children":[
                {"kind":"feed","name":"deleted-feed"},{"kind":"board","id":99}]}]}),
        );
        let workspace = Workspace::new(egui::Id::new("t"), &saved);
        assert!(same_arrangement(&workspace.panel(), &saved));
        // Targets are kept whether or not they still exist (placeholders).
        assert_eq!(
            workspace.targets(),
            [
                feed("reviews"),
                feed("deleted-feed"),
                Target::Board(7),
                Target::Board(99)
            ]
            .into_iter()
            .collect()
        );
        let empty = Workspace::new(egui::Id::new("e"), &Panel::Empty {});
        assert!(empty.tree.is_empty());
        assert_eq!(empty.panel(), Panel::Empty {});
        assert!(!same_arrangement(
            &saved,
            &panel(
                json!({"kind":"split","axis":"horizontal","weights":[1.0,1.0],"children":[
                {"kind":"feed","name":"a"},{"kind":"feed","name":"b"}]})
            )
        ));
    }

    #[test]
    fn placements_build_tabs_and_splits_around_the_focused_panel() {
        let mut w = Workspace::new(egui::Id::new("p"), &Panel::Empty {});
        let a = w.open(feed("a"), Placement::Tab);
        assert_eq!(w.panel(), panel(json!({"kind":"feed","name":"a"})));
        assert_eq!(w.focused, Some(a));
        w.open(feed("b"), Placement::Tab);
        assert_eq!(
            w.panel(),
            panel(json!({"kind":"tabs","active":1,"children":[
                {"kind":"feed","name":"a"},{"kind":"feed","name":"b"}]}))
        );
        w.open(Target::Board(3), Placement::Right);
        assert_eq!(
            w.panel(),
            panel(
                json!({"kind":"split","axis":"horizontal","weights":[1.0,1.0],"children":[
                {"kind":"tabs","active":1,"children":[
                    {"kind":"feed","name":"a"},{"kind":"feed","name":"b"}]},
                {"kind":"board","id":3}]})
            )
        );
        w.open(feed("e"), Placement::Right);
        let Panel::Split {
            children, weights, ..
        } = w.panel()
        else {
            panic!("expected a row")
        };
        // Splitting in the same direction joins the row and halves the share.
        assert_eq!(children.len(), 3);
        assert!(same_arrangement(
            &Panel::Split {
                axis: Axis::Horizontal,
                children: children.clone(),
                weights
            },
            &Panel::Split {
                axis: Axis::Horizontal,
                children,
                weights: vec![2.0, 1.0, 1.0],
            }
        ));
        w.open(feed("d"), Placement::Below);
        let Panel::Split { children, .. } = w.panel() else {
            panic!("expected a row")
        };
        assert!(matches!(
            &children[2],
            Panel::Split {
                axis: Axis::Vertical,
                ..
            }
        ));
        let before = w.panel();
        w.open(feed("z"), Placement::Replace);
        assert_eq!(w.focused_target(), Some(&feed("z")));
        assert_eq!(w.targets().len(), before_targets(&before));
    }

    fn before_targets(panel: &Panel) -> usize {
        match panel {
            Panel::Split { children, .. } | Panel::Tabs { children, .. } => {
                children.iter().map(before_targets).sum()
            }
            Panel::Empty {} => 0,
            _ => 1,
        }
    }

    #[test]
    fn showing_a_placed_target_reveals_it_instead_of_duplicating() {
        let saved = panel(
            json!({"kind":"split","axis":"vertical","weights":[1.0,1.0],"children":[
            {"kind":"tabs","active":0,"children":[{"kind":"feed","name":"a"},{"kind":"feed","name":"b"}]},
            {"kind":"board","id":2}]}),
        );
        let mut w = Workspace::new(egui::Id::new("r"), &saved);
        w.show(feed("b"));
        let Panel::Split { children, .. } = w.panel() else {
            panic!()
        };
        assert!(matches!(children[0], Panel::Tabs { active: 1, .. }));
        assert_eq!(w.focused_target(), Some(&feed("b")));
        assert_eq!(w.targets().len(), 3);
        w.show(Target::Archive);
        assert_eq!(w.focused_target(), Some(&Target::Archive));
        // The archive is viewable but never part of a saved tree; as the
        // active tab it saves as its left neighbour.
        assert!(same_layout(
            &w.panel(),
            &panel(
                json!({"kind":"split","axis":"vertical","weights":[1.0,1.0],"children":[
                {"kind":"tabs","active":1,"children":[{"kind":"feed","name":"a"},{"kind":"feed","name":"b"}]},
                {"kind":"board","id":2}]})
            )
        ));
    }

    #[test]
    fn closing_panels_collapses_containers_and_keeps_focus_valid() {
        let mut w = Workspace::new(egui::Id::new("c"), &Panel::Empty {});
        let a = w.open(feed("a"), Placement::Tab);
        let b = w.open(feed("b"), Placement::Right);
        w.close(b);
        assert_eq!(w.panel(), panel(json!({"kind":"feed","name":"a"})));
        assert_eq!(w.focused, Some(a));
        w.close(a);
        assert_eq!(w.panel(), Panel::Empty {});
        assert_eq!(w.focused, None);
        w.open(feed("c"), Placement::Right);
        assert_eq!(w.panel(), panel(json!({"kind":"feed","name":"c"})));
    }

    #[test]
    fn layouts_switch_keep_working_copies_and_follow_unmodified_saves() {
        let mut layouts = Layouts::new();
        assert_eq!(layouts.active_key(), &LayoutKey::Unsaved);
        layouts.sync_saved(&[
            named("Day", json!({"kind":"feed","name":"reviews"})),
            named("Ops", json!({"kind":"board","id":5})),
        ]);
        // The first saved layout opens automatically when nothing was arranged.
        assert_eq!(layouts.active_key(), &LayoutKey::Saved("Day".into()));
        layouts.active_mut().open(feed("alerts"), Placement::Right);
        assert!(layouts.entries()[0].pending);
        layouts.switch(LayoutKey::Saved("Ops".into()));
        assert_eq!(
            layouts.active().panel(),
            panel(json!({"kind":"board","id":5}))
        );
        layouts.switch(LayoutKey::Saved("Day".into()));
        assert_eq!(layouts.active().targets().len(), 2, "rearrangement kept");
        // A newer save: the unchanged copy follows; the one with unsaved
        // changes keeps them (its auto-save will win) and is flagged.
        layouts.sync_saved(&[
            named("Day", json!({"kind":"feed","name":"changed"})),
            named("Ops", json!({"kind":"board","id":6})),
        ]);
        let entries = layouts.entries();
        assert!(entries[0].pending && entries[0].outdated);
        assert!(!entries[1].pending && !entries[1].outdated);
        layouts.revert();
        assert_eq!(
            layouts.active().panel(),
            panel(json!({"kind":"feed","name":"changed"}))
        );
        layouts.switch(LayoutKey::Saved("Ops".into()));
        assert_eq!(
            layouts.active().panel(),
            panel(json!({"kind":"board","id":6}))
        );
        // A layout missing from the list keeps its working copy but is marked.
        layouts.sync_saved(&[named("Day", json!({"kind":"feed","name":"changed"}))]);
        let ops = layouts
            .entries()
            .into_iter()
            .find(|e| e.key == LayoutKey::Saved("Ops".into()))
            .unwrap();
        assert!(ops.gone && ops.active);
        layouts.switch(LayoutKey::Unsaved);
        assert!(layouts.active().tree.is_empty());
    }

    #[test]
    fn arranged_unsaved_panels_are_not_replaced_by_the_first_saved_layout() {
        let mut layouts = Layouts::new();
        layouts.active_mut().show(feed("a"));
        layouts.sync_saved(&[named("Day", json!({"kind":"board","id":2}))]);
        assert_eq!(layouts.active_key(), &LayoutKey::Unsaved);
        assert_eq!(layouts.entries().len(), 2);
    }

    fn entry(layouts: &Layouts, name: &str) -> LayoutEntry {
        layouts
            .entries()
            .into_iter()
            .find(|e| e.key == LayoutKey::Saved(name.into()))
            .unwrap()
    }

    #[test]
    fn valid_saved_shapes_the_tile_tree_simplifies_are_not_reported_as_rearranged() {
        let one_tab = json!({"kind":"tabs","active":0,"children":[{"kind":"feed","name":"a"}]});
        let nested = json!({"kind":"split","axis":"horizontal","weights":[1.0,1.0],"children":[
            {"kind":"feed","name":"a"},
            {"kind":"split","axis":"horizontal","weights":[1.0,1.0],"children":[
                {"kind":"feed","name":"b"},{"kind":"feed","name":"c"}]}]});
        let mut layouts = Layouts::new();
        layouts.sync_saved(&[named("A", one_tab.clone()), named("B", nested.clone())]);
        layouts.switch(LayoutKey::Saved("B".into()));
        assert!(!entry(&layouts, "A").pending);
        assert!(!entry(&layouts, "B").pending);
        assert!(
            layouts
                .due_saves(Instant::now() + SAVE_DELAY * 2)
                .is_empty()
        );
        // Re-reading the same list changes nothing; a real update is followed.
        layouts.sync_saved(&[named("A", one_tab), named("B", nested)]);
        assert!(!entry(&layouts, "B").outdated);
        layouts.sync_saved(&[
            named("A", json!({"kind":"feed","name":"z"})),
            named("B", json!({"kind":"feed","name":"y"})),
        ]);
        assert_eq!(
            layouts.active().panel(),
            panel(json!({"kind":"feed","name":"y"}))
        );
        layouts.switch(LayoutKey::Saved("A".into()));
        assert_eq!(
            layouts.active().panel(),
            panel(json!({"kind":"feed","name":"z"}))
        );
    }

    #[test]
    fn changes_auto_save_once_settled_and_failures_retry() {
        let saved = json!({"kind":"tabs","active":0,"children":[
            {"kind":"feed","name":"a"},{"kind":"feed","name":"b"}]});
        let mut layouts = Layouts::new();
        layouts.sync_saved(&[named("Day", saved)]);
        let t0 = Instant::now();
        assert!(layouts.due_saves(t0).is_empty(), "nothing changed");
        // Switching tabs changes the saved tree's active index, so it saves.
        layouts.active_mut().show(feed("b"));
        assert!(entry(&layouts, "Day").pending);
        assert!(
            layouts.due_saves(t0).is_empty(),
            "waits for changes to settle"
        );
        assert_eq!(layouts.next_save_deadline(t0), Some(SAVE_DELAY));
        // A further change restarts the wait (a resize drag saves once).
        let t1 = t0 + Duration::from_millis(600);
        layouts.active_mut().open(feed("c"), Placement::Right);
        // The app polls every frame, so it sees the change when it happens.
        assert!(layouts.due_saves(t1).is_empty());
        assert!(
            layouts
                .due_saves(t1 + Duration::from_millis(500))
                .is_empty()
        );
        let due = layouts.due_saves(t1 + SAVE_DELAY);
        assert_eq!(due.len(), 1);
        let (name, tree) = due[0].clone();
        assert_eq!(name, "Day");
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
        assert_eq!(retry, vec![(name.clone(), tree.clone())]);
        // The service's notice for our own save arrives before the response:
        // not treated as a change from another window.
        layouts.sync_saved(&[named("Day", serde_json::to_value(&tree).unwrap())]);
        assert!(!entry(&layouts, "Day").outdated);
        layouts.save_finished("Day", Ok(tree.clone()), t2 + SAVE_RETRY);
        let day = entry(&layouts, "Day");
        assert!(!day.pending && !day.saving && day.save_error.is_none() && !day.outdated);
        assert_eq!(layouts.active().targets().len(), 3, "arrangement kept");
        // Unchanged copies still follow saves made elsewhere.
        layouts.sync_saved(&[named("Day", json!({"kind":"feed","name":"z"}))]);
        assert_eq!(
            layouts.active().panel(),
            panel(json!({"kind":"feed","name":"z"}))
        );
    }

    #[test]
    fn saving_the_unsaved_arrangement_moves_it_under_the_new_name() {
        let mut layouts = Layouts::new();
        layouts.active_mut().show(feed("a"));
        layouts.active_mut().open(Target::Archive, Placement::Right);
        assert!(!layouts.exists("Focus"));
        assert!(
            layouts
                .due_saves(Instant::now() + SAVE_DELAY * 2)
                .is_empty(),
            "no name"
        );
        let tree = layouts.active().panel();
        layouts.adopt(Some(&LayoutKey::Unsaved), "Focus", &tree);
        assert_eq!(layouts.active_key(), &LayoutKey::Saved("Focus".into()));
        assert!(layouts.exists("Focus"));
        // Panels, including the unsaveable archive panel, came along.
        assert!(layouts.active().targets().contains(&Target::Archive));
        let focus = entry(&layouts, "Focus");
        assert!(!focus.pending && focus.unsaveable && !focus.gone);
        layouts.switch(LayoutKey::Unsaved);
        assert!(layouts.active().tree.is_empty(), "Unsaved starts over");
        // "New layout" starts empty.
        layouts.adopt(None, "Blank", &Panel::Empty {});
        assert_eq!(layouts.active_key(), &LayoutKey::Saved("Blank".into()));
        assert!(layouts.active().tree.is_empty());
    }

    #[test]
    fn rebuilt_trees_get_fresh_ids_so_panel_state_does_not_carry_over() {
        let mut layouts = Layouts::new();
        layouts.sync_saved(&[named("Day", json!({"kind":"feed","name":"a"}))]);
        let before = layouts.active().tree.id();
        layouts.active_mut().open(feed("b"), Placement::Right);
        layouts.revert();
        assert_ne!(layouts.active().tree.id(), before);
        let reverted = layouts.active().tree.id();
        layouts.sync_saved(&[named("Day", json!({"kind":"feed","name":"c"}))]);
        assert_ne!(layouts.active().tree.id(), reverted);
    }

    #[test]
    fn an_added_archive_panel_survives_newer_saves_made_elsewhere() {
        let mut layouts = Layouts::new();
        layouts.sync_saved(&[named("Day", json!({"kind":"feed","name":"a"}))]);
        layouts.active_mut().open(Target::Archive, Placement::Right);
        let day = entry(&layouts, "Day");
        assert!(day.unsaveable && !day.pending, "nothing else to save");
        layouts.sync_saved(&[named("Day", json!({"kind":"feed","name":"b"}))]);
        assert!(entry(&layouts, "Day").outdated);
        assert!(layouts.active().targets().contains(&Target::Archive));
    }

    #[test]
    fn extreme_valid_weights_become_finite_positive_shares() {
        let saved = panel(
            json!({"kind":"split","axis":"horizontal","weights":[1e-50,1.0,1e300],
            "children":[{"kind":"feed","name":"a"},{"kind":"feed","name":"b"},{"kind":"feed","name":"c"}]}),
        );
        let w = Workspace::new(egui::Id::new("w"), &saved);
        let root = w.tree.root.unwrap();
        let Some(Tile::Container(Container::Linear(linear))) = w.tree.tiles.get(root) else {
            panic!("expected a split")
        };
        for child in &linear.children {
            let share = linear.shares[*child];
            assert!(share.is_finite() && share > 0.0, "{share}");
        }
    }
}
