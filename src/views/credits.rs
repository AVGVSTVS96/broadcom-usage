use std::path::Path;

use serde::{Deserialize, Serialize};

use super::timeframe::parse_millis;
use crate::event::Event;

/// A grant and its last balance read off Console, which no API key can query.
/// Operators keep these non-secret observations in the pool state folder (credits.json).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ApiCredit {
    pub starts_at: i64,
    pub account: String,
    pub organization: String,
    pub credential_id: String,
    pub remaining: f64,
    pub allowance: f64,
    pub monthly_cap: f64,
    pub observed_at: i64,
    pub expires_at: i64,
    pub renews_at: i64,
}

/// The grant carried forward from its Console reading: `remaining` is that reading
/// less what the proxy priced on the credential since.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreditUsage {
    #[serde(flatten)]
    pub grant: ApiCredit,
    pub recorded: f64,
    pub proxy_usd: f64,
    pub requests: usize,
    pub unpriced: usize,
    pub last_request_at: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct CreditsView {
    pub api: Vec<CreditUsage>,
}

const INVALID: &str = "Credit observations are invalid.";

/// The grants in credits.json; none when the file doesn't exist.
pub fn read_grants(path: &Path) -> Result<Vec<ApiCredit>, String> {
    match std::fs::read(path) {
        Ok(data) => serde_json::from_slice(&data).map_err(|_| INVALID.to_string()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(_) => Err("Credit observations could not be read.".into()),
    }
}

/// Counts only matching proxy receipts during each grant; failed requests are not
/// billed. Needs every proxy event since the earliest grant's `starts_at`. `now` is unix ms.
pub fn credits(grants: &[ApiCredit], events: &[Event], now: i64) -> Result<CreditsView, String> {
    let mut view = CreditsView::default();
    for g in grants {
        if g.account.is_empty()
            || g.organization.is_empty()
            || g.credential_id.is_empty()
            || g.starts_at <= 0
            || g.starts_at > g.observed_at
            || g.observed_at <= 0
            || g.observed_at > now
            || g.expires_at <= g.observed_at
            || g.remaining < 0.0
            || g.allowance <= 0.0
            || g.remaining > g.allowance
            || g.monthly_cap < 0.0
        {
            return Err(INVALID.into());
        }
        let mut item = CreditUsage { grant: g.clone(), recorded: g.remaining, proxy_usd: 0.0, requests: 0, unpriced: 0, last_request_at: None };
        for e in events {
            let at = parse_millis(&e.timestamp);
            if !e.is_proxy() || e.account_id != g.credential_id || at.is_nan() || at < g.starts_at as f64 || at >= g.expires_at as f64 || at > now as f64 {
                continue;
            }
            item.requests += 1;
            if e.cost_known {
                item.proxy_usd += e.cost_usd;
                if at >= g.observed_at as f64 {
                    item.grant.remaining -= e.cost_usd;
                }
            } else if !e.failed {
                item.unpriced += 1;
            }
            if item.last_request_at.is_none_or(|last| at as i64 > last) {
                item.last_request_at = Some(at as i64);
            }
        }
        item.grant.remaining = item.grant.remaining.max(0.0);
        view.api.push(item);
    }
    Ok(view)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_follows_proxy_spend_after_the_console_reading() {
        let hour = 3_600_000;
        let now = 1_800_000_000_000;
        let (start, observed) = (now - 2 * hour, now - hour);
        let grant = ApiCredit {
            account: "a".into(),
            organization: "o".into(),
            credential_id: "key".into(),
            remaining: 150.0,
            allowance: 200.0,
            monthly_cap: 200.0,
            starts_at: start,
            observed_at: observed,
            expires_at: now + hour,
            renews_at: 0,
        };
        let at = |ms: i64| chrono::DateTime::from_timestamp_millis(ms).unwrap().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let proxy = |ms: i64, cost: f64, known: bool, failed: bool| Event {
            source: "proxy".into(),
            account_id: "key".into(),
            timestamp: at(ms),
            cost_usd: cost,
            cost_known: known,
            failed,
            ..Default::default()
        };
        let minute = 60_000;
        let events = [
            proxy(start + minute, 50.0, true, false),
            proxy(observed + minute, 30.0, true, false),
            proxy(observed + 2 * minute, 0.0, false, true),
            proxy(observed + 3 * minute, 0.0, false, false),
        ];
        let got = &credits(&[grant], &events, now).unwrap().api[0];
        assert_eq!(
            (got.grant.remaining, got.recorded, got.proxy_usd, got.requests, got.unpriced),
            (120.0, 150.0, 80.0, 4, 1)
        );
    }
}
