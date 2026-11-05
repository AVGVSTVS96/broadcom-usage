//! Per-file parsing of Claude Code, Codex, Pi and Muse logs into usage candidates.

use std::collections::{HashMap, HashSet};
use std::io::BufRead;

use serde::{Deserialize, Serialize};

use super::js::{J, iso_millis, js_number, json_array, parse_date, sha256_hex, valid_utf8};
use super::json::{Read, Val, read_lines, valid};
use crate::event::Event;

fn is_false(b: &bool) -> bool {
    !*b
}

/// One parsed usage record before cross-file deduplication.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Candidate {
    #[serde(rename = "e")]
    pub event: Event,
    #[serde(rename = "s", skip_serializing_if = "is_false")]
    pub sidechain: bool,
    #[serde(rename = "w", skip_serializing_if = "is_zero")]
    pub cache_write_1h: i64,
    #[serde(rename = "t", skip_serializing_if = "String::is_empty")]
    pub tier: String,
    #[serde(rename = "n", skip_serializing_if = "String::is_empty")]
    pub native_message_id: String,
    #[serde(rename = "p", skip_serializing_if = "String::is_empty")]
    pub parent_event_id: String,
    #[serde(rename = "l", skip_serializing_if = "String::is_empty")]
    pub legacy_event_id: String,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EmptyRequest {
    pub id: String,
    pub native_message_id: String,
    pub session_id: String,
}

/// Everything deduplication needs from one log, and nothing else: prompts and replies
/// are dropped as each record is read.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Parsed {
    pub events: Vec<Candidate>,
    pub malformed: usize,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub session_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub parent_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub forked_at: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub workspace_root: String,
    #[serde(skip_serializing_if = "HashSet::is_empty")]
    pub compaction_ids: HashSet<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub retracted_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub empty_claude_requests: Vec<EmptyRequest>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub session_title: String,
}

impl Parsed {
    /// Calls id and session for everything that ties this file's records to other files':
    /// deduplication compares records by id, and the replay filters, titles and T3
    /// metadata work within a session and its parent sessions.
    pub fn links(&self, mut id: impl FnMut(&str), mut session: impl FnMut(&str)) {
        if !self.session_id.is_empty() {
            session(&self.session_id);
        }
        if !self.parent_id.is_empty() {
            session(&self.parent_id);
        }
        for entry in &self.events {
            id(&entry.event.id);
            if !entry.legacy_event_id.is_empty() {
                id(&entry.legacy_event_id);
            }
            session(&entry.event.session_id);
        }
        for retracted in &self.retracted_ids {
            id(retracted);
        }
        for request in &self.empty_claude_requests {
            session(&request.session_id);
        }
    }
}

/// Reads each non-blank line as a JSON object the visitor accepts, or (when complete)
/// counts it as malformed.
fn parse_records<R: BufRead>(
    r: &mut R,
    lines: usize,
    malformed: &mut usize,
    mut visit: impl FnMut(Val, usize) -> bool,
) -> std::io::Result<Read> {
    read_lines(r, lines, |raw, number, complete| {
        let Some(raw) = raw else {
            *malformed += 1;
            return;
        };
        let text = valid_utf8(raw);
        if text.trim_matches(super::js::js_space).is_empty() {
            return;
        }
        if !valid(text.as_bytes()) {
            if complete {
                *malformed += 1;
            }
            return;
        }
        let record = Val::parse(&text);
        if (!record.is_object() || !visit(record, number)) && complete {
            *malformed += 1;
        }
    })
}

pub fn number(v: Val) -> i64 {
    match v.num() {
        Some(n) if n > 0.0 => n as i64,
        _ => 0,
    }
}

fn model_text(s: String) -> String {
    if s.is_empty() || s == "<synthetic>" {
        "unknown".into()
    } else {
        s
    }
}

fn model_name(v: Val) -> String {
    model_text(v.text())
}

pub fn project_text(s: &str) -> String {
    if s.is_empty() {
        "Unknown project".into()
    } else {
        s.into()
    }
}

pub fn file_session(file: &str) -> String {
    let name = file.rsplit(['/', '\\']).next().unwrap_or(file);
    let name = match name.len().checked_sub(6) {
        Some(cut) if name.is_char_boundary(cut) && name[cut..].eq_ignore_ascii_case(".jsonl") => {
            &name[..cut]
        }
        _ => name,
    };
    if name.is_empty() {
        "unknown".into()
    } else {
        name.into()
    }
}

/// The value fallback ids hash: a record's numeric or non-empty string ordinal, else its
/// line number.
enum Ordinal {
    Num(f64),
    Text(String),
    Line(usize),
}

fn ordinal_value(ordinal: Val, line: usize) -> Ordinal {
    match (ordinal.num(), ordinal.str()) {
        (Some(n), _) => Ordinal::Num(n),
        (_, Some(s)) if !s.is_empty() => Ordinal::Text(s.into_owned()),
        _ => Ordinal::Line(line),
    }
}

