use std::collections::BTreeMap;

use foldhash::{HashMap, HashMapExt, HashSet, HashSetExt};

use serde::Serialize;

use super::attribution::AgentSdk;
use super::identity::{
    Names, SessionRef, Who, agent_of, app_of, basename, label_of, prefix, row_of, who_of,
};
use super::live::LiveSession;
use super::sessions::{Amount, Amounts, NO_ACCOUNT, Period, Session, SessionUsage};
use super::timeframe::Range;
use crate::event::Device;

const COLORS: usize = 7;
const GROUPINGS: [&str; 8] = [
    "agent",
    "session",
    "project",
    "model",
    "harness",
    "entrypoint",
    "device",
    "account",
];
const MEASURES: [&str; 5] = ["tokens", "api", "plan", "fiveHour", "week"];
const DIMENSIONS: [&str; 7] = [
    "app",
    "harness",
    "entrypoint",
    "model",
    "project",
    "device",
    "account",
];

/// What the Usage page asks for.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Query {
    pub range: Range,
    pub by: String,
    pub measure: String,
    #[serde(rename = "where")]
    pub filter: BTreeMap<String, Vec<String>>,
    pub search: String,
    pub limit: usize,
    /// Only rows under this agent row key, so a row drills into its sessions.
    pub within: Option<String>,
}

fn pick(value: &str, options: &[&str], fallback: &str) -> String {
    options
        .iter()
        .find(|o| **o == value)
        .unwrap_or(&fallback)
        .to_string()
}

/// Reads a Usage query from the page's URL query string.
pub fn parse_query(query: &str) -> Query {
    let mut params: HashMap<String, String> = HashMap::new();
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        params
            .entry(key.into_owned())
            .or_insert_with(|| value.into_owned());
    }
    let get = |key: &str| params.get(key).map(String::as_str).unwrap_or_default();
    let mut filter = BTreeMap::new();
    for dimension in DIMENSIONS {
        let mut values: Vec<String> = get(&format!("w.{dimension}"))
            .split(',')
            .filter(|v| !v.is_empty())
            .map(String::from)
            .collect();
        if !values.is_empty() {
            values.sort();
            filter.insert(dimension.to_string(), values);
        }
    }
    Query {
        range: Range::parse(get("range")),
        by: pick(get("by"), &GROUPINGS, "agent"),
        measure: pick(get("measure"), &MEASURES, "api"),
        filter,
        search: get("q").trim().to_string(),
        limit: get("limit")
            .parse::<i64>()
            .ok()
            .filter(|n| *n > 0)
            .map_or(50, |n| n as usize),
        within: Some(get("within").to_string()).filter(|w| !w.is_empty()),
    }
}

/// Percentage points of account limits, summed over accounts and windows. Null: nothing to estimate from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Points {
    pub five_hour: Option<f64>,
    pub week: Option<f64>,
}

/// One account's points within a row.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct AccountPoints {
    pub account: String,
    #[serde(flatten)]
    pub points: Points,
}

/// The rows in view, summed.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    #[serde(flatten)]
    pub amount: Amount,
    pub sessions: usize,
    pub cache_hit_rate: f64,
    pub points: Points,
}

/// One ranked group.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub key: String,
    pub label: String,
    pub detail: String,
    pub who: Option<Who>,
    /// Categorical slot 0 to 6 for the top rows by API value in the unfiltered range, 7 for the rest.
    pub color: usize,
    pub amount: Amount,
    pub points: Points,
    pub by_account: Vec<AccountPoints>,
    pub sessions: usize,
    pub last_active_at: f64,
    /// The chosen measure per bucket of the view's buckets.
    pub series: Vec<f64>,
    pub live: Option<String>,
    /// The one its sessions share, "mixed" when they differ, "" when unknown.
    pub entrypoint: String,
    /// The share of its API value that ran on the Agent SDK or `claude -p`, 0 to 1.
    #[serde(rename = "agentSdk")]
    pub agent_sdk: f64,
    #[serde(skip)]
    session_ids: HashSet<String>,
}

/// How much of the view ran on the Agent SDK or `claude -p`, which the five-hour
/// meter charges `factor` times more per token than interactive Claude Code.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentSdkUsage {
    pub api_usd: f64,
    pub share: f64,
    pub five_hour: Option<f64>,
    pub factor: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Bucket {
    pub at: i64,
    pub size: String,
}

