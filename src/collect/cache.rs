//! The collector's own SQLite file: each log's parsed contents and signature, the id and
//! session links between logs, and the T3 metadata the last pass saw. It is a cache: a
//! missing or outdated file is rebuilt by one full pass.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::parse::Parsed;
use super::t3::Metadata;

const VERSION: i64 = 1;

pub const ID: i64 = 0;
pub const SESSION: i64 = 1;

pub type Link = (i64, String);

/// Where an appended Claude log picks up: complete lines read, the bytes they span, and
/// the bytes just before that point, which must still be there.
#[derive(Debug, Clone, PartialEq)]
pub struct Resume {
    pub lines: usize,
    pub end: u64,
    pub tail: Vec<u8>,
}

pub struct Cached {
    pub id: i64,
    pub signature: String,
    pub resume: Option<Resume>,
}

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let db = Connection::open(path)?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.pragma_update(None, "synchronous", "NORMAL")?;
    let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version != VERSION {
        db.execute_batch(
            "DROP TABLE IF EXISTS files; DROP TABLE IF EXISTS links; DROP TABLE IF EXISTS t3;
             CREATE TABLE files (
                 id INTEGER PRIMARY KEY,
                 key TEXT NOT NULL UNIQUE,
                 signature TEXT NOT NULL,
                 parsed BLOB NOT NULL,
                 lines INTEGER,
                 end INTEGER,
                 tail BLOB
             );
             CREATE TABLE links (
                 kind INTEGER NOT NULL,
                 link TEXT NOT NULL,
                 file INTEGER NOT NULL,
                 PRIMARY KEY (kind, link, file)
             ) WITHOUT ROWID;
             CREATE INDEX links_file ON links (file, kind, link);
             CREATE TABLE t3 (key TEXT PRIMARY KEY, metadata TEXT NOT NULL);",
        )?;
        db.pragma_update(None, "user_version", VERSION)?;
    }
    Ok(db)
}

pub fn files(db: &Connection) -> Result<HashMap<String, Cached>> {
    let mut statement = db.prepare("SELECT id, key, signature, lines, end, tail FROM files")?;
    let rows = statement.query_map([], |r| {
        let resume = match (
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, Option<i64>>(4)?,
            r.get::<_, Option<Vec<u8>>>(5)?,
        ) {
            (Some(lines), Some(end), Some(tail)) => Some(Resume {
                lines: lines as usize,
                end: end as u64,
                tail,
            }),
            _ => None,
        };
        Ok((
            r.get::<_, String>(1)?,
            Cached {
                id: r.get(0)?,
                signature: r.get(2)?,
                resume,
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn parsed(db: &Connection, id: i64) -> Result<Parsed> {
    let blob: Vec<u8> =
        db.query_row("SELECT parsed FROM files WHERE id = ?", [id], |r| r.get(0))?;
    Ok(serde_json::from_slice(&blob)?)
}

pub fn links_of(tx: &Transaction, file: i64) -> Result<Vec<Link>> {
    let mut statement = tx.prepare_cached("SELECT kind, link FROM links WHERE file = ?")?;
    let rows = statement.query_map([file], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn files_linked(tx: &Transaction, (kind, link): &Link) -> Result<Vec<i64>> {
    let mut statement = tx.prepare_cached("SELECT file FROM links WHERE kind = ? AND link = ?")?;
    let rows = statement.query_map(params![kind, link], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn links(parsed: &Parsed) -> HashSet<Link> {
    let mut out = HashSet::new();
    let mut ids = Vec::new();
    let mut sessions = Vec::new();
    parsed.links(
        |id| ids.push(id.to_string()),
        |s| sessions.push(s.to_string()),
    );
    out.extend(ids.into_iter().map(|l| (ID, l)));
    out.extend(sessions.into_iter().map(|l| (SESSION, l)));
    out
}

/// Replaces a file's row and links, returning its id and the links it had before.
pub fn put(
    tx: &Transaction,
    key: &str,
    signature: &str,
    parsed: &Parsed,
    resume: Option<&Resume>,
    links: &HashSet<Link>,
) -> Result<(i64, Vec<Link>)> {
    let existing: Option<i64> = tx
        .query_row("SELECT id FROM files WHERE key = ?", [key], |r| r.get(0))
        .optional()?;
    let old = match existing {
        Some(id) => {
            let old = links_of(tx, id)?;
            tx.execute("DELETE FROM links WHERE file = ?", [id])?;
            old
        }
        None => Vec::new(),
    };
    tx.execute(
        "INSERT INTO files (key, signature, parsed, lines, end, tail) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (key) DO UPDATE SET signature = ?2, parsed = ?3, lines = ?4, end = ?5, tail = ?6",
        params![
            key,
            signature,
            serde_json::to_vec(parsed)?,
            resume.map(|r| r.lines as i64),
            resume.map(|r| r.end as i64),
            resume.map(|r| &r.tail),
        ],
    )?;
    let id = tx.query_row("SELECT id FROM files WHERE key = ?", [key], |r| r.get(0))?;
    let mut insert =
        tx.prepare_cached("INSERT OR IGNORE INTO links (kind, link, file) VALUES (?, ?, ?)")?;
    for (kind, link) in links {
        insert.execute(params![kind, link, id])?;
    }
    Ok((id, old))
}

/// Forgets a file, returning the links it had.
pub fn remove(tx: &Transaction, id: i64) -> Result<Vec<Link>> {
    let old = links_of(tx, id)?;
    tx.execute("DELETE FROM links WHERE file = ?", [id])?;
    tx.execute("DELETE FROM files WHERE id = ?", [id])?;
    Ok(old)
}

pub fn t3(db: &Connection) -> Result<BTreeMap<String, Metadata>> {
    let mut statement = db.prepare("SELECT key, metadata FROM t3")?;
    let rows = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (key, value) = row?;
        out.insert(key, serde_json::from_str(&value)?);
    }
    Ok(out)
}

/// Stores the T3 metadata a pass saw, returning the sessions whose metadata changed.
pub fn put_t3(
    tx: &Transaction,
    old: &BTreeMap<String, Metadata>,
    new: &BTreeMap<String, Metadata>,
) -> Result<Vec<String>> {
    let mut changed = Vec::new();
    let session = |key: &str| key.split_once(':').map_or("", |(_, s)| s).to_string();
    for (key, metadata) in new {
        if old.get(key) != Some(metadata) {
            tx.execute(
                "INSERT INTO t3 (key, metadata) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET metadata = ?2",
                params![key, serde_json::to_string(metadata)?],
            )?;
            changed.push(session(key));
        }
    }
    for key in old.keys().filter(|k| !new.contains_key(*k)) {
        tx.execute("DELETE FROM t3 WHERE key = ?", [key])?;
        changed.push(session(key));
    }
    Ok(changed)
}
