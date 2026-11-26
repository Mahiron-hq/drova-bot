use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow, SqliteSynchronous,
};
use sqlx::{AssertSqlSafe, Row, Sqlite, SqliteConnection, SqlitePool, Transaction};

use crate::drova::{Snapshot, Status};
use crate::telegram::Inline;

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_station_extras.sql"),
];

pub const TRANSITIONS_KEEP_DAYS: i64 = 90;
pub const QUEUE_KEEP_DAYS: i64 = 30;
pub const BLOCKED_KEEP_DAYS: i64 = 90;
const POLL_ERRORS_KEEP: i64 = 1000;
/// A station that answers 404 this long is treated as deleted.
pub const GONE_AFTER: i64 = 3_600;
/// Subscriptions to a deleted station are removed after this many days.
pub const GONE_KEEP_DAYS: i64 = 30;

#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
}

#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct UserRow {
    pub id: i64,
    pub is_merchant: bool,
    pub role_selected: bool,
    pub muted: bool,
    pub mute_until: Option<i64>,
    pub tz: String,
    pub quiet_enabled: bool,
    pub quiet_start: i32,
    pub quiet_end: i32,
    pub notify_offline_return: bool,
    pub show_game: bool,
    pub blocked_at: Option<i64>,
}

impl UserRow {
    pub fn mute_active(&self, now: i64) -> bool {
        self.muted && self.mute_until.is_none_or(|u| u > now)
    }
}

#[derive(Debug, Clone)]
pub struct StationRow {
    pub uuid: String,
    pub name: String,
    pub status: Status,
    pub published: bool,
    pub city: Option<String>,
    pub product_id: Option<String>,
    pub games_count: Option<i64>,
    pub status_since: i64,
    pub checked_at: Option<i64>,
    pub description: Option<String>,
    pub missing_since: Option<i64>,
    pub gone_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct SubRow {
    pub id: i64,
    pub user_id: i64,
    pub last_status: Status,
    pub pending: bool,
    pub notify_busy: bool,
    pub notify_offline: bool,
    pub comment: Option<String>,
    pub station: StationRow,
}

#[derive(Debug, Clone)]
pub struct Transition {
    pub to: Status,
    pub product_id: Option<String>,
    pub at: i64,
}

#[derive(Debug, Clone)]
pub struct QueueRow {
    pub id: i64,
    pub kind: String,
    pub chat_id: i64,
    pub text: String,
    pub keyboard: Option<String>,
    pub attempts: i64,
}

/// A message to be put into the queue.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub kind: &'static str,
    pub chat_id: i64,
    pub station: Option<String>,
    pub text: String,
    pub keyboard: Option<String>,
}

impl Outgoing {
    pub fn new(
        kind: &'static str,
        chat_id: i64,
        station: Option<&str>,
        text: impl Into<String>,
        keyboard: Option<&Inline>,
    ) -> Result<Self> {
        Ok(Self {
            kind,
            chat_id,
            station: station.map(ToOwned::to_owned),
            text: text.into(),
            keyboard: keyboard.map(serde_json::to_string).transpose()?,
        })
    }
}