/// One line of the chart.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Series {
    pub key: String,
    pub label: String,
    pub color: usize,
    pub values: Vec<f64>,
}

/// Limit points no bound request explains.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Unassigned {
    pub account: String,
    pub kind: String,
    pub points: f64,
}

/// How much of the view is named, and since when limits are known.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Coverage {
    pub named_api_usd: f64,
    pub total_api_usd: f64,
    pub limits_since: Option<f64>,
    pub unassigned: Vec<Unassigned>,
}

/// One value of a dimension and how many sessions have it.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Facet {
    pub id: String,
    pub label: String,
    pub count: usize,
}

/// Where the view's data came from and when.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Sources {
    pub devices: Vec<Device>,
    /// The earliest proxy receipt's time; the caller sets it (see `proxy_since`).
    pub proxy_since: Option<f64>,
}

/// The Usage page: totals, the ranked rows, the top-seven series and coverage for one query.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UsageView {
    pub query: Query,
    pub period: Period,
    pub totals: Totals,
    pub previous: Option<Totals>,
    pub buckets: Vec<Bucket>,
    pub series: Vec<Series>,
    pub rows: Vec<Row>,
    pub row_count: usize,
    pub coverage: Coverage,
    pub facets: BTreeMap<String, Vec<Facet>>,
    pub sources: Sources,
    pub agent_sdk: AgentSdkUsage,
}

/// What people call accounts and devices, by id.
#[derive(Debug, Clone, Default)]
pub struct Labels {
    pub accounts: std::collections::HashMap<String, String>,
    pub devices: std::collections::HashMap<String, String>,
}

/// One session's share of one model on one account. Parts of a session sum back to it exactly.
#[derive(Debug, Clone)]
pub struct Part<'a> {
    session: &'a Session,
    who: Who,
    agent: String,
    model: String,
    account: String,
    amount: Amount,
    five_hour: f64,
    week: f64,
    /// Multipliers from the session's series columns [tokens, api, plan, fiveHour, week] to this part's.
    scale: [f64; 5],
}

fn ratio(part: f64, whole: f64) -> f64 {
    if whole == 0.0 { 0.0 } else { part / whole }
}

/// Field by field, the part of `whole` that is both `a` and `b`, assuming they overlap in proportion.
fn overlap_of(a: &Amount, b: &Amount, whole: &Amount) -> Amount {
    let (x, y, w) = (a.fields(), b.fields(), whole.fields());
    Amount::from_fields(std::array::from_fn(|i| {
        if w[i] != 0.0 { x[i] * y[i] / w[i] } else { 0.0 }
    }))
}

/// Splits a session into model × account parts. The amounts per model and per account
/// are exact; a part is their overlap. Limit points belong to their account and split
/// across models by API $.
pub fn parts_of<'a>(s: &'a Session, names: &Names) -> Vec<Part<'a>> {
    let who = who_of(
        names,
        SessionRef {
            session_id: &s.session_id,
            title: s.title.as_deref(),
            project: &s.project,
            repository: s.repository.as_deref(),
        },
    );
    let agent = row_of(&who, &s.key);
    let amount = s.amount;
    let only = |key: &str| Amounts::from_iter([(key.to_string(), amount)]);
    let models = if s.models.is_empty() {
        only("unknown")
    } else {
        s.models.clone()
    };
    let amounts = if s.accounts.is_empty() {
        only(NO_ACCOUNT)
    } else {
        s.accounts.clone()
    };
    let mut kinds = [0.0; 2];
    let mut charged: HashMap<&str, [f64; 2]> = HashMap::new();
    let mut accounts: Vec<&str> = amounts.keys().map(String::as_str).collect();
    for limit in &s.limits {
        let entry = charged.entry(&limit.account).or_insert_with(|| {
            if !amounts.contains_key(&limit.account) {
                accounts.push(&limit.account);
            }
            [0.0; 2]
        });
        let k = super::limits::kind_index(&limit.kind);
        entry[k] += limit.points;
        kinds[k] += limit.points;
    }
    let mut parts = Vec::with_capacity(models.len() * accounts.len());
    for (model, model_amount) in &models {
        let api = if amount.api_usd != 0.0 {
            ratio(model_amount.api_usd, amount.api_usd)
        } else {
            ratio(model_amount.tokens, amount.tokens)
        };
        for account in &accounts {
            let account_amount = amounts.get(*account).copied().unwrap_or_default();
            let points = charged.get(account).copied().unwrap_or_default();
            parts.push(Part {
                session: s,
                who: who.clone(),
                agent: agent.clone(),
                model: model.clone(),
                account: account.to_string(),
                amount: overlap_of(model_amount, &account_amount, &amount),
                five_hour: points[0] * api,
                week: points[1] * api,
                scale: [
                    ratio(model_amount.tokens, amount.tokens)
                        * ratio(account_amount.tokens, amount.tokens),
                    ratio(model_amount.api_usd, amount.api_usd)
                        * ratio(account_amount.api_usd, amount.api_usd),
                    ratio(model_amount.plan_usd, amount.plan_usd)
                        * ratio(account_amount.plan_usd, amount.plan_usd),
                    ratio(points[0], kinds[0]) * api,
                    ratio(points[1], kinds[1]) * api,
                ],
            });
        }
    }
    parts
}