fn fallback_id(harness: &str, session: &str, stamp: &str, ordinal: Ordinal) -> String {
    let ordinal = match &ordinal {
        Ordinal::Num(n) => J::F(*n),
        Ordinal::Text(s) => J::S(s),
        Ordinal::Line(n) => J::I(*n as i64),
    };
    format!(
        "{harness}:metadata:{}",
        sha256_hex(&json_array(&[J::S(session), J::S(stamp), ordinal]))
    )
}

pub fn timestamp(v: Val) -> String {
    match v {
        Val::Num(n) if n > 1e11 => iso_millis(n),
        Val::Num(n) => iso_millis(n * 1000.0),
        Val::Str(_) => iso_millis(parse_date(&v.text())),
        _ => String::new(),
    }
}

fn new_event(harness: &str, stamp: &str, session: &str, model: String, project: &str) -> Event {
    Event {
        timestamp: stamp.into(),
        harness: harness.into(),
        session_id: session.into(),
        model,
        project: project_text(project),
        ..Default::default()
    }
}

/// A Claude Code log's parse state, which can take more lines appended to the file.
#[derive(Default)]
pub struct Claude {
    pub parsed: Parsed,
    represented: HashSet<String>,
    empty: HashMap<String, EmptyRequest>,
    empty_order: Vec<String>,
    kept: HashMap<(String, bool), usize>,
}

impl Claude {
    pub fn resume(parsed: Parsed) -> Self {
        let mut c = Claude::default();
        for (i, entry) in parsed.events.iter().enumerate() {
            c.kept.insert((entry.event.id.clone(), entry.sidechain), i);
            if !entry.sidechain && !entry.native_message_id.is_empty() {
                let id = if entry.parent_event_id.is_empty() {
                    &entry.event.id
                } else {
                    &entry.parent_event_id
                };
                c.represented.insert(id.clone());
            }
        }
        for request in &parsed.empty_claude_requests {
            c.empty_order.push(request.id.clone());
            c.empty.insert(request.id.clone(), request.clone());
        }
        c.parsed = Parsed {
            empty_claude_requests: Vec::new(),
            ..parsed
        };
        c
    }

    /// Parses lines from `r`, numbered on from `lines`.
    pub fn read<R: BufRead>(
        &mut self,
        r: &mut R,
        file: &str,
        lines: usize,
    ) -> std::io::Result<Read> {
        let mut malformed = self.parsed.malformed;
        let read = parse_records(r, lines, &mut malformed, |record, line| {
            self.record(record, line, file)
        });
        self.parsed.malformed = malformed;
        read
    }

    pub fn finish(mut self) -> Parsed {
        for id in &self.empty_order {
            if let Some(request) = self.empty.remove(id) {
                self.parsed.empty_claude_requests.push(request);
            }
        }
        self.parsed
    }

    fn record(&mut self, record: Val, line: usize, file: &str) -> bool {
        let [
            kind,
            custom_title,
            agent_name,
            message,
            stamp,
            session,
            request,
            uuid,
            ordinal,
            cwd,
            sidechain,
            via,
        ] = record.fields([
            "type",
            "customTitle",
            "agentName",
            "message",
            "timestamp",
            "sessionId",
            "requestId",
            "uuid",
            "ordinal",
            "cwd",
            "isSidechain",
            "entrypoint",
        ]);
        if kind.is("custom-title") || kind.is("agent-name") {
            let title = if kind.is("agent-name") {
                agent_name
            } else {
                custom_title
            }
            .text();
            if !title.is_empty() {
                self.parsed.session_title = title;
            }
            return true;
        }
        let [usage, model, message_id] = message.fields(["usage", "model", "id"]);
        if !kind.is("assistant") || !usage.is_object() || model.is("<synthetic>") {
            return true;
        }
        let stamp = timestamp(stamp);
        if stamp.is_empty() {
            return false;
        }
        let mut session = session.text();
        if session.is_empty() {
            session = file_session(file);
        }
        let native_id = message_id.text();
        let id = if !native_id.is_empty() {
            format!("claude:message:{native_id}:{}", request.text())
        } else if !uuid.text().is_empty() {
            format!("claude:uuid:{}", uuid.text())
        } else {
            fallback_id("claude", &session, &stamp, ordinal_value(ordinal, line))
        };
        let sidechain = sidechain == Val::True;
        let (cwd, via) = (cwd.text(), via.text());
        let parent_tier = tier_of(usage, "");
        let mut lines: Vec<Candidate> = Vec::new();
        let append = |lines: &mut Vec<Candidate>,
                      usage: Val,
                      model: Val,
                      usage_id: String,
                      advisor: bool| {
            let [
                cache_creation,
                output_details,
                input,
                output,
                cache_read,
                cache_write,
            ] = usage.fields([
                "cache_creation",
                "output_tokens_details",
                "input_tokens",
                "output_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
            ]);
            let [write_1h, write_5m] =
                cache_creation.fields(["ephemeral_1h_input_tokens", "ephemeral_5m_input_tokens"]);
            let [thinking, reasoning] =
                output_details.fields(["thinking_tokens", "reasoning_tokens"]);
            let mut e = new_event("claude", &stamp, &session, model_name(model), &cwd);
            e.id = usage_id;
            e.entrypoint = via.clone();
            e.input_tokens = number(input);
            e.output_tokens = number(output);
            e.cache_read_tokens = number(cache_read);
            e.cache_write_tokens = number(cache_write).max(number(write_1h) + number(write_5m));
            e.reasoning_tokens = e.output_tokens.min(number(thinking).max(number(reasoning)));
            if e.token_total() == 0 {
                return;
            }
            lines.push(Candidate {
                cache_write_1h: e.cache_write_tokens.min(number(write_1h)),
                tier: tier_of(usage, &parent_tier),
                sidechain,
                native_message_id: native_id.clone(),
                parent_event_id: if advisor { id.clone() } else { String::new() },
                event: e,
                ..Default::default()
            });
        };
        append(&mut lines, usage, model, id.clone(), false);
        let iterations = usage.get("iterations");
        if iterations.is_array() {
            let mut advisor_index = 0i64;
            for (index, value) in iterations.elements().enumerate() {
                let [kind, model] = value.fields(["type", "model"]);
                if !kind.is("advisor_message") {
                    continue;
                }
                let ordinal = advisor_index;
                advisor_index += 1;
                let name = model.text();
                if name.is_empty() || name == "<synthetic>" {
                    continue;
                }
                let advisor_id = format!(
                    "claude:advisor:{}",
                    sha256_hex(&json_array(&[J::S(&id), J::I(ordinal)]))
                );
                let before = lines.len();
                append(&mut lines, value, model, advisor_id, true);
                if index as i64 != ordinal && lines.len() > before {
                    lines.last_mut().unwrap().legacy_event_id = format!(
                        "claude:advisor:{}",
                        sha256_hex(&json_array(&[J::S(&id), J::I(index as i64)]))
                    );
                }
            }
        }
        if !native_id.is_empty() && !sidechain {
            if !lines.is_empty() {
                self.represented.insert(id.clone());
                self.empty.remove(&id);
            } else if !self.represented.contains(&id) {
                if !self.empty.contains_key(&id) {
                    self.empty_order.push(id.clone());
                }
                self.empty.insert(
                    id.clone(),
                    EmptyRequest {
                        id,
                        native_message_id: native_id,
                        session_id: session,
                    },
                );
            }
        }
        for entry in lines {
            self.collapse(entry);
        }
        true
    }

