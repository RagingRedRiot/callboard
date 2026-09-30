//! SQLx-backed feed persistence. The caller owns path selection, private directory
//! creation, ownership checks, and the service lock. No network or GUI is involved.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{FromRow, SqlitePool};

use crate::feed::{
    ChangeSummary, Item, MAX_BODY_BYTES, MAX_SNAPSHOT_BYTES, MAX_TITLE_CHARS, Snapshot,
    ValidationError, validate_feed_name,
};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Layout(#[from] crate::layout::LayoutError),
    #[error("layout exceeds 64 KiB when serialized")]
    LayoutTooLarge,
    #[error("layout not found: {0}")]
    LayoutNotFound(String),
    #[error("layout name is already in use: {0}")]
    LayoutNameTaken(String),
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error("snapshot exceeds 4 MiB when serialized")]
    SnapshotTooLarge,
    #[error("feed not found: {0}")]
    FeedNotFound(String),
    #[error("feed item not found: {feed}/{key}")]
    FeedItemNotFound { feed: String, key: String },
    #[error("board not found: {0}")]
    BoardNotFound(i64),
    #[error("board is not empty: {0}")]
    BoardNotEmpty(i64),
    #[error("{kind} not found: {id}")]
    BoardItemNotFound { kind: &'static str, id: i64 },
    #[error("board name must not be empty")]
    EmptyBoardName,
    #[error("board name is already in use")]
    BoardNameTaken,
    #[error("an archive restore must name a destination board")]
    RestoreBoardRequired,
    #[error("invalid item position: {0}")]
    InvalidPosition(i64),
    #[error("invalid patch: {0}")]
    InvalidPatch(&'static str),
    #[error("{0}")]
    ContentLimit(&'static str),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error("item encoding error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("system time cannot be represented as Unix milliseconds")]
    InvalidSystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedError {
    pub message: String,
    pub at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedInfo {
    pub name: String,
    pub title: String,
    pub source_url: Option<String>,
    pub stale_after: Option<String>,
    /// UTC Unix milliseconds, refreshed by every accepted snapshot.
    pub last_submitted_at_ms: i64,
    pub error: Option<FeedError>,
}

/// A `GET /feeds` entry: metadata plus counts evaluated at read time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedSummary {
    #[serde(flatten)]
    pub info: FeedInfo,
    pub item_count: usize,
    pub snoozed_count: usize,
    /// Earliest future time-snooze deadline; counts change then without a notice.
    pub next_wake_at_ms: Option<i64>,
}

/// A `GET /boards` entry with active (non-archived) item counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardSummary {
    #[serde(flatten)]
    pub info: BoardInfo,
    pub todo_count: usize,
    pub open_todo_count: usize,
    pub note_count: usize,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreferencesPatch {
    #[serde(default)]
    pub last_layout: PatchValue<Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Feed {
    pub info: FeedInfo,
    /// Tool order unless `manual_order` is true.
    pub items: Vec<Item>,
    pub manual_order: bool,
    pub view_state: std::collections::BTreeMap<String, ItemViewState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemViewState {
    pub snoozed_until_ms: Option<i64>,
    pub wake_on_update: bool,
    pub snoozed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedItemUpdate {
    pub state: ItemViewState,
    pub position: i64,
    pub manual_order: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedItemPatch {
    #[serde(default)]
    pub snoozed_until_ms: PatchValue<Option<i64>>,
    #[serde(default, deserialize_with = "present")]
    pub wake_on_update: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    pub position: Option<i64>,
    #[serde(default)]
    pub reset_order: bool,
}

fn present<'de, T: Deserialize<'de>, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BoardContents {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub board: Option<BoardInfo>,
    pub todos: Vec<BoardItem<Todo>>,
    pub notes: Vec<BoardItem<Note>>,
}

/// Display-only data, resolved in the same snapshot as the board contents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResolvedReference {
    Live { item: Item },
    SourceGone,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BoardItem<T> {
    #[serde(flatten)]
    pub item: T,
    pub resolved_reference: Option<ResolvedReference>,
}

fn resolve_reference(
    reference: Option<&SourceReference>,
    sources: &HashMap<(String, String), Item>,
) -> Option<ResolvedReference> {
    reference.map(
        |reference| match sources.get(&(reference.feed.clone(), reference.key.clone())) {
            Some(item) => ResolvedReference::Live { item: item.clone() },
            None => ResolvedReference::SourceGone,
        },
    )
}

/// A reference to a submitted feed item. It intentionally has no foreign key:
/// promoted objects remain usable when their source feed item disappears.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceReference {
    pub feed: String,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct BoardInfo {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub id: i64,
    pub board_id: i64,
    pub title: String,
    pub body: Option<String>,
    pub url: Option<String>,
    pub done: bool,
    pub reference: Option<SourceReference>,
    pub position: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub archived_at_ms: Option<i64>,
    pub archived_from_board: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub id: i64,
    pub board_id: i64,
    pub title: Option<String>,
    pub body: String,
    pub url: Option<String>,
    pub color: Option<String>,
    pub reference: Option<SourceReference>,
    pub position: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub archived_at_ms: Option<i64>,
    pub archived_from_board: Option<String>,
}

/// A field present in a PATCH body. `PatchValue<Option<T>>` distinguishes an
/// omitted field (leave unchanged) from JSON null (clear the optional value).
#[derive(Debug, Clone, Default)]
pub struct PatchValue<T>(pub Option<T>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for PatchValue<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(|value| Self(Some(value)))
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TodoPatch {
    #[serde(default, deserialize_with = "present")]
    pub title: Option<String>,
    #[serde(default)]
    pub body: PatchValue<Option<String>>,
    #[serde(default)]
    pub url: PatchValue<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    pub done: Option<bool>,
    #[serde(default)]
    pub reference: PatchValue<Option<SourceReference>>,
    /// Zero-based position in the active todo list for this board.
    #[serde(default, deserialize_with = "present")]
    pub position: Option<i64>,
    #[serde(default, deserialize_with = "present")]
    pub board_id: Option<i64>,
    #[serde(default, deserialize_with = "present")]
    pub archived: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotePatch {
    #[serde(default)]
    pub title: PatchValue<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    pub body: Option<String>,
    #[serde(default)]
    pub url: PatchValue<Option<String>>,
    #[serde(default)]
    pub color: PatchValue<Option<String>>,
    #[serde(default)]
    pub reference: PatchValue<Option<SourceReference>>,
    /// Zero-based position in the active note list for this board.
    #[serde(default, deserialize_with = "present")]
    pub position: Option<i64>,
    #[serde(default, deserialize_with = "present")]
    pub board_id: Option<i64>,
    #[serde(default, deserialize_with = "present")]
    pub archived: Option<bool>,
}

#[derive(FromRow)]
struct TodoRow {
    id: i64,
    board_id: i64,
    title: String,
    body: Option<String>,
    url: Option<String>,
    done: i64,
    reference_feed: Option<String>,
    reference_key: Option<String>,
    position: i64,
    created_at_ms: i64,
    updated_at_ms: i64,
    archived_at_ms: Option<i64>,
    archived_from_board: Option<String>,
}

impl From<TodoRow> for Todo {
    fn from(r: TodoRow) -> Self {
        Self {
            id: r.id,
            board_id: r.board_id,
            title: r.title,
            body: r.body,
            url: r.url,
            done: r.done != 0,
            reference: r
                .reference_feed
                .zip(r.reference_key)
                .map(|(feed, key)| SourceReference { feed, key }),
            position: r.position,
            created_at_ms: r.created_at_ms,
            updated_at_ms: r.updated_at_ms,
            archived_at_ms: r.archived_at_ms,
            archived_from_board: r.archived_from_board,
        }
    }
}

#[derive(FromRow)]
struct NoteRow {
    id: i64,
    board_id: i64,
    title: Option<String>,
    body: String,
    url: Option<String>,
    color: Option<String>,
    reference_feed: Option<String>,
    reference_key: Option<String>,
    position: i64,
    created_at_ms: i64,
    updated_at_ms: i64,
    archived_at_ms: Option<i64>,
    archived_from_board: Option<String>,
}

impl From<NoteRow> for Note {
    fn from(r: NoteRow) -> Self {
        Self {
            id: r.id,
            board_id: r.board_id,
            title: r.title,
            body: r.body,
            url: r.url,
            color: r.color,
            reference: r
                .reference_feed
                .zip(r.reference_key)
                .map(|(feed, key)| SourceReference { feed, key }),
            position: r.position,
            created_at_ms: r.created_at_ms,
            updated_at_ms: r.updated_at_ms,
            archived_at_ms: r.archived_at_ms,
            archived_from_board: r.archived_from_board,
        }
    }
}

const TODO_COLUMNS: &str = "id, board_id, title, body, url, done, reference_feed, reference_key, position, created_at_ms, updated_at_ms, archived_at_ms, archived_from_board";
const NOTE_COLUMNS: &str = "id, board_id, title, body, color, reference_feed, reference_key, position, created_at_ms, updated_at_ms, archived_at_ms, archived_from_board, url";

#[derive(FromRow)]
struct FeedRow {
    name: String,
    title: String,
    source_url: Option<String>,
    stale_after: Option<String>,
    last_submitted_at_ms: i64,
    error_message: Option<String>,
    error_at_ms: Option<i64>,
    manual_order_enabled: i64,
}

impl From<FeedRow> for FeedInfo {
    fn from(row: FeedRow) -> Self {
        Self {
            name: row.name,
            title: row.title,
            source_url: row.source_url,
            stale_after: row.stale_after,
            last_submitted_at_ms: row.last_submitted_at_ms,
            error: row
                .error_message
                .zip(row.error_at_ms)
                .map(|(message, at_ms)| FeedError { message, at_ms }),
        }
    }
}

#[derive(FromRow)]
struct PreviousItem {
    key: String,
    content_hash: Vec<u8>,
}

/// Ephemeral invalidations, emitted only after successful writes. Subscribers
/// must refetch on startup or lag. Independently opened stores do not share a bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "resource", rename_all = "snake_case")]
pub enum Change {
    Feed { name: String },
    Board { id: i64 },
    Layout { name: String },
}

pub const CHANGE_CAPACITY: usize = 128;

/// Clones share a connection pool. SQLite serializes write transactions even
/// across independently opened stores; reads use consistent transactions.
#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
    changes: tokio::sync::broadcast::Sender<Change>,
}

impl Store {
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Change> {
        self.changes.subscribe()
    }

    fn notify(&self, change: Change) {
        // No listeners is normal, and slow listeners never block writes.
        let _ = self.changes.send(change);
    }

    pub async fn save_layout(
        &self,
        name: &str,
        layout: &crate::layout::Layout,
    ) -> Result<crate::layout::NamedLayout, StoreError> {
        crate::layout::validate_name(name)?;
        layout.validate()?;
        if serde_json::to_vec(layout)?.len() > crate::layout::MAX_LAYOUT_BYTES {
            return Err(StoreError::LayoutTooLarge);
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let updated_at_ms = now_ms()?;
        sqlx::query("INSERT INTO layouts(name, layout_json, updated_at_ms) VALUES(?, ?, ?) ON CONFLICT(name) DO UPDATE SET layout_json = excluded.layout_json, updated_at_ms = excluded.updated_at_ms")
            .bind(name).bind(serde_json::to_string(layout)?).bind(updated_at_ms).execute(&mut *tx).await?;
        tx.commit().await?;
        self.notify(Change::Layout {
            name: name.to_owned(),
        });
        Ok(crate::layout::NamedLayout {
            name: name.to_owned(),
            layout: layout.clone(),
            updated_at_ms,
        })
    }

    pub async fn list_layouts(&self) -> Result<Vec<crate::layout::NamedLayout>, StoreError> {
        let rows = sqlx::query_as::<_, (String, String, i64)>(
            "SELECT name, layout_json, updated_at_ms FROM layouts ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(name, json, updated_at_ms)| {
                Ok(crate::layout::NamedLayout {
                    name,
                    layout: serde_json::from_str(&json)?,
                    updated_at_ms,
                })
            })
            .collect()
    }

    /// Returns false if the layout was already absent. A preference naming it
    /// is cleared by the foreign key.
    pub async fn delete_layout(&self, name: &str) -> Result<bool, StoreError> {
        crate::layout::validate_name(name)?;
        let deleted = sqlx::query("DELETE FROM layouts WHERE name = ?")
            .bind(name)
            .execute(&self.pool)
            .await?
            .rows_affected()
            != 0;
        if deleted {
            self.notify(Change::Layout {
                name: name.to_owned(),
            });
        }
        Ok(deleted)
    }

    /// Rename a layout without replacing another. A preference naming it follows
    /// the rename through the foreign key. Renaming to itself is a no-op.
    pub async fn rename_layout(
        &self,
        name: &str,
        new_name: &str,
    ) -> Result<crate::layout::NamedLayout, StoreError> {
        crate::layout::validate_name(name)?;
        crate::layout::validate_name(new_name)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query_as::<_, (String, i64)>(
            "SELECT layout_json, updated_at_ms FROM layouts WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| StoreError::LayoutNotFound(name.to_owned()))?;
        if name == new_name {
            tx.commit().await?;
            return Ok(crate::layout::NamedLayout {
                name: name.to_owned(),
                layout: serde_json::from_str(&row.0)?,
                updated_at_ms: row.1,
            });
        }
        let taken = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM layouts WHERE name = ?")
            .bind(new_name)
            .fetch_one(&mut *tx)
            .await?
            != 0;
        if taken {
            return Err(StoreError::LayoutNameTaken(new_name.to_owned()));
        }
        let updated_at_ms = now_ms()?;
        sqlx::query("UPDATE layouts SET name = ?, updated_at_ms = ? WHERE name = ?")
            .bind(new_name)
            .bind(updated_at_ms)
            .bind(name)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        for name in [name, new_name] {
            self.notify(Change::Layout {
                name: name.to_owned(),
            });
        }
        Ok(crate::layout::NamedLayout {
            name: new_name.to_owned(),
            layout: serde_json::from_str(&row.0)?,
            updated_at_ms,
        })
    }

    pub async fn preferences(&self) -> Result<crate::layout::Preferences, StoreError> {
        let last_layout =
            sqlx::query_scalar::<_, Option<String>>("SELECT last_layout FROM preferences")
                .fetch_one(&self.pool)
                .await?;
        Ok(crate::layout::Preferences { last_layout })
    }

    /// Apply the fields present in `patch`. `last_layout` must name an existing
    /// layout. Preferences emit no change notice.
    pub async fn patch_preferences(
        &self,
        patch: PreferencesPatch,
    ) -> Result<crate::layout::Preferences, StoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let PatchValue(Some(last_layout)) = &patch.last_layout {
            if let Some(name) = last_layout {
                crate::layout::validate_name(name)?;
                let exists =
                    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM layouts WHERE name = ?")
                        .bind(name)
                        .fetch_one(&mut *tx)
                        .await?
                        != 0;
                if !exists {
                    return Err(StoreError::LayoutNotFound(name.clone()));
                }
            }
            sqlx::query("UPDATE preferences SET last_layout = ?")
                .bind(last_layout)
                .execute(&mut *tx)
                .await?;
        }
        let last_layout =
            sqlx::query_scalar::<_, Option<String>>("SELECT last_layout FROM preferences")
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(crate::layout::Preferences { last_layout })
    }

    /// Open/create a database and apply embedded migrations. Requires a Tokio
    /// runtime and an existing parent directory secured by the caller.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        if let Err(error) = MIGRATOR.run(&pool).await {
            pool.close().await;
            return Err(error.into());
        }
        Ok(Self {
            pool,
            changes: tokio::sync::broadcast::channel(CHANGE_CAPACITY).0,
        })
    }

    /// Close all connections, including those shared with clones.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// Replace a feed atomically. Validation and encoding finish before writing;
    /// any database error rolls back metadata and every item change together.
    pub async fn submit(
        &self,
        name: &str,
        snapshot: &Snapshot,
    ) -> Result<ChangeSummary, StoreError> {
        validate_feed_name(name)?;
        snapshot.validate()?;
        if serde_json::to_vec(snapshot)?.len() > MAX_SNAPSHOT_BYTES {
            return Err(StoreError::SnapshotTooLarge);
        }
        // Typed serialization fixes field order, sorts metadata via BTreeMap,
        // and normalizes omitted defaults. Position is deliberately excluded.
        let contents = snapshot
            .items
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?;
        let hashes: Vec<_> = contents
            .iter()
            .map(|content| Sha256::digest(content.as_bytes()).to_vec())
            .collect();

        // Reserve the writer before reading the previous snapshot, avoiding a
        // stale read followed by a competing write (SQLITE_BUSY_SNAPSHOT).
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let previous = sqlx::query_as::<_, PreviousItem>(
            "SELECT key, content_hash FROM feed_items WHERE feed = ? ORDER BY position, key",
        )
        .bind(name)
        .fetch_all(&mut *tx)
        .await?;
        let manual_order_enabled =
            sqlx::query_scalar::<_, i64>("SELECT manual_order_enabled FROM feeds WHERE name = ?")
                .bind(name)
                .fetch_optional(&mut *tx)
                .await?
                .unwrap_or(0)
                != 0;
        let old_manual_order = sqlx::query_scalar::<_, String>(
            "SELECT key FROM feed_manual_order WHERE feed = ? ORDER BY position",
        )
        .bind(name)
        .fetch_all(&mut *tx)
        .await?;
        let old: HashMap<_, _> = previous
            .iter()
            .map(|item| (item.key.as_str(), item.content_hash.as_slice()))
            .collect();
        let new_keys: HashSet<_> = snapshot
            .items
            .iter()
            .map(|item| item.key.as_str())
            .collect();
        let mut summary = ChangeSummary::default();
        for (item, hash) in snapshot.items.iter().zip(&hashes) {
            match old.get(item.key.as_str()) {
                None => summary.added_keys.push(item.key.clone()),
                Some(previous_hash) if *previous_hash != hash.as_slice() => {
                    summary.updated_keys.push(item.key.clone())
                }
                Some(_) => summary.unchanged += 1,
            }
        }
        summary.removed_keys = previous
            .iter()
            .filter(|item| !new_keys.contains(item.key.as_str()))
            .map(|item| item.key.clone())
            .collect();
        summary.added = summary.added_keys.len();
        summary.updated = summary.updated_keys.len();
        summary.removed = summary.removed_keys.len();

        sqlx::query(
            "INSERT INTO feeds (name, title, source_url, stale_after, last_submitted_at_ms)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(name) DO UPDATE SET
               title = excluded.title, source_url = excluded.source_url,
               stale_after = excluded.stale_after,
               last_submitted_at_ms = excluded.last_submitted_at_ms,
               error_message = NULL, error_at_ms = NULL",
        )
        .bind(name)
        .bind(snapshot.title.as_deref().unwrap_or(name))
        .bind(&snapshot.source_url)
        .bind(&snapshot.stale_after)
        .bind(now_ms()?)
        .execute(&mut *tx)
        .await?;

        for key in &summary.removed_keys {
            sqlx::query("DELETE FROM feed_items WHERE feed = ? AND key = ?")
                .bind(name)
                .bind(key)
                .execute(&mut *tx)
                .await?;
        }
        for key in &summary.updated_keys {
            sqlx::query(
                "DELETE FROM feed_item_view_state WHERE feed = ? AND key = ? AND wake_on_update = 1",
            )
            .bind(name)
            .bind(key)
            .execute(&mut *tx)
            .await?;
        }
        for (position, ((item, content), hash)) in snapshot
            .items
            .iter()
            .zip(&contents)
            .zip(&hashes)
            .enumerate()
        {
            // UPDATE, not REPLACE: retained keys keep their row identity for
            // future view-state foreign keys. Only removed keys are deleted.
            sqlx::query(
                "INSERT INTO feed_items (feed, key, position, content_json, content_hash)
                 VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(feed, key) DO UPDATE SET
                   position = excluded.position, content_json = excluded.content_json,
                   content_hash = excluded.content_hash
                 WHERE feed_items.position != excluded.position
                    OR feed_items.content_hash != excluded.content_hash",
            )
            .bind(name)
            .bind(&item.key)
            .bind(position as i64)
            .bind(content)
            .bind(hash)
            .execute(&mut *tx)
            .await?;
        }
        if manual_order_enabled {
            let new_keys: HashSet<_> = summary.added_keys.iter().map(String::as_str).collect();
            let current_keys: HashSet<_> = snapshot
                .items
                .iter()
                .map(|item| item.key.as_str())
                .collect();
            let mut order = summary.added_keys.clone();
            order.extend(
                old_manual_order
                    .into_iter()
                    .filter(|key| current_keys.contains(key.as_str())),
            );
            for item in &snapshot.items {
                if !new_keys.contains(item.key.as_str())
                    && !order.iter().any(|key| key == &item.key)
                {
                    order.push(item.key.clone());
                }
            }
            sqlx::query("DELETE FROM feed_manual_order WHERE feed = ?")
                .bind(name)
                .execute(&mut *tx)
                .await?;
            for (position, key) in order.iter().enumerate() {
                sqlx::query("INSERT INTO feed_manual_order (feed, key, position) VALUES (?, ?, ?)")
                    .bind(name)
                    .bind(key)
                    .bind(position as i64)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        self.notify(Change::Feed {
            name: name.to_owned(),
        });
        Ok(summary)
    }

    pub async fn patch_feed_item(
        &self,
        feed: &str,
        key: &str,
        patch: FeedItemPatch,
    ) -> Result<FeedItemUpdate, StoreError> {
        validate_feed_name(feed)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let submitted_position = sqlx::query_scalar::<_, i64>(
            "SELECT position FROM feed_items WHERE feed = ? AND key = ?",
        )
        .bind(feed)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| StoreError::FeedItemNotFound {
            feed: feed.to_owned(),
            key: key.to_owned(),
        })?;

        let current = sqlx::query_as::<_, (Option<i64>, i64)>(
            "SELECT snoozed_until_ms, wake_on_update FROM feed_item_view_state WHERE feed = ? AND key = ?",
        )
        .bind(feed)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or((None, 0));
        if patch.reset_order && patch.position.is_some() {
            return Err(StoreError::InvalidPatch(
                "position and reset_order are mutually exclusive",
            ));
        }
        let current = ItemViewState::effective(current.0, current.1 != 0, now_ms()?);
        let snoozed_until_ms = patch.snoozed_until_ms.0.unwrap_or(current.snoozed_until_ms);
        let wake_on_update = patch.wake_on_update.unwrap_or(current.wake_on_update);
        if patch.snoozed_until_ms.0.is_some() || patch.wake_on_update.is_some() {
            if snoozed_until_ms.is_none() && !wake_on_update {
                sqlx::query("DELETE FROM feed_item_view_state WHERE feed = ? AND key = ?")
                    .bind(feed)
                    .bind(key)
                    .execute(&mut *tx)
                    .await?;
            } else {
                sqlx::query("INSERT INTO feed_item_view_state (feed, key, snoozed_until_ms, wake_on_update) VALUES (?, ?, ?, ?) ON CONFLICT(feed, key) DO UPDATE SET snoozed_until_ms = excluded.snoozed_until_ms, wake_on_update = excluded.wake_on_update")
                    .bind(feed).bind(key).bind(snoozed_until_ms).bind(wake_on_update)
                    .execute(&mut *tx).await?;
            }
        }

        let mut manual_order =
            sqlx::query_scalar::<_, i64>("SELECT manual_order_enabled FROM feeds WHERE name = ?")
                .bind(feed)
                .fetch_one(&mut *tx)
                .await?
                != 0;
        let mut position = submitted_position;
        if patch.reset_order {
            sqlx::query("DELETE FROM feed_manual_order WHERE feed = ?")
                .bind(feed)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE feeds SET manual_order_enabled = 0 WHERE name = ?")
                .bind(feed)
                .execute(&mut *tx)
                .await?;
            manual_order = false;
        } else if let Some(target) = patch.position {
            let mut keys = sqlx::query_scalar::<_, String>(
                "SELECT key FROM feed_manual_order WHERE feed = ? ORDER BY position",
            )
            .bind(feed)
            .fetch_all(&mut *tx)
            .await?;
            if keys.is_empty() {
                keys = sqlx::query_scalar::<_, String>(
                    "SELECT key FROM feed_items WHERE feed = ? ORDER BY position, key",
                )
                .bind(feed)
                .fetch_all(&mut *tx)
                .await?;
            }
            if target < 0 || target >= keys.len() as i64 {
                return Err(StoreError::InvalidPosition(target));
            }
            let old_position = keys
                .iter()
                .position(|existing| existing == key)
                .ok_or_else(|| StoreError::FeedItemNotFound {
                    feed: feed.to_owned(),
                    key: key.to_owned(),
                })?;
            let item = keys.remove(old_position);
            keys.insert(target as usize, item);
            sqlx::query("DELETE FROM feed_manual_order WHERE feed = ?")
                .bind(feed)
                .execute(&mut *tx)
                .await?;
            for (index, ordered_key) in keys.iter().enumerate() {
                sqlx::query("INSERT INTO feed_manual_order (feed, key, position) VALUES (?, ?, ?)")
                    .bind(feed)
                    .bind(ordered_key)
                    .bind(index as i64)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query("UPDATE feeds SET manual_order_enabled = 1 WHERE name = ?")
                .bind(feed)
                .execute(&mut *tx)
                .await?;
            position = target;
            manual_order = true;
        }
        if manual_order {
            position = sqlx::query_scalar(
                "SELECT position FROM feed_manual_order WHERE feed = ? AND key = ?",
            )
            .bind(feed)
            .bind(key)
            .fetch_one(&mut *tx)
            .await?;
        }
        let state = ItemViewState::effective(snoozed_until_ms, wake_on_update, now_ms()?);
        tx.commit().await?;
        self.notify(Change::Feed {
            name: feed.to_owned(),
        });
        Ok(FeedItemUpdate {
            state,
            position,
            manual_order,
        })
    }

    /// Read metadata and items from the same database snapshot.
    pub async fn feed(&self, name: &str) -> Result<Option<Feed>, StoreError> {
        validate_feed_name(name)?;
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query_as::<_, FeedRow>("SELECT * FROM feeds WHERE name = ?")
            .bind(name)
            .fetch_optional(&mut *tx)
            .await?;
        let result = if let Some(row) = row {
            let manual_order = row.manual_order_enabled != 0;
            let contents = sqlx::query_scalar::<_, String>(
                "SELECT i.content_json FROM feed_items i
                 LEFT JOIN feed_manual_order o ON o.feed = i.feed AND o.key = i.key
                 WHERE i.feed = ?
                 ORDER BY CASE WHEN ? THEN o.position ELSE i.position END, i.position, i.key",
            )
            .bind(name)
            .bind(manual_order)
            .fetch_all(&mut *tx)
            .await?;
            let items = contents
                .iter()
                .map(|content| serde_json::from_str(content))
                .collect::<Result<Vec<_>, _>>()?;
            let current_time = now_ms()?;
            let mut view_state = std::collections::BTreeMap::new();
            for (key, snoozed_until_ms, wake_on_update) in
                sqlx::query_as::<_, (String, Option<i64>, i64)>(
                    "SELECT key, snoozed_until_ms, wake_on_update FROM feed_item_view_state WHERE feed = ?",
                )
                .bind(name)
                .fetch_all(&mut *tx)
                .await?
            {
                view_state.insert(key, ItemViewState::effective(snoozed_until_ms, wake_on_update != 0, current_time));
            }
            Some(Feed {
                info: row.into(),
                items,
                manual_order,
                view_state,
            })
        } else {
            None
        };
        tx.commit().await?;
        Ok(result)
    }

    /// Feed metadata in name order, without loading item bodies.
    pub async fn list_feeds(&self) -> Result<Vec<FeedInfo>, StoreError> {
        Ok(
            sqlx::query_as::<_, FeedRow>("SELECT * FROM feeds ORDER BY name")
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .map(Into::into)
                .collect(),
        )
    }

    /// Feed metadata with item and snooze counts, all from one snapshot.
    pub async fn feed_summaries(&self) -> Result<Vec<FeedSummary>, StoreError> {
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query_as::<_, FeedRow>("SELECT * FROM feeds ORDER BY name")
            .fetch_all(&mut *tx)
            .await?;
        let item_counts: HashMap<String, i64> =
            sqlx::query_as("SELECT feed, count(*) FROM feed_items GROUP BY feed")
                .fetch_all(&mut *tx)
                .await?
                .into_iter()
                .collect();
        let snoozes = sqlx::query_as::<_, (String, Option<i64>, i64)>(
            "SELECT feed, snoozed_until_ms, wake_on_update FROM feed_item_view_state",
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let current_time = now_ms()?;
        let mut snoozed: HashMap<String, (usize, Option<i64>)> = HashMap::new();
        for (feed, until, on_update) in snoozes {
            let state = ItemViewState::effective(until, on_update != 0, current_time);
            if state.snoozed {
                let entry = snoozed.entry(feed).or_default();
                entry.0 += 1;
                if let Some(until) = state.snoozed_until_ms {
                    entry.1 = Some(entry.1.map_or(until, |t| t.min(until)));
                }
            }
        }
        Ok(rows
            .into_iter()
            .map(|row| {
                let item_count = item_counts.get(&row.name).copied().unwrap_or(0) as usize;
                let (snoozed_count, next_wake_at_ms) =
                    snoozed.get(&row.name).copied().unwrap_or_default();
                FeedSummary {
                    info: row.into(),
                    item_count,
                    snoozed_count,
                    next_wake_at_ms,
                }
            })
            .collect())
    }

    /// User boards with active todo, open todo, and note counts.
    pub async fn board_summaries(&self) -> Result<Vec<BoardSummary>, StoreError> {
        let rows = sqlx::query_as::<_, (i64, String, i64, i64, i64)>(
            "SELECT b.id, b.name,
                (SELECT count(*) FROM todos t WHERE t.board_id = b.id AND t.archived_at_ms IS NULL),
                (SELECT count(*) FROM todos t WHERE t.board_id = b.id AND t.archived_at_ms IS NULL AND t.done = 0),
                (SELECT count(*) FROM notes n WHERE n.board_id = b.id AND n.archived_at_ms IS NULL)
             FROM boards b WHERE b.is_archive = 0 ORDER BY b.name, b.id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, name, todos, open, notes)| BoardSummary {
                info: BoardInfo { id, name },
                todo_count: todos as usize,
                open_todo_count: open as usize,
                note_count: notes as usize,
            })
            .collect())
    }

    /// Record a fetch failure without touching items or the last submission.
    /// An unknown feed is an error; only an accepted snapshot creates a feed.
    pub async fn report_error(&self, name: &str, message: &str) -> Result<(), StoreError> {
        validate_feed_name(name)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let result =
            sqlx::query("UPDATE feeds SET error_message = ?, error_at_ms = ? WHERE name = ?")
                .bind(message)
                .bind(now_ms()?)
                .bind(name)
                .execute(&mut *tx)
                .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::FeedNotFound(name.to_owned()));
        }
        tx.commit().await?;
        self.notify(Change::Feed {
            name: name.to_owned(),
        });
        Ok(())
    }

    /// Delete a feed and cascade to its items. Returns false if already absent.
    pub async fn delete_feed(&self, name: &str) -> Result<bool, StoreError> {
        validate_feed_name(name)?;
        let deleted = sqlx::query("DELETE FROM feeds WHERE name = ?")
            .bind(name)
            .execute(&self.pool)
            .await?
            .rows_affected()
            != 0;
        if deleted {
            self.notify(Change::Feed {
                name: name.to_owned(),
            });
        }
        Ok(deleted)
    }

    pub async fn create_board(&self, name: &str) -> Result<BoardInfo, StoreError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(StoreError::EmptyBoardName);
        }
        let result = sqlx::query("INSERT INTO boards (name) VALUES (?)")
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(board_name_error)?;
        self.notify(Change::Board {
            id: result.last_insert_rowid(),
        });
        Ok(BoardInfo {
            id: result.last_insert_rowid(),
            name: name.to_owned(),
        })
    }

    pub async fn list_boards(&self) -> Result<Vec<BoardInfo>, StoreError> {
        Ok(sqlx::query_as::<_, BoardInfo>(
            "SELECT id, name FROM boards WHERE is_archive = 0 ORDER BY name, id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn rename_board(&self, id: i64, name: &str) -> Result<(), StoreError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(StoreError::EmptyBoardName);
        }
        let result = sqlx::query("UPDATE boards SET name = ? WHERE id = ? AND is_archive = 0")
            .bind(name)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(board_name_error)?;
        if result.rows_affected() == 0 {
            return Err(StoreError::BoardNotFound(id));
        }
        self.notify(Change::Board { id });
        Ok(())
    }

    /// Delete an empty board, or move all its items to the private archive when
    /// `archive_contents` records the caller's explicit confirmation.
    pub async fn delete_board(&self, id: i64, archive_contents: bool) -> Result<(), StoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let name = sqlx::query_scalar::<_, String>(
            "SELECT name FROM boards WHERE id = ? AND is_archive = 0",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::BoardNotFound(id))?;
        let count: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM todos WHERE board_id = ?) +\
             (SELECT count(*) FROM notes WHERE board_id = ?)",
        )
        .bind(id)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if count != 0 && !archive_contents {
            return Err(StoreError::BoardNotEmpty(id));
        }
        if count != 0 {
            let at = now_ms()?;
            sqlx::query("UPDATE todos SET board_id = 1, archived_at_ms = coalesce(archived_at_ms, ?), archived_from_board = ?, updated_at_ms = ? WHERE board_id = ?")
                .bind(at).bind(&name).bind(at).bind(id).execute(&mut *tx).await?;
            sqlx::query("UPDATE notes SET board_id = 1, archived_at_ms = coalesce(archived_at_ms, ?), archived_from_board = ?, updated_at_ms = ? WHERE board_id = ?")
                .bind(at).bind(&name).bind(at).bind(id).execute(&mut *tx).await?;
        }
        sqlx::query("DELETE FROM boards WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.notify(Change::Board { id });
        if count != 0 {
            self.notify(Change::Board { id: 1 });
        }
        Ok(())
    }

    pub async fn add_todo(
        &self,
        board_id: i64,
        title: &str,
        body: Option<&str>,
        url: Option<&str>,
        reference: Option<&SourceReference>,
    ) -> Result<Todo, StoreError> {
        validate_board_content(Some(title), body)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_user_board(&mut tx, board_id).await?;
        let position = next_position(&mut tx, "todos", board_id, false).await?;
        let at = now_ms()?;
        sqlx::query("INSERT INTO todos (board_id, title, body, url, reference_feed, reference_key, position, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(board_id).bind(title).bind(body).bind(url)
            .bind(reference.map(|r| r.feed.as_str())).bind(reference.map(|r| r.key.as_str()))
            .bind(position).bind(at).bind(at).execute(&mut *tx).await?;
        let id = sqlx::query_scalar::<_, i64>("SELECT last_insert_rowid()")
            .fetch_one(&mut *tx)
            .await?;
        let row =
            sqlx::query_as::<_, TodoRow>(&format!("SELECT {TODO_COLUMNS} FROM todos WHERE id = ?"))
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        self.notify(Change::Board { id: board_id });
        Ok(row.into())
    }

    pub async fn add_note(
        &self,
        board_id: i64,
        title: Option<&str>,
        body: &str,
        color: Option<&str>,
        reference: Option<&SourceReference>,
    ) -> Result<Note, StoreError> {
        self.add_note_with_url(board_id, title, body, None, color, reference)
            .await
    }

    pub async fn add_note_with_url(
        &self,
        board_id: i64,
        title: Option<&str>,
        body: &str,
        url: Option<&str>,
        color: Option<&str>,
        reference: Option<&SourceReference>,
    ) -> Result<Note, StoreError> {
        validate_board_content(title, Some(body))?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_user_board(&mut tx, board_id).await?;
        let position = next_position(&mut tx, "notes", board_id, false).await?;
        let at = now_ms()?;
        sqlx::query("INSERT INTO notes (board_id, title, body, color, reference_feed, reference_key, position, created_at_ms, updated_at_ms, url) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(board_id).bind(title).bind(body).bind(color)
            .bind(reference.map(|r| r.feed.as_str())).bind(reference.map(|r| r.key.as_str()))
            .bind(position).bind(at).bind(at).bind(url).execute(&mut *tx).await?;
        let id = sqlx::query_scalar::<_, i64>("SELECT last_insert_rowid()")
            .fetch_one(&mut *tx)
            .await?;
        let row =
            sqlx::query_as::<_, NoteRow>(&format!("SELECT {NOTE_COLUMNS} FROM notes WHERE id = ?"))
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        self.notify(Change::Board { id: board_id });
        Ok(row.into())
    }

    pub async fn promote_todo(
        &self,
        feed: &str,
        key: &str,
        board_id: i64,
    ) -> Result<Todo, StoreError> {
        validate_feed_name(feed)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_user_board(&mut tx, board_id).await?;
        let content = sqlx::query_scalar::<_, String>(
            "SELECT content_json FROM feed_items WHERE feed = ? AND key = ?",
        )
        .bind(feed)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| StoreError::FeedItemNotFound {
            feed: feed.into(),
            key: key.into(),
        })?;
        let item: Item = serde_json::from_str(&content)?;
        let position = next_position(&mut tx, "todos", board_id, false).await?;
        let at = now_ms()?;
        sqlx::query("INSERT INTO todos (board_id, title, body, url, reference_feed, reference_key, position, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(board_id).bind(&item.title).bind(&item.body).bind(&item.url)
            .bind(feed).bind(key).bind(position).bind(at).bind(at)
            .execute(&mut *tx).await?;
        let id = sqlx::query_scalar::<_, i64>("SELECT last_insert_rowid()")
            .fetch_one(&mut *tx)
            .await?;
        let row =
            sqlx::query_as::<_, TodoRow>(&format!("SELECT {TODO_COLUMNS} FROM todos WHERE id = ?"))
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        self.notify(Change::Board { id: board_id });
        Ok(row.into())
    }

    pub async fn promote_note(
        &self,
        feed: &str,
        key: &str,
        board_id: i64,
    ) -> Result<Note, StoreError> {
        validate_feed_name(feed)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_user_board(&mut tx, board_id).await?;
        let content = sqlx::query_scalar::<_, String>(
            "SELECT content_json FROM feed_items WHERE feed = ? AND key = ?",
        )
        .bind(feed)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| StoreError::FeedItemNotFound {
            feed: feed.into(),
            key: key.into(),
        })?;
        let item: Item = serde_json::from_str(&content)?;
        let position = next_position(&mut tx, "notes", board_id, false).await?;
        let at = now_ms()?;
        sqlx::query("INSERT INTO notes (board_id, title, body, color, reference_feed, reference_key, position, created_at_ms, updated_at_ms, url) VALUES (?, ?, ?, NULL, ?, ?, ?, ?, ?, ?)")
            .bind(board_id).bind(&item.title).bind(item.body.as_deref().unwrap_or(""))
            .bind(feed).bind(key).bind(position).bind(at).bind(at).bind(&item.url)
            .execute(&mut *tx).await?;
        let id = sqlx::query_scalar::<_, i64>("SELECT last_insert_rowid()")
            .fetch_one(&mut *tx)
            .await?;
        let row =
            sqlx::query_as::<_, NoteRow>(&format!("SELECT {NOTE_COLUMNS} FROM notes WHERE id = ?"))
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        self.notify(Change::Board { id: board_id });
        Ok(row.into())
    }

    /// Read the board and both item lists from one SQLite snapshot.
    /// None selects the system archive; a supplied ID must be a user board.
    pub async fn board_contents(
        &self,
        id: Option<i64>,
        archived: bool,
    ) -> Result<BoardContents, StoreError> {
        let mut tx = self.pool.begin().await?;
        let board = if let Some(id) = id {
            Some(
                sqlx::query_as::<_, BoardInfo>(
                    "SELECT id, name FROM boards WHERE id = ? AND is_archive = 0",
                )
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(StoreError::BoardNotFound(id))?,
            )
        } else {
            None
        };
        let condition = if archived || id.is_none() {
            "IS NOT NULL"
        } else {
            "IS NULL"
        };
        let todos: Vec<Todo> = sqlx::query_as::<_, TodoRow>(&format!("SELECT {TODO_COLUMNS} FROM todos WHERE board_id = ? AND archived_at_ms {condition} ORDER BY position, id"))
            .bind(id.unwrap_or(1)).fetch_all(&mut *tx).await?.into_iter().map(Into::into).collect();
        let notes: Vec<Note> = sqlx::query_as::<_, NoteRow>(&format!("SELECT {NOTE_COLUMNS} FROM notes WHERE board_id = ? AND archived_at_ms {condition} ORDER BY position, id"))
            .bind(id.unwrap_or(1)).fetch_all(&mut *tx).await?.into_iter().map(Into::into).collect();
        // Resolve only this view's distinct references, without a query per item.
        let source_rows = sqlx::query_as::<_, (String, String, String)>(&format!(
            "SELECT f.feed, f.key, f.content_json FROM feed_items f JOIN (
                SELECT reference_feed AS feed, reference_key AS key FROM todos
                    WHERE board_id = ? AND archived_at_ms {condition}
                UNION
                SELECT reference_feed AS feed, reference_key AS key FROM notes
                    WHERE board_id = ? AND archived_at_ms {condition}
            ) refs ON f.feed = refs.feed AND f.key = refs.key"
        ))
        .bind(id.unwrap_or(1))
        .bind(id.unwrap_or(1))
        .fetch_all(&mut *tx)
        .await?;
        let mut sources = HashMap::new();
        for (feed, key, json) in source_rows {
            sources.insert((feed, key), serde_json::from_str::<Item>(&json)?);
        }
        let todos = todos
            .into_iter()
            .map(|item| BoardItem {
                resolved_reference: resolve_reference(item.reference.as_ref(), &sources),
                item,
            })
            .collect();
        let notes = notes
            .into_iter()
            .map(|item| BoardItem {
                resolved_reference: resolve_reference(item.reference.as_ref(), &sources),
                item,
            })
            .collect();
        tx.commit().await?;
        Ok(BoardContents {
            board,
            todos,
            notes,
        })
    }

    pub async fn board_todos(
        &self,
        board_id: i64,
        include_archived: bool,
    ) -> Result<Vec<Todo>, StoreError> {
        ensure_user_board_pool(&self.pool, board_id).await?;
        let sql = format!(
            "SELECT {TODO_COLUMNS} FROM todos WHERE board_id = ? {} ORDER BY position, id",
            if include_archived {
                ""
            } else {
                "AND archived_at_ms IS NULL"
            }
        );
        Ok(sqlx::query_as::<_, TodoRow>(&sql)
            .bind(board_id)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    pub async fn board_notes(
        &self,
        board_id: i64,
        include_archived: bool,
    ) -> Result<Vec<Note>, StoreError> {
        ensure_user_board_pool(&self.pool, board_id).await?;
        let sql = format!(
            "SELECT {NOTE_COLUMNS} FROM notes WHERE board_id = ? {} ORDER BY position, id",
            if include_archived {
                ""
            } else {
                "AND archived_at_ms IS NULL"
            }
        );
        Ok(sqlx::query_as::<_, NoteRow>(&sql)
            .bind(board_id)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    /// The system archive is separate from any user's named board.
    pub async fn archived_todos(&self) -> Result<Vec<Todo>, StoreError> {
        Ok(sqlx::query_as::<_, TodoRow>(&format!(
            "SELECT {TODO_COLUMNS} FROM todos WHERE board_id = 1 ORDER BY position, id"
        ))
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Into::into)
        .collect())
    }

    pub async fn archived_notes(&self) -> Result<Vec<Note>, StoreError> {
        Ok(sqlx::query_as::<_, NoteRow>(&format!(
            "SELECT {NOTE_COLUMNS} FROM notes WHERE board_id = 1 ORDER BY position, id"
        ))
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Into::into)
        .collect())
    }

    pub async fn archive_todo(&self, id: i64) -> Result<(), StoreError> {
        self.patch_todo(
            id,
            TodoPatch {
                archived: Some(true),
                ..Default::default()
            },
        )
        .await?;
        Ok(())
    }
    pub async fn archive_note(&self, id: i64) -> Result<(), StoreError> {
        self.patch_note(
            id,
            NotePatch {
                archived: Some(true),
                ..Default::default()
            },
        )
        .await?;
        Ok(())
    }
    pub async fn restore_todo(&self, id: i64, destination_board: i64) -> Result<Todo, StoreError> {
        self.patch_todo(
            id,
            TodoPatch {
                archived: Some(false),
                board_id: Some(destination_board),
                ..Default::default()
            },
        )
        .await
    }
    pub async fn restore_note(&self, id: i64, destination_board: i64) -> Result<Note, StoreError> {
        self.patch_note(
            id,
            NotePatch {
                archived: Some(false),
                board_id: Some(destination_board),
                ..Default::default()
            },
        )
        .await
    }
    pub async fn move_todo(&self, id: i64, destination_board: i64) -> Result<Todo, StoreError> {
        self.patch_todo(
            id,
            TodoPatch {
                board_id: Some(destination_board),
                ..Default::default()
            },
        )
        .await
    }
    pub async fn move_note(&self, id: i64, destination_board: i64) -> Result<Note, StoreError> {
        self.patch_note(
            id,
            NotePatch {
                board_id: Some(destination_board),
                ..Default::default()
            },
        )
        .await
    }
    pub async fn set_todo_done(&self, id: i64, done: bool) -> Result<(), StoreError> {
        self.patch_todo(
            id,
            TodoPatch {
                done: Some(done),
                ..Default::default()
            },
        )
        .await?;
        Ok(())
    }

    pub async fn patch_todo(&self, id: i64, patch: TodoPatch) -> Result<Todo, StoreError> {
        validate_board_content(
            patch.title.as_deref(),
            patch.body.0.as_ref().and_then(Option::as_deref),
        )?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let old_board = sqlx::query_scalar::<_, i64>("SELECT board_id FROM todos WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        let (board_id, archived_at, old_position, moved_or_restored) = patch_target(
            &mut tx,
            "todos",
            "todo",
            id,
            patch.board_id,
            patch.archived,
            patch.position,
        )
        .await?;
        let position = if archived_at.is_some() {
            old_position
        } else if let Some(position) = patch.position {
            position
        } else if moved_or_restored {
            next_position(&mut tx, "todos", board_id, false).await?
        } else {
            old_position
        };
        let reference = patch.reference.0.as_ref().and_then(Option::as_ref);
        sqlx::query("UPDATE todos SET title = coalesce(?, title), body = CASE WHEN ? THEN ? ELSE body END, url = CASE WHEN ? THEN ? ELSE url END, done = coalesce(?, done), reference_feed = CASE WHEN ? THEN ? ELSE reference_feed END, reference_key = CASE WHEN ? THEN ? ELSE reference_key END, board_id = ?, position = ?, archived_at_ms = ?, archived_from_board = CASE WHEN ? THEN NULL ELSE archived_from_board END, updated_at_ms = ? WHERE id = ?")
            .bind(patch.title.as_deref())
            .bind(patch.body.0.is_some()).bind(patch.body.0.as_ref().and_then(Option::as_deref))
            .bind(patch.url.0.is_some()).bind(patch.url.0.as_ref().and_then(Option::as_deref))
            .bind(patch.done)
            .bind(patch.reference.0.is_some()).bind(reference.map(|r| r.feed.as_str()))
            .bind(patch.reference.0.is_some()).bind(reference.map(|r| r.key.as_str()))
            .bind(board_id).bind(position).bind(archived_at).bind(moved_or_restored)
            .bind(now_ms()?).bind(id).execute(&mut *tx).await?;
        if archived_at.is_none() && patch.position.is_some() {
            reorder_item(&mut tx, "todos", board_id, id, position).await?;
        }
        let row =
            sqlx::query_as::<_, TodoRow>(&format!("SELECT {TODO_COLUMNS} FROM todos WHERE id = ?"))
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        self.notify(Change::Board { id: board_id });
        if let Some(old_board) = old_board
            && old_board != board_id
        {
            self.notify(Change::Board { id: old_board });
        }
        Ok(row.into())
    }

    pub async fn patch_note(&self, id: i64, patch: NotePatch) -> Result<Note, StoreError> {
        validate_board_content(
            patch.title.0.as_ref().and_then(Option::as_deref),
            patch.body.as_deref(),
        )?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let old_board = sqlx::query_scalar::<_, i64>("SELECT board_id FROM notes WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        let (board_id, archived_at, old_position, moved_or_restored) = patch_target(
            &mut tx,
            "notes",
            "note",
            id,
            patch.board_id,
            patch.archived,
            patch.position,
        )
        .await?;
        let position = if archived_at.is_some() {
            old_position
        } else if let Some(position) = patch.position {
            position
        } else if moved_or_restored {
            next_position(&mut tx, "notes", board_id, false).await?
        } else {
            old_position
        };
        let reference = patch.reference.0.as_ref().and_then(Option::as_ref);
        sqlx::query("UPDATE notes SET title = CASE WHEN ? THEN ? ELSE title END, body = coalesce(?, body), url = CASE WHEN ? THEN ? ELSE url END, color = CASE WHEN ? THEN ? ELSE color END, reference_feed = CASE WHEN ? THEN ? ELSE reference_feed END, reference_key = CASE WHEN ? THEN ? ELSE reference_key END, board_id = ?, position = ?, archived_at_ms = ?, archived_from_board = CASE WHEN ? THEN NULL ELSE archived_from_board END, updated_at_ms = ? WHERE id = ?")
            .bind(patch.title.0.is_some()).bind(patch.title.0.as_ref().and_then(Option::as_deref))
            .bind(patch.body.as_deref())
            .bind(patch.url.0.is_some()).bind(patch.url.0.as_ref().and_then(Option::as_deref))
            .bind(patch.color.0.is_some()).bind(patch.color.0.as_ref().and_then(Option::as_deref))
            .bind(patch.reference.0.is_some()).bind(reference.map(|r| r.feed.as_str()))
            .bind(patch.reference.0.is_some()).bind(reference.map(|r| r.key.as_str()))
            .bind(board_id).bind(position).bind(archived_at).bind(moved_or_restored)
            .bind(now_ms()?).bind(id).execute(&mut *tx).await?;
        if archived_at.is_none() && patch.position.is_some() {
            reorder_item(&mut tx, "notes", board_id, id, position).await?;
        }
        let row =
            sqlx::query_as::<_, NoteRow>(&format!("SELECT {NOTE_COLUMNS} FROM notes WHERE id = ?"))
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        self.notify(Change::Board { id: board_id });
        if let Some(old_board) = old_board
            && old_board != board_id
        {
            self.notify(Change::Board { id: old_board });
        }
        Ok(row.into())
    }

    pub async fn delete_todo(&self, id: i64) -> Result<(), StoreError> {
        self.delete_item("todos", "todo", id).await
    }
    pub async fn delete_note(&self, id: i64) -> Result<(), StoreError> {
        self.delete_item("notes", "note", id).await
    }

    async fn delete_item(
        &self,
        table: &'static str,
        kind: &'static str,
        id: i64,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let board_id = sqlx::query_scalar::<_, i64>(&format!(
            "DELETE FROM {table} WHERE id = ? RETURNING board_id"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::BoardItemNotFound { kind, id })?;
        tx.commit().await?;
        self.notify(Change::Board { id: board_id });
        Ok(())
    }
}

async fn patch_target(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &'static str,
    kind: &'static str,
    id: i64,
    requested_board: Option<i64>,
    requested_archive: Option<bool>,
    requested_position: Option<i64>,
) -> Result<(i64, Option<i64>, i64, bool), StoreError> {
    let (old_board, old_archived, old_position) = sqlx::query_as::<_, (i64, Option<i64>, i64)>(
        &format!("SELECT board_id, archived_at_ms, position FROM {table} WHERE id = ?"),
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(StoreError::BoardItemNotFound { kind, id })?;
    if let Some(position) = requested_position
        && position < 0
    {
        return Err(StoreError::InvalidPosition(position));
    }
    let was_archived = old_archived.is_some();
    let archive = requested_archive.unwrap_or(was_archived);
    let board_id = requested_board.unwrap_or(old_board);
    if requested_board.is_some() || board_id != 1 {
        ensure_user_board(tx, board_id).await?;
    }
    if archive {
        if requested_position.is_some() {
            return Err(StoreError::InvalidPatch(
                "restore an archived item before ordering it",
            ));
        }
        return Ok((
            board_id,
            Some(old_archived.unwrap_or(now_ms()?)),
            old_position,
            old_board != board_id,
        ));
    }
    if board_id == 1 {
        return Err(StoreError::RestoreBoardRequired);
    }

    let transitioned = was_archived || old_board != board_id;
    let position = if transitioned {
        next_position(tx, table, board_id, false).await?
    } else {
        old_position
    };
    Ok((board_id, None, position, transitioned))
}

async fn reorder_item(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &'static str,
    board_id: i64,
    item_id: i64,
    position: i64,
) -> Result<(), StoreError> {
    let mut ids = sqlx::query_scalar::<_, i64>(&format!(
        "SELECT id FROM {table} WHERE board_id = ? AND archived_at_ms IS NULL ORDER BY position, id"
    ))
    .bind(board_id)
    .fetch_all(&mut **tx)
    .await?;
    if position < 0 || position >= ids.len() as i64 {
        return Err(StoreError::InvalidPosition(position));
    }
    let Some(old_index) = ids.iter().position(|id| *id == item_id) else {
        return Err(StoreError::BoardItemNotFound {
            kind: if table == "todos" { "todo" } else { "note" },
            id: item_id,
        });
    };
    let item = ids.remove(old_index);
    ids.insert(position as usize, item);
    for (position, id) in ids.into_iter().enumerate() {
        sqlx::query(&format!("UPDATE {table} SET position = ? WHERE id = ?"))
            .bind(position as i64)
            .bind(id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

fn board_name_error(error: sqlx::Error) -> StoreError {
    match &error {
        sqlx::Error::Database(database_error) if database_error.is_unique_violation() => {
            StoreError::BoardNameTaken
        }
        _ => StoreError::Database(error),
    }
}

async fn ensure_user_board(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: i64,
) -> Result<(), StoreError> {
    if sqlx::query_scalar::<_, i64>("SELECT id FROM boards WHERE id = ? AND is_archive = 0")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .is_none()
    {
        return Err(StoreError::BoardNotFound(id));
    }
    Ok(())
}

async fn ensure_user_board_pool(pool: &SqlitePool, id: i64) -> Result<(), StoreError> {
    if sqlx::query_scalar::<_, i64>("SELECT id FROM boards WHERE id = ? AND is_archive = 0")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .is_none()
    {
        return Err(StoreError::BoardNotFound(id));
    }
    Ok(())
}

async fn next_position(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &'static str,
    board_id: i64,
    include_archived: bool,
) -> Result<i64, StoreError> {
    let filter = if include_archived {
        ""
    } else {
        "AND archived_at_ms IS NULL"
    };
    Ok(sqlx::query_scalar::<_, Option<i64>>(&format!(
        "SELECT max(position) FROM {table} WHERE board_id = ? {filter}"
    ))
    .bind(board_id)
    .fetch_one(&mut **tx)
    .await?
    .unwrap_or(-1)
        + 1)
}

fn now_ms() -> Result<i64, StoreError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StoreError::InvalidSystemTime)?
        .as_millis()
        .try_into()
        .map_err(|_| StoreError::InvalidSystemTime)
}

impl ItemViewState {
    fn effective(until: Option<i64>, on_update: bool, now: i64) -> Self {
        if until.is_some_and(|deadline| deadline <= now) {
            return Self {
                snoozed_until_ms: None,
                wake_on_update: false,
                snoozed: false,
            };
        }
        Self {
            snoozed_until_ms: until,
            wake_on_update: on_update,
            snoozed: until.is_some() || on_update,
        }
    }
}

fn validate_board_content(title: Option<&str>, body: Option<&str>) -> Result<(), StoreError> {
    if title.is_some_and(|s| s.chars().count() > MAX_TITLE_CHARS) {
        return Err(StoreError::ContentLimit("title exceeds 500 characters"));
    }
    if body.is_some_and(|s| s.len() > MAX_BODY_BYTES) {
        return Err(StoreError::ContentLimit("body exceeds 16 KiB"));
    }
    Ok(())
}
