//! HTTP: the deck views under `/v8/management/deck`, the live stream, and the push ingest.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::json;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use crate::app::App;
use crate::ingest;

pub fn router(app: Arc<App>) -> Router {
    let deck = Router::new()
        .route("/usage", get(crate::app::usage))
        .route("/sessions", get(crate::app::sessions))
        .route("/credits", get(crate::app::credits))
        .route("/events", get(events));
    Router::new()
        .nest("/v8/management/deck", deck)
        .route("/v0/management/usage/events", post(push).layer(DefaultBodyLimit::max(256 << 20)))
        .layer(middleware::from_fn_with_state(app.clone(), authorize))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(app)
}

async fn authorize(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let given = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if app.key.is_empty() || !constant_time_eq(given.as_bytes(), app.key.as_bytes()) {
        return (StatusCode::UNAUTHORIZED, axum::Json(json!({"error": "invalid management key"}))).into_response();
    }
    next.run(request).await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn push(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    let gzip = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("gzip"));
    let bad = |message: &str| (StatusCode::BAD_REQUEST, axum::Json(json!({"error": message}))).into_response();
    let batch = match ingest::decode(&body, gzip) {
        Ok(batch) => batch,
        Err(message) => return bad(message),
    };
    if let Some(message) = ingest::validate(&batch, &app.store.local().id) {
        return bad(message);
    }
    let store = app.store.clone();
    match tokio::task::spawn_blocking(move || store.apply(&batch, false)).await {
        Ok(Ok(result)) => axum::Json(json!({
            "accepted": result.accepted,
            "updated": result.updated,
            "deleted": result.deleted,
            "receivedAt": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        }))
        .into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": "could not save usage"}))).into_response(),
    }
}

async fn events(State(app): State<Arc<App>>) -> Response {
    let frames = ReceiverStream::new(app.deck.stream.subscribe()).map(|frame| Ok::<_, Infallible>(Bytes::from(frame)));
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(frames))
        .unwrap()
}
