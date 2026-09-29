//! Toolkit-independent persisted panel trees. Targets need not currently exist.
use crate::feed::validate_feed_name;
use serde::{Deserialize, Serialize};

pub const MAX_LAYOUT_BYTES: usize = 64 * 1024;
pub const MAX_LAYOUT_NODES: usize = 256;
pub const MAX_LAYOUT_DEPTH: usize = 16;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    pub tree: Panel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Panel {
    Empty {},
    Feed {
        name: String,
    },
    Board {
        id: i64,
    },
    Split {
        axis: Axis,
        children: Vec<Panel>,
        weights: Vec<f64>,
    },
    Tabs {
        children: Vec<Panel>,
        active: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedLayout {
    pub name: String,
    pub tree: Panel,
    pub updated_at_ms: i64,
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

impl Layout {
    pub fn validate(&self) -> Result<(), LayoutError> {
        let mut count = 0;
        self.tree.validate(1, &mut count)
    }
}

impl Panel {
    fn validate(&self, depth: usize, count: &mut usize) -> Result<(), LayoutError> {
        *count += 1;
        if depth > MAX_LAYOUT_DEPTH || *count > MAX_LAYOUT_NODES {
            return Err(LayoutError("tree exceeds 16 levels or 256 nodes"));
        }
        let children = match self {
            Self::Empty {} if depth == 1 => return Ok(()),
            Self::Empty {} => return Err(LayoutError("empty is only valid as the root")),
            Self::Feed { name } => {
                return validate_feed_name(name).map_err(|_| LayoutError("invalid feed target"));
            }
            Self::Board { id } if *id > 1 => return Ok(()),
            Self::Board { .. } => {
                return Err(LayoutError(
                    "board target must be a user board ID greater than 1",
                ));
            }
            Self::Split {
                children, weights, ..
            } => {
                if children.len() < 2
                    || children.len() != weights.len()
                    || weights.iter().any(|v| !v.is_finite() || *v <= 0.0)
                    || !weights.iter().sum::<f64>().is_finite()
                {
                    return Err(LayoutError(
                        "split requires at least two children and one finite positive weight per child",
                    ));
                }
                children
            }
            Self::Tabs { children, active } => {
                if children.is_empty() || *active >= children.len() {
                    return Err(LayoutError(
                        "tabs require children and an in-range active index",
                    ));
                }
                children
            }
        };
        for child in children {
            child.validate(depth + 1, count)?;
        }
        Ok(())
    }
}