    /// Keeps one candidate per id and sidechain flag: Claude writes a message's usage
    /// once per content block. The kept one is the first with the largest total, which is
    /// what deduplication would pick, at its first position.
    fn collapse(&mut self, mut entry: Candidate) {
        let key = (entry.event.id.clone(), entry.sidechain);
        let Some(&i) = self.kept.get(&key) else {
            self.kept.insert(key, self.parsed.events.len());
            self.parsed.events.push(entry);
            return;
        };
        let kept = &mut self.parsed.events[i];
        if entry.event.token_total() > kept.event.token_total() {
            if entry.legacy_event_id.is_empty() {
                entry.legacy_event_id = std::mem::take(&mut kept.legacy_event_id);
            }
            *kept = entry;
        }
    }
}

/// A Claude usage block's service tier; speed "fast" is priority.
fn tier_of(usage: Val, fallback: &str) -> String {
    let [speed, tier] = usage.fields(["speed", "service_tier"]);
    if speed.is("fast") {
        return "priority".into();
    }
    let tier = tier.text();
    if tier.is_empty() {
        fallback.into()
    } else {
        tier
    }
}

const INPUT: usize = 0;
const CACHED: usize = 1;
const CACHE_WRITE: usize = 2;
const CACHE_CREATION: usize = 3;
const OUTPUT: usize = 4;
const REASONING: usize = 5;
const TOTAL: usize = 6;

type Tokens = [i64; 7];

fn codex_tokens(v: Val) -> Tokens {
    v.fields([
        "input_tokens",
        "cached_input_tokens",
        "cache_write_input_tokens",
        "cache_creation_input_tokens",
        "output_tokens",
        "reasoning_output_tokens",
        "total_tokens",
    ])
    .map(number)
}

fn token_delta(current: Tokens, previous: Option<&Tokens>) -> Tokens {
    let Some(previous) = previous else {
        return current;
    };
    if current[INPUT] < previous[INPUT]
        || current[OUTPUT] < previous[OUTPUT]
        || (current[TOTAL] > 0 && previous[TOTAL] > 0 && current[TOTAL] < previous[TOTAL])
    {
        return current;
    }
    std::array::from_fn(|i| (current[i] - previous[i]).max(0))
}

fn codex_model(v: Val) -> String {
    let [model, model_name, metadata] = v.fields(["model", "model_name", "metadata"]);
    [model.text(), model_name.text()]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| metadata.get("model").text())
}

#[derive(Clone, Default)]
struct CodexContext {
    model: String,
    cwd: String,
    tier: String,
}

fn uuid_v7(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            14 => c == b'7',
            19 => matches!(c.to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b'),
            _ => c.is_ascii_hexdigit(),
        })
}

