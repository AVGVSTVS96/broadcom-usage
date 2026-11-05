//! The LiteLLM-derived price catalog and saved rules, as an immutable policy.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::event::Event;

const SOURCE: &str =
    "https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json";
const DOWNLOAD: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const FILE: &str = "pricing-state.json";
const REFRESH_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// USD per token, by LiteLLM field name (`input_cost_per_token_priority`, ...).
pub type Rates = HashMap<String, f64>;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub source: String,
    pub models: HashMap<String, Rates>,
}

/// USD per million tokens. A missing cache rate is unavailable; zero is free.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleRates {
    #[serde(default)]
    pub input_per_million: f64,
    #[serde(default)]
    pub output_per_million: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_per_million: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_million: Option<f64>,
    #[serde(
        default,
        rename = "cacheWrite1hPerMillion",
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_write_1h_per_million: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleKind {
    Alias,
    Rates,
    Free,
}

/// A saved override: an alias to `target`, custom `rates` shown as `nickname`, or free.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    #[serde(default)]
    pub model: String,
    pub kind: RuleKind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub nickname: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rates: Option<RuleRates>,
}

enum Ruled {
    Alias(String),
    Free,
    Custom { name: String, rates: Rates },
    Catalog,
}

#[derive(Clone, Copy)]
enum Price<'a> {
    Free,
    Custom(&'a Rates),
    Catalog(&'a Rates),
}

#[derive(Clone, Copy)]
struct Resolved<'a> {
    model: &'a str,
    price: Price<'a>,
}

/// An immutable catalog plus rules; `revision` is token-tracker's content hash of both.
pub struct Policy {
    pub catalog: Catalog,
    pub rules: Vec<Rule>,
    pub revision: String,
    ruled: HashMap<String, Ruled>,
}

/// Bumped whenever how a record is priced changes, so stored prices are worked out again.
const PRICING_RULES: u32 = 2;

impl Policy {
    /// What a stored price was worked out with: this catalog and rules, under this code.
    pub fn priced_with(&self) -> String {
        format!("{}/{PRICING_RULES}", self.revision)
    }
}

static BUNDLED: LazyLock<Catalog> = LazyLock::new(|| {
    serde_json::from_str(include_str!("pricing.json"))
        .expect("the bundled pricing.json is a catalog")
});

const COST_FIELDS: [&str; 5] = [
    "input_cost_per_token",
    "output_cost_per_token",
    "cache_read_input_token_cost",
    "cache_creation_input_token_cost",
    "cache_creation_input_token_cost_above_1hr",
];

impl Policy {
    pub fn new(catalog: Catalog, rules: Vec<Rule>) -> Policy {
        let revision = revision(&catalog, &rules);
        let mut ruled = HashMap::with_capacity(rules.len());
        for rule in &rules {
            ruled
                .entry(rule.model.clone())
                .or_insert_with(|| match (rule.kind, &rule.rates) {
                    (RuleKind::Alias, _) => Ruled::Alias(rule.target.clone()),
                    (RuleKind::Free, _) => Ruled::Free,
                    (RuleKind::Rates, Some(rates)) => Ruled::Custom {
                        name: if rule.nickname.is_empty() {
                            &rule.model
                        } else {
                            &rule.nickname
                        }
                        .clone(),
                        rates: custom_rates(rates),
                    },
                    (RuleKind::Rates, None) => Ruled::Catalog,
                });
        }
        Policy {
            catalog,
            rules,
            revision,
            ruled,
        }
    }

    /// The bundled LiteLLM catalog with no rules.
    pub fn bundled() -> Policy {
        Policy::new(BUNDLED.clone(), Vec::new())
    }

    /// Prices a freshly collected record: (cost, known).
    pub fn estimate_cost(&self, e: &Event, cache_write_1h: i64, tier: &str) -> (f64, bool) {
        if self.uses_reported_grok_cost(e) {
            return reported_grok_cost(e);
        }
        estimate(e, cache_write_1h, tier, self.resolve(raw_model(e)))
    }

