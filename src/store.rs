//! The usage warehouse: every device's records as they arrived (`records`), and one
//! resolved row per record identity (`events`) that the views query by time.
//!
//! Resolution happens on write. Of the copies of one (harness, id) across devices the
//! larger token total wins, then this server's own, then the lower device id. A proxy
//! receipt is the source of truth for a request: the transcript record of the same
//! upstream response or request is hidden, and the receipt takes its project, title,
//! thread and device from its session's latest transcript record.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use tokio::sync::watch;

use crate::event::{Applied, Batch, Device, Event};
use crate::pricing::Policy;

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
CREATE TABLE IF NOT EXISTS devices (
  id TEXT PRIMARY KEY,
  label TEXT NOT NULL,
  synced_at INTEGER
);
CREATE TABLE IF NOT EXISTS records (
  device TEXT NOT NULL,
  id TEXT NOT NULL,
  harness TEXT NOT NULL,
  total INTEGER NOT NULL,
  event TEXT NOT NULL,
  PRIMARY KEY (device, id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS records_identity ON records (harness, id);
CREATE TABLE IF NOT EXISTS events (
  harness TEXT NOT NULL,
  id TEXT NOT NULL,
  source TEXT NOT NULL,
  response_id TEXT NOT NULL,
  upstream_request_id TEXT NOT NULL,
  account_id TEXT NOT NULL,
  failed INTEGER NOT NULL,
  ts TEXT NOT NULL,
  millis INTEGER,
  model TEXT NOT NULL,
  raw_model TEXT NOT NULL,
  project TEXT NOT NULL,
  repository TEXT,
  session_id TEXT NOT NULL,
  input INTEGER NOT NULL,
  output INTEGER NOT NULL,
  cache_read INTEGER NOT NULL,
  cache_write INTEGER NOT NULL,
  unclassified INTEGER NOT NULL,
  reasoning INTEGER NOT NULL,
  cost_usd REAL NOT NULL,
  reported_cost_usd REAL,
  cost_known INTEGER NOT NULL,
  service_tier TEXT,
  cache_write_1h INTEGER,
  requests INTEGER,
  device_id TEXT NOT NULL,
  session_title TEXT NOT NULL,
  project_name TEXT NOT NULL,
  t3_thread_id TEXT NOT NULL,
  t3_thread_url TEXT NOT NULL,
  entrypoint TEXT NOT NULL,
  response_key TEXT,
  request_key TEXT,
  hidden INTEGER NOT NULL DEFAULT 0,
  gone INTEGER NOT NULL DEFAULT 0,
  rev INTEGER NOT NULL,
  PRIMARY KEY (harness, id)
);
CREATE INDEX IF NOT EXISTS events_rev ON events (rev);
CREATE INDEX IF NOT EXISTS events_session ON events (harness, session_id, ts);
CREATE INDEX IF NOT EXISTS events_response ON events (harness, response_key) WHERE response_key IS NOT NULL;
CREATE INDEX IF NOT EXISTS events_request ON events (harness, request_key, model) WHERE request_key IS NOT NULL;
CREATE TABLE IF NOT EXISTS cursors (
  path TEXT PRIMARY KEY,
  inode INTEGER NOT NULL,
  offset INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS events_proxy ON events (account_id, millis) WHERE source = 'proxy';
";

/// Whether a transcript row's upstream response or request has a proxy receipt.
const HAS_RECEIPT: &str = "(EXISTS (SELECT 1 FROM events p WHERE p.source = 'proxy' AND p.gone = 0 \
    AND p.harness = events.harness AND p.response_key = events.response_key) OR EXISTS (SELECT 1 FROM events p \
    WHERE p.source = 'proxy' AND p.gone = 0 AND p.harness = events.harness AND p.request_key = events.request_key \
    AND p.model = events.model))";

/// A row the views count.
const VISIBLE: &str = "hidden = 0 AND gone = 0";

const COLUMNS: &str = "source, response_id, upstream_request_id, account_id, failed, id, ts, harness, model, raw_model, \
    project, repository, session_id, input, output, cache_read, cache_write, unclassified, reasoning, cost_usd, \
    reported_cost_usd, cost_known, service_tier, cache_write_1h, requests, device_id, session_title, project_name, \
    t3_thread_id, t3_thread_url, entrypoint";

pub struct Store {
    write: Mutex<Connection>,
    read: Mutex<Connection>,
    local: Device,
    policy: Mutex<Arc<Policy>>,
    revision: watch::Sender<u64>,
}

impl Store {
    pub fn open(path: &Path, local: Device, policy: Arc<Policy>) -> Result<Self> {
        let write = Connection::open(path)?;
        write.execute_batch(SCHEMA)?;
        write.busy_timeout(std::time::Duration::from_secs(30))?;
        let read = Connection::open(path)?;
        read.busy_timeout(std::time::Duration::from_secs(30))?;
        write.execute(
            "INSERT INTO devices (id, label) VALUES (?1, ?2) ON CONFLICT (id) DO UPDATE SET label = excluded.label",
            params![local.id, local.label],
        )?;
        Ok(Self {
            write: Mutex::new(write),
            read: Mutex::new(read),
            local,
            policy: Mutex::new(policy),
            revision: watch::channel(0).0,
        })
    }

    pub fn local(&self) -> &Device {
        &self.local
    }

    /// Bumps on every change to records or prices.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.revision.subscribe()
    }

    fn changed(&self) {
        self.revision.send_modify(|r| *r += 1);
    }

    /// Upserts a device's records and withdraws its retracted ids. Optional session
    /// metadata a collector couldn't read this time keeps its previous value. With
    /// `fill` (history import) existing records are kept and nothing is withdrawn.
    pub fn apply(&self, batch: &Batch, fill: bool) -> Result<Applied> {
        let id = batch.device.id.as_str();
        if id.trim().is_empty() || id.len() > 200 {
            bail!("a valid device id is required");
        }
        let policy = self.policy.lock().unwrap().clone();
        let mut db = self.write.lock().unwrap();
        let tx = db.transaction()?;
        let mut result = Applied::default();
        let created = !exists(&tx, "SELECT 1 FROM devices WHERE id = ?1", id)?;
        let label = batch.device.label.trim();
        let synced = batch.device.synced_at.or((!fill).then(now_millis));
        tx.execute(
            "INSERT INTO devices (id, label, synced_at) VALUES (?1, ?2, ?3) ON CONFLICT (id) DO UPDATE SET \
             label = CASE WHEN ?2 <> '' AND ?1 <> ?4 THEN ?2 ELSE label END, synced_at = coalesce(?3, synced_at)",
            params![id, if id == self.local.id { "" } else { label }, synced, self.local.id],
        )?;
        let mut touched = BTreeSet::new();
        for incoming in &batch.events {
            if incoming.id.is_empty() {
                continue;
            }
            let previous = record(&tx, id, &incoming.id)?;
            if previous.is_some() && fill {
                continue;
            }
            let merged = merge(incoming, previous.as_ref(), id);
            if previous.as_ref() == Some(&merged) {
                continue;
            }
            if let Some(p) = &previous {
                touched.insert((p.harness.clone(), p.id.clone()));
                result.updated += 1;
            } else {
                result.accepted += 1;
            }
            tx.execute(
                "INSERT OR REPLACE INTO records (device, id, harness, total, event) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, merged.id, merged.harness, merged.token_total(), serde_json::to_string(&merged)?],
            )?;
            touched.insert((merged.harness.clone(), merged.id.clone()));
        }
        if !fill {
            for deleted in &batch.deleted_ids {
                if let Some(p) = record(&tx, id, deleted)? {
                    tx.execute("DELETE FROM records WHERE device = ?1 AND id = ?2", params![id, deleted])?;
                    touched.insert((p.harness, p.id));
                    result.deleted += 1;
                }
            }
        }
        Resolver::new(&tx, &self.local.id, &policy)?.resolve(&touched)?;
        tx.commit()?;
        if !touched.is_empty() || created {
            self.changed();
        }
        Ok(result)
    }

    /// Where a followed file was read up to: (inode, offset).
    pub fn cursor(&self, path: &str) -> Result<Option<(u64, u64)>> {
        let db = self.read.lock().unwrap();
        Ok(db
            .prepare_cached("SELECT inode, offset FROM cursors WHERE path = ?1")?
            .query_row([path], |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)))
            .optional()?)
    }

    pub fn set_cursor(&self, path: &str, inode: u64, offset: u64) -> Result<()> {
        self.write.lock().unwrap().execute(
            "INSERT OR REPLACE INTO cursors (path, inode, offset) VALUES (?1, ?2, ?3)",
            params![path, inode as i64, offset as i64],
        )?;
        Ok(())
    }

    /// Whether the store holds no records yet, so history can load in bulk.
    pub fn is_empty(&self) -> Result<bool> {
        let db = self.read.lock().unwrap();
        Ok(!db.prepare("SELECT 1 FROM records LIMIT 1")?.exists([])?)
    }

    /// Stores a device's records as they are, without resolving; `resolve_all` follows.
    pub fn load(&self, device: &Device, events: impl IntoIterator<Item = Event>) -> Result<usize> {
        let mut db = self.write.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO devices (id, label, synced_at) VALUES (?1, ?2, ?3) ON CONFLICT (id) DO UPDATE SET \
             label = CASE WHEN ?2 <> '' THEN ?2 ELSE label END, synced_at = coalesce(?3, synced_at)",
            params![device.id, device.label, device.synced_at],
        )?;
        let mut count = 0;
        {
            let mut insert = tx.prepare(
                "INSERT OR REPLACE INTO records (device, id, harness, total, event) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for e in events {
                insert.execute(params![device.id, e.id, e.harness, e.token_total(), serde_json::to_string(&e)?])?;
                count += 1;
            }
        }
        tx.commit()?;
        Ok(count)
    }

    /// Rebuilds every resolved row from the records, as after a bulk import.
    pub fn resolve_all(&self) -> Result<()> {
        let policy = self.policy.lock().unwrap().clone();
        let mut db = self.write.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute("DELETE FROM events", [])?;
        let resolver = Resolver::new(&tx, &self.local.id, &policy)?;
        let mut winners = tx.prepare(
            "SELECT event FROM (SELECT event, row_number() OVER (PARTITION BY harness, id \
             ORDER BY total DESC, device = ?1 DESC, device ASC) AS rank FROM records) WHERE rank = 1",
        )?;
        let mut rows = winners.query([&self.local.id])?;
        let mut proxies = Vec::new();
        while let Some(row) = rows.next()? {
            let e: Event = serde_json::from_str(row.get_ref(0)?.as_str()?)?;
            resolver.insert(&e)?;
            if e.is_proxy() {
                proxies.push((e.harness, e.id));
            }
        }
        drop(rows);
        drop(winners);
        tracing::info!("resolved {} records", proxies.len());
        tx.execute(&format!("UPDATE events SET hidden = {HAS_RECEIPT} WHERE source <> 'proxy'"), [])?;
        tracing::info!("hid transcript records the proxy answered for");
        for (harness, id) in &proxies {
            resolver.reconcile(harness, id)?;
        }
        tx.commit()?;
        self.changed();
        Ok(())
    }

    /// Applies a new pricing policy to every resolved row.
    pub fn set_policy(&self, policy: Arc<Policy>) -> Result<usize> {
        *self.policy.lock().unwrap() = policy.clone();
        let mut db = self.write.lock().unwrap();
        let tx = db.transaction()?;
        let rev = next_rev(&tx)?;
        let rows: Vec<Event> = tx
            .prepare(&format!("SELECT {COLUMNS} FROM events WHERE gone = 0"))?
            .query_map([], read_event)?
            .collect::<rusqlite::Result<_>>()?;
        let mut changed = 0;
        {
            let mut update = tx.prepare(
                "UPDATE events SET model = ?3, raw_model = ?4, cost_usd = ?5, cost_known = ?6, rev = ?7 \
                 WHERE harness = ?1 AND id = ?2",
            )?;
            for e in rows {
                if let Some(p) = policy.reprice(&e) {
                    update.execute(params![p.harness, p.id, p.model, p.raw_model, p.cost_usd, p.cost_known, rev])?;
                    changed += 1;
                }
            }
        }
        tx.commit()?;
        if changed > 0 {
            self.changed();
        }
        Ok(changed)
    }

    /// Rows changed since `rev` (all of them for -1) in timestamp and id order, each with
    /// its rowid and whether the views count it, and the store's latest revision.
    pub fn rows_after(&self, rev: i64) -> Result<(Vec<(i64, bool, crate::views::Row)>, i64)> {
        let db = self.read.lock().unwrap();
        let tx = db.unchecked_transaction()?;
        let latest: i64 = tx.query_row("SELECT coalesce(max(rev), 0) FROM events", [], |r| r.get(0))?;
        let mut strings: HashMap<String, Arc<str>> = HashMap::new();
        let mut intern = |r: &Row, i: usize| -> rusqlite::Result<Arc<str>> {
            let text = r.get_ref(i)?.as_str()?;
            Ok(match strings.get(text) {
                Some(s) => s.clone(),
                None => {
                    let s: Arc<str> = Arc::from(text);
                    strings.insert(text.to_string(), s.clone());
                    s
                }
            })
        };
        let mut statement = tx.prepare_cached(&format!(
            "SELECT rowid, {VISIBLE} AND millis IS NOT NULL, coalesce(millis, 0), source = 'proxy', failed, cost_known, \
             cost_usd, harness, model, project, repository, session_id, device_id, session_title, entrypoint, account_id, \
             input, output, cache_read, cache_write, unclassified, reasoning, requests \
             FROM events WHERE rev > ?1 AND rev <= ?2 ORDER BY ts, id"
        ))?;
        let mut rows = statement.query([rev, latest])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push((
                r.get(0)?,
                r.get(1)?,
                crate::views::Row {
                    millis: r.get::<_, i64>(2)? as f64,
                    proxy: r.get(3)?,
                    failed: r.get(4)?,
                    cost_known: r.get(5)?,
                    cost_usd: r.get(6)?,
                    harness: intern(r, 7)?,
                    model: intern(r, 8)?,
                    project: intern(r, 9)?,
                    repository: match r.get_ref(10)? {
                        rusqlite::types::ValueRef::Null => None,
                        _ => Some(intern(r, 10)?),
                    },
                    session_id: intern(r, 11)?,
                    device_id: intern(r, 12)?,
                    session_title: intern(r, 13)?,
                    entrypoint: intern(r, 14)?,
                    account_id: intern(r, 15)?,
                    input_tokens: r.get(16)?,
                    output_tokens: r.get(17)?,
                    cache_read_tokens: r.get(18)?,
                    cache_write_tokens: r.get(19)?,
                    unclassified_tokens: r.get(20)?,
                    reasoning_tokens: r.get(21)?,
                    requests: r.get(22)?,
                },
            ));
        }
        Ok((out, latest))
    }

    /// Every resolved record, sorted, as Go's `Store.Events()` returns them.
    pub fn events(&self) -> Result<Vec<Event>> {
        let db = self.read.lock().unwrap();
        let mut events: Vec<Event> = db
            .prepare(&format!("SELECT {COLUMNS} FROM events WHERE {VISIBLE}"))?
            .query_map([], read_event)?
            .collect::<rusqlite::Result<_>>()?;
        events.sort_unstable_by(|a, b| a.timestamp.cmp(&b.timestamp).then_with(|| a.id.cmp(&b.id)));
        Ok(events)
    }

    /// Proxy receipts charged to these credentials.
    pub fn receipts(&self, credentials: &[String]) -> Result<Vec<Event>> {
        let db = self.read.lock().unwrap();
        let mut statement =
            db.prepare_cached(&format!("SELECT {COLUMNS} FROM events WHERE source = 'proxy' AND gone = 0 AND account_id = ?1"))?;
        let mut out = Vec::new();
        for credential in credentials {
            out.extend(statement.query_map([credential], read_event)?.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(out)
    }

    /// When the earliest proxy receipt was made, unix ms.
    pub fn proxy_since(&self) -> Result<Option<i64>> {
        let db = self.read.lock().unwrap();
        Ok(db
            .prepare_cached("SELECT min(millis) FROM events WHERE source = 'proxy' AND gone = 0")?
            .query_row([], |r| r.get(0))?)
    }

    /// The server first, synced now, then other devices by label.
    pub fn devices(&self) -> Result<Vec<Device>> {
        let db = self.read.lock().unwrap();
        let mut others: Vec<Device> = db
            .prepare_cached("SELECT id, label, synced_at FROM devices WHERE id <> ?1 ORDER BY label, id")?
            .query_map([&self.local.id], |r| Ok(Device { id: r.get(0)?, label: r.get(1)?, synced_at: r.get(2)? }))?
            .collect::<rusqlite::Result<_>>()?;
        let mut out = vec![Device { synced_at: Some(now_millis()), ..self.local.clone() }];
        out.append(&mut others);
        Ok(out)
    }

    /// One device's stored records, as it sent them.
    pub fn device_records(&self, device: &str) -> Result<Vec<Event>> {
        let db = self.read.lock().unwrap();
        let rows = db
            .prepare("SELECT event FROM records WHERE device = ?1")?
            .query_map([device], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.iter().filter_map(|s| serde_json::from_str(s).ok()).collect())
    }
}

fn exists(tx: &Transaction, sql: &str, id: &str) -> Result<bool> {
    Ok(tx.prepare_cached(sql)?.query_row([id], |_| Ok(())).optional()?.is_some())
}

fn record(tx: &Transaction, device: &str, id: &str) -> Result<Option<Event>> {
    let text: Option<String> = tx
        .prepare_cached("SELECT event FROM records WHERE device = ?1 AND id = ?2")?
        .query_row(params![device, id], |r| r.get(0))
        .optional()?;
    Ok(match text {
        Some(t) => Some(serde_json::from_str(&t)?),
        None => None,
    })
}

pub fn now_millis() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn is_remote(repository: &Option<String>) -> bool {
    repository.as_deref().is_some_and(|r| !r.is_empty() && !r.starts_with("local:"))
}

/// token-tracker's normalization of an uploaded record. A record that names its T3
/// thread is authoritative for the thread's link (an empty link clears it).
pub fn merge(incoming: &Event, previous: Option<&Event>, device: &str) -> Event {
    let mut e = incoming.clone();
    e.device_id = device.to_string();
    if !e.cost_known {
        e.cost_usd = 0.0;
    }
    let Some(previous) = previous else { return e };
    if e.session_title.is_empty() {
        e.session_title = previous.session_title.clone();
    }
    if e.project_name.is_empty() {
        e.project_name = previous.project_name.clone();
    }
    if e.entrypoint.is_empty() {
        e.entrypoint = previous.entrypoint.clone();
    }
    if incoming.t3_thread_id.is_empty() && incoming.t3_thread_url.is_empty() {
        e.t3_thread_id = previous.t3_thread_id.clone();
        e.t3_thread_url = previous.t3_thread_url.clone();
        if !previous.t3_thread_id.is_empty()
            && is_remote(&previous.repository)
            && !is_remote(&incoming.repository)
            && incoming.project == previous.project
            && incoming.harness == previous.harness
            && incoming.session_id == previous.session_id
        {
            e.repository = previous.repository.clone();
        }
    }
    e
}

/// The upstream response a record answers, by field or by the id its transcript gave it.
fn response_identity(e: &Event) -> Option<String> {
    if !e.response_id.is_empty() {
        return Some(e.response_id.clone());
    }
    let id = e.id.as_str();
    match e.harness.as_str() {
        "claude" => {
            if let Some(rest) = id.strip_prefix("claude:advisor:") {
                return Some(format!("advisor:{rest}"));
            }
            id.strip_prefix("claude:message:").map(|rest| rest.split(':').next().unwrap_or("").to_string())
        }
        "codex" => id.strip_prefix("codex:response:").map(str::to_string),
        _ => None,
    }
    .filter(|s| !s.is_empty())
}

fn request_identity(e: &Event) -> Option<String> {
    if !e.upstream_request_id.is_empty() {
        return Some(e.upstream_request_id.clone());
    }
    if e.harness == "claude" {
        if let Some(rest) = e.id.strip_prefix("claude:message:") {
            return rest.split_once(':').map(|(_, r)| r.to_string()).filter(|s| !s.is_empty());
        }
    }
    None
}

/// The wire record, with what its transcript knows about the session.
fn with_transcript(mut wire: Event, transcript: &Event) -> Event {
    wire.project = transcript.project.clone();
    wire.repository = transcript.repository.clone();
    wire.device_id = transcript.device_id.clone();
    wire.session_id = transcript.session_id.clone();
    if !transcript.session_title.is_empty() {
        wire.session_title = transcript.session_title.clone();
    }
    wire.project_name = transcript.project_name.clone();
    wire.t3_thread_id = transcript.t3_thread_id.clone();
    wire.t3_thread_url = transcript.t3_thread_url.clone();
    if wire.entrypoint.is_empty() {
        wire.entrypoint = transcript.entrypoint.clone();
    }
    wire
}

type Identity = (String, String, Option<String>);

/// The revision this transaction's changes carry.
fn next_rev(tx: &Transaction) -> Result<i64> {
    Ok(tx.query_row("SELECT coalesce(max(rev), 0) + 1 FROM events", [], |r| r.get(0))?)
}

struct Resolver<'a> {
    tx: &'a Transaction<'a>,
    local: &'a str,
    policy: &'a Policy,
    rev: i64,
}

