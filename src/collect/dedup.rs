//! Cross-file deduplication over one pass's parsed logs, which it may rewrite in place.

use std::collections::{HashMap, HashSet};

use super::parse::{Candidate, Harness, Parsed, Usage, project_text};

fn harness(file: &Parsed) -> Option<Harness> {
    file.events.first().map(|e| e.usage.harness)
}

fn owner(entry: &Candidate) -> &str {
    if entry.parent_event_id.is_empty() {
        &entry.usage.id
    } else {
        &entry.parent_event_id
    }
}

/// The identities claiming one native Claude request: the first one, and whether a
/// different one was seen.
struct Owners {
    id: String,
    multiple: bool,
    billed: bool,
}

fn remember<'a>(
    originals: &'a mut HashMap<(String, String), Owners>,
    native: &str,
    session: &str,
    id: &str,
) -> &'a mut Owners {
    let o = originals
        .entry((native.to_string(), session.to_string()))
        .or_insert_with(|| Owners {
            id: id.to_string(),
            multiple: false,
            billed: false,
        });
    if o.id != id {
        o.multiple = true;
    }
    o
}

fn filter_claude_replays(files: &mut [Parsed]) {
    let mut originals: HashMap<(String, String), Owners> = HashMap::new();
    for file in files.iter() {
        for request in &file.empty_claude_requests {
            remember(
                &mut originals,
                &request.native_message_id,
                &request.session_id,
                &request.id,
            );
        }
        for entry in &file.events {
            if entry.usage.harness != Harness::Claude
                || entry.sidechain
                || entry.native_message_id.is_empty()
            {
                continue;
            }
            let o = remember(
                &mut originals,
                &entry.native_message_id,
                &entry.usage.session_id,
                owner(entry),
            );
            if entry.usage.token_total() > 0 {
                o.billed = true;
            }
        }
    }
    for file in files.iter_mut() {
        drop_where(file, |entry| {
            entry.usage.harness == Harness::Claude
                && entry.sidechain
                && !entry.native_message_id.is_empty()
                && originals
                    .get(&(
                        entry.native_message_id.clone(),
                        entry.usage.session_id.clone(),
                    ))
                    .is_some_and(|o| !o.multiple && o.billed)
        });
    }
}

/// Removes the events `drop` picks, retracting their identities unless another file
/// keeps them.
fn drop_where(file: &mut Parsed, mut drop: impl FnMut(&Candidate) -> bool) {
    let mut dropped = Vec::new();
    file.events.retain(|entry| {
        let gone = drop(entry);
        if gone {
            dropped.push(entry.usage.id.clone());
            if !entry.legacy_event_id.is_empty() {
                dropped.push(entry.legacy_event_id.clone());
            }
        }
        !gone
    });
    file.retracted_ids.extend(dropped);
}

fn filter_pi_replays(files: &mut [Parsed]) {
    let sessions: Vec<usize> = (0..files.len())
        .filter(|&i| {
            files[i]
                .events
                .iter()
                .any(|e| e.usage.harness == Harness::Pi)
        })
        .collect();
    let mut parents: HashMap<String, (String, String)> = HashMap::new();
    let mut native: HashMap<String, HashSet<String>> = HashMap::new();
    for &i in &sessions {
        let file = &files[i];
        parents.insert(
            file.session_id.clone(),
            (file.parent_id.clone(), file.forked_at.clone()),
        );
        let prefix = format!("pi:{}:", file.session_id).len();
        let set = file
            .events
            .iter()
            .map(|entry| {
                format!(
                    "{}:{}",
                    entry.usage.id.get(prefix..).unwrap_or(""),
                    entry.usage.timestamp
                )
            })
            .collect();
        native.insert(file.session_id.clone(), set);
    }
    for &i in &sessions {
        let file = &mut files[i];
        if file.parent_id.is_empty() || file.parent_id == file.session_id {
            continue;
        }
        let prefix = format!("pi:{}:", file.session_id);
        let mut moved_ids = Vec::new();
        for entry in &mut file.events {
            let Some(native_id) = entry.usage.id.strip_prefix(&prefix).map(str::to_string) else {
                continue;
            };
            let (mut session, mut id) = (entry.usage.session_id.clone(), entry.usage.id.clone());
            let (mut owner_parent, mut owner_forked) =
                (file.parent_id.clone(), file.forked_at.clone());
            let mut visited = HashSet::from([file.session_id.clone()]);
            while !owner_parent.is_empty() && !visited.contains(&owner_parent) {
                let inherited = native.get(&owner_parent).is_some_and(|set| {
                    set.contains(&format!("{native_id}:{}", entry.usage.timestamp))
                });
                let before_copy = !owner_forked.is_empty() && entry.usage.timestamp < owner_forked;
                if !inherited && !before_copy {
                    break;
                }
                session = owner_parent.clone();
                id = format!("pi:{session}:{native_id}");
                visited.insert(owner_parent.clone());
                let Some((parent, forked)) = parents.get(&owner_parent) else {
                    break;
                };
                (owner_parent, owner_forked) = (parent.clone(), forked.clone());
            }
            if id != entry.usage.id {
                moved_ids.push(std::mem::replace(&mut entry.usage.id, id));
                entry.usage.session_id = session;
                entry.sidechain = true;
            }
        }
        file.retracted_ids.extend(moved_ids);
    }
}