    /// The model name records show, after alias rules.
    pub fn display_model(&self, raw: &str) -> String {
        self.resolve(raw).map_or(raw, |r| r.model).to_string()
    }

    /// Applies the policy to a stored record; None when nothing changes. Records without
    /// tier or cache-duration metadata (older collectors) keep the cost their machine reported.
    pub fn reprice(&self, e: &Event) -> Option<Event> {
        let raw = raw_model(e);
        let resolved = self.resolve(raw);
        let model = resolved.map_or(raw, |r| r.model);
        let (cost, known) = if self.uses_reported_grok_cost(e) {
            reported_grok_cost(e)
        } else if has_pricing_metadata(e, resolved)
            || matches!(
                resolved,
                Some(Resolved {
                    price: Price::Free,
                    ..
                })
            )
        {
            let tier = e.service_tier.as_deref().unwrap_or("");
            estimate(e, e.cache_write_1h_tokens.unwrap_or(0), tier, resolved)
        } else {
            (if e.cost_known { e.cost_usd } else { 0.0 }, e.cost_known)
        };
        if e.raw_model == raw && e.model == model && e.cost_usd == cost && e.cost_known == known {
            return None;
        }
        Some(Event {
            raw_model: raw.to_string(),
            model: model.to_string(),
            cost_usd: cost,
            cost_known: known,
            ..e.clone()
        })
    }

    /// Why these rules can't be saved together, if they can't.
    pub fn problem(&self) -> Option<String> {
        let name = |s: &str| !s.is_empty() && s.len() <= 200 && s == s.trim();
        let rate = |r: Option<f64>| r.is_none_or(|r| r >= 0.0 && r.is_finite());
        for rule in &self.rules {
            let ok = name(&rule.model)
                && match rule.kind {
                    RuleKind::Alias => name(&rule.target),
                    RuleKind::Free => true,
                    RuleKind::Rates => rule.rates.as_ref().is_some_and(|r| {
                        (rule.nickname.is_empty() || name(&rule.nickname))
                            && [Some(r.input_per_million), Some(r.output_per_million)]
                                .into_iter()
                                .all(rate)
                            && [
                                r.cache_read_per_million,
                                r.cache_write_per_million,
                                r.cache_write_1h_per_million,
                            ]
                            .into_iter()
                            .all(rate)
                    }),
                };
            if !ok {
                return Some("Enter a valid model name and finite, nonnegative rates.".into());
            }
        }
        if self.ruled.len() != self.rules.len() {
            return Some("Only one pricing rule can be saved for each model.".into());
        }
        for rule in self.rules.iter().filter(|r| r.kind == RuleKind::Alias) {
            let mut seen = Vec::new();
            let mut current = rule.model.as_str();
            while let Some(Ruled::Alias(target)) = self.ruled.get(current) {
                if seen.contains(&current) {
                    return Some(format!(
                        "The alias for {} would create a cycle.",
                        rule.model
                    ));
                }
                seen.push(current);
                current = target;
            }
            if self.resolve(&rule.model).is_none() {
                return Some(format!(
                    "The alias target for {} has no known catalog or custom pricing.",
                    rule.model
                ));
            }
        }
        None
    }

    fn resolve<'a>(&'a self, model: &'a str) -> Option<Resolved<'a>> {
        let mut aliases = Vec::new();
        let mut current = model;
        loop {
            if aliases.contains(&current) {
                return None;
            }
            return match self.ruled.get(current) {
                Some(Ruled::Alias(target)) => {
                    aliases.push(current);
                    current = target;
                    continue;
                }
                Some(Ruled::Free) => Some(Resolved {
                    model: current,
                    price: Price::Free,
                }),
                Some(Ruled::Custom { name, rates }) => Some(Resolved {
                    model: name,
                    price: Price::Custom(rates),
                }),
                None | Some(Ruled::Catalog) => self.catalog_price(current).map(|rates| Resolved {
                    model: current,
                    price: Price::Catalog(rates),
                }),
            };
        }
    }

