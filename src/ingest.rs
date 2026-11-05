//! Batches other machines push: the same gzip JSON and checks the Go proxy's
//! `POST /v0/management/usage/events` takes, so a pusher only changes its URL.

use std::io::Read;

use crate::event::Batch;

pub const MAX_RECORDS: usize = 10_000;

/// Reads a pushed body, gunzipping it when the client said so.
pub fn decode(body: &[u8], gzip: bool) -> Result<Batch, &'static str> {
    let mut text = Vec::new();
    if gzip {
        flate2::read::GzDecoder::new(body)
            .take(1 << 30)
            .read_to_end(&mut text)
            .map_err(|_| "invalid gzip body")?;
    } else {
        text = body.to_vec();
    }
    serde_json::from_slice(&text).map_err(|_| "invalid usage batch")
}

/// Why a batch can't be stored, or None.
pub fn validate(batch: &Batch, local: &str) -> Option<&'static str> {
    let id = batch.device.id.trim();
    if id.is_empty() || id.len() > 200 || batch.device.label.len() > 200 {
        return Some("A valid device id is required.");
    }
    if id == local {
        return Some("The server collects its own logs.");
    }
    if batch.events.len() + batch.deleted_ids.len() > MAX_RECORDS {
        return Some("A batch can contain at most 10,000 records.");
    }
    for e in &batch.events {
        if e.id.is_empty() || crate::store::millis_of(&e.timestamp).is_none() {
            return Some("Usage records need a stable id and a valid timestamp.");
        }
        let counts = [
            e.input_tokens,
            e.output_tokens,
            e.cache_read_tokens,
            e.cache_write_tokens,
            e.reasoning_tokens,
            e.unclassified_tokens,
            e.requests.unwrap_or(0),
        ];
        let costs = [e.cost_usd, e.reported_cost_usd.unwrap_or(0.0)];
        if counts.iter().any(|&n| n < 0) || costs.iter().any(|&c| !(c >= 0.0) || c.is_infinite()) {
            return Some("Usage records need nonnegative token counts and cost.");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Device, Event};

    fn batch(device: &str, events: Vec<Event>) -> Batch {
        Batch { device: Device { id: device.into(), label: "laptop".into(), synced_at: None }, events, deleted_ids: vec![] }
    }

    fn event() -> Event {
        Event { id: "a".into(), timestamp: "2026-11-01T00:00:00Z".into(), ..Default::default() }
    }

    #[test]
    fn checks_what_go_checks() {
        assert_eq!(validate(&batch("laptop", vec![event()]), "server"), None);
        assert!(validate(&batch("server", vec![]), "server").is_some());
        assert!(validate(&batch(" ", vec![]), "server").is_some());
        assert!(validate(&batch("laptop", vec![Event { timestamp: "soon".into(), ..event() }]), "server").is_some());
        assert!(validate(&batch("laptop", vec![Event { output_tokens: -1, ..event() }]), "server").is_some());
        assert!(validate(&batch("laptop", vec![Event { cost_usd: f64::NAN, ..event() }]), "server").is_some());
        assert!(validate(&batch("laptop", vec![event(); MAX_RECORDS + 1]), "server").is_some());
    }

    #[test]
    fn reads_gzip_bodies() {
        use std::io::Write;
        let json = serde_json::to_vec(&batch("laptop", vec![event()])).unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&json).unwrap();
        assert_eq!(decode(&gz.finish().unwrap(), true).unwrap().events.len(), 1);
        assert!(decode(b"nope", true).is_err());
    }
}
