use chrono::{
    DateTime, Datelike, Days, Local, NaiveDate, NaiveDateTime, NaiveTime, Offset, TimeDelta,
    TimeZone,
};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

/// One of the time ranges the Usage page offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub enum Range {
    #[serde(rename = "today")]
    Today,
    #[serde(rename = "yesterday")]
    Yesterday,
    #[serde(rename = "7d")]
    Week,
    #[serde(rename = "30d")]
    Month,
    #[serde(rename = "6m")]
    HalfYear,
}

impl Range {
    /// Every range in the order the page shows them.
    pub const ALL: [Range; 5] = [
        Range::Today,
        Range::Yesterday,
        Range::Week,
        Range::Month,
        Range::HalfYear,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Range::Today => "today",
            Range::Yesterday => "yesterday",
            Range::Week => "7d",
            Range::Month => "30d",
            Range::HalfYear => "6m",
        }
    }

    /// The range a query names, 7d when it names none.
    pub fn parse(value: &str) -> Range {
        Range::ALL
            .into_iter()
            .find(|r| r.as_str() == value)
            .unwrap_or(Range::Week)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Timeframe {
    pub start: DateTime<Tz>,
    pub previous_start: DateTime<Tz>,
    pub end: DateTime<Tz>,
}

/// The instant a wall-clock time names in the zone, resolved as Go's time.Date does:
/// a repeated hour reads as its first pass, a skipped one with the offset after the jump.
pub(crate) fn at_local(zone: Tz, wall: NaiveDateTime) -> DateTime<Tz> {
    let offset =
        |utc: &NaiveDateTime| zone.offset_from_utc_datetime(utc).fix().local_minus_utc() as i64;
    let guess = offset(&wall);
    let mut utc = wall - TimeDelta::seconds(guess);
    let actual = offset(&utc);
    if actual != guess {
        utc = wall - TimeDelta::seconds(actual);
    }
    zone.from_utc_datetime(&utc)
}

fn start_of_day(t: DateTime<Tz>) -> DateTime<Tz> {
    at_local(t.timezone(), t.date_naive().and_time(NaiveTime::MIN))
}

fn add_days(t: DateTime<Tz>, days: i64) -> DateTime<Tz> {
    let wall = t.naive_local();
    let date = if days >= 0 {
        wall.date() + Days::new(days as u64)
    } else {
        wall.date() - Days::new(days.unsigned_abs())
    };
    at_local(t.timezone(), date.and_time(wall.time()))
}

/// Keeps the day of month, clamped to the target month's last day.
fn add_months(t: DateTime<Tz>, months: i32) -> DateTime<Tz> {
    let wall = t.naive_local();
    let total = wall.year() * 12 + wall.month0() as i32 + months;
    let (year, month) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
    let day = wall.day().min(days_in_month(year, month));
    let date = NaiveDate::from_ymd_opt(year, month, day).expect("a clamped day exists");
    at_local(t.timezone(), date.and_time(wall.time()))
}

pub(crate) fn days_in_month(year: i32, month: u32) -> u32 {
    let next = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    NaiveDate::from_ymd_opt(next.0, next.1, 1)
        .and_then(|d| d.pred_opt())
        .map_or(31, |d| d.day())
}

/// token-tracker's timeframe: calendar days in the zone, and 7d and 30d counted back from now.
pub fn usage_timeframe(r: Range, zone: Tz, now: DateTime<Tz>) -> Timeframe {
    let current = now.with_timezone(&zone);
    let today = start_of_day(current);
    let end = start_of_day(add_days(today, 1));
    match r {
        Range::Today => Timeframe {
            start: today,
            previous_start: add_days(today, -1),
            end,
        },
        Range::Yesterday => {
            let start = add_days(today, -1);
            Timeframe {
                start,
                previous_start: add_days(start, -1),
                end: today,
            }
        }
        Range::HalfYear => {
            let start = add_months(end, -6);
            Timeframe {
                start,
                previous_start: add_months(start, -6),
                end,
            }
        }
        Range::Week | Range::Month => {
            let days = if r == Range::Week { 7 } else { 30 };
            let start = add_days(current, -days);
            Timeframe {
                start,
                previous_start: add_days(start, -days),
                end,
            }
        }
    }
}

/// The starts of each chart bucket in the range: hours step in exact time, days and weeks on the calendar.
pub fn bucket_starts(r: Range, frame: &Timeframe) -> (&'static str, Vec<i64>) {
    let mut at = frame.start;
    if matches!(r, Range::Week | Range::Month) {
        at = start_of_day(at);
    }
    let step = |t: DateTime<Tz>| match r {
        Range::Week => t + TimeDelta::hours(4),
        Range::Month => add_days(t, 1),
        Range::HalfYear => add_days(t, 7),
        Range::Today | Range::Yesterday => t + TimeDelta::hours(1),
    };
    let size = match r {
        Range::Week => "4h",
        Range::Month => "1d",
        Range::HalfYear => "1w",
        Range::Today | Range::Yesterday => "1h",
    };
    let end = frame.end.timestamp_millis();
    let mut starts = Vec::new();
    while at.timestamp_millis() < end {
        starts.push(at.timestamp_millis());
        at = step(at);
    }
    (size, starts)
}

/// Reads a timestamp the way JavaScript's Date.parse does for ISO strings, NaN when it can't.
pub fn parse_millis(value: &str) -> f64 {
    if value.as_bytes().get(10) == Some(&b'T') {
        if let Ok(t) = DateTime::parse_from_rfc3339(value) {
            return t.timestamp_millis() as f64;
        }
        if let Ok(wall) = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
            && let Some(t) = Local.from_local_datetime(&wall).earliest()
        {
            return t.timestamp_millis() as f64;
        }
    } else if value.len() == 10
        && let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d")
    {
        return date.and_time(NaiveTime::MIN).and_utc().timestamp_millis() as f64;
    }
    f64::NAN
}

/// The first day of the month an instant falls in, in the zone.
pub(crate) fn month_start(zone: Tz, millis: f64) -> DateTime<Tz> {
    let start = zone
        .timestamp_millis_opt(millis as i64)
        .single()
        .expect("an instant has one wall time");
    let first = NaiveDate::from_ymd_opt(start.year(), start.month(), 1).expect("the first exists");
    at_local(zone, first.and_time(NaiveTime::MIN))
}

pub(crate) fn next_month(t: DateTime<Tz>) -> DateTime<Tz> {
    let (year, month) = if t.month() == 12 {
        (t.year() + 1, 1)
    } else {
        (t.year(), t.month() + 1)
    };
    let first = NaiveDate::from_ymd_opt(year, month, 1).expect("the first exists");
    at_local(t.timezone(), first.and_time(NaiveTime::MIN))
}

const NAMESPACES: &[(&str, &str)] = &[
    ("openai", "openai"),
    ("anthropic", "anthropic"),
    ("xai", "xai"),
    ("x-ai", "xai"),
    ("google", "google"),
    ("gemini", "google"),
    ("google_genai", "google"),
    ("vertex_ai", "google"),
    ("vertex-ai", "google"),
    ("deepseek", "deepseek"),
    ("qwen", "alibaba"),
    ("alibaba", "alibaba"),
    ("dashscope", "alibaba"),
    ("mistral", "mistral"),
    ("mistralai", "mistral"),
    ("meta", "meta"),
    ("meta-llama", "meta"),
    ("moonshot", "moonshot"),
    ("moonshotai", "moonshot"),
    ("zai", "zai"),
    ("z-ai", "zai"),
    ("zhipu", "zai"),
    ("cohere", "cohere"),
    ("amazon", "amazon"),
    ("aws", "amazon"),
    ("bedrock", "amazon"),
    ("bedrock_converse", "amazon"),
    ("bedrock_mantle", "amazon"),
    ("azure", "azure"),
    ("azure_ai", "azure"),
    ("openrouter", "openrouter"),
    ("together", "together"),
    ("together_ai", "together"),
    ("groq", "groq"),
    ("perplexity", "perplexity"),
    ("nvidia", "nvidia"),
    ("huggingface", "huggingface"),
    ("fireworks", "fireworks"),
    ("fireworks_ai", "fireworks"),
];

const FAMILIES: &[(&str, &[&str])] = &[
    (
        "openai",
        &["gpt-", "chatgpt-", "o1-", "o3-", "o4-", "codex-mini-"],
    ),
    ("anthropic", &["claude-"]),
    ("google", &["gemini-", "gemma-"]),
    ("xai", &["grok-"]),
    ("deepseek", &["deepseek-"]),
    ("alibaba", &["qwen"]),
    (
        "mistral",
        &[
            "mistral-",
            "mixtral-",
            "codestral-",
            "magistral-",
            "ministral-",
            "devstral-",
        ],
    ),
    (
        "meta",
        &["llama-", "llama2-", "llama3-", "llama4-", "muse-"],
    ),
    ("moonshot", &["kimi-", "moonshot-"]),
    ("zai", &["glm-"]),
    ("cohere", &["command-r", "command-a"]),
    ("amazon", &["amazon.nova-"]),
];

/// The company behind a model id.
pub fn provider(model: &str) -> &'static str {
    let normalized = model.trim().to_lowercase();
    if let Some((namespace, _)) = normalized.split_once('/') {
        return NAMESPACES
            .iter()
            .find(|(name, _)| *name == namespace)
            .map_or("unknown", |(_, company)| company);
    }
    for (company, prefixes) in FAMILIES {
        if prefixes.iter().any(|prefix| normalized.starts_with(prefix)) {
            return company;
        }
    }
    match normalized.as_str() {
        "o1" | "o3" | "o4" => "openai",
        _ => "unknown",
    }
}