fn codex_task_start(payload: Val) -> Option<(f64, f64)> {
    let [started_at, turn_id] = payload.fields(["started_at", "turn_id"]);
    let started = timestamp(started_at);
    let turn_id = turn_id.text();
    if uuid_v7(&turn_id) {
        let turn = i64::from_str_radix(&format!("{}{}", &turn_id[..8], &turn_id[9..13]), 16)
            .unwrap_or(0) as f64;
        if started.is_empty() || (turn / 1000.0).floor() == (parse_date(&started) / 1000.0).floor()
        {
            return Some((turn, 1.0));
        }
    }
    let precision = match started_at.num() {
        Some(n) if n == n.trunc() && n <= 1e11 => 1000.0,
        _ => 1.0,
    };
    (!started.is_empty()).then(|| (parse_date(&started), precision))
}

fn codex_event(stamp: &str, session: &str, ctx: &CodexContext, tokens: &Tokens) -> Event {
    let mut e = new_event("codex", stamp, session, ctx.model.clone(), &ctx.cwd);
    let input = tokens[INPUT];
    e.cache_read_tokens = tokens[CACHED].min(input);
    e.cache_write_tokens = tokens[CACHE_WRITE]
        .max(tokens[CACHE_CREATION])
        .min(input - e.cache_read_tokens);
    e.input_tokens = input - e.cache_read_tokens - e.cache_write_tokens;
    e.output_tokens = tokens[OUTPUT];
    e.reasoning_tokens = tokens[REASONING].min(e.output_tokens);
    e
}

struct Precise {
    entry: Candidate,
    turn_id: String,
    explicit_model: String,
    thread_id: String,
    response_id: String,
    line: usize,
    tokens: Tokens,
    thread_tokens: Option<Tokens>,
}

struct Snapshot {
    entry: Candidate,
    line: usize,
    tokens: Tokens,
    total: Option<Tokens>,
    appended: Option<usize>,
}

