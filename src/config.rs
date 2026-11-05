//! The service's settings: one JSON file, every field optional.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::views::{AgentSdk, ConfiguredPlan};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    pub listen: String,
    /// The usage database, the collector's parse cache and pricing-state.json.
    pub state_dir: PathBuf,
    /// Holds the management key clients send as a bearer token.
    pub key_file: PathBuf,
    /// This machine in the store; keep the id the Go proxy used so its history stays one device.
    pub device_id: Option<String>,
    pub device_label: Option<String>,
    /// The account pool's state: usage-history.ndjson, attribution.ndjson, plans/, credits.json.
    pub pool_dir: PathBuf,
    pub accounts_file: PathBuf,
    pub timezone: Option<String>,
    /// Subscriptions the pool can't read from an account profile, like ChatGPT Pro for Codex.
    pub plans: Vec<ConfiguredPlan>,
    /// How much more the five-hour meter charges these entrypoints per token than interactive use.
    pub agent_sdk_factor: f64,
    pub agent_sdk_entrypoints: Vec<String>,
    pub hex: PathBuf,
    pub claude: PathBuf,
    /// The Go proxy's usage folder, whose journals are followed read-only: its history
    /// loads once into an empty store, and anything it still writes follows.
    pub go_usage_dir: Option<PathBuf>,
    /// Collect this machine's transcripts here (off while the Go proxy still collects them).
    pub collect: bool,
    /// Per-request receipts a proxy appends, one record per line.
    pub receipts: Option<PathBuf>,
    /// Repositories whose GitHub Actions upload claude-usage artifacts (autofork's capture step).
    pub github_repos: Vec<String>,
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

pub fn expand(path: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => home().join(rest),
        Err(_) => path.to_path_buf(),
    }
}

impl Default for Config {
    fn default() -> Self {
        let home = home();
        Self {
            listen: "127.0.0.1:8320".into(),
            state_dir: home.join(".local/state/broadcom-usage"),
            key_file: home.join(".config/cliproxyapi/key"),
            device_id: None,
            device_label: None,
            pool_dir: home.join(".local/state/cliproxyapi"),
            accounts_file: home.join(".config/cliproxyapi/accounts.json"),
            timezone: None,
            plans: Vec::new(),
            agent_sdk_factor: AgentSdk::default().factor,
            agent_sdk_entrypoints: AgentSdk::default().entrypoints,
            hex: home.join("hex"),
            claude: home.join(".claude"),
            go_usage_dir: None,
            collect: true,
            receipts: None,
            github_repos: Vec::new(),
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else { return Ok(Self::default()) };
        let text = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let mut config: Self = serde_json::from_slice(&text).with_context(|| format!("parsing {}", path.display()))?;
        for p in [
            &mut config.state_dir,
            &mut config.key_file,
            &mut config.pool_dir,
            &mut config.accounts_file,
            &mut config.hex,
            &mut config.claude,
        ] {
            *p = expand(p);
        }
        for p in [&mut config.go_usage_dir, &mut config.receipts].into_iter().flatten() {
            *p = expand(p);
        }
        Ok(config)
    }

    pub fn agent_sdk(&self) -> AgentSdk {
        AgentSdk { factor: self.agent_sdk_factor, entrypoints: self.agent_sdk_entrypoints.clone() }
    }

    /// This machine's device id: configured, else saved in the state folder once.
    pub fn device_id(&self) -> Result<String> {
        if let Some(id) = self.device_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            return Ok(id.to_string());
        }
        let path = self.state_dir.join("device-id");
        if let Ok(id) = std::fs::read_to_string(&path) {
            if !id.trim().is_empty() {
                return Ok(id.trim().to_string());
            }
        }
        let id = format!("{:032x}", rand_u128());
        std::fs::write(&path, format!("{id}\n"))?;
        Ok(id)
    }

    pub fn device_label(&self) -> String {
        self.device_label.clone().unwrap_or_else(|| {
            std::fs::read_to_string("/etc/hostname").map(|h| h.trim().to_string()).unwrap_or_else(|_| "server".into())
        })
    }
}

fn rand_u128() -> u128 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    let a = h.finish() as u128;
    let mut h2 = std::collections::hash_map::RandomState::new().build_hasher();
    h2.write_u64(a as u64);
    (a << 64) | h2.finish() as u128
}
