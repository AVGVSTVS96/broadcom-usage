use foldhash::{HashMap, HashMapExt};
use std::path::Path;
use std::process::Command;
use std::sync::LazyLock;

use regex::Regex;
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};

/// Who a usage row belongs to. Threads are keyed by app and thread id; a name alone never joins.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Who {
    pub kind: String,
    pub app: String,
    pub thread: String,
    pub name: String,
    pub owner: Option<String>,
    pub unit: String,
    pub title: String,
    pub project: String,
    pub repo: String,
    pub workflow: String,
    pub run: Option<String>,
    pub href: Option<String>,
}

impl Serialize for Who {
    /// Exactly the fields each kind has, nulls included.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("kind", &self.kind)?;
        match self.kind.as_str() {
            "thread" => {
                map.serialize_entry("app", &self.app)?;
                map.serialize_entry("thread", &self.thread)?;
                map.serialize_entry("name", &self.name)?;
                map.serialize_entry("href", &self.href)?;
            }
            "job" => {
                map.serialize_entry("owner", &self.owner)?;
                map.serialize_entry("unit", &self.unit)?;
            }
            "session" => {
                map.serialize_entry("title", &self.title)?;
                map.serialize_entry("project", &self.project)?;
                map.serialize_entry("href", &self.href)?;
            }
            "action" => {
                map.serialize_entry("repo", &self.repo)?;
                map.serialize_entry("workflow", &self.workflow)?;
                map.serialize_entry("run", &self.run)?;
                map.serialize_entry("href", &self.href)?;
            }
            _ => map.serialize_entry("project", &self.project)?,
        }
        map.end()
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Thread {
    app: String,
    thread: String,
    name: String,
}

/// hex's threads, for naming sessions.
#[derive(Debug, Clone, Default)]
pub struct Names {
    /// Thread by its current Claude session, from `hex threads`.
    current: HashMap<String, Thread>,
    /// Thread by the first 8 characters of a past session id, from `hex log`
    /// messages whose meta names the session's log.
    logged: HashMap<String, Thread>,
    /// Threads by name, across apps.
    named: HashMap<String, Vec<Thread>>,
}

/// What names a session: its id, latest title and project.
#[derive(Debug, Clone, Copy)]
pub struct SessionRef<'a> {
    pub session_id: &'a str,
    pub title: Option<&'a str>,
    pub project: &'a str,
    pub repository: Option<&'a str>,
}

static JOB_TITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.+)/([0-9A-Za-z_.@-]+)$").unwrap());
/// tokenhub's GitHub import names each session `github:<run>:<attempt>:…` and titles it `<workflow> #<run>`.
static RUN_SESSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^github:([0-9]+):").unwrap());
static RUN_NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" #[0-9]+$").unwrap());
static PAST_LOG_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-([0-9a-f]{8})\.md$").unwrap());

fn thread_link(app: &str, id: &str) -> Option<String> {
    match app {
        "t3" => Some(format!("t3-thread://v1/{id}")),
        "telegram" => {
            let (chat, topic) = id.split_once(':').unwrap_or((id, ""));
            let internal = chat.strip_prefix("-100").unwrap_or(chat);
            Some(if topic.is_empty() {
                format!("https://t.me/c/{internal}")
            } else {
                format!("https://t.me/c/{internal}/{topic}")
            })
        }
        _ => None,
    }
}

pub(crate) fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or_default()
}

/// The first `n` bytes of an id, or all of it.
pub(crate) fn prefix(id: &str, n: usize) -> &str {
    id.get(..n).unwrap_or(id)
}