    fn catalog_price(&self, model: &str) -> Option<&Rates> {
        let models = &self.catalog.models;
        if let Some(rates) = models.get(model) {
            return Some(rates);
        }
        for prefix in ["openai/", "anthropic/", "xai/", "meta/"] {
            if let Some(rates) = models.get(&format!("{prefix}{model}")) {
                return Some(rates);
            }
            if let Some(rates) = model.strip_prefix(prefix).and_then(|bare| models.get(bare)) {
                return Some(rates);
            }
        }
        let b = model.as_bytes();
        let dated = b.len() > 9
            && b[b.len() - 8..].iter().all(u8::is_ascii_digit)
            && b[b.len() - 9] == b'-';
        if model.starts_with("claude-") && dated {
            return models.get(&model[..model.len() - 9]);
        }
        None
    }

    fn uses_reported_grok_cost(&self, e: &Event) -> bool {
        e.harness == "grok" && !self.ruled.contains_key(raw_model(e))
    }
}

pub fn raw_model(e: &Event) -> &str {
    if e.raw_model.is_empty() {
        &e.model
    } else {
        &e.raw_model
    }
}

fn reported_grok_cost(e: &Event) -> (f64, bool) {
    match e.reported_cost_usd {
        Some(cost) if cost > 0.0 && cost.is_finite() => (cost, true),
        _ => (0.0, false),
    }
}

fn has_pricing_metadata(e: &Event, resolved: Option<Resolved>) -> bool {
    let custom = matches!(
        resolved,
        Some(Resolved {
            price: Price::Custom(_),
            ..
        })
    );
    e.unclassified_tokens == 0
        && (custom || e.service_tier.is_some())
        && (e.cache_write_tokens == 0 || e.cache_write_1h_tokens.is_some())
}

fn custom_rates(r: &RuleRates) -> Rates {
    let per_token = [
        Some(r.input_per_million),
        Some(r.output_per_million),
        r.cache_read_per_million,
        r.cache_write_per_million,
        r.cache_write_1h_per_million,
    ];
    COST_FIELDS
        .into_iter()
        .zip(per_token)
        .filter_map(|(field, rate)| Some((field.to_string(), rate? / 1_000_000.0)))
        .collect()
}

fn tier_suffix(tier: &str) -> Option<&'static str> {
    Some(match tier.trim_matches(js_space).to_lowercase().as_str() {
        "" | "default" | "standard" | "auto" => "",
        "priority" | "fast" => "_priority",
        "ultrafast" => "_ultrafast",
        "flex" => "_flex",
        "batch" | "batches" => "_batches",
        _ => return None,
    })
}

fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{85}' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// The five per-token rates for a context size and tier; None where the catalog has none.
fn rates_for(rates: &Rates, context: &str, tier: &str) -> [Option<f64>; 5] {
    COST_FIELDS.map(|field| {
        rates
            .get(&format!("{field}{context}{tier}"))
            .or_else(|| {
                if context.is_empty() {
                    None
                } else {
                    rates.get(&format!("{field}{tier}"))
                }
            })
            .copied()
            .filter(|rate| rate.is_finite() && *rate >= 0.0)
    })
}

fn estimate(e: &Event, cache_write_1h: i64, tier: &str, resolved: Option<Resolved>) -> (f64, bool) {
    let rates = match resolved.map(|r| r.price) {
        None => return (0.0, false),
        Some(Price::Free) => return (0.0, true),
        Some(Price::Custom(rates)) => rates_for(rates, context_suffix(e, rates), ""),
        Some(Price::Catalog(rates)) => match tier_suffix(tier) {
            Some(tier) => rates_for(rates, context_suffix(e, rates), tier),
            None => return (0.0, false),
        },
    };
    let write_1h = cache_write_1h.max(0).min(e.cache_write_tokens);
    let tokens = [
        e.input_tokens,
        e.output_tokens,
        e.cache_read_tokens,
        e.cache_write_tokens - write_1h,
        write_1h,
    ];
    if tokens
        .iter()
        .zip(rates)
        .any(|(&n, rate)| n > 0 && rate.is_none())
    {
        return (0.0, false);
    }
    let cost = tokens
        .iter()
        .zip(rates)
        .fold(0.0, |sum, (&n, rate)| sum + n as f64 * rate.unwrap_or(0.0));
    if cost.is_finite() {
        (cost, true)
    } else {
        (0.0, false)
    }
}

