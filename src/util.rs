use std::fmt::Write as _;

pub const STATION_URL_PREFIX: &str = "https://drova.io/stations/";

pub fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

pub fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

pub fn station_url(uuid: &str) -> String {
    format!("{STATION_URL_PREFIX}{uuid}")
}

/// Returns the lowercase station UUID from a Drova station link like
/// `https://drova.io/stations/<uuid>`.
pub fn parse_station_link(token: &str) -> Option<String> {
    let token = token
        .trim()
        .trim_end_matches(['.', ',', ';', '!', '?', ')', '"', ']', '\'']);
    let lower = token.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .unwrap_or(&lower);
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let path = rest.strip_prefix("drova.io/")?;
    let path = path.split(['?', '#']).next().unwrap_or_default();
    if !path.starts_with("stations/") {
        return None;
    }
    let last = path.trim_end_matches('/').rsplit('/').next()?;
    is_uuid(last).then(|| last.to_owned())
}

/// All distinct station UUIDs mentioned as links in free text, in order of appearance.
pub fn extract_station_ids(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for token in text.split(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '(')) {
        if let Some(id) = parse_station_link(token)
            && !out.contains(&id)
        {
            out.push(id);
        }
    }
    out
}

pub fn first_station_id(text: &str) -> Option<String> {
    extract_station_ids(text).into_iter().next()
}

pub fn html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Cuts a string to at most `max` characters, adding an ellipsis when cut.
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

pub fn format_duration(seconds: i64) -> String {
    let total = seconds.max(0);
    let days = total / 86_400;
    let hours = total % 86_400 / 3_600;
    let minutes = total % 3_600 / 60;
    let mut out = String::new();
    if days > 0 {
        let _ = write!(out, "{days} д");
    }
    if hours > 0 {
        if !out.is_empty() {
            out.push(' ');
        }
        let _ = write!(out, "{hours} ч");
    }
    if minutes > 0 || out.is_empty() {
        if !out.is_empty() {
            out.push(' ');
        }
        let _ = write!(out, "{minutes} мин");
    }
    out
}

pub fn plural<'a>(n: i64, one: &'a str, few: &'a str, many: &'a str) -> &'a str {
    let n = n.abs();
    if (11..=19).contains(&(n % 100)) {
        return many;
    }
    match n % 10 {
        1 => one,
        2..=4 => few,
        _ => many,
    }
}

pub fn users_count(n: i64) -> String {
    format!(
        "{n} {}",
        plural(n, "пользователь", "пользователя", "пользователей")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "05225c83-dd3b-4c12-832b-48227262f960";

    #[test]
    fn links_are_parsed_like_before() {
        let want = Some(ID.to_owned());
        assert_eq!(
            parse_station_link(&format!("https://drova.io/stations/{ID}")),
            want
        );
        assert_eq!(
            parse_station_link(&format!("https://www.drova.io/stations/{ID}/")),
            want
        );
        assert_eq!(
            parse_station_link(&format!("HTTP://DROVA.IO/stations/{}", ID.to_uppercase())),
            want
        );
        assert_eq!(
            parse_station_link(&format!("https://drova.io/stations/{ID}?a=1).")),
            want
        );
        assert_eq!(parse_station_link(&format!("drova.io/stations/{ID}")), want);
        assert_eq!(
            parse_station_link(&format!("https://evil.io/stations/{ID}")),
            None
        );
        assert_eq!(
            parse_station_link(&format!("https://drova.io/x/{ID}")),
            None
        );
        assert_eq!(
            parse_station_link("https://drova.io/stations/not-a-uuid"),
            None
        );
        assert_eq!(
            parse_station_link(&format!("https://drova.io.evil/stations/{ID}")),
            None
        );
    }

    #[test]
    fn extracts_unique_ids_from_text() {
        let text = format!(
            "a https://drova.io/stations/{ID}\n(https://drova.io/stations/{ID}) <https://drova.io/stations/00009bd4-fbe3-4b1a-b14a-df438f5f0018>"
        );
        assert_eq!(
            extract_station_ids(&text),
            vec![
                ID.to_owned(),
                "00009bd4-fbe3-4b1a-b14a-df438f5f0018".to_owned()
            ]
        );
        assert_eq!(extract_station_ids("").len(), 0);
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(0), "0 мин");
        assert_eq!(format_duration(59), "0 мин");
        assert_eq!(format_duration(3_600), "1 ч");
        assert_eq!(format_duration(90_061), "1 д 1 ч 1 мин");
        assert_eq!(format_duration(-5), "0 мин");
    }

    #[test]
    fn plurals() {
        assert_eq!(users_count(1), "1 пользователь");
        assert_eq!(users_count(3), "3 пользователя");
        assert_eq!(users_count(11), "11 пользователей");
        assert_eq!(users_count(21), "21 пользователь");
        assert_eq!(users_count(0), "0 пользователей");
    }

    #[test]
    fn escapes_html() {
        assert_eq!(
            html("<b>&\"x\"</b>"),
            "&lt;b&gt;&amp;&quot;x&quot;&lt;/b&gt;"
        );
        assert_eq!(clip("абвгд", 3), "аб…");
        assert_eq!(clip("аб", 3), "аб");
    }
}
