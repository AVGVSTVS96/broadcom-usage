use foldhash::{HashMap, HashMapExt, HashSet, HashSetExt};

use super::limits::{AccountLookup, KINDS, LimitReading, sorted_readings};
use super::sessions::Prepared;

/// Readings of one window report its reset a few milliseconds apart.
const SAME_RESET: f64 = 10.0 * 60_000.0;
pub(crate) const PERIODS: [f64; 2] = [5.0 * 3_600_000.0, 7.0 * 86_400_000.0];

/// One account's limit window as its readings saw it.
#[derive(Debug, Clone, PartialEq)]
pub struct LimitWindow {
    pub account: String,
    pub kind: usize,
    pub resets_at: f64,
    pub since: f64,
    pub until: f64,
    pub sampled: f64,
    /// Rises no bound request explains, as (at, points).
    pub unassigned: Vec<(f64, f64)>,
}

/// The points one request was charged per kind, and the window (by index) that charged them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LimitCharge {
    pub points: [f64; 2],
    pub window: [Option<usize>; 2],
}

/// The entrypoints the five-hour meter charges as the Agent SDK or `claude -p`, and how
/// many interactive tokens one of their tokens costs there.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentSdk {
    pub factor: f64,
    pub entrypoints: Vec<String>,
}

impl Default for AgentSdk {
    fn default() -> Self {
        AgentSdk {
            factor: 1.7,
            entrypoints: ["sdk-cli", "sdk-ts", "sdk-py"].map(String::from).to_vec(),
        }
    }
}

impl AgentSdk {
    pub fn includes(&self, entrypoint: &str) -> bool {
        self.entrypoints.iter().any(|e| e == entrypoint)
    }

    /// What one token of this entrypoint costs on the five-hour meter, in interactive tokens.
    pub fn weight(&self, entrypoint: &str) -> f64 {
        if self.includes(entrypoint) {
            self.factor
        } else {
            1.0
        }
    }
}

