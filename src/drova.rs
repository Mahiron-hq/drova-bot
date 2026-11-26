use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, CACHE_CONTROL, HeaderMap, HeaderValue, PRAGMA};
use serde::Deserialize;

use crate::util::is_uuid;

const API_BASE: &str = "https://services.drova.io";
const MAX_BODY: usize = 2 * 1024 * 1024;
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
pub const DESKTOP_PRODUCT: &str = "9fd0eb43-b2bb-4ce3-93b8-9df63f209098";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Status {
    Ready,
    Busy,
    Offline,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Busy => "busy",
            Self::Offline => "offline",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ready" => Some(Self::Ready),
            "busy" => Some(Self::Busy),
            "offline" => Some(Self::Offline),
            _ => None,
        }
    }

    /// Maps a raw Drova state. Unknown states count as offline, the flag tells the caller.
    pub fn from_raw(raw: &str) -> (Self, bool) {
        match raw.trim().to_ascii_uppercase().as_str() {
            "READY" | "LISTEN" | "FREE" | "AVAILABLE" | "IDLE" | "ONLINE" | "OPEN" => {
                (Self::Ready, true)
            }
            "BUSY" | "HANDSHAKE" | "OCCUPIED" | "PLAYING" | "IN_SESSION" | "RESERVED" | "TAKEN" => {
                (Self::Busy, true)
            }
            "OFFLINE" | "DISCONNECTED" | "UNAVAILABLE" | "MAINTENANCE" | "DOWN" | "INACTIVE" => {
                (Self::Offline, true)
            }
            _ => (Self::Offline, false),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub uuid: String,
    pub name: String,
    pub raw_state: String,
    pub status: Status,
    pub known_state: bool,
    pub published: bool,
    pub city: Option<String>,
    pub product_id: Option<String>,
    pub games_count: Option<i64>,
}

#[derive(Debug, thiserror::Error)]
pub enum DrovaError {
    #[error("станция не найдена (HTTP 404)")]
    NotFound,
    #[error("HTTP {0}")]
    Http(u16),
    #[error("сеть: {0}")]
    Network(String),
    #[error("ответ не разобран: {0}")]
    Parse(String),
}

#[derive(Deserialize)]
struct RawStation {
    uuid: Option<String>,
    name: Option<String>,
    state: Option<String>,
    published: Option<serde_json::Value>,
    city_name: Option<String>,
    product_id: Option<String>,
    product_list: Option<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
struct RawProduct {
    title: Option<String>,
}

#[derive(Clone)]
pub struct DrovaClient {
    http: reqwest::Client,
}

impl DrovaClient {
    pub fn new(timeout: Duration) -> anyhow::Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json,text/plain,*/*"),
        );
        headers.insert(
            ACCEPT_LANGUAGE,
            HeaderValue::from_static("ru-RU,ru;q=0.9,en-US;q=0.8,en;q=0.7"),
        );
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        headers.insert(PRAGMA, HeaderValue::from_static("no-cache"));
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(headers)
            .https_only(true)
            .redirect(reqwest::redirect::Policy::limited(3))
            .connect_timeout(Duration::from_secs(10))
            .timeout(timeout)
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .map_err(|e| anyhow::anyhow!("Drova HTTP client: {}", e.without_url()))?;
        Ok(Self { http })
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, DrovaError> {
        let mut resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| DrovaError::Network(describe(&e)))?;
        match resp.status() {
            StatusCode::OK => {}
            StatusCode::NOT_FOUND => return Err(DrovaError::NotFound),
            s => return Err(DrovaError::Http(s.as_u16())),
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| DrovaError::Network(describe(&e)))?
        {
            if body.len() + chunk.len() > MAX_BODY {
                return Err(DrovaError::Parse("ответ слишком большой".to_owned()));
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|e| DrovaError::Parse(e.to_string()))
    }

    pub async fn station(&self, uuid: &str) -> Result<Snapshot, DrovaError> {
        if !is_uuid(uuid) {
            return Err(DrovaError::Parse("неверный UUID".to_owned()));
        }
        let raw: RawStation = self
            .get_json(&format!("{API_BASE}/server-manager/servers/public/{uuid}"))
            .await?;
        parse_station(raw)
    }

    /// Game title by product id; `None` when Drova does not know the product.
    pub async fn product_title(&self, product_id: &str) -> Result<Option<String>, DrovaError> {
        if product_id == DESKTOP_PRODUCT {
            return Ok(Some(DESKTOP_TITLE.to_owned()));
        }
        if !is_uuid(product_id) {
            return Ok(None);
        }
        match self
            .get_json::<RawProduct>(&format!(
                "{API_BASE}/product-manager/product/get/{product_id}"
            ))
            .await
        {
            Ok(p) => Ok(p
                .title
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty())),
            Err(DrovaError::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

pub const DESKTOP_TITLE: &str = "Рабочий стол";

fn describe(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        return "таймаут".to_owned();
    }
    if e.is_connect() {
        return "нет соединения".to_owned();
    }
    let mut text = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        text = format!("{text}: {s}");
        source = s.source();
    }
    text
}

fn parse_station(raw: RawStation) -> Result<Snapshot, DrovaError> {
    let uuid = raw
        .uuid
        .map(|u| u.to_ascii_lowercase())
        .filter(|u| is_uuid(u))
        .ok_or_else(|| DrovaError::Parse("нет uuid".to_owned()))?;
    let name = raw
        .name
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| DrovaError::Parse("нет имени станции".to_owned()))?;
    let raw_state = raw
        .state
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| DrovaError::Parse("нет состояния".to_owned()))?;
    let (status, known_state) = Status::from_raw(&raw_state);
    let published = match raw.published {
        Some(serde_json::Value::Bool(b)) => b,
        _ => true,
    };
    Ok(Snapshot {
        uuid,
        name,
        raw_state,
        status,
        known_state,
        published,
        city: raw
            .city_name
            .map(|c| c.trim().to_owned())
            .filter(|c| !c.is_empty()),
        product_id: raw
            .product_id
            .map(|p| p.to_ascii_lowercase())
            .filter(|p| is_uuid(p)),
        games_count: raw.product_list.map(|l| l.len() as i64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(json: &str) -> RawStation {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn parses_public_station() {
        let s = parse_station(raw(
            r#"{"uuid":"05225C83-dd3b-4c12-832b-48227262f960","name":" PC ","state":"LISTEN",
            "published":false,"city_name":"St Petersburg","product_id":"b05acb00-93ab-4b6d-ab6c-792f72e43665",
            "product_list":["a","b"],"extra":1}"#,
        ))
        .unwrap();
        assert_eq!(s.uuid, "05225c83-dd3b-4c12-832b-48227262f960");
        assert_eq!(s.name, "PC");
        assert_eq!(s.status, Status::Ready);
        assert!(!s.published);
        assert_eq!(s.games_count, Some(2));
        assert_eq!(s.city.as_deref(), Some("St Petersburg"));
    }

    #[test]
    fn rejects_broken_station() {
        assert!(parse_station(raw(r#"{"uuid":"x","name":"a","state":"BUSY"}"#)).is_err());
        assert!(
            parse_station(raw(
                r#"{"uuid":"05225c83-dd3b-4c12-832b-48227262f960","name":"","state":"BUSY"}"#
            ))
            .is_err()
        );
    }

    #[test]
    fn unknown_state_is_offline_and_flagged() {
        let s = parse_station(raw(
            r#"{"uuid":"05225c83-dd3b-4c12-832b-48227262f960","name":"a","state":"WEIRD","published":"yes","product_id":"bad"}"#,
        ))
        .unwrap();
        assert_eq!(s.status, Status::Offline);
        assert!(!s.known_state);
        assert!(s.published);
        assert_eq!(s.product_id, None);
        assert_eq!(Status::from_raw("handshake").0, Status::Busy);
    }
}
