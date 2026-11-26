//! Converts the HTML description of a Drova station into short Telegram HTML.

use crate::util::html;

#[derive(Debug, PartialEq, Eq)]
struct Run {
    text: String,
    href: Option<String>,
}

#[derive(Default)]
struct Builder {
    runs: Vec<Run>,
    href: Option<String>,
    space: bool,
}

impl Builder {
    fn last_char(&self) -> Option<char> {
        self.runs.iter().rev().find_map(|r| r.text.chars().last())
    }

    fn push_char(&mut self, c: char) {
        let same = self.runs.last().is_some_and(|r| r.href == self.href);
        if !same {
            self.runs.push(Run {
                text: String::new(),
                href: self.href.clone(),
            });
        }
        if let Some(r) = self.runs.last_mut() {
            r.text.push(c);
        }
    }

    fn text(&mut self, s: &str) {
        for c in s.chars() {
            if c.is_whitespace() {
                self.space = true;
                continue;
            }
            if self.space
                && self.last_char().is_some_and(|l| l != '\n')
                && let Some(r) = self.runs.iter_mut().rev().find(|r| !r.text.is_empty())
            {
                r.text.push(' ');
            }
            self.space = false;
            self.push_char(if c == '\u{2014}' { '–' } else { c });
        }
    }

    fn newline(&mut self, force: bool) {
        self.space = false;
        let trailing = self
            .runs
            .iter()
            .rev()
            .flat_map(|r| r.text.chars().rev())
            .take_while(|c| *c == '\n')
            .count();
        let empty = self.last_char().is_none();
        if empty || trailing >= 2 || (!force && trailing >= 1) {
            return;
        }
        let href = self.href.take();
        self.push_char('\n');
        self.href = href;
    }
}

fn decode_entity(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        return char::from_u32(code);
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "laquo" => '«',
        "raquo" => '»',
        "ndash" | "mdash" => '–',
        "hellip" => '…',
        "copy" => '©',
        _ => return None,
    })
}

fn decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest[1..].find(';').map(|e| e + 1).filter(|e| *e <= 12);
        match end.and_then(|e| decode_entity(&rest[1..e]).map(|c| (c, e))) {
            Some((c, e)) => {
                out.push(c);
                rest = &rest[e + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name) {
        let start = from + i;
        from = start + name.len();
        let before_ok = lower[..start].ends_with(|c: char| c.is_whitespace());
        let after = lower[from..].trim_start();
        if !before_ok || !after.starts_with('=') {
            continue;
        }
        let value_start = tag.len() - after.len() + 1;
        let value = tag[value_start..].trim_start();
        let (quote, body) = match value.chars().next()? {
            q @ ('"' | '\'') => (Some(q), &value[1..]),
            _ => (None, value),
        };
        let end = match quote {
            Some(q) => body.find(q)?,
            None => body
                .find(|c: char| c.is_whitespace() || c == '>')
                .unwrap_or(body.len()),
        };
        return Some(decode(&body[..end]));
    }
    None
}

fn safe_link(href: &str) -> Option<String> {
    let h = href.trim();
    let lower = h.to_ascii_lowercase();
    let ok = (lower.starts_with("https://") || lower.starts_with("http://"))
        && h.len() <= 500
        && !h.chars().any(|c| c.is_whitespace() || c.is_control());
    ok.then(|| h.to_owned())
}

fn parse(src: &str) -> Vec<Run> {
    let mut b = Builder::default();
    let mut rest = src;
    let mut skip_until: Option<&str> = None;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            if skip_until.is_none() {
                b.text(&decode(rest));
            }
            break;
        };
        if skip_until.is_none() {
            b.text(&decode(&rest[..lt]));
        }
        rest = &rest[lt..];
        if rest.starts_with("<!--") {
            rest = rest.find("-->").map_or("", |e| &rest[e + 3..]);
            continue;
        }
        let Some(gt) = rest.find('>') else {
            if skip_until.is_none() {
                b.text(&decode(rest));
            }
            break;
        };
        let tag = &rest[1..gt];
        rest = &rest[gt + 1..];
        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_lowercase();
        if let Some(wait) = skip_until {
            if closing && name == wait {
                skip_until = None;
            }
            continue;
        }
        match name.as_str() {
            "script" | "style" if !closing => {
                skip_until = Some(if name == "script" { "script" } else { "style" });
            }
            "br" => b.newline(true),
            "p" | "div" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "ul" | "ol" | "tr"
            | "blockquote" | "table" => b.newline(false),
            "li" => {
                b.newline(false);
                if !closing {
                    b.text("• ");
                }
            }
            "a" => {
                b.href = if closing {
                    None
                } else {
                    attr(tag, "href").and_then(|h| safe_link(&h))
                }
            }
            _ => {}
        }
    }
    b.runs
}