/// Names a session, in order: a GitHub Actions run, by tokenhub's session id; its
/// thread's current session; a past session of a thread, by hex's log; a title that
/// names exactly one thread; a bin/job title `<owner>/<unit>`; any other title; else
/// its project. A name alone never joins two threads.
pub fn who_of(names: &Names, s: SessionRef) -> Who {
    let project = s.repository.unwrap_or(s.project).to_string();
    if let Some(run) = RUN_SESSION.captures(s.session_id) {
        let repo = project
            .strip_prefix("https://github.com/")
            .unwrap_or(&project)
            .to_string();
        let workflow = s
            .title
            .map(|title| RUN_NUMBER.replace(title, "").trim().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "workflow".into());
        let run = run[1].to_string();
        let href = Some(format!("https://github.com/{repo}/actions/runs/{run}"));
        return Who {
            kind: "action".into(),
            repo,
            workflow,
            run: Some(run),
            href,
            ..Who::default()
        };
    }
    let title = s.title.map(str::trim).unwrap_or_default();
    let found = names
        .current
        .get(s.session_id)
        .or_else(|| names.logged.get(prefix(s.session_id, 8)))
        .or_else(|| match names.named.get(title) {
            Some(threads) if !title.is_empty() && threads.len() == 1 => threads.first(),
            _ => None,
        });
    if let Some(t) = found {
        return Who {
            kind: "thread".into(),
            app: t.app.clone(),
            thread: t.thread.clone(),
            name: t.name.clone(),
            href: thread_link(&t.app, &t.thread),
            ..Who::default()
        };
    }
    if let Some(job) = JOB_TITLE.captures(title) {
        return Who {
            kind: "job".into(),
            owner: Some(job[1].to_string()),
            unit: job[2].to_string(),
            ..Who::default()
        };
    }
    if !title.is_empty() {
        return Who {
            kind: "session".into(),
            title: title.into(),
            project,
            ..Who::default()
        };
    }
    Who {
        kind: "project".into(),
        project,
        ..Who::default()
    }
}

/// The agent row a session ranks under.
pub fn row_of(who: &Who, session_key: &str) -> String {
    match who.kind.as_str() {
        "thread" => format!("t:{}:{}", who.app, who.thread),
        "job" => format!(
            "j:{}/{}",
            who.owner.as_deref().unwrap_or_default(),
            who.unit
        ),
        "session" => format!("s:{session_key}"),
        "action" => format!("g:{}:{}", who.repo, who.workflow),
        _ => format!("p:{}", who.project),
    }
}

/// The agent a session ranks under: an Actions run's agent is its workflow, which links to the repo's runs.
pub fn agent_of(who: &Who) -> Who {
    let mut who = who.clone();
    if who.kind == "action" {
        who.run = None;
        who.href = Some(format!("https://github.com/{}/actions", who.repo));
    }
    who
}

pub fn label_of(who: &Who) -> (String, String) {
    match who.kind.as_str() {
        "thread" => (who.name.clone(), who.app.clone()),
        "job" => match &who.owner {
            Some(owner) => (who.unit.clone(), format!("job of {owner}")),
            None => (who.unit.clone(), "job".into()),
        },
        "session" => (who.title.clone(), basename(&who.project).into()),
        "action" => (
            format!("{} · {}", basename(&who.repo), who.workflow),
            "GitHub Actions".into(),
        ),
        _ => {
            let name = basename(&who.project);
            let label = if name.is_empty() { &who.project } else { name };
            (label.to_string(), "untitled sessions".into())
        }
    }
}

pub fn app_of(who: &Who) -> &str {
    match who.kind.as_str() {
        "thread" => &who.app,
        "job" => "job",
        "action" => "actions",
        _ => "other",
    }
}

/// Runs `hex <command> --json`, so the views never read hex.db's tables.
fn hex_json<T: for<'de> Deserialize<'de>>(hex: &Path, command: &str) -> Result<T, String> {
    let output = Command::new(hex.join(".hex/bin/hex"))
        .args([command, "--json"])
        .output()
        .map_err(|err| format!("hex {command}: {err}"))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "hex {command}: {}",
            if message.is_empty() {
                output.status.to_string()
            } else {
                message
            }
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|err| format!("hex {command}: {err}"))
}

/// Each past session's first 8 characters → (app, thread), from messages whose meta
/// names the session's log. Only hex's old log import wrote those, so hex's whole log
/// is read once, not at every change.
pub type Past = HashMap<String, (String, String)>;

