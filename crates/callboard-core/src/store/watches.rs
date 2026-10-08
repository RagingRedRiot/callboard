//! Watch persistence (DESIGN.md §4a). Item states are computed at read time
//! from stored clocks (`watch::Clocks`); only writes emit change notices.

use std::collections::HashMap;

use sqlx::FromRow;

use super::{Change, FeedError, MAX_SNAPSHOT_BYTES, Store, StoreError, now_ms};
use crate::feed::MAX_ITEMS;
use crate::watch::{
    AddedItem, Clocks, DEFAULT_QUIET_AFTER, DEFAULT_WAITING_AFTER, Failure, ItemAction,
    ItemContent, ItemState, NewItem, Report, ReportSummary, Reported, StateAt, Watch, WatchInfo,
    WatchItem, WatchSummary, order_key, validate_watch_name, window,
};

#[derive(FromRow)]
struct WatchRow {
    name: String,
    title: String,
    description: Option<String>,
    source_url: Option<String>,
    stale_after: Option<String>,
    waiting_after: String,
    quiet_after: String,
    last_reported_at_ms: i64,
    error_message: Option<String>,
    error_at_ms: Option<i64>,
}

impl WatchRow {
    fn windows(&self) -> (Option<i64>, Option<i64>) {
        (window(&self.waiting_after), window(&self.quiet_after))
    }
}

impl From<WatchRow> for WatchInfo {
    fn from(row: WatchRow) -> Self {
        Self {
            name: row.name,
            title: row.title,
            description: row.description,
            source_url: row.source_url,
            stale_after: row.stale_after,
            waiting_after: row.waiting_after,
            quiet_after: row.quiet_after,
            last_reported_at_ms: row.last_reported_at_ms,
            error: row
                .error_message
                .zip(row.error_at_ms)
                .map(|(message, at_ms)| FeedError { message, at_ms }),
        }
    }
}

#[derive(FromRow)]
struct ItemRow {
    id: i64,
    url: String,
    label: Option<String>,
    added_at_ms: i64,
    content_json: Option<String>,
    fingerprint: Option<String>,
    reported_at_ms: Option<i64>,
    changed_at_ms: Option<i64>,
    error_message: Option<String>,
    error_at_ms: Option<i64>,
    attention_since_ms: Option<i64>,
    waiting_since_ms: Option<i64>,
    quiet_reported: i64,
}

const ITEM_COLUMNS: &str = "id, url, label, added_at_ms, content_json, fingerprint, reported_at_ms, changed_at_ms, error_message, error_at_ms, attention_since_ms, waiting_since_ms, quiet_reported";

impl ItemRow {
    fn clocks(&self) -> Clocks {
        Clocks {
            added_at_ms: self.added_at_ms,
            attention_since_ms: self.attention_since_ms,
            waiting_since_ms: self.waiting_since_ms,
        }
    }

    fn into_item(self, state: StateAt) -> Result<WatchItem, StoreError> {
        Ok(WatchItem {
            id: self.id,
            url: self.url,
            label: self.label,
            added_at_ms: self.added_at_ms,
            content: self
                .content_json
                .map(|json| serde_json::from_str::<ItemContent>(&json))
                .transpose()?,
            fingerprint: self.fingerprint,
            reported_at_ms: self.reported_at_ms,
            changed_at_ms: self.changed_at_ms,
            error: self
                .error_message
                .zip(self.error_at_ms)
                .map(|(message, at_ms)| FeedError { message, at_ms }),
            attention_since_ms: self.attention_since_ms,
            state: state.state,
            state_since_ms: state.since_ms,
            waiting_since_ms: state.waiting_since_ms,
        })
    }
}

type Tx<'a> = sqlx::Transaction<'a, sqlx::Sqlite>;

async fn watch_row(tx: &mut Tx<'_>, name: &str) -> Result<Option<WatchRow>, StoreError> {
    Ok(
        sqlx::query_as::<_, WatchRow>("SELECT * FROM watches WHERE name = ?")
            .bind(name)
            .fetch_optional(&mut **tx)
            .await?,
    )
}

async fn item_rows(tx: &mut Tx<'_>, watch: &str) -> Result<Vec<ItemRow>, StoreError> {
    Ok(sqlx::query_as::<_, ItemRow>(&format!(
        "SELECT {ITEM_COLUMNS} FROM watch_items WHERE watch = ?"
    ))
    .bind(watch)
    .fetch_all(&mut **tx)
    .await?)
}

async fn item_row(tx: &mut Tx<'_>, watch: &str, id: i64) -> Result<ItemRow, StoreError> {
    sqlx::query_as::<_, ItemRow>(&format!(
        "SELECT {ITEM_COLUMNS} FROM watch_items WHERE watch = ? AND id = ?"
    ))
    .bind(watch)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| StoreError::WatchItemNotFound {
        watch: watch.to_owned(),
        id,
    })
}

