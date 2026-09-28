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
    ChangeSummary, Item, MAX_SNAPSHOT_BYTES, Snapshot, ValidationError, validate_feed_name,
};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error("snapshot exceeds 4 MiB when serialized")]
    SnapshotTooLarge,
    #[error("feed not found: {0}")]
    FeedNotFound(String),
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Feed {
    pub info: FeedInfo,
    /// Submitted order; manual order and view state will be added separately.
    pub items: Vec<Item>,
}

#[derive(FromRow)]
struct FeedRow {
    name: String,
    title: String,
    source_url: Option<String>,
    stale_after: Option<String>,
    last_submitted_at_ms: i64,
    error_message: Option<String>,
    error_at_ms: Option<i64>,
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

/// Clones share a connection pool. SQLite serializes write transactions even
/// across independently opened stores; reads use consistent transactions.
#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
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
        Ok(Self { pool })
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
        tx.commit().await?;
        Ok(summary)
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
            let contents = sqlx::query_scalar::<_, String>(
                "SELECT content_json FROM feed_items WHERE feed = ? ORDER BY position, key",
            )
            .bind(name)
            .fetch_all(&mut *tx)
            .await?;
            let items = contents
                .iter()
                .map(|content| serde_json::from_str(content))
                .collect::<Result<Vec<_>, _>>()?;
            Some(Feed {
                info: row.into(),
                items,
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
        Ok(())
    }

    /// Delete a feed and cascade to its items. Returns false if already absent.
    pub async fn delete_feed(&self, name: &str) -> Result<bool, StoreError> {
        validate_feed_name(name)?;
        Ok(sqlx::query("DELETE FROM feeds WHERE name = ?")
            .bind(name)
            .execute(&self.pool)
            .await?
            .rows_affected()
            != 0)
    }
}

fn now_ms() -> Result<i64, StoreError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StoreError::InvalidSystemTime)?
        .as_millis()
        .try_into()
        .map_err(|_| StoreError::InvalidSystemTime)
}
