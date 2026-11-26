//! One-time import of the database of the old Python bot.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use crate::db::{Db, TRANSITIONS_KEEP_DAYS};
use crate::drova::Status;
use crate::util::{now, parse_station_link};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub users: i64,
    pub stations: i64,
    pub subscriptions: i64,
    pub comments: i64,
    pub transitions: i64,
    pub campaigns: i64,
    pub skipped_subscriptions: i64,
}

/// Parses the timestamps the old bot wrote: ISO 8601 with offset or `YYYY-MM-DD HH:MM:SS` in UTC.
pub fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(ts) = s.parse::<jiff::Timestamp>() {
        return Some(ts.as_second());
    }
    let dt: jiff::civil::DateTime = s.replace(' ', "T").parse().ok()?;
    dt.to_zoned_datetime(jiff::tz::TimeZone::UTC)
        .ok()
        .map(|z| z.timestamp().as_second())
}

pub async fn migrate(src: &Path, dst: &Path) -> Result<Report> {
    if !src.is_file() {
        bail!("source database {} does not exist", src.display());
    }
    if dst.exists() {
        bail!(
            "target {} already exists, refusing to overwrite",
            dst.display()
        );
    }
    let tmp = dst.with_extension("sqlite3.tmp");
    for p in [
        tmp.clone(),
        tmp.with_extension("tmp-wal"),
        tmp.with_extension("tmp-shm"),
    ] {
        if p.exists() {
            std::fs::remove_file(&p).with_context(|| format!("cannot remove {}", p.display()))?;
        }
    }

    let src_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(src)
                .read_only(true)
                .create_if_missing(false),
        )
        .await
        .with_context(|| format!("cannot open {}", src.display()))?;
    let is_legacy: Option<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'monitoring'",
    )
    .fetch_optional(&src_pool)
    .await?;
    if is_legacy.is_none() {
        bail!("{} is not a database of the old bot", src.display());
    }

    let dst_pool = crate::db::connect(&tmp, true).await?;
    let db = Db::from_pool(dst_pool).await?;
    let report = copy(&src_pool, &db).await;
    src_pool.close().await;
    let report = match report {
        Ok(r) => r,
        Err(e) => {
            db.close().await;
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    let ok: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(db.pool())
        .await?;
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(db.pool())
        .await?;
    db.close().await;
    if ok != "ok" {
        bail!("integrity check of the new database failed: {ok}");
    }
    std::fs::rename(&tmp, dst)
        .with_context(|| format!("cannot move result to {}", dst.display()))?;
    for ext in ["sqlite3.tmp-wal", "sqlite3.tmp-shm"] {
        let _ = std::fs::remove_file(dst.with_extension(ext));
    }
    Ok(report)
}

async fn copy(src: &sqlx::SqlitePool, db: &Db) -> Result<Report> {
    let now = now();
    let mut report = Report::default();
    let mut tx = db.pool().begin().await?;

    let mut users: HashSet<i64> = HashSet::new();
    let settings_cols: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('user_settings')")
            .fetch_all(src)
            .await?;
    let has = |c: &str| settings_cols.iter().any(|x| x == c);
    if !settings_cols.is_empty() {
        let sql = format!(
            "SELECT user_id, muted, {}, {}, {} FROM user_settings",
            if has("mute_until") {
                "mute_until"
            } else {
                "NULL AS mute_until"
            },
            if has("is_merchant") {
                "is_merchant"
            } else {
                "0 AS is_merchant"
            },
            if has("role_selected") {
                "role_selected"
            } else {
                "0 AS role_selected"
            },
        );
        for r in sqlx::query(sqlx::AssertSqlSafe(sql)).fetch_all(src).await? {
            let id: i64 = r.try_get("user_id")?;
            let muted: i64 = r.try_get("muted")?;
            let until_raw: Option<String> = r.try_get("mute_until")?;
            let until = until_raw.as_deref().and_then(parse_time);
            let muted = muted == 1 && (until_raw.is_none() || until.is_some_and(|u| u > now));
            sqlx::query(
                "INSERT INTO users (id, is_merchant, role_selected, muted, mute_until, created_at, seen_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(r.try_get::<i64, _>("is_merchant")? == 1)
            .bind(r.try_get::<i64, _>("role_selected")? == 1)
            .bind(muted)
            .bind(if muted { until } else { None })
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            users.insert(id);
        }
    }

    let monitoring = sqlx::query(
        "SELECT id, user_id, url, server_name, last_status, pending_notify FROM monitoring ORDER BY id",
    )
    .fetch_all(src)
    .await?;

    let mut latest: HashMap<String, (String, Option<String>, String)> = HashMap::new();
    for r in sqlx::query(
        "SELECT url, from_status, to_status, changed_at FROM status_transitions ORDER BY id",
    )
    .fetch_all(src)
    .await
    .unwrap_or_default()
    {
        latest.insert(
            r.try_get("url")?,
            (
                r.try_get("to_status")?,
                r.try_get("from_status")?,
                r.try_get("changed_at")?,
            ),
        );
    }

    let mut stations: HashSet<String> = HashSet::new();
    let comments: HashMap<(i64, String), (String, Option<String>)> =
        sqlx::query("SELECT user_id, url, comment, updated_at FROM server_comments")
            .fetch_all(src)
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(|r| {
                Some((
                    (r.try_get("user_id").ok()?, r.try_get("url").ok()?),
                    (r.try_get("comment").ok()?, r.try_get("updated_at").ok()?),
                ))
            })
            .collect();

    for r in &monitoring {
        let url: String = r.try_get("url")?;
        let Some(uuid) = parse_station_link(&url) else {
            report.skipped_subscriptions += 1;
            continue;
        };
        let user_id: i64 = r.try_get("user_id")?;
        let last_status: String = r.try_get("last_status")?;
        let last = Status::parse(&last_status).unwrap_or(Status::Busy);
        if !stations.contains(&uuid) {
            let name: String = r.try_get("server_name")?;
            let (status, since) = match latest.get(&url) {
                Some((to, _, at)) => (
                    Status::parse(to).unwrap_or(last),
                    parse_time(at).unwrap_or(now),
                ),
                None => (last, now),
            };
            sqlx::query(
                "INSERT INTO stations (uuid, name, status, status_since) VALUES (?, ?, ?, ?)",
            )
            .bind(&uuid)
            .bind(&name)
            .bind(status.as_str())
            .bind(since)
            .execute(&mut *tx)
            .await?;
            stations.insert(uuid.clone());
        }
        if !users.contains(&user_id) {
            sqlx::query("INSERT INTO users (id, created_at, seen_at) VALUES (?, ?, ?)")
                .bind(user_id)
                .bind(now)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            users.insert(user_id);
        }
        let comment = comments.get(&(user_id, url.clone()));
        let pending: i64 = r.try_get("pending_notify")?;
        sqlx::query(
            "INSERT OR IGNORE INTO subscriptions (id, user_id, station, last_status, pending, comment, comment_at, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(r.try_get::<i64, _>("id")?)
        .bind(user_id)
        .bind(&uuid)
        .bind(last.as_str())
        .bind(pending == 1 || last != Status::Ready)
        .bind(comment.map(|c| c.0.as_str()))
        .bind(comment.map(|c| c.1.as_deref().and_then(parse_time).unwrap_or(now)))
        .bind(now)
        .execute(&mut *tx)
        .await?;
        report.subscriptions += 1;
        if comment.is_some() {
            report.comments += 1;
        }
    }

    let cutoff = now - TRANSITIONS_KEEP_DAYS * 86_400;
    let rows = sqlx::query(
        "SELECT url, from_status, to_status, changed_at FROM status_transitions ORDER BY id",
    )
    .fetch_all(src)
    .await
    .unwrap_or_default();
    let mut last_before: HashMap<String, (Option<String>, String, i64)> = HashMap::new();
    let mut inside: Vec<(String, Option<String>, String, i64)> = Vec::new();
    for r in rows {
        let url: String = r.try_get("url")?;
        let Some(uuid) = parse_station_link(&url) else {
            continue;
        };
        if !stations.contains(&uuid) {
            continue;
        }
        let changed: String = r.try_get("changed_at")?;
        let Some(at) = parse_time(&changed) else {
            continue;
        };
        let from: Option<String> = r.try_get("from_status")?;
        let to: String = r.try_get("to_status")?;
        if at < cutoff {
            last_before.insert(uuid, (from, to, at));
        } else {
            inside.push((uuid, from, to, at));
        }
    }
    let mut all: Vec<(String, Option<String>, String, i64)> = last_before
        .into_iter()
        .map(|(u, (f, t, a))| (u, f, t, a))
        .chain(inside)
        .collect();
    all.sort_by_key(|x| x.3);
    for (uuid, from, to, at) in all {
        sqlx::query(
            "INSERT INTO transitions (station, from_status, to_status, at) VALUES (?, ?, ?, ?)",
        )
        .bind(uuid)
        .bind(from.filter(|f| Status::parse(f).is_some()))
        .bind(to)
        .bind(at)
        .execute(&mut *tx)
        .await?;
        report.transitions += 1;
    }

    for r in sqlx::query("SELECT kind, due_at FROM notification_campaigns")
        .fetch_all(src)
        .await
        .unwrap_or_default()
    {
        let kind: String = r.try_get("kind")?;
        let due: String = r.try_get("due_at")?;
        if let Some(due) = parse_time(&due) {
            sqlx::query(
                "INSERT OR IGNORE INTO campaigns (kind, due_at, created_at) VALUES (?, ?, ?)",
            )
            .bind(if kind == "weekly_donate" {
                "weekly"
            } else {
                kind.as_str()
            })
            .bind(due)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            report.campaigns += 1;
        }
    }

    tx.commit().await?;
    report.users = users.len() as i64;
    report.stations = stations.len() as i64;

    let source_subs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM monitoring")
        .fetch_one(src)
        .await?;
    let new_subs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM subscriptions")
        .fetch_one(db.pool())
        .await?;
    if new_subs + report.skipped_subscriptions != source_subs {
        bail!("subscription count mismatch: source {source_subs}, imported {new_subs}");
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_old_timestamps() {
        assert_eq!(parse_time("1970-01-01 00:01:00"), Some(60));
        assert_eq!(parse_time("1970-01-01T03:00:00+03:00"), Some(0));
        assert_eq!(parse_time("1970-01-01T00:00:01.952541+00:00"), Some(1));
        assert_eq!(parse_time("garbage"), None);
    }

    #[tokio::test]
    async fn imports_old_schema() {
        let dir = std::env::temp_dir().join(format!("drova-legacy-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("old.sqlite3");
        let dst = dir.join("new.sqlite3");
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
        let pool = crate::db::connect(&src, true).await.unwrap();
        sqlx::raw_sql(
            "CREATE TABLE monitoring (id INTEGER PRIMARY KEY AUTOINCREMENT, user_id BIGINT NOT NULL, url TEXT NOT NULL,
                server_name TEXT NOT NULL, last_status VARCHAR(10) NOT NULL, pending_notify INTEGER NOT NULL DEFAULT 1);
             CREATE TABLE user_settings (user_id BIGINT PRIMARY KEY, muted INTEGER NOT NULL DEFAULT 0,
                mute_until TEXT DEFAULT NULL, is_merchant INTEGER NOT NULL DEFAULT 0, role_selected INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE status_transitions (id INTEGER PRIMARY KEY AUTOINCREMENT, url TEXT NOT NULL, server_name TEXT NOT NULL,
                from_status VARCHAR(10), to_status VARCHAR(10) NOT NULL, changed_at TEXT NOT NULL);
             CREATE TABLE server_comments (id INTEGER PRIMARY KEY AUTOINCREMENT, user_id BIGINT NOT NULL, url TEXT NOT NULL,
                comment TEXT NOT NULL, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
             CREATE TABLE notification_campaigns (kind TEXT NOT NULL, due_at TEXT NOT NULL, created_at TEXT, PRIMARY KEY (kind, due_at));
             INSERT INTO monitoring VALUES (7, 10, 'https://drova.io/stations/05225c83-dd3b-4c12-832b-48227262f960', 'A', 'busy', 1);
             INSERT INTO monitoring VALUES (9, 11, 'https://drova.io/stations/05225c83-dd3b-4c12-832b-48227262f960', 'A', 'ready', 0);
             INSERT INTO monitoring VALUES (12, 11, 'https://drova.io/stations/00009bd4-fbe3-4b1a-b14a-df438f5f0018', 'B', 'offline', 1);
             INSERT INTO user_settings VALUES (11, 1, NULL, 1, 1);
             INSERT INTO user_settings VALUES (20, 1, '2000-01-01T00:00:00+00:00', 0, 0);
             INSERT INTO server_comments (user_id, url, comment) VALUES (11, 'https://drova.io/stations/00009bd4-fbe3-4b1a-b14a-df438f5f0018', 'мой');
             INSERT INTO status_transitions (url, server_name, from_status, to_status, changed_at)
                VALUES ('https://drova.io/stations/05225c83-dd3b-4c12-832b-48227262f960', 'A', NULL, 'busy', '2999-01-01T00:00:00+00:00');
             INSERT INTO notification_campaigns VALUES ('weekly_donate', '2026-11-23T16:00:00+00:00', NULL);",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let report = migrate(&src, &dst).await.unwrap();
        assert_eq!(report.users, 3);
        assert_eq!(report.stations, 2);
        assert_eq!(report.subscriptions, 3);
        assert_eq!(report.comments, 1);
        assert_eq!(report.campaigns, 1);
        assert!(migrate(&src, &dst).await.is_err());

        let db = Db::open(&dst).await.unwrap();
        let u = db.user(11).await.unwrap().unwrap();
        assert!(u.is_merchant && u.muted && u.mute_until.is_none());
        let u = db.user(20).await.unwrap().unwrap();
        assert!(!u.muted);
        let subs = db.subs_for_user(11).await.unwrap();
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].id, 9);
        assert_eq!(subs[1].comment.as_deref(), Some("мой"));
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_settings WHERE is_merchant = 1")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(n, 1);
        db.close().await;
        assert!(Db::open(&src).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
