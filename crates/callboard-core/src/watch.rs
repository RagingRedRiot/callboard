//! Watches (DESIGN.md §4a): user-chosen items that a script reports on.
//! Validation and the read-time state of each item; persistence is in
//! `store::watches`.
use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::feed::{
    ITEM_COLORS, MAX_BODY_BYTES, MAX_DESCRIPTION_CHARS, MAX_ITEMS, MAX_META_KEYS, MAX_TAGS,
    MAX_TITLE_CHARS, MetaValue, ValidationError, invalid, validate_feed_name,
};
use crate::store::FeedError;

pub const MAX_FINGERPRINT_CHARS: usize = 256;
pub const MAX_URL_BYTES: usize = 2048;
/// A label, like a title, is at most this many characters.
pub const MAX_LABEL_CHARS: usize = MAX_TITLE_CHARS;
/// Per-item and per-watch error messages.
pub const MAX_ERROR_CHARS: usize = 1_000;
pub const DEFAULT_WAITING_AFTER: &str = "1d";
pub const DEFAULT_QUIET_AFTER: &str = "7d";
/// The window value that never ends.
pub const NEVER: &str = "never";

/// Watch names follow the feed name rules, in a namespace of their own.
pub fn validate_watch_name(name: &str) -> Result<(), ValidationError> {
    validate_feed_name(name).map_err(|_| invalid("watch name must match [a-z0-9][a-z0-9._-]{0,63}"))
}

/// A `waiting_after` or `quiet_after` value: a duration, or `never`.
pub fn validate_window(field: &str, value: &str) -> Result<(), ValidationError> {
    if value == NEVER {
        return Ok(());
    }
    humantime::parse_duration(value)
        .map(|_| ())
        .map_err(|e| invalid(format!("invalid {field}: {e} (or use never)")))
}

/// A window in milliseconds; `None` for `never`. Stored values are validated,
/// so an unparseable one also counts as `never`.
pub fn window(value: &str) -> Option<i64> {
    humantime::parse_duration(value)
        .ok()
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
}

/// The fields a script reports for an item, as stored and shown. The item's
/// URL is the user's and is not part of a report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemContent {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, MetaValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

/// One item in a report: either a report (`title` and `fingerprint`
/// required) or a failure (`error` alone).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemReport {
    pub id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, MetaValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub acknowledge: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What an item report asks for, once validated.
pub enum Reported<'a> {
    Content {
        content: ItemContent,
        fingerprint: &'a str,
        acknowledge: bool,
    },
    Error(&'a str),
}

impl ItemReport {
    fn check(&self) -> Result<(), &'static str> {
        if let Some(error) = &self.error {
            let only_error = self.title.is_none()
                && self.body.is_none()
                && self.tags.is_empty()
                && self.meta.is_empty()
                && self.color.is_none()
                && self.fingerprint.is_none()
                && !self.acknowledge;
            if !only_error {
                return Err("an error report carries only id and error");
            }
            if error.chars().count() > MAX_ERROR_CHARS {
                return Err("error exceeds 1000 characters");
            }
            return Ok(());
        }
        let Some(title) = &self.title else {
            return Err("title is required");
        };
        let Some(fingerprint) = &self.fingerprint else {
            return Err("fingerprint is required");
        };
        if title.chars().count() > MAX_TITLE_CHARS {
            Err("title exceeds 500 characters")
        } else if fingerprint.chars().count() > MAX_FINGERPRINT_CHARS {
            Err("fingerprint exceeds 256 characters")
        } else if self.body.as_ref().is_some_and(|s| s.len() > MAX_BODY_BYTES) {
            Err("body exceeds 16 KiB")
        } else if self.tags.len() > MAX_TAGS {
            Err("more than 32 tags")
        } else if self.meta.len() > MAX_META_KEYS {
            Err("more than 32 metadata keys")
        } else if self
            .color
            .as_deref()
            .is_some_and(|c| !ITEM_COLORS.contains(&c))
        {
            Err("color must be red, orange, yellow, green, blue, purple, pink, or gray")
        } else {
            Ok(())
        }
    }

    /// The validated request. Call after [`Report::validate`].
    pub fn reported(&self) -> Reported<'_> {
        match (&self.error, &self.title, &self.fingerprint) {
            (Some(error), ..) => Reported::Error(error),
            (None, Some(title), Some(fingerprint)) => Reported::Content {
                content: ItemContent {
                    title: title.clone(),
                    body: self.body.clone(),
                    tags: self.tags.clone(),
                    meta: self.meta.clone(),
                    color: self.color.clone(),
                },
                fingerprint,
                acknowledge: self.acknowledge,
            },
            _ => unreachable!("validated item report"),
        }
    }
}

