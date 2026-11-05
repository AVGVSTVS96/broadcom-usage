use std::path::Path;

use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use super::sessions::Prepared;
use super::timeframe::{month_start, next_month, parse_millis};

/// One paid subscription seat on a plan.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Seat {
    pub label: String,
    pub tier: String,
    pub monthly_usd: f64,
    pub since: f64,
}

/// A subscription that covers its providers' usage.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Plan {
    pub name: String,
    pub providers: Vec<String>,
    pub seats: Vec<Seat>,
}

/// A plan from config (`usage.plans`), like a ChatGPT subscription.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfiguredPlan {
    pub name: String,
    pub providers: Vec<String>,
    #[serde(rename = "monthlyUSD", alias = "monthly-usd")]
    pub monthly_usd: f64,
    pub since: String,
}

fn claude_tier(tier: &str) -> Option<(&'static str, f64)> {
    match tier {
        "default_claude_max_20x" => Some(("Max 20x", 200.0)),
        "default_claude_max_5x" => Some(("Max 5x", 100.0)),
        "default_claude_ai" => Some(("Pro", 20.0)),
        _ => None,
    }
}

fn tier_id(name: &str) -> &'static str {
    match name {
        "max 20x" => "default_claude_max_20x",
        "max 5x" => "default_claude_max_5x",
        "pro" => "default_claude_ai",
        _ => "",
    }
}

pub(crate) fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

#[derive(Deserialize, Default)]
pub(crate) struct AccountList {
    #[serde(default)]
    pub accounts: Vec<ListedAccount>,
}

#[derive(Deserialize)]
pub(crate) struct ListedAccount {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub tier: Option<String>,
}

#[derive(Deserialize, Default)]
struct SavedPlan {
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    since: Option<f64>,
}

/// The pooled Claude accounts' plans, from the pool's account list (accounts.json)
/// and the plan each profile reported (`<state>/plans/<id>.json`), then the configured plans.
pub fn read_plans(accounts_file: Option<&Path>, state_dir: Option<&Path>, configured: &[ConfiguredPlan]) -> Vec<Plan> {
    let list: AccountList = accounts_file.and_then(read_json).unwrap_or_default();
    let mut seats = Vec::new();
    for account in list.accounts {
        let saved: SavedPlan = state_dir
            .and_then(|dir| read_json(&dir.join("plans").join(format!("{}.json", account.id))))
            .unwrap_or_default();
        let tier = match (saved.tier, &account.tier) {
            (Some(tier), _) => tier,
            (None, Some(name)) => tier_id(name).to_string(),
            (None, None) => String::new(),
        };
        if let (Some((name, monthly)), Some(since)) = (claude_tier(&tier), saved.since.filter(|s| *s != 0.0)) {
            seats.push(Seat { label: account.email, tier: name.into(), monthly_usd: monthly, since });
        }
    }
    let mut plans = Vec::new();
    if !seats.is_empty() {
        plans.push(Plan { name: "Claude".into(), providers: vec!["anthropic".into()], seats });
    }
    for plan in configured {
        plans.push(Plan {
            name: plan.name.clone(),
            providers: plan.providers.clone(),
            seats: vec![Seat {
                label: plan.name.clone(),
                tier: plan.name.clone(),
                monthly_usd: plan.monthly_usd,
                since: parse_millis(&plan.since),
            }],
        });
    }
    plans
}

/// Go's math.Min and Max, where NaN wins.
fn go_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() { f64::NAN } else { a.min(b) }
}

fn go_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() { f64::NAN } else { a.max(b) }
}

#[derive(Debug, Clone, Copy)]
struct Month {
    start: f64,
    end: f64,
}

fn overlap(start: f64, end: f64, from: f64, to: f64) -> f64 {
    go_max(0.0, go_min(end, to) - go_max(start, from))
}

/// A seat accrues its monthly price evenly across each calendar month it was active.
fn seat_fees(seat: &Seat, m: Month, from: f64, to: f64) -> f64 {
    seat.monthly_usd * overlap(m.start, m.end, go_max(from, seat.since), to) / (m.end - m.start)
}

/// Spreads each plan's monthly fees over the API value its usage had that calendar
/// month, so a request's plan $ is its API $ times that month's rate. It needs every
/// event of each month it prices, and every event after now.
#[derive(Debug, Clone)]
pub struct PlanPricing {
    active: Vec<Plan>,
    covers: Vec<f64>,
    months: Vec<Month>,
    rate: Vec<Vec<f64>>,
    now: f64,
}

