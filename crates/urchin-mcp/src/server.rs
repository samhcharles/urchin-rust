/// JSON-RPC 2.0 over stdio for MCP. One request per line on stdin,
/// one response per line on stdout. All logs go to stderr — stdout is protocol-only.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use urchin_core::{config::Config, identity::Identity, journal::Journal};

use crate::tools::{self, ToolContext};

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME:      &str = "urchin";
const SERVER_VERSION:   &str = env!("CARGO_PKG_VERSION");

pub async fn run(cfg: Config) -> Result<()> {
    let ctx = ToolContext {
        journal:  Arc::new(Journal::new(cfg.journal_path.clone())),
        identity: Arc::new(Identity::resolve()),
        config:   Arc::new(cfg),
    };

    let stdin  = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();
    let mut line = String::new();

    tracing::info!("mcp stdio loop started");

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 { break; } // EOF: peer closed stdin

        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }

        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                // Can't recover an id if we can't parse — emit a Parse error with null id.
                let err = error_response(&Value::Null, -32700, &format!("parse error: {}", e));
                write_response(&mut stdout, &err).await?;
                continue;
            }
        };

        if let Some(resp) = handle(&req, &ctx) {
            write_response(&mut stdout, &resp).await?;
        }
    }

    Ok(())
}

fn handle(req: &Value, ctx: &ToolContext) -> Option<Value> {
    let id     = req.get("id").cloned();
    let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    // Notifications (no id) get no response — only side effects.
    let is_notification = id.is_none();

    match method {
        "initialize" => {
            let result = json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name":    SERVER_NAME,
                    "version": SERVER_VERSION
                }
            });
            Some(success_response(&id.unwrap_or(Value::Null), result))
        }

        "initialized" | "notifications/initialized" => {
            // Notification — no response.
            None
        }

        "tools/list" => {
            let result = json!({ "tools": tools::tool_list() });
            Some(success_response(&id.unwrap_or(Value::Null), result))
        }

        "tools/call" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

            match tools::call(name, &args, ctx) {
                Ok(text) => Some(success_response(
                    &id.unwrap_or(Value::Null),
                    json!({
                        "content": [{"type": "text", "text": text}]
                    }),
                )),
                Err(e) => Some(success_response(
                    &id.unwrap_or(Value::Null),
                    json!({
                        "content": [{"type": "text", "text": e.to_string()}],
                        "isError": true
                    }),
                )),
            }
        }

        _ if is_notification => None,

        _ => Some(error_response(
            &id.unwrap_or(Value::Null),
            -32601,
            &format!("method not found: {}", method),
        )),
    }
}

fn success_response(id: &Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

async fn write_response(
    stdout: &mut tokio::io::Stdout,
    resp: &Value,
) -> Result<()> {
    let s = serde_json::to_string(resp)?;
    stdout.write_all(s.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn test_ctx() -> (ToolContext, NamedTempFile) {
        let tmp = NamedTempFile::new().unwrap();
        let mut cfg = Config::default();
        cfg.journal_path = tmp.path().to_path_buf();
        let ctx = ToolContext {
            journal:  Arc::new(Journal::new(tmp.path().to_path_buf())),
            identity: Arc::new(Identity { account: "t".into(), device: "t".into() }),
            config:   Arc::new(cfg),
        };
        (ctx, tmp)
    }

    #[test]
    fn initialize_responds_with_protocol_version() {
        let (ctx, _tmp) = test_ctx();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        });
        let resp = handle(&req, &ctx).unwrap();
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(resp["result"]["serverInfo"]["name"], "urchin");
    }

    #[test]
    fn initialized_notification_returns_nothing() {
        let (ctx, _tmp) = test_ctx();
        let req = json!({
            "jsonrpc": "2.0",
            "method": "initialized"
        });
        assert!(handle(&req, &ctx).is_none());
    }

    #[test]
    fn tools_list_returns_five_tools() {
        let (ctx, _tmp) = test_ctx();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        });
        let resp = handle(&req, &ctx).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 5);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"urchin_status"));
        assert!(names.contains(&"urchin_ingest"));
        assert!(names.contains(&"urchin_recent_activity"));
        assert!(names.contains(&"urchin_project_context"));
        assert!(names.contains(&"urchin_search"));
    }

    #[test]
    fn tools_call_status_returns_text_content() {
        let (ctx, _tmp) = test_ctx();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "urchin_status", "arguments": {}}
        });
        let resp = handle(&req, &ctx).unwrap();
        let content = &resp["result"]["content"];
        assert_eq!(content[0]["type"], "text");
        assert!(content[0]["text"].as_str().unwrap().contains("running:  true"));
    }

    #[test]
    fn unknown_method_returns_error() {
        let (ctx, _tmp) = test_ctx();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "nope"
        });
        let resp = handle(&req, &ctx).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[test]
    fn tool_error_surfaces_as_is_error() {
        let (ctx, _tmp) = test_ctx();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {"name": "urchin_ingest", "arguments": {}}
        });
        let resp = handle(&req, &ctx).unwrap();
        assert_eq!(resp["result"]["isError"], true);
    }
}
