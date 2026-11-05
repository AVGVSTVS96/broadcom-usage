//! The usage, sessions and credits views, and the live sessions: pure functions over
//! the store's events (merged, reconciled, repriced, sorted by timestamp then id) and
//! the pool's state files.

mod attribution;
mod credits;
mod identity;
mod limits;
mod live;
mod plans;
mod row;
mod schema;
mod sessions;
mod timeframe;
mod view;

#[cfg(test)]
mod parity;

pub use attribution::{AgentSdk, attribution_reach};
pub use credits::{ApiCredit, CreditUsage, CreditsView, credits, read_grants};
pub use identity::{Names, Past, Who, load_names, read_past};
pub use limits::{AppendLog, Binding, LimitReading, parse_binding, parse_reading};
pub use live::{Live, LiveSession, Processes, live_map, read_processes, seats_of};
pub use plans::{ConfiguredPlan, Plan, PlanPricing, read_plans};
pub use row::Row;
pub use schema::schemas;
pub use sessions::{Amount, LimitInputs, Prepared, Session, SessionUsage, prepare, session_usage};
pub use timeframe::{Range, parse_millis};
pub use view::{
    Labels, Part, Query, UsageView, parse_query, parts_of, proxy_since, usage_view, with_live,
};

use std::path::Path;

use chrono::TimeZone;
use chrono_tz::Tz;

/// The earliest event time (unix ms) `session_usage` reads for a range: its previous
/// period, and every request its limit rises are split over. Events after now count.
pub fn reads_from(r: Range, zone: Tz, now: i64, readings: &[LimitReading]) -> f64 {
    let frame = timeframe::usage_timeframe(
        r,
        zone,
        zone.timestamp_millis_opt(now)
            .single()
            .expect("one wall time"),
    );
    let previous = frame.previous_start.timestamp_millis() as f64;
    previous.min(attribution_reach(
        readings,
        frame.start.timestamp_millis() as f64,
    ))
}

/// The earliest event time (unix ms) `PlanPricing` needs to price a range: the start of
/// the calendar month its previous period begins in, since a month's rate spreads its
/// fees over all of that month's API value.
pub fn pricing_from(r: Range, zone: Tz, now: i64) -> f64 {
    let frame = timeframe::usage_timeframe(
        r,
        zone,
        zone.timestamp_millis_opt(now)
            .single()
            .expect("one wall time"),
    );
    timeframe::month_start(zone, frame.previous_start.timestamp_millis() as f64).timestamp_millis()
        as f64
}

/// Account labels: each account's email (accounts.json), and each API-credit key named
/// for the account its credit belongs to (credits.json).
pub fn account_labels(
    accounts_file: &Path,
    credits_file: Option<&Path>,
) -> std::collections::HashMap<String, String> {
    let mut labels: std::collections::HashMap<String, String> =
        plans::read_json::<plans::AccountList>(accounts_file)
            .unwrap_or_default()
            .accounts
            .into_iter()
            .map(|a| (a.id, a.email))
            .collect();
    for grant in credits_file
        .and_then(|f| credits::read_grants(f).ok())
        .unwrap_or_default()
    {
        if let Some(email) = labels
            .get(&grant.account)
            .filter(|_| !labels.contains_key(&grant.credential_id))
        {
            let label = format!("{email} API credit");
            labels.insert(grant.credential_id, label);
        }
    }
    labels
}

#[cfg(test)]
mod label_tests {
    #[test]
    fn api_credit_keys_take_their_accounts_name() {
        let dir =
            std::env::temp_dir().join(format!("broadcom-usage-labels-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (accounts, credits) = (dir.join("accounts.json"), dir.join("credits.json"));
        std::fs::write(
            &accounts,
            r#"{"accounts":[{"id":"acct","email":"me@example.com","tier":"max 20x"}]}"#,
        )
        .unwrap();
        std::fs::write(
            &credits,
            r#"[{"startsAt":0,"account":"acct","organization":"Org","credentialId":"claude:apikey:abc","remaining":1,"allowance":1,"monthlyCap":1,"observedAt":0,"expiresAt":0,"renewsAt":0}]"#,
        )
        .unwrap();
        let labels = super::account_labels(&accounts, Some(&credits));
        assert_eq!(labels["acct"], "me@example.com");
        assert_eq!(labels["claude:apikey:abc"], "me@example.com API credit");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
