use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::{get, post},
    Router,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use urchin_core::{
    config::Config, ephemeral::EphemeralMode, event::Event, identity::Identity, journal::Journal,
};

#[derive(Clone)]
pub struct AppState {
    pub journal: Arc<Journal>,
    pub journal_path: PathBuf,
    pub identity: Arc<Identity>,
    /// Required Bearer token. If None, auth is disabled (safe because server binds loopback-only).
    pub token: Option<String>,
    /// Cross-process ephemeral mode flag.
    pub ephemeral: EphemeralMode,
}

impl AppState {
    pub fn from_config(cfg: &Config) -> Self {
        let index_path = cfg.journal_path.with_file_name("index.db");
        let journal =
            Journal::new_with_index(cfg.journal_path.clone(), index_path).unwrap_or_else(|e| {
                tracing::warn!("SQLite index unavailable, using JSONL fallback: {}", e);
                Journal::new(cfg.journal_path.clone())
            });
        Self {
            journal: Arc::new(journal),
            journal_path: cfg.journal_path.clone(),
            identity: Arc::new(Identity::resolve()),
            token: cfg.intake_token.clone(),
            ephemeral: EphemeralMode::default(),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ingest", post(ingest))
        .route("/ingest/batch", post(ingest_batch))
        .route("/recent", get(recent))
        .route("/query", get(query))
        .with_state(state)
}

/// Start the intake server on 127.0.0.1:<cfg.intake_port>.
/// Blocks until the process is killed or the listener dies.
pub async fn serve(cfg: &Config) -> Result<()> {
    let state = AppState::from_config(cfg);
    let addr = format!("127.0.0.1:{}", cfg.intake_port);
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!("intake listening on {}", addr);
    axum::serve(listener, router(state)).await?;
    Ok(())
}

/// Start the intake server with an external shutdown signal.
/// The server stops cleanly once `shutdown` resolves.
pub async fn serve_with_shutdown(
    cfg: &Config,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let state = AppState::from_config(cfg);
    let addr = format!("127.0.0.1:{}", cfg.intake_port);
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!("intake listening on {}", addr);
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

async fn health(State(state): State<AppState>) -> Json<Value> {
    let count = match state.journal.stats() {
        Ok(s) => s.event_count,
        Err(_) => 0,
    };
    Json(json!({
        "status":    "ok",
        "events":    count,
        "ephemeral": state.ephemeral.is_active(),
    }))
}

/// Accept a fully-formed Event from the SDK or any caller.
///
/// - Returns 401 if a token is configured and the Bearer header is wrong/missing.
/// - Returns 202 (drops silently) if ephemeral mode is active.
/// - Returns 400 if `content` or `source` are blank.
/// - Returns 200 on successful write.
async fn ingest(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(event): Json<Event>,
) -> (StatusCode, Json<Value>) {
    // ── Bearer auth ───────────────────────────────────────────────────────────
    if let Some(expected) = &state.token {
        let authorized = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|t| t == expected.as_str())
            .unwrap_or(false);
        if !authorized {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "unauthorized"})),
            );
        }
    }

    // ── Ephemeral mode — accept but drop ─────────────────────────────────────
    if state.ephemeral.is_active() {
        return (
            StatusCode::ACCEPTED,
            Json(json!({"id": event.id, "status": "dropped", "reason": "ephemeral"})),
        );
    }

    // ── Payload validation ────────────────────────────────────────────────────
    if event.content.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "content must not be empty"})),
        );
    }
    if event.source.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "source must not be empty"})),
        );
    }

    // ── Write ─────────────────────────────────────────────────────────────────
    let id = event.id;
    if let Err(e) = state.journal.append(&event) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        );
    }
    if let Err(e) = state.journal.flush() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        );
    }
    (StatusCode::OK, Json(json!({"id": id, "status": "ok"})))
}

const BATCH_MAX: usize = 1000;

