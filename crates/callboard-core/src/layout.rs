//! Toolkit-independent persisted canvas layouts (DESIGN.md §6.4). Card targets
//! need not currently exist.
use crate::feed::validate_feed_name;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const MAX_LAYOUT_BYTES: usize = 64 * 1024;
pub const MAX_CARDS: usize = 256;
/// Largest magnitude of any coordinate, in canvas units.
pub const MAX_COORDINATE: f64 = 1_000_000.0;
/// Largest card width or height, in canvas units.
pub const MAX_CARD_SIZE: f64 = 100_000.0;

/// The arrangement of cards on the canvas.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    /// The canvas point shown at the top-left of the canvas area.
    pub view: View,
    /// Back to front: the last card is drawn on top.
    pub cards: Vec<Card>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct View {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Card {
    pub target: Target,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    /// The expanded height, kept while the card is collapsed.
    pub height: f64,
    pub collapsed: bool,
}

/// What a card shows. The deleted-board archive is not a layout target.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Target {
    Feed { name: String },
    Board { id: i64 },
    Watch { name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedLayout {
    pub name: String,
    #[serde(flatten)]
    pub layout: Layout,
    pub updated_at_ms: i64,
}

/// `PATCH /layouts/{name}` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutRename {
    pub name: String,
}

/// Per-user preferences. `last_layout` names an existing layout or is null.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preferences {
    pub last_layout: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid layout: {0}")]
pub struct LayoutError(pub &'static str);

pub fn validate_name(name: &str) -> Result<(), LayoutError> {
    if name.trim().is_empty()
        || name != name.trim()
        || name.chars().count() > 100
        || name.chars().any(char::is_control)
    {
        return Err(LayoutError(
            "name must be 1–100 characters without surrounding whitespace or control characters",
        ));
    }
    Ok(())
}

fn coordinate(value: f64) -> bool {
    value.is_finite() && value.abs() <= MAX_COORDINATE
}

fn size(value: f64) -> bool {
    value.is_finite() && (1.0..=MAX_CARD_SIZE).contains(&value)
}

impl Layout {
    pub fn validate(&self) -> Result<(), LayoutError> {
        if !coordinate(self.view.x) || !coordinate(self.view.y) {
            return Err(LayoutError(
                "view coordinates must be finite with magnitude at most 1,000,000",
            ));
        }
        if self.cards.len() > MAX_CARDS {
            return Err(LayoutError("a layout holds at most 256 cards"));
        }
        let mut targets = BTreeSet::new();
        for card in &self.cards {
            card.target.validate()?;
            if !targets.insert(&card.target) {
                return Err(LayoutError(
                    "a layout holds one card per feed, board, or watch",
                ));
            }
            if !coordinate(card.x) || !coordinate(card.y) {
                return Err(LayoutError(
                    "card coordinates must be finite with magnitude at most 1,000,000",
                ));
            }
            if !size(card.width) || !size(card.height) {
                return Err(LayoutError(
                    "card width and height must be finite, from 1 to 100,000",
                ));
            }
        }
        Ok(())
    }
}

impl Target {
    fn validate(&self) -> Result<(), LayoutError> {
        match self {
            Self::Feed { name } => {
                validate_feed_name(name).map_err(|_| LayoutError("invalid feed target"))
            }
            Self::Watch { name } => crate::watch::validate_watch_name(name)
                .map_err(|_| LayoutError("invalid watch target")),
            Self::Board { id } if *id > 1 => Ok(()),
            Self::Board { .. } => Err(LayoutError(
                "board target must be a user board ID greater than 1",
            )),
        }
    }
}
