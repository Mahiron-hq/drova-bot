use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use anyhow::Result;
use tokio::sync::{Mutex, Notify};

use crate::config::Config;
use crate::db::{Db, SubRow, UserRow};
use crate::drova::{DESKTOP_PRODUCT, DESKTOP_TITLE, DrovaClient, Hardware};
use crate::telegram::Telegram;
use crate::texts::{self, SubView};
use crate::util::now;

const PRODUCT_TTL: i64 = 30 * 86_400;
const DIALOG_TTL: i64 = 15 * 60;
const HARDWARE_TTL: i64 = 6 * 3_600;
const HARDWARE_RETRY: i64 = 5 * 60;
const HARDWARE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const HARDWARE_CACHE_MAX: usize = 2_000;
const TITLE_UNKNOWN_RETRY: i64 = 6 * 3_600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentOrigin {
    Card,
    CommentsList,
    Other,
}

#[derive(Debug, Clone)]
pub enum Dialog {
    RemoveUrl,
    Comment {
        sub_id: i64,
        origin: Option<(i64, i64, CommentOrigin)>,
    },
    Import,
    Quiet,
    Tz,
    Broadcast,
    BroadcastConfirm(String),
}

#[derive(Default)]
pub struct Health {
    pub started: AtomicI64,
    pub last_updates: AtomicI64,
    pub last_cycle: AtomicI64,
    pub api_down: AtomicBool,
    pub cycle: std::sync::Mutex<Option<(i64, i64, usize, usize)>>,
}

pub struct App {
    pub cfg: Config,
    pub db: Db,
    pub tg: Telegram,
    pub drova: DrovaClient,
    pub bot_username: String,
    pub health: Health,
    pub queue_wake: Notify,
    dialogs: Mutex<HashMap<i64, (Dialog, i64)>>,
    cooldowns: Mutex<HashMap<(i64, String), i64>>,
    flood: Mutex<HashMap<i64, (i64, u32)>>,
    user_locks: Mutex<HashMap<i64, Arc<Mutex<()>>>>,
    hardware: Mutex<HashMap<String, (Option<Hardware>, i64)>>,
    /// Product ids whose title lookup failed, with the time of the next try.
    title_misses: Mutex<HashMap<String, i64>>,
}

impl App {
    pub fn new(
        cfg: Config,
        db: Db,
        tg: Telegram,
        drova: DrovaClient,
        bot_username: String,
    ) -> Self {
        let health = Health::default();
        health.started.store(now(), Ordering::Relaxed);
        Self {
            cfg,
            db,
            tg,
            drova,
            bot_username,
            health,
            queue_wake: Notify::new(),
            dialogs: Mutex::new(HashMap::new()),
            cooldowns: Mutex::new(HashMap::new()),
            flood: Mutex::new(HashMap::new()),
            user_locks: Mutex::new(HashMap::new()),
            hardware: Mutex::new(HashMap::new()),
            title_misses: Mutex::new(HashMap::new()),
        }
    }

    pub async fn user_lock(&self, user_id: i64) -> Arc<Mutex<()>> {
        let mut locks = self.user_locks.lock().await;
        if locks.len() > 10_000 {
            locks.retain(|_, l| Arc::strong_count(l) > 1);
        }
        locks.entry(user_id).or_default().clone()
    }

    pub async fn set_dialog(&self, user_id: i64, d: Dialog) {
        let mut map = self.dialogs.lock().await;
        let t = now();
        map.retain(|_, (_, exp)| *exp > t);
        map.insert(user_id, (d, t + DIALOG_TTL));
    }

    pub async fn dialog(&self, user_id: i64) -> Option<Dialog> {
        let map = self.dialogs.lock().await;
        map.get(&user_id)
            .filter(|(_, exp)| *exp > now())
            .map(|(d, _)| d.clone())
    }

    pub async fn clear_dialog(&self, user_id: i64) {
        self.dialogs.lock().await.remove(&user_id);
    }

    /// True when the action is allowed; otherwise returns the seconds left.
    pub async fn cooldown(&self, user_id: i64, key: &str, seconds: i64) -> Result<(), i64> {
        let mut map = self.cooldowns.lock().await;
        let t = now();
        if map.len() > 10_000 {
            map.retain(|_, until| *until > t);
        }
        let k = (user_id, key.to_owned());
        if let Some(until) = map.get(&k)
            && *until > t
        {
            return Err(*until - t);
        }
        map.insert(k, t + seconds);
        Ok(())
    }

    pub async fn reset_cooldown(&self, user_id: i64, key: &str) {
        self.cooldowns
            .lock()
            .await
            .remove(&(user_id, key.to_owned()));
    }

    /// Simple flood guard: at most 20 updates per 10 seconds per user.
    /// Returns `None` when allowed, `Some(first)` when blocked (`first` = warn once).
    pub async fn flood_check(&self, user_id: i64) -> Option<bool> {
        let mut map = self.flood.lock().await;
        let t = now();
        if map.len() > 10_000 {
            map.retain(|_, (start, _)| t - *start < 10);
        }
        let e = map.entry(user_id).or_insert((t, 0));
        if t - e.0 >= 10 {
            *e = (t, 0);
        }
        e.1 += 1;
        match e.1 {
            0..=20 => None,
            21 => Some(true),
            _ => Some(false),
        }
    }

