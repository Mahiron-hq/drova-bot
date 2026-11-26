use jiff::Timestamp;
use jiff::tz::{Offset, TimeZone};

pub const DEFAULT_TZ: &str = "Europe/Moscow";

pub const PRESETS: &[(&str, &str)] = &[
    ("Калининград", "Europe/Kaliningrad"),
    ("Москва", "Europe/Moscow"),
    ("Самара", "Europe/Samara"),
    ("Екатеринбург", "Asia/Yekaterinburg"),
    ("Омск", "Asia/Omsk"),
    ("Новосибирск", "Asia/Novosibirsk"),
    ("Красноярск", "Asia/Krasnoyarsk"),
    ("Иркутск", "Asia/Irkutsk"),
    ("Якутск", "Asia/Yakutsk"),
    ("Владивосток", "Asia/Vladivostok"),
    ("Магадан", "Asia/Magadan"),
    ("Камчатка", "Asia/Kamchatka"),
];

/// Accepts an IANA name (`Europe/Berlin`) or a fixed offset (`UTC+5`, `+05:30`, `GMT-3`).
/// Returns the canonical string to store.
pub fn normalize(input: &str) -> Option<String> {
    let s = input.trim();
    if s.is_empty() || s.len() > 64 {
        return None;
    }
    if let Some(minutes) = parse_offset(s) {
        return Some(format_offset(minutes));
    }
    let tz = jiff::tz::db().get(s).ok()?;
    Some(tz.iana_name().unwrap_or(s).to_owned())
}

pub fn resolve(stored: &str) -> TimeZone {
    if let Some(minutes) = parse_offset(stored)
        && let Ok(offset) = Offset::from_seconds(minutes * 60)
    {
        return TimeZone::fixed(offset);
    }
    jiff::tz::db()
        .get(stored)
        .or_else(|_| jiff::tz::db().get(DEFAULT_TZ))
        .unwrap_or(TimeZone::UTC)
}

/// Human label: preset city name or the stored value, plus the current UTC offset.
pub fn label(stored: &str) -> String {
    let tz = resolve(stored);
    let offset = tz.to_offset(Timestamp::now()).seconds() / 60;
    let name = PRESETS
        .iter()
        .find(|(_, iana)| *iana == stored)
        .map_or(stored, |(city, _)| city);
    if parse_offset(stored).is_some() {
        name.to_owned()
    } else {
        format!("{name} ({})", format_offset(offset))
    }
}

fn parse_offset(s: &str) -> Option<i32> {
    let upper = s.to_ascii_uppercase().replace(['−', '–'], "-");
    let prefixed = upper.starts_with("UTC") || upper.starts_with("GMT");
    let rest = upper.get(if prefixed { 3 } else { 0 }..)?.trim();
    if rest.is_empty() {
        return prefixed.then_some(0);
    }
    let (sign, body) = if let Some(b) = rest.strip_prefix('+') {
        (1, b)
    } else {
        (-1, rest.strip_prefix('-')?)
    };
    let (h, m) = match body.split_once(':') {
        Some((h, m)) => (h.trim().parse::<i32>().ok()?, m.trim().parse::<i32>().ok()?),
        None => (body.trim().parse::<i32>().ok()?, 0),
    };
    if !(0..=14).contains(&h) || !(0..60).contains(&m) {
        return None;
    }
    Some(sign * (h * 60 + m))
}

fn format_offset(minutes: i32) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let abs = minutes.abs();
    if abs % 60 == 0 {
        format!("UTC{sign}{}", abs / 60)
    } else {
        format!("UTC{sign}{}:{:02}", abs / 60, abs % 60)
    }
}

/// Minutes since local midnight.
pub fn local_minutes(tz: &TimeZone, at: i64) -> i32 {
    let Ok(ts) = Timestamp::from_second(at) else {
        return 0;
    };
    let dt = ts.to_zoned_datetime(tz.clone()).datetime();
    i32::from(dt.hour()) * 60 + i32::from(dt.minute())
}

pub fn in_window(minute: i32, start: i32, end: i32) -> bool {
    if start == end {
        return false;
    }
    if start < end {
        (start..end).contains(&minute)
    } else {
        minute >= start || minute < end
    }
}

/// Parses `23:00-08:00`, `23-8`, `23.00 – 8.30`.
pub fn parse_window(input: &str) -> Option<(i32, i32)> {
    let s: String = input
        .chars()
        .map(|c| {
            if matches!(c, '\u{2013}' | '\u{2014}' | '\u{2212}') {
                '-'
            } else {
                c
            }
        })
        .filter(|c| !c.is_whitespace())
        .collect();
    let (a, b) = s.split_once('-')?;
    let (a, b) = (parse_hhmm(a)?, parse_hhmm(b)?);
    (a != b).then_some((a, b))
}

fn parse_hhmm(s: &str) -> Option<i32> {
    let (h, m) = match s.split_once([':', '.']) {
        Some((h, m)) => (h.parse::<i32>().ok()?, m.parse::<i32>().ok()?),
        None => (s.parse::<i32>().ok()?, 0),
    };
    ((0..24).contains(&h) && (0..60).contains(&m)).then_some(h * 60 + m)
}

pub fn hhmm(minutes: i32) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

pub fn format_time(tz: &TimeZone, at: i64) -> String {
    hhmm(local_minutes(tz, at))
}

pub fn format_date_time(tz: &TimeZone, at: i64) -> String {
    let Ok(ts) = Timestamp::from_second(at) else {
        return String::new();
    };
    ts.to_zoned_datetime(tz.clone())
        .strftime("%d.%m %H:%M")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_and_names() {
        assert_eq!(normalize("utc+5").as_deref(), Some("UTC+5"));
        assert_eq!(normalize("+05:30").as_deref(), Some("UTC+5:30"));
        assert_eq!(normalize("GMT-3").as_deref(), Some("UTC-3"));
        assert_eq!(normalize("UTC").as_deref(), Some("UTC+0"));
        assert_eq!(normalize("Europe/Berlin").as_deref(), Some("Europe/Berlin"));
        assert_eq!(normalize("Mars/Base"), None);
        assert_eq!(normalize("UTC+15"), None);
        assert_eq!(normalize(""), None);
    }

    #[test]
    fn windows() {
        assert_eq!(parse_window("23:00-08:00"), Some((1380, 480)));
        assert_eq!(parse_window("23 – 8"), Some((1380, 480)));
        assert_eq!(parse_window("1.30-2"), Some((90, 120)));
        assert_eq!(parse_window("8-8"), None);
        assert_eq!(parse_window("25-8"), None);
        assert!(in_window(1430, 1380, 480));
        assert!(in_window(0, 1380, 480));
        assert!(!in_window(480, 1380, 480));
        assert!(in_window(100, 90, 120));
        assert!(!in_window(120, 90, 120));
    }

    #[test]
    fn local_time_uses_zone() {
        let msk = resolve("Europe/Moscow");
        assert_eq!(format_time(&msk, 0), "03:00");
        let fixed = resolve("UTC+5:30");
        assert_eq!(format_time(&fixed, 0), "05:30");
        assert_eq!(format_time(&resolve("Nope/Nope"), 0), "03:00");
    }
}