impl<'a> Resolver<'a> {
    fn new(tx: &'a Transaction<'a>, local: &'a str, policy: &'a Policy) -> Result<Self> {
        Ok(Self { tx, local, policy, rev: next_rev(tx)? })
    }

    fn resolve(&self, keys: &BTreeSet<(String, String)>) -> Result<()> {
        let mut identities: BTreeSet<Identity> = BTreeSet::new();
        let mut sessions: BTreeSet<(String, String)> = BTreeSet::new();
        let mut note = |e: &Event| {
            if let Some(r) = response_identity(e) {
                identities.insert((e.harness.clone(), r, None));
            }
            if let Some(r) = request_identity(e) {
                identities.insert((e.harness.clone(), r, Some(e.model.clone())));
            }
            sessions.insert((e.harness.clone(), e.session_id.clone()));
        };
        let mut proxies = BTreeSet::new();
        for (harness, id) in keys {
            if let Some(old) = self.resolved(harness, id)? {
                note(&old);
            }
            match self.winner(harness, id)? {
                Some(winner) => {
                    note(&winner);
                    if winner.is_proxy() {
                        proxies.insert((harness.clone(), id.clone()));
                    }
                    self.write(&winner)?;
                }
                None => {
                    self.tx
                        .prepare_cached("UPDATE events SET gone = 1, rev = ?3 WHERE harness = ?1 AND id = ?2 AND gone = 0")?
                        .execute(params![harness, id, self.rev])?;
                }
            }
        }
        for (harness, key, model) in &identities {
            let (column, model_clause) = if model.is_some() { ("request_key", " AND model = ?3") } else { ("response_key", "") };
            let args: Vec<&dyn rusqlite::ToSql> = match model {
                Some(m) => vec![harness, key, m],
                None => vec![harness, key],
            };
            self.tx
                .prepare_cached(&format!(
                    "UPDATE events SET hidden = 1 - hidden, rev = {} WHERE source <> 'proxy' AND harness = ?1 \
                     AND {column} = ?2{model_clause} AND hidden <> {HAS_RECEIPT}",
                    self.rev
                ))?
                .execute(args.as_slice())?;
            self.collect(
                &format!("SELECT harness, id FROM events WHERE source = 'proxy' AND gone = 0 AND harness = ?1 AND {column} = ?2{model_clause}"),
                args.as_slice(),
                &mut proxies,
            )?;
        }
        for (harness, session) in &sessions {
            self.collect(
                "SELECT harness, id FROM events WHERE source = 'proxy' AND gone = 0 AND harness = ?1 AND session_id = ?2",
                params![harness, session],
                &mut proxies,
            )?;
        }
        for (harness, id) in &proxies {
            self.reconcile(harness, id)?;
        }
        Ok(())
    }

