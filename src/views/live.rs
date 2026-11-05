use foldhash::{HashMap, HashMapExt, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::identity::{Names, SessionRef, Who, label_of, row_of, who_of};
use super::limits::Binding;
use super::plans::read_json;
use super::sessions::{Session, SessionUsage};
use super::timeframe::{locale_compare, parse_millis};

const DAY_MS: f64 = 86_400_000.0;

/// A Claude session running now.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LiveSession {
    pub id: String,
    pub label: String,
    /// The Usage row key this session ranks under with by=agent.
    pub row: String,
    pub app: Option<String>,
    pub state: String,
    /// The account the pool has the session on.
    pub account: Option<String>,
    pub host: String,
    pub cwd: String,
    pub since: f64,
    /// How its Claude Code process was started, from its transcript; "" until it has one.
    pub entrypoint: String,
    /// What a blocked session waits on: its job's `needs`, else its `detail`.
    pub question: Option<String>,
    /// The reply its job suggests.
    pub reply: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Running {
    pub session_id: String,
    pub cwd: String,
    pub name: Option<String>,
    pub busy: bool,
    pub started_at: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Blocked {
    pub session_id: String,
    pub name: Option<String>,
    pub cwd: String,
    pub since: f64,
    pub question: Option<String>,
    pub reply: Option<String>,
}

/// Running Claude sessions, blocked jobs, and which hex apps are connected.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Processes {
    pub running: Vec<Running>,
    pub blocked: Vec<Blocked>,
    pub apps: Vec<(String, bool)>,
}

fn sorted_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Reads `<claude>/sessions/<pid>.json` (live only while /proc has the pid),
/// `<claude>/jobs/*/state.json` and `<hex>/state/<app>/{sessions,hub.sock}`.
pub fn read_processes(claude: &Path, hex: &Path) -> Processes {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct SessionFile {
        session_id: Option<String>,
        pid: Option<f64>,
        cwd: Option<String>,
        name: Option<String>,
        status: Option<String>,
        started_at: Option<f64>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct JobState {
        state: Option<String>,
        session_id: Option<String>,
        name: Option<String>,
        cwd: Option<String>,
        updated_at: Option<String>,
        needs: Option<Value>,
        detail: Option<Value>,
        suggested_reply: Option<Value>,
    }
    let text = |value: Option<Value>| {
        value
            .and_then(|v| v.as_str().map(String::from))
            .filter(|s| !s.is_empty())
    };

    let mut found = Processes::default();
    let mut by_id: HashMap<String, Running> = HashMap::new();
    let sessions = claude.join("sessions");
    for name in sorted_entries(&sessions) {
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        if stem.is_empty() || !stem.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Some(s) = read_json::<SessionFile>(&sessions.join(&name)) else {
            continue;
        };
        let (Some(id), Some(pid)) = (s.session_id.filter(|id| !id.is_empty()), s.pid) else {
            continue;
        };
        if !Path::new(&format!("/proc/{pid}")).exists() {
            continue;
        }
        let running = Running {
            session_id: id.clone(),
            cwd: s.cwd.unwrap_or_default(),
            name: s.name,
            busy: s.status.as_deref() == Some("busy"),
            started_at: s.started_at.unwrap_or(0.0),
        };
        by_id.insert(id, running);
    }
    found.running = by_id.into_values().collect();
    found
        .running
        .sort_by(|a, b| locale_compare(&a.session_id, &b.session_id));

    let jobs = claude.join("jobs");
    for job in sorted_entries(&jobs) {
        let Some(state) = read_json::<JobState>(&jobs.join(job).join("state.json")) else {
            continue;
        };
        let Some(id) = state.session_id.filter(|id| !id.is_empty()) else {
            continue;
        };
        if state.state.as_deref() != Some("blocked") {
            continue;
        }
        let since = parse_millis(state.updated_at.as_deref().unwrap_or_default());
        found.blocked.push(Blocked {
            session_id: id,
            name: state.name,
            cwd: state.cwd.unwrap_or_default(),
            since: if since.is_nan() { 0.0 } else { since },
            question: text(state.needs).or_else(|| text(state.detail)),
            reply: text(state.suggested_reply),
        });
    }
    found
        .blocked
        .sort_by(|a, b| locale_compare(&a.session_id, &b.session_id));

    let apps = hex.join("state");
    for app in sorted_entries(&apps) {
        let dir = apps.join(&app);
        if dir.join("sessions").exists() {
            found.apps.push((app, dir.join("hub.sock").exists()));
        }
    }
    found
}

/// Each session's latest account, for sessions the proxy saw in the last day. `now` is unix ms.
pub fn seats_of(rows: &[Binding], now: i64) -> Vec<(String, String)> {
    let since = (now as f64) - DAY_MS;
    let mut seats: Vec<(String, String)> = Vec::new();
    let mut at: HashMap<&str, usize> = HashMap::new();
    for row in rows {
        let Some(account) = row.account_id.as_deref().filter(|a| !a.is_empty()) else {
            continue;
        };
        if row.millis < since {
            continue;
        }
        match at.get(row.session_id.as_str()) {
            Some(&i) => seats[i].1 = account.to_string(),
            None => {
                at.insert(&row.session_id, seats.len());
                seats.push((row.session_id.clone(), account.to_string()));
            }
        }
    }
    seats
}

/// The live sessions and apps, as the stream's `s:<session>` and `app:<name>` keys.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Live {
    pub sessions: Vec<LiveSession>,
    pub apps: Vec<(String, bool)>,
}

impl Live {
    /// The stream's keys in order: sessions, then apps.
    pub fn entries(&self) -> Vec<(String, Value)> {
        let sessions = self.sessions.iter().map(|s| {
            (
                format!("s:{}", s.id),
                serde_json::to_value(s).expect("plain data"),
            )
        });
        let apps = self
            .apps
            .iter()
            .map(|(name, connected)| (format!("app:{name}"), json!({ "connected": connected })));
        sessions.chain(apps).collect()
    }
}

/// Builds the live sessions. `first_seen` holds when each session was first seen, for
/// those with no start time of their own; sessions no longer live leave it. `tracked`
/// is the 7d (else today) session usage; `now` is unix ms.
pub fn live_map(
    found: &Processes,
    seats: &[(String, String)],
    names: Option<&Names>,
    tracked: Option<&SessionUsage>,
    first_seen: &mut HashMap<String, f64>,
    now: i64,
) -> Live {
    let seats: HashMap<&str, &str> = seats
        .iter()
        .map(|(s, a)| (s.as_str(), a.as_str()))
        .collect();
    let mut known: HashMap<&str, &Session> = HashMap::new();
    let mut projects: HashMap<&str, &str> = HashMap::new();
    for s in tracked.map_or(&[][..], |t| &t.sessions) {
        known.insert(&s.session_id, s);
        projects.insert(&s.cwd, &s.project);
    }
    let blocked: HashMap<&str, &Blocked> = found
        .blocked
        .iter()
        .map(|job| (job.session_id.as_str(), job))
        .collect();

    let mut session = |id: &str, name: Option<&str>, cwd: &str, state: &str, since: f64| {
        let record = known.get(id).copied();
        let job = blocked.get(id).copied();
        let seen = *first_seen.entry(id.to_string()).or_insert(now as f64);
        let project = match record {
            Some(r) => r.project.as_str(),
            None => projects.get(cwd).copied().unwrap_or(cwd),
        };
        let who = |title: Option<&str>| match names {
            Some(names) => who_of(
                names,
                SessionRef {
                    session_id: id,
                    title,
                    project,
                    repository: None,
                },
            ),
            None => Who {
                kind: "project".into(),
                project: project.into(),
                ..Who::default()
            },
        };
        let title = record.and_then(|r| r.title.as_deref());
        let ranked = who(title);
        let shown = match name {
            Some(name) if !name.is_empty() && title.is_none() => who(Some(name)),
            _ => ranked.clone(),
        };
        let (mut key, mut cwd, mut since) = (id, cwd.to_string(), since);
        if let Some(r) = record {
            key = &r.key;
            if cwd.is_empty() {
                cwd = r.cwd.clone();
            }
            if since == 0.0 {
                since = r.first_at;
            }
        }
        if since == 0.0 {
            since = seen;
        }
        LiveSession {
            id: id.to_string(),
            label: label_of(&shown).0,
            row: row_of(&ranked, key),
            app: (ranked.kind == "thread").then(|| ranked.app.clone()),
            state: state.to_string(),
            account: seats.get(id).map(|a| a.to_string()),
            host: "server".into(),
            cwd,
            since,
            entrypoint: record.map(|r| r.entrypoint.clone()).unwrap_or_default(),
            question: job.and_then(|j| j.question.clone()),
            reply: job.and_then(|j| j.reply.clone()),
        }
    };

    let mut live = Live {
        sessions: Vec::new(),
        apps: found.apps.clone(),
    };
    for r in &found.running {
        let state = if blocked.contains_key(r.session_id.as_str()) {
            "blocked"
        } else if r.busy {
            "working"
        } else {
            "idle"
        };
        live.sessions.push(session(
            &r.session_id,
            r.name.as_deref(),
            &r.cwd,
            state,
            r.started_at,
        ));
    }
    for job in &found.blocked {
        if !live.sessions.iter().any(|s| s.id == job.session_id) {
            live.sessions.push(session(
                &job.session_id,
                job.name.as_deref(),
                &job.cwd,
                "blocked",
                job.since,
            ));
        }
    }
    let ids: HashSet<&str> = live.sessions.iter().map(|s| s.id.as_str()).collect();
    first_seen.retain(|id, _| ids.contains(id.as_str()));
    live
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_and_blocked_sessions() {
        let root = std::env::temp_dir().join(format!("views-live-{}", std::process::id()));
        let (claude, hex) = (root.join("claude"), root.join("hex"));
        let file = |path: std::path::PathBuf, text: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        let pid = std::process::id();
        file(
            claude.join(format!("sessions/{pid}.json")),
            r#"{"sessionId":"busy","pid":PID,"cwd":"/w","status":"busy","startedAt":5}"#
                .replace("PID", &pid.to_string())
                .as_str(),
        );
        file(
            claude.join("sessions/1.json"),
            r#"{"sessionId":"gone","pid":4294967295}"#,
        );
        file(
            claude.join("jobs/j/state.json"),
            r#"{"state":"blocked","sessionId":"asks","cwd":"/w","updatedAt":"1970-01-01T00:00:01Z","needs":"","detail":"Waiting on a prompt","suggestedReply":"go"}"#,
        );
        file(hex.join("state/telegram/sessions/x"), "");
        file(hex.join("state/telegram/hub.sock"), "");
        let found = read_processes(&claude, &hex);
        let live = live_map(
            &found,
            &[("asks".into(), "acct".into())],
            None,
            None,
            &mut HashMap::new(),
            9,
        );
        let got: Vec<_> = live
            .sessions
            .iter()
            .map(|s| {
                (
                    s.id.as_str(),
                    s.state.as_str(),
                    s.since,
                    s.account.as_deref(),
                    s.question.as_deref(),
                    s.reply.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("busy", "working", 5.0, None, None, None),
                (
                    "asks",
                    "blocked",
                    1000.0,
                    Some("acct"),
                    Some("Waiting on a prompt"),
                    Some("go")
                )
            ]
        );
        assert_eq!(live.apps, [("telegram".to_string(), true)]);
        std::fs::remove_dir_all(root).unwrap();
    }
}
