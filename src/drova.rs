use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, CACHE_CONTROL, HeaderMap, HeaderValue, PRAGMA};
use serde::Deserialize;

use crate::util::is_uuid;

const API_BASE: &str = "https://services.drova.io";
const MAX_BODY: usize = 2 * 1024 * 1024;
const MAX_DESCRIPTION: usize = 20_000;
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

    /// Maps a raw Drova state the same way drova.io does. Unknown states count as offline,
    /// the flag tells the caller.
    pub fn from_raw(raw: &str) -> (Self, bool) {
        match raw.trim().to_ascii_uppercase().as_str() {
            "LISTEN" => (Self::Ready, true),
            "BUSY" | "HANDSHAKE" => (Self::Busy, true),
            "OFFLINE" => (Self::Offline, true),
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
    /// Raw HTML from the merchant.
    pub description: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hardware {
    pub cpu: Option<String>,
    pub gpus: Vec<(String, Option<u64>)>,
    pub ram_bytes: Option<u64>,
    pub bios: Option<String>,
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
    description: Option<String>,
}

#[derive(Deserialize)]
struct RawHardware {
    ram_bytes: Option<serde_json::Value>,
    bios: Option<RawVendorItem>,
    processor: Option<RawVendorItem>,
    graphic: Option<Vec<RawGpu>>,
}

#[derive(Deserialize)]
struct RawVendorItem {
    manufacturer: Option<String>,
    version: Option<String>,
}

#[derive(Deserialize)]
struct RawGpu {
    name: Option<String>,
    ram_bytes: Option<serde_json::Value>,
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

    /// Station hardware; `None` when Drova has no data for the station.
    pub async fn hardware(&self, uuid: &str) -> Result<Option<Hardware>, DrovaError> {
        if !is_uuid(uuid) {
            return Ok(None);
        }
        match self
            .get_json::<RawHardware>(&format!("{API_BASE}/server-manager/hardware/list/{uuid}"))
            .await
        {
            Ok(raw) => Ok(parse_hardware(raw)),
            Err(DrovaError::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
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
        description: raw
            .description
            .filter(|d| !d.trim().is_empty())
            .map(|d| d.chars().take(MAX_DESCRIPTION).collect()),
    })
}

fn clean(s: Option<String>) -> Option<String> {
    let s = s?.split_whitespace().collect::<Vec<_>>().join(" ");
    (!s.is_empty()).then_some(s)
}

fn bytes(v: Option<&serde_json::Value>) -> Option<u64> {
    let v = v?;
    v.as_u64()
        .or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
        .filter(|b| *b > 0)
}

fn parse_hardware(raw: RawHardware) -> Option<Hardware> {
    let hw = Hardware {
        cpu: raw.processor.and_then(|p| clean(p.version)),
        gpus: raw
            .graphic
            .unwrap_or_default()
            .into_iter()
            .filter_map(|g| Some((clean(g.name)?, bytes(g.ram_bytes.as_ref()))))
            .collect(),
        ram_bytes: bytes(raw.ram_bytes.as_ref()),
        bios: raw.bios.and_then(|b| {
            let parts: Vec<String> = [b.manufacturer, b.version]
                .into_iter()
                .filter_map(clean)
                .collect();
            (!parts.is_empty()).then(|| parts.join(" "))
        }),
    };
    (hw != Hardware::default()).then_some(hw)
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
    fn parses_hardware() {
        let raw: RawHardware = serde_json::from_str(
            r#"{"ram_bytes":68642062336,"bios":{"manufacturer":null,"version":" 1.2 "},
            "board":{"manufacturer":null,"name":null},
            "processor":{"manufacturer":"AMD","version":"AMD Ryzen 9 5900X 12-Core Processor   "},
            "graphic":[{"manufacturer":"NVIDIA","ram_bytes":12878610432,"name":"NVIDIA GeForce RTX 4070 SUPER"},{"name":null}]}"#,
        )
        .unwrap();
        let hw = parse_hardware(raw).unwrap();
        assert_eq!(
            hw.cpu.as_deref(),
            Some("AMD Ryzen 9 5900X 12-Core Processor")
        );
        assert_eq!(
            hw.gpus,
            vec![(
                "NVIDIA GeForce RTX 4070 SUPER".to_owned(),
                Some(12_878_610_432)
            )]
        );
        assert_eq!(hw.ram_bytes, Some(68_642_062_336));
        assert_eq!(hw.bios.as_deref(), Some("1.2"));
        let empty: RawHardware = serde_json::from_str(r#"{"bios":{},"graphic":[]}"#).unwrap();
        assert!(parse_hardware(empty).is_none());
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
        assert_eq!(Status::from_raw("LISTEN"), (Status::Ready, true));
        assert_eq!(Status::from_raw("FREE"), (Status::Offline, false));
    }
}
