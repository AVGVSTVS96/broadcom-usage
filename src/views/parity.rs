//! Writes this module's answers for a fixture next to the Go deck's, for compare.py.
//! `DECK_PARITY=<fixture> DECK_OUT=<dir> cargo test --release parity -- --ignored --nocapture`;
//! with `DECK_LIVE_CLAUDE` and `DECK_LIVE_HEX` it also writes the live keys.

use std::collections::BTreeMap;

use foldhash::{HashMap, HashMapExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono_tz::Tz;
use serde::Serialize;

use super::*;
use crate::event::{Device, Event};

fn write(dir: &Path, name: &str, value: &impl Serialize) {
    std::fs::write(dir.join(name), serde_json::to_vec(value).unwrap()).unwrap();
}

/// Checks a real answer against its published schema.
fn conforms(schema: &str, value: &impl Serialize) {
    let schema = schemas().into_iter().find(|(n, _)| *n == schema).unwrap().1;
    let value = serde_json::to_value(value).unwrap();
    let errors: Vec<String> = jsonschema::validator_for(&schema.to_value())
        .unwrap()
        .iter_errors(&value)
        .take(5)
        .map(|e| format!("{e} at {}", e.instance_path()))
        .collect();
    assert!(errors.is_empty(), "{errors:#?}");
}

#[test]
#[ignore = "needs DECK_PARITY"]
fn parity() {
    let dir = PathBuf::from(std::env::var("DECK_PARITY").expect("DECK_PARITY"));
    let out = PathBuf::from(std::env::var("DECK_OUT").expect("DECK_OUT"));
    std::fs::create_dir_all(&out).unwrap();
    let state = dir.join("deck/state");
    let now: i64 = std::fs::read_to_string(dir.join("deck/now"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let zone: Tz = "America/Los_Angeles".parse().unwrap();

    let started = Instant::now();
    let events: Vec<Event> = std::fs::read_to_string(dir.join("events.ndjson"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let devices: Vec<Device> =
        serde_json::from_slice(&std::fs::read(dir.join("devices.json")).unwrap()).unwrap();
    eprintln!("load {} events {:?}", events.len(), started.elapsed());

    #[derive(serde::Deserialize)]
    struct Config {
        plans: Vec<ConfiguredPlan>,
    }
    let configured: Config =
        serde_json::from_slice(&std::fs::read(state.join("plans-config.json")).unwrap()).unwrap();
    let plans = read_plans(
        Some(&state.join("accounts.json")),
        Some(&state),
        &configured.plans,
    );
    let readings = AppendLog::new(state.join("usage-history.ndjson"), parse_reading)
        .read()
        .0
        .to_vec();
    let bindings = AppendLog::new(state.join("attribution.ndjson"), parse_binding)
        .read()
        .0
        .to_vec();

    let started = Instant::now();
    let rows: Vec<Row> = events.iter().map(Row::from).collect();
    eprintln!("rows {:?}", started.elapsed());
    let started = Instant::now();
    let prepared = prepare(&rows);
    let pricing = PlanPricing::new(&plans, &prepared, zone, now);
    let limits = LimitInputs::new(&readings, &bindings, AgentSdk::default());
    eprintln!("prepare, plan pricing and bindings {:?}", started.elapsed());

    let mut tracked = HashMap::new();
    for r in Range::ALL {
        let started = Instant::now();
        let usage = session_usage(
            &prepared,
            r,
            zone,
            now,
            Some(&pricing),
            Some(&limits),
            &devices,
        );
        eprintln!(
            "{} {:?}, {} sessions",
            r.as_str(),
            started.elapsed(),
            usage.sessions.len()
        );

        let from = reads_from(r, zone, now, &readings).min(pricing_from(r, zone, now));
        let span: Vec<Row> = rows
            .iter()
            .filter(|row| row.millis.partial_cmp(&from) != Some(std::cmp::Ordering::Less))
            .cloned()
            .collect();
        let started = Instant::now();
        let span_prepared = prepare(&span);
        let span_pricing = PlanPricing::new(&plans, &span_prepared, zone, now);
        let from_span = session_usage(
            &span_prepared,
            r,
            zone,
            now,
            Some(&span_pricing),
            Some(&limits),
            &devices,
        );
        eprintln!(
            "  over its span ({} events) {:?}",
            span.len(),
            started.elapsed()
        );
        assert!(
            from_span == usage,
            "{} differs when read from its span",
            r.as_str()
        );

        conforms("sessions", &usage);
        write(&out, &format!("rust-sessions-{}.json", r.as_str()), &usage);
        tracked.insert(r, usage);
    }

    let hex = dir.join("deck/hex");
    let names = load_names(&hex, &read_past(&hex).unwrap()).unwrap();
    let labels = Labels {
        accounts: account_labels(&state.join("accounts.json"), None),
        devices: tracked[&Range::Week]
            .devices
            .iter()
            .map(|d| (d.id.clone(), d.label.clone()))
            .collect(),
    };
    let queries: Vec<String> =
        serde_json::from_slice(&std::fs::read(dir.join("deck/out/queries.json")).unwrap()).unwrap();
    let started = Instant::now();
    let parts: HashMap<Range, Vec<Part>> = tracked
        .iter()
        .map(|(r, usage)| {
            (
                *r,
                usage
                    .sessions
                    .iter()
                    .flat_map(|s| parts_of(s, &names))
                    .collect(),
            )
        })
        .collect();
    eprintln!("parts {:?}", started.elapsed());
    #[derive(Serialize)]
    struct Answer<'a> {
        query: &'a str,
        view: UsageView,
    }
    let started = Instant::now();
    let answers: Vec<Answer> = queries
        .iter()
        .map(|query| {
            let q = parse_query(query);
            Answer {
                query,
                view: with_live(
                    &usage_view(
                        &tracked[&q.range],
                        &parts[&q.range],
                        &labels,
                        &q,
                        &AgentSdk::default(),
                    ),
                    &[],
                ),
            }
        })
        .collect();
    eprintln!("{} views {:?}", queries.len(), started.elapsed());
    answers.iter().for_each(|a| conforms("usage", &a.view));
    write(&out, "rust-views.json", &answers);

    let started = Instant::now();
    let grants = read_grants(&state.join("credits.json")).unwrap();
    let view = credits(&grants, &events, now).unwrap();
    conforms("credits", &view);
    write(&out, "rust-credits.json", &view);
    eprintln!("credits {:?}", started.elapsed());

    if let (Ok(claude), Ok(hex_dir)) = (
        std::env::var("DECK_LIVE_CLAUDE"),
        std::env::var("DECK_LIVE_HEX"),
    ) {
        let found = read_processes(Path::new(&claude), Path::new(&hex_dir));
        let live = live_map(
            &found,
            &seats_of(&bindings, now),
            Some(&names),
            Some(&tracked[&Range::Week]),
            &mut HashMap::new(),
            now,
        );
        live.sessions
            .iter()
            .for_each(|s| conforms("live-session", s));
        let entries: BTreeMap<String, serde_json::Value> = live.entries().into_iter().collect();
        write(&out, "rust-live.json", &entries);
    }
}