pub fn parse_codex<R: BufRead>(r: &mut R, file: &str) -> std::io::Result<Parsed> {
    let mut parsed = Parsed {
        session_id: file_session(file),
        ..Default::default()
    };
    let mut ctx = CodexContext {
        model: "unknown".into(),
        ..Default::default()
    };
    let mut contexts: HashMap<String, CodexContext> = HashMap::new();
    let mut previous: Option<Tokens> = None;
    let mut precise: Vec<Precise> = Vec::new();
    let mut snapshots: Vec<Snapshot> = Vec::new();
    let mut compaction_markers: HashMap<String, usize> = HashMap::new();
    let mut first_metadata = true;
    let mut history_start: Option<f64> = None;
    let (mut copied_ancestry, mut inherited_task) = (false, false);
    let mut native_start_line: Option<usize> = None;
    let mut malformed = 0;
    parse_records(r, 0, &mut malformed, |record, line| {
        let [payload, kind, stamp_value, ordinal] =
            record.fields(["payload", "type", "timestamp", "ordinal"]);
        let before_history = |ordinal: Val| matches!((history_start, ordinal.num()), (Some(start), Some(n)) if n < start);
        match kind.text().as_str() {
            "session_meta" => {
                let [
                    id,
                    session_id,
                    history,
                    source,
                    forked_from,
                    parent_thread,
                    created,
                    cwd,
                ] = payload.fields([
                    "id",
                    "session_id",
                    "subagent_history_start_ordinal",
                    "source",
                    "forked_from_id",
                    "parent_thread_id",
                    "timestamp",
                    "cwd",
                ]);
                if !first_metadata {
                    if id.text() == parsed.parent_id {
                        copied_ancestry = true;
                    }
                    return true;
                }
                first_metadata = false;
                history_start = history.num();
                if !id.text().is_empty() {
                    parsed.session_id = id.text();
                } else if !session_id.text().is_empty() {
                    parsed.session_id = session_id.text();
                }
                parsed.parent_id = [forked_from.text(), parent_thread.text()]
                    .into_iter()
                    .find(|s| !s.is_empty())
                    .unwrap_or_else(|| {
                        source
                            .get("subagent")
                            .get("thread_spawn")
                            .get("parent_thread_id")
                            .text()
                    });
                parsed.forked_at = timestamp(created);
                if parsed.forked_at.is_empty() {
                    parsed.forked_at = timestamp(stamp_value);
                }
                ctx.cwd = cwd.text();
                let model = codex_model(payload);
                if !model.is_empty() {
                    ctx.model = model_text(model);
                }
                true
            }
            "turn_context" => {
                let [cwd, tier, turn_id] = payload.fields(["cwd", "service_tier", "turn_id"]);
                let model = codex_model(payload);
                if !model.is_empty() {
                    ctx.model = model_text(model);
                }
                if !cwd.text().is_empty() {
                    ctx.cwd = cwd.text();
                }
                if let Val::Str(_) = tier {
                    ctx.tier = tier.text();
                }
                if !turn_id.text().is_empty() {
                    contexts.insert(turn_id.text(), ctx.clone());
                }
                true
            }
            "compacted" => {
                let id = payload.get("compaction_response_id").text();
                if !id.is_empty() {
                    compaction_markers.entry(id).or_insert(line);
                }
                true
            }
            "event_msg" => {
                let [kind, info, settings] = payload.fields(["type", "info", "thread_settings"]);
                if kind.is("task_started")
                    && !parsed.parent_id.is_empty()
                    && !parsed.forked_at.is_empty()
                    && native_start_line.is_none()
                {
                    if let Some((millis, precision)) = codex_task_start(payload) {
                        let creation = parse_date(&parsed.forked_at);
                        if (millis / precision).floor() < (creation / precision).floor() {
                            inherited_task = true;
                        } else {
                            native_start_line = Some(line);
                        }
                    }
                    return true;
                }
                if kind.is("thread_settings_applied") {
                    let tier = settings.get("service_tier");
                    if let Val::Str(_) = tier {
                        ctx.tier = tier.text();
                    }
                    return true;
                }
                if !kind.is("token_count") || !info.is_object() {
                    return true;
                }
                let stamp = timestamp(stamp_value);
                if stamp.is_empty() {
                    return false;
                }
                let [total_usage, last_usage] =
                    info.fields(["total_token_usage", "last_token_usage"]);
                let mut tokens = None;
                let mut total = None;
                if total_usage.is_object() {
                    let totals = codex_tokens(total_usage);
                    total = Some(totals);
                    if previous != Some(totals) {
                        tokens = Some(if last_usage.is_object() {
                            codex_tokens(last_usage)
                        } else {
                            token_delta(totals, previous.as_ref())
                        });
                    }
                    previous = Some(totals);
                } else if last_usage.is_object() {
                    tokens = Some(codex_tokens(last_usage));
                }
                let Some(tokens) = tokens else { return true };
                if before_history(ordinal) {
                    return true;
                }
                let mut current = ctx.clone();
                let explicit = codex_model(payload);
                if !explicit.is_empty() {
                    current.model = explicit;
                } else {
                    let model = codex_model(info);
                    if !model.is_empty() {
                        current.model = model;
                    }
                }
                let mut e = codex_event(&stamp, &parsed.session_id, &current, &tokens);
                if e.token_total() > 0 {
                    e.id = format!(
                        "codex:total:{}",
                        sha256_hex(&json_array(&[
                            J::S(&parsed.session_id),
                            J::S(&stamp),
                            J::I(e.input_tokens),
                            J::I(e.output_tokens),
                            J::I(e.cache_read_tokens),
                            J::I(e.cache_write_tokens),
                            J::I(e.reasoning_tokens),
                        ]))
                    );
                    snapshots.push(Snapshot {
                        entry: Candidate {
                            event: e,
                            tier: current.tier,
                            ..Default::default()
                        },
                        line,
                        tokens,
                        total,
                        appended: None,
                    });
                }
                true
            }
            "token_usage_record" => {
                let [usage, thread_id, response_id, tier, turn_id, thread_usage] =
                    payload.fields([
                        "usage",
                        "thread_id",
                        "response_id",
                        "service_tier",
                        "turn_id",
                        "thread_token_usage",
                    ]);
                if !usage.is_object() || before_history(ordinal) {
                    return true;
                }
                let stamp = timestamp(stamp_value);
                if stamp.is_empty() {
                    return false;
                }
                let thread_id = thread_id.text();
                if !thread_id.is_empty() && thread_id != parsed.session_id {
                    return true;
                }
                let tokens = codex_tokens(usage);
                let mut e = codex_event(&stamp, &parsed.session_id, &ctx, &tokens);
                if e.token_total() == 0 {
                    return true;
                }
                let response_id = response_id.text();
                e.id = if response_id.is_empty() {
                    fallback_id(
                        "codex",
                        &parsed.session_id,
                        &stamp,
                        ordinal_value(ordinal, line),
                    )
                } else {
                    format!("codex:response:{response_id}")
                };
                let mut tier = tier.text();
                if tier.is_empty() {
                    tier = ctx.tier.clone();
                }
                precise.push(Precise {
                    entry: Candidate {
                        event: e,
                        tier,
                        ..Default::default()
                    },
                    turn_id: turn_id.text(),
                    explicit_model: codex_model(payload),
                    thread_id,
                    response_id,
                    line,
                    tokens,
                    thread_tokens: thread_usage.is_object().then(|| codex_tokens(thread_usage)),
                });
                true
            }
            _ => true,
        }
    })?;
    parsed.malformed = malformed;
    let first_record = precise
        .iter()
        .filter(|p| !p.thread_id.is_empty() && !compaction_markers.contains_key(&p.response_id))
        .map(|p| p.entry.event.timestamp.as_str())
        .fold("\u{ffff}", |a, b| if b < a { b } else { a })
        .to_string();
    let boundary = if !parsed.parent_id.is_empty() && (copied_ancestry || inherited_task) {
        match native_start_line {
            Some(line) => Some(line),
            None if inherited_task => Some(usize::MAX),
            None => None,
        }
    } else {
        None
    };
    for snapshot in &mut snapshots {
        if snapshot.entry.event.timestamp >= first_record {
            continue;
        }
        if boundary.is_some_and(|b| snapshot.line < b) {
            parsed.retracted_ids.push(snapshot.entry.event.id.clone());
            continue;
        }
        snapshot.appended = Some(parsed.events.len());
        parsed.events.push(snapshot.entry.clone());
    }
    parsed.compaction_ids = precise
        .iter()
        .filter(|p| compaction_markers.contains_key(&p.response_id))
        .map(|p| p.response_id.clone())
        .collect();
    let next_lines: Vec<usize> = precise.iter().map(|p| p.line).collect();
    for (index, mut pending) in precise.into_iter().enumerate() {
        let marker = compaction_markers.get(&pending.response_id).copied();
        if pending.thread_id.is_empty() && marker.is_none() {
            continue;
        }
        if pending.thread_id.is_empty() && boundary.is_some_and(|b| pending.line < b) {
            parsed.retracted_ids.push(pending.entry.event.id);
            continue;
        }
        if let Some(marker_line) = marker {
            let low = snapshots.partition_point(|s| s.line <= pending.line);
            let boundary = next_lines
                .get(index + 1)
                .map_or(marker_line, |&next| marker_line.min(next));
            let covered = snapshots[low..].iter_mut().take_while(|s| s.line < boundary).find(|s| {
                s.entry.event.timestamp < first_record
                    && (s.tokens == pending.tokens
                        || matches!((pending.thread_tokens, s.total), (Some(a), Some(b)) if a == b))
            });
            if let Some(covered) = covered {
                covered.entry.event.id = pending.entry.event.id.clone();
                if let Some(i) = covered.appended {
                    parsed.events[i].event.id = pending.entry.event.id;
                }
                continue;
            }
        }
        if let Some(turn) = contexts.get(&pending.turn_id) {
            pending.entry.event.model = turn.model.clone();
            pending.entry.event.project = project_text(&turn.cwd);
            if pending.entry.tier.is_empty() {
                pending.entry.tier = turn.tier.clone();
            }
        }
        if !pending.explicit_model.is_empty() {
            pending.entry.event.model = model_text(pending.explicit_model);
        }
        parsed.events.push(pending.entry);
    }
    Ok(parsed)
}