/// A script's report on its watch: metadata, replacing the stored values,
/// and reports on any of its items. Not a snapshot: items left out keep
/// their last report.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Report {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_after: Option<String>,
    #[serde(default)]
    pub items: Vec<ItemReport>,
}

impl Report {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.items.len() > MAX_ITEMS {
            return Err(invalid("report exceeds 1000 items"));
        }
        if self
            .title
            .as_ref()
            .is_some_and(|s| s.chars().count() > MAX_TITLE_CHARS)
        {
            return Err(invalid("watch title exceeds 500 characters"));
        }
        if self
            .description
            .as_ref()
            .is_some_and(|s| s.chars().count() > MAX_DESCRIPTION_CHARS)
        {
            return Err(invalid("watch description exceeds 1000 characters"));
        }
        if let Some(duration) = &self.stale_after {
            humantime::parse_duration(duration)
                .map_err(|e| invalid(format!("invalid stale_after: {e}")))?;
        }
        if let Some(value) = &self.waiting_after {
            validate_window("waiting_after", value)?;
        }
        if let Some(value) = &self.quiet_after {
            validate_window("quiet_after", value)?;
        }
        let mut ids = HashSet::with_capacity(self.items.len());
        for (index, item) in self.items.iter().enumerate() {
            let checked = if ids.insert(item.id) {
                item.check()
            } else {
                Err("duplicate id")
            };
            checked.map_err(|reason| invalid(format!("item {index}: {reason}")))?;
        }
        Ok(())
    }
}

/// `POST /watches/{name}/items`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewItem {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl NewItem {
    /// The URL is compared as an exact string, after trimming surrounding
    /// whitespace; an empty label counts as none.
    pub fn normalized(&self) -> Result<(String, Option<String>), ValidationError> {
        let url = self.url.trim();
        if url.is_empty() {
            return Err(invalid("url must not be empty"));
        }
        if url.len() > MAX_URL_BYTES {
            return Err(invalid("url exceeds 2048 bytes"));
        }
        let label = self
            .label
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty());
        if label.is_some_and(|l| l.chars().count() > MAX_LABEL_CHARS) {
            return Err(invalid("label exceeds 500 characters"));
        }
        Ok((url.to_owned(), label.map(str::to_owned)))
    }
}

/// `PATCH /watches/{name}/items/{id}`: exactly one action.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemAction {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub acknowledge: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep_waiting: bool,
}

/// `POST /watches/{name}/error`: the whole watch, or one item with `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    pub message: String,
}

/// The response to a report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportSummary {
    /// The report created the watch.
    pub created: bool,
    /// Items reported on, failures included.
    pub reported: usize,
    /// Items that need attention because of this report, in report order.
    pub attention_ids: Vec<i64>,
    /// Items that became quiet since the previous report, each listed once.
    pub quiet_ids: Vec<i64>,
    /// Reported IDs that are not on the watch, such as removed items.
    pub ignored_ids: Vec<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemState {
    /// Display order: attention first, then quiet, new, and waiting.
    Attention,
    Quiet,
    New,
    Waiting,
}

/// The times an item's state is computed from (§4a.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clocks {
    pub added_at_ms: i64,
    /// Set while the item needs attention: when that started.
    pub attention_since_ms: Option<i64>,
    /// When the item last entered Waiting by an acknowledgement or Keep
    /// waiting. Unset until then, while the `waiting_after` window decides.
    pub waiting_since_ms: Option<i64>,
}

