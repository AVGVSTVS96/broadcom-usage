//! Collects this machine's agent transcripts into usage records.
//!
//! Each pass parses only the logs whose signature changed since the cache recorded them
//! (an appended Claude log only from where it left off), then recomputes the records
//! those logs can affect: every log sharing a record id or session with them.

mod cache;
mod dedup;
mod grok;
mod js;
mod json;
mod parse;
mod repository;
mod t3;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use rusqlite::Connection;

use crate::event::Event;
use crate::pricing::Policy;
use cache::{Link, Resume};
use parse::{Claude, Harness, Parsed, Usage};

/// Where a machine's agent logs live. Empty fields use the same defaults and environment
/// variables (CLAUDE_CONFIG_DIR, CODEX_HOME, PI_CODING_AGENT_DIR, GROK_HOME, MUSE_HOME)
/// token-tracker uses.
#[derive(Debug, Clone, Default)]
pub struct Sources {
    pub home: Option<PathBuf>,
    pub claude: Vec<PathBuf>,
    pub codex: Vec<PathBuf>,
    pub pi: Vec<PathBuf>,
    pub grok: Vec<PathBuf>,
    pub muse: Vec<PathBuf>,
    pub t3_data: Option<PathBuf>,
    pub t3_url: Option<String>,
    pub device_id: String,
    pub max_workers: usize,
}

/// One pass's result: the records that may have changed since the previous pass (every
/// record on a cold cache), priced and sorted by time and id, and the ids proved to be
/// replays that earlier passes may have reported.
#[derive(Debug, Clone, Default)]
pub struct Collection {
    pub events: Vec<Event>,
    pub retracted_ids: Vec<String>,
    pub warnings: Vec<String>,
}

struct Source {
    harness: Harness,
    path: PathBuf,
}

pub struct Collector {
    sources: Vec<Source>,
    t3_data: PathBuf,
    t3_url: String,
    device_id: String,
    workers: usize,
    db: Connection,
}