pub fn parse_pi<R: BufRead>(r: &mut R, file: &str) -> std::io::Result<Parsed> {
    let mut parsed = Parsed {
        session_id: file_session(file),
        ..Default::default()
    };
    let (mut cwd, mut model) = (String::new(), String::from("unknown"));
    let mut malformed = 0;
    parse_records(r, 0, &mut malformed, |record, line| {
        let [
            kind,
            id,
            parent,
            stamp,
            record_cwd,
            model_id,
            message,
            ordinal,
        ] = record.fields([
            "type",
            "id",
            "parentSession",
            "timestamp",
            "cwd",
            "modelId",
            "message",
            "ordinal",
        ]);
        if kind.is("session") {
            if !id.text().is_empty() {
                parsed.session_id = id.text();
            }
            let parent = parent.text();
            (parsed.parent_id, parsed.forked_at) = (String::new(), String::new());
            if !parent.is_empty() {
                parsed.parent_id = file_session(&parent)
                    .rsplit('_')
                    .next()
                    .unwrap_or_default()
                    .into();
                parsed.forked_at = timestamp(stamp);
            }
            cwd = record_cwd.text();
            return true;
        }
        if kind.is("model_change") {
            model = model_name(model_id);
            return true;
        }
        let [role, usage, message_model] = message.fields(["role", "usage", "model"]);
        if !kind.is("message") || !role.is("assistant") || !usage.is_object() {
            return true;
        }
        let stamp = timestamp(stamp);
        if stamp.is_empty() {
            return false;
        }
        if !message_model.text().is_empty() {
            model = model_name(message_model);
        }
        let [input, output, cache_read, cache_write, reasoning, write_1h] = usage.fields([
            "input",
            "output",
            "cacheRead",
            "cacheWrite",
            "reasoning",
            "cacheWrite1h",
        ]);
        let mut e = new_event("pi", &stamp, &parsed.session_id, model.clone(), &cwd);
        e.input_tokens = number(input);
        e.output_tokens = number(output);
        e.cache_read_tokens = number(cache_read);
        e.cache_write_tokens = number(cache_write);
        e.reasoning_tokens = number(reasoning).min(e.output_tokens);
        if e.token_total() == 0 {
            return true;
        }
        e.id = if id.text().is_empty() {
            fallback_id(
                "pi",
                &parsed.session_id,
                &stamp,
                ordinal_value(ordinal, line),
            )
        } else {
            format!("pi:{}:{}", parsed.session_id, id.text())
        };
        let cache_write_1h = e.cache_write_tokens.min(number(write_1h));
        parsed.events.push(Candidate {
            event: e,
            cache_write_1h,
            ..Default::default()
        });
        true
    })?;
    parsed.malformed = malformed;
    Ok(parsed)
}

/// Muse's microsecond clock; other scales are normalized.
fn muse_timestamp(v: Val) -> String {
    let Some(value) = v.num() else {
        return timestamp(v);
    };
    if value > 1e17 {
        iso_millis(value / 1e6)
    } else if value > 1e14 {
        iso_millis(value / 1000.0)
    } else if value > 1e11 {
        iso_millis(value)
    } else {
        iso_millis(value * 1000.0)
    }
}

