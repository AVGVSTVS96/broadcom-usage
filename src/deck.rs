//! Keeps the views warm: each range's session usage is computed off the request path,
//! rebuilt when usage, limits or plans change while someone looks at it, and every
//! query over it is answered from that and cached.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use chrono_tz::Tz;

use crate::config::Config;
use crate::store::{Store, now_millis};
use crate::stream::Stream;
use crate::views::{
    self, AppendLog, Binding, CreditsView, Labels, LimitInputs, LimitReading, Live, Names, Past,
    PlanPricing, Processes, Range, Row, SessionUsage, UsageView,
};

/// A range older than this reloads behind the request that finds it.
const FRESH_FOR: Duration = Duration::from_secs(5 * 60);
/// Ranges asked for this recently reload when usage changes.
const VIEWED_FOR: Duration = Duration::from_secs(10 * 60);
const MAX_VIEWS: usize = 64;

#[derive(Default)]
struct RangeCache {
    value: Option<Arc<SessionUsage>>,
    at: Option<Instant>,
    viewed: Option<Instant>,
    views: HashMap<String, Arc<UsageView>>,
}

/// The resolved rows in time order, kept current from the store's revisions: a change
/// costs reading the rows it touched, not the history.
#[derive(Default)]
struct Rows {
    rows: Vec<Row>,
    ids: Vec<i64>,
    at: HashMap<i64, usize>,
    rev: Option<i64>,
}

/// Past this many changed rows a refresh reads every row again rather than shifting.
const REREAD_OVER: usize = 2_000;

impl Rows {
    fn sync(&mut self, store: &Store) -> Result<()> {
        let (changes, latest) = store.rows_after(self.rev.unwrap_or(-1))?;
        if self.rev.is_some() && changes.len() > REREAD_OVER {
            *self = Rows::default();
            return self.sync(store);
        }
        self.rev = Some(latest);
        for (id, visible, row) in changes {
            if let Some(&i) = self.at.get(&id) {
                if visible && self.rows[i].millis == row.millis {
                    self.rows[i] = row;
                    continue;
                }
                self.rows.remove(i);
                self.ids.remove(i);
                self.at.remove(&id);
                self.reindex(i);
            }
            if visible {
                let i = self.rows.partition_point(|r| r.millis <= row.millis);
                self.rows.insert(i, row);
                self.ids.insert(i, id);
                self.reindex(i);
            }
        }
        Ok(())
    }

    fn reindex(&mut self, from: usize) {
        for (i, &id) in self.ids.iter().enumerate().skip(from) {
            self.at.insert(id, i);
        }
    }

    /// The rows from `since` (unix ms) on.
    fn since(&self, since: f64) -> &[Row] {
        &self.rows[self.rows.partition_point(|r| r.millis < since)..]
    }
}

#[derive(Default)]
struct State {
    ranges: HashMap<Range, RangeCache>,
    names: Option<Arc<Names>>,
    names_error: Option<String>,
    processes: Processes,
    live: Live,
    first_seen: foldhash::HashMap<String, f64>,
    credits: Option<Arc<Result<CreditsView, String>>>,
}

pub struct Deck {
    config: Config,
    zone: Tz,
    store: Arc<Store>,
    pub stream: Stream,
    state: Mutex<State>,
    rows: Mutex<Rows>,
    readings: Mutex<AppendLog<LimitReading>>,
    bindings: Mutex<AppendLog<Binding>>,
    loader: tokio::sync::Mutex<()>,
    past: Mutex<Option<Past>>,
}

/// The machine's IANA zone, as Go's deck finds it.
pub fn local_zone() -> Tz {
    let name = std::env::var("TZ")
        .ok()
        .map(|z| z.trim_start_matches(':').to_string())
        .or_else(|| {
            std::fs::read_link("/etc/localtime").ok().and_then(|link| {
                link.to_string_lossy()
                    .split_once("zoneinfo/")
                    .map(|(_, z)| z.to_string())
            })
        });
    name.and_then(|z| z.parse().ok()).unwrap_or(Tz::UTC)
}

