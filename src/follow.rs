//! Follows append-only ndjson files into the store, from where the last read stopped:
//! the Go proxy's per-device journals (`{"e":record}` and `{"d":id}` lines) while it
//! still keeps usage, and a proxy's receipts file (one record per line).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;

use crate::event::{Batch, Device, Event};
use crate::store::Store;

#[derive(Clone, Copy, PartialEq)]
pub enum Format {
    /// Go's store journal: an upsert or a withdrawal per line.
    Journal,
    /// One record per line.
    Receipts,
}

pub struct Followed {
    pub path: PathBuf,
    pub device: Device,
    pub format: Format,
}

#[derive(Deserialize)]
struct JournalLine {
    #[serde(default)]
    e: Option<Event>,
    #[serde(default)]
    d: Option<String>,
}

enum Change {
    Put(Event),
    Withdraw(String),
}

fn parse(line: &[u8], format: Format) -> Option<Change> {
    match format {
        Format::Receipts => serde_json::from_slice::<Event>(line)
            .ok()
            .filter(|e| !e.id.is_empty())
            .map(Change::Put),
        Format::Journal => match serde_json::from_slice::<JournalLine>(line).ok()? {
            JournalLine { e: Some(e), .. } => Some(Change::Put(e)),
            JournalLine { d: Some(id), .. } if !id.is_empty() => Some(Change::Withdraw(id)),
            _ => None,
        },
    }
}

/// Reads what was appended since the cursor and applies it. A file that was replaced
/// or shrank is read again from the start; applying a record already held changes nothing.
/// Returns how many lines were read.
pub fn catch_up(store: &Store, followed: &Followed, bulk: bool) -> Result<usize> {
    let key = followed.path.to_string_lossy().into_owned();
    let Ok(mut file) = File::open(&followed.path) else {
        return Ok(0);
    };
    let meta = file.metadata()?;
    let (inode, mut offset) = match store.cursor(&key)? {
        Some((inode, offset)) if inode == meta.ino() && offset <= meta.len() => (inode, offset),
        _ => (meta.ino(), 0),
    };
    if offset == meta.len() {
        return Ok(0);
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut read = 0;
    let mut chunk = vec![0u8; 8 << 20];
    let mut carry: Vec<u8> = Vec::new();
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        carry.extend_from_slice(&chunk[..n]);
        let Some(end) = carry.iter().rposition(|&b| b == b'\n') else {
            continue;
        };
        let changes: Vec<Change> = carry[..end]
            .split(|&b| b == b'\n')
            .filter_map(|line| parse(line, followed.format))
            .collect();
        read += changes.len();
        if bulk {
            store.load(
                &followed.device,
                changes.into_iter().filter_map(|c| match c {
                    Change::Put(e) => Some(e),
                    Change::Withdraw(_) => None,
                }),
            )?;
        } else {
            apply_in_order(store, &followed.device, changes)?;
        }
        offset += end as u64 + 1;
        carry.drain(..=end);
        store.set_cursor(&key, inode, offset)?;
    }
    Ok(read)
}

/// Applies upserts and withdrawals in file order, a run of one kind at a time.
fn apply_in_order(store: &Store, device: &Device, changes: Vec<Change>) -> Result<()> {
    let mut batch = Batch {
        device: device.clone(),
        ..Default::default()
    };
    for change in changes {
        match change {
            Change::Put(e) => {
                if !batch.deleted_ids.is_empty() {
                    store.apply(&std::mem::take(&mut batch), false)?;
                    batch.device = device.clone();
                }
                batch.events.push(e);
            }
            Change::Withdraw(id) => {
                if !batch.events.is_empty() {
                    store.apply(&std::mem::take(&mut batch), false)?;
                    batch.device = device.clone();
                }
                batch.deleted_ids.push(id);
            }
        }
        if batch.events.len() + batch.deleted_ids.len() >= 2000 {
            store.apply(&std::mem::take(&mut batch), false)?;
            batch.device = device.clone();
        }
    }
    if !batch.events.is_empty() || !batch.deleted_ids.is_empty() {
        store.apply(&batch, false)?;
    }
    Ok(())
}

/// The Go store's journals: one per device in `<dir>/events`, labeled by `<dir>/devices.json`.
pub fn go_journals(dir: &Path, local: &Device) -> Vec<Followed> {
    let devices: Vec<Device> = std::fs::read(dir.join("devices.json"))
        .ok()
        .and_then(|d| serde_json::from_slice(&d).ok())
        .unwrap_or_default();
    devices
        .into_iter()
        .map(|device| {
            let device = if device.id == local.id {
                local.clone()
            } else {
                device
            };
            Followed {
                path: dir
                    .join("events")
                    .join(format!("{}.ndjson", path_escape(&device.id))),
                device,
                format: Format::Journal,
            }
        })
        .collect()
}

/// Go's url.PathEscape, as its journal file names use it.
fn path_escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b':'
            | b'@'
            | b'&'
            | b'='
            | b'+'
            | b'$' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
