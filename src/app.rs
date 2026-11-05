//! The running service: the store, its inputs, and the views over it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use notify::{RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::event::Device;
use crate::follow::{self, Followed, Format};
use crate::store::Store;
use crate::stream::Stream;

pub struct App {
    pub config: Config,
    pub store: Arc<Store>,
    pub stream: Stream,
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

/// Catches up whenever a followed file changes: once the writes go quiet for a second,
/// or five seconds into a steady stream.
pub async fn follow_files(app: Arc<App>) {
    let dirs: Vec<PathBuf> = app
        .followed()
        .iter()
        .filter_map(|f| f.path.parent().map(|p| p.to_path_buf()))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut watcher = match notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok_and(|e| !matches!(e.kind, notify::EventKind::Access(_))) {
            let _ = tx.send(());
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("watching followed files: {e}");
            return;
        }
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

pub async fn usage(State(_app): State<Arc<App>>, Query(_q): Query<Vec<(String, String)>>) -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, "views not wired yet").into_response()
}

pub async fn sessions(State(_app): State<Arc<App>>) -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, "views not wired yet").into_response()
}

pub async fn credits(State(_app): State<Arc<App>>) -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, "views not wired yet").into_response()
}
