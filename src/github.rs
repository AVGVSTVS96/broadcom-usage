//! Claude usage from GitHub Actions: autofork's `.github/actions/usage` step uploads each
//! run's accounting as a `claude-usage-*` artifact, and each repository becomes one device.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use regex::Regex;
use reqwest::StatusCode;
use reqwest::header::{ACCEPT, AUTHORIZATION, ETAG, IF_NONE_MATCH, LINK, USER_AGENT};
use serde::{Deserialize, Serialize};

use crate::event::{Batch, Device, Event};
use crate::pricing::{Pricing, write_atomic};

const INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Per repository, the artifact listing a pass read in full (its ETag, so an unchanged
/// listing costs GitHub nothing) and the artifacts already handed over.
#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    etags: HashMap<String, String>,
    #[serde(default)]
    read: HashMap<String, Vec<i64>>,
}

#[derive(Deserialize)]
struct Artifact {
    id: i64,
    name: String,
    #[serde(default)]
    expired: bool,
    #[serde(default)]
    created_at: String,
}

#[derive(Deserialize)]
struct Usage {
    version: i64,
    repository: String,
    #[serde(default)]
    events: Vec<Event>,
}

pub struct GitHub {
    api: String,
    token: Option<String>,
    repos: Vec<String>,
    path: PathBuf,
    client: reqwest::Client,
    pricing: Arc<Pricing>,
    state: State,
}

impl GitHub {
    /// Reads the `owner/name` repositories that run autofork's capture step, keeping its
    /// ETags in `state_dir/github.json`.
    pub fn new(state_dir: &Path, repos: Vec<String>, pricing: Arc<Pricing>) -> GitHub {
        let path = state_dir.join("github.json");
        let state = std::fs::read(&path).ok().and_then(|data| serde_json::from_slice(&data).ok()).unwrap_or_default();
        let client = reqwest::Client::builder().timeout(Duration::from_secs(60)).build().expect("an HTTP client");
        GitHub { api: "https://api.github.com".into(), token: None, repos, path, client, pricing, state }
    }

    /// Reads on start and every five minutes. GitHub only announces finished runs through
    /// webhooks, which would need a public address into this server.
    pub async fn run(mut self, mut deliver: impl FnMut(Batch) -> Result<()>) {
        let mut reported = String::new();
        loop {
            let problem = self.pass(&mut deliver).await.err().map(|err| format!("{err:#}")).unwrap_or_default();
            if problem != reported {
                if problem.is_empty() {
                    tracing::info!("GitHub Actions records are current again");
                } else {
                    tracing::warn!("GitHub Actions records: {problem}");
                }
                reported = problem;
            }
            tokio::time::sleep(INTERVAL).await;
        }
    }

    /// Hands each unread artifact's records to `deliver`, one batch per artifact, oldest first.
    /// An artifact counts as read only once `deliver` accepts it.
    pub async fn pass(&mut self, deliver: &mut impl FnMut(Batch) -> Result<()>) -> Result<()> {
        let token = match &self.token {
            Some(token) => token.clone(),
            None => gh_token().await?,
        };
        let mut failed = Vec::new();
        for repo in self.repos.clone() {
            if let Err(err) = self.read_repo(&token, &repo, deliver).await {
                failed.push(format!("{repo} ({err:#})"));
            }
        }
        write_atomic(&self.path, &serde_json::to_vec(&self.state)?)?;
        if !failed.is_empty() {
            bail!("could not read {}", failed.join(", "));
        }
        Ok(())
    }

