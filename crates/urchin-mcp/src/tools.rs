//! Tool schemas and execution for the MCP server.
//! Each tool takes a Value argument map, reads/writes the journal, returns a text block.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use urchin_core::{
    config::Config,
    ephemeral::EphemeralMode,
    event::{Actor, Event, EventKind},
    identity::Identity,
    journal::Journal,
    query,
};

pub struct ToolContext {
    pub journal: Arc<Journal>,
    pub identity: Arc<Identity>,
    pub config: Arc<Config>,
    /// Ephemeral mode: when true, ingest/remember are no-ops.
    pub ephemeral: Arc<AtomicBool>,
    /// Count of events suppressed during ephemeral mode.
    pub suppressed: Arc<AtomicUsize>,
}

/// JSON Schema definitions returned from tools/list.
pub fn tool_list() -> Value {
    json!([
        {
            "name": "urchin_status",
            "description": "Show Urchin daemon health: event count, last event, journal path, intake port, vault root.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "urchin_ingest",
            "description": "Write an event into the Urchin journal. Use this to record a note, decision, or context tied to a workspace.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content":   { "type": "string",  "description": "The memory payload." },
                    "workspace": { "type": "string",  "description": "Absolute path to the workspace/repo this event belongs to." },
                    "source":    { "type": "string",  "description": "Origin tool: claude, copilot, cli, agent, etc. Defaults to 'mcp'." },
                    "title":     { "type": "string",  "description": "Optional short title." },
                    "kind":      { "type": "string",  "description": "conversation | agent | command | commit | file | decision. Defaults to conversation." },
                    "tags":      { "type": "array",   "items": { "type": "string" } },
                    "session":   { "type": "string",  "description": "Optional session identifier." }
                },
                "required": ["content", "workspace"],
                "additionalProperties": false
            }
        },
        {
            "name": "urchin_recent_activity",
            "description": "List recent events across all sources, newest first. Filter by source or time window.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "hours":  { "type": "number", "description": "Look back this many hours. Default 24." },
                    "source": { "type": "string", "description": "Filter to a single source (e.g. 'claude')." },
                    "limit":  { "type": "number", "description": "Max events to return. Default 20." }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "urchin_project_context",
            "description": "Events scoped to a project — match on content substring or tag (case-insensitive).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Project name or substring to match." },
                    "hours":   { "type": "number", "description": "Look back this many hours. Default 168 (1 week)." },
                    "limit":   { "type": "number", "description": "Max events to return. Default 30." }
                },
                "required": ["project"],
                "additionalProperties": false
            }
        },
        {
            "name": "urchin_search",
            "description": "Case-insensitive substring search over event content.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search term." },
                    "hours": { "type": "number", "description": "Look back this many hours. Default 168." },
                    "limit": { "type": "number", "description": "Max events to return. Default 20." }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        },
        {
            "name": "urchin_workspace_context",
            "description": "Return events scoped to a specific workspace path. Call this at the start of a coding session to load relevant memory for the current repo.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path":  { "type": "string", "description": "Absolute path to the workspace/repo. Prefix-matched." },
                    "hours": { "type": "number", "description": "Look back this many hours. Default 168 (1 week)." },
                    "limit": { "type": "number", "description": "Max events to return. Default 40." }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        },
        {
            "name": "urchin_remember",
            "description": "Quick-capture: write a memory note without a required workspace. Use this for ideas, decisions, or observations that aren't tied to a specific repo.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content":   { "type": "string", "description": "The note to capture." },
                    "tags":      { "type": "array", "items": { "type": "string" }, "description": "Optional tags." },
                    "workspace": { "type": "string", "description": "Optional workspace path." }
                },
                "required": ["content"],
                "additionalProperties": false
            }
        },
        {
            "name": "urchin_ephemeral",
            "description": "Control ephemeral (burn) mode. When active, no events are written to the journal. Use start before sensitive work, end when done.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["start", "end", "status"], "description": "Action to perform." }
                },
                "required": ["action"],
                "additionalProperties": false
            }
        },
    ])
}

pub fn call(name: &str, args: &Value, ctx: &ToolContext) -> Result<String> {
    match name {
        "urchin_status" => status(ctx),
        "urchin_ingest" => ingest(args, ctx),
        "urchin_recent_activity" => recent_activity(args, ctx),
        "urchin_project_context" => project_context(args, ctx),
        "urchin_search" => search(args, ctx),
        "urchin_workspace_context" => workspace_context(args, ctx),
        "urchin_remember" => remember(args, ctx),
        "urchin_ephemeral" => ephemeral(args, ctx),
        other => Err(anyhow::anyhow!("unknown tool: {}", other)),
    }
}

