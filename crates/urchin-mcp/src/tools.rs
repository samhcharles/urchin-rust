/// Tool schemas and execution for the MCP server.
/// Each tool takes a Value argument map, reads/writes the journal, returns a text block.

use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};

use urchin_core::{
    config::Config,
    event::{Actor, Event, EventKind},
    identity::Identity,
    journal::Journal,
};

pub struct ToolContext {
    pub journal:  Arc<Journal>,
    pub identity: Arc<Identity>,
    pub config:   Arc<Config>,
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
                    "kind":      { "type": "string",  "description": "conversation | agent | command | commit | file. Defaults to conversation." },
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
        }
    ])
}

pub fn call(name: &str, args: &Value, ctx: &ToolContext) -> Result<String> {
    match name {
        "urchin_status"          => status(ctx),
        "urchin_ingest"          => ingest(args, ctx),
        "urchin_recent_activity" => recent_activity(args, ctx),
        "urchin_project_context" => project_context(args, ctx),
        "urchin_search"          => search(args, ctx),
        other => Err(anyhow::anyhow!("unknown tool: {}", other)),
    }
}

fn status(ctx: &ToolContext) -> Result<String> {
    let stats = ctx.journal.stats()?;
    let mut out = String::new();
    out.push_str("urchin — local memory sync substrate\n\n");
    out.push_str(&format!("running:  true\n"));
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
    out.push_str(&format!("journal:  {}\n", ctx.config.journal_path.display()));
    out.push_str(&format!("intake:   {}\n", ctx.config.intake_port));
    out.push_str(&format!("vault:    {}\n", ctx.config.vault_root.display()));
    out.push_str(&format!("account:  {}\n", ctx.identity.account));
    out.push_str(&format!("device:   {}\n", ctx.identity.device));
    Ok(out)
}

fn ingest(args: &Value, ctx: &ToolContext) -> Result<String> {
    let content   = required_str(args, "content")?;
    let workspace = required_str(args, "workspace")?;
    let source    = opt_str(args, "source").unwrap_or_else(|| "mcp".to_string());
    let title     = opt_str(args, "title");
    let kind_raw  = opt_str(args, "kind").unwrap_or_else(|| "conversation".to_string());
    let session   = opt_str(args, "session");
    let tags      = opt_str_array(args, "tags");

    let mut event = Event::new(source.clone(), parse_kind(&kind_raw), content.clone());
    event.workspace = Some(workspace.clone());
    event.title     = title.clone();
    event.tags      = tags;
    event.session   = session;
    event.actor = Some(Actor {
        account:   Some(ctx.identity.account.clone()),
        device:    Some(ctx.identity.device.clone()),
        workspace: Some(workspace),
    });

    ctx.journal.append(&event)?;

    let label = title.unwrap_or_else(|| truncate(&content, 60));
    Ok(format!("Recorded [{}]: {}", source, label))
}

fn recent_activity(args: &Value, ctx: &ToolContext) -> Result<String> {
    let hours       = opt_f64(args, "hours").unwrap_or(24.0);
    let source      = opt_str(args, "source");
    let limit       = opt_usize(args, "limit").unwrap_or(20);
    let cutoff      = Utc::now() - Duration::milliseconds((hours * 3_600_000.0) as i64);

    let events = ctx.journal.read_all()?;
    let filtered: Vec<&Event> = events
        .iter()
        .filter(|e| e.timestamp >= cutoff)
        .filter(|e| source.as_deref().map(|s| e.source == s).unwrap_or(true))
        .collect();

    Ok(format_events(filtered, limit))
}

fn project_context(args: &Value, ctx: &ToolContext) -> Result<String> {
    let project = required_str(args, "project")?.to_lowercase();
    let hours   = opt_f64(args, "hours").unwrap_or(168.0);
    let limit   = opt_usize(args, "limit").unwrap_or(30);
    let cutoff  = Utc::now() - Duration::milliseconds((hours * 3_600_000.0) as i64);

    let events = ctx.journal.read_all()?;
    let filtered: Vec<&Event> = events
        .iter()
        .filter(|e| e.timestamp >= cutoff)
        .filter(|e| {
            e.content.to_lowercase().contains(&project)
                || e.tags.iter().any(|t| t.to_lowercase().contains(&project))
                || e.workspace.as_deref().map(|w| w.to_lowercase().contains(&project)).unwrap_or(false)
        })
        .collect();

    Ok(format_events(filtered, limit))
}

fn search(args: &Value, ctx: &ToolContext) -> Result<String> {
    let query  = required_str(args, "query")?.to_lowercase();
    let hours  = opt_f64(args, "hours").unwrap_or(168.0);
    let limit  = opt_usize(args, "limit").unwrap_or(20);
    let cutoff = Utc::now() - Duration::milliseconds((hours * 3_600_000.0) as i64);

    let events = ctx.journal.read_all()?;
    let filtered: Vec<&Event> = events
        .iter()
        .filter(|e| e.timestamp >= cutoff)
        .filter(|e| e.content.to_lowercase().contains(&query))
        .collect();

    Ok(format_events(filtered, limit))
}

fn format_events(events: Vec<&Event>, limit: usize) -> String {
    // Newest first
    let mut sorted: Vec<&Event> = events;
    sorted.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    let take: Vec<&&Event> = sorted.iter().take(limit).collect();

    if take.is_empty() {
        return "(no matching events)".to_string();
    }

    take.iter()
        .map(|e| format_event_line(e))
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_event_line(e: &Event) -> String {
    let ts = e.timestamp.format("%Y-%m-%dT%H:%M:%SZ");
    let content = truncate(&e.content, 120);
    format!("[{}] {} — {}", ts, e.source, content)
}

fn truncate(s: &str, n: usize) -> String {
    let collapsed: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if collapsed.chars().count() <= n {
        collapsed
    } else {
        let mut out: String = collapsed.chars().take(n).collect();
        out.push('…');
        out
    }
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

fn required_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("missing required argument: {}", key))
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
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

// Keep an internal ref to DateTime so unused-import warnings don't happen if chrono shape changes.
#[allow(dead_code)]
fn _touch_datetime(_t: DateTime<Utc>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn ctx_with_tmp_journal() -> (ToolContext, NamedTempFile) {
        let tmp = NamedTempFile::new().unwrap();
        let mut cfg = Config::default();
        cfg.journal_path = tmp.path().to_path_buf();
        let ctx = ToolContext {
            journal:  Arc::new(Journal::new(tmp.path().to_path_buf())),
            identity: Arc::new(Identity { account: "test".into(), device: "test".into() }),
            config:   Arc::new(cfg),
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
        ingest(&json!({"content": "from claude", "workspace": "/w", "source": "claude"}), &ctx).unwrap();
        ingest(&json!({"content": "from shell",  "workspace": "/w", "source": "shell"}),  &ctx).unwrap();

        let only_claude = recent_activity(&json!({"source": "claude"}), &ctx).unwrap();
        assert!(only_claude.contains("from claude"));
        assert!(!only_claude.contains("from shell"));
    }

    #[test]
    fn project_context_matches_by_workspace_path() {
        let (ctx, _tmp) = ctx_with_tmp_journal();
        ingest(&json!({"content": "a", "workspace": "/home/me/projects/urchin-rust"}), &ctx).unwrap();
        ingest(&json!({"content": "b", "workspace": "/home/me/projects/other"}),        &ctx).unwrap();

        let out = project_context(&json!({"project": "urchin-rust"}), &ctx).unwrap();
        assert!(out.contains("— a"));
        assert!(!out.contains("— b"));
    }
}