    async fn read_repo(&mut self, token: &str, repo: &str, deliver: &mut impl FnMut(Batch) -> Result<()>) -> Result<()> {
        let listing = format!("{}/repos/{repo}/actions/artifacts?per_page=100", self.api);
        let Some((etag, pages)) = self.pages(token, listing, self.state.etags.get(repo).cloned()).await? else {
            return Ok(());
        };
        let mut artifacts = Vec::new();
        for page in pages {
            #[derive(Deserialize)]
            struct Page {
                artifacts: Vec<Artifact>,
            }
            let page: Page = serde_json::from_slice(&page)?;
            artifacts.extend(page.artifacts.into_iter().filter(|a| a.name.starts_with("claude-usage-") && !a.expired));
        }
        artifacts.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        let read = self.state.read.remove(repo).unwrap_or_default();
        let mut kept = Vec::new();
        let mut problem = None;
        for artifact in artifacts {
            if !read.contains(&artifact.id) {
                let copied = self.copy(token, repo, &artifact).await.and_then(|batch| batch.map_or(Ok(()), &mut *deliver));
                if let Err(err) = copied {
                    problem = Some(err);
                    continue;
                }
            }
            kept.push(artifact.id);
        }
        if !kept.is_empty() {
            self.state.read.insert(repo.into(), kept);
        }
        match problem {
            None => {
                self.state.etags.insert(repo.into(), etag);
                Ok(())
            }
            Some(err) => {
                self.state.etags.remove(repo);
                Err(err)
            }
        }
    }

    async fn copy(&self, token: &str, repo: &str, artifact: &Artifact) -> Result<Option<Batch>> {
        let url = format!("{}/repos/{repo}/actions/artifacts/{}/zip", self.api, artifact.id);
        let archive = self.get(token, &url, None).await?.bytes().await?;
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))?;
        let mut json = Vec::new();
        zip.by_name("claude-usage.json")?.take(256 << 20).read_to_end(&mut json)?;
        let usage: Usage = serde_json::from_slice(&json)?;
        if usage.version != 1 || !usage.repository.eq_ignore_ascii_case(repo) {
            bail!("artifact {} is not {repo}'s version 1 usage", artifact.id);
        }
        if usage.events.is_empty() {
            return Ok(None);
        }
        let policy = self.pricing.policy();
        let events = usage
            .events
            .into_iter()
            .map(|mut e| {
                if e.service_tier.is_none() && e.reported_cost_usd.is_none() {
                    e.service_tier = Some("standard".into());
                }
                policy.reprice(&e).unwrap_or(e)
            })
            .collect();
        let synced_at = chrono::DateTime::parse_from_rfc3339(&artifact.created_at).ok().map(|at| at.timestamp_millis());
        let device = Device { id: format!("github:{repo}"), label: format!("GitHub {repo}"), synced_at };
        Ok(Some(Batch { device, events, deleted_ids: Vec::new() }))
    }

    /// Every page of a listing and the first page's ETag, or None when it matches `etag`.
    async fn pages(&self, token: &str, url: String, etag: Option<String>) -> Result<Option<(String, Vec<Vec<u8>>)>> {
        static NEXT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"<([^>]+)>;\s*rel="next""#).unwrap());
        let first = self.get(token, &url, etag.as_deref()).await?;
        if first.status() == StatusCode::NOT_MODIFIED {
            return Ok(None);
        }
        let listed = first.headers().get(ETAG).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        let mut pages = Vec::new();
        let mut response = Some(first);
        while let Some(page) = response.take() {
            let next = page.headers().get(LINK).and_then(|v| v.to_str().ok()).and_then(|link| NEXT.captures(link)).map(|m| m[1].to_string());
            pages.push(page.bytes().await?.to_vec());
            if let Some(next) = next {
                response = Some(self.get(token, &next, None).await?);
            }
        }
        Ok(Some((listed, pages)))
    }

    async fn get(&self, token: &str, url: &str, etag: Option<&str>) -> Result<reqwest::Response> {
        let mut request = self
            .client
            .get(url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(ACCEPT, "application/vnd.github+json")
            .header(USER_AGENT, "broadcom-usage")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(etag) = etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        let response = request.send().await?;
        let status = response.status();
        if status != StatusCode::OK && status != StatusCode::NOT_MODIFIED {
            bail!("GitHub answered {status}");
        }
        Ok(response)
    }
}