fn status(ctx: &ToolContext) -> Result<String> {
    let stats = ctx.journal.stats()?;
    let mut out = String::new();
    out.push_str("urchin — local memory sync substrate\n\n");
    out.push_str("running:  true\n");
    out.push_str(&format!("events:   {}\n", stats.event_count));
    out.push_str(&format!("size:     {} KB\n", stats.file_size_bytes / 1024));
    if let Some(last) = stats.last_event {
        out.push_str(&format!(
            "last:     {} ({})\n",
            last.timestamp.format("%Y-%m-%dT%H:%M:%SZ"),
            last.source,
        ));
    } else {
        out.push_str("last:     (no events yet)\n");
    }
    out.push_str(&format!(
        "journal:  {}\n",
        ctx.config.journal_path.display()
    ));
    out.push_str(&format!("intake:   {}\n", ctx.config.intake_port));
    out.push_str(&format!("vault:    {}\n", ctx.config.vault_root.display()));
    out.push_str(&format!("account:  {}\n", ctx.identity.account));
    out.push_str(&format!("device:   {}\n", ctx.identity.device));
    Ok(out)
}

fn ingest(args: &Value, ctx: &ToolContext) -> Result<String> {
    if ctx.ephemeral.load(Ordering::Relaxed) {
        ctx.suppressed.fetch_add(1, Ordering::Relaxed);
        return Ok("(ephemeral mode: event suppressed)".to_string());
    }

    let content = required_str(args, "content")?;
    let workspace = required_str(args, "workspace")?;
    let source = opt_str(args, "source").unwrap_or_else(|| "mcp".to_string());
    let title = opt_str(args, "title");
    let kind_raw = opt_str(args, "kind").unwrap_or_else(|| "conversation".to_string());
    let session = opt_str(args, "session");
    let tags = opt_str_array(args, "tags");

    let mut event = Event::new(source.clone(), parse_kind(&kind_raw), content.clone());
    event.workspace = Some(workspace.clone());
    event.title = title.clone();
    event.tags = tags;
    event.session = session;
    event.actor = Some(Actor {
        account: Some(ctx.identity.account.clone()),
        device: Some(ctx.identity.device.clone()),
        workspace: Some(workspace),
        node_id: Some(ctx.identity.node_id.clone()),
    });

    ctx.journal.append(&event)?;
    ctx.journal.flush()?;

    let label = title.unwrap_or_else(|| truncate_label(&content, 60));
    Ok(format!("Recorded [{}]: {}", source, label))
}

fn recent_activity(args: &Value, ctx: &ToolContext) -> Result<String> {
    let hours = opt_f64(args, "hours").unwrap_or(24.0);
    let source = opt_str(args, "source");
    let limit = opt_usize(args, "limit").unwrap_or(20);

    let events = ctx.journal.query_recent(hours, source.as_deref(), limit)?;
    let refs: Vec<&urchin_core::event::Event> = events.iter().collect();
    Ok(query::format_events(&refs))
}

fn project_context(args: &Value, ctx: &ToolContext) -> Result<String> {
    let project = required_str(args, "project")?;
    let hours = opt_f64(args, "hours").unwrap_or(168.0);
    let limit = opt_usize(args, "limit").unwrap_or(30);

    let events = ctx.journal.query_project(&project, hours, limit)?;
    let refs: Vec<&urchin_core::event::Event> = events.iter().collect();
    Ok(query::format_events(&refs))
}

fn search(args: &Value, ctx: &ToolContext) -> Result<String> {
    let query_str = required_str(args, "query")?;
    let hours = opt_f64(args, "hours").unwrap_or(168.0);
    let limit = opt_usize(args, "limit").unwrap_or(20);

    let events = ctx.journal.query_search(&query_str, hours, limit)?;
    let refs: Vec<&urchin_core::event::Event> = events.iter().collect();
    Ok(query::format_events(&refs))
}

fn workspace_context(args: &Value, ctx: &ToolContext) -> Result<String> {
    let path = required_str(args, "path")?;
    let hours = opt_f64(args, "hours").unwrap_or(168.0);
    let limit = opt_usize(args, "limit").unwrap_or(40);

    let events = ctx.journal.query_workspace(&path, hours, limit)?;
    if events.is_empty() {
        return Ok(format!("No events found for workspace: {}", path));
    }
    let refs: Vec<&urchin_core::event::Event> = events.iter().collect();
    Ok(format!(
        "Events for {}:\n\n{}",
        path,
        query::format_events(&refs)
    ))
}

