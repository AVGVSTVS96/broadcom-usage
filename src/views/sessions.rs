use std::collections::BTreeMap;

use foldhash::{HashMap, HashMapExt, HashSet, HashSetExt};
use std::ops::Deref;

use chrono::TimeZone;
use chrono_tz::Tz;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::attribution::{AgentSdk, LimitCharge, attribute_limits};
use super::limits::{
    AccountLookup, Binding, KINDS, LimitReading, binding_history, sorted_readings,
};
use super::plans::PlanPricing;
use super::timeframe::{
    Range, bucket_starts, locale_compare, parse_millis, provider, usage_timeframe,
};
use crate::event::{Device, Event};

/// The account of requests bound to no account, such as Codex or unpooled sessions.
pub const NO_ACCOUNT: &str = "none";

/// Tokens, requests and dollars summed over requests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Amount {
    pub tokens: f64,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub reasoning: f64,
    pub requests: f64,
    pub api_usd: f64,
    pub plan_usd: f64,
    pub unpriced_tokens: f64,
}

impl Amount {
    fn add_event(&mut self, event: &Prepared, plan_usd: f64) {
        self.tokens += event.tokens;
        self.input += event.input_tokens as f64;
        self.output += event.output_tokens as f64;
        self.cache_read += event.cache_read_tokens as f64;
        self.cache_write += event.cache_write_tokens as f64;
        self.reasoning += event.reasoning_tokens as f64;
        self.requests += event.requests.map_or(1.0, |n| n as f64);
        self.api_usd += event.cost_usd;
        self.plan_usd += plan_usd;
        if !event.cost_known {
            self.unpriced_tokens += event.tokens;
        }
    }

    pub(crate) fn fields(&self) -> [f64; 10] {
        [
            self.tokens,
            self.input,
            self.output,
            self.cache_read,
            self.cache_write,
            self.reasoning,
            self.requests,
            self.api_usd,
            self.plan_usd,
            self.unpriced_tokens,
        ]
    }

    pub(crate) fn from_fields(f: [f64; 10]) -> Amount {
        Amount {
            tokens: f[0],
            input: f[1],
            output: f[2],
            cache_read: f[3],
            cache_write: f[4],
            reasoning: f[5],
            requests: f[6],
            api_usd: f[7],
            plan_usd: f[8],
            unpriced_tokens: f[9],
        }
    }

    pub(crate) fn add(&mut self, other: &Amount) {
        let (mut sum, more) = (self.fields(), other.fields());
        sum.iter_mut().zip(more).for_each(|(a, b)| *a += b);
        *self = Amount::from_fields(sum);
    }
}

/// Amounts by key, in the order keys were first added.
pub type Amounts = IndexMap<String, Amount, foldhash::fast::RandomState>;

/// The limit points one session was charged on one account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionLimit {
    pub account: String,
    pub kind: String,
    pub points: f64,
}

/// One session's usage in a range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub key: String,
    pub device_id: String,
    pub harness: String,
    pub session_id: String,
    pub project: String,
    pub repository: Option<String>,
    pub cwd: String,
    pub title: Option<String>,
    /// How its Claude Code process was started (cli, sdk-cli, sdk-ts, t3-code...); "" when unknown.
    pub entrypoint: String,
    pub first_at: f64,
    pub last_at: f64,
    pub amount: Amount,
    pub models: Amounts,
    /// By the account the session was bound to at each request's time; "none" when unbound.
    pub accounts: Amounts,
    /// Sparse: [bucket index, tokens, API $, plan $, five-hour points, week points].
    pub series: Vec<[f64; 6]>,
    pub limits: Vec<SessionLimit>,
}

/// The points of one account's limit window that fell in the range: assigned to the
/// range's requests, or unassigned when no bound request explains the rise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowUsage {
    pub account: String,
    pub kind: String,
    pub resets_at: f64,
    pub since: f64,
    pub sampled: f64,
    pub assigned: f64,
    pub unassigned: f64,
    pub rises: Vec<[f64; 2]>,
}

/// A range's bounds in unix ms and the zone its days are counted in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Period {
    pub from: i64,
    pub to: i64,
    pub timezone: String,
}

/// The period before the range, summed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Previous {
    #[serde(flatten)]
    pub amount: Amount,
    pub sessions: usize,
}