async fn require_watch(tx: &mut Tx<'_>, name: &str) -> Result<WatchRow, StoreError> {
    watch_row(tx, name)
        .await?
        .ok_or_else(|| StoreError::WatchNotFound(name.to_owned()))
}

/// Store an item's clocks after an acknowledgement or Keep waiting. Leaving
/// a state also forgets that a report listed it as quiet.
async fn set_clocks(tx: &mut Tx<'_>, id: i64, clocks: Clocks) -> Result<(), StoreError> {
    sqlx::query(
        "UPDATE watch_items SET attention_since_ms = ?, waiting_since_ms = ?, quiet_reported = 0
         WHERE id = ?",
    )
    .bind(clocks.attention_since_ms)
    .bind(clocks.waiting_since_ms)
    .bind(id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl Store {
    /// Apply a script's report: replace the watch metadata (creating the
    /// watch if unknown) and record each item report. The fingerprint is
    /// compared before an acknowledgement is applied, so a report that
    /// changes and acknowledges an item moves it straight to Waiting.
    pub async fn report_watch(
        &self,
        name: &str,
        report: &Report,
    ) -> Result<ReportSummary, StoreError> {
        validate_watch_name(name)?;
        report.validate()?;
        if serde_json::to_vec(report)?.len() > MAX_SNAPSHOT_BYTES {
            return Err(StoreError::SnapshotTooLarge);
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let at = now_ms()?;
        let created = watch_row(&mut tx, name).await?.is_none();
        sqlx::query(
            "INSERT INTO watches (name, title, description, source_url, stale_after,
                                  waiting_after, quiet_after, last_reported_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(name) DO UPDATE SET
               title = excluded.title, description = excluded.description,
               source_url = excluded.source_url, stale_after = excluded.stale_after,
               waiting_after = excluded.waiting_after, quiet_after = excluded.quiet_after,
               last_reported_at_ms = excluded.last_reported_at_ms,
               error_message = NULL, error_at_ms = NULL",
        )
        .bind(name)
        .bind(report.title.as_deref().unwrap_or(name))
        .bind(&report.description)
        .bind(&report.source_url)
        .bind(&report.stale_after)
        .bind(
            report
                .waiting_after
                .as_deref()
                .unwrap_or(DEFAULT_WAITING_AFTER),
        )
        .bind(report.quiet_after.as_deref().unwrap_or(DEFAULT_QUIET_AFTER))
        .bind(at)
        .execute(&mut *tx)
        .await?;

        let rows: HashMap<i64, ItemRow> = item_rows(&mut tx, name)
            .await?
            .into_iter()
            .map(|row| (row.id, row))
            .collect();
        let mut summary = ReportSummary {
            created,
            ..Default::default()
        };
        for item in &report.items {
            let Some(row) = rows.get(&item.id) else {
                summary.ignored_ids.push(item.id);
                continue;
            };
            summary.reported += 1;
            let (content, fingerprint, acknowledge) = match item.reported() {
                Reported::Error(message) => {
                    sqlx::query(
                        "UPDATE watch_items SET error_message = ?, error_at_ms = ? WHERE id = ?",
                    )
                    .bind(message)
                    .bind(at)
                    .bind(item.id)
                    .execute(&mut *tx)
                    .await?;
                    continue;
                }
                Reported::Content {
                    content,
                    fingerprint,
                    acknowledge,
                } => (content, fingerprint, acknowledge),
            };
            // The first report is the baseline; only a later change counts.
            let changed = row
                .fingerprint
                .as_deref()
                .is_some_and(|previous| previous != fingerprint);
            let mut clocks = row.clocks();
            let mut quiet_reported = row.quiet_reported;
            let mut raised = false;
            if changed && clocks.attention_since_ms.is_none() {
                clocks.attention_since_ms = Some(at);
                quiet_reported = 0;
                raised = true;
            }
            if acknowledge && clocks.attention_since_ms.is_some() {
                clocks.attention_since_ms = None;
                clocks.waiting_since_ms = Some(at);
                quiet_reported = 0;
                raised = false;
            }
            if raised {
                summary.attention_ids.push(item.id);
            }
            sqlx::query(
                "UPDATE watch_items SET content_json = ?, fingerprint = ?, reported_at_ms = ?,
                   changed_at_ms = ?, error_message = NULL, error_at_ms = NULL,
                   attention_since_ms = ?, waiting_since_ms = ?, quiet_reported = ?
                 WHERE id = ?",
            )
            .bind(serde_json::to_string(&content)?)
            .bind(fingerprint)
            .bind(at)
            .bind(if changed { Some(at) } else { row.changed_at_ms })
            .bind(clocks.attention_since_ms)
            .bind(clocks.waiting_since_ms)
            .bind(quiet_reported)
            .bind(item.id)
            .execute(&mut *tx)
            .await?;
        }

        // Items that became quiet since the previous report, listed once.
        let watch = require_watch(&mut tx, name).await?;
        let (waiting_after, quiet_after) = watch.windows();
        let items = item_rows(&mut tx, name).await?;
        let mut newly_quiet = Vec::new();
        for row in &items {
            let state = row.clocks().state_at(waiting_after, quiet_after, at);
            let quiet = state.state == ItemState::Quiet;
            if quiet != (row.quiet_reported != 0) {
                if quiet {
                    newly_quiet.push((state.since_ms, row.id));
                }
                sqlx::query("UPDATE watch_items SET quiet_reported = ? WHERE id = ?")
                    .bind(quiet)
                    .bind(row.id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        newly_quiet.sort();
        summary.quiet_ids = newly_quiet.into_iter().map(|(_, id)| id).collect();
        tx.commit().await?;
        self.notify(Change::Watch {
            name: name.to_owned(),
        });
        Ok(summary)
    }

    /// Record a failed run for the watch, or for one item with `id`, keeping
    /// the last good reports. An unknown watch or item is an error.
    pub async fn report_watch_error(
        &self,
        name: &str,
        failure: &Failure,
    ) -> Result<(), StoreError> {
        validate_watch_name(name)?;
        if failure.message.chars().count() > crate::watch::MAX_ERROR_CHARS {
            return Err(StoreError::ContentLimit("error exceeds 1000 characters"));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        require_watch(&mut tx, name).await?;
        let at = now_ms()?;
        match failure.id {
            None => {
                sqlx::query("UPDATE watches SET error_message = ?, error_at_ms = ? WHERE name = ?")
                    .bind(&failure.message)
                    .bind(at)
                    .bind(name)
                    .execute(&mut *tx)
                    .await?;
            }
            Some(id) => {
                item_row(&mut tx, name, id).await?;
                sqlx::query(
                    "UPDATE watch_items SET error_message = ?, error_at_ms = ? WHERE id = ?",
                )
                .bind(&failure.message)
                .bind(at)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        self.notify(Change::Watch {
            name: name.to_owned(),
        });
        Ok(())
    }

    /// A watch with its items in queue order, read from one snapshot.
    pub async fn watch(&self, name: &str) -> Result<Option<Watch>, StoreError> {
        validate_watch_name(name)?;
        let mut tx = self.pool.begin().await?;
        let Some(row) = watch_row(&mut tx, name).await? else {
            tx.commit().await?;
            return Ok(None);
        };
        let rows = item_rows(&mut tx, name).await?;
        tx.commit().await?;
        let at = now_ms()?;
        let (waiting_after, quiet_after) = row.windows();
        let mut next_wake_at_ms: Option<i64> = None;
        let mut items = Vec::with_capacity(rows.len());
        for item in rows {
            let state = item.clocks().state_at(waiting_after, quiet_after, at);
            if let Some(next) = state.next_change_ms {
                next_wake_at_ms = Some(next_wake_at_ms.map_or(next, |t| t.min(next)));
            }
            items.push((order_key(&state, item.id), item.into_item(state)?));
        }
        items.sort_by_key(|(key, _)| *key);
        Ok(Some(Watch {
            info: row.into(),
            items: items.into_iter().map(|(_, item)| item).collect(),
            next_wake_at_ms,
        }))
    }

    /// Watch metadata with state counts, all from one snapshot.
    pub async fn watch_summaries(&self) -> Result<Vec<WatchSummary>, StoreError> {
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query_as::<_, WatchRow>("SELECT * FROM watches ORDER BY name")
            .fetch_all(&mut *tx)
            .await?;
        let clocks = sqlx::query_as::<_, (String, i64, Option<i64>, Option<i64>)>(
            "SELECT watch, added_at_ms, attention_since_ms, waiting_since_ms FROM watch_items",
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let at = now_ms()?;
        let windows: HashMap<&str, _> = rows
            .iter()
            .map(|row| (row.name.as_str(), row.windows()))
            .collect();
        #[derive(Default, Clone, Copy)]
        struct Counts {
            items: usize,
            attention: usize,
            quiet: usize,
            new: usize,
            next: Option<i64>,
        }
        let mut counts: HashMap<String, Counts> = HashMap::new();
        for (watch, added_at_ms, attention_since_ms, waiting_since_ms) in clocks {
            let Some((waiting_after, quiet_after)) = windows.get(watch.as_str()).copied() else {
                continue;
            };
            let state = Clocks {
                added_at_ms,
                attention_since_ms,
                waiting_since_ms,
            }
            .state_at(waiting_after, quiet_after, at);
            let entry = counts.entry(watch).or_default();
            entry.items += 1;
            match state.state {
                ItemState::Attention => entry.attention += 1,
                ItemState::Quiet => entry.quiet += 1,
                ItemState::New => entry.new += 1,
                ItemState::Waiting => (),
            }
            if let Some(next) = state.next_change_ms {
                entry.next = Some(entry.next.map_or(next, |t| t.min(next)));
            }
        }
        Ok(rows
            .into_iter()
            .map(|row| {
                let c = counts.get(&row.name).copied().unwrap_or_default();
                WatchSummary {
                    info: row.into(),
                    item_count: c.items,
                    attention_count: c.attention,
                    quiet_count: c.quiet,
                    new_count: c.new,
                    next_wake_at_ms: c.next,
                }
            })
            .collect())
    }

    /// Delete a watch and its items. Returns false if already absent.
    pub async fn delete_watch(&self, name: &str) -> Result<bool, StoreError> {
        validate_watch_name(name)?;
        let deleted = sqlx::query("DELETE FROM watches WHERE name = ?")
            .bind(name)
            .execute(&self.pool)
            .await?
            .rows_affected()
            != 0;
        if deleted {
            self.notify(Change::Watch {
                name: name.to_owned(),
            });
        }
        Ok(deleted)
    }

    /// Add an item to an existing watch. A URL already on the watch adds
    /// nothing and returns the existing item, its label unchanged.
    pub async fn add_watch_item(
        &self,
        name: &str,
        item: &NewItem,
    ) -> Result<AddedItem, StoreError> {
        validate_watch_name(name)?;
        let (url, label) = item.normalized()?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let watch = require_watch(&mut tx, name).await?;
        let existing =
            sqlx::query_scalar::<_, i64>("SELECT id FROM watch_items WHERE watch = ? AND url = ?")
                .bind(name)
                .bind(&url)
                .fetch_optional(&mut *tx)
                .await?;
        let (id, created) = match existing {
            Some(id) => (id, false),
            None => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM watch_items WHERE watch = ?",
                )
                .bind(name)
                .fetch_one(&mut *tx)
                .await?;
                if count as usize >= MAX_ITEMS {
                    return Err(StoreError::ContentLimit("a watch holds at most 1000 items"));
                }
                let id = sqlx::query(
                    "INSERT INTO watch_items (watch, url, label, added_at_ms) VALUES (?, ?, ?, ?)",
                )
                .bind(name)
                .bind(&url)
                .bind(&label)
                .bind(now_ms()?)
                .execute(&mut *tx)
                .await?
                .last_insert_rowid();
                (id, true)
            }
        };
        let row = item_row(&mut tx, name, id).await?;
        tx.commit().await?;
        if created {
            self.notify(Change::Watch {
                name: name.to_owned(),
            });
        }
        let (waiting_after, quiet_after) = watch.windows();
        let state = row.clocks().state_at(waiting_after, quiet_after, now_ms()?);
        Ok(AddedItem {
            created,
            item: row.into_item(state)?,
        })
    }

    /// Remove an item. Returns false if it was not on the watch.
    pub async fn remove_watch_item(&self, name: &str, id: i64) -> Result<bool, StoreError> {
        validate_watch_name(name)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        require_watch(&mut tx, name).await?;
        let deleted = sqlx::query("DELETE FROM watch_items WHERE watch = ? AND id = ?")
            .bind(name)
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            != 0;
        tx.commit().await?;
        if deleted {
            self.notify(Change::Watch {
                name: name.to_owned(),
            });
        }
        Ok(deleted)
    }

    /// Acknowledge an item (clearing its attention) or keep a quiet item
    /// waiting (restarting its waiting clock). Either does nothing when the
    /// item is not in the state it applies to.
    pub async fn act_on_watch_item(
        &self,
        name: &str,
        id: i64,
        action: ItemAction,
    ) -> Result<WatchItem, StoreError> {
        validate_watch_name(name)?;
        if action.acknowledge == action.keep_waiting {
            return Err(StoreError::InvalidPatch(
                "give exactly one of acknowledge and keep_waiting",
            ));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let watch = require_watch(&mut tx, name).await?;
        let (waiting_after, quiet_after) = watch.windows();
        let mut row = item_row(&mut tx, name, id).await?;
        let at = now_ms()?;
        let state = row.clocks().state_at(waiting_after, quiet_after, at).state;
        let applies = if action.acknowledge {
            state == ItemState::Attention
        } else {
            state == ItemState::Quiet
        };
        if applies {
            row.attention_since_ms = None;
            row.waiting_since_ms = Some(at);
            row.quiet_reported = 0;
            set_clocks(&mut tx, id, row.clocks()).await?;
        }
        tx.commit().await?;
        if applies {
            self.notify(Change::Watch {
                name: name.to_owned(),
            });
        }
        let state = row.clocks().state_at(waiting_after, quiet_after, at);
        row.into_item(state)
    }
}