#[derive(serde::Deserialize)]
struct BatchRequest {
    events: Vec<Event>,
}

async fn ingest_batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<BatchRequest>,
) -> (StatusCode, Json<Value>) {
    if let Some(expected) = &state.token {
        let authorized = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|t| t == expected.as_str())
            .unwrap_or(false);
        if !authorized {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "unauthorized"})),
            );
        }
    }

    if state.ephemeral.is_active() {
        let n = body.events.len();
        return (
            StatusCode::ACCEPTED,
            Json(json!({"accepted": 0, "dropped": n, "errors": []})),
        );
    }

    let events = if body.events.len() > BATCH_MAX {
        &body.events[..BATCH_MAX]
    } else {
        &body.events[..]
    };

    let mut accepted = 0usize;
    let mut dropped = body.events.len().saturating_sub(BATCH_MAX);
    let mut errors: Vec<Value> = Vec::new();

    for event in events {
        if event.content.trim().is_empty() {
            dropped += 1;
            errors.push(json!({"id": event.id, "error": "content must not be empty"}));
            continue;
        }
        if event.source.trim().is_empty() {
            dropped += 1;
            errors.push(json!({"id": event.id, "error": "source must not be empty"}));
            continue;
        }
        match state.journal.append(event) {
            Ok(_) => accepted += 1,
            Err(e) => {
                dropped += 1;
                errors.push(json!({"id": event.id, "error": e.to_string()}));
            }
        }
    }

    if accepted > 0 {
        let _ = state.journal.flush();
    }

    (
        StatusCode::OK,
        Json(json!({"accepted": accepted, "dropped": dropped, "errors": errors})),
    )
}

#[derive(serde::Deserialize)]
struct RecentParams {
    n: Option<usize>,
    source: Option<String>,
    hours: Option<f64>,
}

