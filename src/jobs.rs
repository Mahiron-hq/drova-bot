use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::watch;

use crate::app::App;
use crate::db::{GONE_KEEP_DAYS, Outgoing};
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
    let users = app.db.expired_mutes(now()).await?;
    if users.is_empty() {
        return Ok(());
    }
    tracing::info!(count = users.len(), "silence expired");
    for id in users {
        let Some(u) = app.db.user(id).await? else {
            continue;
        };
        let report = match app.status_text(&u).await? {
            Some(status) => Some(Outgoing::new(
                "unmute_report",
                id,
                None,
                texts::unmuted_report(&status),
                Some(&kb::status()),
            )?),
            None => None,
        };
        app.db.end_mute(id, now(), report.as_ref()).await?;
    }
    app.wake_queue();
    Ok(())
}

async fn expired_gone(app: &App) -> Result<()> {
    let cutoff = now() - GONE_KEEP_DAYS * 86_400;
    let subs = app.db.expired_gone_subs(cutoff).await?;
    if subs.is_empty() {
        return Ok(());
    }
    let mut removed = 0;
    for s in subs {
        let active = app
            .db
            .user(s.user_id)
            .await?
            .is_some_and(|u| u.blocked_at.is_none());
        let st = &s.station;
        let notice = if active {
            let text = texts::gone_unsubscribed(&st.name, &st.uuid, s.comment.as_deref());
            Some(Outgoing::new(
                "gone_unsub",
                s.user_id,
                Some(&st.uuid),
                text,
                None,
            )?)
        } else {
            None
        };
        if app
            .db
            .remove_gone_sub(s.id, cutoff, notice.as_ref(), now())
            .await?
        {
            removed += 1;
        }
    }
    tracing::info!(removed, "subscriptions to deleted stations removed");
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
    if app.db.campaign_exists("weekly", due).await? {
        return Ok(());
    }
    let targets = app.db.weekly_targets(t).await?;
    let msgs = targets
        .iter()
        .map(|id| Outgoing::new("weekly", *id, None, text, None))
        .collect::<Result<Vec<_>>>()?;
    if !app.db.start_campaign("weekly", due, t, &msgs).await? {
        return Ok(());
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