    fn collect(&self, sql: &str, args: impl rusqlite::Params, into: &mut BTreeSet<(String, String)>) -> Result<()> {
        let mut statement = self.tx.prepare_cached(sql)?;
        let rows = statement.query_map(args, |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            into.insert(row?);
        }
        Ok(())
    }

    fn resolved(&self, harness: &str, id: &str) -> Result<Option<Event>> {
        Ok(self
            .tx
            .prepare_cached(&format!("SELECT {COLUMNS} FROM events WHERE harness = ?1 AND id = ?2 AND gone = 0"))?
            .query_row(params![harness, id], read_event)
            .optional()?)
    }

    /// The copy that counts: the larger token total, then this server's own, then the lower device id.
    fn winner(&self, harness: &str, id: &str) -> Result<Option<Event>> {
        let text: Option<String> = self
            .tx
            .prepare_cached(
                "SELECT event FROM records WHERE harness = ?1 AND id = ?2 \
                 ORDER BY total DESC, device = ?3 DESC, device ASC LIMIT 1",
            )?
            .query_row(params![harness, id, self.local], |r| r.get(0))
            .optional()?;
        Ok(match text {
            Some(t) => Some(serde_json::from_str(&t)?),
            None => None,
        })
    }

    fn write(&self, e: &Event) -> Result<()> {
        self.insert(e)?;
        if !e.is_proxy() {
            self.tx
                .prepare_cached(&format!(
                    "UPDATE events SET hidden = 1 - hidden WHERE harness = ?1 AND id = ?2 AND hidden <> {HAS_RECEIPT}"
                ))?
                .execute(params![e.harness, e.id])?;
        }
        Ok(())
    }