async fn recent(
    State(state): State<AppState>,
    Query(params): Query<RecentParams>,
) -> (StatusCode, Json<Value>) {
    let n = params.n.unwrap_or(20).min(200);
    let hours = params.hours.unwrap_or(168.0);
    match state
        .journal
        .query_recent(hours, params.source.as_deref(), n)
    {
        Ok(events) => {
            let items: Vec<Value> = events
                .iter()
                .map(|e| {
                    json!({
                        "id":        e.id,
                        "timestamp": e.timestamp,
                        "source":    e.source,
                        "kind":      e.kind,
                        "content":   e.content,
                    })
                })
                .collect();
            (
                StatusCode::OK,
                Json(json!({"events": items, "count": items.len()})),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        ),
    }
}

#[derive(serde::Deserialize)]
struct QueryParams {
    q: String,
    limit: Option<usize>,
    hours: Option<f64>,
}

async fn query(
    State(state): State<AppState>,
    Query(params): Query<QueryParams>,
) -> (StatusCode, Json<Value>) {
    if params.q.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "q must not be empty"})),
        );
    }
    let limit = params.limit.unwrap_or(20).min(200);
    let hours = params.hours.unwrap_or(168.0);
    match state.journal.query_search(&params.q, hours, limit) {
        Ok(events) => {
            let items: Vec<Value> = events
                .iter()
                .map(|e| {
                    json!({
                        "id":        e.id,
                        "timestamp": e.timestamp,
                        "source":    e.source,
                        "kind":      e.kind,
                        "content":   e.content,
                    })
                })
                .collect();
            (
                StatusCode::OK,
                Json(json!({"events": items, "count": items.len(), "query": params.q})),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tempfile::{NamedTempFile, TempDir};
    use tower::ServiceExt;

    fn test_state(journal_path: PathBuf, ephemeral_dir: &TempDir) -> AppState {
        AppState {
            journal: Arc::new(Journal::new(journal_path.clone())),
            journal_path,
            identity: Arc::new(Identity::for_test("test", "test")),
            token: None,
            ephemeral: EphemeralMode::new(&ephemeral_dir.path().to_path_buf()),
        }
    }

    fn test_state_with_token(
        journal_path: PathBuf,
        ephemeral_dir: &TempDir,
        token: &str,
    ) -> AppState {
        let mut state = test_state(journal_path, ephemeral_dir);
        state.token = Some(token.to_string());
        state
    }

    async fn json_body(resp: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    const EVENT_JSON: &str = r#"{
        "id": "56816532-adb7-4000-8a0f-1dda8408aab5",
        "timestamp": "2026-04-28T12:00:00Z",
        "source": "test",
        "kind": "conversation",
        "content": "hello from test"
    }"#;

    #[tokio::test]
    async fn health_reflects_ingested_events() {
        let tmp_j = NamedTempFile::new().unwrap();
        let tmp_e = TempDir::new().unwrap();
        let state = test_state(tmp_j.path().to_path_buf(), &tmp_e);
        let app = router(state);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let before = json_body(resp).await;
        assert_eq!(before["events"], 0);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ingest")
                    .header("content-type", "application/json")
                    .body(Body::from(EVENT_JSON))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let posted = json_body(resp).await;
        assert_eq!(posted["status"], "ok");
        assert_eq!(posted["id"], "56816532-adb7-4000-8a0f-1dda8408aab5");

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let after = json_body(resp).await;
        assert_eq!(after["events"], 1);
    }

    #[tokio::test]
    async fn ingest_rejects_missing_required_fields() {
        let tmp_j = NamedTempFile::new().unwrap();
        let tmp_e = TempDir::new().unwrap();
        let state = test_state(tmp_j.path().to_path_buf(), &tmp_e);
        let app = router(state);

        let body = r#"{"source":"test","content":"oops"}"#;
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ingest")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(resp.status().is_client_error());
    }

    #[tokio::test]
    async fn ingest_rejects_request_without_bearer_when_token_set() {
        let tmp_j = NamedTempFile::new().unwrap();
        let tmp_e = TempDir::new().unwrap();
        let state = test_state_with_token(tmp_j.path().to_path_buf(), &tmp_e, "secret");
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ingest")
                    .header("content-type", "application/json")
                    .body(Body::from(EVENT_JSON))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn ingest_rejects_wrong_bearer() {
        let tmp_j = NamedTempFile::new().unwrap();
        let tmp_e = TempDir::new().unwrap();
        let state = test_state_with_token(tmp_j.path().to_path_buf(), &tmp_e, "secret");
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ingest")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer wrong-token")
                    .body(Body::from(EVENT_JSON))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn ingest_accepts_correct_bearer() {
        let tmp_j = NamedTempFile::new().unwrap();
        let tmp_e = TempDir::new().unwrap();
        let state = test_state_with_token(tmp_j.path().to_path_buf(), &tmp_e, "secret");
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ingest")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer secret")
                    .body(Body::from(EVENT_JSON))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["status"], "ok");
    }

    #[tokio::test]
    async fn ingest_rejects_empty_content() {
        let tmp_j = NamedTempFile::new().unwrap();
        let tmp_e = TempDir::new().unwrap();
        let state = test_state(tmp_j.path().to_path_buf(), &tmp_e);
        let app = router(state);

        let body = r#"{
            "id": "56816532-adb7-4000-8a0f-1dda8408aab5",
            "timestamp": "2026-04-28T12:00:00Z",
            "source": "test",
            "kind": "conversation",
            "content": "   "
        }"#;
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ingest")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn ingest_rejects_empty_source() {
        let tmp_j = NamedTempFile::new().unwrap();
        let tmp_e = TempDir::new().unwrap();
        let state = test_state(tmp_j.path().to_path_buf(), &tmp_e);
        let app = router(state);

        let body = r#"{
            "id": "56816532-adb7-4000-8a0f-1dda8408aab5",
            "timestamp": "2026-04-28T12:00:00Z",
            "source": "",
            "kind": "conversation",
            "content": "hello"
        }"#;
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ingest")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    fn batch_body(events: &[(&str, &str, &str)]) -> String {
        let items: Vec<String> = events.iter().enumerate().map(|(i, (source, kind, content))| {
            format!(
                r#"{{"id":"56816532-adb7-4000-8a0f-1dda8408aa{:02x}","timestamp":"2026-04-28T12:00:00Z","source":"{}","kind":"{}","content":"{}"}}"#,
                i, source, kind, content
            )
        }).collect();
        format!(r#"{{"events":[{}]}}"#, items.join(","))
    }

    #[tokio::test]
    async fn batch_accepts_multiple_events() {
        let tmp_j = NamedTempFile::new().unwrap();
        let tmp_e = TempDir::new().unwrap();
        let state = test_state(tmp_j.path().to_path_buf(), &tmp_e);
        let app = router(state);

        let body = batch_body(&[
            ("test", "conversation", "first event"),
            ("test", "command", "second event"),
            ("test", "purchase", "third event"),
        ]);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ingest/batch")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let result = json_body(resp).await;
        assert_eq!(result["accepted"], 3);
        assert_eq!(result["dropped"], 0);
    }

    #[tokio::test]
    async fn batch_partial_success_on_invalid_events() {
        let tmp_j = NamedTempFile::new().unwrap();
        let tmp_e = TempDir::new().unwrap();
        let state = test_state(tmp_j.path().to_path_buf(), &tmp_e);
        let app = router(state);

        let body = r#"{"events":[
            {"id":"56816532-adb7-4000-8a0f-1dda8408aa00","timestamp":"2026-04-28T12:00:00Z","source":"test","kind":"conversation","content":"valid"},
            {"id":"56816532-adb7-4000-8a0f-1dda8408aa01","timestamp":"2026-04-28T12:00:00Z","source":"test","kind":"conversation","content":"   "},
            {"id":"56816532-adb7-4000-8a0f-1dda8408aa02","timestamp":"2026-04-28T12:00:00Z","source":"test","kind":"conversation","content":"also valid"}
        ]}"#;

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ingest/batch")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        let result = json_body(resp).await;
        assert_eq!(result["accepted"], 2);
        assert_eq!(result["dropped"], 1);
        assert_eq!(result["errors"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn batch_drops_all_in_ephemeral_mode() {
        let tmp_j = NamedTempFile::new().unwrap();
        let tmp_e = TempDir::new().unwrap();
        let state = test_state(tmp_j.path().to_path_buf(), &tmp_e);
        state.ephemeral.activate().unwrap();
        let app = router(state);

        let body = batch_body(&[
            ("test", "conversation", "one"),
            ("test", "conversation", "two"),
        ]);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ingest/batch")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let result = json_body(resp).await;
        assert_eq!(result["accepted"], 0);
        assert_eq!(result["dropped"], 2);
    }

    #[tokio::test]
    async fn batch_requires_auth_when_token_set() {
        let tmp_j = NamedTempFile::new().unwrap();
        let tmp_e = TempDir::new().unwrap();
        let state = test_state_with_token(tmp_j.path().to_path_buf(), &tmp_e, "secret");
        let app = router(state);

        let body = batch_body(&[("test", "conversation", "hello")]);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ingest/batch")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn ingest_drops_silently_in_ephemeral_mode() {
        let tmp_j = NamedTempFile::new().unwrap();
        let tmp_e = TempDir::new().unwrap();
        let state = test_state(tmp_j.path().to_path_buf(), &tmp_e);
        state.ephemeral.activate().unwrap();
        let app = router(state);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ingest")
                    .header("content-type", "application/json")
                    .body(Body::from(EVENT_JSON))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body = json_body(resp).await;
        assert_eq!(body["status"], "dropped");

        // Nothing written to journal
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let health = json_body(resp).await;
        assert_eq!(health["events"], 0);
    }
}