fn remember(args: &Value, ctx: &ToolContext) -> Result<String> {
    if ctx.ephemeral.load(Ordering::Relaxed) {
        ctx.suppressed.fetch_add(1, Ordering::Relaxed);
        return Ok("(ephemeral mode: note suppressed)".to_string());
    }

    let content = required_str(args, "content")?;
    let tags = opt_str_array(args, "tags");
    let workspace = opt_str(args, "workspace");

    let mut event = Event::new("mcp", EventKind::Decision, content.clone());
    event.workspace = workspace.clone();
    event.tags = tags;
    event.actor = Some(Actor {
        account: Some(ctx.identity.account.clone()),
        device: Some(ctx.identity.device.clone()),
        workspace,
        node_id: Some(ctx.identity.node_id.clone()),
    });

    ctx.journal.append(&event)?;
    ctx.journal.flush()?;
    Ok(format!("Remembered: {}", truncate_label(&content, 80)))
}

fn ephemeral(args: &Value, ctx: &ToolContext) -> Result<String> {
    let action = required_str(args, "action")?;
    let file_mode = EphemeralMode::default();
    match action.as_str() {
        "start" => {
            ctx.ephemeral.store(true, Ordering::Relaxed);
            ctx.suppressed.store(0, Ordering::Relaxed);
            // Persist flag cross-process so urchin-intake also suppresses writes.
            if let Err(e) = file_mode.activate() {
                tracing::warn!("ephemeral: could not write flag file: {}", e);
            }
            Ok("Ephemeral mode ACTIVE — no events will be written until you call end.".to_string())
        }
        "end" => {
            ctx.ephemeral.store(false, Ordering::Relaxed);
            let n = ctx.suppressed.swap(0, Ordering::Relaxed);
            if let Err(e) = file_mode.deactivate() {
                tracing::warn!("ephemeral: could not remove flag file: {}", e);
            }
            Ok(format!(
                "Ephemeral mode ended. {} event(s) were suppressed and are permanently gone.",
                n
            ))
        }
        "status" => {
            let active = ctx.ephemeral.load(Ordering::Relaxed) || file_mode.is_active();
            let n = ctx.suppressed.load(Ordering::Relaxed);
            if active {
                Ok(format!(
                    "Ephemeral mode: ACTIVE ({} event(s) suppressed so far)",
                    n
                ))
            } else {
                Ok(
                    "Ephemeral mode: inactive — all events are being recorded normally."
                        .to_string(),
                )
            }
        }
        other => Err(anyhow::anyhow!(
            "unknown action '{}'; expected start | end | status",
            other
        )),
    }
}

fn parse_kind(s: &str) -> EventKind {
    match s {
        "agent" => EventKind::Agent,
        "command" => EventKind::Command,
        "commit" => EventKind::Commit,
        "file" => EventKind::File,
        "decision" => EventKind::Decision,
        "purchase" => EventKind::Purchase,
        "location" => EventKind::Location,
        "health_metric" => EventKind::HealthMetric,
        "calendar_event" => EventKind::CalendarEvent,
        "search_query" => EventKind::SearchQuery,
        "watch_history" => EventKind::WatchHistory,
        "conversation" => EventKind::Conversation,
        other => EventKind::Other(other.to_string()),
    }
}

fn truncate_label(s: &str, n: usize) -> String {
    let collapsed: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if collapsed.chars().count() <= n {
        collapsed
    } else {
        let mut out: String = collapsed.chars().take(n).collect();
        out.push('…');
        out
    }
}

fn required_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("missing required argument: {}", key))
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn opt_f64(args: &Value, key: &str) -> Option<f64> {
    args.get(key).and_then(|v| v.as_f64())
}

fn opt_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(|v| v.as_u64()).map(|n| n as usize)
}