fn entrypoint_of(s: &Session) -> &str {
    if s.entrypoint.is_empty() {
        "unknown"
    } else {
        &s.entrypoint
    }
}

fn dimension_of<'p>(dimension: &str, p: &'p Part) -> &'p str {
    match dimension {
        "app" => app_of(&p.who),
        "harness" => &p.session.harness,
        "model" => &p.model,
        "project" => &p.session.project,
        "device" => &p.session.device_id,
        "entrypoint" => entrypoint_of(p.session),
        _ => &p.account,
    }
}

fn entrypoint_name(id: &str) -> Option<&'static str> {
    Some(match id {
        "cli" => "Interactive",
        "sdk-cli" => "claude -p (Agent SDK)",
        "sdk-ts" => "Agent SDK, TypeScript",
        "sdk-py" => "Agent SDK, Python",
        "unknown" => "Not recorded",
        _ => return None,
    })
}

/// What people call a dimension's value: account and device labels, "No account" for unbound requests.
fn name_of(dimension: &str, id: &str, labels: &Labels) -> String {
    let found = match dimension {
        "account" if id == NO_ACCOUNT => Some("No account"),
        "account" => labels.accounts.get(id).map(String::as_str),
        "device" => labels.devices.get(id).map(String::as_str),
        "entrypoint" => entrypoint_name(id),
        _ => None,
    };
    found.unwrap_or(id).to_string()
}

fn group_key(by: &str, p: &Part) -> String {
    match by {
        "agent" => p.agent.clone(),
        "session" => format!("s:{}", p.session.key),
        "project" => format!("p:{}", p.session.project),
        "model" => format!("m:{}", p.model),
        "harness" => format!("h:{}", p.session.harness),
        "device" => format!("d:{}", p.session.device_id),
        "entrypoint" => format!("e:{}", entrypoint_of(p.session)),
        _ => format!("a:{}", p.account),
    }
}

fn describe(by: &str, p: &Part, labels: &Labels) -> (String, String, Option<Who>) {
    match by {
        "agent" => {
            let (label, detail) = label_of(&p.who);
            (label, detail, Some(agent_of(&p.who)))
        }
        "session" => {
            let (label, detail) = label_of(&p.who);
            let label = p.session.title.clone().unwrap_or(label);
            if p.who.kind == "action" {
                return (
                    label,
                    format!("{} · {detail}", p.who.repo),
                    Some(p.who.clone()),
                );
            }
            let id = prefix(&p.session.session_id, 8);
            let detail = [basename(&p.session.project), &p.session.harness, id].join(" · ");
            (label, detail, Some(p.who.clone()))
        }
        "project" => {
            let project = &p.session.project;
            let name = basename(project);
            (
                if name.is_empty() { project } else { name }.to_string(),
                project.clone(),
                None,
            )
        }
        _ => (
            name_of(by, dimension_of(by, p), labels),
            String::new(),
            None,
        ),
    }
}