/// An item's state at a moment, with when it entered that state and when it
/// next changes by the passage of time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateAt {
    pub state: ItemState,
    pub since_ms: i64,
    /// When the item entered Waiting, which starts its waiting clock; unset
    /// while it is new or needs attention.
    pub waiting_since_ms: Option<i64>,
    pub next_change_ms: Option<i64>,
}

impl Clocks {
    /// The state at `now_ms`, given the watch's windows (`None` = never).
    pub fn state_at(
        &self,
        waiting_after: Option<i64>,
        quiet_after: Option<i64>,
        now_ms: i64,
    ) -> StateAt {
        if let Some(since) = self.attention_since_ms {
            return StateAt {
                state: ItemState::Attention,
                since_ms: since,
                waiting_since_ms: None,
                next_change_ms: None,
            };
        }
        let waiting_since = match self.waiting_since_ms {
            Some(since) => since,
            None => match waiting_after {
                // `never`: new until the first change.
                None => {
                    return StateAt {
                        state: ItemState::New,
                        since_ms: self.added_at_ms,
                        waiting_since_ms: None,
                        next_change_ms: None,
                    };
                }
                Some(window) => self.added_at_ms.saturating_add(window),
            },
        };
        if now_ms < waiting_since {
            return StateAt {
                state: ItemState::New,
                since_ms: self.added_at_ms,
                waiting_since_ms: None,
                next_change_ms: Some(waiting_since),
            };
        }
        let quiet_since = quiet_after.map(|window| waiting_since.saturating_add(window));
        match quiet_since {
            Some(quiet) if now_ms >= quiet => StateAt {
                state: ItemState::Quiet,
                since_ms: quiet,
                waiting_since_ms: Some(waiting_since),
                next_change_ms: None,
            },
            _ => StateAt {
                state: ItemState::Waiting,
                since_ms: waiting_since,
                waiting_since_ms: Some(waiting_since),
                next_change_ms: quiet_since,
            },
        }
    }
}

/// Queue order (§4a.6): by state, then oldest in that state first, then by ID.
pub fn order_key(state: &StateAt, id: i64) -> (ItemState, i64, i64) {
    (state.state, state.since_ms, id)
}

/// A watch's metadata, as its script last reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchInfo {
    pub name: String,
    pub title: String,
    pub description: Option<String>,
    pub source_url: Option<String>,
    pub stale_after: Option<String>,
    pub waiting_after: String,
    pub quiet_after: String,
    /// UTC Unix milliseconds, refreshed by every report.
    pub last_reported_at_ms: i64,
    pub error: Option<FeedError>,
}

/// One watch item: the user's URL and label, the script's last report, and
/// its state at read time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchItem {
    pub id: i64,
    pub url: String,
    pub label: Option<String>,
    pub added_at_ms: i64,
    /// The last good report; unset until the first one.
    pub content: Option<ItemContent>,
    pub fingerprint: Option<String>,
    pub reported_at_ms: Option<i64>,
    /// The last fingerprint change.
    pub changed_at_ms: Option<i64>,
    pub error: Option<FeedError>,
    pub attention_since_ms: Option<i64>,
    pub state: ItemState,
    /// When the item entered its state.
    pub state_since_ms: i64,
    pub waiting_since_ms: Option<i64>,
}

/// `GET /watches/{name}`: items in queue order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Watch {
    pub info: WatchInfo,
    pub items: Vec<WatchItem>,
    /// When an item next changes state by the passage of time; refetch then.
    pub next_wake_at_ms: Option<i64>,
}

/// A `GET /watches` entry: metadata plus counts evaluated at read time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchSummary {
    #[serde(flatten)]
    pub info: WatchInfo,
    pub item_count: usize,
    pub attention_count: usize,
    pub quiet_count: usize,
    pub new_count: usize,
    pub next_wake_at_ms: Option<i64>,
}

/// `POST /watches/{name}/items`: the item, and whether it was added (false
/// when the URL was already on the watch).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AddedItem {
    pub created: bool,
    pub item: WatchItem,
}