/// token-tracker's GetSessionUsage answer for one range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUsage {
    pub generated_at: i64,
    pub period: Period,
    pub bucket: String,
    pub buckets: Vec<i64>,
    pub sessions: Vec<Session>,
    pub windows: Vec<WindowUsage>,
    pub previous: Previous,
    pub devices: Vec<Device>,
    /// The five-hour weight Agent SDK requests were charged at.
    pub sdk_factor: f64,
}

/// An event with what every view derives from it.
#[derive(Debug, Clone, Copy)]
pub struct Prepared<'a> {
    pub event: &'a Event,
    pub millis: f64,
    pub provider: &'static str,
    pub tokens: f64,
}

impl Deref for Prepared<'_> {
    type Target = Event;

    fn deref(&self) -> &Event {
        self.event
    }
}

pub fn prepare(events: &[Event]) -> Vec<Prepared<'_>> {
    let mut providers: HashMap<&str, &'static str> = HashMap::new();
    events
        .iter()
        .map(|event| Prepared {
            event,
            millis: parse_millis(&event.timestamp),
            provider: providers
                .entry(&event.model)
                .or_insert_with(|| provider(&event.model)),
            tokens: event.token_total() as f64,
        })
        .collect()
}

fn device_of(event: &Event) -> &str {
    if event.device_id.is_empty() {
        "local"
    } else {
        &event.device_id
    }
}

/// Limit readings, the pool's session bindings and the Agent SDK weighting, ready for attribution.
pub struct LimitInputs {
    pub readings: Vec<LimitReading>,
    pub lookup: AccountLookup,
    pub sdk: AgentSdk,
}

impl LimitInputs {
    pub fn new(readings: &[LimitReading], bindings: &[Binding], sdk: AgentSdk) -> Self {
        LimitInputs {
            readings: sorted_readings(readings),
            lookup: AccountLookup::new(&binding_history(bindings)),
            sdk,
        }
    }

    fn factor(limits: Option<&LimitInputs>) -> f64 {
        limits.map_or(1.0, |l| {
            if l.sdk.factor < 1.0 {
                1.0
            } else {
                l.sdk.factor
            }
        })
    }
}

struct Row {
    session: Session,
    series: BTreeMap<usize, [f64; 6]>,
    limits: HashMap<String, usize>,
    titled_at: f64,
}