fn series_column(measure: &str) -> usize {
    match measure {
        "tokens" => 1,
        "api" => 2,
        "plan" => 3,
        "fiveHour" => 4,
        _ => 5,
    }
}

#[derive(Default)]
struct Share {
    api_usd: f64,
    five_hour: f64,
    week: f64,
}

struct Group<'a> {
    key: String,
    label: String,
    detail: String,
    who: Option<Who>,
    amount: Amount,
    five_hour: f64,
    week: f64,
    accounts: Vec<String>,
    by_account: HashMap<String, Share>,
    seen: HashSet<*const Session>,
    sessions: Vec<&'a Session>,
    last_active_at: f64,
    series: Vec<f64>,
    entrypoint: String,
    sdk_api: f64,
}

fn group_parts<'p>(
    parts: impl IntoIterator<Item = &'p Part<'p>>,
    by: &str,
    measure: &str,
    buckets: usize,
    labels: &Labels,
    sdk: &AgentSdk,
) -> Vec<Group<'p>> {
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut groups: Vec<Group> = Vec::new();
    let column = series_column(measure);
    for p in parts {
        let key = group_key(by, p);
        let g = match index.get(&key) {
            Some(&i) => &mut groups[i],
            None => {
                let (label, detail, who) = describe(by, p, labels);
                index.insert(key.clone(), groups.len());
                groups.push(Group {
                    key,
                    label,
                    detail,
                    who,
                    amount: Amount::default(),
                    five_hour: 0.0,
                    week: 0.0,
                    accounts: Vec::new(),
                    by_account: HashMap::new(),
                    seen: HashSet::new(),
                    sessions: Vec::new(),
                    last_active_at: 0.0,
                    series: vec![0.0; buckets],
                    entrypoint: String::new(),
                    sdk_api: 0.0,
                });
                groups.last_mut().expect("just pushed")
            }
        };
        g.amount.add(&p.amount);
        let via = &p.session.entrypoint;
        if g.entrypoint.is_empty() || g.entrypoint == *via {
            g.entrypoint = via.clone();
        } else if !via.is_empty() {
            g.entrypoint = "mixed".into();
        }
        if sdk.includes(via) {
            g.sdk_api += p.amount.api_usd;
        }
        g.five_hour += p.five_hour;
        g.week += p.week;
        if g.seen.insert(p.session) {
            g.sessions.push(p.session);
        }
        g.last_active_at = g.last_active_at.max(p.session.last_at);
        if p.amount.tokens != 0.0 || p.five_hour != 0.0 || p.week != 0.0 {
            let share = g.by_account.entry(p.account.clone()).or_insert_with(|| {
                g.accounts.push(p.account.clone());
                Share::default()
            });
            share.api_usd += p.amount.api_usd;
            share.five_hour += p.five_hour;
            share.week += p.week;
        }
        if buckets > 0 {
            for row in &p.session.series {
                g.series[row[0] as usize] += row[column] * p.scale[column - 1];
            }
        }
    }
    groups
}

/// The color slot of each of the unfiltered range's top groups by API value.
fn top_by_api(all: &[Part], by: &str) -> HashMap<String, usize> {
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut api: Vec<(String, f64)> = Vec::new();
    for p in all {
        let key = group_key(by, p);
        match index.get(&key) {
            Some(&i) => api[i].1 += p.amount.api_usd,
            None => {
                index.insert(key.clone(), api.len());
                api.push((key, p.amount.api_usd));
            }
        }
    }
    api.sort_by(|a, b| descending(a.1, b.1));
    api.into_iter()
        .take(COLORS)
        .enumerate()
        .map(|(slot, (key, _))| (key, slot))
        .collect()
}

fn value_of(g: &Group, measure: &str) -> f64 {
    match measure {
        "tokens" => g.amount.tokens,
        "api" => g.amount.api_usd,
        "plan" => g.amount.plan_usd,
        "fiveHour" => g.five_hour,
        _ => g.week,
    }
}

fn descending(a: f64, b: f64) -> std::cmp::Ordering {
    b.partial_cmp(&a).unwrap_or(std::cmp::Ordering::Equal)
}

fn hit_rate(a: &Amount) -> f64 {
    ratio(a.cache_read, a.input + a.cache_read + a.cache_write)
}

