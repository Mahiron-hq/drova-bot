use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::watch;

use crate::app::App;
use crate::db::GONE_KEEP_DAYS;
use crate::keyboards as kb;
use crate::texts;
use crate::util::now;

const PRUNE_EVERY: i64 = 86_400;
/// A missed weekly reminder is still sent if the bot was down for less than this.
const WEEKLY_CATCH_UP: i64 = 86_400;

/// Monday 19:00 Moscow time of the current week, as a Unix timestamp.
pub fn weekly_due(now: i64) -> Option<i64> {
    let tz = crate::tz::resolve("Europe/Moscow");
    let local = jiff::Timestamp::from_second(now)
        .ok()?
        .to_zoned_datetime(tz.clone());
    let days_from_monday = i64::from(local.weekday().to_monday_zero_offset());
    let monday = local
        .date()
        .checked_sub(jiff::Span::new().days(days_from_monday))
        .ok()?;
    let due = monday.at(19, 0, 0, 0).to_zoned_datetime(tz).ok()?;
    Some(due.timestamp().as_second())
}

pub async fn run(app: Arc<App>, mut shutdown: watch::Receiver<bool>) {
    let mut next_prune = now() + 300;
    loop {
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(60)) => {}
            _ = shutdown.changed() => return,
        }
        if let Err(e) = expired_mutes(&app).await {
            tracing::error!(error = format!("{e:#}"), "expired silence job failed");
        }
        if let Err(e) = expired_gone(&app).await {
            tracing::error!(error = format!("{e:#}"), "deleted stations job failed");
        }
        if let Err(e) = weekly(&app).await {
            tracing::error!(error = format!("{e:#}"), "weekly reminder job failed");
        }
        if now() >= next_prune {
            next_prune = now() + PRUNE_EVERY;
            match app.db.prune(now()).await {
                Ok(p) => tracing::info!(
                    queue = p.queue,
                    transitions = p.transitions,
                    poll_errors = p.poll_errors,
                    users = p.users,
                    stations = p.stations,
                    "old data pruned"
                ),
                Err(e) => tracing::error!(error = format!("{e:#}"), "prune failed"),
            }
        }
    }
}

async fn expired_mutes(app: &App) -> Result<()> {
    let users = app.db.take_expired_mutes(now()).await?;
    if users.is_empty() {
        return Ok(());
    }
    tracing::info!(count = users.len(), "silence expired");
    for id in users {
        let Some(u) = app.db.user(id).await? else {
            continue;
        };
        let Some(status) = app.status_text(&u).await? else {
            continue;
        };
        app.enqueue(
            "unmute_report",
            id,
            None,
            &texts::unmuted_report(&status),
            Some(&kb::status()),
        )
        .await?;
    }
    app.wake_queue();
    Ok(())
}

async fn expired_gone(app: &App) -> Result<()> {
    let subs = app
        .db
        .take_expired_gone(now() - GONE_KEEP_DAYS * 86_400)
        .await?;
    if subs.is_empty() {
        return Ok(());
    }
    tracing::info!(
        count = subs.len(),
        "subscriptions to deleted stations removed"
    );
    for s in subs {
        if app
            .db
            .user(s.user_id)
            .await?
            .is_none_or(|u| u.blocked_at.is_some())
        {
            continue;
        }
        let st = &s.station;
        let text = texts::gone_unsubscribed(&st.name, &st.uuid, s.comment.as_deref());
        app.enqueue("gone_unsub", s.user_id, Some(&st.uuid), &text, None)
            .await?;
    }
    app.wake_queue();
    Ok(())
}

async fn weekly(app: &App) -> Result<()> {
    let Some(text) = app.cfg.weekly_reminder_text.as_deref() else {
        return Ok(());
    };
    let t = now();
    let Some(due) = weekly_due(t) else {
        return Ok(());
    };
    if t < due || t - due > WEEKLY_CATCH_UP {
        return Ok(());
    }
    if !app.db.create_campaign("weekly", due, t).await? {
        return Ok(());
    }
    let targets = app.db.weekly_targets(t).await?;
    for id in &targets {
        app.enqueue("weekly", *id, None, text, None).await?;
    }
    tracing::info!(count = targets.len(), "weekly reminder queued");
    app.wake_queue();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::weekly_due;

    #[test]
    fn monday_evening_moscow() {
        // 2026-11-25 (Wednesday) 12:00 UTC -> Monday 2026-11-23 16:00 UTC
        let wed: jiff::Timestamp = "2026-11-25T12:00:00Z".parse().unwrap();
        let mon: jiff::Timestamp = "2026-11-23T16:00:00Z".parse().unwrap();
        assert_eq!(weekly_due(wed.as_second()), Some(mon.as_second()));
        let early: jiff::Timestamp = "2026-11-23T10:00:00Z".parse().unwrap();
        assert_eq!(weekly_due(early.as_second()), Some(mon.as_second()));
    }
}