fn filter_codex_replays(files: &mut [Parsed]) {
    let mut parents: HashMap<String, usize> = HashMap::new();
    for (i, file) in files.iter().enumerate() {
        if harness(file) == Some(Harness::Codex) {
            parents.insert(file.session_id.clone(), i);
        }
    }
    type Replay = (String, i64, i64, i64, i64, i64);
    let key = |e: &Usage| -> Replay {
        (
            e.timestamp.clone(),
            e.input_tokens,
            e.output_tokens,
            e.cache_read_tokens,
            e.cache_write_tokens,
            e.reasoning_tokens,
        )
    };
    let mut parent_replays: HashMap<usize, HashSet<Replay>> = HashMap::new();
    for c in 0..files.len() {
        let child = &files[c];
        if harness(child) != Some(Harness::Codex)
            || child.parent_id.is_empty()
            || child.parent_id == child.session_id
        {
            continue;
        }
        let parent = parents.get(&child.parent_id).copied();
        if let Some(p) = parent.filter(|&p| p != c) {
            parent_replays
                .entry(p)
                .or_insert_with(|| files[p].events.iter().map(|e| key(&e.usage)).collect());
        }
        let parent_compactions = parent
            .map(|p| files[p].compaction_ids.clone())
            .unwrap_or_default();
        let replays = parent.and_then(|p| parent_replays.get(&p));
        let child = &files[c];
        let keep: Vec<bool> = child
            .events
            .iter()
            .map(|replay| {
                if !child.forked_at.is_empty() && replay.usage.timestamp < child.forked_at {
                    return false;
                }
                let response = replay
                    .usage
                    .id
                    .strip_prefix("codex:response:")
                    .unwrap_or(&replay.usage.id);
                if parent.is_some()
                    && child.compaction_ids.contains(response)
                    && parent_compactions.contains(response)
                {
                    return false;
                }
                if parent.is_none_or(|p| p == c) || !replay.usage.id.starts_with("codex:total:") {
                    return true;
                }
                !replays.is_some_and(|r| r.contains(&key(&replay.usage)))
            })
            .collect();
        let mut keep = keep.into_iter();
        drop_where(&mut files[c], |_| !keep.next().unwrap_or(true));
        parent_replays.remove(&c);
    }
}

/// Gives child Muse sessions their enclosing session's project.
fn apply_muse_parent_projects(files: &mut [Parsed]) {
    let sessions: Vec<usize> = (0..files.len())
        .filter(|&i| {
            !files[i].workspace_root.is_empty()
                || files[i]
                    .events
                    .iter()
                    .any(|e| e.usage.harness == Harness::Muse)
        })
        .collect();
    if sessions.is_empty() {
        return;
    }
    let mut projects: HashMap<String, String> = HashMap::new();
    let mut parents: HashMap<String, String> = HashMap::new();
    for &i in &sessions {
        let file = &files[i];
        if !file.parent_id.is_empty() {
            parents.insert(file.session_id.clone(), file.parent_id.clone());
        }
        let mut known = file
            .events
            .iter()
            .map(|e| e.usage.project.clone())
            .find(|p| p != "Unknown project")
            .unwrap_or_default();
        if known.is_empty() && !file.workspace_root.is_empty() {
            known = project_text(&file.workspace_root);
        }
        if !known.is_empty() {
            projects.entry(file.session_id.clone()).or_insert(known);
        }
    }
    for file in files.iter() {
        if !file.session_id.is_empty() && !file.parent_id.is_empty() {
            parents
                .entry(file.session_id.clone())
                .or_insert_with(|| file.parent_id.clone());
        }
    }
    for &i in &sessions {
        let file = &mut files[i];
        let mut project = projects.get(&file.session_id).cloned().unwrap_or_default();
        let mut visited = HashSet::from([file.session_id.clone()]);
        let mut parent = parents.get(&file.session_id).cloned().unwrap_or_default();
        while project.is_empty() && !parent.is_empty() && visited.insert(parent.clone()) {
            project = projects.get(&parent).cloned().unwrap_or_default();
            parent = parents.get(&parent).cloned().unwrap_or_default();
        }
        if project.is_empty() {
            continue;
        }
        for entry in &mut file.events {
            if entry.usage.project == "Unknown project" {
                entry.usage.project = project.clone();
            }
        }
    }
}

/// Drops and reattributes replayed records, listing every identity they lose with the
/// file's retracted ids.
pub fn filter_replays(files: &mut [Parsed]) {
    filter_claude_replays(files);
    filter_codex_replays(files);
    filter_pi_replays(files);
    apply_muse_parent_projects(files);
}

