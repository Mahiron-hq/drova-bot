use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

#[derive(Clone)]
pub struct Config {
    pub bot_token: String,
    pub database_path: PathBuf,
    pub poll_interval: Duration,
    pub request_delay: Duration,
    pub http_timeout: Duration,
    pub poll_concurrency: usize,
    pub log_level: tracing::Level,
    pub admin_ids: Vec<i64>,
    /// Test mode: when set, the bot never sends anything to chats outside this list.
    pub allowed_chat_ids: Option<HashSet<i64>>,
    pub max_subs_user: i64,
    pub max_subs_merchant: i64,
    pub donate_text: Option<String>,
    pub weekly_reminder_text: Option<String>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("database_path", &self.database_path)
            .field("poll_interval", &self.poll_interval)
            .field("admin_ids", &self.admin_ids)
            .field("test_mode", &self.allowed_chat_ids.is_some())
            .finish_non_exhaustive()
    }
}

fn var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

fn number<T: std::str::FromStr>(name: &str, default: T) -> Result<T> {
    match var(name) {
        None => Ok(default),
        Some(v) => v
            .parse()
            .map_err(|_| anyhow::anyhow!("{name}: invalid number {v:?}")),
    }
}

fn seconds(name: &str, default: f64, min: f64, max: f64) -> Result<Duration> {
    let value: f64 = number(name, default)?;
    if !(min..=max).contains(&value) {
        bail!("{name} must be between {min} and {max}");
    }
    Ok(Duration::from_secs_f64(value))
}

fn ids(name: &str) -> Result<Vec<i64>> {
    let mut out = Vec::new();
    for item in var(name).unwrap_or_default().split([',', ';', ' ']) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        out.push(
            item.parse()
                .with_context(|| format!("{name}: invalid id {item:?}"))?,
        );
    }
    Ok(out)
}

fn text_file(name: &str) -> Result<Option<String>> {
    let Some(path) = var(name) else {
        return Ok(None);
    };
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("{name}: cannot read {path}"))?;
    let text = text.trim().to_owned();
    if text.is_empty() {
        return Ok(None);
    }
    if text.chars().count() > 4000 {
        bail!("{name}: text is longer than 4000 characters");
    }
    Ok(Some(text))
}

fn token_looks_valid(token: &str) -> bool {
    match token.split_once(':') {
        Some((id, secret)) => {
            !id.is_empty()
                && id.bytes().all(|b| b.is_ascii_digit())
                && secret.len() >= 20
                && secret
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        }
        None => false,
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let bot_token = var("BOT_TOKEN").context("BOT_TOKEN is not set")?;
        if !token_looks_valid(&bot_token) {
            bail!("BOT_TOKEN has an invalid format");
        }
        let log_level = match var("LOG_LEVEL")
            .unwrap_or_default()
            .to_ascii_uppercase()
            .as_str()
        {
            "" | "INFO" => tracing::Level::INFO,
            "DEBUG" => tracing::Level::DEBUG,
            "TRACE" => tracing::Level::TRACE,
            "WARN" | "WARNING" => tracing::Level::WARN,
            "ERROR" | "CRITICAL" => tracing::Level::ERROR,
            other => bail!("LOG_LEVEL: unknown level {other:?}"),
        };
        let allowed = ids("ALLOWED_CHAT_IDS")?;
        let max_subs_user: i64 = number("MAX_SUBSCRIPTIONS_USER", 5)?;
        let max_subs_merchant: i64 = number("MAX_SUBSCRIPTIONS_MERCHANT", 20)?;
        if !(1..=50).contains(&max_subs_user) || !(1..=50).contains(&max_subs_merchant) {
            bail!("subscription limits must be between 1 and 50");
        }
        let poll_concurrency: usize = number("POLL_CONCURRENCY", 5)?;
        if !(1..=20).contains(&poll_concurrency) {
            bail!("POLL_CONCURRENCY must be between 1 and 20");
        }
        Ok(Self {
            bot_token,
            database_path: PathBuf::from(
                var("DATABASE_PATH").unwrap_or_else(|| "drova_monitor.sqlite3".to_owned()),
            ),
            poll_interval: seconds("POLL_INTERVAL_SECONDS", 30.0, 10.0, 3600.0)?,
            request_delay: seconds("REQUEST_DELAY_SECONDS", 0.7, 0.0, 10.0)?,
            http_timeout: seconds("HTTP_TIMEOUT_SECONDS", 15.0, 1.0, 120.0)?,
            poll_concurrency,
            log_level,
            admin_ids: ids("ADMIN_IDS")?,
            allowed_chat_ids: (!allowed.is_empty()).then(|| allowed.into_iter().collect()),
            max_subs_user,
            max_subs_merchant,
            donate_text: text_file("DONATE_TEXT_FILE")?,
            weekly_reminder_text: text_file("WEEKLY_REMINDER_FILE")?,
        })
    }

    pub fn is_admin(&self, user_id: i64) -> bool {
        self.admin_ids.contains(&user_id)
    }
}

#[cfg(test)]
mod tests {
    use super::token_looks_valid;

    #[test]
    fn token_format() {
        assert!(token_looks_valid("123456:ABCdefGHIjklMNOpqrSTUvwxYZ_-12"));
        assert!(!token_looks_valid("123456"));
        assert!(!token_looks_valid("abc:ABCdefGHIjklMNOpqrSTUvwxYZ"));
        assert!(!token_looks_valid("123:short"));
        assert!(!token_looks_valid("123:ABCdefGHIjklMNOpqrSTU/wxYZ"));
    }
}