fn context_suffix(e: &Event, rates: &Rates) -> &'static str {
    let input = e.input_tokens + e.cache_read_tokens + e.cache_write_tokens;
    if input > 272_000 && rates.contains_key("input_cost_per_token_above_272k_tokens") {
        "_above_272k_tokens"
    } else if input > 200_000 && rates.contains_key("input_cost_per_token_above_200k_tokens") {
        "_above_200k_tokens"
    } else {
        ""
    }
}

/// sha256 of `JSON.stringify([catalog, rules])` with sorted keys, as token-tracker computes it.
fn revision(catalog: &Catalog, rules: &[Rule]) -> String {
    let value = serde_json::to_value((catalog, rules)).expect("pricing serializes");
    let mut json = String::new();
    write_stable(&mut json, &value);
    Sha256::digest(json.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        })
}

fn write_stable(out: &mut String, value: &Value) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_unstable_by_key(|(key, _)| *key);
            out.push('{');
            for (i, (key, value)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::from(key.as_str()).to_string());
                out.push(':');
                write_stable(out, value);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_stable(out, item);
            }
            out.push(']');
        }
        Value::Number(n) => out.push_str(&js_number(n.as_f64().unwrap_or(f64::NAN))),
        other => out.push_str(&other.to_string()),
    }
}

/// Formats a number as JavaScript's `Number.prototype.toString` does.
fn js_number(f: f64) -> String {
    if !f.is_finite() {
        return "null".into();
    }
    if f == 0.0 {
        return "0".into();
    }
    let sign = if f < 0.0 { "-" } else { "" };
    let shortest = format!("{:e}", f.abs());
    let (mantissa, exp) = shortest.split_once('e').expect("exponent form");
    let digits = mantissa.replace('.', "");
    let k = digits.len() as i32;
    let n = exp.parse::<i32>().expect("exponent") + 1;
    if k <= n && n <= 21 {
        format!("{sign}{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{sign}{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("{sign}0.{}{digits}", "0".repeat(-n as usize))
    } else {
        let frac = if k > 1 {
            format!(".{}", &digits[1..])
        } else {
            String::new()
        };
        format!(
            "{sign}{}{frac}e{}{}",
            &digits[..1],
            if n > 0 { "+" } else { "-" },
            (n - 1).abs()
        )
    }
}

/// pricing-state.json, the same file token-tracker keeps.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stored<C, R> {
    catalog: C,
    rules: R,
    checked_at: Option<String>,
    refresh_error: Option<String>,
}

struct State {
    policy: Arc<Policy>,
    checked_at: Option<String>,
    refresh_error: Option<String>,
}

/// Owns pricing-state.json in the service's state dir and hands out the current policy.
pub struct Pricing {
    path: PathBuf,
    download: String,
    state: Mutex<State>,
}

impl Pricing {
    /// Reads `dir/pricing-state.json`. Without one it adopts a valid `seed` file (read only),
    /// or else starts from the bundled catalog.
    pub fn open(dir: &Path, seed: Option<&Path>) -> Pricing {
        let path = dir.join(FILE);
        let unseeded = || State {
            policy: Arc::new(Policy::bundled()),
            checked_at: None,
            refresh_error: None,
        };
        let state = if path.exists() {
            read_state(&path).unwrap_or_else(|err| {
                tracing::warn!("pricing: {}: {err:#}", path.display());
                State {
                    refresh_error: Some(
                        "The saved pricing catalog could not be read; bundled prices remain available.".into(),
                    ),
                    ..unseeded()
                }
            })
        } else {
            match seed.map(read_state) {
                Some(Ok(seeded)) => {
                    if let Err(err) = write_state(&path, &seeded) {
                        tracing::warn!("pricing: could not save {}: {err:#}", path.display());
                    }
                    seeded
                }
                _ => unseeded(),
            }
        };
        Pricing {
            path,
            download: DOWNLOAD.into(),
            state: Mutex::new(state),
        }
    }