    fn insert(&self, e: &Event) -> Result<()> {
        let priced = self.policy.reprice(e).unwrap_or_else(|| e.clone());
        self.tx.prepare_cached(&format!(
            "INSERT INTO events ({COLUMNS}, millis, response_key, request_key, hidden, gone, rev) VALUES \
             (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, \
              ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, 0, 0, ?35) ON CONFLICT (harness, id) DO UPDATE SET {}, \
              millis = excluded.millis, response_key = excluded.response_key, request_key = excluded.request_key, \
              hidden = 0, gone = 0, rev = excluded.rev",
            COLUMNS.split(", ").map(|c| format!("{c} = excluded.{c}")).collect::<Vec<_>>().join(", ")
        ))?
        .execute(rusqlite::params_from_iter(event_values(&priced).into_iter().chain([
            millis_of(&priced.timestamp).map_or(rusqlite::types::Value::Null, rusqlite::types::Value::Integer),
            opt(response_identity(e)),
            opt(request_identity(e)),
            rusqlite::types::Value::Integer(self.rev),
        ])))?;
        Ok(())
    }

    /// Rewrites a proxy receipt with its transcript's metadata, from its raw record.
    fn reconcile(&self, harness: &str, id: &str) -> Result<()> {
        let Some(wire) = self.winner(harness, id)? else { return Ok(()) };
        let mut resolved = wire.clone();
        let transcript = |sql: &str, args: &[&dyn rusqlite::ToSql]| -> Result<Option<Event>> {
            Ok(self.tx.prepare_cached(sql)?.query_row(args, read_event).optional()?)
        };
        let matched = match response_identity(&wire) {
            Some(r) => transcript(
                &format!(
                    "SELECT {COLUMNS} FROM events WHERE source <> 'proxy' AND gone = 0 AND harness = ?1 AND response_key = ?2 \
                     ORDER BY ts DESC LIMIT 1"
                ),
                &[&harness, &r],
            )?,
            None => None,
        };
        let matched = match (matched, request_identity(&wire)) {
            (None, Some(r)) => transcript(
                &format!(
                    "SELECT {COLUMNS} FROM events WHERE source <> 'proxy' AND gone = 0 AND harness = ?1 AND request_key = ?2 AND model = ?3 \
                     ORDER BY ts DESC LIMIT 1"
                ),
                &[&harness, &r, &wire.model],
            )?,
            (m, _) => m,
        };
        if let Some(t) = &matched {
            resolved = with_transcript(resolved, t);
        }
        let latest = transcript(
            &format!(
                "SELECT {COLUMNS} FROM events WHERE source <> 'proxy' AND gone = 0 AND harness = ?1 AND session_id = ?2 \
                 ORDER BY ts DESC, device_id = ?3 ASC, device_id ASC LIMIT 1"
            ),
            &[&harness, &resolved.session_id, &self.local],
        )?;
        if let Some(t) = &latest {
            resolved = with_transcript(resolved, t);
        }
        let priced = self.policy.reprice(&resolved).unwrap_or(resolved);
        if self.resolved(harness, id)?.as_ref() == Some(&priced) {
            return Ok(());
        }
        let mut values = event_values(&priced);
        values.push(rusqlite::types::Value::Text(harness.to_string()));
        values.push(rusqlite::types::Value::Text(id.to_string()));
        values.push(rusqlite::types::Value::Integer(self.rev));
        let assignments: Vec<String> =
            COLUMNS.split(", ").enumerate().map(|(i, c)| format!("{c} = ?{}", i + 1)).collect();
        self.tx
            .prepare_cached(&format!("UPDATE events SET {}, rev = ?34 WHERE harness = ?32 AND id = ?33", assignments.join(", ")))?
            .execute(rusqlite::params_from_iter(values))?;
        Ok(())
    }
}

