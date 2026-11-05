//! The collector's own SQLite file: each log's parsed contents and signature, the id and
//! session links between logs, and the T3 metadata the last pass saw. It is a cache: a
//! missing or outdated file is rebuilt by one full pass.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::Result;
use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::parse::Parsed;
use super::t3::Metadata;

const VERSION: i64 = 4;

/// A record id or session that ties files together, as a 64-bit FNV-1a hash. A collision
/// only widens a pass to more files, which resends unchanged records.
pub type Link = i64;

fn link(kind: u8, value: &str) -> Link {
    let hash = [kind]
        .iter()
        .chain(value.as_bytes())
        .fold(0xcbf29ce484222325u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
        });
    hash as i64
}

pub fn session_link(session: &str) -> Link {
    link(1, session)
}

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
    db.pragma_update(None, "cache_size", -32768)?;
    let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version != VERSION {
        db.execute_batch(
            "DROP TABLE IF EXISTS files; DROP TABLE IF EXISTS parsed; DROP TABLE IF EXISTS links; DROP TABLE IF EXISTS t3;
             CREATE TABLE files (
                 id INTEGER PRIMARY KEY,
                 key TEXT NOT NULL UNIQUE,
                 signature TEXT NOT NULL,
                 lines INTEGER,
                 end INTEGER,
                 tail BLOB
             );
             CREATE TABLE parsed (file INTEGER PRIMARY KEY, blob BLOB NOT NULL);
             CREATE TABLE links (
                 link INTEGER NOT NULL,
                 file INTEGER NOT NULL,
                 PRIMARY KEY (link, file)
             ) WITHOUT ROWID;
             CREATE INDEX links_file ON links (file, link);
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
        db.query_row("SELECT blob FROM parsed WHERE file = ?", [id], |r| r.get(0))?;
    let mut bytes = Vec::new();
    DeflateDecoder::new(&blob[..]).read_to_end(&mut bytes)?;
    Ok(postcard::from_bytes(&bytes)?)
}

pub fn links_of(tx: &Transaction, file: i64) -> Result<Vec<Link>> {
    let mut statement = tx.prepare_cached("SELECT link FROM links WHERE file = ?")?;
    let rows = statement.query_map([file], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The cached files whose records can change with the given links: every file sharing
/// a link with them, and with those files, until no new file is reached.
pub fn affected(tx: &Transaction, seeds: &HashSet<Link>) -> Result<HashSet<i64>> {
    tx.execute("CREATE TEMP TABLE IF NOT EXISTS seeds (link INTEGER PRIMARY KEY)", [])?;
    let mut insert = tx.prepare_cached("INSERT OR IGNORE INTO temp.seeds (link) VALUES (?)")?;
    for link in seeds {
        insert.execute([link])?;
    }
    let mut statement = tx.prepare_cached(
        "WITH RECURSIVE reach (file, link) AS (
             SELECT NULL, link FROM temp.seeds
             UNION
             SELECT links.file, NULL FROM reach JOIN links ON links.link = reach.link
             UNION
             SELECT NULL, links.link FROM reach JOIN links ON links.file = reach.file
         )
         SELECT file FROM reach WHERE file IS NOT NULL",
    )?;
    let files = statement.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    tx.execute("DELETE FROM temp.seeds", [])?;
    Ok(files)
}

pub fn links(parsed: &Parsed) -> HashSet<Link> {
    let mut ids = HashSet::new();
    let mut sessions = HashSet::new();
    parsed.links(
        |id| _ = ids.insert(link(0, id)),
        |s| _ = sessions.insert(session_link(s)),
    );
    ids.extend(sessions);
    ids
}

/// A freshly parsed file, encoded for the cache.
pub struct Entry {
    pub parsed: Parsed,
    pub resume: Option<Resume>,
    pub links: HashSet<Link>,
    blob: Vec<u8>,
}

impl Entry {
    pub fn new(parsed: Parsed, resume: Option<Resume>) -> Result<Entry> {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&postcard::to_stdvec(&parsed)?)?;
        let blob = encoder.finish()?;
        Ok(Entry {
            links: links(&parsed),
            parsed,
            resume,
            blob,
        })
    }
}

/// Replaces a file's row and links, returning its id and the links it had before.
pub fn put(
    tx: &Transaction,
    key: &str,
    signature: &str,
    entry: &Entry,
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
        "INSERT INTO files (key, signature, lines, end, tail) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (key) DO UPDATE SET signature = ?2, lines = ?3, end = ?4, tail = ?5",
        params![
            key,
            signature,
            entry.resume.as_ref().map(|r| r.lines as i64),
            entry.resume.as_ref().map(|r| r.end as i64),
            entry.resume.as_ref().map(|r| &r.tail),
        ],
    )?;
    let id = tx.query_row("SELECT id FROM files WHERE key = ?", [key], |r| r.get(0))?;
    tx.execute(
        "INSERT OR REPLACE INTO parsed (file, blob) VALUES (?, ?)",
        params![id, entry.blob],
    )?;
    let mut insert = tx.prepare_cached("INSERT OR IGNORE INTO links (link, file) VALUES (?, ?)")?;
    for link in &entry.links {
        insert.execute(params![link, id])?;
    }
    Ok((id, old))
}

/// Forgets a file, returning the links it had.
pub fn remove(tx: &Transaction, id: i64) -> Result<Vec<Link>> {
    let old = links_of(tx, id)?;
    tx.execute("DELETE FROM links WHERE file = ?", [id])?;
    tx.execute("DELETE FROM parsed WHERE file = ?", [id])?;
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
