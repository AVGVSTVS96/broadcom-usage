//! The running service: the store, what feeds it, and the deck over it, each following
//! the real change that concerns it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::extract::{RawQuery, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use notify::{RecursiveMode, Watcher};
use serde::Serialize;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::deck::Deck;
use crate::event::{Batch, Device};
use crate::follow::{self, Followed, Format};
use crate::github::GitHub;
use crate::pricing::Pricing;
use crate::store::Store;
use crate::views::Range;

pub struct App {
    pub config: Config,
    pub store: Arc<Store>,
    pub pricing: Arc<Pricing>,
    pub deck: Arc<Deck>,
    pub key: String,
}

impl App {
    pub fn local(&self) -> &Device {
        self.store.local()
    }

    /// Every file this service follows into the store.
    fn followed(&self) -> Vec<Followed> {
        let mut out = Vec::new();
        if let Some(dir) = &self.config.go_usage_dir {
            out.extend(follow::go_journals(dir, self.local()));
        }
        if let Some(path) = &self.config.receipts {
            out.push(Followed { path: path.clone(), device: self.local().clone(), format: Format::Receipts });
        }
        out
    }

    /// Reads what the followed files gained; loads in bulk into an empty store.
    pub fn catch_up(&self) -> Result<usize> {
        let bulk = self.store.is_empty()?;
        let mut read = 0;
        for followed in self.followed() {
            read += follow::catch_up(&self.store, &followed, bulk)?;
        }
        if bulk && read > 0 {
            self.store.resolve_all()?;
        }
        Ok(read)
    }
}

/// Starts everything that keeps the store and the deck current.
pub fn start(app: Arc<App>) {
    let deck = app.deck.clone();
    tokio::spawn({
        let store = app.store.clone();
        app.pricing.clone().run(move |policy| {
            let store = store.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = store.set_policy(policy) {
                    tracing::warn!("repricing: {e:#}");
                }
            });
        })
    });
    if app.config.collect {
        let app = app.clone();
        std::thread::spawn(move || collect(&app));
    }
    if !app.config.github_repos.is_empty() {
        let github = GitHub::new(&app.config.state_dir, app.config.github_repos.clone(), app.pricing.clone());
        let store = app.store.clone();
        tokio::spawn(github.run(move |batch| tokio::task::block_in_place(|| store.apply(&batch, true).map(|_| ()))));
    }
    tokio::spawn({
        let deck = deck.clone();
        let mut revision = app.store.subscribe();
        async move {
            while revision.changed().await.is_ok() {
                tokio::time::sleep(Duration::from_secs(15)).await;
                revision.mark_unchanged();
                deck.usage_changed();
            }
        }
    });
    let pool = app.config.pool_dir.clone();
    spawn_watch(vec![(pool.clone(), false), (pool.join("plans"), false)], Duration::from_secs(1), {
        let deck = deck.clone();
        move || {
            deck.usage_changed();
            deck.publish();
        }
    });
    let claude = &app.config.claude;
    let hex_state = app.config.hex.join("state");
    spawn_watch(
        vec![(claude.join("sessions"), false), (claude.join("jobs"), true), (hex_state, true)],
        Duration::from_millis(250),
        {
            let deck = deck.clone();
            move || {
                deck.refresh_processes();
                deck.publish();
            }
        },
    );
    tokio::spawn(follow_hex(deck.clone(), app.config.hex.join(".hex/bin/hex")));
    tokio::spawn({
        let deck = deck.clone();
        async move {
            let mut minute = tokio::time::interval(Duration::from_secs(60));
            let mut beat = tokio::time::interval(Duration::from_secs(20));
            let mut five = tokio::time::interval(Duration::from_secs(300));
            loop {
                tokio::select! {
                    _ = minute.tick() => { deck.refresh_processes(); deck.publish(); }
                    _ = beat.tick() => deck.stream.heartbeat(),
                    _ = five.tick() => if deck.stream.listeners() > 0 { deck.usage_changed() },
                }
            }
        }
    });
    tokio::spawn(async move {
        let first = app.clone();
        if let Ok(Err(e)) = tokio::task::spawn_blocking(move || first.catch_up()).await {
            tracing::warn!("following files: {e:#}");
        }
        tokio::spawn(follow_files(app));
        let names = deck.clone();
        let _ = tokio::task::spawn_blocking(move || names.refresh_names()).await;
        deck.publish();
        if let Err(e) = deck.warm(vec![Range::Week]).await {
            tracing::warn!("loading 7d: {e:#}");
        }
    });
}