async fn gh_token() -> Result<String> {
    let out = tokio::process::Command::new("gh").args(["auth", "token"]).output().await.context("gh auth token")?;
    if !out.status.success() {
        bail!("gh auth token: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Mutex;

    use axum::Router;
    use axum::extract::{Path as UrlPath, State as Shared};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::get;

    use super::*;

    fn usage_zip(events: &[Event]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("claude-usage.json", zip::write::SimpleFileOptions::default()).unwrap();
        let doc = serde_json::json!({ "version": 1, "repository": "me/fork", "events": events });
        zip.write_all(doc.to_string().as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    fn event(id: &str, output: i64) -> Event {
        Event {
            id: id.into(),
            timestamp: "2026-01-01T00:00:00.000Z".into(),
            harness: "claude".into(),
            model: "claude-opus-4-5-20251101".into(),
            input_tokens: 100,
            output_tokens: output,
            ..Event::default()
        }
    }

    type Downloads = Arc<Mutex<HashMap<String, usize>>>;

    async fn listing(headers: HeaderMap) -> impl IntoResponse {
        if headers.get("if-none-match").is_some_and(|v| v == "\"v1\"") {
            return (StatusCode::NOT_MODIFIED, [("etag", "\"v1\"")], String::new());
        }
        let body = r#"{"artifacts":[
            {"id":3,"name":"claude-usage-3-1-sync","created_at":"2026-11-01T10:00:00Z"},
            {"id":2,"name":"claude-usage-2-1-sync","created_at":"2026-10-31T10:00:00Z"},
            {"id":4,"name":"claude-usage-4-1-sync","expired":true},
            {"id":5,"name":"build-output"}]}"#;
        (StatusCode::OK, [("etag", "\"v1\"")], body.to_string())
    }

    async fn archive(Shared(downloads): Shared<Downloads>, UrlPath(id): UrlPath<String>) -> Vec<u8> {
        *downloads.lock().unwrap().entry(id.clone()).or_default() += 1;
        usage_zip(&if id == "3" { vec![event("github:new", 10), event("github:old", 99)] } else { vec![] })
    }

    #[tokio::test]
    async fn copies_new_artifacts_once() {
        let downloads = Downloads::default();
        let app = Router::new()
            .route("/repos/me/fork/actions/artifacts", get(listing))
            .route("/repos/me/fork/actions/artifacts/{id}/zip", get(archive))
            .with_state(downloads.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });

        let dir = std::env::temp_dir().join(format!("broadcom-usage-github-{}", std::process::id()));
        let pricing = Arc::new(Pricing::open(&dir, None));
        let mut reader = GitHub::new(&dir, vec!["me/fork".into()], pricing);
        reader.api = api;
        reader.token = Some("token".into());
        let mut batches = Vec::new();
        for _ in 0..2 {
            reader
                .pass(&mut |batch| {
                    batches.push(batch);
                    Ok(())
                })
                .await
                .unwrap();
        }
        assert_eq!(*downloads.lock().unwrap(), HashMap::from([("2".into(), 1), ("3".into(), 1)]));
        assert_eq!(batches.len(), 1, "only artifact 3 has records, and the second pass reads nothing");
        let batch = &batches[0];
        assert_eq!(batch.device.id, "github:me/fork");
        assert_eq!(batch.device.label, "GitHub me/fork");
        assert!(batch.device.synced_at.is_some());
        let fresh = batch.events.iter().find(|e| e.id == "github:new").unwrap();
        assert!(fresh.service_tier.is_some() && fresh.cost_known && fresh.cost_usd > 0.0, "new record not priced: {fresh:?}");
        assert!(dir.join("github.json").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_refused_batch_is_read_again() {
        let downloads = Downloads::default();
        let app = Router::new()
            .route("/repos/me/fork/actions/artifacts", get(listing))
            .route("/repos/me/fork/actions/artifacts/{id}/zip", get(archive))
            .with_state(downloads.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });

        let dir = std::env::temp_dir().join(format!("broadcom-usage-github-refused-{}", std::process::id()));
        let mut reader = GitHub::new(&dir, vec!["me/fork".into()], Arc::new(Pricing::open(&dir, None)));
        reader.api = api;
        reader.token = Some("token".into());
        assert!(reader.pass(&mut |_| bail!("store is full")).await.is_err());
        let mut accepted = 0;
        reader
            .pass(&mut |_| {
                accepted += 1;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(accepted, 1);
        assert_eq!(downloads.lock().unwrap()["3"], 2);
        assert_eq!(downloads.lock().unwrap()["2"], 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