pub fn read_past(hex: &Path) -> Result<Past, String> {
    #[derive(Deserialize)]
    struct Message {
        #[serde(default)]
        to: Option<String>,
        #[serde(default)]
        meta: Option<Meta>,
    }
    #[derive(Deserialize)]
    struct Meta {
        #[serde(default)]
        app: Option<String>,
        #[serde(default)]
        log: Option<String>,
    }
    let rows: Vec<Message> = hex_json(hex, "log")?;
    let mut past = Past::new();
    for row in rows {
        let Some(meta) = row.meta else { continue };
        let (app, log) = (meta.app.unwrap_or_default(), meta.log.unwrap_or_default());
        if let Some(id) = PAST_LOG_NAME.captures(&log).filter(|_| !app.is_empty()) {
            past.insert(id[1].to_string(), (app, row.to.unwrap_or_default()));
        }
    }
    Ok(past)
}

/// hex's threads by session, past session and name.
pub fn load_names(hex: &Path, past: &Past) -> Result<Names, String> {
    #[derive(Deserialize)]
    struct Row {
        #[serde(default)]
        app: Option<String>,
        #[serde(default)]
        thread: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        session: Option<String>,
    }
    let rows: Vec<Row> = hex_json(hex, "threads")?;
    let mut names = Names::default();
    let mut threads: HashMap<(String, String), Thread> = HashMap::new();
    let mut order = Vec::new();
    for row in rows {
        let t = Thread {
            app: row.app.unwrap_or_default(),
            thread: row.thread.unwrap_or_default(),
            name: row.name.unwrap_or_default(),
        };
        let key = (t.app.clone(), t.thread.clone());
        if !threads.contains_key(&key) {
            order.push(key.clone());
        }
        if let Some(session) = row.session.filter(|s| !s.is_empty()) {
            names.current.insert(session, t.clone());
        }
        threads.insert(key, t);
    }
    for (id, key) in past {
        if let Some(t) = threads.get(key) {
            names.logged.insert(id.clone(), t.clone());
        }
    }
    for key in order {
        let t = threads[&key].clone();
        names.named.entry(t.name.clone()).or_default().push(t);
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session<'a>(id: &'a str, title: Option<&'a str>) -> SessionRef<'a> {
        SessionRef {
            session_id: id,
            title,
            project: "/home/me/hex",
            repository: None,
        }
    }

    #[test]
    fn names_sessions_in_order() {
        let thread = |name: &str| Thread {
            app: "telegram".into(),
            thread: format!("-1001:{name}"),
            name: name.into(),
        };
        let mut names = Names::default();
        names.current.insert("live".into(), thread("Live"));
        names.logged.insert("pastpast".into(), thread("Past"));
        names
            .named
            .insert("Twice".into(), vec![thread("Twice"), thread("Twice")]);
        names.named.insert("Once".into(), vec![thread("Once")]);
        let row = |s: SessionRef| row_of(&who_of(&names, s), "k");
        assert_eq!(row(session("live", None)), "t:telegram:-1001:Live");
        assert_eq!(row(session("pastpast-1234", None)), "t:telegram:-1001:Past");
        assert_eq!(row(session("x", Some(" Once "))), "t:telegram:-1001:Once");
        assert_eq!(row(session("x", Some("Twice"))), "s:k");
        assert_eq!(row(session("x", Some("hex/kb-wave"))), "j:hex/kb-wave");
        assert_eq!(row(session("x", None)), "p:/home/me/hex");
        let action = who_of(
            &names,
            SessionRef {
                repository: Some("https://github.com/o/r"),
                ..session("github:42:1:x", Some("CI #42"))
            },
        );
        assert_eq!(
            (row_of(&action, "k"), action.href.as_deref()),
            (
                "g:o/r:CI".into(),
                Some("https://github.com/o/r/actions/runs/42")
            )
        );
        assert_eq!(
            thread_link("telegram", "-1001:7").as_deref(),
            Some("https://t.me/c/1/7")
        );
    }

    #[test]
    fn who_writes_only_its_kinds_fields() {
        let job = Who {
            kind: "job".into(),
            unit: "u".into(),
            ..Who::default()
        };
        assert_eq!(
            serde_json::to_string(&job).unwrap(),
            r#"{"kind":"job","owner":null,"unit":"u"}"#
        );
    }
}