impl PlanPricing {
    /// `now` is unix ms.
    pub fn new(plans: &[Plan], events: &[Prepared], zone: Tz, now: i64) -> Self {
        let now = now as f64;
        let mut active = Vec::new();
        let mut covers = Vec::new();
        for plan in plans.iter().filter(|plan| !plan.seats.is_empty()) {
            covers.push(plan.seats.iter().fold(f64::INFINITY, |cover, seat| go_min(cover, seat.since)));
            active.push(plan.clone());
        }
        let from = covers.iter().fold(now, |from, cover| go_min(from, *cover));
        let mut months = Vec::new();
        if !from.is_nan() {
            let mut cursor = month_start(zone, from);
            while (cursor.timestamp_millis() as f64) <= now {
                let next = next_month(cursor);
                months.push(Month { start: cursor.timestamp_millis() as f64, end: next.timestamp_millis() as f64 });
                cursor = next;
            }
        }
        let mut pricing = PlanPricing { active, covers, months, rate: Vec::new(), now };
        let mut api = vec![vec![0.0; pricing.months.len()]; pricing.active.len()];
        for event in events {
            if !event.cost_known || event.cost_usd <= 0.0 {
                continue;
            }
            if let (Some(plan), Some(m)) = (pricing.plan_of(event.provider, event.millis), pricing.month_of(event.millis)) {
                api[plan][m] += event.cost_usd;
            }
        }
        pricing.rate = pricing
            .active
            .iter()
            .zip(&api)
            .map(|(plan, api)| {
                pricing
                    .months
                    .iter()
                    .zip(api)
                    .map(|(m, api)| {
                        let fees: f64 = plan.seats.iter().fold(0.0, |fees, seat| fees + seat_fees(seat, *m, m.start, now));
                        if *api > 0.0 { fees / api } else { 0.0 }
                    })
                    .collect()
            })
            .collect();
        pricing
    }

    fn month_of(&self, millis: f64) -> Option<usize> {
        let (mut low, mut high) = (0isize, self.months.len() as isize - 1);
        while low <= high {
            let i = (low + high) / 2;
            let m = self.months[i as usize];
            if millis < m.start {
                high = i - 1;
            } else if millis >= m.end {
                low = i + 1;
            } else {
                return Some(i as usize);
            }
        }
        if millis >= self.now { self.months.len().checked_sub(1) } else { None }
    }

    fn plan_of(&self, provider: &str, millis: f64) -> Option<usize> {
        (0..self.active.len()).find(|&i| self.active[i].providers.iter().any(|p| p == provider) && millis >= self.covers[i])
    }

    /// A request's plan $.
    pub fn price(&self, event: &Prepared) -> f64 {
        match (self.plan_of(event.provider, event.millis), self.month_of(event.millis)) {
            (Some(plan), Some(m)) => event.cost_usd * self.rate[plan][m],
            _ => event.cost_usd,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;
    use crate::views::sessions::prepare;
    use chrono::TimeZone;

    #[test]
    fn plan_rate_spreads_monthly_fees() {
        let utc: Tz = "UTC".parse().unwrap();
        let now = utc.with_ymd_and_hms(2026, 10, 16, 0, 0, 0).unwrap();
        let since = utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap().timestamp_millis() as f64;
        let plans = [Plan {
            name: "Claude".into(),
            providers: vec!["anthropic".into()],
            seats: vec![Seat { label: String::new(), tier: String::new(), monthly_usd: 310.0, since }],
        }];
        let event = |model: &str, at: &str, cost: f64| Event {
            model: model.into(),
            timestamp: at.into(),
            cost_usd: cost,
            cost_known: true,
            ..Default::default()
        };
        let events = [
            event("claude-opus-4-5", "2026-09-10T00:00:00Z", 600.0),
            event("claude-opus-4-5", "2026-10-10T00:00:00Z", 50.0),
            event("gpt-5", "2026-10-10T00:00:00Z", 70.0),
        ];
        let prepared = prepare(&events);
        let p = PlanPricing::new(&plans, &prepared, utc, now.timestamp_millis());
        assert!((p.price(&prepared[0]) - 310.0).abs() < 1e-9);
        assert!((p.price(&prepared[1]) - 150.0).abs() < 1e-9);
        assert_eq!(p.price(&prepared[2]), 70.0);
    }
}
