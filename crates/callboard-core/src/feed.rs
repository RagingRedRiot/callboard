use std::collections::{BTreeMap, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};

pub const MAX_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_ITEMS: usize = 1_000;
pub const MAX_TITLE_CHARS: usize = 500;
pub const MAX_DESCRIPTION_CHARS: usize = 1_000;
pub const MAX_BODY_BYTES: usize = 16 * 1024;
pub const MAX_TAGS: usize = 32;
pub const MAX_META_KEYS: usize = 32;

/// Item colors (DESIGN.md §3.2); the GUI picks a shade per theme.
pub const ITEM_COLORS: [&str; 8] = [
    "red", "orange", "yellow", "green", "blue", "purple", "pink", "gray",
];

/// A scalar value: nested objects, arrays, and null are not accepted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MetaValue {
    String(String),
    Number(serde_json::Number),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub key: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, MetaValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

/// Replacement metadata and ordered items, before persistence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_for: Option<String>,
    // Deliberately required: an omitted items field must never clear a feed.
    pub items: Vec<Item>,
}

/// Item changes only. Metadata, ordering, and submission time are not updates.
/// Added/updated keys follow the new snapshot; removed keys follow the old one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSummary {
    pub added: usize,
    pub removed: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub added_keys: Vec<String>,
    pub removed_keys: Vec<String>,
    pub updated_keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError(String);

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ValidationError {}

pub(crate) fn invalid(message: impl Into<String>) -> ValidationError {
    ValidationError(message.into())
}

/// Feed identity is ASCII and matches `[a-z0-9][a-z0-9._-]{0,63}`.
/// A color must be one of [`ITEM_COLORS`].
pub fn validate_color(color: &str) -> Result<(), ValidationError> {
    if ITEM_COLORS.contains(&color) {
        Ok(())
    } else {
        Err(invalid(
            "color must be red, orange, yellow, green, blue, purple, pink, or gray",
        ))
    }
}

pub fn validate_feed_name(name: &str) -> Result<(), ValidationError> {
    let is_alphanumeric = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if name.is_empty()
        || name.len() > 64
        || !is_alphanumeric(name.as_bytes()[0])
        || !name
            .bytes()
            .all(|b| is_alphanumeric(b) || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(invalid("feed name must match [a-z0-9][a-z0-9._-]{0,63}"));
    }
    Ok(())
}

impl Snapshot {
    /// Validate a model constructed in memory. Transport byte limits must also
    /// be enforced by the caller; `parse_submission` does this for CLI input.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.items.len() > MAX_ITEMS {
            return Err(invalid("snapshot exceeds 1000 items"));
        }
        if self
            .title
            .as_ref()
            .is_some_and(|s| s.chars().count() > MAX_TITLE_CHARS)
        {
            return Err(invalid("feed title exceeds 500 characters"));
        }
        if self
            .description
            .as_ref()
            .is_some_and(|s| s.chars().count() > MAX_DESCRIPTION_CHARS)
        {
            return Err(invalid("feed description exceeds 1000 characters"));
        }
        if let Some(duration) = &self.stale_after {
            humantime::parse_duration(duration)
                .map_err(|e| invalid(format!("invalid stale_after: {e}")))?;
        }
        if let Some(duration) = &self.new_for {
            humantime::parse_duration(duration)
                .map_err(|e| invalid(format!("invalid new_for: {e}")))?;
        }
        let mut keys = HashSet::with_capacity(self.items.len());
        for (index, item) in self.items.iter().enumerate() {
            let reason = if !keys.insert(&item.key) {
                Some("duplicate key")
            } else if item.title.chars().count() > MAX_TITLE_CHARS {
                Some("title exceeds 500 characters")
            } else if item.body.as_ref().is_some_and(|s| s.len() > MAX_BODY_BYTES) {
                Some("body exceeds 16 KiB")
            } else if item.tags.len() > MAX_TAGS {
                Some("more than 32 tags")
            } else if item.meta.len() > MAX_META_KEYS {
                Some("more than 32 metadata keys")
            } else if item
                .color
                .as_deref()
                .is_some_and(|c| !ITEM_COLORS.contains(&c))
            {
                Some("color must be red, orange, yellow, green, blue, purple, pink, or gray")
            } else {
                None
            };
            if let Some(reason) = reason {
                return Err(invalid(format!("item {index}: {reason}")));
            }
        }
        Ok(())
    }
}

/// Parse the CLI's JSON array or full snapshot object. Empty input is an error;
/// clearing a feed requires explicitly supplying an empty items array.
/// Stream readers should cap reads at MAX_SNAPSHOT_BYTES + 1 before calling.
pub fn parse_submission(input: &[u8]) -> Result<Snapshot, ValidationError> {
    if input.len() > MAX_SNAPSHOT_BYTES {
        return Err(invalid("snapshot exceeds 4 MiB"));
    }
    let first = input.iter().find(|b| !b.is_ascii_whitespace());
    let snapshot: Snapshot = match first {
        None => return Err(invalid("empty input; use [] to explicitly clear a feed")),
        Some(b'[') => Snapshot {
            title: None,
            description: None,
            source_url: None,
            stale_after: None,
            new_for: None,
            items: serde_json::from_slice(input)
                .map_err(|e| invalid(format!("invalid items JSON: {e}")))?,
        },
        _ => serde_json::from_slice(input)
            .map_err(|e| invalid(format!("invalid snapshot JSON: {e}")))?,
    };
    snapshot.validate()?;
    Ok(snapshot)
}