fn env_dirs(name: &str) -> Vec<PathBuf> {
    std::env::var(name)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn base(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

impl Collector {
    /// Resolves the source directories once; directories created later are still found
    /// because each pass walks them again.
    pub fn open(sources: Sources, cache_path: &Path) -> Result<Self> {
        let home = sources
            .home
            .clone()
            .or_else(std::env::home_dir)
            .unwrap_or_default();
        let expand = |root: &Path| -> PathBuf {
            let root = match root.to_str() {
                Some("~") => home.clone(),
                Some(s) if s.starts_with("~/") => home.join(&s[2..]),
                _ => root.to_path_buf(),
            };
            std::path::absolute(&root).unwrap_or(root)
        };
        let pick = |configured: &[PathBuf], env: &str, default: PathBuf| -> Vec<PathBuf> {
            if !configured.is_empty() {
                return configured.to_vec();
            }
            let dirs = env_dirs(env);
            if dirs.is_empty() { vec![default] } else { dirs }
        };
        let mut claude = sources.claude.clone();
        if claude.is_empty() {
            claude = env_dirs("CLAUDE_CONFIG_DIR");
            if claude.is_empty()
                && std::env::var("CLAUDE_CONFIG_DIR")
                    .unwrap_or_default()
                    .trim()
                    .is_empty()
            {
                claude = vec![home.join(".claude")];
                let xdg = std::env::var_os("XDG_CONFIG_HOME")
                    .map_or_else(|| home.join(".config"), PathBuf::from);
                if xdg.join("claude").join("projects").is_dir() {
                    claude.push(xdg.join("claude"));
                }
            }
        }
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map_or_else(|| home.join(".local").join("share"), PathBuf::from);
        let under = |root: &Path, leaf: &str| {
            if base(root) == leaf {
                root.to_path_buf()
            } else {
                root.join(leaf)
            }
        };
        let mut list: Vec<Source> = Vec::new();
        let mut add = |harness: Harness, root: PathBuf| {
            let path = expand(&root);
            if !list.iter().any(|s| s.harness == harness && s.path == path) {
                list.push(Source { harness, path });
            }
        };
        for root in &claude {
            add(Harness::Claude, under(root, "projects"));
        }
        for root in pick(&sources.codex, "CODEX_HOME", home.join(".codex")) {
            if matches!(base(&root).as_str(), "sessions" | "archived_sessions") {
                add(Harness::Codex, root);
            } else {
                add(Harness::Codex, root.join("sessions"));
                add(Harness::Codex, root.join("archived_sessions"));
            }
        }
        for root in pick(
            &sources.pi,
            "PI_CODING_AGENT_DIR",
            home.join(".pi").join("agent"),
        ) {
            add(Harness::Pi, under(&root, "sessions"));
        }
        for root in pick(&sources.grok, "GROK_HOME", home.join(".grok")) {
            add(Harness::Grok, under(&root, "sessions"));
        }
        for root in pick(&sources.muse, "MUSE_HOME", data_home.join("muse")) {
            add(Harness::Muse, under(&root, "sessions"));
        }
        let workers = match sources.max_workers {
            0 => (std::thread::available_parallelism().map_or(1, |n| n.get()) / 2).max(1),
            n => n,
        };
        Ok(Collector {
            sources: list,
            t3_data: expand(
                &sources
                    .t3_data
                    .unwrap_or_else(|| home.join(".t3").join("userdata")),
            ),
            t3_url: sources
                .t3_url
                .unwrap_or_else(|| "https://app.t3.codes".into()),
            device_id: sources.device_id,
            workers,
            db: cache::open(cache_path)?,
        })
    }

    /// The directories whose changes call for a new pass.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.sources.iter().map(|s| s.path.clone()).collect()
    }

    /// Runs one pass and hands its records to `deliver`. The cache keeps the pass only
    /// when `deliver` succeeds, so records it failed to store come back next pass.
    pub fn collect(
        &mut self,
        policy: &Policy,
        deliver: impl FnOnce(Collection) -> Result<()>,
    ) -> Result<()> {
        let (logs, mut warnings) = list_logs(&self.sources);
        let cached = cache::files(&self.db)?;
        let cold = cached.is_empty();
        let stale: Vec<usize> = (0..logs.len())
            .filter(|&i| {
                cached
                    .get(&logs[i].key)
                    .is_none_or(|c| c.signature != logs[i].signature)
            })
            .collect();
        let mut prior: HashMap<usize, (Parsed, Resume)> = HashMap::new();
        for &i in &stale {
            if let Some(c) = cached.get(&logs[i].key)
                && let Some(resume) = c.resume.clone().filter(|_| {
                    logs[i].harness == Harness::Claude
                        && same_file(&c.signature, &logs[i].signature)
                })
            {
                prior.insert(i, (cache::parsed(&self.db, c.id)?, resume));
            }
        }
        let mut fresh = parse_all(&logs, &stale, prior, self.workers);

        let tx = self.db.transaction()?;
        let mut seeds: HashSet<Link> = HashSet::new();
        let mut ids: HashMap<String, i64> = cached.iter().map(|(k, c)| (k.clone(), c.id)).collect();
        for (&i, entry) in &fresh {
            let (id, old) = cache::put(&tx, &logs[i].key, &logs[i].signature, entry)?;
            ids.insert(logs[i].key.clone(), id);
            if !cold {
                seeds.extend(old);
                seeds.extend(entry.links.iter().cloned());
            }
        }
        let present: HashSet<&str> = logs
            .iter()
            .enumerate()
            .filter(|(i, l)| {
                fresh.contains_key(i)
                    || cached
                        .get(&l.key)
                        .is_some_and(|c| c.signature == l.signature)
            })
            .map(|(_, l)| l.key.as_str())
            .collect();
        for (key, c) in &cached {
            if !present.contains(key.as_str()) {
                seeds.extend(cache::remove(&tx, c.id)?);
                ids.remove(key);
            }
        }

        let grok_roots: Vec<PathBuf> = self
            .sources
            .iter()
            .filter(|s| s.harness == Harness::Grok)
            .map(|s| s.path.clone())
            .collect();
        let (grok_files, grok_warnings) = grok::collect(&grok_roots);
        warnings.extend(grok_warnings);
        for file in &grok_files {
            seeds.extend(cache::links(file));
        }

        let t3_now = t3::read(&self.t3_data, &self.t3_url);
        let t3_before = cache::t3(&tx)?;
        for session in cache::put_t3(&tx, &t3_before, &t3_now)? {
            seeds.insert(cache::session_link(&session));
        }

        let picked: HashSet<i64> = if cold {
            ids.values().copied().collect()
        } else {
            cache::affected(&tx, &seeds)?
        };
        let mut files: Vec<Parsed> = Vec::with_capacity(picked.len() + grok_files.len());
        for (i, log) in logs.iter().enumerate() {
            let Some(id) = ids.get(&log.key).filter(|id| picked.contains(id)) else {
                continue;
            };
            files.push(match fresh.remove(&i) {
                Some(entry) => entry.parsed,
                None => cache::parsed(&tx, *id)?,
            });
        }
        files.extend(grok_files);

        let mut collection = build(files, &t3_now, policy, &self.device_id);
        warnings.append(&mut collection.warnings);
        collection.warnings = warnings;
        deliver(collection)?;
        tx.commit()?;
        Ok(())
    }
}

struct LogFile {
    harness: Harness,
    path: PathBuf,
    key: String,
    signature: String,
}

/// A signature changes on any write or replacement, including same-size rewrites that
/// restore the modification time.
fn signature(m: &std::fs::Metadata) -> String {
    let nanos = |s: i64, ns: i64| s * 1_000_000_000 + ns;
    format!(
        "{}:{}:{}:{}:{}",
        m.dev(),
        m.ino(),
        m.size(),
        nanos(m.mtime(), m.mtime_nsec()),
        nanos(m.ctime(), m.ctime_nsec())
    )
}

fn same_file(a: &str, b: &str) -> bool {
    a.splitn(3, ':').take(2).eq(b.splitn(3, ':').take(2))
}

fn jsonl_name(name: &str) -> bool {
    match name.rfind('.') {
        Some(0) | None => false,
        Some(i) => name[i..].eq_ignore_ascii_case(".jsonl"),
    }
}

/// Every log in token-tracker's order: each source root (which may be a symlink) walked
/// without following nested symlinks, paths sorted.
fn list_logs(sources: &[Source]) -> (Vec<LogFile>, Vec<String>) {
    let mut logs = Vec::new();
    let mut warnings = Vec::new();
    for source in sources.iter().filter(|s| s.harness != Harness::Grok) {
        let real = match std::fs::canonicalize(&source.path) {
            Ok(real) if real.is_dir() => real,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            _ => {
                warnings.push(format!(
                    "{}: Usage directory could not be read.",
                    source.harness
                ));
                continue;
            }
        };
        let mut paths = Vec::new();
        let mut problems = 0;
        let mut pending = vec![real];
        while let Some(dir) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                problems += 1;
                continue;
            };
            for entry in entries.flatten() {
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    pending.push(entry.path());
                } else if kind.is_file() && jsonl_name(&entry.file_name().to_string_lossy()) {
                    paths.push(entry.path());
                }
            }
        }
        if problems > 0 {
            warnings.push(format!(
                "{}: some directories could not be read.",
                source.harness
            ));
        }
        paths.sort_by(|a, b| {
            a.as_os_str()
                .as_encoded_bytes()
                .cmp(b.as_os_str().as_encoded_bytes())
        });
        for path in paths {
            let Ok(m) = std::fs::metadata(&path) else {
                continue;
            };
            let key = format!("{}:{}", source.harness, path.display());
            logs.push(LogFile {
                harness: source.harness,
                signature: signature(&m),
                key,
                path,
            });
        }
    }
    (logs, warnings)
}