    pub fn policy(&self) -> Arc<Policy> {
        self.state.lock().unwrap().policy.clone()
    }

    fn until_due(&self) -> Duration {
        let state = self.state.lock().unwrap();
        let checked = state
            .checked_at
            .as_deref()
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok());
        checked.map_or(Duration::ZERO, |at| {
            (at.with_timezone(&Utc) + REFRESH_EVERY - Utc::now())
                .to_std()
                .unwrap_or(Duration::ZERO)
        })
    }

    /// Downloads LiteLLM's catalog and merges it over the bundled and saved models, keeping
    /// saved rules. A failed download keeps the previous catalog and records why.
    pub async fn refresh(&self) -> Result<()> {
        let downloaded = download(&self.download).await;
        let mut state = self.state.lock().unwrap();
        state.checked_at = Some(iso_now());
        let result = match downloaded {
            Err(err) => {
                state.refresh_error =
                    Some("Could not refresh model prices. The previous catalog and your saved rules remain available.".into());
                Err(err)
            }
            Ok(mut catalog) => {
                let mut models = BUNDLED.models.clone();
                models.extend(
                    state
                        .policy
                        .catalog
                        .models
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone())),
                );
                models.extend(catalog.models);
                catalog.models = models;
                let policy = Policy::new(catalog, state.policy.rules.clone());
                state.refresh_error = policy.problem();
                if state.refresh_error.is_none() {
                    state.policy = Arc::new(policy);
                }
                Ok(())
            }
        };
        write_state(&self.path, &state).context("could not save pricing settings")?;
        result
    }

    /// Refreshes whenever the last check is a day old, telling `changed` about each new policy.
    pub async fn run(self: Arc<Self>, changed: impl Fn(Arc<Policy>)) {
        loop {
            tokio::time::sleep(self.until_due()).await;
            let before = self.policy().revision.clone();
            let refreshed = self.refresh().await;
            if let Err(err) = refreshed {
                tracing::warn!("pricing: refresh: {err:#}");
            }
            let after = self.policy();
            if after.revision != before {
                changed(after);
            }
        }
    }
}

fn read_state(path: &Path) -> Result<State> {
    let data = std::fs::read(path)?;
    if data.len() > 20 << 20 {
        bail!("the saved pricing catalog is too large");
    }
    let stored: Stored<Catalog, Vec<Rule>> = serde_json::from_slice(&data)?;
    let policy = Policy::new(stored.catalog, stored.rules);
    if let Some(problem) = policy.problem() {
        bail!(problem);
    }
    Ok(State {
        policy: Arc::new(policy),
        checked_at: stored.checked_at,
        refresh_error: stored.refresh_error,
    })
}

fn write_state(path: &Path, state: &State) -> Result<()> {
    let stored = Stored {
        catalog: &state.policy.catalog,
        rules: &state.policy.rules,
        checked_at: state.checked_at.clone(),
        refresh_error: state.refresh_error.clone(),
    };
    let mut data = serde_json::to_vec(&stored)?;
    data.push(b'\n');
    write_atomic(path, &data)
}

pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let dir = path.parent().context("a state file has a directory")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)?;
    std::io::Write::write_all(&mut file, data)?;
    file.sync_all()?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

async fn download(url: &str) -> Result<Catalog> {
    let response = reqwest::Client::new()
        .get(url)
        .timeout(Duration::from_secs(15))
        .send()
        .await?
        .error_for_status()?;
    decode_upstream(&response.bytes().await?)
}