/// Splits every rise of an account's limit across the requests that caused it,
/// weighted by API cost, and for the five-hour limit also by the Agent SDK factor,
/// since that meter charges Agent SDK tokens `sdk.factor` times more. Per window,
/// charges plus unassigned points equal the sampled rise exactly. Charges are
/// indexed like `events`.
pub fn attribute_limits(
    readings: &[LimitReading],
    lookup: &AccountLookup,
    events: &[Prepared],
    sdk: &AgentSdk,
) -> (Vec<LimitWindow>, Vec<Option<LimitCharge>>) {
    let earliest = readings
        .iter()
        .map(|row| row.at - PERIODS[1])
        .fold(f64::INFINITY, f64::min);
    let mut bound: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, event) in events.iter().enumerate() {
        if event.millis < earliest || event.millis.is_nan() {
            continue;
        }
        let account = lookup.account_of(event);
        if !account.is_empty() {
            bound.entry(account).or_default().push(i);
        }
    }
    for list in bound.values_mut() {
        list.sort_by(|&a, &b| events[a].millis.total_cmp(&events[b].millis));
    }

    let mut charges: Vec<Option<LimitCharge>> = vec![None; events.len()];
    let mut windows: Vec<LimitWindow> = Vec::new();
    let mut charge = |windows: &mut Vec<LimitWindow>, w: usize, from: f64, to: f64, points: f64| {
        let window = &mut windows[w];
        window.sampled += points;
        let empty = Vec::new();
        let list = bound.get(window.account.as_str()).unwrap_or(&empty);
        let kind = window.kind;
        let cost = |e: &Prepared| {
            if kind == 0 {
                e.cost_usd * sdk.weight(&e.entrypoint)
            } else {
                e.cost_usd
            }
        };
        let low = list.partition_point(|&i| events[i].millis <= from);
        let mut end = low;
        let mut weight = 0.0;
        while end < list.len() && events[list[end]].millis <= to {
            weight += cost(&events[list[end]]);
            end += 1;
        }
        if weight <= 0.0 {
            window.unassigned.push((to, points));
            return;
        }
        for &index in &list[low..end] {
            let share = charges[index].get_or_insert_with(LimitCharge::default);
            share.points[kind] += points * cost(&events[index]) / weight;
            share.window[kind] = Some(w);
        }
    };

    let mut accounts: Vec<&str> = Vec::new();
    let mut by_account: HashMap<&str, Vec<LimitReading>> = HashMap::new();
    for row in readings {
        by_account
            .entry(&row.account)
            .or_insert_with(|| {
                accounts.push(&row.account);
                Vec::new()
            })
            .push(row.clone());
    }
    for account in accounts {
        let rows = sorted_readings(&by_account[account]);
        let monitored_since = rows[0].at;
        let manual_resets = manual_resets_of(&rows);
        for (kind, period) in PERIODS.into_iter().enumerate() {
            // Rises are charged over the time since the previous reading of this kind,
            // so no request is charged twice, even when readings return to an earlier reset.
            let mut opened: Vec<(usize, f64)> = Vec::new();
            let mut current: Option<usize> = None;
            let (mut last, mut manual_reset_at) = (0.0, 0.0);
            for row in &rows {
                let Some(value) = row.value(kind) else {
                    continue;
                };
                let Some(reset) = row
                    .resets_at(kind)
                    .or(current.map(|c| windows[opened[c].0].resets_at))
                else {
                    continue;
                };
                if manual_resets.contains(&row.at.to_bits()) {
                    manual_reset_at = row.at;
                }
                current = opened.iter().position(|&(w, _)| {
                    windows[w].since >= manual_reset_at
                        && (reset - windows[w].resets_at).abs() <= SAME_RESET
                });
                let open = match current {
                    Some(open) => {
                        let (w, peak) = opened[open];
                        if value > peak {
                            charge(&mut windows, w, last, row.at, value - peak);
                            opened[open].1 = value;
                        }
                        open
                    }
                    None => {
                        let w = windows.len();
                        windows.push(LimitWindow {
                            account: account.to_string(),
                            kind,
                            resets_at: reset,
                            since: row.at,
                            until: row.at,
                            sampled: 0.0,
                            unassigned: Vec::new(),
                        });
                        opened.push((w, value));
                        let start = reset - period;
                        // A window opened while monitored rose from zero; otherwise its first
                        // reading is a baseline nothing logged here can explain.
                        if value > 0.0
                            && start >= monitored_since
                            && start >= last
                            && start < row.at
                        {
                            charge(&mut windows, w, start, row.at, value);
                        } else if value > 0.0 {
                            windows[w].sampled += value;
                            windows[w].unassigned.push((row.at, value));
                        }
                        opened.len() - 1
                    }
                };
                current = Some(open);
                last = row.at;
                let window = &mut windows[opened[open].0];
                window.until = window.until.max(row.at);
            }
        }
    }
    (windows, charges)
}

/// Manual resets: both counters cleared without their scheduled reset times moving.
/// The following reading must stay below the old week value, so one bad sample
/// cannot reset accounting. Keyed by the reading's time bits.
fn manual_resets_of(rows: &[LimitReading]) -> HashSet<u64> {
    let mut resets = HashSet::new();
    for (i, row) in rows.iter().enumerate().skip(1) {
        if row.week != Some(0.0) || row.five_hour != Some(0.0) {
            continue;
        }
        let before = &rows[i - 1];
        let (Some(before_week), Some(before_five)) = (before.week, before.five_hour) else {
            continue;
        };
        if before_week == 0.0 || before_five == 0.0 {
            continue;
        }
        if let Some(next_week) = rows[i + 1..].iter().find_map(|next| next.week)
            && next_week < before_week
        {
            resets.insert(row.at.to_bits());
        }
    }
    resets
}

