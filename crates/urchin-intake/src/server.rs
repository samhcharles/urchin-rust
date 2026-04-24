use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use urchin_core::{
    config::Config,
    event::{Actor, Event, EventKind},
    identity::Identity,
    journal::Journal,
};

#[derive(Clone)]
pub struct AppState {
    pub journal: Arc<Journal>,
    pub journal_path: PathBuf,
    pub identity: Arc<Identity>,
}

impl AppState {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            journal: Arc::new(Journal::new(cfg.journal_path.clone())),
            journal_path: cfg.journal_path.clone(),
            identity: Arc::new(Identity::resolve()),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ingest", post(ingest))
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

async fn health(State(state): State<AppState>) -> Json<Value> {
    let count = match state.journal.stats() {
        Ok(s) => s.event_count,
        Err(_) => 0,
    };
    Json(json!({
        "status":  "ok",
        "events":  count,
        "journal": state.journal_path.display().to_string(),
    }))
}

#[derive(Deserialize)]
pub struct IngestRequest {
    pub content: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub session: Option<String>,
}

async fn ingest(
    State(state): State<AppState>,
    Json(req): Json<IngestRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let kind = parse_kind(req.kind.as_deref().unwrap_or("conversation"));
    let mut event = Event::new(
        req.source.unwrap_or_else(|| "http".into()),
        kind,
        req.content,
    );
    event.workspace = req.workspace;
    event.title     = req.title;
    event.tags      = req.tags;
    event.session   = req.session;
    event.actor = Some(Actor {
        account:   Some(state.identity.account.clone()),
        device:    Some(state.identity.device.clone()),
        workspace: event.workspace.clone(),
    });

    if let Err(e) = state.journal.append(&event) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        ));
    }

    Ok(Json(json!({
        "id":     event.id,
        "status": "ok",
    })))
}

fn parse_kind(s: &str) -> EventKind {
    match s {
        "agent"        => EventKind::Agent,
        "command"      => EventKind::Command,
        "commit"       => EventKind::Commit,
        "file"         => EventKind::File,
        "conversation" => EventKind::Conversation,
        other          => EventKind::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tempfile::NamedTempFile;
    use tower::ServiceExt;

    fn test_state(path: PathBuf) -> AppState {
        AppState {
            journal:      Arc::new(Journal::new(path.clone())),
            journal_path: path,
            identity:     Arc::new(Identity {
                account: "test".into(),
                device:  "test".into(),
            }),
        }
    }

    async fn json_body(resp: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn health_reflects_ingested_events() {
        let tmp = NamedTempFile::new().unwrap();
        let state = test_state(tmp.path().to_path_buf());
        let app = router(state);

        // health before
        let resp = app.clone().oneshot(
            Request::builder().uri("/health").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let before = json_body(resp).await;
        assert_eq!(before["status"], "ok");
        assert_eq!(before["events"], 0);

        // POST /ingest
        let body = r#"{"source":"test","content":"hello from test","kind":"conversation"}"#;
        let resp = app.clone().oneshot(
            Request::builder()
                .method("POST")
                .uri("/ingest")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap()
        ).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let posted = json_body(resp).await;
        assert_eq!(posted["status"], "ok");
        assert!(posted["id"].is_string());

        // health after
        let resp = app.oneshot(
            Request::builder().uri("/health").body(Body::empty()).unwrap()
        ).await.unwrap();
        let after = json_body(resp).await;
        assert_eq!(after["events"], 1);
    }

    #[tokio::test]
    async fn ingest_rejects_missing_content() {
        let tmp = NamedTempFile::new().unwrap();
        let state = test_state(tmp.path().to_path_buf());
        let app = router(state);

        let body = r#"{"source":"test"}"#;
        let resp = app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/ingest")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap()
        ).await.unwrap();

        assert!(resp.status().is_client_error());
    }
}
