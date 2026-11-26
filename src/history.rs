use std::collections::HashMap;

use jiff::tz::TimeZone;

use crate::db::Transition;
use crate::drova::Status;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interval {
    pub status: Status,
    pub start: i64,
    pub end: i64,
    pub product_id: Option<String>,
    pub next: Option<Status>,
}

/// Splits `[start, end)` into status intervals. `rows` must be sorted by time and may
/// include the last transition before the window.
pub fn intervals(rows: &[Transition], start: i64, end: i64) -> Vec<Interval> {
    let mut out: Vec<Interval> = Vec::new();
    if end <= start {
        return out;
    }
    let mut current: Option<(Status, i64, Option<String>)> = None;
    for t in rows {
        if t.at > end {
            break;
        }
        let at = t.at.max(start);
        if let Some((status, since, product)) = current.take()
            && at > since
        {
            out.push(Interval {
                status,
                start: since,
                end: at,
                product_id: product,
                next: Some(t.to),
            });
        }
        current = Some((t.to, at, t.product_id.clone()));
    }
    if let Some((status, since, product)) = current
        && end > since
    {
        out.push(Interval {
            status,
            start: since,
            end,
            product_id: product,
            next: None,
        });
    }
    out
}

#[derive(Debug, Default, PartialEq)]
pub struct Stats {
    pub busy: i64,
    pub ready: i64,
    pub offline: i64,
    pub sessions: i64,
    pub avg_session: Option<i64>,
    pub avg_idle_between: Option<i64>,
    /// Share of online time the station was busy, percent.
    pub load: Option<i64>,
    /// Busiest local hours: (hour, busy percent of that hour).
    pub peak_hours: Vec<(u8, i64)>,
    /// (product id, sessions, busy seconds), most played first.
    pub top_games: Vec<(String, i64, i64)>,
}

fn avg(v: &[i64]) -> Option<i64> {
    (!v.is_empty()).then(|| v.iter().sum::<i64>() / v.len() as i64)
}

pub fn stats(items: &[Interval], tz: &TimeZone) -> Stats {
    let mut s = Stats::default();
    let mut sessions = Vec::new();
    let mut idle = Vec::new();
    let mut games: HashMap<String, (i64, i64)> = HashMap::new();
    let mut hour_busy = [0_i64; 24];
    let mut hour_total = [0_i64; 24];
    for i in items {
        let len = i.end - i.start;
        match i.status {
            Status::Busy => {
                s.busy += len;
                sessions.push(len);
                if let Some(p) = &i.product_id {
                    let e = games.entry(p.clone()).or_default();
                    e.0 += 1;
                    e.1 += len;
                }
            }
            Status::Ready => {
                s.ready += len;
                if i.next == Some(Status::Busy) {
                    idle.push(len);
                }
            }
            Status::Offline => s.offline += len,
        }
        if i.status != Status::Offline {
            spread_by_hour(i, tz, &mut hour_busy, &mut hour_total);
        }
    }
    s.sessions = sessions.len() as i64;
    s.avg_session = avg(&sessions);
    s.avg_idle_between = avg(&idle);
    let online = s.busy + s.ready;
    s.load = (online > 0).then(|| s.busy * 100 / online);
    let mut hours: Vec<(u8, i64)> = (0..24_u8)
        .filter_map(|h| {
            let total = *hour_total.get(usize::from(h))?;
            let busy = *hour_busy.get(usize::from(h))?;
            (total >= 1800 && busy > 0).then(|| (h, busy * 100 / total))
        })
        .collect();
    hours.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    hours.truncate(3);
    s.peak_hours = hours;
    let mut top: Vec<(String, i64, i64)> = games.into_iter().map(|(p, (n, t))| (p, n, t)).collect();
    top.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
    top.truncate(5);
    s.top_games = top;
    s
}

fn spread_by_hour(i: &Interval, tz: &TimeZone, busy: &mut [i64; 24], total: &mut [i64; 24]) {
    let mut t = i.start;
    while t < i.end {
        let Ok(ts) = jiff::Timestamp::from_second(t) else {
            return;
        };
        let dt = ts.to_zoned_datetime(tz.clone()).datetime();
        let into_hour = i64::from(dt.minute()) * 60 + i64::from(dt.second());
        let step = (3600 - into_hour).min(i.end - t).max(1);
        let h = usize::from(dt.hour().unsigned_abs());
        if let Some(x) = total.get_mut(h) {
            *x += step;
        }
        if i.status == Status::Busy
            && let Some(x) = busy.get_mut(h)
        {
            *x += step;
        }
        t += step;
    }
}

/// Local midnight `days - 1` days ago, so the window covers `days` calendar days.
pub fn calendar_start(now: i64, days: i64, tz: &TimeZone) -> i64 {
    let Ok(ts) = jiff::Timestamp::from_second(now) else {
        return now - days * 86_400;
    };
    let date = ts.to_zoned_datetime(tz.clone()).date();
    let Ok(first) = date.checked_sub(jiff::Span::new().days(days - 1)) else {
        return now - days * 86_400;
    };
    first
        .to_zoned_datetime(tz.clone())
        .map_or(now - days * 86_400, |z| z.timestamp().as_second())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(to: Status, at: i64, game: Option<&str>) -> Transition {
        Transition {
            to,
            product_id: game.map(Into::into),
            at,
        }
    }

    #[test]
    fn builds_intervals_with_state_before_window() {
        let rows = vec![
            t(Status::Ready, 0, None),
            t(Status::Busy, 200, Some("g")),
            t(Status::Ready, 500, Some("g")),
        ];
        let v = intervals(&rows, 100, 1000);
        assert_eq!(v.len(), 3);
        assert_eq!(
            v[0],
            Interval {
                status: Status::Ready,
                start: 100,
                end: 200,
                product_id: None,
                next: Some(Status::Busy)
            }
        );
        assert_eq!(v[2].end, 1000);
        assert_eq!(intervals(&rows, 10, 10).len(), 0);
        assert_eq!(intervals(&[], 0, 10).len(), 0);
    }

    #[test]
    fn computes_stats() {
        let rows = vec![
            t(Status::Ready, 0, None),
            t(Status::Busy, 3600, Some("a")),
            t(Status::Ready, 7200, Some("a")),
            t(Status::Busy, 9000, Some("b")),
            t(Status::Offline, 9900, None),
        ];
        let v = intervals(&rows, 0, 14_400);
        let s = stats(&v, &TimeZone::UTC);
        assert_eq!(s.sessions, 2);
        assert_eq!(s.busy, 3600 + 900);
        assert_eq!(s.ready, 3600 + 1800);
        assert_eq!(s.offline, 4500);
        assert_eq!(s.avg_session, Some(2250));
        assert_eq!(s.avg_idle_between, Some(2700));
        assert_eq!(s.load, Some(45));
        assert_eq!(s.top_games[0].0, "a");
        assert_eq!(s.peak_hours[0], (1, 100));
    }

    #[test]
    fn calendar_window() {
        let msk = crate::tz::resolve("Europe/Moscow");
        // 1970-01-03 12:00 UTC -> local window starts 1970-01-01 00:00 MSK = -10800
        assert_eq!(calendar_start(2 * 86_400 + 43_200, 3, &msk), -10_800);
    }
}
