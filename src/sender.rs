use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::app::App;
use crate::db::UserRow;
use crate::telegram::{Inline, Markup, TgError};
use crate::tz;
use crate::util::now;

const BATCH: i64 = 50;
const PER_CHAT_GAP: Duration = Duration::from_millis(1100);

/// Messages in quiet hours arrive without sound.
pub fn quiet_now(u: &UserRow, at: i64) -> bool {
    u.quiet_enabled
        && tz::in_window(
            tz::local_minutes(&tz::resolve(&u.tz), at),
            u.quiet_start,
            u.quiet_end,
        )
}

fn retry_delay(attempts: i64) -> i64 {
    (60 * (attempts + 1)).min(3600)
}

pub async fn run(app: Arc<App>, mut shutdown: watch::Receiver<bool>) {
    let mut last_sent: HashMap<i64, Instant> = HashMap::new();
    loop {
        if let Err(e) = process(&app, &mut last_sent).await {
            tracing::error!(error = format!("{e:#}"), "queue processing failed");
        }
        tokio::select! {
            () = app.queue_wake.notified() => {}
            () = tokio::time::sleep(Duration::from_secs(5)) => {}
            _ = shutdown.changed() => {
                if let Err(e) = process(&app, &mut last_sent).await {
                    tracing::error!(error = format!("{e:#}"), "final queue flush failed");
                }
                return;
            }
        }
    }
}

async fn process(app: &App, last_sent: &mut HashMap<i64, Instant>) -> Result<()> {
    let t = now();
    let db = &app.db;
    let mut dropped = db.drop_stale(&["admin"], 3600, t).await?;
    dropped += db
        .drop_stale(&["ready", "merchant_ready"], 15 * 60, t)
        .await?;
    dropped += db
        .drop_stale(&["session_start", "offline"], 30 * 60, t)
        .await?;
    dropped += db
        .drop_stale(&["broadcast", "weekly", "unmute_report"], 7 * 86_400, t)
        .await?;
    if dropped > 0 {
        tracing::info!(dropped, "stale queued messages dropped");
    }
    last_sent.retain(|_, at| at.elapsed() < PER_CHAT_GAP);
    loop {
        let rows = db.due(now(), BATCH).await?;
        if rows.is_empty() {
            return Ok(());
        }
        for row in rows {
            let t = now();
            let user = db.user(row.chat_id).await?;
            if row.kind != "admin" {
                match &user {
                    None => {
                        db.mark_dropped(row.id, "no such user", t).await?;
                        continue;
                    }
                    Some(u) if u.blocked_at.is_some() => {
                        db.mark_dropped(row.id, "blocked", t).await?;
                        continue;
                    }
                    Some(_) => {}
                }
            }
            let silent = user.as_ref().is_some_and(|u| quiet_now(u, t));
            if let Some(at) = last_sent.get(&row.chat_id) {
                let passed = at.elapsed();
                if passed < PER_CHAT_GAP {
                    tokio::time::sleep(PER_CHAT_GAP.saturating_sub(passed)).await;
                }
            }
            let markup = row
                .keyboard
                .as_deref()
                .and_then(|k| serde_json::from_str::<Inline>(k).ok())
                .map(Markup::Inline);
            let result = app
                .tg
                .send(row.chat_id, &row.text, markup.as_ref(), silent)
                .await;
            let t = now();
            match result {
                Ok(message_id) => {
                    last_sent.insert(row.chat_id, Instant::now());
                    db.mark_sent(row.id, message_id, t).await?;
                }
                Err(TgError::Suppressed) => db.mark_dropped(row.id, "test mode", t).await?,
                Err(TgError::Forbidden(e)) => {
                    tracing::info!(chat = row.chat_id, "user blocked the bot");
                    db.mark_dropped(row.id, &format!("forbidden: {e}"), t)
                        .await?;
                    if user.is_some() {
                        db.mark_blocked(row.chat_id, t).await?;
                        db.drop_pending_for_chat(row.chat_id, t).await?;
                    }
                }
                Err(TgError::RetryAfter(s)) => {
                    tracing::warn!(seconds = s, "flood control, queue paused");
                    db.mark_retry(row.id, t + s as i64, "flood control").await?;
                    return Ok(());
                }
                Err(e) if e.is_network() => {
                    tracing::warn!(error = %e, "Telegram unreachable, queue paused");
                    db.mark_retry(row.id, t + retry_delay(row.attempts), &e.to_string())
                        .await?;
                    return Ok(());
                }
                Err(TgError::BadRequest(e))
                    if e.contains("chat not found") || row.attempts >= 4 =>
                {
                    tracing::warn!(chat = row.chat_id, error = %e, "message dropped");
                    db.mark_dropped(row.id, &e, t).await?;
                }
                Err(e) => {
                    if row.attempts >= 9 {
                        db.mark_dropped(row.id, &e.to_string(), t).await?;
                    } else {
                        db.mark_retry(row.id, t + retry_delay(row.attempts), &e.to_string())
                            .await?;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(enabled: bool) -> UserRow {
        UserRow {
            id: 1,
            is_merchant: false,
            role_selected: true,
            muted: false,
            mute_until: None,
            tz: "Europe/Moscow".into(),
            quiet_enabled: enabled,
            quiet_start: 23 * 60,
            quiet_end: 8 * 60,
            notify_offline_return: true,
            show_game: true,
            blocked_at: None,
        }
    }

    #[test]
    fn quiet_hours_follow_user_zone() {
        // 21:00 UTC = 00:00 MSK
        assert!(quiet_now(&user(true), 21 * 3600));
        // 06:00 UTC = 09:00 MSK
        assert!(!quiet_now(&user(true), 6 * 3600));
        assert!(!quiet_now(&user(false), 21 * 3600));
        assert_eq!(retry_delay(0), 60);
        assert_eq!(retry_delay(100), 3600);
    }
}
