use foldhash::{HashMap, HashMapExt, HashSet, HashSetExt};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

use super::sessions::Prepared;
use super::timeframe::parse_millis;

/// One row of the pool's per-session account log (attribution.ndjson).
#[derive(Debug, Clone, PartialEq)]
pub struct Binding {
    pub at: String,
    pub session_id: String,
    pub account_id: Option<String>,
    pub event: String,
    pub cause: String,
    pub millis: f64,
}

/// One sample of an account's limits (usage-history.ndjson).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitReading {
    pub account: String,
    pub at: f64,
    #[serde(default)]
    pub five_hour: Option<f64>,
    #[serde(default)]
    pub week: Option<f64>,
    #[serde(default)]
    pub five_hour_resets_at: Option<f64>,
    #[serde(default)]
    pub week_resets_at: Option<f64>,
}

/// A limit window's kind, in the order points and charges index them.
pub const KINDS: [&str; 2] = ["fiveHour", "week"];

pub(crate) fn kind_index(kind: &str) -> usize {
    usize::from(kind != KINDS[0])
}

impl LimitReading {
    pub(crate) fn value(&self, kind: usize) -> Option<f64> {
        [self.five_hour, self.week][kind]
    }

    pub(crate) fn resets_at(&self, kind: usize) -> Option<f64> {
        [self.five_hour_resets_at, self.week_resets_at][kind]
    }
}

pub fn parse_reading(line: &[u8]) -> Option<LimitReading> {
    serde_json::from_slice(line).ok()
}