/// Reads <date>/<session>/session.jsonl and grouped child sessions such as
/// <session>/subagent/<child>/session.jsonl.
fn muse_path_ids(file: &str) -> (String, String) {
    let parts: Vec<&str> = file.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    let at = |from_end: usize| {
        if parts.len() >= from_end {
            parts[parts.len() - from_end]
        } else {
            ""
        }
    };
    let group = |s: &str| matches!(s, "subagent" | "approval-review" | "workflows");
    let mut session = file_session(file);
    if session == "session" {
        session = if at(2).is_empty() {
            "unknown".into()
        } else {
            at(2).into()
        };
    }
    let mut parent = if group(at(3)) {
        at(4)
    } else if group(at(2)) {
        at(3)
    } else {
        ""
    }
    .to_string();
    if parent == session {
        parent.clear();
    }
    (session, parent)
}

struct Muse {
    parsed: Parsed,
    cwd: String,
    model: String,
}

impl Muse {
    fn visit(&mut self, record: Val, line: usize, child_path: &str) -> bool {
        let [
            children,
            stream,
            payload,
            payload_type,
            recorded_at,
            id,
            sequence,
        ] = record.fields([
            "children",
            "stream",
            "payload",
            "payload_type",
            "recorded_at",
            "id",
            "sequence",
        ]);
        if children.is_array() {
            for (index, child) in children.elements().enumerate() {
                let [record_json, child_index] = child.fields(["record_json", "child_index"]);
                let raw = record_json.text();
                if raw.is_empty() {
                    continue;
                }
                let ordinal = js_number(child_index.num().unwrap_or(index as f64));
                let next = if child_path.is_empty() {
                    ordinal
                } else {
                    format!("{child_path}:{ordinal}")
                };
                if !valid(raw.as_bytes()) {
                    self.parsed.malformed += 1;
                    continue;
                }
                let inner = Val::parse(&raw);
                if !inner.is_object() || !self.visit(inner, line, &next) {
                    self.parsed.malformed += 1;
                }
            }
            return true;
        }
        let [stream_kind, stream_id] = stream.fields(["kind", "id"]);
        let stream_kind = stream_kind.text();
        if !stream_kind.is_empty() && stream_kind != "session" {
            return true;
        }
        if stream_kind == "session" && !stream_id.text().is_empty() {
            self.parsed.session_id = stream_id.text();
        }
        if matches!(
            payload_type.text().as_str(),
            "runtime.session.metadata"
                | "session.workspace_branch.observed"
                | "run.model.configured"
        ) {
            let [root, model_id] = payload.get("record").fields(["workspace_root", "model_id"]);
            let root = root.text();
            if !root.is_empty() {
                self.cwd = root;
                if self.parsed.workspace_root.is_empty() {
                    self.parsed.workspace_root = self.cwd.clone();
                }
            }
            if !model_id.text().is_empty() {
                self.model = model_name(model_id);
            }
            return true;
        }
        let [event, source_record] = payload.fields(["event", "source_run_record_id"]);
        let [kind, usage, event_model] = event.fields(["kind", "usage", "model"]);
        if !kind.is("model_completed") || !usage.is_object() {
            return true;
        }
        let stamp = muse_timestamp(recorded_at);
        if stamp.is_empty() {
            return false;
        }
        let [input, cache_read, cached, cache_write, output, reasoning] = usage.fields([
            "input_tokens",
            "cache_read_tokens",
            "cached_tokens",
            "cache_write_tokens",
            "output_tokens",
            "reasoning_tokens",
        ]);
        let mut model = event_model.text();
        if model.is_empty() {
            model = self.model.clone();
        }
        let mut e = new_event(
            "muse",
            &stamp,
            &self.parsed.session_id,
            model_text(model),
            &self.cwd,
        );
        let input = number(input);
        e.cache_read_tokens = input.min(number(cache_read).max(number(cached)));
        e.cache_write_tokens = (input - e.cache_read_tokens).min(number(cache_write));
        e.input_tokens = input - e.cache_read_tokens - e.cache_write_tokens;
        e.output_tokens = number(output);
        e.reasoning_tokens = e.output_tokens.min(number(reasoning));
        if e.token_total() == 0 {
            return true;
        }
        let mut native_id = source_record.text();
        if native_id.is_empty() {
            native_id = id.text();
        }
        e.id = if !native_id.is_empty() {
            format!("muse:record:{native_id}")
        } else {
            let ordinal = match sequence.num() {
                Some(n) => Ordinal::Num(n),
                None if !child_path.is_empty() => Ordinal::Text(format!("{line}:{child_path}")),
                None => ordinal_value(sequence, line),
            };
            fallback_id("muse", &self.parsed.session_id, &stamp, ordinal)
        };
        self.parsed.events.push(Candidate {
            event: e,
            ..Default::default()
        });
        true
    }
}