/// The earliest request time attribution weighs for rises read at or after `from`:
/// each rise is split over the requests since the previous reading of its kind, or,
/// for an account's first reading of a kind, since the window's start.
pub fn attribution_reach(readings: &[LimitReading], from: f64) -> f64 {
    let mut before: HashMap<(&str, usize), Option<f64>> = HashMap::new();
    for row in readings {
        for kind in 0..KINDS.len() {
            let last = before.entry((row.account.as_str(), kind)).or_default();
            if row.at < from && row.value(kind).is_some() && row.resets_at(kind).is_some() {
                *last = Some(last.map_or(row.at, |at| at.max(row.at)));
            }
        }
    }
    before
        .into_iter()
        .map(|((_, kind), last)| last.unwrap_or(from - PERIODS[kind]))
        .fold(from, f64::min)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;
    use crate::views::Row;
    use crate::views::limits::{Binding, binding_history};
    use crate::views::sessions::prepare;

    fn reading(at: f64, five: f64, five_reset: f64, week: Option<(f64, f64)>) -> LimitReading {
        LimitReading {
            account: "acct".into(),
            at,
            five_hour: Some(five),
            week: week.map(|w| w.0),
            five_hour_resets_at: Some(five_reset),
            week_resets_at: week.map(|w| w.1),
        }
    }

    fn started(sessions: &[&str]) -> AccountLookup {
        let rows: Vec<Binding> = sessions
            .iter()
            .map(|s| Binding {
                at: String::new(),
                session_id: s.to_string(),
                account_id: Some("acct".into()),
                event: "start".into(),
                cause: String::new(),
                millis: 0.0,
            })
            .collect();
        AccountLookup::new(&binding_history(&rows))
    }

    fn claude(session: &str, at: &str, cost: f64, entrypoint: &str) -> Event {
        Event {
            harness: "claude".into(),
            session_id: session.into(),
            timestamp: at.into(),
            cost_usd: cost,
            entrypoint: entrypoint.into(),
            ..Default::default()
        }
    }

    #[test]
    fn limit_rises_split_by_cost() {
        let events = [
            claude("s1", "1970-01-01T00:01:40.000Z", 1.0, ""),
            claude("s1", "1970-01-01T00:02:30.000Z", 3.0, ""),
            Event {
                harness: "codex".into(),
                ..claude("s1", "1970-01-01T00:02:40.000Z", 9.0, "")
            },
        ];
        let readings = [
            reading(50_000.0, 2.0, 1e9, None),
            reading(200_000.0, 10.0, 1e9, None),
            reading(300_000.0, 13.0, 1e9, None),
        ];
        let (windows, charges) = attribute_limits(
            &readings,
            &started(&["s1"]),
            &prepare(&events.map(|e| Row::from(&e))),
            &AgentSdk {
                factor: 1.0,
                ..AgentSdk::default()
            },
        );
        assert_eq!(windows.len(), 1);
        assert_eq!(charges[0].as_ref().unwrap().points[0], 2.0);
        assert_eq!(charges[1].as_ref().unwrap().points[0], 6.0);
        assert!(charges[2].is_none());
        let unassigned: f64 = windows[0].unassigned.iter().map(|r| r.1).sum();
        assert_eq!((unassigned, windows[0].sampled), (5.0, 13.0));
    }

    #[test]
    fn manual_reset_needs_a_lower_week_after() {
        let row = |at: f64, five: f64, week: f64| LimitReading {
            account: String::new(),
            at,
            five_hour: Some(five),
            week: Some(week),
            five_hour_resets_at: None,
            week_resets_at: None,
        };
        let rows = [
            row(1.0, 40.0, 50.0),
            row(2.0, 0.0, 0.0),
            row(3.0, 5.0, 10.0),
            row(4.0, 0.0, 0.0),
            row(5.0, 1.0, 60.0),
        ];
        let resets = manual_resets_of(&rows);
        assert!(resets.contains(&2f64.to_bits()) && !resets.contains(&4f64.to_bits()));
    }

    #[test]
    fn agent_sdk_requests_carry_the_factor_on_the_five_hour_limit_only() {
        let events = [
            claude("sdk", "1970-01-01T00:01:40.000Z", 1.0, "sdk-cli"),
            claude("tui", "1970-01-01T00:01:50.000Z", 1.0, "cli"),
        ];
        let readings = [
            reading(50_000.0, 1.0, 1e9, Some((1.0, 2e9))),
            reading(200_000.0, 4.0, 1e9, Some((3.0, 2e9))),
        ];
        let (_, charges) = attribute_limits(
            &readings,
            &started(&["sdk", "tui"]),
            &prepare(&events.map(|e| Row::from(&e))),
            &AgentSdk {
                factor: 2.0,
                ..AgentSdk::default()
            },
        );
        let (sdk, tui) = (charges[0].as_ref().unwrap(), charges[1].as_ref().unwrap());
        assert_eq!((sdk.points, tui.points), ([2.0, 1.0], [1.0, 1.0]));
    }
}