impl Deck {
    pub fn new(config: Config, store: Arc<Store>) -> Self {
        let zone = config
            .timezone
            .as_deref()
            .and_then(|z| z.parse().ok())
            .unwrap_or_else(local_zone);
        let readings = AppendLog::new(
            config.pool_dir.join("usage-history.ndjson"),
            views::parse_reading,
        );
        let mut ranges: HashMap<Range, RangeCache> = HashMap::new();
        ranges.entry(Range::Week).or_default().viewed = Some(Instant::now());
        let bindings = AppendLog::new(
            config.pool_dir.join("attribution.ndjson"),
            views::parse_binding,
        );
        Self {
            config,
            zone,
            store,
            stream: Stream::default(),
            state: Mutex::new(State {
                ranges,
                ..Default::default()
            }),
            rows: Mutex::default(),
            readings: Mutex::new(readings),
            bindings: Mutex::new(bindings),
            loader: tokio::sync::Mutex::new(()),
            past: Mutex::new(None),
        }
    }

    fn bindings(&self) -> Vec<Binding> {
        self.bindings.lock().unwrap().read().0.to_vec()
    }

    /// Computes these ranges' session usage from one read of the span they need.
    fn load(&self, ranges: &[Range]) -> Result<Vec<SessionUsage>> {
        let now = now_millis();
        let mut readings = self.readings.lock().unwrap();
        let mut bindings = self.bindings.lock().unwrap();
        let (readings, bindings) = (readings.read().0, bindings.read().0);
        let from = ranges
            .iter()
            .map(|&r| {
                views::reads_from(r, self.zone, now, readings)
                    .min(views::pricing_from(r, self.zone, now))
            })
            .fold(f64::INFINITY, f64::min);
        let mut rows = self.rows.lock().unwrap();
        rows.sync(&self.store)?;
        let prepared = views::prepare(rows.since(from));
        let plans = views::read_plans(
            Some(&self.config.accounts_file),
            Some(&self.config.pool_dir),
            &self.config.plans,
        );
        let pricing = PlanPricing::new(&plans, &prepared, self.zone, now);
        let limits = LimitInputs::new(readings, bindings, self.config.agent_sdk());
        let devices = self.store.devices()?;
        Ok(ranges
            .iter()
            .map(|&r| {
                views::session_usage(
                    &prepared,
                    r,
                    self.zone,
                    now,
                    Some(&pricing),
                    Some(&limits),
                    &devices,
                )
            })
            .collect())
    }

    /// Reloads ranges, one load at a time; listeners hear only when a range's sessions
    /// or limit windows changed, since every load moves the clock fields.
    pub async fn warm(self: &Arc<Self>, ranges: Vec<Range>) -> Result<()> {
        let started = Instant::now();
        let _loading = self.loader.lock().await;
        let ranges: Vec<Range> = {
            let state = self.state.lock().unwrap();
            ranges
                .into_iter()
                .filter(|r| {
                    state
                        .ranges
                        .get(r)
                        .and_then(|c| c.at)
                        .is_none_or(|at| at < started)
                })
                .collect()
        };
        if ranges.is_empty() {
            return Ok(());
        }
        let deck = self.clone();
        let loading = ranges.clone();
        let values = tokio::task::spawn_blocking(move || deck.load(&loading)).await??;
        let mut stale = false;
        for (r, value) in ranges.into_iter().zip(values) {
            let changed = {
                let mut state = self.state.lock().unwrap();
                let cache = state.ranges.entry(r).or_default();
                let changed = cache.value.as_ref().is_none_or(|old| {
                    old.sessions != value.sessions || old.windows != value.windows
                });
                cache.value = Some(Arc::new(value));
                cache.at = Some(Instant::now());
                if changed {
                    cache.views.clear();
                }
                changed
            };
            if changed && matches!(r, Range::Week | Range::Today) {
                self.publish();
            }
            stale |= changed;
        }
        if stale {
            self.stream.stale(&["usage"]);
        }
        Ok(())
    }

    /// A range's session usage: a request never waits on a loaded range, and one older
    /// than FRESH_FOR reloads behind it.
    pub async fn sessions(self: &Arc<Self>, r: Range) -> Result<Arc<SessionUsage>> {
        loop {
            let (value, at) = {
                let mut state = self.state.lock().unwrap();
                let cache = state.ranges.entry(r).or_default();
                cache.viewed = Some(Instant::now());
                (cache.value.clone(), cache.at)
            };
            if let Some(value) = value {
                if at.is_none_or(|at| at.elapsed() > FRESH_FOR) {
                    let deck = self.clone();
                    tokio::spawn(async move { deck.warm(vec![r]).await });
                }
                return Ok(value);
            }
            self.warm(vec![r]).await?;
        }
    }

    fn labels(&self, tracked: &SessionUsage) -> Labels {
        Labels {
            accounts: views::account_labels(&self.config.accounts_file),
            devices: tracked
                .devices
                .iter()
                .map(|d| (d.id.clone(), d.label.clone()))
                .collect(),
        }
    }