fn opt_str_array(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn ctx_with_tmp_journal() -> (ToolContext, NamedTempFile) {
        let tmp = NamedTempFile::new().unwrap();
        let mut cfg = Config::default();
        cfg.journal_path = tmp.path().to_path_buf();
        let ctx = ToolContext {
            journal: Arc::new(Journal::new(tmp.path().to_path_buf())),
            identity: Arc::new(Identity::for_test("test", "test")),
            config: Arc::new(cfg),
            ephemeral: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            suppressed: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        (ctx, tmp)
    }

    #[test]
    fn status_on_empty_journal() {
        let (ctx, _tmp) = ctx_with_tmp_journal();
        let out = status(&ctx).unwrap();
        assert!(out.contains("events:   0"));
    }

    #[test]
    fn ingest_writes_and_search_finds() {
        let (ctx, _tmp) = ctx_with_tmp_journal();
        let args = json!({"content": "the quick brown fox", "workspace": "/tmp/wp"});
        let ack = ingest(&args, &ctx).unwrap();
        assert!(ack.starts_with("Recorded "));

        let found = search(&json!({"query": "quick"}), &ctx).unwrap();
        assert!(found.contains("the quick brown fox"));
    }

    #[test]
    fn recent_activity_filters_by_source() {
        let (ctx, _tmp) = ctx_with_tmp_journal();
        ingest(
            &json!({"content": "from claude", "workspace": "/w", "source": "claude"}),
            &ctx,
        )
        .unwrap();
        ingest(
            &json!({"content": "from shell",  "workspace": "/w", "source": "shell"}),
            &ctx,
        )
        .unwrap();

        let only_claude = recent_activity(&json!({"source": "claude"}), &ctx).unwrap();
        assert!(only_claude.contains("from claude"));
        assert!(!only_claude.contains("from shell"));
    }

    #[test]
    fn project_context_matches_by_workspace_path() {
        let (ctx, _tmp) = ctx_with_tmp_journal();
        ingest(
            &json!({"content": "a", "workspace": "/home/me/projects/urchin-rust"}),
            &ctx,
        )
        .unwrap();
        ingest(
            &json!({"content": "b", "workspace": "/home/me/projects/other"}),
            &ctx,
        )
        .unwrap();

        let out = project_context(&json!({"project": "urchin-rust"}), &ctx).unwrap();
        assert!(out.contains("— a"));
        assert!(!out.contains("— b"));
    }

    #[test]
    fn workspace_context_filters_by_path_prefix() {
        let (ctx, _tmp) = ctx_with_tmp_journal();
        ingest(
            &json!({"content": "inside", "workspace": "/home/me/dev/urchin"}),
            &ctx,
        )
        .unwrap();
        ingest(
            &json!({"content": "outside", "workspace": "/home/me/dev/other"}),
            &ctx,
        )
        .unwrap();

        let out = workspace_context(&json!({"path": "/home/me/dev/urchin"}), &ctx).unwrap();
        assert!(out.contains("inside"));
        assert!(!out.contains("outside"));
    }

    #[test]
    fn workspace_context_empty_returns_no_events_message() {
        let (ctx, _tmp) = ctx_with_tmp_journal();
        let out = workspace_context(&json!({"path": "/nonexistent/path"}), &ctx).unwrap();
        assert!(out.contains("No events found"));
    }

    #[test]
    fn remember_writes_event() {
        let (ctx, _tmp) = ctx_with_tmp_journal();
        let out = remember(
            &json!({"content": "store this idea", "tags": ["idea"]}),
            &ctx,
        )
        .unwrap();
        assert!(out.contains("store this idea"));

        let found = search(&json!({"query": "store this idea"}), &ctx).unwrap();
        assert!(found.contains("store this idea"));
    }

    #[test]
    fn ephemeral_lifecycle_suppresses_events() {
        let (ctx, _tmp) = ctx_with_tmp_journal();

        // Start ephemeral mode
        let start = ephemeral(&json!({"action": "start"}), &ctx).unwrap();
        assert!(start.contains("ACTIVE"));

        // Ingest + remember are suppressed
        let r1 = ingest(&json!({"content": "secret", "workspace": "/w"}), &ctx).unwrap();
        assert!(r1.contains("suppressed"));
        let r2 = remember(&json!({"content": "also secret"}), &ctx).unwrap();
        assert!(r2.contains("suppressed"));

        // Status shows 2 suppressed
        let s = ephemeral(&json!({"action": "status"}), &ctx).unwrap();
        assert!(s.contains("2"));

        // End — events are gone
        let end = ephemeral(&json!({"action": "end"}), &ctx).unwrap();
        assert!(end.contains("2 event(s) were suppressed"));

        // Journal is empty
        let journal = ctx.journal.read_all().unwrap();
        assert_eq!(journal.len(), 0);
    }
}
