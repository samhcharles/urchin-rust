# AGENTS.md — Urchin Rust

This file is the context document for any AI agent working in this repo.
Read this first. It tells you what Urchin is, what the rules are, and where to start.

---

## Who you are working with

**Samuel** — founder of Orinadus. Non-coder. Works entirely through AI agents.
Explain decisions plainly. No jargon without definition. Infrastructure thinking, not app thinking.

---

## What Urchin is

A local-first memory sync substrate. Every AI tool a developer uses — Claude, Copilot, Gemini,
Codex, VS Code, shell — works in isolation. Urchin runs as a background daemon, collects activity
from all of them, normalizes it into a canonical append-only journal, and makes that journal
queryable by any tool via MCP and HTTP.

**One sentence: Urchin gives every tool the same memory.**

## Positioning rule — memorize this

Urchin does not own your tools. It connects them.
It is additive. Nobody loses anything. Every tool you already use gets better.
The substrate earns its place by being useful, not by creating switching costs.
We unify. We do not replace. We do not trap.

---

## Architecture

```
crates/
  urchin-core        pure types: Event, Journal, Identity, Config
  urchin-intake      axum HTTP server — POST /ingest
  urchin-mcp         MCP over stdio — 5 tools
  urchin-collectors  passive readers: claude, copilot, gemini, shell, git
  urchin-vault       Obsidian vault writer — marker blocks + _urchin/ namespace only
  urchin-cli         single binary: serve | mcp | doctor | ingest
```

### Event model (urchin-core)

The `Event` struct is the canonical unit of memory:
- `id` — UUID v4
- `timestamp` — UTC
- `source` — "claude" | "copilot" | "gemini" | "shell" | "git" | "agent" | "cli"
- `kind` — Conversation | Agent | Command | Commit | File | Other
- `content` — the actual memory payload
- `workspace`, `session`, `title`, `tags` — optional context
- `actor` — account + device identity envelope

### Journal (urchin-core)

Append-only JSONL at `~/.local/share/urchin/journal/events.jsonl`.
Events are written once, never mutated. This is the source of truth.

### Collectors (urchin-collectors)

One module per source. Each reads from the tool's native output.
**Collectors are passive — they read, they never write to source tools.**

| Module | Reads from |
|---|---|
| claude | `~/.claude/history.jsonl`, project transcripts |
| copilot | `~/.copilot/session-state/` |
| gemini | `~/.gemini/tmp/*/chats/*.json` |
| shell | `~/.bash_history` |
| git | commit history across repo roots |
| agent_bridge | generic JSONL queue at `URCHIN_AGENT_EVENTS_PATH` |

### MCP tools

Five tools. These must be implemented and must match what the Node.js spike exposes:

| Tool | Purpose |
|---|---|
| `urchin_status` | daemon health, event counts, last sync |
| `urchin_ingest` | write an event into the journal |
| `urchin_recent_activity` | recent events, filterable by source/hours/limit |
| `urchin_project_context` | events scoped to a project/workspace |
| `urchin_search` | full-text search over events |

### Vault writer (urchin-vault)

The vault contract lives at `~/brain/_urchin/README.md` (YAML frontmatter).
It defines: `vault_root`, `writeable_roots`, `projection_roots`, `archive_root`, `marker_prefix`.

**Write rules (non-negotiable):**
1. Only write inside `<!-- URCHIN:*:START -->` / `<!-- URCHIN:*:END -->` marker blocks in human notes
2. Never touch human content outside those markers
3. `_urchin/` namespace is machine-owned — write freely there
4. If `custom_field` or unknown frontmatter exists, preserve it exactly
5. If wikilinks or aliases exist in a note, they must survive any write
6. Writes must be idempotent — running twice must produce the same result

---

## Test brain

The live Obsidian vault at `~/brain` is the test surface for this build.
Test surfaces are at `~/brain/_urchin/test-surfaces/`:

| File | Tests |
|---|---|
| `intake-events.md` | intake and dedup |
| `projection-block.md` | marker block write-back (has human text that must survive) |
| `frontmatter.md` | frontmatter round-trip (has `custom_field` that must survive) |
| `links.md` | wikilink and alias preservation |
| `move-rename/source.md` | move/rename behavior |
| `archive/source.md` | archive-as-path-move |

---

## The Node.js spike (reference)

The working prototype is at `~/dev/urchin` and `github.com/samhcharles/urchin`.
It has 30 source files, 444 events in journal, MCP working, HTTP intake working.
Use it as a behavioral reference — but do NOT copy its architecture.
The Rust rewrite is a clean implementation, not a port.

Key things the spike proved work:
- MCP over stdio with 5 tools
- HTTP intake on a fixed port
- Append-only journal with provenance
- Identity envelope (actor/account/device/workspace)
- Collector pattern for claude, copilot, gemini, shell, git

Key things to leave behind from the spike:
- Single-threaded sync loop
- No separation between journal and cache
- Vault writer mixed into the sync path
- No proper error recovery or retry

---

## Infrastructure

- **WSL** — primary build surface, this is where `~/dev/urchin-rust` lives
- **VPS** — Coolify-managed, SSH alias `srv`, no direct code deploy
- **Obsidian vault** — `~/brain`, syncs to Windows via Obsidian Sync
- **GitHub** — `samhcharles/urchin-rust` (this repo), `samhcharles/urchin` (Node.js spike)

---

## Rules for this build

1. `urchin-core` must have zero I/O — only types, serialization, and pure logic
2. All async runtime is tokio
3. Single binary output: `cargo build` → `target/debug/urchin`
4. Config comes from `~/.config/urchin/config.toml` + env var overrides
5. Errors use `anyhow` for the binary, `thiserror` for library crates (add when needed)
6. No panics in library code — return `Result<_>`
7. The MCP server must be stdio-based (not HTTP) — this is how Claude Code and VS Code wire it
8. Keep it simple. Do not over-engineer before the basics work.

---

## Where to start

Phase 1 — get `urchin doctor` and `urchin ingest` working end-to-end:
1. Flesh out `urchin-core`: Event, Journal, Config, Identity (mostly done in scaffold)
2. Wire `urchin-cli` ingest command to actually write to the journal
3. Wire `urchin-cli` doctor to show real status (journal size, config paths)
4. Add basic tests for Event serialization and Journal append/read

Phase 2 — HTTP intake:
5. Implement `urchin-intake` server with POST /ingest and GET /health
6. Wire intake into `urchin serve`

Phase 3 — MCP server:
7. Implement `urchin-mcp` with all 5 tools over stdio
8. Wire into `urchin mcp`

Phase 4 — collectors:
9. Implement shell and git collectors first (simplest)
10. Then claude, copilot, gemini

Phase 5 — vault projection:
11. Read the vault contract from `_urchin/README.md`
12. Implement marker block writer
13. Test against `~/brain/_urchin/test-surfaces/`