    /// Answers the Usage page's query.
    pub async fn usage(self: &Arc<Self>, raw_query: &str) -> Result<UsageView> {
        let q = views::parse_query(raw_query);
        let range = q.range;
        let tracked = self.sessions(q.range).await?;
        let names = {
            let state = self.state.lock().unwrap();
            match (&state.names, &state.names_error) {
                (Some(names), _) => names.clone(),
                (None, Some(e)) => return Err(anyhow!(e.clone())),
                (None, None) => return Err(anyhow!("hex's thread names haven't loaded yet.")),
            }
        };
        let key = serde_json::to_string(&q)?;
        let cached = self
            .state
            .lock()
            .unwrap()
            .ranges
            .get(&q.range)
            .and_then(|c| c.views.get(&key).cloned());
        let view = match cached {
            Some(view) => view,
            None => {
                let week = self
                    .state
                    .lock()
                    .unwrap()
                    .ranges
                    .get(&Range::Week)
                    .and_then(|c| c.value.clone());
                let labels = self.labels(week.as_deref().unwrap_or(&tracked));
                let deck = self.clone();
                let tracked = tracked.clone();
                let view = tokio::task::spawn_blocking(move || -> Result<UsageView> {
                    let parts: Vec<_> = tracked
                        .sessions
                        .iter()
                        .flat_map(|s| views::parts_of(s, &names))
                        .collect();
                    let mut view =
                        views::usage_view(&tracked, &parts, &labels, &q, &deck.config.agent_sdk());
                    view.sources.proxy_since = deck.store.proxy_since()?.map(|ms| ms as f64);
                    Ok(view)
                })
                .await??;
                let view = Arc::new(view);
                let mut state = self.state.lock().unwrap();
                if let Some(cache) = state.ranges.get_mut(&range) {
                    if cache.views.len() > MAX_VIEWS {
                        cache.views.clear();
                    }
                    cache.views.insert(key, view.clone());
                }
                view
            }
        };
        let live = self.state.lock().unwrap().live.sessions.clone();
        Ok(views::with_live(&view, &live))
    }

    /// The API-credit grants, carried forward by the proxy's receipts since their reading.
    pub async fn credits(self: &Arc<Self>) -> Arc<Result<CreditsView, String>> {
        if let Some(view) = self.state.lock().unwrap().credits.clone() {
            return view;
        }
        self.refresh_credits().await
    }