    pub fn limit_for(&self, u: &UserRow) -> i64 {
        if u.is_merchant {
            self.cfg.max_subs_merchant
        } else {
            self.cfg.max_subs_user
        }
    }

    pub fn can_see_stats(&self, u: &UserRow) -> bool {
        u.is_merchant || self.cfg.is_admin(u.id)
    }

    /// Game title from the cache, fetched from Drova when missing or old.
    pub async fn title(&self, product_id: &str) -> Option<String> {
        if product_id == DESKTOP_PRODUCT {
            return Some(DESKTOP_TITLE.to_owned());
        }
        let cached = self.db.product(product_id).await.ok().flatten();
        if let Some((title, at)) = &cached
            && now() - at < PRODUCT_TTL
        {
            return Some(title.clone());
        }
        let t = now();
        if self
            .title_misses
            .lock()
            .await
            .get(product_id)
            .is_some_and(|next| *next > t)
        {
            return cached.map(|c| c.0);
        }
        let retry_in = match tokio::time::timeout(
            HARDWARE_TIMEOUT,
            self.drova.product_title(product_id),
        )
        .await
        {
            Ok(Ok(Some(title))) => {
                if let Err(e) = self.db.save_product(product_id, &title, t).await {
                    tracing::warn!(error = %e, "cannot save product title");
                }
                self.title_misses.lock().await.remove(product_id);
                return Some(title);
            }
            Ok(Ok(None)) => TITLE_UNKNOWN_RETRY,
            Ok(Err(e)) => {
                tracing::debug!(product_id, error = %e, "product title lookup failed");
                HARDWARE_RETRY
            }
            Err(_) => HARDWARE_RETRY,
        };
        let mut misses = self.title_misses.lock().await;
        misses.retain(|_, next| *next > t);
        if misses.len() < HARDWARE_CACHE_MAX {
            misses.insert(product_id.to_owned(), t + retry_in);
        }
        cached.map(|c| c.0)
    }

    /// Station hardware, cached for a few hours; failed lookups are retried after a few minutes.
    pub async fn hardware(&self, uuid: &str) -> Option<Hardware> {
        let t = now();
        if let Some((hw, until)) = self.hardware.lock().await.get(uuid)
            && *until > t
        {
            return hw.clone();
        }
        let (hw, ttl) =
            match tokio::time::timeout(HARDWARE_TIMEOUT, self.drova.hardware(uuid)).await {
                Ok(Ok(hw)) => (hw, HARDWARE_TTL),
                Ok(Err(e)) => {
                    tracing::debug!(uuid, error = %e, "hardware lookup failed");
                    (None, HARDWARE_RETRY)
                }
                Err(_) => (None, HARDWARE_RETRY),
            };
        let mut map = self.hardware.lock().await;
        map.retain(|_, (_, until)| *until > t);
        if map.len() < HARDWARE_CACHE_MAX {
            map.insert(uuid.to_owned(), (hw.clone(), t + ttl));
        }
        hw
    }

    /// Cached titles only, for building views without network calls.
    pub async fn titles(&self, ids: &[String]) -> HashMap<String, String> {
        let mut out = self.db.product_titles(ids).await.unwrap_or_default();
        if ids.iter().any(|i| i == DESKTOP_PRODUCT) {
            out.insert(DESKTOP_PRODUCT.to_owned(), DESKTOP_TITLE.to_owned());
        }
        out
    }

    pub async fn user_or_default(&self, user_id: i64) -> Result<UserRow> {
        if let Some(u) = self.db.user(user_id).await? {
            return Ok(u);
        }
        self.db.touch_user(user_id, now()).await?;
        self.db
            .user(user_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("user {user_id} vanished"))
    }

    /// Subscriptions with cached titles and waiting counts.
    pub async fn sub_views(
        &self,
        u: &UserRow,
    ) -> Result<(Vec<SubRow>, HashMap<String, String>, HashMap<String, i64>)> {
        let subs = self.db.subs_for_user(u.id).await?;
        let ids: Vec<String> = subs
            .iter()
            .filter_map(|s| s.station.product_id.clone())
            .collect();
        let titles = if u.show_game {
            self.titles(&ids).await
        } else {
            HashMap::new()
        };
        let waiting = self.db.waiting_counts(u.id, now()).await?;
        Ok((subs, titles, waiting))
    }

    pub async fn status_text(&self, u: &UserRow) -> Result<Option<String>> {
        let (subs, titles, waiting) = self.sub_views(u).await?;
        if subs.is_empty() {
            return Ok(None);
        }
        let views: Vec<SubView<'_>> = subs
            .iter()
            .map(|s| SubView {
                sub: s,
                game: s
                    .station
                    .product_id
                    .as_ref()
                    .and_then(|p| titles.get(p))
                    .map(String::as_str),
                waiting: waiting.get(&s.station.uuid).copied().unwrap_or(0),
            })
            .collect();
        Ok(Some(texts::status(&views, now())))
    }

    pub fn wake_queue(&self) {
        self.queue_wake.notify_one();
    }

    pub fn share_link(&self, uuid: &str) -> String {
        format!("https://t.me/{}?start={uuid}", self.bot_username)
    }
}
