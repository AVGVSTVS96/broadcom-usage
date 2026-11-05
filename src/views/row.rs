use std::sync::Arc;

/// One resolved record as the views read it: only what they use, with the strings
/// records repeat (harness, model, project, session, device, title, entrypoint,
/// account) shared across rows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Row {
    pub millis: f64,
    pub proxy: bool,
    pub failed: bool,
    pub cost_known: bool,
    pub cost_usd: f64,
    pub harness: Arc<str>,
    pub model: Arc<str>,
    pub project: Arc<str>,
    pub repository: Option<Arc<str>>,
    pub session_id: Arc<str>,
    pub device_id: Arc<str>,
    pub session_title: Arc<str>,
    pub entrypoint: Arc<str>,
    pub account_id: Arc<str>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub unclassified_tokens: i64,
    pub reasoning_tokens: i64,
    pub requests: Option<i64>,
}

impl Row {
    pub fn token_total(&self) -> i64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens + self.unclassified_tokens
    }
}