async fn insert_msgs(conn: &mut SqliteConnection, msgs: &[Outgoing], now: i64) -> Result<()> {
    for m in msgs {
        sqlx::query(
            "INSERT INTO queue (kind, chat_id, station, text, keyboard, not_before, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(m.kind)
        .bind(m.chat_id)
        .bind(&m.station)
        .bind(&m.text)
        .bind(&m.keyboard)
        .bind(now)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Whether a subscriber still waits for the station to become free.
pub fn pending_for(merchant: bool, status: Status, published: bool) -> bool {
    if merchant {
        status != Status::Ready
    } else {
        !(status == Status::Ready && published)
    }
}

#[derive(Debug)]
pub enum AddResult {
    Added(i64),
    Updated(i64),
    Limit,
}

#[derive(Debug, Clone, Copy)]
pub enum SubFlag {
    Busy,
    Offline,
}

#[derive(Debug, Clone, Copy)]
pub enum UserFlag {
    OfflineReturn,
    ShowGame,
    Quiet,
}

/// Outcome of storing a fresh station snapshot.
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct Applied {
    pub old: Option<StationRow>,
    pub new: StationRow,
    #[cfg_attr(not(test), allow(dead_code))]
    pub status_changed: bool,
    /// A new game started while the station stayed busy.
    #[cfg_attr(not(test), allow(dead_code))]
    pub game_changed_while_busy: bool,
    /// Drova reported BUSY after another state or with a new game; HANDSHAKE alone does not count.
    pub session_started: bool,
    /// The station was marked as deleted and is back.
    pub returned: bool,
    /// A newer snapshot was already stored; nothing was changed.
    pub stale: bool,
}

#[derive(Debug, Default)]
pub struct Stats {
    pub users: i64,
    pub active_users: i64,
    pub blocked_users: i64,
    pub merchants: i64,
    pub muted: i64,
    pub subscriptions: i64,
    pub stations: i64,
    pub queue_pending: i64,
    pub errors_24h: i64,
}

#[derive(Debug, Default)]
pub struct Pruned {
    pub queue: u64,
    pub transitions: u64,
    pub poll_errors: u64,
    pub users: u64,
    pub stations: u64,
}

fn status(row: &SqliteRow, col: &str) -> Result<Status> {
    let s: String = row.try_get(col)?;
    Status::parse(&s).with_context(|| format!("bad status {s:?} in {col}"))
}

fn user_row(r: &SqliteRow) -> Result<UserRow> {
    Ok(UserRow {
        id: r.try_get("id")?,
        is_merchant: r.try_get("is_merchant")?,
        role_selected: r.try_get("role_selected")?,
        muted: r.try_get("muted")?,
        mute_until: r.try_get("mute_until")?,
        tz: r.try_get("tz")?,
        quiet_enabled: r.try_get("quiet_enabled")?,
        quiet_start: r.try_get("quiet_start")?,
        quiet_end: r.try_get("quiet_end")?,
        notify_offline_return: r.try_get("notify_offline_return")?,
        show_game: r.try_get("show_game")?,
        blocked_at: r.try_get("blocked_at")?,
    })
}

fn station_row(r: &SqliteRow, prefix: &str) -> Result<StationRow> {
    let c = |n: &str| format!("{prefix}{n}");
    Ok(StationRow {
        uuid: r.try_get(c("uuid").as_str())?,
        name: r.try_get(c("name").as_str())?,
        status: status(r, &c("status"))?,
        published: r.try_get(c("published").as_str())?,
        city: r.try_get(c("city").as_str())?,
        product_id: r.try_get(c("product_id").as_str())?,
        games_count: r.try_get(c("games_count").as_str())?,
        status_since: r.try_get(c("status_since").as_str())?,
        checked_at: r.try_get(c("checked_at").as_str())?,
        description: r.try_get(c("description").as_str())?,
        missing_since: r.try_get(c("missing_since").as_str())?,
        gone_at: r.try_get(c("gone_at").as_str())?,
    })
}

const SUB_SELECT: &str = "SELECT s.id, s.user_id, s.last_status, s.pending, s.notify_busy, s.notify_offline, s.comment,
    st.uuid AS st_uuid, st.name AS st_name, st.status AS st_status,
    st.published AS st_published, st.city AS st_city, st.product_id AS st_product_id,
    st.games_count AS st_games_count, st.status_since AS st_status_since, st.checked_at AS st_checked_at,
    st.description AS st_description, st.missing_since AS st_missing_since, st.gone_at AS st_gone_at
    FROM subscriptions AS s JOIN stations AS st ON st.uuid = s.station";

fn sub_row(r: &SqliteRow) -> Result<SubRow> {
    Ok(SubRow {
        id: r.try_get("id")?,
        user_id: r.try_get("user_id")?,
        last_status: status(r, "last_status")?,
        pending: r.try_get("pending")?,
        notify_busy: r.try_get("notify_busy")?,
        notify_offline: r.try_get("notify_offline")?,
        comment: r.try_get("comment")?,
        station: station_row(r, "st_")?,
    })
}

fn in_list(n: usize) -> String {
    vec!["?"; n].join(",")
}

impl Db {
    pub async fn open(path: &Path) -> Result<Self> {
        let pool = connect(path, true).await?;
        refuse_legacy(&pool).await?;
        migrate(&pool, Some(path)).await?;
        Ok(Self { pool })
    }

    /// Fresh database for tests or for the legacy import.
    pub async fn from_pool(pool: SqlitePool) -> Result<Self> {
        migrate(&pool, None).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// Transaction that takes the write lock at once, so reads inside it stay current.
    async fn write_tx(&self) -> Result<Transaction<'static, Sqlite>> {
        Ok(self.pool.begin_with("BEGIN IMMEDIATE").await?)
    }

    // Users

    /// Registers activity: creates the user if needed and clears the blocked mark.
    pub async fn touch_user(&self, id: i64, now: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO users (id, created_at, seen_at) VALUES (?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET seen_at = excluded.seen_at, blocked_at = NULL",
        )
        .bind(id)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn user(&self, id: i64) -> Result<Option<UserRow>> {
        let row = sqlx::query("SELECT * FROM users WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(user_row).transpose()
    }

    pub async fn set_role(&self, id: i64, merchant: bool) -> Result<()> {
        sqlx::query("UPDATE users SET is_merchant = ?, role_selected = 1 WHERE id = ?")
            .bind(merchant)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_mute(&self, id: i64, until: Option<i64>) -> Result<()> {
        sqlx::query("UPDATE users SET muted = 1, mute_until = ? WHERE id = ?")
            .bind(until)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn unmute(&self, id: i64) -> Result<()> {
        sqlx::query("UPDATE users SET muted = 0, mute_until = NULL WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Users whose silence has expired.
    pub async fn expired_mutes(&self, now: i64) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT id FROM users WHERE muted = 1 AND mute_until IS NOT NULL AND mute_until <= ?
             ORDER BY id",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Ends expired silence and queues the report together; false when it was not expired.
    pub async fn end_mute(&self, id: i64, now: i64, report: Option<&Outgoing>) -> Result<bool> {
        let mut tx = self.write_tx().await?;
        let blocked: Option<Option<i64>> = sqlx::query_scalar(
            "UPDATE users SET muted = 0, mute_until = NULL
             WHERE id = ? AND muted = 1 AND mute_until IS NOT NULL AND mute_until <= ?
             RETURNING blocked_at",
        )
        .bind(id)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(blocked) = blocked else {
            return Ok(false);
        };
        if blocked.is_none()
            && let Some(m) = report
        {
            insert_msgs(&mut tx, std::slice::from_ref(m), now).await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    pub async fn set_tz(&self, id: i64, tz: &str) -> Result<()> {
        sqlx::query("UPDATE users SET tz = ? WHERE id = ?")
            .bind(tz)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_quiet_window(&self, id: i64, start: i32, end: i32) -> Result<()> {
        sqlx::query(
            "UPDATE users SET quiet_enabled = 1, quiet_start = ?, quiet_end = ? WHERE id = ?",
        )
        .bind(start)
        .bind(end)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn toggle_user_flag(&self, id: i64, flag: UserFlag) -> Result<()> {
        let sql = match flag {
            UserFlag::OfflineReturn => {
                "UPDATE users SET notify_offline_return = 1 - notify_offline_return WHERE id = ?"
            }
            UserFlag::ShowGame => "UPDATE users SET show_game = 1 - show_game WHERE id = ?",
            UserFlag::Quiet => "UPDATE users SET quiet_enabled = 1 - quiet_enabled WHERE id = ?",
        };
        sqlx::query(sql).bind(id).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn mark_blocked(&self, id: i64, now: i64) -> Result<()> {
        sqlx::query("UPDATE users SET blocked_at = COALESCE(blocked_at, ?) WHERE id = ?")
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Removes the user with all subscriptions, comments and queued messages.
    pub async fn forget_user(&self, id: i64) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM queue WHERE chat_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM subscriptions WHERE user_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Removes a user who still blocks the bot since before `cutoff`.
    async fn forget_blocked(&self, id: i64, cutoff: i64) -> Result<bool> {
        let mut tx = self.write_tx().await?;
        let gone = sqlx::query(
            "DELETE FROM users WHERE id = ? AND blocked_at IS NOT NULL AND blocked_at < ?",
        )
        .bind(id)
        .bind(cutoff)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if gone {
            sqlx::query("DELETE FROM queue WHERE chat_id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM subscriptions WHERE user_id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(gone)
    }

    pub async fn broadcast_targets(&self) -> Result<Vec<i64>> {
        Ok(
            sqlx::query_scalar("SELECT id FROM users WHERE blocked_at IS NULL ORDER BY id")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub async fn weekly_targets(&self, now: i64) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT u.id FROM users AS u
             WHERE u.blocked_at IS NULL
               AND NOT (u.muted = 1 AND (u.mute_until IS NULL OR u.mute_until > ?))
               AND EXISTS (SELECT 1 FROM subscriptions AS s WHERE s.user_id = u.id)
             ORDER BY u.id",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?)
    }

    // Stations

    pub async fn station(&self, uuid: &str) -> Result<Option<StationRow>> {
        let row = sqlx::query("SELECT * FROM stations WHERE uuid = ?")
            .bind(uuid)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(|r| station_row(r, "")).transpose()
    }

    /// Stations that have at least one subscriber who did not block the bot,
    /// with a flag for stations marked as deleted.
    pub async fn stations_to_poll(&self) -> Result<Vec<(String, bool)>> {
        Ok(sqlx::query_as(
            "SELECT DISTINCT s.station, st.gone_at IS NOT NULL FROM subscriptions AS s
             JOIN users AS u ON u.id = s.user_id
             JOIN stations AS st ON st.uuid = s.station
             WHERE u.blocked_at IS NULL ORDER BY s.station",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// Remembers the first 404 of a streak and returns its time.
    pub async fn mark_missing(&self, uuid: &str, now: i64) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar(
            "UPDATE stations SET missing_since = COALESCE(missing_since, ?) WHERE uuid = ? RETURNING missing_since",
        )
        .bind(now)
        .bind(uuid)
        .fetch_optional(&self.pool)
        .await?
        .flatten())
    }

    /// Marks the station as deleted and queues the notices together; false when it already was.
    pub async fn mark_gone(&self, uuid: &str, now: i64, notices: &[Outgoing]) -> Result<bool> {
        let mut tx = self.write_tx().await?;
        let r = sqlx::query("UPDATE stations SET gone_at = ? WHERE uuid = ? AND gone_at IS NULL")
            .bind(now)
            .bind(uuid)
            .execute(&mut *tx)
            .await?;
        if r.rows_affected() == 0 {
            return Ok(false);
        }
        insert_msgs(&mut tx, notices, now).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Subscriptions to stations deleted before `cutoff`.
    pub async fn expired_gone_subs(&self, cutoff: i64) -> Result<Vec<SubRow>> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "{SUB_SELECT} WHERE st.gone_at IS NOT NULL AND st.gone_at < ? ORDER BY s.id"
        )))
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(sub_row).collect()
    }

    /// Removes the subscription if its station is still deleted since before `cutoff`,
    /// queueing the notice together; false when nothing was removed.
    pub async fn remove_gone_sub(
        &self,
        id: i64,
        cutoff: i64,
        notice: Option<&Outgoing>,
        now: i64,
    ) -> Result<bool> {
        let mut tx = self.write_tx().await?;
        let r = sqlx::query(
            "DELETE FROM subscriptions WHERE id = ? AND station IN
                (SELECT uuid FROM stations WHERE gone_at IS NOT NULL AND gone_at < ?)",
        )
        .bind(id)
        .bind(cutoff)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() == 0 {
            return Ok(false);
        }
        if let Some(m) = notice {
            insert_msgs(&mut tx, std::slice::from_ref(m), now).await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Stores a snapshot. `on_return` is queued in the same transaction when the station
    /// was marked as deleted.
    pub async fn apply_snapshot(
        &self,
        snap: &Snapshot,
        now: i64,
        on_return: &[Outgoing],
    ) -> Result<Applied> {
        let mut tx = self.write_tx().await?;
        let old_row = sqlx::query("SELECT * FROM stations WHERE uuid = ?")
            .bind(&snap.uuid)
            .fetch_optional(&mut *tx)
            .await?;
        let prev_state: Option<String> = old_row
            .as_ref()
            .map(|r| r.try_get("raw_state"))
            .transpose()?;
        let old = old_row.as_ref().map(|r| station_row(r, "")).transpose()?;
        if let Some(o) = &old
            && o.checked_at.is_some_and(|c| c > snap.fetched_at)
        {
            return Ok(Applied {
                new: o.clone(),
                old,
                status_changed: false,
                game_changed_while_busy: false,
                session_started: false,
                returned: false,
                stale: true,
            });
        }
        let status_changed = old.as_ref().is_none_or(|o| o.status != snap.status);
        let game_changed_while_busy = old.as_ref().is_some_and(|o| {
            o.status == crate::drova::Status::Busy
                && snap.status == crate::drova::Status::Busy
                && o.product_id.is_some()
                && snap.product_id.is_some()
                && o.product_id != snap.product_id
        });
        let session_started = snap.raw_state.eq_ignore_ascii_case("BUSY")
            && prev_state
                .as_deref()
                .is_some_and(|r| !r.eq_ignore_ascii_case("BUSY") || game_changed_while_busy);
        let status_since = match &old {
            Some(o) if !status_changed => o.status_since,
            _ => now,
        };
        sqlx::query(
            "INSERT INTO stations (uuid, name, status, raw_state, published, city, product_id, games_count,
                status_since, checked_at, description)
             VALUES (?, ?, ?, ?, COALESCE(?, 1), ?, ?, ?, ?, ?, ?)
             ON CONFLICT(uuid) DO UPDATE SET name = excluded.name, status = excluded.status,
                raw_state = excluded.raw_state, published = COALESCE(?, stations.published),
                city = excluded.city,
                product_id = COALESCE(excluded.product_id, stations.product_id),
                games_count = excluded.games_count, status_since = excluded.status_since,
                checked_at = excluded.checked_at, description = excluded.description,
                missing_since = NULL, gone_at = NULL",
        )
        .bind(&snap.uuid)
        .bind(&snap.name)
        .bind(snap.status.as_str())
        .bind(&snap.raw_state)
        .bind(snap.published)
        .bind(&snap.city)
        .bind(&snap.product_id)
        .bind(snap.games_count)
        .bind(status_since)
        .bind(snap.fetched_at)
        .bind(&snap.description)
        .bind(snap.published)
        .execute(&mut *tx)
        .await?;
        let returned = old.as_ref().is_some_and(|o| o.gone_at.is_some());
        if returned {
            insert_msgs(&mut tx, on_return, now).await?;
        }
        if status_changed || game_changed_while_busy {
            sqlx::query(
                "INSERT INTO transitions (station, from_status, to_status, product_id, at) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(&snap.uuid)
            .bind(old.as_ref().map(|o| o.status.as_str()))
            .bind(snap.status.as_str())
            .bind(&snap.product_id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        let new = sqlx::query("SELECT * FROM stations WHERE uuid = ?")
            .bind(&snap.uuid)
            .fetch_one(&mut *tx)
            .await
            .map(|r| station_row(&r, ""))??;
        tx.commit().await?;
        Ok(Applied {
            new,
            status_changed,
            game_changed_while_busy,
            session_started,
            returned,
            stale: false,
            old,
        })
    }

    // Subscriptions

    pub async fn subs_for_user(&self, user_id: i64) -> Result<Vec<SubRow>> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "{SUB_SELECT} WHERE s.user_id = ? ORDER BY s.id"
        )))
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(sub_row).collect()
    }

    pub async fn sub(&self, user_id: i64, id: i64) -> Result<Option<SubRow>> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "{SUB_SELECT} WHERE s.user_id = ? AND s.id = ?"
        )))
        .bind(user_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(sub_row).transpose()
    }

    pub async fn subs_for_station(&self, uuid: &str) -> Result<Vec<(SubRow, UserRow)>> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "{SUB_SELECT} WHERE s.station = ? ORDER BY s.id"
        )))
        .bind(uuid)
        .fetch_all(&self.pool)
        .await?;
        let subs: Vec<SubRow> = rows.iter().map(sub_row).collect::<Result<_>>()?;
        let mut out = Vec::with_capacity(subs.len());
        for s in subs {
            if let Some(u) = self.user(s.user_id).await? {
                out.push((s, u));
            }
        }
        Ok(out)
    }

    /// Adds or refreshes a subscription; the waiting state comes from the stored station.
    pub async fn add_subscription(
        &self,
        user_id: i64,
        uuid: &str,
        merchant: bool,
        limit: i64,
        now: i64,
    ) -> Result<AddResult> {
        let mut tx = self.write_tx().await?;
        let st = sqlx::query("SELECT * FROM stations WHERE uuid = ?")
            .bind(uuid)
            .fetch_optional(&mut *tx)
            .await?
            .as_ref()
            .map(|r| station_row(r, ""))
            .transpose()?
            .with_context(|| format!("station {uuid} is not stored"))?;
        let last_status = st.status;
        let pending = pending_for(merchant, st.status, st.published);
        let existing: Option<i64> =
            sqlx::query_scalar("SELECT id FROM subscriptions WHERE user_id = ? AND station = ?")
                .bind(user_id)
                .bind(uuid)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(id) = existing {
            sqlx::query("UPDATE subscriptions SET last_status = ?, pending = ? WHERE id = ?")
                .bind(last_status.as_str())
                .bind(pending)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(AddResult::Updated(id));
        }
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM subscriptions WHERE user_id = ?")
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await?;
        if count >= limit {
            return Ok(AddResult::Limit);
        }
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO subscriptions (user_id, station, last_status, pending, created_at)
             VALUES (?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(user_id)
        .bind(uuid)
        .bind(last_status.as_str())
        .bind(pending)
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(AddResult::Added(id))
    }

    pub async fn sub_count(&self, user_id: i64) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT COUNT(*) FROM subscriptions WHERE user_id = ?")
                .bind(user_id)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub async fn remove_sub(&self, user_id: i64, id: i64) -> Result<bool> {
        let r = sqlx::query("DELETE FROM subscriptions WHERE user_id = ? AND id = ?")
            .bind(user_id)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn remove_sub_by_station(&self, user_id: i64, uuid: &str) -> Result<bool> {
        let r = sqlx::query("DELETE FROM subscriptions WHERE user_id = ? AND station = ?")
            .bind(user_id)
            .bind(uuid)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn clear_subs(&self, user_id: i64) -> Result<u64> {
        let r = sqlx::query("DELETE FROM subscriptions WHERE user_id = ?")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }

    pub async fn set_comment(
        &self,
        user_id: i64,
        id: i64,
        comment: Option<&str>,
        now: i64,
    ) -> Result<bool> {
        let r = sqlx::query(
            "UPDATE subscriptions SET comment = ?, comment_at = ? WHERE user_id = ? AND id = ?",
        )
        .bind(comment)
        .bind(comment.map(|_| now))
        .bind(user_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn set_comment_by_station(
        &self,
        user_id: i64,
        uuid: &str,
        comment: &str,
        now: i64,
    ) -> Result<()> {
        sqlx::query("UPDATE subscriptions SET comment = ?, comment_at = ? WHERE user_id = ? AND station = ?")
            .bind(comment)
            .bind(now)
            .bind(user_id)
            .bind(uuid)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn toggle_sub_flag(&self, user_id: i64, id: i64, flag: SubFlag) -> Result<()> {
        let sql = match flag {
            SubFlag::Busy => {
                "UPDATE subscriptions SET notify_busy = 1 - notify_busy WHERE user_id = ? AND id = ?"
            }
            SubFlag::Offline => {
                "UPDATE subscriptions SET notify_offline = 1 - notify_offline WHERE user_id = ? AND id = ?"
            }
        };
        sqlx::query(sql)
            .bind(user_id)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Station became busy or offline: every subscriber waits for the next free moment.
    pub async fn set_station_waiting(&self, uuid: &str, status: Status) -> Result<()> {
        sqlx::query("UPDATE subscriptions SET last_status = ?, pending = 1 WHERE station = ?")
            .bind(status.as_str())
            .bind(uuid)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Other users actively waiting for each station: not muted, not blocked.
    pub async fn waiting_counts(&self, user_id: i64, now: i64) -> Result<HashMap<String, i64>> {
        let rows = sqlx::query(
            "SELECT s.station, COUNT(DISTINCT s.user_id) AS n
             FROM subscriptions AS s JOIN users AS u ON u.id = s.user_id
             WHERE s.user_id != ?
               AND u.blocked_at IS NULL
               AND NOT (u.muted = 1 AND (u.mute_until IS NULL OR u.mute_until > ?))
               AND s.station IN (SELECT station FROM subscriptions WHERE user_id = ?)
             GROUP BY s.station",
        )
        .bind(user_id)
        .bind(now)
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = HashMap::new();
        for r in rows {
            out.insert(r.try_get("station")?, r.try_get("n")?);
        }
        Ok(out)
    }

    // Transitions

    /// Transitions inside the window plus the last one before it.
    pub async fn transitions(&self, uuid: &str, start: i64, end: i64) -> Result<Vec<Transition>> {
        let rows = sqlx::query(
            "SELECT from_status, to_status, product_id, at FROM (
                SELECT id, from_status, to_status, product_id, at FROM transitions
                WHERE station = ? AND at < ? ORDER BY at DESC, id DESC LIMIT 1)
             UNION ALL
             SELECT from_status, to_status, product_id, at FROM (
                SELECT id, from_status, to_status, product_id, at FROM transitions
                WHERE station = ? AND at >= ? AND at <= ? ORDER BY at, id)",
        )
        .bind(uuid)
        .bind(start)
        .bind(uuid)
        .bind(start)
        .bind(end)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push(Transition {
                to: status(r, "to_status")?,
                product_id: r.try_get("product_id")?,
                at: r.try_get("at")?,
            });
        }
        out.sort_by_key(|t| t.at);
        Ok(out)
    }

    /// Game and length of the latest finished busy period.
    pub async fn last_session(&self, uuid: &str) -> Result<Option<(Option<String>, i64)>> {
        let rows = sqlx::query(
            "SELECT to_status, product_id, at FROM transitions WHERE station = ? ORDER BY at DESC, id DESC LIMIT 2",
        )
        .bind(uuid)
        .fetch_all(&self.pool)
        .await?;
        let (Some(last), Some(prev)) = (rows.first(), rows.get(1)) else {
            return Ok(None);
        };
        if status(prev, "to_status")? != Status::Busy || status(last, "to_status")? == Status::Busy
        {
            return Ok(None);
        }
        let start: i64 = prev.try_get("at")?;
        let end: i64 = last.try_get("at")?;
        Ok(Some((prev.try_get("product_id")?, end - start)))
    }

    // Products

    pub async fn product(&self, id: &str) -> Result<Option<(String, i64)>> {
        let row = sqlx::query("SELECT title, fetched_at FROM products WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| Ok((r.try_get("title")?, r.try_get("fetched_at")?)))
            .transpose()
    }

    pub async fn save_product(&self, id: &str, title: &str, now: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO products (id, title, fetched_at) VALUES (?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET title = excluded.title, fetched_at = excluded.fetched_at",
        )
        .bind(id)
        .bind(title)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn product_titles(&self, ids: &[String]) -> Result<HashMap<String, String>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let sql = format!(
            "SELECT id, title FROM products WHERE id IN ({})",
            in_list(ids.len())
        );
        let mut q = sqlx::query(AssertSqlSafe(sql));
        for id in ids {
            q = q.bind(id);
        }
        let mut out = HashMap::new();
        for r in q.fetch_all(&self.pool).await? {
            out.insert(r.try_get("id")?, r.try_get("title")?);
        }
        Ok(out)
    }

    // Queue

    /// Queues all messages or none.
    pub async fn enqueue_all(&self, msgs: &[Outgoing], now: i64) -> Result<()> {
        if msgs.is_empty() {
            return Ok(());
        }
        let mut tx = self.write_tx().await?;
        insert_msgs(&mut tx, msgs, now).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Queues free-station messages and marks the subscriptions as notified in one step.
    pub async fn enqueue_ready(&self, msgs: &[Outgoing], done: &[i64], now: i64) -> Result<()> {
        let mut tx = self.write_tx().await?;
        insert_msgs(&mut tx, msgs, now).await?;
        if !done.is_empty() {
            let sql = format!(
                "UPDATE subscriptions SET last_status = 'ready', pending = 0 WHERE id IN ({})",
                in_list(done.len())
            );
            let mut q = sqlx::query(AssertSqlSafe(sql));
            for id in done {
                q = q.bind(id);
            }
            q.execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn is_pending(&self, id: i64) -> Result<bool> {
        Ok(
            sqlx::query_scalar::<_, i64>("SELECT 1 FROM queue WHERE id = ? AND state = 'pending'")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?
                .is_some(),
        )
    }

    pub async fn due(&self, now: i64, limit: i64) -> Result<Vec<QueueRow>> {
        let rows = sqlx::query(
            "SELECT id, kind, chat_id, text, keyboard, attempts FROM queue
             WHERE state = 'pending' AND not_before <= ? ORDER BY id LIMIT ?",
        )
        .bind(now)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(QueueRow {
                    id: r.try_get("id")?,
                    kind: r.try_get("kind")?,
                    chat_id: r.try_get("chat_id")?,
                    text: r.try_get("text")?,
                    keyboard: r.try_get("keyboard")?,
                    attempts: r.try_get("attempts")?,
                })
            })
            .collect()
    }

    pub async fn mark_sent(&self, id: i64, message_id: i64, now: i64) -> Result<()> {
        sqlx::query("UPDATE queue SET state = 'sent', message_id = ?, done_at = ? WHERE id = ?")
            .bind(message_id)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn mark_dropped(&self, id: i64, reason: &str, now: i64) -> Result<()> {
        sqlx::query("UPDATE queue SET state = 'dropped', last_error = ?, done_at = ? WHERE id = ?")
            .bind(reason)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn mark_retry(&self, id: i64, not_before: i64, error: &str) -> Result<()> {
        sqlx::query(
            "UPDATE queue SET attempts = attempts + 1, not_before = ?, last_error = ? WHERE id = ?",
        )
        .bind(not_before)
        .bind(crate::util::clip(error, 500))
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Drops undelivered messages of a kind that became useless after `max_age` seconds.
    pub async fn drop_stale(&self, kinds: &[&str], max_age: i64, now: i64) -> Result<u64> {
        let sql = format!(
            "UPDATE queue SET state = 'dropped', last_error = 'stale', done_at = ?
             WHERE state = 'pending' AND created_at <= ? AND kind IN ({})",
            in_list(kinds.len())
        );
        let mut q = sqlx::query(AssertSqlSafe(sql))
            .bind(now)
            .bind(now - max_age);
        for k in kinds {
            q = q.bind(*k);
        }
        Ok(q.execute(&self.pool).await?.rows_affected())
    }

    pub async fn drop_pending_for_chat(&self, chat_id: i64, now: i64) -> Result<()> {
        sqlx::query(
            "UPDATE queue SET state = 'dropped', last_error = 'blocked', done_at = ?
             WHERE state = 'pending' AND chat_id = ? AND kind != 'admin'",
        )
        .bind(now)
        .bind(chat_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Free-station messages for this station from the last `window` seconds.
    /// Undelivered ones are dropped; delivered ones that still need the "taken" edit are returned.
    pub async fn recent_free_messages(
        &self,
        uuid: &str,
        window: i64,
        now: i64,
    ) -> Result<Vec<(i64, i64, i64, String)>> {
        sqlx::query(
            "UPDATE queue SET state = 'dropped', last_error = 'taken before delivery', done_at = ?
             WHERE station = ? AND kind IN ('ready', 'merchant_ready') AND state = 'pending'",
        )
        .bind(now)
        .bind(uuid)
        .execute(&self.pool)
        .await?;
        let rows = sqlx::query(
            "SELECT id, chat_id, message_id, text FROM queue
             WHERE station = ? AND kind IN ('ready', 'merchant_ready') AND state = 'sent'
               AND taken_edited = 0 AND message_id IS NOT NULL AND done_at >= ?",
        )
        .bind(uuid)
        .bind(now - window)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok((
                    r.try_get("id")?,
                    r.try_get("chat_id")?,
                    r.try_get("message_id")?,
                    r.try_get("text")?,
                ))
            })
            .collect()
    }

    pub async fn mark_taken_edited(&self, id: i64) -> Result<()> {
        sqlx::query("UPDATE queue SET taken_edited = 1 WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn sent_text(&self, chat_id: i64, message_id: i64) -> Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT text FROM queue WHERE chat_id = ? AND message_id = ? ORDER BY id DESC LIMIT 1",
        )
        .bind(chat_id)
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    // Campaigns

    pub async fn campaign_exists(&self, kind: &str, due_at: i64) -> Result<bool> {
        Ok(
            sqlx::query_scalar::<_, i64>("SELECT 1 FROM campaigns WHERE kind = ? AND due_at = ?")
                .bind(kind)
                .bind(due_at)
                .fetch_optional(&self.pool)
                .await?
                .is_some(),
        )
    }

    /// Records the campaign and queues its messages together; false when it already ran.
    pub async fn start_campaign(
        &self,
        kind: &str,
        due_at: i64,
        now: i64,
        msgs: &[Outgoing],
    ) -> Result<bool> {
        let mut tx = self.write_tx().await?;
        let r = sqlx::query(
            "INSERT OR IGNORE INTO campaigns (kind, due_at, created_at) VALUES (?, ?, ?)",
        )
        .bind(kind)
        .bind(due_at)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() == 0 {
            return Ok(false);
        }
        insert_msgs(&mut tx, msgs, now).await?;
        tx.commit().await?;
        Ok(true)
    }

    // Poll errors and stats

    pub async fn record_poll_error(&self, uuid: &str, error: &str, now: i64) -> Result<()> {
        sqlx::query("INSERT INTO poll_errors (station, error, at) VALUES (?, ?, ?)")
            .bind(uuid)
            .bind(crate::util::clip(error, 500))
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn recent_errors(&self, limit: i64) -> Result<Vec<(i64, String, String)>> {
        let rows = sqlx::query(
            "SELECT e.at, COALESCE(st.name, e.station) AS name, e.error FROM poll_errors AS e
             LEFT JOIN stations AS st ON st.uuid = e.station ORDER BY e.id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| Ok((r.try_get("at")?, r.try_get("name")?, r.try_get("error")?)))
            .collect()
    }

    pub async fn stats(&self, now: i64) -> Result<Stats> {
        let r = sqlx::query(
            "SELECT
                (SELECT COUNT(*) FROM users) AS users,
                (SELECT COUNT(DISTINCT s.user_id) FROM subscriptions AS s JOIN users AS u ON u.id = s.user_id
                    WHERE u.blocked_at IS NULL) AS active_users,
                (SELECT COUNT(*) FROM users WHERE blocked_at IS NOT NULL) AS blocked_users,
                (SELECT COUNT(*) FROM users WHERE is_merchant = 1) AS merchants,
                (SELECT COUNT(*) FROM users WHERE muted = 1 AND (mute_until IS NULL OR mute_until > ?)) AS muted,
                (SELECT COUNT(*) FROM subscriptions) AS subscriptions,
                (SELECT COUNT(DISTINCT station) FROM subscriptions) AS stations,
                (SELECT COUNT(*) FROM queue WHERE state = 'pending') AS queue_pending,
                (SELECT COUNT(*) FROM poll_errors WHERE at > ?) AS errors_24h",
        )
        .bind(now)
        .bind(now - 86_400)
        .fetch_one(&self.pool)
        .await?;
        Ok(Stats {
            users: r.try_get("users")?,
            active_users: r.try_get("active_users")?,
            blocked_users: r.try_get("blocked_users")?,
            merchants: r.try_get("merchants")?,
            muted: r.try_get("muted")?,
            subscriptions: r.try_get("subscriptions")?,
            stations: r.try_get("stations")?,
            queue_pending: r.try_get("queue_pending")?,
            errors_24h: r.try_get("errors_24h")?,
        })
    }

    pub async fn prune(&self, now: i64) -> Result<Pruned> {
        let day = 86_400;
        let queue = sqlx::query("DELETE FROM queue WHERE state != 'pending' AND done_at < ?")
            .bind(now - QUEUE_KEEP_DAYS * day)
            .execute(&self.pool)
            .await?
            .rows_affected();
        let cutoff = now - TRANSITIONS_KEEP_DAYS * day;
        let transitions = sqlx::query(
            "DELETE FROM transitions WHERE at < ? AND id NOT IN (
                SELECT t.id FROM transitions AS t
                WHERE t.at < ? AND t.id = (SELECT t2.id FROM transitions AS t2
                    WHERE t2.station = t.station AND t2.at < ? ORDER BY t2.at DESC, t2.id DESC LIMIT 1))",
        )
        .bind(cutoff)
        .bind(cutoff)
        .bind(cutoff)
        .execute(&self.pool)
        .await?
        .rows_affected();
        let poll_errors = sqlx::query(
            "DELETE FROM poll_errors WHERE id NOT IN (SELECT id FROM poll_errors ORDER BY id DESC LIMIT ?)",
        )
        .bind(POLL_ERRORS_KEEP)
        .execute(&self.pool)
        .await?
        .rows_affected();
        let blocked: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM users WHERE blocked_at IS NOT NULL AND blocked_at < ?",
        )
        .bind(now - BLOCKED_KEEP_DAYS * day)
        .fetch_all(&self.pool)
        .await?;
        let mut users = 0;
        for id in &blocked {
            if self
                .forget_blocked(*id, now - BLOCKED_KEEP_DAYS * day)
                .await?
            {
                users += 1;
            }
        }
        let old_stations: Vec<String> = sqlx::query_scalar(
            "SELECT uuid FROM stations WHERE uuid NOT IN (SELECT station FROM subscriptions)
             AND COALESCE(checked_at, status_since) < ?",
        )
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await?;
        for uuid in &old_stations {
            let mut tx = self.pool.begin().await?;
            sqlx::query("DELETE FROM transitions WHERE station = ?")
                .bind(uuid)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM stations WHERE uuid = ?")
                .bind(uuid)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
        Ok(Pruned {
            queue,
            transitions,
            poll_errors,
            users,
            stations: old_stations.len() as u64,
        })
    }
}

pub async fn connect(path: &Path, create: bool) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str("sqlite:")?
        .filename(path)
        .create_if_missing(create)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(10));
    SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await
        .with_context(|| format!("cannot open database {}", path.display()))
}

async fn user_version(pool: &SqlitePool) -> Result<i64> {
    Ok(sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?)
}

async fn refuse_legacy(pool: &SqlitePool) -> Result<()> {
    let legacy: Option<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'monitoring'",
    )
    .fetch_optional(pool)
    .await?;
    if legacy.is_some() && user_version(pool).await? == 0 {
        bail!(
            "this is a database of the old Python bot; convert it first with `drova-bot migrate-legacy <old> <new>`"
        );
    }
    Ok(())
}

async fn migrate(pool: &SqlitePool, path: Option<&Path>) -> Result<()> {
    let current = user_version(pool).await?;
    let target = MIGRATIONS.len() as i64;
    if current > target {
        bail!("database schema version {current} is newer than this program supports ({target})");
    }
    if current == target {
        return Ok(());
    }
    if current > 0
        && let Some(path) = path
    {
        let backup = path.with_extension(format!("pre-v{target}.sqlite3"));
        if !backup.exists() {
            sqlx::query("VACUUM INTO ?")
                .bind(backup.to_string_lossy().as_ref())
                .execute(pool)
                .await
                .context("backup before migration")?;
            tracing::info!(path = %backup.display(), "database backup made before migration");
        }
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let version = index as i64 + 1;
        let mut tx = pool.begin().await?;
        sqlx::raw_sql(AssertSqlSafe(*sql)).execute(&mut *tx).await?;
        sqlx::raw_sql(AssertSqlSafe(format!("PRAGMA user_version = {version}")))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        tracing::info!(version, "database migrated");
    }
    Ok(())
}

#[cfg(test)]
pub async fn memory() -> Db {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    Db::from_pool(pool).await.unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drova::Snapshot;

    fn snap(status: Status, product: Option<&str>) -> Snapshot {
        snap_raw(status, product, "X")
    }

    fn snap_raw(status: Status, product: Option<&str>, raw: &str) -> Snapshot {
        Snapshot {
            uuid: "05225c83-dd3b-4c12-832b-48227262f960".into(),
            name: "PC".into(),
            raw_state: raw.into(),
            status,
            known_state: true,
            published: Some(true),
            city: None,
            product_id: product.map(Into::into),
            games_count: Some(3),
            description: Some("<p>hi</p>".into()),
            fetched_at: 0,
        }
    }

    fn at(mut s: Snapshot, t: i64) -> Snapshot {
        s.fetched_at = t;
        s
    }

    fn msg(chat: i64, kind: &'static str) -> Outgoing {
        Outgoing::new(kind, chat, None, "x", None).unwrap()
    }

    async fn queued(db: &Db, kind: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM queue WHERE kind = ? AND state = 'pending'")
            .bind(kind)
            .fetch_one(db.pool())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn snapshots_make_transitions() {
        let db = memory().await;
        let a = db
            .apply_snapshot(&at(snap(Status::Ready, None), 100), 100, &[])
            .await
            .unwrap();
        assert!(a.status_changed && a.old.is_none());
        let a = db
            .apply_snapshot(&at(snap(Status::Ready, None), 130), 130, &[])
            .await
            .unwrap();
        assert!(!a.status_changed);
        assert_eq!(a.new.status_since, 100);
        db.apply_snapshot(&at(snap(Status::Busy, Some("g1")), 200), 200, &[])
            .await
            .unwrap();
        let a = db
            .apply_snapshot(&at(snap(Status::Busy, Some("g2")), 260), 260, &[])
            .await
            .unwrap();
        assert!(a.game_changed_while_busy && !a.status_changed);
        db.apply_snapshot(&at(snap(Status::Ready, Some("g2")), 400), 400, &[])
            .await
            .unwrap();
        let t = db.transitions(&a.new.uuid, 150, 1000).await.unwrap();
        assert_eq!(t.len(), 4);
        assert_eq!(t[0].at, 100);
        let (game, len) = db.last_session(&a.new.uuid).await.unwrap().unwrap();
        assert_eq!(game.as_deref(), Some("g2"));
        assert_eq!(len, 140);
    }

    #[tokio::test]
    async fn session_start_and_unknown_old_game() {
        let db = memory().await;
        let apply = |s: Snapshot, t: i64| {
            let db = db.clone();
            async move { db.apply_snapshot(&at(s, t), t, &[]).await.unwrap() }
        };
        apply(snap_raw(Status::Busy, None, "BUSY"), 10).await;
        let a = apply(snap_raw(Status::Busy, Some("g1"), "BUSY"), 20).await;
        assert!(!a.game_changed_while_busy && !a.session_started);
        let a = apply(snap_raw(Status::Ready, Some("g1"), "LISTEN"), 30).await;
        assert!(!a.session_started);
        let a = apply(snap_raw(Status::Busy, Some("g2"), "HANDSHAKE"), 40).await;
        assert!(a.status_changed && !a.session_started);
        let a = apply(snap_raw(Status::Busy, Some("g2"), "BUSY"), 50).await;
        assert!(!a.status_changed && a.session_started);
        let a = apply(snap_raw(Status::Busy, Some("g2"), "BUSY"), 60).await;
        assert!(!a.session_started);
        let a = apply(snap_raw(Status::Busy, Some("g3"), "BUSY"), 70).await;
        assert!(a.game_changed_while_busy && a.session_started);
    }

    #[tokio::test]
    async fn gone_station_lifecycle() {
        let db = memory().await;
        let uuid = "05225c83-dd3b-4c12-832b-48227262f960";
        db.touch_user(1, 0).await.unwrap();
        db.apply_snapshot(&at(snap(Status::Ready, None), 0), 0, &[])
            .await
            .unwrap();
        db.add_subscription(1, uuid, false, 5, 0).await.unwrap();
        assert_eq!(db.mark_missing(uuid, 100).await.unwrap(), Some(100));
        assert_eq!(db.mark_missing(uuid, 200).await.unwrap(), Some(100));
        assert!(db.mark_gone(uuid, 4_000, &[msg(1, "gone")]).await.unwrap());
        assert!(!db.mark_gone(uuid, 4_100, &[msg(1, "gone")]).await.unwrap());
        assert_eq!(queued(&db, "gone").await, 1);
        assert_eq!(
            db.stations_to_poll().await.unwrap(),
            vec![(uuid.to_owned(), true)]
        );
        let sub = &db.subs_for_user(1).await.unwrap()[0];
        assert_eq!(sub.station.gone_at, Some(4_000));
        assert_eq!(sub.station.description.as_deref(), Some("<p>hi</p>"));
        let a = db
            .apply_snapshot(
                &at(snap(Status::Ready, None), 5_000),
                5_000,
                &[msg(1, "returned")],
            )
            .await
            .unwrap();
        assert!(a.returned && a.new.gone_at.is_none() && a.new.missing_since.is_none());
        assert_eq!(queued(&db, "returned").await, 1);
        db.mark_missing(uuid, 6_000).await.unwrap();
        db.mark_gone(uuid, 7_000, &[]).await.unwrap();
        assert!(db.expired_gone_subs(7_000).await.unwrap().is_empty());
        let subs = db.expired_gone_subs(7_001).await.unwrap();
        assert_eq!(subs.len(), 1);
        db.apply_snapshot(&at(snap(Status::Ready, None), 7_500), 7_500, &[])
            .await
            .unwrap();
        let notice = msg(1, "gone_unsub");
        assert!(
            !db.remove_gone_sub(subs[0].id, 7_001, Some(&notice), 7_600)
                .await
                .unwrap()
        );
        db.mark_gone(uuid, 7_000, &[]).await.unwrap();
        assert!(
            db.remove_gone_sub(subs[0].id, 7_001, Some(&notice), 7_600)
                .await
                .unwrap()
        );
        assert_eq!(queued(&db, "gone_unsub").await, 1);
        assert!(db.subs_for_user(1).await.unwrap().is_empty());
        assert_eq!(db.stations_to_poll().await.unwrap(), Vec::new());
    }

    #[tokio::test]
    async fn subscription_limit_and_views() {
        let db = memory().await;
        db.touch_user(1, 0).await.unwrap();
        db.apply_snapshot(&at(snap(Status::Busy, None), 0), 0, &[])
            .await
            .unwrap();
        let uuid = "05225c83-dd3b-4c12-832b-48227262f960";
        let r = db.add_subscription(1, uuid, false, 1, 0).await.unwrap();
        assert!(matches!(r, AddResult::Added(_)));
        let r = db.add_subscription(1, uuid, false, 1, 0).await.unwrap();
        assert!(matches!(r, AddResult::Updated(_)));
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM monitoring WHERE url LIKE 'https://drova.io/stations/%'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(n, 1);
        let subs = db.subs_for_user(1).await.unwrap();
        db.set_comment(1, subs[0].id, Some("hi"), 5).await.unwrap();
        let c: String = sqlx::query_scalar("SELECT comment FROM server_comments")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(c, "hi");
        db.forget_user(1).await.unwrap();
        assert!(db.subs_for_user(1).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn prune_keeps_last_transition_before_cutoff() {
        let db = memory().await;
        let day = 86_400;
        let now = 200 * day;
        db.apply_snapshot(&at(snap(Status::Ready, None), 10 * day), 10 * day, &[])
            .await
            .unwrap();
        db.apply_snapshot(&at(snap(Status::Busy, None), 20 * day), 20 * day, &[])
            .await
            .unwrap();
        db.apply_snapshot(&at(snap(Status::Ready, None), 190 * day), 190 * day, &[])
            .await
            .unwrap();
        db.touch_user(1, 0).await.unwrap();
        db.add_subscription(1, "05225c83-dd3b-4c12-832b-48227262f960", false, 5, 0)
            .await
            .unwrap();
        let p = db.prune(now).await.unwrap();
        assert_eq!(p.transitions, 1);
        let t = db
            .transitions("05225c83-dd3b-4c12-832b-48227262f960", now - 50 * day, now)
            .await
            .unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].to, Status::Busy);
    }

    #[tokio::test]
    async fn stale_snapshot_and_missing_flag() {
        let db = memory().await;
        db.apply_snapshot(&at(snap(Status::Busy, None), 100), 100, &[])
            .await
            .unwrap();
        let a = db
            .apply_snapshot(&at(snap(Status::Ready, None), 90), 110, &[])
            .await
            .unwrap();
        assert!(a.stale);
        assert_eq!(a.new.status, Status::Busy);
        let mut hidden = at(snap(Status::Busy, None), 120);
        hidden.published = Some(false);
        db.apply_snapshot(&hidden, 120, &[]).await.unwrap();
        let mut unknown = at(snap(Status::Busy, None), 130);
        unknown.published = None;
        let a = db.apply_snapshot(&unknown, 130, &[]).await.unwrap();
        assert!(!a.stale && !a.new.published);
    }

    #[tokio::test]
    async fn waiting_state_comes_from_station() {
        let db = memory().await;
        let uuid = "05225c83-dd3b-4c12-832b-48227262f960";
        db.touch_user(1, 0).await.unwrap();
        db.apply_snapshot(&at(snap(Status::Ready, None), 10), 10, &[])
            .await
            .unwrap();
        db.add_subscription(1, uuid, false, 5, 0).await.unwrap();
        assert!(!db.subs_for_user(1).await.unwrap()[0].pending);
        db.apply_snapshot(&at(snap(Status::Busy, None), 20), 20, &[])
            .await
            .unwrap();
        db.add_subscription(1, uuid, false, 5, 0).await.unwrap();
        let sub = db.subs_for_user(1).await.unwrap().remove(0);
        assert!(sub.pending && sub.last_status == Status::Busy);
        db.enqueue_ready(&[msg(1, "ready")], &[sub.id], 30)
            .await
            .unwrap();
        assert!(!db.subs_for_user(1).await.unwrap()[0].pending);
        assert_eq!(queued(&db, "ready").await, 1);
    }

    #[tokio::test]
    async fn mute_end_and_blocked_prune_check_state() {
        let db = memory().await;
        db.touch_user(1, 0).await.unwrap();
        db.set_mute(1, Some(100)).await.unwrap();
        assert_eq!(db.expired_mutes(50).await.unwrap(), Vec::<i64>::new());
        assert_eq!(db.expired_mutes(100).await.unwrap(), vec![1]);
        let report = msg(1, "unmute_report");
        assert!(db.end_mute(1, 100, Some(&report)).await.unwrap());
        assert!(!db.end_mute(1, 100, Some(&report)).await.unwrap());
        assert_eq!(queued(&db, "unmute_report").await, 1);
        db.mark_blocked(1, 10).await.unwrap();
        db.touch_user(1, 20).await.unwrap();
        assert!(!db.forget_blocked(1, 1_000).await.unwrap());
        db.mark_blocked(1, 30).await.unwrap();
        assert!(db.forget_blocked(1, 1_000).await.unwrap());
        assert!(db.user(1).await.unwrap().is_none());
    }
}