    pub async fn refresh_credits(self: &Arc<Self>) -> Arc<Result<CreditsView, String>> {
        let deck = self.clone();
        let view = tokio::task::spawn_blocking(move || {
            let path = deck.config.pool_dir.join("credits.json");
            let grants = match views::read_grants(&path) {
                Ok(grants) => grants,
                Err(e) => return Err(e),
            };
            let credentials: Vec<String> = grants.iter().map(|g| g.credential_id.clone()).collect();
            let receipts = deck
                .store
                .receipts(&credentials)
                .map_err(|e| e.to_string())?;
            views::credits(&grants, &receipts, now_millis())
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
        let view = Arc::new(view);
        let previous = self.state.lock().unwrap().credits.replace(view.clone());
        if previous.is_none_or(|p| p != view) {
            self.stream.stale(&["credits"]);
        }
        view
    }

    /// Reloads the ranges in view after the store, limits or plans change.
    pub fn usage_changed(self: &Arc<Self>) {
        let listening = self.stream.listeners() > 0;
        let due: Vec<Range> = {
            let mut state = self.state.lock().unwrap();
            state.credits = None;
            state
                .ranges
                .iter()
                .filter(|(r, c)| {
                    c.value.is_some()
                        && (c.viewed.is_some_and(|v| v.elapsed() < VIEWED_FOR)
                            || (**r == Range::Week && listening))
                })
                .map(|(r, _)| *r)
                .collect()
        };
        if listening {
            let deck = self.clone();
            tokio::spawn(async move { deck.refresh_credits().await });
        }
        if !due.is_empty() {
            let deck = self.clone();
            tokio::spawn(async move {
                if let Err(e) = deck.warm(due).await {
                    tracing::warn!("loading usage: {e:#}");
                }
            });
        }
    }

    /// Re-reads hex's thread names; past thread names are read once.
    pub fn refresh_names(&self) {
        let hex = &self.config.hex;
        let mut past = self.past.lock().unwrap();
        if past.is_none() {
            match views::read_past(hex) {
                Ok(p) => *past = Some(p),
                Err(e) => tracing::warn!("hex history: {e}"),
            }
        }
        let empty = Past::default();
        let result = views::load_names(hex, past.as_ref().unwrap_or(&empty));
        let mut state = self.state.lock().unwrap();
        match result {
            Ok(names) => {
                state.names = Some(Arc::new(names));
                state.names_error = None;
                for cache in state.ranges.values_mut() {
                    cache.views.clear();
                }
            }
            Err(e) => {
                tracing::warn!("hex names: {e}");
                if state.names.is_none() {
                    state.names_error = Some(e);
                }
            }
        }
    }

    pub fn refresh_processes(&self) {
        let found = views::read_processes(&self.config.claude, &self.config.hex);
        self.state.lock().unwrap().processes = found;
    }

    /// Sends the live keys that changed.
    pub fn publish(&self) {
        let seats = views::seats_of(&self.bindings(), now_millis());
        let entries = {
            let mut state = self.state.lock().unwrap();
            let tracked = [Range::Week, Range::Today]
                .iter()
                .find_map(|r| state.ranges.get(r).and_then(|c| c.value.clone()));
            let names = state.names.clone();
            let State {
                processes,
                first_seen,
                ..
            } = &mut *state;
            let live = views::live_map(
                processes,
                &seats,
                names.as_deref(),
                tracked.as_deref(),
                first_seen,
                now_millis(),
            );
            let entries = live.entries();
            state.live = live;
            entries
        };
        self.stream.publish(
            entries
                .into_iter()
                .map(|(k, v)| (k, serde_json::value::to_raw_value(&v).expect("plain data")))
                .collect(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Batch, Device, Event};
    use crate::pricing::Policy;

    fn record(id: &str, at: &str, output: i64) -> Event {
        Event {
            id: id.into(),
            timestamp: at.into(),
            harness: "claude".into(),
            model: "claude-opus-4-5".into(),
            session_id: "s".into(),
            output_tokens: output,
            ..Default::default()
        }
    }

    fn apply(store: &Store, device: &str, events: Vec<Event>, deleted: &[&str]) {
        let device = Device {
            id: device.into(),
            label: device.into(),
            synced_at: None,
        };
        let deleted_ids = deleted.iter().map(|s| s.to_string()).collect();
        store
            .apply(
                &Batch {
                    device,
                    events,
                    deleted_ids,
                },
                false,
            )
            .unwrap();
    }

    #[test]
    fn rows_follow_the_store_in_time_order() {
        let dir = std::env::temp_dir().join(format!("broadcom-usage-rows-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let local = Device {
            id: "server".into(),
            label: "server".into(),
            synced_at: None,
        };
        let store = Store::open(&dir.join("usage.db"), local, Arc::new(Policy::bundled())).unwrap();
        let mut rows = Rows::default();
        let check = |rows: &mut Rows, want: &[i64]| {
            rows.sync(&store).unwrap();
            let fresh = {
                let mut r = Rows::default();
                r.sync(&store).unwrap();
                r
            };
            let outputs = |r: &Rows| r.rows.iter().map(|r| r.output_tokens).collect::<Vec<_>>();
            assert_eq!(outputs(rows), want);
            assert_eq!(
                outputs(rows),
                outputs(&fresh),
                "an incremental refresh equals a full read"
            );
            assert!(rows.ids.iter().enumerate().all(|(i, id)| rows.at[id] == i));
        };
        apply(
            &store,
            "mac",
            vec![
                record("a", "2026-11-01T00:00:02Z", 2),
                record("b", "2026-11-01T00:00:04Z", 4),
            ],
            &[],
        );
        check(&mut rows, &[2, 4]);
        apply(
            &store,
            "mac",
            vec![
                record("c", "2026-11-01T00:00:03Z", 3),
                record("a", "2026-11-01T00:00:02Z", 20),
            ],
            &[],
        );
        check(&mut rows, &[20, 3, 4]);
        apply(
            &store,
            "mac",
            vec![record("b", "2026-11-01T00:00:01Z", 4)],
            &[],
        );
        check(&mut rows, &[4, 20, 3]);
        apply(&store, "mac", vec![], &["c"]);
        check(&mut rows, &[4, 20]);
        let receipt = Event {
            source: "proxy".into(),
            upstream_request_id: "r".into(),
            ..record("p", "2026-11-01T00:00:05Z", 9)
        };
        apply(
            &store,
            "server",
            vec![
                receipt,
                record("claude:message:m:r", "2026-11-01T00:00:06Z", 9),
            ],
            &[],
        );
        check(&mut rows, &[4, 20, 9]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
