use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;

use crate::app::App;
use crate::db::{Applied, GONE_AFTER, StationRow};
use crate::drova::{DrovaError, Snapshot, Status};
use crate::keyboards as kb;
use crate::telegram::TgError;
use crate::texts::{self, FreeInfo};
use crate::util::now;

const FAIL_STREAK_ALERT: i64 = 3;
const ADMIN_DIGEST_GAP: i64 = 30 * 60;
const TAKEN_WINDOW: i64 = 5 * 60;
const MAX_BACKOFF: u32 = 10;
const GONE_POLL_EVERY: i64 = 3_600;

pub struct Monitor {
    app: Arc<App>,
    streaks: HashMap<String, i64>,
    alerted: HashSet<String>,
    pending_down: HashMap<String, String>,
    pending_up: HashSet<String>,
    last_digest: Option<i64>,
    unknown_states: HashSet<String>,
    gone_polled: HashMap<String, i64>,
    backoff: u32,
}

impl Monitor {
    pub fn new(app: Arc<App>) -> Self {
        Self {
            app,
            streaks: HashMap::new(),
            alerted: HashSet::new(),
            pending_down: HashMap::new(),
            pending_up: HashSet::new(),
            last_digest: None,
            unknown_states: HashSet::new(),
            gone_polled: HashMap::new(),
            backoff: 1,
        }
    }

    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(3)) => {}
            _ = shutdown.changed() => return,
        }
        loop {
            if let Err(e) = self.cycle().await {
                tracing::error!(error = format!("{e:#}"), "polling cycle failed");
            }
            let wait = self.app.cfg.poll_interval * self.backoff;
            let wait = wait.min(Duration::from_secs(300).max(self.app.cfg.poll_interval));
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                _ = shutdown.changed() => return,
            }
        }
    }

    async fn cycle(&mut self) -> Result<()> {
        let started = Instant::now();
        let all = self.app.db.stations_to_poll().await?;
        let t0 = now();
        let known: HashSet<&str> = all.iter().map(|(u, _)| u.as_str()).collect();
        self.streaks.retain(|u, _| known.contains(u.as_str()));
        self.alerted.retain(|u| known.contains(u.as_str()));
        self.pending_down.retain(|u, _| known.contains(u.as_str()));
        self.pending_up.retain(|u| known.contains(u.as_str()));
        let gone: HashSet<String> = all
            .iter()
            .filter(|(_, g)| *g)
            .map(|(u, _)| u.clone())
            .collect();
        self.gone_polled.retain(|u, _| gone.contains(u));
        let mut stations = Vec::with_capacity(all.len());
        for (uuid, is_gone) in all {
            if is_gone {
                if self
                    .gone_polled
                    .get(&uuid)
                    .is_some_and(|t| t0 - t < GONE_POLL_EVERY)
                {
                    continue;
                }
                self.gone_polled.insert(uuid.clone(), t0);
            }
            stations.push(uuid);
        }
        let total = stations.len();
        let mut tasks = JoinSet::new();
        let sem = Arc::new(Semaphore::new(self.app.cfg.poll_concurrency));
        for uuid in stations {
            let sem = sem.clone();
            let drova = self.app.drova.clone();
            let delay = self.app.cfg.request_delay;
            tasks.spawn(async move {
                let _permit = sem.acquire_owned().await;
                let r = drova.station(&uuid).await;
                tokio::time::sleep(delay).await;
                (uuid, r)
            });
        }
        let mut results: Vec<(String, Result<Snapshot, DrovaError>)> = Vec::with_capacity(total);
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(r) => results.push(r),
                Err(e) => tracing::error!(error = %e, "poll task failed"),
            }
        }
        results.sort_by(|a, b| a.0.cmp(&b.0));
        let ok = results.iter().filter(|r| r.1.is_ok()).count();
        let active: Vec<&Result<Snapshot, DrovaError>> = results
            .iter()
            .filter(|r| !gone.contains(&r.0))
            .map(|r| &r.1)
            .collect();
        let active_ok = active.iter().filter(|r| r.is_ok()).count();
        let not_found = active
            .iter()
            .filter(|r| matches!(r, Err(DrovaError::NotFound)))
            .count();
        let api_down = active.len() >= 3 && active_ok == 0;
        let mass_missing = active.len() >= 3 && not_found * 3 > active.len();
        let was_down = self.app.health.api_down.load(Ordering::Relaxed);
        if api_down && !was_down {
            let err = results
                .iter()
                .find_map(|r| r.1.as_ref().err().map(ToString::to_string))
                .unwrap_or_default();
            tracing::warn!(error = %err, "Drova API looks down");
            self.app.health.api_down.store(true, Ordering::Relaxed);
            self.admin_alert(&texts::api_down(&err)).await;
        } else if !api_down && was_down && ok > 0 {
            tracing::info!("Drova API is back");
            self.app.health.api_down.store(false, Ordering::Relaxed);
            self.backoff = 1;
            self.streaks.clear();
            self.admin_alert(texts::API_UP).await;
        }
        if api_down {
            self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
        }
        for (uuid, r) in results {
            match r {
                Ok(snap) => {
                    if let Err(e) = self.process(&snap).await {
                        tracing::error!(uuid, error = format!("{e:#}"), "cannot process snapshot");
                    }
                }
                Err(e) => {
                    let text = e.to_string();
                    if gone.contains(&uuid) {
                        tracing::debug!(uuid, error = %text, "deleted station still missing");
                        continue;
                    }
                    tracing::warn!(uuid, error = %text, "poll failed");
                    self.app.db.record_poll_error(&uuid, &text, now()).await?;
                    if !api_down {
                        self.on_failure(&uuid, &text);
                    }
                    if matches!(e, DrovaError::NotFound)
                        && let Err(e) = self.on_not_found(&uuid, mass_missing).await
                    {
                        tracing::error!(
                            uuid,
                            error = format!("{e:#}"),
                            "cannot handle missing station"
                        );
                    }
                }
            }
        }
        self.flush_digest().await?;
        let t = now();
        self.app.health.last_cycle.store(t, Ordering::Relaxed);
        if let Ok(mut c) = self.app.health.cycle.lock() {
            *c = Some((t, started.elapsed().as_millis() as i64, ok, total));
        }
        tracing::info!(
            total,
            ok,
            ms = started.elapsed().as_millis() as u64,
            "polling cycle done"
        );
        self.app.wake_queue();
        Ok(())
    }

    fn on_failure(&mut self, uuid: &str, error: &str) {
        let streak = self.streaks.entry(uuid.to_owned()).or_default();
        *streak += 1;
        if *streak >= FAIL_STREAK_ALERT && self.alerted.insert(uuid.to_owned()) {
            self.pending_down.insert(uuid.to_owned(), error.to_owned());
            self.pending_up.remove(uuid);
        }
    }

    async fn on_not_found(&mut self, uuid: &str, mass: bool) -> Result<()> {
        let t = now();
        let Some(since) = self.app.db.mark_missing(uuid, t).await? else {
            return Ok(());
        };
        if mass || t - since < GONE_AFTER || !self.app.db.mark_gone(uuid, t).await? {
            return Ok(());
        }
        self.streaks.remove(uuid);
        self.alerted.remove(uuid);
        self.pending_down.remove(uuid);
        self.pending_up.remove(uuid);
        self.gone_polled.insert(uuid.to_owned(), t);
        let Some(st) = self.app.db.station(uuid).await? else {
            return Ok(());
        };
        tracing::info!(uuid, "station looks deleted");
        for (sub, user) in self.app.db.subs_for_station(uuid).await? {
            if user.blocked_at.is_some() {
                continue;
            }
            let text = texts::station_gone(&st.name, uuid, t - since, sub.comment.as_deref());
            self.app
                .enqueue("gone", user.id, Some(uuid), &text, Some(&kb::gone(sub.id)))
                .await?;
        }
        self.app.wake_queue();
        Ok(())
    }

    fn on_success(&mut self, uuid: &str) {
        self.streaks.remove(uuid);
        if self.alerted.remove(uuid) && self.pending_down.remove(uuid).is_none() {
            self.pending_up.insert(uuid.to_owned());
        }
    }

    async fn station_label(&self, uuid: &str) -> String {
        match self.app.db.station(uuid).await {
            Ok(Some(st)) => format!("{} ({})", st.name, crate::util::station_url(uuid)),
            _ => crate::util::station_url(uuid),
        }
    }

    async fn flush_digest(&mut self) -> Result<()> {
        if self.app.cfg.admin_ids.is_empty()
            || (self.pending_down.is_empty() && self.pending_up.is_empty())
        {
            return Ok(());
        }
        let t = now();
        if self.last_digest.is_some_and(|l| t - l < ADMIN_DIGEST_GAP) {
            return Ok(());
        }
        let mut down = Vec::new();
        for (uuid, err) in std::mem::take(&mut self.pending_down) {
            down.push((self.station_label(&uuid).await, err));
        }
        let mut up = Vec::new();
        for uuid in std::mem::take(&mut self.pending_up) {
            up.push(self.station_label(&uuid).await);
        }
        down.sort();
        up.sort();
        self.last_digest = Some(t);
        let when = jiff::Timestamp::from_second(t)
            .map(|ts| ts.to_string())
            .unwrap_or_default();
        self.admin_alert(&texts::admin_digest(&down, &up, &when, FAIL_STREAK_ALERT))
            .await;
        Ok(())
    }

    async fn admin_alert(&self, text: &str) {
        for id in &self.app.cfg.admin_ids {
            if let Err(e) = self.app.enqueue("admin", *id, None, text, None).await {
                tracing::error!(error = %e, "cannot queue admin alert");
            }
        }
        self.app.wake_queue();
    }

    async fn process(&mut self, snap: &Snapshot) -> Result<()> {
        self.on_success(&snap.uuid);
        if !snap.known_state
            && self
                .unknown_states
                .insert(snap.raw_state.to_ascii_uppercase())
        {
            tracing::warn!(state = %snap.raw_state, "unknown station state");
            self.admin_alert(&texts::unknown_state(&snap.raw_state, &snap.name))
                .await;
        }
        if let Some(p) = &snap.product_id {
            self.app.title(p).await;
        }
        let t = now();
        let applied = self.app.db.apply_snapshot(snap, t).await?;
        if applied.returned {
            tracing::info!(uuid = %snap.uuid, "deleted station is back");
            self.gone_polled.remove(&snap.uuid);
            notify_returned(&self.app, &applied.new).await?;
        }
        match applied.new.status {
            Status::Busy | Status::Offline => self.on_taken(&applied, t).await,
            Status::Ready => self.on_ready(&applied).await,
        }
    }

    async fn on_taken(&self, a: &Applied, t: i64) -> Result<()> {
        let st = &a.new;
        self.app.db.set_station_waiting(&st.uuid, st.status).await?;
        let Some(old) = &a.old else {
            return Ok(());
        };
        let fresh = a.status_changed || a.game_changed_while_busy;
        let session_started = a.session_started;
        if !fresh && !session_started {
            return Ok(());
        }
        let game = match &st.product_id {
            Some(p) if st.status == Status::Busy => self.app.title(p).await,
            _ => None,
        };
        if fresh {
            let suffix = texts::taken_suffix(st.status, game.as_deref());
            for (_, chat, message_id, text) in self
                .app
                .db
                .take_recent_free_messages(&st.uuid, TAKEN_WINDOW, t)
                .await?
            {
                let edited = texts::mark_taken(&text, &suffix);
                match self.app.tg.edit(chat, message_id, &edited, None).await {
                    Ok(()) | Err(TgError::Suppressed) => {}
                    Err(TgError::Forbidden(_)) => self.app.db.mark_blocked(chat, t).await?,
                    Err(e) => tracing::debug!(chat, error = %e, "cannot edit free message"),
                }
            }
        }
        let went_offline = st.status == Status::Offline && old.status != Status::Offline;
        if !went_offline && !session_started {
            return Ok(());
        }
        for (sub, user) in self.app.db.subs_for_station(&st.uuid).await? {
            if !user.is_merchant || user.blocked_at.is_some() || user.mute_active(t) {
                continue;
            }
            let comment = sub.comment.as_deref();
            if session_started && sub.notify_busy {
                let g = if user.show_game {
                    game.as_deref()
                } else {
                    None
                };
                let text = texts::session_started(&st.name, &st.uuid, g, comment);
                self.app
                    .enqueue(
                        "session_start",
                        user.id,
                        Some(&st.uuid),
                        &text,
                        Some(&kb::open_station(&st.uuid)),
                    )
                    .await?;
            }
            if went_offline && sub.notify_offline {
                let text = texts::went_offline(&st.name, &st.uuid, comment);
                self.app
                    .enqueue(
                        "offline",
                        user.id,
                        Some(&st.uuid),
                        &text,
                        Some(&kb::open_station(&st.uuid)),
                    )
                    .await?;
            }
        }
        Ok(())
    }

    async fn on_ready(&self, a: &Applied) -> Result<()> {
        let st = &a.new;
        let t = now();
        let mut done = Vec::new();
        let mut recipients = Vec::new();
        for (sub, user) in self.app.db.subs_for_station(&st.uuid).await? {
            if !sub.pending || user.blocked_at.is_some() {
                continue;
            }
            if !st.published && !user.is_merchant {
                continue;
            }
            done.push(sub.id);
            if user.mute_active(t) {
                continue;
            }
            if sub.last_status == Status::Offline && !user.notify_offline_return {
                continue;
            }
            recipients.push((sub, user));
        }
        if recipients.is_empty() {
            self.app.db.mark_subs_ready(&done).await?;
            return Ok(());
        }
        let session = self.app.db.last_session(&st.uuid).await?;
        let game_id = session
            .as_ref()
            .and_then(|s| s.0.clone())
            .or_else(|| st.product_id.clone());
        let game = match &game_id {
            Some(p) => self.app.title(p).await,
            None => None,
        };
        let others = recipients.len() as i64 - 1;
        let kind = if st.published {
            "ready"
        } else {
            "merchant_ready"
        };
        let keyboard = kb::notify(&st.uuid);
        for (sub, user) in &recipients {
            let text = texts::free_notification(&FreeInfo {
                name: &st.name,
                uuid: &st.uuid,
                from_offline: sub.last_status == Status::Offline,
                published: st.published,
                game: if user.show_game {
                    game.as_deref()
                } else {
                    None
                },
                session: session.as_ref().map(|s| s.1),
                comment: sub.comment.as_deref(),
                others,
            });
            self.app
                .enqueue(kind, user.id, Some(&st.uuid), &text, Some(&keyboard))
                .await?;
        }
        self.app.db.mark_subs_ready(&done).await?;
        tracing::info!(uuid = %st.uuid, recipients = recipients.len(), kind, "free notifications queued");
        self.app.wake_queue();
        Ok(())
    }
}

/// Tells subscribers that a station marked as deleted is on Drova again.
pub async fn notify_returned(app: &App, st: &StationRow) -> Result<()> {
    for (sub, user) in app.db.subs_for_station(&st.uuid).await? {
        if user.blocked_at.is_some() {
            continue;
        }
        let text = texts::station_returned(&st.name, &st.uuid, sub.comment.as_deref());
        app.enqueue(
            "returned",
            user.id,
            Some(&st.uuid),
            &text,
            Some(&kb::open_station(&st.uuid)),
        )
        .await?;
    }
    app.wake_queue();
    Ok(())
}