const TAIL: u64 = 64;

/// Reads one log, returning None when it could not be read whole. A Claude log ending in
/// a complete line can be resumed from there when more is appended.
fn parse_file(log: &LogFile, prior: Option<(Parsed, Resume)>) -> Option<(Parsed, Option<Resume>)> {
    let file = File::open(&log.path).ok()?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let name = log.path.to_string_lossy();
    let parsed = match log.harness {
        Harness::Claude => {
            let (mut claude, start) = match prior.and_then(|(parsed, resume)| {
                resumable(reader.get_mut(), resume).map(|r| (parsed, r))
            }) {
                Some((parsed, resume)) => (Claude::resume(parsed), resume),
                None => (
                    Claude::default(),
                    Resume {
                        lines: 0,
                        end: 0,
                        tail: Vec::new(),
                    },
                ),
            };
            reader.seek(SeekFrom::Start(start.end)).ok()?;
            let read = claude.read(&mut reader, &name, start.lines).ok()?;
            let end = start.end + read.end;
            let resume = if read.tail {
                None
            } else {
                tail(reader.get_mut(), end).map(|tail| Resume {
                    lines: read.lines,
                    end,
                    tail,
                })
            };
            return Some((claude.finish(), resume));
        }
        Harness::Codex => parse::parse_codex(&mut reader, &name),
        Harness::Muse => parse::parse_muse(&mut reader, &name),
        _ => parse::parse_pi(&mut reader, &name),
    };
    parsed.ok().map(|p| (p, None))
}

