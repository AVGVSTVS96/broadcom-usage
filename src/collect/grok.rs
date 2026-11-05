//! Grok's per-session accounting: a usage.json ledger, or the updates.jsonl stream when
//! the ledger is missing or unreadable.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use super::js::{encode_uri_component, iso_millis, js_trim, parse_date, valid_utf8};
use super::json::{Val, read_lines, valid};
use super::parse::{Candidate, Harness, Parsed, Usage};

struct Session {
    session_id: String,
    project: String,
    model: String,
    parent_id: String,
    forked_at: String,
    kind: String,
    ledger: Option<PathBuf>,
    updates: Option<PathBuf>,
    parsed: Option<Parsed>,
}

fn turn_id(session: &str, turn: i64, model: &str) -> String {
    format!(
        "grok:turn:{}:{turn}:{}",
        encode_uri_component(session),
        encode_uri_component(model)
    )
}

fn turn_number(e: &Usage) -> i64 {
    e.id.split(':')
        .nth(3)
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

fn count(v: Val) -> Option<i64> {
    v.num()
        .filter(|n| *n >= 0.0 && *n <= 9007199254740991.0 && *n == n.trunc())
        .map(|n| n as i64)
}

fn incomplete(v: Val) -> bool {
    v.exists() && v != Val::False
}

fn stamp(v: Val) -> String {
    match v {
        Val::Num(n) if n > 1e11 => iso_millis(n),
        Val::Num(n) => iso_millis(n * 1000.0),
        Val::Str(_) => iso_millis(parse_date(&v.str().unwrap_or_default())),
        _ => String::new(),
    }
}

fn append_usage(
    parsed: &mut Parsed,
    usage: Val,
    s: &Session,
    turn: i64,
    at: &str,
    fallback_model: &str,
) {
    let mut rows: Vec<(String, Val)> = usage
        .get("modelUsage")
        .members()
        .map(|(k, v)| (k.into_owned(), Val::parse(v)))
        .collect();
    if rows.is_empty() {
        let mut model = usage.get("primaryModelId").text();
        if model.is_empty() {
            model = fallback_model.to_string();
        }
        if model.is_empty() {
            model = "unknown".into();
        }
        rows.push((model, usage));
    }
    for (model, value) in rows {
        let f = value.fields([
            "inputTokens",
            "outputTokens",
            "cachedReadTokens",
            "cacheCreationTokens",
            "reasoningTokens",
            "modelCalls",
            "costUsdTicks",
            "costIsPartial",
            "usageIsIncomplete",
        ]);
        let (Some(input), Some(output)) = (count(f[0]), count(f[1])) else {
            parsed.malformed += 1;
            continue;
        };
        if f[2..6].iter().any(|v| v.exists() && count(*v).is_none()) {
            parsed.malformed += 1;
            continue;
        }
        let cache_read = input.min(count(f[2]).unwrap_or(0));
        let cache_write = (input - cache_read).min(count(f[3]).unwrap_or(0));
        if input == 0 && output == 0 {
            continue;
        }
        let model = if model.is_empty() {
            "unknown".to_string()
        } else {
            model
        };
        let mut e = Usage {
            id: turn_id(&s.session_id, turn, &model),
            timestamp: at.into(),
            harness: Harness::Grok,
            session_id: s.session_id.clone(),
            model,
            project: s.project.clone(),
            input_tokens: input - cache_read - cache_write,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            reasoning_tokens: output.min(count(f[4]).unwrap_or(0)),
            ..Default::default()
        };
        if !incomplete(f[7])
            && !incomplete(f[8])
            && !incomplete(usage.get("usageIsIncomplete"))
            && let Some(ticks) = count(f[6]).filter(|t| *t > 0)
        {
            let cost = f[6].num().unwrap_or(ticks as f64) / 1e10;
            e.reported_cost_usd = Some(cost);
        }
        e.requests = count(f[5]);
        parsed.events.push(Candidate {
            usage: e,
            ..Default::default()
        });
    }
}

fn parse_ledger(data: &str, s: &Session) -> Parsed {
    let mut parsed = Parsed {
        session_id: s.session_id.clone(),
        ..Default::default()
    };
    let ledger = Val::parse(data);
    let turns = ledger.get("turns");
    let id = ledger.get("sessionId").text();
    if !ledger.is_object() || !turns.is_array() || (!id.is_empty() && id != s.session_id) {
        parsed.malformed += 1;
        return parsed;
    }
    for row in turns.elements() {
        let turn = count(row.get("turnNumber")).unwrap_or(0);
        let at = stamp(row.get("endedAt"));
        if turn == 0 || at.is_empty() {
            parsed.malformed += 1;
            continue;
        }
        append_usage(&mut parsed, row, s, turn, &at, &s.model);
    }
    parsed
}

fn parse_updates(data: &[u8], s: &Session) -> Parsed {
    let mut parsed = Parsed {
        session_id: s.session_id.clone(),
        ..Default::default()
    };
    let mut turn = 0;
    let mut model = s.model.clone();
    let mut completed: HashMap<String, i64> = HashMap::new();
    let _ = read_lines(&mut &data[..], 0, |raw, _, complete| {
        let Some(raw) = raw else {
            parsed.malformed += 1;
            return;
        };
        let text = valid_utf8(raw);
        if text.trim_matches(super::js::js_space).is_empty() {
            return;
        }
        if !valid(text.as_bytes()) {
            if complete {
                parsed.malformed += 1;
            }
            return;
        }
        let record = Val::parse(&text);
        let params = record.get("params");
        let mut update = params.get("update");
        if !record.is_object() || !params.is_object() || !update.is_object() {
            update = Val::Missing;
        }
        let meta_model = update.get("_meta").get("modelId").text();
        if !meta_model.is_empty() {
            model = meta_model;
        }
        if !update.get("sessionUpdate").is("turn_completed") {
            return;
        }
        let native_id = params.get("_meta").get("eventId").text();
        let existing = completed
            .get(&native_id)
            .copied()
            .filter(|_| !native_id.is_empty());
        let ordinal = existing.unwrap_or_else(|| {
            turn += 1;
            turn
        });
        if !native_id.is_empty() {
            completed.insert(native_id, ordinal);
        }
        let mut usage = update.get("usage");
        if !usage.exists() {
            if existing.is_some() {
                parsed.malformed += 1;
            }
            return;
        }
        let mut at = stamp(params.get("_meta").get("agentTimestampMs"));
        if at.is_empty() {
            at = stamp(record.get("timestamp"));
        }
        if at.is_empty() {
            parsed.malformed += 1;
            return;
        }
        if !usage.is_object() {
            usage = Val::Obj("{}");
        }
        let mut replacement = Parsed::default();
        append_usage(&mut replacement, usage, s, ordinal, &at, &model);
        parsed.malformed += replacement.malformed;
        if existing.is_none() || replacement.malformed == 0 {
            if existing.is_some() {
                parsed
                    .events
                    .retain(|entry| turn_number(&entry.usage) != ordinal);
            }
            parsed.events.extend(replacement.events);
        }
    });
    parsed
}

fn regular_file(file: &Path) -> bool {
    fs::symlink_metadata(file).is_ok_and(|m| m.is_file())
}

fn subdirectories(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

fn read_bounded(file: &Path, limit: u64) -> Option<Vec<u8>> {
    if fs::metadata(file).ok()?.len() > limit {
        return None;
    }
    fs::read(file).ok()
}

fn path_unescape(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

fn text_of(data: &[u8], path: &str) -> String {
    let text = valid_utf8(data);
    path.split('.')
        .fold(Val::parse(&text), |v, key| v.get(key))
        .text()
}

/// Grok's parsed sessions. Parent turns already include their subagents, so subagent
/// sessions only prove ownership; forks keep inherited turns on the parent.
pub fn collect(roots: &[PathBuf]) -> (Vec<Parsed>, Vec<String>) {
    let mut sessions: Vec<Session> = Vec::new();
    let mut parents: HashMap<String, String> = HashMap::new();
    let mut warnings = Vec::new();
    for root in roots {
        let Ok(real) = fs::canonicalize(root) else {
            continue;
        };
        for group in subdirectories(&real) {
            let base = group
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut group_project = path_unescape(&base)
                .filter(|d| d.starts_with('/'))
                .unwrap_or_default();
            if group_project.is_empty()
                && regular_file(&group.join(".cwd"))
                && let Some(data) = read_bounded(&group.join(".cwd"), 16 * 1024)
            {
                group_project = js_trim(&valid_utf8(&data)).to_string();
            }
            for directory in subdirectories(&group) {
                let summary = Some(directory.join("summary.json"))
                    .filter(|f| regular_file(f))
                    .and_then(|f| read_bounded(&f, 1 << 20))
                    .filter(|data| valid(data))
                    .unwrap_or_default();
                let field = |path: &str| text_of(&summary, path);
                let mut s = Session {
                    session_id: field("info.id"),
                    project: field("info.cwd"),
                    model: field("current_model_id"),
                    parent_id: field("parent_session_id"),
                    kind: field("session_kind"),
                    forked_at: String::new(),
                    ledger: Some(directory.join("usage.json")).filter(|f| regular_file(f)),
                    updates: Some(directory.join("updates.jsonl")).filter(|f| regular_file(f)),
                    parsed: None,
                };
                if s.session_id.is_empty() {
                    s.session_id = directory
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                }
                if s.project.is_empty() {
                    s.project = group_project.clone();
                }
                if s.project.is_empty() {
                    s.project = "Unknown project".into();
                }
                let summary_text = valid_utf8(&summary);
                let summary_val = Val::parse(&summary_text);
                s.forked_at = stamp(summary_val.get("forked_at"));
                if s.forked_at.is_empty() {
                    s.forked_at = stamp(summary_val.get("created_at"));
                }
                let session_id = s.session_id.clone();
                sessions.push(s);
                let subagents = directory.join("subagents");
                if !fs::symlink_metadata(&subagents).is_ok_and(|m| m.is_dir()) {
                    continue;
                }
                for child in subdirectories(&subagents) {
                    let file = child.join("meta.json");
                    if !regular_file(&file) {
                        continue;
                    }
                    let Some(data) = read_bounded(&file, 1 << 20).filter(|d| valid(d)) else {
                        continue;
                    };
                    let child_id = text_of(&data, "child_session_id");
                    let mut parent_id = text_of(&data, "parent_session_id");
                    if parent_id.is_empty() {
                        parent_id = session_id.clone();
                    }
                    if !child_id.is_empty() && child_id != parent_id {
                        parents.insert(child_id, parent_id);
                    }
                }
            }
        }
    }
    let subagent =
        |s: &Session| s.kind.starts_with("subagent") || parents.contains_key(&s.session_id);
    for s in &mut sessions {
        if (s.ledger.is_none() && s.updates.is_none()) || subagent(s) {
            continue;
        }
        let mut ledger_malformed = 0;
        if let Some(file) = &s.ledger {
            match read_bounded(file, 64 << 20).filter(|d| valid(d)) {
                Some(data) => {
                    let parsed = parse_ledger(&valid_utf8(&data), s);
                    ledger_malformed = parsed.malformed;
                    s.parsed = Some(parsed);
                }
                None => ledger_malformed = 1,
            }
        }
        if (s.parsed.is_none() || ledger_malformed > 0)
            && let Some(data) = s.updates.as_ref().and_then(|f| read_bounded(f, u64::MAX))
        {
            s.parsed = Some(parse_updates(&data, s));
        }
    }

    let by_id: HashMap<&str, &Session> = sessions
        .iter()
        .map(|s| (s.session_id.as_str(), s))
        .collect();
    let mut native_turns: HashMap<&str, HashMap<String, HashSet<i64>>> = HashMap::new();
    for s in &sessions {
        let turns = native_turns.entry(&s.session_id).or_default();
        for entry in s.parsed.iter().flat_map(|p| &p.events) {
            let key = format!("{}:{}", turn_number(&entry.usage), entry.usage.model);
            turns
                .entry(key)
                .or_default()
                .insert(parse_date(&entry.usage.timestamp) as i64);
        }
    }
    let orphans = sessions
        .iter()
        .filter(|s| subagent(s))
        .filter(|s| {
            let parent_of = |s: &Session| {
                parents
                    .get(&s.session_id)
                    .cloned()
                    .unwrap_or_else(|| s.parent_id.clone())
            };
            let mut parent = by_id.get(parent_of(s).as_str()).copied();
            let mut visited = HashSet::from([s.session_id.as_str()]);
            while let Some(p) = parent.filter(|p| subagent(p) && visited.insert(&p.session_id)) {
                parent = by_id.get(parent_of(p).as_str()).copied();
            }
            parent.is_none_or(|p| (p.ledger.is_none() && p.updates.is_none()) || p.parsed.is_none())
        })
        .count();
    if orphans > 0 {
        warnings.push(format!(
            "grok: {orphans} subagent sessions have no readable parent accounting."
        ));
    }
    let mut files = Vec::new();
    for s in &sessions {
        let Some(parsed) = &s.parsed else { continue };
        let mut file = parsed.clone();
        for entry in &mut file.events {
            if s.parent_id.is_empty() {
                continue;
            }
            let e = &mut entry.usage;
            let mut owner = s;
            let mut visited = HashSet::new();
            while !owner.parent_id.is_empty() && visited.insert(owner.session_id.as_str()) {
                let parent = by_id.get(owner.parent_id.as_str()).copied();
                let stamps = native_turns
                    .get(owner.parent_id.as_str())
                    .and_then(|t| t.get(&format!("{}:{}", turn_number(e), e.model)));
                let millis = parse_date(&e.timestamp) as i64;
                let inherited = stamps
                    .is_some_and(|set| (-2..=2).any(|offset| set.contains(&(millis + offset))));
                let before_copy = !owner.forked_at.is_empty() && e.timestamp < owner.forked_at;
                let absent_parent_tie = parent.is_none()
                    && !owner.forked_at.is_empty()
                    && e.timestamp == owner.forked_at;
                if !inherited && !before_copy && !absent_parent_tie {
                    break;
                }
                e.session_id = owner.parent_id.clone();
                let Some(parent) = parent else { break };
                owner = parent;
            }
            e.id = turn_id(&e.session_id, turn_number(e), &e.model);
            entry.sidechain = e.session_id != s.session_id;
        }
        files.push(file);
    }
    (files, warnings)
}