/// Every session active in the range, with the same records, deduplication and
/// prices as the totals, so the sessions' sums equal them. `now` is unix ms.
pub fn session_usage(
    events: &[Prepared],
    r: Range,
    zone: Tz,
    now: i64,
    plans: Option<&PlanPricing>,
    limits: Option<&LimitInputs>,
    devices: &[Device],
) -> SessionUsage {
    let at = zone
        .timestamp_millis_opt(now)
        .single()
        .expect("an instant has one wall time");
    let frame = usage_timeframe(r, zone, at);
    let now_millis = now as f64;
    let (start, previous_start, end) = (
        frame.start.timestamp_millis() as f64,
        frame.previous_start.timestamp_millis() as f64,
        frame.end.timestamp_millis() as f64,
    );
    let (size, starts) = bucket_starts(r, &frame);
    let bucket_of = |millis: f64| {
        starts
            .partition_point(|s| *s as f64 <= millis)
            .saturating_sub(1)
    };

    let (windows, charges) = match limits {
        Some(l) => attribute_limits(&l.readings, &l.lookup, events, &l.sdk),
        None => (Vec::new(), Vec::new()),
    };
    let mut assigned_in_range = vec![0.0; windows.len()];
    let mut previous = Amount::default();
    let mut previous_sessions = HashSet::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut rows: Vec<Row> = Vec::new();
    for (i, event) in events.iter().enumerate() {
        let millis = event.millis;
        if millis.is_nan() || millis > now_millis || millis < previous_start || millis >= end {
            continue;
        }
        let device = device_of(event);
        let key = format!("{device}:{}:{}", event.harness, event.session_id);
        let plan_usd = plans.map_or(event.cost_usd, |p| p.price(event));
        if millis < start {
            previous.add_event(event, plan_usd);
            previous_sessions.insert(key);
            continue;
        }
        let at = match index.get(&key) {
            Some(&at) => at,
            None => {
                index.insert(key.clone(), rows.len());
                rows.push(Row {
                    session: Session {
                        key,
                        device_id: device.to_string(),
                        harness: event.harness.clone(),
                        session_id: event.session_id.clone(),
                        project: event
                            .repository
                            .clone()
                            .unwrap_or_else(|| event.project.clone()),
                        repository: event.repository.clone(),
                        cwd: event.project.clone(),
                        title: None,
                        entrypoint: String::new(),
                        first_at: millis,
                        last_at: millis,
                        amount: Amount::default(),
                        models: Amounts::default(),
                        accounts: Amounts::default(),
                        series: Vec::new(),
                        limits: Vec::new(),
                    },
                    series: BTreeMap::new(),
                    limits: HashMap::new(),
                    titled_at: f64::NEG_INFINITY,
                });
                rows.len() - 1
            }
        };
        let row = &mut rows[at];
        let s = &mut row.session;
        s.first_at = s.first_at.min(millis);
        s.last_at = s.last_at.max(millis);
        let title = event.session_title.trim();
        if !title.is_empty() && millis >= row.titled_at {
            s.title = Some(title.to_string());
            row.titled_at = millis;
        }
        if !event.entrypoint.is_empty() {
            s.entrypoint = event.entrypoint.clone();
        }
        s.amount.add_event(event, plan_usd);
        s.models
            .entry(event.model.clone())
            .or_default()
            .add_event(event, plan_usd);
        let account = limits.map_or("", |l| l.lookup.account_of(event));
        let account = if account.is_empty() {
            NO_ACCOUNT
        } else {
            account
        };
        s.accounts
            .entry(account.to_string())
            .or_default()
            .add_event(event, plan_usd);
        let bucket = bucket_of(millis);
        let point = row
            .series
            .entry(bucket)
            .or_insert([bucket as f64, 0.0, 0.0, 0.0, 0.0, 0.0]);
        point[1] += event.tokens;
        point[2] += event.cost_usd;
        point[3] += plan_usd;
        let Some(Some(LimitCharge { points, window })) = charges.get(i) else {
            continue;
        };
        point[4] += points[0];
        point[5] += points[1];
        for (k, kind) in KINDS.iter().enumerate() {
            let Some(w) = window[k] else { continue };
            assigned_in_range[w] += points[k];
            let account = &windows[w].account;
            let at = *row
                .limits
                .entry(format!("{account}:{kind}"))
                .or_insert_with(|| {
                    s.limits.push(SessionLimit {
                        account: account.clone(),
                        kind: kind.to_string(),
                        points: 0.0,
                    });
                    s.limits.len() - 1
                });
            s.limits[at].points += points[k];
        }
    }

    let range_end = end.min(now_millis);
    let mut usages = Vec::new();
    for (w, window) in windows.iter().enumerate() {
        if window.until < start || window.since > range_end {
            continue;
        }
        let rises: Vec<[f64; 2]> = window
            .unassigned
            .iter()
            .filter(|(at, _)| *at >= start && *at <= range_end)
            .map(|(at, points)| [*at, *points])
            .collect();
        let unassigned = rises.iter().fold(0.0, |sum, r| sum + r[1]);
        let assigned = assigned_in_range[w];
        usages.push(WindowUsage {
            account: window.account.clone(),
            kind: KINDS[window.kind].to_string(),
            resets_at: window.resets_at,
            since: window.since,
            sampled: assigned + unassigned,
            assigned,
            unassigned,
            rises,
        });
    }

    rows.sort_by(|a, b| {
        b.session
            .amount
            .api_usd
            .partial_cmp(&a.session.amount.api_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| locale_compare(&a.session.key, &b.session.key))
    });
    let sessions = rows
        .into_iter()
        .map(|row| Session {
            series: row.series.into_values().collect(),
            ..row.session
        })
        .collect();
    SessionUsage {
        generated_at: now,
        period: Period {
            from: frame.start.timestamp_millis(),
            to: frame.end.timestamp_millis(),
            timezone: zone.name().to_string(),
        },
        bucket: size.to_string(),
        buckets: starts,
        sessions,
        windows: usages,
        previous: Previous {
            amount: previous,
            sessions: previous_sessions.len(),
        },
        devices: devices.to_vec(),
        sdk_factor: LimitInputs::factor(limits),
    }
}
