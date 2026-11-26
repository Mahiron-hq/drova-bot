pub mod types;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use reqwest::multipart;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::time::Instant;

pub use types::*;

const API: &str = "https://api.telegram.org";
const MAX_TEXT: usize = 4096;
const MAX_DOWNLOAD: usize = 512 * 1024;
/// Telegram allows about 30 messages per second for a bot in total.
const GLOBAL_GAP: Duration = Duration::from_millis(40);

#[derive(Debug, thiserror::Error)]
pub enum TgError {
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("retry after {0} s")]
    RetryAfter(u64),
    #[error("message is not modified")]
    NotModified,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("network: {0}")]
    Network(String),
    #[error("telegram error {0}: {1}")]
    Api(i64, String),
    #[error("suppressed in test mode")]
    Suppressed,
}

impl TgError {
    pub fn is_network(&self) -> bool {
        matches!(self, Self::Network(_)) || matches!(self, Self::Api(code, _) if *code >= 500)
    }
}

#[derive(Clone)]
pub struct Telegram {
    http: reqwest::Client,
    base: Arc<str>,
    file_base: Arc<str>,
    allowed: Option<Arc<HashSet<i64>>>,
    next_slot: Arc<Mutex<Instant>>,
}

impl Telegram {
    pub fn new(token: &str, allowed: Option<HashSet<i64>>) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .https_only(true)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(75))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .map_err(|e| anyhow::anyhow!("Telegram HTTP client: {}", e.without_url()))?;
        Ok(Self {
            http,
            base: format!("{API}/bot{token}/").into(),
            file_base: format!("{API}/file/bot{token}/").into(),
            allowed: allowed.map(Arc::new),
            next_slot: Arc::new(Mutex::new(Instant::now())),
        })
    }

    pub fn test_mode(&self) -> bool {
        self.allowed.is_some()
    }

    fn check_chat(&self, method: &str, chat_id: i64) -> Result<(), TgError> {
        match &self.allowed {
            Some(set) if !set.contains(&chat_id) => {
                tracing::info!(method, chat_id, "test mode: call suppressed");
                Err(TgError::Suppressed)
            }
            _ => Ok(()),
        }
    }

    async fn throttle(&self) {
        let wait = {
            let mut next = self.next_slot.lock().await;
            let now = Instant::now();
            let start = (*next).max(now);
            *next = start + GLOBAL_GAP;
            start - now
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    async fn call_raw(&self, method: &str, req: reqwest::RequestBuilder) -> Result<Value, TgError> {
        let resp = req
            .send()
            .await
            .map_err(|e| TgError::Network(e.without_url().to_string()))?;
        let status = resp.status();
        let body: Value = resp.json().await.map_err(|e| {
            if status.is_server_error() {
                TgError::Api(i64::from(status.as_u16()), "server error".to_owned())
            } else {
                TgError::Network(e.without_url().to_string())
            }
        })?;
        if body.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok(body.get("result").cloned().unwrap_or(Value::Null));
        }
        let code = body.get("error_code").and_then(Value::as_i64).unwrap_or(0);
        let desc = body
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let retry = body
            .get("parameters")
            .and_then(|p| p.get("retry_after"))
            .and_then(Value::as_u64);
        tracing::debug!(method, code, "telegram error");
        Err(match code {
            429 => TgError::RetryAfter(retry.unwrap_or(5)),
            403 => TgError::Forbidden(desc),
            400 if desc.contains("message is not modified") => TgError::NotModified,
            400 => TgError::BadRequest(desc),
            _ => TgError::Api(code, desc),
        })
    }

    /// Calls a method; a short flood wait is retried once.
    pub async fn call<T: DeserializeOwned>(&self, method: &str, body: Value) -> Result<T, TgError> {
        let mut retried = false;
        loop {
            self.throttle().await;
            let req = self.http.post(format!("{}{method}", self.base)).json(&body);
            match self.call_raw(method, req).await {
                Err(TgError::RetryAfter(s)) if !retried && s <= 5 => {
                    retried = true;
                    tokio::time::sleep(Duration::from_secs(s)).await;
                }
                Err(e) => return Err(e),
                Ok(v) => {
                    return serde_json::from_value(v)
                        .map_err(|e| TgError::Api(0, format!("unexpected result: {e}")));
                }
            }
        }
    }

    pub async fn get_me(&self) -> Result<User, TgError> {
        self.call("getMe", json!({})).await
    }

    pub async fn get_updates(&self, offset: i64, timeout: u64) -> Result<Vec<Update>, TgError> {
        let req = self
            .http
            .post(format!("{}getUpdates", self.base))
            .json(&json!({
                "offset": offset,
                "timeout": timeout,
                "allowed_updates": ["message", "callback_query", "my_chat_member"],
            }));
        let v = self.call_raw("getUpdates", req).await?;
        let Value::Array(items) = v else {
            return Ok(Vec::new());
        };
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let id = item.get("update_id").and_then(Value::as_i64).unwrap_or(0);
            match serde_json::from_value::<Update>(item) {
                Ok(u) => out.push(u),
                Err(e) => {
                    tracing::warn!(update_id = id, error = %e, "skipping update that cannot be parsed");
                    out.push(Update {
                        update_id: id,
                        message: None,
                        callback_query: None,
                        my_chat_member: None,
                    });
                }
            }
        }
        Ok(out)
    }

    pub async fn send(
        &self,
        chat_id: i64,
        text: &str,
        markup: Option<&Markup>,
        silent: bool,
    ) -> Result<i64, TgError> {
        self.check_chat("sendMessage", chat_id)?;
        let mut body = json!({
            "chat_id": chat_id,
            "text": fit(text),
            "parse_mode": "HTML",
            "link_preview_options": { "is_disabled": true },
            "disable_notification": silent,
        });
        if let Some(m) = markup {
            set(&mut body, "reply_markup", m.to_json());
        }
        let sent: SentMessage = self.call("sendMessage", body).await?;
        Ok(sent.message_id)
    }

    pub async fn edit(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        inline: Option<&Inline>,
    ) -> Result<(), TgError> {
        self.check_chat("editMessageText", chat_id)?;
        let mut body = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": fit(text),
            "parse_mode": "HTML",
            "link_preview_options": { "is_disabled": true },
        });
        set(
            &mut body,
            "reply_markup",
            Markup::Inline(inline.cloned().unwrap_or_default()).to_json(),
        );
        match self.call::<Value>("editMessageText", body).await {
            Ok(_) | Err(TgError::NotModified) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn answer_callback(&self, id: &str, text: Option<&str>, alert: bool) {
        let mut body = json!({ "callback_query_id": id, "show_alert": alert });
        if let Some(t) = text {
            set(&mut body, "text", json!(t));
        }
        if let Err(e) = self.call::<Value>("answerCallbackQuery", body).await {
            tracing::debug!(error = %e, "answerCallbackQuery failed");
        }
    }

    pub async fn delete(&self, chat_id: i64, message_id: i64) {
        if self.check_chat("deleteMessage", chat_id).is_err() {
            return;
        }
        let body = json!({ "chat_id": chat_id, "message_id": message_id });
        if let Err(e) = self.call::<Value>("deleteMessage", body).await {
            tracing::debug!(error = %e, "deleteMessage failed");
        }
    }

    pub async fn typing(&self, chat_id: i64) {
        if self.check_chat("sendChatAction", chat_id).is_err() {
            return;
        }
        let body = json!({ "chat_id": chat_id, "action": "typing" });
        if let Err(e) = self.call::<Value>("sendChatAction", body).await {
            tracing::debug!(error = %e, "sendChatAction failed");
        }
    }

    pub async fn send_document(
        &self,
        chat_id: i64,
        file_name: &str,
        bytes: Vec<u8>,
        caption: &str,
    ) -> Result<(), TgError> {
        self.check_chat("sendDocument", chat_id)?;
        self.throttle().await;
        let part = multipart::Part::bytes(bytes)
            .file_name(file_name.to_owned())
            .mime_str("text/plain")
            .map_err(|e| TgError::BadRequest(e.without_url().to_string()))?;
        let form = multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .text("caption", caption.to_owned())
            .text("parse_mode", "HTML")
            .part("document", part);
        let req = self
            .http
            .post(format!("{}sendDocument", self.base))
            .multipart(form);
        self.call_raw("sendDocument", req).await.map(|_| ())
    }

    /// Downloads a small file sent by a user.
    pub async fn download(&self, file_id: &str) -> Result<Vec<u8>, TgError> {
        let file: File = self.call("getFile", json!({ "file_id": file_id })).await?;
        if file.file_size.unwrap_or(0) as usize > MAX_DOWNLOAD {
            return Err(TgError::BadRequest("file too large".to_owned()));
        }
        let path = file
            .file_path
            .filter(|p| !p.contains("..") && !p.starts_with('/'))
            .ok_or_else(|| TgError::BadRequest("no file path".to_owned()))?;
        let mut resp = self
            .http
            .get(format!("{}{path}", self.file_base))
            .send()
            .await
            .map_err(|e| TgError::Network(e.without_url().to_string()))?;
        if !resp.status().is_success() {
            return Err(TgError::Api(
                i64::from(resp.status().as_u16()),
                "download failed".to_owned(),
            ));
        }
        let mut out = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| TgError::Network(e.without_url().to_string()))?
        {
            if out.len() + chunk.len() > MAX_DOWNLOAD {
                return Err(TgError::BadRequest("file too large".to_owned()));
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    pub async fn set_commands(
        &self,
        commands: &[BotCommand],
        chat_id: Option<i64>,
    ) -> Result<(), TgError> {
        let mut body = json!({ "commands": commands });
        if let Some(id) = chat_id {
            self.check_chat("setMyCommands", id)?;
            set(&mut body, "scope", json!({ "type": "chat", "chat_id": id }));
        }
        self.call::<Value>("setMyCommands", body).await.map(|_| ())
    }
}

fn set(body: &mut Value, key: &str, value: Value) {
    if let Some(o) = body.as_object_mut() {
        o.insert(key.to_owned(), value);
    }
}

/// Keeps a message under the Telegram limit without breaking HTML: an over-long text
/// is cut at a line break, and tags opened on cut lines are not expected in such texts.
fn fit(text: &str) -> String {
    if text.chars().count() <= MAX_TEXT {
        return text.to_owned();
    }
    let mut out = String::new();
    for line in text.lines() {
        if out.chars().count() + line.chars().count() + 2 > MAX_TEXT - 2 {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_text_is_cut_at_lines() {
        let text = "строка\n".repeat(1000);
        let cut = fit(&text);
        assert!(cut.chars().count() <= MAX_TEXT);
        assert!(cut.ends_with('…'));
        assert_eq!(fit("коротко"), "коротко");
    }

    #[tokio::test]
    async fn suppressed_outside_allowlist() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let tg = Telegram::new(
            "1:AAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            Some([1].into_iter().collect()),
        )
        .unwrap();
        assert!(matches!(
            tg.send(2, "x", None, false).await,
            Err(TgError::Suppressed)
        ));
        assert!(tg.test_mode());
    }
}