fn decode_upstream(body: &[u8]) -> Result<Catalog> {
    let raw: HashMap<String, Value> =
        serde_json::from_slice(body).context("the downloaded pricing catalog is invalid")?;
    let is_rate = |field: &str| {
        [
            "input_cost_per_token",
            "output_cost_per_token",
            "cache_read_input_token_cost",
            "cache_creation_input_token_cost",
        ]
        .iter()
        .any(|base| {
            field
                .strip_prefix(base)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('_'))
        })
    };
    let mut models = HashMap::new();
    for (model, value) in raw {
        let Value::Object(fields) = value else {
            continue;
        };
        let mut rates = Rates::new();
        for (field, rate) in fields {
            if !is_rate(&field) || rate.is_null() {
                continue;
            }
            match rate.as_f64() {
                Some(rate) if rate >= 0.0 => rates.insert(field, rate),
                _ => bail!("the downloaded price for {model} is invalid"),
            };
        }
        if rates.contains_key("input_cost_per_token") && rates.contains_key("output_cost_per_token")
        {
            models.insert(model, rates);
        }
    }
    if models.len() < 100 {
        bail!("the downloaded pricing catalog is unexpectedly incomplete");
    }
    Ok(Catalog {
        updated_at: iso_now(),
        source: SOURCE.into(),
        models,
    })
}

