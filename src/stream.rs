//! One SSE stream per tab: a snapshot of the live keys on connect, then only the keys
//! whose JSON changed, and `stale` frames naming resources to refetch.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::value::RawValue;
use tokio::sync::mpsc;

type Live = HashMap<String, Box<RawValue>>;

#[derive(Default)]
pub struct Stream {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    clients: Vec<mpsc::Sender<String>>,
    keys: Vec<String>,
    live: Live,
}

fn object(keys: &[String], values: &Live) -> String {
    let mut out = String::from("{");
    for (i, key) in keys.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&serde_json::to_string(key).unwrap());
        out.push(':');
        out.push_str(values.get(key).map_or("null", |v| v.get()));
    }
    out.push('}');
    out
}

fn frame(kind: &str, body: &str) -> String {
    format!("data: {{\"type\":\"{kind}\",\"live\":{body}}}\n\n")
}

impl Stream {
    pub fn listeners(&self) -> usize {
        self.state.lock().unwrap().clients.len()
    }

    /// A new client's channel, which starts with the snapshot.
    pub fn subscribe(&self) -> mpsc::Receiver<String> {
        let mut state = self.state.lock().unwrap();
        let (tx, rx) = mpsc::channel(64);
        let _ = tx.try_send(frame("snapshot", &object(&state.keys, &state.live)));
        state.clients.push(tx);
        rx
    }

    /// Replaces the live keys, sending the ones that changed; a removed key is sent as null.
    pub fn publish(&self, entries: Vec<(String, Box<RawValue>)>) {
        let mut state = self.state.lock().unwrap();
        let mut next = Live::with_capacity(entries.len());
        let mut keys = Vec::with_capacity(entries.len());
        let mut changed = Vec::new();
        let mut patch = Live::new();
        for (key, value) in entries {
            if state
                .live
                .get(&key)
                .is_none_or(|previous| previous.get() != value.get())
            {
                changed.push(key.clone());
                patch.insert(key.clone(), value.clone());
            }
            keys.push(key.clone());
            next.insert(key, value);
        }
        let mut order: Vec<String> = Vec::with_capacity(keys.len());
        for key in &state.keys {
            if next.contains_key(key) {
                order.push(key.clone());
            } else {
                changed.push(key.clone());
            }
        }
        for key in keys {
            if !state.live.contains_key(&key) {
                order.push(key);
            }
        }
        state.keys = order;
        state.live = next;
        if !changed.is_empty() {
            let message = frame("patch", &object(&changed, &patch));
            send(&mut state, message);
        }
    }

    pub fn stale(&self, tags: &[&str]) {
        if tags.is_empty() {
            return;
        }
        let message = format!(
            "data: {}\n\n",
            serde_json::json!({"type": "stale", "tags": tags})
        );
        send(&mut self.state.lock().unwrap(), message);
    }

    pub fn heartbeat(&self) {
        send(&mut self.state.lock().unwrap(), ": \n\n".into());
    }
}

/// Drops a client too slow to keep up; its tab reconnects and takes a fresh snapshot.
fn send(state: &mut State, message: String) {
    state
        .clients
        .retain(|client| client.try_send(message.clone()).is_ok());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(s: &str) -> Box<RawValue> {
        RawValue::from_string(s.into()).unwrap()
    }

    #[tokio::test]
    async fn sends_snapshot_then_changed_keys_only() {
        let stream = Stream::default();
        stream.publish(vec![
            ("s:a".into(), raw(r#"{"state":"idle"}"#)),
            ("app:x".into(), raw("1")),
        ]);
        let mut client = stream.subscribe();
        assert_eq!(
            client.recv().await.unwrap(),
            "data: {\"type\":\"snapshot\",\"live\":{\"s:a\":{\"state\":\"idle\"},\"app:x\":1}}\n\n"
        );
        stream.publish(vec![("s:a".into(), raw(r#"{"state":"working"}"#))]);
        assert_eq!(
            client.recv().await.unwrap(),
            "data: {\"type\":\"patch\",\"live\":{\"s:a\":{\"state\":\"working\"},\"app:x\":null}}\n\n"
        );
        stream.publish(vec![("s:a".into(), raw(r#"{"state":"working"}"#))]);
        stream.stale(&["usage"]);
        assert_eq!(
            client.recv().await.unwrap(),
            "data: {\"type\":\"stale\",\"tags\":[\"usage\"]}\n\n"
        );
    }
}