pub fn parse_muse<R: BufRead>(r: &mut R, file: &str) -> std::io::Result<Parsed> {
    let (session_id, parent_id) = muse_path_ids(file);
    let mut muse = Muse {
        parsed: Parsed {
            session_id,
            parent_id,
            ..Default::default()
        },
        cwd: String::new(),
        model: "unknown".into(),
    };
    let mut malformed = 0;
    parse_records(r, 0, &mut malformed, |record, line| {
        muse.visit(record, line, "")
    })?;
    muse.parsed.malformed += malformed;
    Ok(muse.parsed)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn lines(records: &[&str]) -> std::io::Cursor<Vec<u8>> {
        std::io::Cursor::new(
            records
                .iter()
                .map(|r| format!("{r}\n"))
                .collect::<String>()
                .into_bytes(),
        )
    }

    pub fn claude(records: &[&str], file: &str) -> Parsed {
        let mut c = Claude::default();
        c.read(&mut lines(records), file, 0).unwrap();
        c.finish()
    }

    #[test]
    fn claude_collapses_repeats_and_reads_titles() {
        let parsed = claude(
            &[
                r#"{"type":"custom-title","customTitle":"fix the bug"}"#,
                r#"{"type":"assistant","sessionId":"s1","requestId":"r1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/w","message":{"id":"m1","model":"claude-opus-4-5","usage":{"input_tokens":10,"output_tokens":1}}}"#,
                r#"{"type":"assistant","sessionId":"s1","requestId":"r1","timestamp":"2026-01-01T00:00:01.000Z","cwd":"/w","message":{"id":"m1","model":"claude-opus-4-5","usage":{"input_tokens":10,"output_tokens":5,"service_tier":"standard"}}}"#,
                "not json",
            ],
            "/p/s1.jsonl",
        );
        assert_eq!(parsed.events.len(), 1);
        assert_eq!(parsed.events[0].event.output_tokens, 5);
        assert_eq!(parsed.events[0].tier, "standard");
        assert_eq!(
            (parsed.malformed, parsed.session_title.as_str()),
            (1, "fix the bug")
        );
    }

    #[test]
    fn claude_records_the_entrypoint() {
        let parsed = claude(
            &[
                r#"{"type":"assistant","entrypoint":"sdk-cli","sessionId":"s1","requestId":"r1","timestamp":"2026-01-01T00:00:00.000Z","message":{"id":"m1","model":"claude-opus-4-5","usage":{"input_tokens":10,"output_tokens":1}}}"#,
            ],
            "/p/s1.jsonl",
        );
        assert_eq!(parsed.events.len(), 1);
        assert_eq!(parsed.events[0].event.entrypoint, "sdk-cli");
    }

    #[test]
    fn claude_resumes_appended_lines_exactly() {
        let records = [
            r#"{"type":"assistant","sessionId":"s1","requestId":"r1","timestamp":"2026-01-01T00:00:00.000Z","message":{"id":"m1","model":"claude-opus-4-5","usage":{"input_tokens":0,"output_tokens":0}}}"#,
            r#"{"type":"assistant","sessionId":"s1","requestId":"r2","timestamp":"2026-01-01T00:00:00.000Z","message":{"id":"m2","model":"claude-opus-4-5","usage":{"input_tokens":3,"output_tokens":1,"iterations":[{"type":"x"},{"type":"advisor_message","model":"claude-sonnet-4-5","input_tokens":4}]}}}"#,
            r#"{"type":"assistant","sessionId":"s1","timestamp":"2026-01-01T00:00:02.000Z","message":{"model":"m","usage":{"input_tokens":1}}}"#,
            r#"{"type":"assistant","sessionId":"s1","requestId":"r1","timestamp":"2026-01-01T00:00:03.000Z","message":{"id":"m1","model":"claude-opus-4-5","usage":{"input_tokens":7,"output_tokens":2}}}"#,
            r#"{"type":"assistant","sessionId":"s1","requestId":"r3","timestamp":"2026-01-01T00:00:04.000Z","message":{"id":"m3","model":"claude-opus-4-5","usage":{"input_tokens":0}}}"#,
            r#"{"type":"agent-name","agentName":"helper"}"#,
            r#"{"type":"assistant","sessionId":"s1","requestId":"r2","timestamp":"2026-01-01T00:00:05.000Z","message":{"id":"m2","model":"claude-opus-4-5","usage":{"input_tokens":30,"output_tokens":1}}}"#,
        ];
        let whole = claude(&records, "/p/s1.jsonl");
        for split in 0..=records.len() {
            let mut first = Claude::default();
            let read = first
                .read(&mut lines(&records[..split]), "/p/s1.jsonl", 0)
                .unwrap();
            let mut resumed = Claude::resume(first.finish());
            resumed
                .read(&mut lines(&records[split..]), "/p/s1.jsonl", read.lines)
                .unwrap();
            assert_eq!(resumed.finish(), whole, "split at {split}");
        }
    }

    #[test]
    fn codex_counters_become_requests() {
        let parent = parse_codex(
            &mut lines(&[
                r#"{"type":"session_meta","timestamp":"2026-01-01T00:00:00.000Z","payload":{"id":"p1","cwd":"/w","model":"gpt-5"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-01-01T00:00:10.000Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"total_tokens":110}}}}"#,
                r#"{"type":"event_msg","timestamp":"2026-01-01T00:00:20.000Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"total_tokens":110}}}}"#,
                r#"{"type":"event_msg","timestamp":"2026-01-01T00:00:30.000Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":250,"cached_input_tokens":100,"output_tokens":30,"total_tokens":280}}}}"#,
            ]),
            "/s/rollout-p1.jsonl",
        )
        .unwrap();
        assert_eq!(parent.events.len(), 2);
        let second = &parent.events[1].event;
        assert_eq!(
            (
                second.input_tokens,
                second.cache_read_tokens,
                second.output_tokens
            ),
            (90, 60, 20)
        );
        assert_eq!(second.project, "/w");
    }
}