fn opt(value: Option<String>) -> rusqlite::types::Value {
    value.map_or(rusqlite::types::Value::Null, rusqlite::types::Value::Text)
}

fn event_values(e: &Event) -> Vec<rusqlite::types::Value> {
    use rusqlite::types::Value::{Integer, Null, Real, Text};
    let text = |s: &String| Text(s.clone());
    vec![
        text(&e.source),
        text(&e.response_id),
        text(&e.upstream_request_id),
        text(&e.account_id),
        Integer(e.failed as i64),
        text(&e.id),
        text(&e.timestamp),
        text(&e.harness),
        text(&e.model),
        text(&e.raw_model),
        text(&e.project),
        e.repository.clone().map_or(Null, Text),
        text(&e.session_id),
        Integer(e.input_tokens),
        Integer(e.output_tokens),
        Integer(e.cache_read_tokens),
        Integer(e.cache_write_tokens),
        Integer(e.unclassified_tokens),
        Integer(e.reasoning_tokens),
        Real(e.cost_usd),
        e.reported_cost_usd.map_or(Null, Real),
        Integer(e.cost_known as i64),
        e.service_tier.clone().map_or(Null, Text),
        e.cache_write_1h_tokens.map_or(Null, Integer),
        e.requests.map_or(Null, Integer),
        text(&e.device_id),
        text(&e.session_title),
        text(&e.project_name),
        text(&e.t3_thread_id),
        text(&e.t3_thread_url),
        text(&e.entrypoint),
    ]
}

