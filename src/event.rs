//! The usage record every source produces, in token-tracker's UsageEvent JSON shape,
//! so pushed batches, stored history and the views all read the same thing.

use serde::{Deserialize, Serialize};

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub response_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub upstream_request_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account_id: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub failed: bool,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub timestamp: String,
    #[serde(default)]
    pub harness: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub raw_model: String,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub cache_read_tokens: i64,
    #[serde(default)]
    pub cache_write_tokens: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unclassified_tokens: i64,
    #[serde(default)]
    pub reasoning_tokens: i64,
    #[serde(default, rename = "costUsd")]
    pub cost_usd: f64,
    #[serde(
        default,
        rename = "reportedCostUsd",
        skip_serializing_if = "Option::is_none"
    )]
    pub reported_cost_usd: Option<f64>,
    #[serde(default)]
    pub cost_known: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(
        default,
        rename = "cacheWrite1hTokens",
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_write_1h_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<i64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub device_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session_title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project_name: String,
    #[serde(
        default,
        rename = "t3ThreadId",
        skip_serializing_if = "String::is_empty"
    )]
    pub t3_thread_id: String,
    #[serde(
        default,
        rename = "t3ThreadUrl",
        skip_serializing_if = "String::is_empty"
    )]
    pub t3_thread_url: String,
    /// How the Claude Code process was started: cli, sdk-cli (claude -p), sdk-ts, or a product's own name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub entrypoint: String,
}

impl Event {
    pub fn token_total(&self) -> i64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_read_tokens
            + self.cache_write_tokens
            + self.unclassified_tokens
    }

    pub fn is_proxy(&self) -> bool {
        self.source == "proxy"
    }
}

/// A machine (or GitHub repository) whose records the store holds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub synced_at: Option<i64>,
}

/// One device's changes: records to add or correct, and ids proved to be replays.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Batch {
    #[serde(default)]
    pub device: Device,
    #[serde(default)]
    pub events: Vec<Event>,
    #[serde(default, rename = "deletedIds", skip_serializing_if = "Vec::is_empty")]
    pub deleted_ids: Vec<String>,
}

/// What a batch changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Applied {
    pub accepted: usize,
    pub updated: usize,
    pub deleted: usize,
}