fn iso_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_rules() -> Vec<Rule> {
        vec![
            Rule {
                model: "my-model".into(),
                kind: RuleKind::Alias,
                target: "gpt-5".into(),
                nickname: String::new(),
                rates: None,
            },
            Rule {
                model: "cheap".into(),
                kind: RuleKind::Rates,
                target: String::new(),
                nickname: "Cheap".into(),
                rates: Some(RuleRates {
                    input_per_million: 1.0,
                    output_per_million: 2.0,
                    cache_read_per_million: Some(0.1),
                    ..RuleRates::default()
                }),
            },
        ]
    }

    fn test_event(
        harness: &str,
        model: &str,
        input: i64,
        output: i64,
        cache_read: i64,
        cache_write: i64,
    ) -> Event {
        Event {
            id: "a".into(),
            timestamp: "2026-01-01T00:00:00.000Z".into(),
            harness: harness.into(),
            model: model.into(),
            project: "p".into(),
            session_id: "s".into(),
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            ..Event::default()
        }
    }

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    #[test]
    fn revision_matches_token_tracker() {
        assert_eq!(
            Policy::bundled().revision,
            "b6d7dedba6e2b99bec30330f941cba7d8bfc98c6cbbc3a02ed9ede3c86128713"
        );
        let ruled = Policy::new(BUNDLED.clone(), test_rules());
        assert_eq!(
            ruled.revision,
            "bdfc8da71746171d07ab564de94505ff037a5816eb8787fb9998cbe7ee5bbaaf"
        );
    }

    #[tokio::test]
    async fn seeds_then_refreshes_its_own_file() {
        let root =
            std::env::temp_dir().join(format!("broadcom-usage-pricing-{}", std::process::id()));
        let seed = root.join("seed").join(FILE);
        let seeded = State {
            policy: Arc::new(Policy::new(BUNDLED.clone(), test_rules())),
            checked_at: None,
            refresh_error: None,
        };
        write_state(&seed, &seeded).unwrap();
        let seed_bytes = std::fs::read(&seed).unwrap();

        let dir = root.join("own");
        let pricing = Pricing::open(&dir, Some(&seed));
        assert_eq!(pricing.policy().revision, seeded.policy.revision);
        assert!(dir.join(FILE).exists());

        let upstream: serde_json::Map<String, Value> = (0..120)
            .map(|i| (format!("model-{i}"), serde_json::json!({ "input_cost_per_token": 1e-6, "output_cost_per_token": 2e-6, "max_tokens": 10 })))
            .collect();
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(move || async move { Value::Object(upstream).to_string() }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });

        let pricing = Pricing {
            download: url,
            ..Pricing::open(&dir, None)
        };
        assert_eq!(pricing.until_due(), Duration::ZERO);
        pricing.refresh().await.unwrap();
        let policy = pricing.policy();
        assert_eq!(
            policy.catalog.models["model-7"],
            Rates::from([
                ("input_cost_per_token".into(), 1e-6),
                ("output_cost_per_token".into(), 2e-6)
            ])
        );
        assert!(policy.catalog.models.contains_key("gpt-5") && policy.rules == test_rules());
        assert!(pricing.until_due() > Duration::from_secs(23 * 60 * 60));
        assert_eq!(Pricing::open(&dir, None).policy().revision, policy.revision);
        assert_eq!(
            std::fs::read(&seed).unwrap(),
            seed_bytes,
            "the seed file is only read"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn js_numbers() {
        for (f, s) in [
            (1.0, "1"),
            (0.1, "0.1"),
            (1e-7, "1e-7"),
            (2.5e-6, "0.0000025"),
            (1e21, "1e+21"),
            (123e18, "123000000000000000000"),
            (-1.5, "-1.5"),
        ] {
            assert_eq!(js_number(f), s);
        }
    }

    #[test]
    fn estimate_cost() {
        let policy = Policy::bundled();
        let opus = test_event(
            "claude",
            "claude-opus-4-5-20251101",
            1000,
            2000,
            300000,
            5000,
        );
        let (cost, known) = policy.estimate_cost(&opus, 2000, "standard");
        assert!(
            known && near(cost, 0.24374999999999997),
            "opus standard {cost} {known}"
        );
        assert_eq!(
            policy.estimate_cost(&opus, 2000, "auto"),
            (cost, known),
            "auto bills as standard"
        );
        assert!(
            !policy.estimate_cost(&opus, 0, "priority").1,
            "a catalog price never borrows standard rates for another tier"
        );
        let gpt = test_event("codex", "gpt-5", 250000, 1000, 30000, 0);
        assert!(
            near(policy.estimate_cost(&gpt, 0, "").0, 0.32625),
            "gpt-5 above 272k"
        );
        assert!(
            near(policy.estimate_cost(&gpt, 0, "flex").0, 0.163125),
            "gpt-5 flex"
        );
    }

    #[test]
    fn reprice_rules_and_legacy() {
        let policy = Policy::new(BUNDLED.clone(), test_rules());
        let mut alias = test_event("codex", "my-model", 250000, 1000, 30000, 0);
        alias.service_tier = Some(String::new());
        alias.cache_write_1h_tokens = Some(0);
        let got = policy.reprice(&alias).unwrap();
        assert!(
            got.model == "gpt-5" && got.raw_model == "my-model" && near(got.cost_usd, 0.32625),
            "alias {got:?}"
        );
        let custom = Event {
            model: "cheap".into(),
            ..alias
        };
        let got = policy.reprice(&custom).unwrap();
        assert!(
            got.model == "Cheap" && near(got.cost_usd, 0.255),
            "custom {got:?}"
        );
        let mut legacy = test_event(
            "claude",
            "claude-opus-4-5-20251101",
            1000,
            2000,
            300000,
            5000,
        );
        legacy.cost_usd = 1.25;
        legacy.cost_known = true;
        let got = policy.reprice(&legacy).unwrap_or(legacy);
        assert!(
            got.cost_usd == 1.25 && got.cost_known,
            "a record without tier metadata keeps its reported cost: {got:?}"
        );
        assert!(
            policy.reprice(&got).is_none(),
            "a repriced record is stable"
        );
        let mut cyclic = test_rules();
        cyclic.push(Rule {
            model: "gpt-5".into(),
            kind: RuleKind::Alias,
            target: "my-model".into(),
            nickname: String::new(),
            rates: None,
        });
        assert!(
            Policy::new(BUNDLED.clone(), cyclic).problem().is_some(),
            "alias cycle accepted"
        );
    }
}