/// The root collation's order of ASCII punctuation, symbols and digits, which
/// JavaScript's localeCompare sorts before letters.
const ASCII_ORDER: &str = " _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$0123456789";

fn primary(c: char) -> u32 {
    if let Some(i) = ASCII_ORDER.find(c) {
        return i as u32;
    }
    let base = ASCII_ORDER.len() as u32;
    if c.is_ascii_alphabetic() {
        return base + (c.to_ascii_lowercase() as u32 - 'a' as u32);
    }
    base + 26 + c as u32
}

/// Orders strings the way JavaScript's localeCompare does for the ASCII ids sessions
/// use: letters ignore case first, then lowercase leads.
pub fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    a.chars()
        .zip(b.chars())
        .map(|(x, y)| primary(x).cmp(&primary(y)))
        .find(|order| order.is_ne())
        .unwrap_or_else(|| a.chars().count().cmp(&b.chars().count()))
        .then_with(|| match a.chars().zip(b.chars()).find(|(x, y)| x != y) {
            Some((x, _)) if x.is_lowercase() => std::cmp::Ordering::Less,
            Some(_) => std::cmp::Ordering::Greater,
            None => std::cmp::Ordering::Equal,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(zone: Tz, value: &str) -> DateTime<Tz> {
        at_local(
            zone,
            NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M").unwrap(),
        )
    }

    #[test]
    fn buckets_follow_the_zone() {
        let la: Tz = "America/Los_Angeles".parse().unwrap();
        let cases = [
            (
                Range::Today,
                "2026-11-01 08:00",
                "1h",
                25,
                "2026-11-01 00:00",
            ),
            (
                Range::Today,
                "2026-03-08 12:00",
                "1h",
                23,
                "2026-03-08 00:00",
            ),
            (
                Range::Yesterday,
                "2026-11-02 08:00",
                "1h",
                25,
                "2026-11-01 00:00",
            ),
            (
                Range::Week,
                "2026-11-01 08:30",
                "4h",
                49,
                "2026-10-25 08:30",
            ),
            (
                Range::Month,
                "2026-11-01 08:30",
                "1d",
                31,
                "2026-10-02 08:30",
            ),
            (
                Range::HalfYear,
                "2026-08-30 10:00",
                "1w",
                27,
                "2026-02-28 00:00",
            ),
        ];
        for (r, now, size, buckets, from) in cases {
            let frame = usage_timeframe(r, la, at(la, now));
            let (got, starts) = bucket_starts(r, &frame);
            assert_eq!(
                (got, starts.len(), frame.start),
                (size, buckets, at(la, from)),
                "{} at {now}",
                r.as_str()
            );
        }
    }

    #[test]
    fn locale_compare_matches_javascript() {
        let want = [
            "a",
            "A",
            "a b",
            "a_b",
            "a-b",
            "a:b",
            "a.b",
            "a/b",
            "a0",
            "b",
            "B",
            "github:1234:1",
            "github:12345:1",
        ];
        let mut got = want.to_vec();
        got.reverse();
        got.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(got, want);
    }

    #[test]
    fn parses_iso_timestamps() {
        assert_eq!(parse_millis("1970-01-01T00:00:01.5Z"), 1500.0);
        assert_eq!(parse_millis("2026-04-24"), 1_776_988_800_000.0);
        assert!(parse_millis("yesterday").is_nan());
    }
}