fn tail(file: &mut File, end: u64) -> Option<Vec<u8>> {
    let start = end.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = vec![0; (end - start) as usize];
    file.read_exact(&mut tail).ok()?;
    Some(tail)
}

fn resumable(file: &mut File, resume: Resume) -> Option<Resume> {
    (tail(file, resume.end)? == resume.tail).then_some(resume)
}

fn parse_all(
    logs: &[LogFile],
    stale: &[usize],
    prior: HashMap<usize, (Parsed, Resume)>,
    workers: usize,
) -> HashMap<usize, cache::Entry> {
    let next = AtomicUsize::new(0);
    let prior = Mutex::new(prior);
    let results = Mutex::new(HashMap::with_capacity(stale.len()));
    std::thread::scope(|scope| {
        for _ in 0..workers.min(stale.len()) {
            scope.spawn(|| {
                while let Some(&i) = stale.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let before = prior.lock().unwrap().remove(&i);
                    let parsed = parse_file(&logs[i], before);
                    if let Some(entry) = parsed.and_then(|(p, r)| cache::Entry::new(p, r).ok()) {
                        results.lock().unwrap().insert(i, entry);
                    }
                }
            });
        }
    });
    results.into_inner().unwrap()
}

fn event(u: &Usage) -> Event {
    Event {
        id: u.id.clone(),
        timestamp: u.timestamp.clone(),
        harness: u.harness.to_string(),
        model: u.model.clone(),
        project: u.project.clone(),
        session_id: u.session_id.clone(),
        entrypoint: u.entrypoint.clone(),
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_read_tokens: u.cache_read_tokens,
        cache_write_tokens: u.cache_write_tokens,
        reasoning_tokens: u.reasoning_tokens,
        cost_usd: u.reported_cost_usd.unwrap_or_default(),
        cost_known: u.reported_cost_usd.is_some(),
        reported_cost_usd: u.reported_cost_usd,
        requests: u.requests,
        ..Default::default()
    }
}