/// Builds the Usage page for one query over a range's session usage and its parts.
pub fn usage_view(
    tracked: &SessionUsage,
    all: &[Part],
    labels: &Labels,
    q: &Query,
    sdk: &AgentSdk,
) -> UsageView {
    let buckets = tracked.buckets.len();
    let parts: Vec<&Part> = all
        .iter()
        .filter(|p| {
            q.within.as_ref().is_none_or(|w| p.agent == *w)
                && q.filter.iter().all(|(dimension, values)| {
                    values.iter().any(|v| v == dimension_of(dimension, p))
                })
        })
        .collect();

    let mut color = top_by_api(all, &q.by);

    let mut groups = group_parts(
        parts.iter().copied(),
        &q.by,
        &q.measure,
        buckets,
        labels,
        sdk,
    );
    groups.sort_by(|a, b| {
        descending(value_of(a, &q.measure), value_of(b, &q.measure))
            .then_with(|| descending(a.amount.api_usd, b.amount.api_usd))
    });

    let needle = q.search.to_lowercase();
    let shown: Vec<&Group> = groups
        .iter()
        .filter(|g| {
            needle.is_empty()
                || format!("{}\n{}", g.label, g.detail)
                    .to_lowercase()
                    .contains(&needle)
        })
        .collect();
    let limits = !tracked.windows.is_empty();
    let points = |five: f64, week: f64| {
        if limits {
            Points {
                five_hour: Some(five),
                week: Some(week),
            }
        } else {
            Points::default()
        }
    };

    let mut total = Amount::default();
    let mut total_sessions = HashSet::new();
    let (mut total_five, mut total_week, mut named, mut sdk_api, mut sdk_five) =
        (0.0, 0.0, 0.0, 0.0, 0.0);
    for p in &parts {
        total.add(&p.amount);
        if sdk.includes(&p.session.entrypoint) {
            sdk_api += p.amount.api_usd;
            sdk_five += p.five_hour;
        }
        total_sessions.insert(p.session as *const Session);
        total_five += p.five_hour;
        total_week += p.week;
        if p.who.kind != "project" {
            named += p.amount.api_usd;
        }
    }

    let top = &groups[..groups.len().min(COLORS)];
    let taken: HashSet<usize> = top
        .iter()
        .filter_map(|g| color.get(&g.key).copied())
        .collect();
    let mut free = (0..COLORS).filter(|slot| !taken.contains(slot));
    for g in top {
        if !color.contains_key(&g.key) {
            color.insert(g.key.clone(), free.next().unwrap_or(COLORS));
        }
    }
    let color_of = |key: &str| color.get(key).copied().unwrap_or(COLORS);
    let mut series: Vec<Series> = top
        .iter()
        .map(|g| Series {
            key: g.key.clone(),
            label: g.label.clone(),
            color: color_of(&g.key),
            values: g.series.clone(),
        })
        .collect();
    let rest = &groups[top.len()..];
    if !rest.is_empty() {
        let mut values = vec![0.0; buckets];
        for g in rest {
            values
                .iter_mut()
                .zip(&g.series)
                .for_each(|(sum, v)| *sum += v);
        }
        series.push(Series {
            key: "rest".into(),
            label: "Everything else".into(),
            color: COLORS,
            values,
        });
    }

    let rows = shown
        .iter()
        .take(q.limit)
        .map(|g| {
            let mut accounts = g.accounts.clone();
            accounts.sort_by(|a, b| descending(g.by_account[a].api_usd, g.by_account[b].api_usd));
            let by_account = accounts
                .into_iter()
                .map(|account| {
                    let share = &g.by_account[&account];
                    AccountPoints {
                        points: points(share.five_hour, share.week),
                        account,
                    }
                })
                .collect();
            Row {
                key: g.key.clone(),
                label: g.label.clone(),
                detail: g.detail.clone(),
                who: g.who.clone(),
                color: color_of(&g.key),
                amount: g.amount,
                points: points(g.five_hour, g.week),
                by_account,
                sessions: g.sessions.len(),
                last_active_at: g.last_active_at,
                series: g.series.clone(),
                live: None,
                entrypoint: g.entrypoint.clone(),
                agent_sdk: ratio(g.sdk_api, g.amount.api_usd),
                session_ids: g.sessions.iter().map(|s| s.session_id.clone()).collect(),
            }
        })
        .collect();

    let mut unassigned: Vec<Unassigned> = Vec::new();
    let mut since: Option<f64> = None;
    for w in &tracked.windows {
        match unassigned
            .iter_mut()
            .find(|u| u.account == w.account && u.kind == w.kind)
        {
            Some(u) => u.points += w.unassigned,
            None => unassigned.push(Unassigned {
                account: w.account.clone(),
                kind: w.kind.clone(),
                points: w.unassigned,
            }),
        }
        if since.is_none_or(|s| w.since < s) {
            since = Some(w.since);
        }
    }
    unassigned.retain(|u| u.points > 0.0);

    let mut facets = BTreeMap::new();
    for dimension in DIMENSIONS {
        let mut found: Vec<(Facet, HashSet<*const Session>)> = Vec::new();
        let mut at: HashMap<&str, usize> = HashMap::new();
        for p in all {
            let value = dimension_of(dimension, p);
            let j = *at.entry(value).or_insert_with(|| {
                found.push((
                    Facet {
                        id: value.into(),
                        label: name_of(dimension, value, labels),
                        count: 0,
                    },
                    HashSet::new(),
                ));
                found.len() - 1
            });
            found[j].1.insert(p.session);
        }
        let mut list: Vec<Facet> = found
            .into_iter()
            .map(|(facet, sessions)| Facet {
                count: sessions.len(),
                ..facet
            })
            .collect();
        list.sort_by_key(|facet| std::cmp::Reverse(facet.count));
        list.truncate(100);
        facets.insert(dimension.to_string(), list);
    }

    UsageView {
        query: q.clone(),
        period: tracked.period.clone(),
        totals: Totals {
            amount: total,
            sessions: total_sessions.len(),
            cache_hit_rate: hit_rate(&total),
            points: points(total_five, total_week),
        },
        previous: (q.filter.is_empty() && q.within.is_none()).then(|| Totals {
            amount: tracked.previous.amount,
            sessions: tracked.previous.sessions,
            cache_hit_rate: hit_rate(&tracked.previous.amount),
            points: Points::default(),
        }),
        buckets: tracked
            .buckets
            .iter()
            .map(|&at| Bucket {
                at,
                size: tracked.bucket.clone(),
            })
            .collect(),
        series,
        rows,
        row_count: shown.len(),
        coverage: Coverage {
            named_api_usd: named,
            total_api_usd: total.api_usd,
            limits_since: since,
            unassigned,
        },
        facets,
        sources: Sources {
            devices: tracked.devices.clone(),
            proxy_since: None,
        },
        agent_sdk: AgentSdkUsage {
            api_usd: sdk_api,
            share: ratio(sdk_api, total.api_usd),
            five_hour: limits.then(|| ratio(sdk_five, total_five)),
            factor: tracked.sdk_factor,
        },
    }
}

/// The earliest proxy receipt's time, for `Sources::proxy_since`.
pub fn proxy_since(events: &[crate::event::Event]) -> Option<f64> {
    events
        .iter()
        .filter(|e| e.is_proxy())
        .map(|e| super::timeframe::parse_millis(&e.timestamp))
        .fold(None, |since, at| {
            if since.is_none_or(|s| at < s) {
                Some(at)
            } else {
                since
            }
        })
}

/// The browser's order too: a row with a blocked session reads blocked even while another works.
fn live_strength(state: &str) -> u8 {
    match state {
        "blocked" => 3,
        "working" => 2,
        "idle" => 1,
        _ => 0,
    }
}

/// Lights each row from the live sessions; cheap, so it runs on every request over a cached view.
pub fn with_live(view: &UsageView, live: &[LiveSession]) -> UsageView {
    let mut out = view.clone();
    for row in &mut out.rows {
        row.live = None;
        for s in live
            .iter()
            .filter(|s| row.session_ids.contains(&s.id) || s.row == row.key)
        {
            if row
                .live
                .as_deref()
                .is_none_or(|best| live_strength(&s.state) > live_strength(best))
            {
                row.live = Some(s.state.clone());
            }
        }
    }
    out
}