pub fn parse_binding(line: &[u8]) -> Option<Binding> {
    let row: Value = serde_json::from_slice(line).ok()?;
    let text = |key: &str| {
        row.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let (at, session_id, event) = (text("at"), text("sessionId"), text("event"));
    let millis = parse_millis(&at);
    if at.is_empty() || millis.is_nan() || session_id.is_empty() {
        return None;
    }
    if !matches!(event.as_str(), "start" | "failure" | "observe-live") {
        return None;
    }
    let account_id = match row.get("accountId") {
        Some(Value::String(account)) => Some(account.clone()),
        None | Some(Value::Null) if event == "observe-live" => None,
        _ => return None,
    };
    Some(Binding {
        at,
        session_id,
        account_id,
        event,
        cause: text("cause"),
        millis,
    })
}

/// Follows an append-only ndjson file, parsing only the bytes added since the
/// last read; a file that shrank is read again from the start.
pub struct AppendLog<T> {
    path: PathBuf,
    parse: fn(&[u8]) -> Option<T>,
    size: u64,
    rest: Vec<u8>,
    rows: Vec<T>,
}

impl<T> AppendLog<T> {
    pub fn new(path: impl Into<PathBuf>, parse: fn(&[u8]) -> Option<T>) -> Self {
        AppendLog {
            path: path.into(),
            parse,
            size: 0,
            rest: Vec::new(),
            rows: Vec::new(),
        }
    }

    /// Every row so far, and whether this read added or dropped any.
    pub fn read(&mut self) -> (&[T], bool) {
        let size = std::fs::metadata(&self.path).map_or(0, |m| m.len());
        if size < self.size {
            self.size = 0;
            self.rest.clear();
            self.rows.clear();
        }
        if size == self.size {
            return (&self.rows, false);
        }
        let Ok(mut file) = File::open(&self.path) else {
            return (&self.rows, false);
        };
        let mut added = Vec::new();
        if file.seek(SeekFrom::Start(self.size)).is_err()
            || file.take(size - self.size).read_to_end(&mut added).is_err()
        {
            return (&self.rows, false);
        }
        self.size += added.len() as u64;
        self.rest.extend_from_slice(&added);
        if let Some(cut) = self.rest.iter().rposition(|&b| b == b'\n') {
            let complete: Vec<u8> = self.rest.drain(..=cut).collect();
            self.rows
                .extend(complete.split(|&b| b == b'\n').filter_map(self.parse));
        }
        (&self.rows, true)
    }
}

/// Readings in time order, keeping file order for ties.
pub fn sorted_readings(rows: &[LimitReading]) -> Vec<LimitReading> {
    let mut out = rows.to_vec();
    out.sort_by(|a, b| a.at.partial_cmp(&b.at).unwrap_or(std::cmp::Ordering::Equal));
    out
}

fn by_millis(rows: &mut [Binding]) {
    rows.sort_by(|a, b| {
        a.millis
            .partial_cmp(&b.millis)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// token-tracker's attribution history: account-bound hooks, plus process scans for
/// sessions no hook named, where a scan that saw several accounts within a second
/// binds the session to none.
pub fn binding_history(all: &[Binding]) -> Vec<Binding> {
    let mut rows = all.to_vec();
    by_millis(&mut rows);
    let mut authoritative = HashSet::new();
    let mut history = Vec::new();
    for row in rows.iter().filter(|row| row.event != "observe-live") {
        if row.account_id.as_deref().is_some_and(|a| !a.is_empty()) {
            authoritative.insert(row.session_id.as_str());
        }
        history.push(row.clone());
    }
    let mut order = Vec::new();
    let mut observed: HashMap<&str, Vec<&Binding>> = HashMap::new();
    for row in &rows {
        if row.event != "observe-live" || authoritative.contains(row.session_id.as_str()) {
            continue;
        }
        observed
            .entry(&row.session_id)
            .or_insert_with(|| {
                order.push(row.session_id.as_str());
                Vec::new()
            })
            .push(row);
    }
    for session in order {
        let observations = &observed[session];
        let mut i = 0;
        while i < observations.len() {
            let mut first = observations[i].clone();
            let mut accounts: HashSet<Option<String>> =
                [first.account_id.clone()].into_iter().collect();
            i += 1;
            while i < observations.len() && observations[i].millis - first.millis <= 1000.0 {
                accounts.insert(observations[i].account_id.clone());
                i += 1;
            }
            if accounts.len() != 1 {
                first.account_id = None;
            }
            history.push(first);
        }
    }
    by_millis(&mut history);
    history
}

/// Finds the account a Claude request's session was bound to at the request's own time.
pub struct AccountLookup {
    by_session: HashMap<String, Vec<(f64, Option<String>)>>,
}

impl AccountLookup {
    pub fn new(history: &[Binding]) -> Self {
        let mut by_session: HashMap<String, Vec<(f64, Option<String>)>> = HashMap::new();
        for row in history {
            by_session
                .entry(row.session_id.clone())
                .or_default()
                .push((row.millis, row.account_id.clone()));
        }
        AccountLookup { by_session }
    }

    pub fn account_of<'a>(&'a self, event: &'a Prepared) -> &'a str {
        if !event.account_id.is_empty() {
            return &event.account_id[..];
        }
        let Some(list) = self.by_session.get(&*event.session_id) else {
            return "";
        };
        if &*event.harness != "claude" || event.millis.is_nan() {
            return "";
        }
        let at = list.partition_point(|(millis, _)| *millis <= event.millis);
        match at.checked_sub(1).map(|i| &list[i].1) {
            Some(Some(account)) => account,
            _ => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::Row;
    use crate::views::sessions::prepare;

    fn bind(session: &str, account: &str, event: &str, millis: f64) -> Binding {
        Binding {
            at: String::new(),
            session_id: session.into(),
            account_id: Some(account.into()),
            event: event.into(),
            cause: String::new(),
            millis,
        }
    }

    #[test]
    fn scans_that_saw_two_accounts_bind_none() {
        let history = binding_history(&[
            bind("hooked", "a", "start", 10.0),
            bind("hooked", "b", "observe-live", 20.0),
            bind("scanned", "a", "observe-live", 30.0),
            bind("scanned", "b", "observe-live", 900.0),
            bind("scanned", "b", "observe-live", 5000.0),
        ]);
        let lookup = AccountLookup::new(&history);
        let at = |session: &str, ms: f64| {
            let rows = [Row {
                harness: "claude".into(),
                session_id: session.into(),
                millis: ms,
                ..Default::default()
            }];
            lookup.account_of(&prepare(&rows)[0]).to_string()
        };
        assert_eq!(
            [
                at("hooked", 30.0),
                at("scanned", 100.0),
                at("scanned", 6000.0),
                at("scanned", 10.0)
            ],
            ["a", "", "b", ""]
        );
    }

    #[test]
    fn append_log_reads_only_what_was_added() {
        let dir = std::env::temp_dir().join(format!("views-append-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("usage-history.ndjson");
        let write = |text: &str, append: bool| {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(append)
                .truncate(!append)
                .open(&path)
                .unwrap();
            file.write_all(text.as_bytes()).unwrap();
        };
        let mut log = AppendLog::new(&path, parse_reading);
        write(
            "{\"account\":\"a\",\"at\":1,\"fiveHour\":1,\"week\":2}\n{\"account\":\"a\",\"at\":2,",
            false,
        );
        assert_eq!(log.read().0.len(), 1);
        write(
            "\"fiveHour\":null,\"week\":3}\nnot json\n{\"at\":3}\n",
            true,
        );
        let rows = log.read().0;
        assert_eq!(
            (rows.len(), rows[1].five_hour, rows[1].week),
            (2, None, Some(3.0))
        );
        write(
            "{\"account\":\"b\",\"at\":9,\"fiveHour\":0,\"week\":0}\n",
            false,
        );
        let rows = log.read().0;
        assert_eq!((rows.len(), rows[0].account.as_str()), (1, "b"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bindings_keep_only_known_events() {
        let parse = |line: &str| parse_binding(line.as_bytes()).map(|b| b.account_id);
        assert_eq!(
            parse(
                r#"{"at":"2026-10-14T06:00:03.181Z","sessionId":"s","accountId":"a","event":"start"}"#
            ),
            Some(Some("a".into()))
        );
        assert_eq!(
            parse(
                r#"{"at":"2026-10-14T06:00:03.181Z","sessionId":"s","accountId":null,"event":"observe-live"}"#
            ),
            Some(None)
        );
        assert_eq!(
            parse(
                r#"{"at":"2026-10-14T06:00:03.181Z","sessionId":"s","accountId":null,"event":"start"}"#
            ),
            None
        );
        assert_eq!(
            parse(
                r#"{"at":"2026-10-14T06:00:03.181Z","sessionId":"s","accountId":"a","event":"move"}"#
            ),
            None
        );
    }
}