/// Telegram HTML of at most `max` visible characters and whether the text was cut.
pub fn render(src: &str, max: usize) -> Option<(String, bool)> {
    let mut runs = parse(src);
    while let Some(r) = runs.last_mut() {
        let t = r.text.trim_end().len();
        r.text.truncate(t);
        if r.text.is_empty() {
            runs.pop();
        } else {
            break;
        }
    }
    let mut left = max;
    let mut cut = false;
    let mut out = String::new();
    for r in &runs {
        if left == 0 {
            cut = true;
            break;
        }
        let n = r.text.chars().count();
        let text: String = if n > left {
            cut = true;
            let taken: String = r.text.chars().take(left).collect();
            let mid_word = r.text.chars().nth(left).is_some_and(|c| !c.is_whitespace());
            match taken.rfind(char::is_whitespace) {
                Some(ws) if mid_word => taken[..ws].trim_end().to_owned(),
                _ => taken.trim_end().to_owned(),
            }
        } else {
            r.text.clone()
        };
        left = left.saturating_sub(n);
        if text.trim().is_empty() {
            out.push_str(&text);
            continue;
        }
        match &r.href {
            Some(h) => {
                out.push_str("<a href=\"");
                out.push_str(&html(h));
                out.push_str("\">");
                out.push_str(&html(&text));
                out.push_str("</a>");
            }
            None => out.push_str(&html(&text)),
        }
        if cut {
            break;
        }
    }
    let out = out.trim_end().to_owned();
    (!out.is_empty()).then_some((out, cut))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_drova_html() {
        let src = "<h1 style=\"x\">My name&#39;s <b>Go &amp; Fun</b>!</h1><br /><p>Line   one</p><p>Ask: \
                   <a href=\"https://t.me/maksi_p\" rel=\"nofollow\">&#64;maksi_p</a>.</p>\
                   <img src=\"https://x/y.jpg\" /><script>alert(1)</script><p>&lt;tag&gt; &#x1f3ae;</p>";
        let (out, cut) = render(src, 500).unwrap();
        assert!(!cut);
        assert_eq!(
            out,
            "My name's Go &amp; Fun!\n\nLine one\nAsk: <a href=\"https://t.me/maksi_p\">@maksi_p</a>.\n&lt;tag&gt; \u{1f3ae}"
        );
    }

    #[test]
    fn cuts_and_rejects_bad_links() {
        let (out, cut) = render("<p>абвгд ежз</p>", 7).unwrap();
        assert!(cut);
        assert_eq!(out, "абвгд");
        let (out, _) = render(
            "<a href=\"javascript:alert(1)\">x</a> <a href='http://a.b/?q=\"'>y</a>",
            50,
        )
        .unwrap();
        assert_eq!(out, "x <a href=\"http://a.b/?q=&quot;\">y</a>");
        assert!(render("<p> </p><br>", 10).is_none());
        assert_eq!(
            render("a &unknown; &#999999999; b", 50).unwrap().0,
            "a &amp;unknown; &amp;#999999999; b"
        );
    }
}
