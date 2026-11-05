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

impl From<&crate::event::Event> for Row {
    fn from(e: &crate::event::Event) -> Self {
        Row {
            millis: super::parse_millis(&e.timestamp),
            proxy: e.is_proxy(),
            failed: e.failed,
            cost_known: e.cost_known,
            cost_usd: e.cost_usd,
            harness: e.harness.as_str().into(),
            model: e.model.as_str().into(),
            project: e.project.as_str().into(),
            repository: e.repository.as_deref().map(Arc::from),
            session_id: e.session_id.as_str().into(),
            device_id: e.device_id.as_str().into(),
            session_title: e.session_title.as_str().into(),
            entrypoint: e.entrypoint.as_str().into(),
            account_id: e.account_id.as_str().into(),
            input_tokens: e.input_tokens,
            output_tokens: e.output_tokens,
            cache_read_tokens: e.cache_read_tokens,
            cache_write_tokens: e.cache_write_tokens,
            unclassified_tokens: e.unclassified_tokens,
            reasoning_tokens: e.reasoning_tokens,
            requests: e.requests,
        }
    }
}

impl Row {
    pub fn token_total(&self) -> i64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens + self.unclassified_tokens
    }
}
