use broadcom_usage::event::{Batch, Device, Event};
use broadcom_usage::pricing::Pricing;
use broadcom_usage::store::Store;

fn device(id: &str) -> Device {
    Device {
        id: id.into(),
        label: id.into(),
        synced_at: None,
    }
}

fn open(dir: &std::path::Path) -> Store {
    let pricing = Pricing::open(dir, None);
    Store::open(&dir.join("usage.db"), device("server"), pricing.policy()).unwrap()
}

fn record(id: &str, output: i64) -> Event {
    Event {
        id: id.into(),
        timestamp: "2026-11-03T00:00:00Z".into(),
        harness: "claude".into(),
        model: "claude-opus-4-5-20251101".into(),
        project: "/work/project".into(),
        session_id: "session".into(),
        input_tokens: 100,
        output_tokens: output,
        service_tier: Some("standard".into()),
        cache_write_1h_tokens: Some(0),
        ..Default::default()
    }
}

fn batch(device_id: &str, events: Vec<Event>, deleted: &[&str]) -> Batch {
    Batch {
        device: device(device_id),
        events,
        deleted_ids: deleted.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn applies_persists_and_keeps_metadata() {
    let dir = tempdir();
    let store = open(&dir);
    let mut titled = record("a", 10);
    titled.session_title = "title".into();
    titled.t3_thread_id = "t1".into();
    titled.t3_thread_url = "https://example.com/t1".into();
    let result = store
        .apply(&batch("mac", vec![titled, record("b", 5)], &[]), false)
        .unwrap();
    assert_eq!(result.accepted, 2);
    let result = store
        .apply(
            &batch("mac", vec![record("a", 20), record("b", 5)], &["missing"]),
            false,
        )
        .unwrap();
    assert_eq!(
        (result.accepted, result.updated, result.deleted),
        (0, 1, 0),
        "a correction replaces, an unchanged record is skipped"
    );
    store.apply(&batch("mac", vec![], &["b"]), false).unwrap();
    let result = store
        .apply(
            &batch("mac", vec![record("a", 1), record("c", 1)], &[]),
            true,
        )
        .unwrap();
    assert_eq!(result.accepted, 1, "an import only fills missing records");
    drop(store);

    let store = open(&dir);
    let records = store.device_records("mac").unwrap();
    assert_eq!(records.len(), 2);
    let a = records.iter().find(|e| e.id == "a").unwrap();
    assert_eq!((a.output_tokens, a.session_title.as_str()), (20, "title"));
    assert!(
        !a.t3_thread_url.is_empty(),
        "metadata a collector couldn't read this time is kept"
    );
    let devices = store.devices().unwrap();
    assert_eq!(
        (devices[0].id.as_str(), devices[1].id.as_str()),
        ("server", "mac")
    );
    assert!(devices[1].synced_at.is_some());
    let events = store.events().unwrap();
    assert_eq!(events.len(), 2);
    assert!(events[0].cost_known && events[0].cost_usd > 0.0);
}

#[test]
fn keeps_one_copy_across_devices() {
    let dir = tempdir();
    let store = open(&dir);
    store
        .apply(&batch("server", vec![record("x", 5)], &[]), false)
        .unwrap();
    store
        .apply(
            &batch("mac", vec![record("x", 5), record("y", 1)], &[]),
            false,
        )
        .unwrap();
    store
        .apply(&batch("zz", vec![record("y", 9)], &[]), false)
        .unwrap();
    let events = store.events().unwrap();
    let owner = |id: &str| {
        events
            .iter()
            .find(|e| e.id == id)
            .unwrap()
            .device_id
            .clone()
    };
    assert_eq!(events.len(), 2);
    assert_eq!(owner("x"), "server", "a tie goes to the server's own copy");
    assert_eq!(owner("y"), "zz", "the larger copy wins");
    store.apply(&batch("zz", vec![], &["y"]), false).unwrap();
    assert_eq!(
        store
            .events()
            .unwrap()
            .iter()
            .find(|e| e.id == "y")
            .unwrap()
            .device_id,
        "mac",
        "withdrawing a copy brings back the next"
    );
}

fn wire() -> Event {
    Event {
        source: "proxy".into(),
        id: "proxy:one".into(),
        response_id: "msg_1".into(),
        upstream_request_id: "req_1".into(),
        account_id: "actual-account".into(),
        timestamp: "2026-11-03T00:00:00Z".into(),
        harness: "claude".into(),
        model: "claude-opus-4-5-20251101".into(),
        session_id: "session".into(),
        project: "microscope".into(),
        entrypoint: "microscope".into(),
        input_tokens: 100,
        output_tokens: 20,
        ..Default::default()
    }
}

#[test]
fn proxy_receipt_takes_its_transcripts_metadata() {
    let dir = tempdir();
    let store = open(&dir);
    store
        .apply(&batch("server", vec![wire()], &[]), false)
        .unwrap();
    let got = store.events().unwrap();
    assert_eq!(
        (got.len(), got[0].project.as_str()),
        (1, "microscope"),
        "a receipt stands alone"
    );

    let mut transcript = record("claude:message:msg_1:req_1", 10);
    transcript.timestamp = "2026-11-03T00:00:01Z".into();
    transcript.session_title = "Code review".into();
    let mut historical = transcript.clone();
    historical.id = "claude:message:msg_earlier:req_earlier".into();
    historical.output_tokens = 5;
    historical.session_title = String::new();
    store
        .apply(&batch("mac", vec![transcript, historical], &[]), false)
        .unwrap();
    let check = |store: &Store| {
        let got = store.events().unwrap();
        assert_eq!(
            got.len(),
            2,
            "matched response counted once, earlier history kept: {got:?}"
        );
        let receipt = got
            .iter()
            .find(|e| e.is_proxy())
            .expect("wire accounting kept");
        assert_eq!(receipt.output_tokens, 20);
        assert_eq!(receipt.account_id, "actual-account");
        assert_eq!(receipt.project, "/work/project");
        assert_eq!(receipt.session_title, "Code review");
        assert_eq!(receipt.device_id, "mac");
        assert_eq!(receipt.entrypoint, "microscope");
    };
    check(&store);
    drop(store);
    let store = open(&dir);
    check(&store);
    assert_eq!(
        store.device_records("mac").unwrap().len(),
        2,
        "raw evidence kept"
    );

    store
        .apply(&batch("server", vec![], &["proxy:one"]), false)
        .unwrap();
    assert_eq!(
        store.events().unwrap().len(),
        2,
        "without its receipt the transcript counts again"
    );
}

#[test]
fn only_exact_request_identity_reconciles() {
    let cases: Vec<(&str, Event, Event, usize)> = vec![
        (
            "header fallback",
            Event {
                source: "proxy".into(),
                harness: "claude".into(),
                model: "opus".into(),
                upstream_request_id: "req_1".into(),
                ..Default::default()
            },
            Event {
                harness: "claude".into(),
                model: "opus".into(),
                id: "claude:message:msg_1:req_1".into(),
                ..Default::default()
            },
            1,
        ),
        (
            "codex native",
            Event {
                source: "proxy".into(),
                harness: "codex".into(),
                response_id: "resp_1".into(),
                ..Default::default()
            },
            Event {
                harness: "codex".into(),
                id: "codex:response:resp_1".into(),
                ..Default::default()
            },
            1,
        ),
        (
            "different response",
            Event {
                source: "proxy".into(),
                harness: "claude".into(),
                response_id: "msg_1".into(),
                ..Default::default()
            },
            Event {
                harness: "claude".into(),
                id: "claude:message:msg_2:req_2".into(),
                ..Default::default()
            },
            2,
        ),
        (
            "no identity",
            Event {
                source: "proxy".into(),
                harness: "claude".into(),
                session_id: "same".into(),
                ..Default::default()
            },
            Event {
                harness: "claude".into(),
                session_id: "same".into(),
                id: "t".into(),
                ..Default::default()
            },
            2,
        ),
        (
            "advisor retained",
            Event {
                source: "proxy".into(),
                harness: "claude".into(),
                response_id: "msg_1".into(),
                ..Default::default()
            },
            Event {
                harness: "claude".into(),
                id: "claude:advisor:hash".into(),
                ..Default::default()
            },
            2,
        ),
    ];
    for (name, mut wire, mut transcript, want) in cases {
        let dir = tempdir();
        let store = open(&dir);
        wire.id = "proxy:w".into();
        wire.timestamp = "2026-11-03T00:00:00Z".into();
        if transcript.id.is_empty() {
            transcript.id = "t".into();
        }
        transcript.timestamp = "2026-11-03T00:00:00Z".into();
        store
            .apply(&batch("server", vec![wire], &[]), false)
            .unwrap();
        store
            .apply(&batch("mac", vec![transcript], &[]), false)
            .unwrap();
        assert_eq!(store.events().unwrap().len(), want, "{name}");
    }
}

#[test]
fn bulk_load_resolves_like_applying() {
    let dir = tempdir();
    let store = open(&dir);
    let mut transcript = record("claude:message:msg_1:req_1", 10);
    transcript.timestamp = "2026-11-03T00:00:01Z".into();
    store.load(&device("server"), vec![wire()]).unwrap();
    store
        .load(
            &device("mac"),
            vec![Event {
                device_id: "mac".into(),
                ..transcript
            }],
        )
        .unwrap();
    store.resolve_all().unwrap();
    let got = store.events().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].project.as_str(), got[0].device_id.as_str()),
        ("/work/project", "mac")
    );
}

fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "broadcom-usage-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