/// Deduplicates, attributes and prices one pass's files into its collection.
fn build(
    mut files: Vec<Parsed>,
    t3: &BTreeMap<String, t3::Metadata>,
    policy: &Policy,
    device_id: &str,
) -> Collection {
    let mut titles: HashMap<String, String> = HashMap::new();
    for file in files.iter().filter(|f| !f.session_title.is_empty()) {
        for entry in &file.events {
            titles.insert(entry.usage.session_id.clone(), file.session_title.clone());
        }
    }
    dedup::filter_replays(&mut files);
    let (entries, retained) = dedup::deduplicate(&files);
    let mut retracted: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for file in &files {
        let legacy = file
            .events
            .iter()
            .map(|e| &e.legacy_event_id)
            .filter(|id| !id.is_empty());
        for id in file.retracted_ids.iter().chain(legacy) {
            if !retained.contains_key(id.as_str()) && seen.insert(id) {
                retracted.push(id.clone());
            }
        }
    }
    retracted.sort();

    let mut resolver = repository::Resolver::new();
    let remote = |r: &Option<String>| r.as_ref().is_some_and(|r| !r.starts_with("local:"));
    let metadata = |harness: Harness, session: &str| t3.get(&format!("{harness}:{session}"));
    let mut session_repositories: HashMap<(Harness, String), String> = HashMap::new();
    let mut attempted = HashSet::new();
    for entry in &entries {
        let e = &entry.usage;
        let key = (e.harness, e.session_id.clone());
        let Some(m) = metadata(e.harness, &e.session_id) else {
            continue;
        };
        if attempted.contains(&key) || remote(&resolver.get(&e.project)) {
            continue;
        }
        attempted.insert(key.clone());
        for cwd in &m.repository_paths {
            if let Some(attached) = resolver.get(cwd).filter(|r| !r.starts_with("local:")) {
                session_repositories.insert(key, attached);
                break;
            }
        }
    }

    let mut unknown = std::collections::BTreeSet::new();
    let mut events = Vec::with_capacity(entries.len());
    for entry in &entries {
        let mut e = event(&entry.usage);
        let m = metadata(entry.usage.harness, &e.session_id);
        if let Some(title) = titles.get(&e.session_id) {
            e.session_title = title.clone();
        } else if let Some(m) = m {
            e.session_title = m.session_title.clone();
        }
        if let Some(m) = m {
            e.project_name = m.project_name.clone();
            e.t3_thread_id = m.t3_thread_id.clone();
            e.t3_thread_url = m.t3_thread_url.clone();
        }
        let (cost, known) = policy.estimate_cost(&e, entry.cache_write_1h, &entry.tier);
        if !known {
            unknown.insert(e.model.clone());
        }
        (e.cost_usd, e.cost_known) = (cost, known);
        e.raw_model = e.model.clone();
        e.model = policy.display_model(&e.model);
        e.service_tier = Some(entry.tier.clone());
        e.cache_write_1h_tokens = Some(entry.cache_write_1h);
        let r = resolver.get(&e.project);
        e.repository = match session_repositories.get(&(entry.usage.harness, e.session_id.clone()))
        {
            Some(attached) if !remote(&r) => Some(attached.clone()),
            _ => r,
        };
        if !device_id.is_empty() {
            e.device_id = device_id.to_string();
        }
        events.push(e);
    }
    events.sort_by(|a, b| a.timestamp.cmp(&b.timestamp).then_with(|| a.id.cmp(&b.id)));
    let mut warnings = Vec::new();
    if !unknown.is_empty() {
        warnings.push(format!(
            "No reliable API price for: {}. These tokens are included, but their costs are unavailable.",
            unknown.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    Collection {
        events,
        retracted_ids: retracted,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        collector: Collector,
    }

    fn claude_line(
        session: &str,
        request: &str,
        message: &str,
        input: i64,
        sidechain: bool,
    ) -> String {
        format!(
            r#"{{"type":"assistant","isSidechain":{sidechain},"sessionId":"{session}","requestId":"{request}","timestamp":"2026-01-01T00:00:0{input}.000Z","cwd":"/w","message":{{"id":"{message}","model":"claude-opus-4-5","usage":{{"input_tokens":{input},"output_tokens":1}}}}}}"#
        )
    }

    impl Fixture {
        fn new(name: &str) -> Fixture {
            let root = std::env::temp_dir().join(format!("collect-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("claude/projects/p/s1/subagents")).unwrap();
            let collector = Fixture::open(&root);
            Fixture { root, collector }
        }

        fn open(root: &Path) -> Collector {
            let none = vec![root.join("none")];
            let sources = Sources {
                home: Some(root.to_path_buf()),
                claude: vec![root.join("claude")],
                codex: none.clone(),
                pi: none.clone(),
                grok: none.clone(),
                muse: none,
                t3_data: Some(root.join("none")),
                device_id: "test".into(),
                ..Default::default()
            };
            Collector::open(sources, &root.join("cache.sqlite")).unwrap()
        }

        fn append(&self, file: &str, lines: &[String]) {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.root.join(file))
                .unwrap();
            for line in lines {
                writeln!(f, "{line}").unwrap();
            }
        }

        fn pass(&mut self) -> Collection {
            let mut out = Collection::default();
            self.collector
                .collect(&Policy::bundled(), |c| Ok(out = c))
                .unwrap();
            out
        }

        fn ids(&mut self) -> Vec<String> {
            self.pass().events.into_iter().map(|e| e.id).collect()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn passes_return_only_what_changed() {
        let mut f = Fixture::new("passes");
        f.append(
            "claude/projects/p/s1.jsonl",
            &[claude_line("s1", "r1", "m1", 1, false)],
        );
        f.append(
            "claude/projects/p/s1/subagents/agent-a.jsonl",
            &[claude_line("s1", "r9", "m1", 2, true)],
        );
        f.append(
            "claude/projects/p/s2.jsonl",
            &[claude_line("s2", "r2", "m2", 3, false)],
        );
        let cold = f.pass();
        assert_eq!(cold.events.len(), 2);
        assert_eq!(cold.retracted_ids, ["claude:message:m1:r9"]);
        assert!(f.ids().is_empty());
        f.collector = Fixture::open(&f.root);
        assert!(f.ids().is_empty(), "a restart parses nothing new");

        f.append(
            "claude/projects/p/s1.jsonl",
            &[claude_line("s1", "r4", "m4", 4, false)],
        );
        assert_eq!(f.ids(), ["claude:message:m1:r1", "claude:message:m4:r4"]);
        let warm = f.pass().events;
        assert!(warm.is_empty());

        let fresh_root = f.root.with_extension("fresh");
        let _ = std::fs::remove_dir_all(&fresh_root);
        copy_dir(&f.root.join("claude"), &fresh_root.join("claude"));
        let mut fresh = Fixture {
            collector: Fixture::open(&fresh_root),
            root: fresh_root,
        };
        let mut all = fresh.pass().events;
        all.retain(|e| e.session_id == "s1");
        f.append(
            "claude/projects/p/s1.jsonl",
            &[claude_line("s1", "r5", "m5", 5, false)],
        );
        fresh.append(
            "claude/projects/p/s1.jsonl",
            &[claude_line("s1", "r5", "m5", 5, false)],
        );
        let appended = f.pass().events;
        let expected: Vec<Event> = fresh
            .pass()
            .events
            .into_iter()
            .filter(|e| e.session_id == "s1")
            .collect();
        assert_eq!(
            appended, expected,
            "an appended log reads the same as a fresh parse"
        );
        assert_eq!(all.len() + 1, appended.len());
    }

    #[test]
    fn a_failed_delivery_comes_back() {
        let mut f = Fixture::new("delivery");
        f.append(
            "claude/projects/p/s1.jsonl",
            &[claude_line("s1", "r1", "m1", 1, false)],
        );
        let failed = f
            .collector
            .collect(&Policy::bundled(), |_| anyhow::bail!("store is down"));
        assert!(failed.is_err());
        assert_eq!(f.ids(), ["claude:message:m1:r1"]);
        assert!(f.ids().is_empty());
    }

    #[test]
    fn a_rewritten_log_is_parsed_whole() {
        let mut f = Fixture::new("rewrite");
        f.append(
            "claude/projects/p/s1.jsonl",
            &[claude_line("s1", "r1", "m1", 1, false)],
        );
        f.pass();
        std::fs::write(
            f.root.join("claude/projects/p/s1.jsonl"),
            claude_line("s1", "r2", "m2", 2, false) + "\n",
        )
        .unwrap();
        assert_eq!(f.ids(), ["claude:message:m2:r2"]);
    }

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap().flatten() {
            if entry.file_type().unwrap().is_dir() {
                copy_dir(&entry.path(), &to.join(entry.file_name()));
            } else {
                std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
            }
        }
    }
}