fn read_event(r: &Row) -> rusqlite::Result<Event> {
    Ok(Event {
        source: r.get(0)?,
        response_id: r.get(1)?,
        upstream_request_id: r.get(2)?,
        account_id: r.get(3)?,
        failed: r.get(4)?,
        id: r.get(5)?,
        timestamp: r.get(6)?,
        harness: r.get(7)?,
        model: r.get(8)?,
        raw_model: r.get(9)?,
        project: r.get(10)?,
        repository: r.get(11)?,
        session_id: r.get(12)?,
        input_tokens: r.get(13)?,
        output_tokens: r.get(14)?,
        cache_read_tokens: r.get(15)?,
        cache_write_tokens: r.get(16)?,
        unclassified_tokens: r.get(17)?,
        reasoning_tokens: r.get(18)?,
        cost_usd: r.get(19)?,
        reported_cost_usd: r.get(20)?,
        cost_known: r.get(21)?,
        service_tier: r.get(22)?,
        cache_write_1h_tokens: r.get(23)?,
        requests: r.get(24)?,
        device_id: r.get(25)?,
        session_title: r.get(26)?,
        project_name: r.get(27)?,
        t3_thread_id: r.get(28)?,
        t3_thread_url: r.get(29)?,
        entrypoint: r.get(30)?,
    })
}

/// A record timestamp in unix ms, read as JavaScript's Date.parse reads ISO strings.
pub fn millis_of(timestamp: &str) -> Option<i64> {
    use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone};
    if let Ok(t) = DateTime::parse_from_rfc3339(timestamp) {
        return Some(t.timestamp_millis());
    }
    if let Ok(t) = NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H:%M:%S%.f") {
        return chrono::Local.from_local_datetime(&t).earliest().map(|t| t.timestamp_millis());
    }
    NaiveDate::parse_from_str(timestamp, "%Y-%m-%d").ok().map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis())
}
