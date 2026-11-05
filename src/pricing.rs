//! The LiteLLM-derived price catalog and saved rules, as an immutable policy.

use crate::event::Event;

pub struct Policy;

impl Policy {
    /// Prices a freshly collected record: (cost, known).
    pub fn estimate_cost(&self, _e: &Event, _cache_write_1h: i64, _tier: &str) -> (f64, bool) {
        todo!()
    }

    /// The model name records show, after alias rules.
    pub fn display_model(&self, _raw: &str) -> String {
        todo!()
    }

    /// Applies the policy to a stored record; None when nothing changes.
    pub fn reprice(&self, _e: &Event) -> Option<Event> {
        todo!()
    }
}