/// Collects this machine's transcripts into the store, then again whenever they change:
/// once writes go quiet for a second, or five seconds into a steady stream.
fn collect(app: &App) {
    let sources = crate::collect::Sources { device_id: app.local().id.clone(), ..Default::default() };
    let mut collector = match crate::collect::Collector::open(sources, &app.config.state_dir.join("collect.db")) {
        Ok(c) => c,
        Err(e) => return tracing::error!("collector: {e:#}"),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut watcher) = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok_and(|e| !matches!(e.kind, notify::EventKind::Access(_))) {
            let _ = tx.send(());
        }
    }) else {
        return;
    };
    for root in collector.roots() {
        if let Err(e) = watcher.watch(&root, RecursiveMode::Recursive) {
            tracing::debug!("watching {}: {e}", root.display());
        }
    }
    loop {
        let policy = app.pricing.policy();
        let device = app.local().clone();
        let result = collector.collect(&policy, |c| {
            for warning in &c.warnings {
                tracing::warn!("collector: {warning}");
            }
            app.store.apply(&Batch { device, events: c.events, deleted_ids: c.retracted_ids }, false).map(|_| ())
        });
        if let Err(e) = result {
            tracing::warn!("collecting: {e:#}");
        }
        if rx.recv().is_err() {
            return;
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while let Ok(()) = rx.recv_timeout(Duration::from_secs(1).min(deadline.saturating_duration_since(std::time::Instant::now()))) {
            if std::time::Instant::now() >= deadline {
                break;
            }
        }
        while rx.try_recv().is_ok() {}
    }
}

/// Calls `changed` once things go quiet for `wait` after a burst of changes under
/// `paths` (each watched alone, or with its direct subfolders).
fn spawn_watch(paths: Vec<(PathBuf, bool)>, wait: Duration, changed: impl Fn() + Send + 'static) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok_and(|e| !matches!(e.kind, notify::EventKind::Access(_))) {
            let _ = tx.send(());
        }
    });
    let Ok(mut watcher) = watcher else { return };
    for (path, children) in &paths {
        let mode = if *children { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive };
        if let Err(e) = watcher.watch(path, mode) {
            tracing::debug!("watching {}: {e}", path.display());
        }
    }
    tokio::spawn(async move {
        let _watcher = watcher;
        while rx.recv().await.is_some() {
            tokio::time::sleep(wait).await;
            while rx.try_recv().is_ok() {}
            changed();
        }
    });
}

/// Follows `hex watch`, which prints a line whenever a thread's name changes.
async fn follow_hex(deck: Arc<Deck>, hex: PathBuf) {
    use tokio::io::AsyncBufReadExt;
    if !hex.exists() {
        return;
    }
    loop {
        let child = tokio::process::Command::new(&hex)
            .arg("watch")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn();
        if let Ok(mut child) = child {
            if let Some(out) = child.stdout.take() {
                let mut lines = tokio::io::BufReader::new(out).lines();
                while let Ok(Some(_)) = lines.next_line().await {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    let names = deck.clone();
                    let _ = tokio::task::spawn_blocking(move || names.refresh_names()).await;
                    deck.publish();
                    deck.stream.stale(&["usage"]);
                }
            }
            let _ = child.wait().await;
        }
        tracing::warn!("hex watch exited; starting it again");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Catches up whenever a followed file changes: once the writes go quiet for a second,
/// or five seconds into a steady stream.
pub async fn follow_files(app: Arc<App>) {
    let dirs: std::collections::BTreeSet<PathBuf> =
        app.followed().iter().filter_map(|f| f.path.parent().map(Path::to_path_buf)).collect();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let Ok(mut watcher) = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok_and(|e| !matches!(e.kind, notify::EventKind::Access(_))) {
            let _ = tx.send(());
        }
    }) else {
        return;
    };
    for dir in &dirs {
        if let Err(e) = watcher.watch(dir, RecursiveMode::NonRecursive) {
            tracing::warn!("watching {}: {e}", dir.display());
        }
    }
    let pass = |app: Arc<App>| async move {
        match tokio::task::spawn_blocking(move || app.catch_up()).await {
            Ok(Ok(n)) if n > 0 => tracing::debug!("followed {n} lines"),
            Ok(Err(e)) => tracing::warn!("following files: {e:#}"),
            _ => {}
        }
    };
    pass(app.clone()).await;
    while rx.recv().await.is_some() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::select! {
                more = rx.recv() => if more.is_none() { return },
                _ = tokio::time::sleep(Duration::from_secs(1)) => break,
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
        pass(app.clone()).await;
    }
}

fn reply<T: Serialize>(result: Result<T, String>) -> Response {
    let headers = [(header::CACHE_CONTROL, "no-store")];
    match result {
        Ok(value) => (headers, axum::Json(value)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, headers, axum::Json(serde_json::json!({"error": e}))).into_response(),
    }
}

pub async fn usage(State(app): State<Arc<App>>, RawQuery(query): RawQuery) -> Response {
    reply(app.deck.usage(query.as_deref().unwrap_or("")).await.map_err(|e| e.to_string()))
}

pub async fn sessions(State(app): State<Arc<App>>, RawQuery(query): RawQuery) -> Response {
    let range = form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .find(|(k, _)| k == "range")
        .map_or(Range::Week, |(_, v)| Range::parse(&v));
    reply(app.deck.sessions(range).await.map_err(|e| e.to_string()))
}

pub async fn credits(State(app): State<Arc<App>>) -> Response {
    reply(app.deck.credits().await.as_ref().clone())
}