/// Keeps one candidate per id: a primary record over a sidechain copy, then the larger
/// token total. Order is each id's first appearance; the map gives each kept id's position.
pub fn deduplicate(files: &[Parsed]) -> (Vec<&Candidate>, HashMap<&str, usize>) {
    let total = files.iter().map(|f| f.events.len()).sum();
    let mut index: HashMap<&str, usize> = HashMap::with_capacity(total);
    let mut entries: Vec<&Candidate> = Vec::with_capacity(total);
    for incoming in files.iter().flat_map(|f| &f.events) {
        let Some(&i) = index.get(incoming.usage.id.as_str()) else {
            index.insert(&incoming.usage.id, entries.len());
            entries.push(incoming);
            continue;
        };
        let existing = entries[i];
        if (existing.sidechain && !incoming.sidechain)
            || (existing.sidechain == incoming.sidechain
                && incoming.usage.token_total() > existing.usage.token_total())
        {
            entries[i] = incoming;
        }
    }
    (entries, index)
}

#[cfg(test)]
mod tests {
    use super::super::parse::tests::{claude, lines};
    use super::super::parse::{parse_codex, parse_pi};
    use super::*;

    #[test]
    fn claude_sidechain_replays_are_dropped_and_retracted() {
        let main = claude(
            &[
                r#"{"type":"assistant","sessionId":"s1","requestId":"r1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/w","message":{"id":"m1","model":"claude-opus-4-5","usage":{"input_tokens":10,"output_tokens":5}}}"#,
            ],
            "/p/s1.jsonl",
        );
        let replay = claude(
            &[
                r#"{"type":"assistant","isSidechain":true,"sessionId":"s1","requestId":"r9","timestamp":"2026-01-01T00:01:00.000Z","message":{"id":"m1","model":"claude-opus-4-5","usage":{"input_tokens":10,"cache_read_input_tokens":9000,"output_tokens":5}}}"#,
            ],
            "/p/s1/subagents/agent.jsonl",
        );
        let mut files = vec![main, replay];
        filter_replays(&mut files);
        let (entries, _) = deduplicate(&files);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].usage.id, "claude:message:m1:r1");
        assert_eq!(files[1].retracted_ids, ["claude:message:m1:r9"]);
    }

    #[test]
    fn codex_forks_keep_only_their_own_requests() {
        let parent = parse_codex(
            &mut lines(&[
                r#"{"type":"session_meta","timestamp":"2026-01-01T00:00:00.000Z","payload":{"id":"p1","cwd":"/w","model":"gpt-5"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-01-01T00:00:10.000Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"total_tokens":110}}}}"#,
                r#"{"type":"event_msg","timestamp":"2026-01-01T00:00:30.000Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":250,"cached_input_tokens":100,"output_tokens":30,"total_tokens":280}}}}"#,
            ]),
            "/s/rollout-p1.jsonl",
        )
        .unwrap();
        let child = parse_codex(
            &mut lines(&[
                r#"{"type":"session_meta","timestamp":"2026-01-01T00:00:25.000Z","payload":{"id":"c1","forked_from_id":"p1","cwd":"/w"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-01-01T00:00:10.000Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"total_tokens":110}}}}"#,
                r#"{"type":"event_msg","timestamp":"2026-01-01T00:01:00.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":7,"output_tokens":3}}}}"#,
            ]),
            "/s/rollout-c1.jsonl",
        )
        .unwrap();
        let mut files = vec![parent, child];
        filter_replays(&mut files);
        assert_eq!(deduplicate(&files).0.len(), 3);
    }

    #[test]
    fn pi_fork_entries_belong_to_the_parent() {
        let parent = parse_pi(
            &mut lines(&[
                r#"{"type":"session","id":"aaa","cwd":"/w"}"#,
                r#"{"type":"message","id":"e1","timestamp":"2026-01-01T00:00:00.000Z","message":{"role":"assistant","model":"m","usage":{"input":5,"output":1}}}"#,
            ]),
            "/pi/2026_aaa.jsonl",
        )
        .unwrap();
        let fork = parse_pi(
            &mut lines(&[
                r#"{"type":"session","id":"bbb","cwd":"/w","parentSession":"/pi/2026_aaa.jsonl","timestamp":"2026-01-01T00:05:00.000Z"}"#,
                r#"{"type":"message","id":"e1","timestamp":"2026-01-01T00:00:00.000Z","message":{"role":"assistant","model":"m","usage":{"input":5,"output":1}}}"#,
                r#"{"type":"message","id":"e2","timestamp":"2026-01-01T00:06:00.000Z","message":{"role":"assistant","model":"m","usage":{"input":2,"output":1}}}"#,
            ]),
            "/pi/2026_bbb.jsonl",
        )
        .unwrap();
        let mut files = vec![parent, fork];
        filter_replays(&mut files);
        let (entries, _) = deduplicate(&files);
        let ids: HashSet<&str> = entries.iter().map(|e| e.usage.id.as_str()).collect();
        assert_eq!(ids, HashSet::from(["pi:aaa:e1", "pi:bbb:e2"]));
        assert_eq!(files[1].retracted_ids, ["pi:bbb:e1"]);
    }
}
