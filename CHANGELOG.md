# Changelog

All notable changes to Urchin are documented in this file.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).
Versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [0.3.5] - 2026-05-19

### Removed

- `urchin-agent` crate: ReAct reasoning loop removed. Reasoning belongs in consumers, not the ingestion substrate.
- `urchin-vault` crate: vault projection removed. Writing to a vault is a consumer responsibility.
- `urchin_agent_reflect` and `urchin_semantic_search` MCP tools removed (8 tools remain).
- `urchin agent reflect` and `urchin vault project` CLI commands removed.

### Added

**Personal EventKind variants** (`urchin-core`)
- `Purchase`, `Location`, `HealthMetric`, `CalendarEvent`, `SearchQuery`, `WatchHistory`
- Serde representation changed from `lowercase` to `snake_case` (backwards-compatible for all existing single-word variants)

**EventMeta struct** (`urchin-core`)
- Optional structured fields: `amount`, `currency`, `merchant`, `category`, `lat`, `lng`, `value`, `unit`, `duration_secs`, `attendees`
- Added as `meta: Option<EventMeta>` on `Event`, omitted from JSON when absent

**Personal data connectors** (`urchin-collectors`)
- `google-takeout`: reads Location History/Records.json, Search and YouTube activity from `~/.local/share/urchin/imports/google-takeout/`
- `apple-health`: streaming XML parser via `quick-xml` for steps, heart rate, sleep, and workouts from `~/.local/share/urchin/imports/apple-health/export.xml`
- `bank-csv`: auto-detects Chase, BofA, and generic column layouts from `~/.local/share/urchin/imports/bank/*.csv`
- `calendar`: iCal VEVENT parser (handles folded lines, computes duration, counts attendees) from `~/.local/share/urchin/imports/calendar/*.ics`
- All four registered in `CollectorRegistry::with_defaults()` and accessible via `urchin collect <name>`

**Batch intake endpoint** (`urchin-intake`)
- `POST /ingest/batch`: accepts up to 1000 events per request
- Validates each event individually; partial success allowed
- Returns `{ "accepted": N, "dropped": N, "errors": [...] }`
- Ephemeral mode drops all. Auth required if token configured.

**Documentation**
- `CONNECTORS.md`: Collector trait, checkpoint patterns, connector table, how to add a connector
- `EVENTS.md`: full EventKind and EventMeta field reference
- `PRIVACY.md`: what is stored, what is not transmitted, ephemeral mode, deletion

**Bug fixes** (pre-existing)
- `codex.rs`: fixed `truncate` visibility (`fn` -> `pub(crate)`) and import in `codex.rs` tests
- `codex.rs` tests: added missing `history_path` field to `CodexOpts` initializers

---

## [0.3.4] - 2026-05-04

### Added

**Cross-process ephemeral mode (`urchin-core`)**
- `EphemeralMode` struct in `crates/urchin-core/src/ephemeral.rs`
- File-backed flag at `~/.local/share/urchin/ephemeral.lock`
- `activate()`: writes flag file; `deactivate()`: removes it (idempotent); `is_active()`: `path.exists()`
- `Default` impl resolves to standard data dir: callers need no config
- 3 unit tests: inactive by default, activate/deactivate roundtrip, deactivate idempotent

**Intake auth + ephemeral awareness (`urchin-intake`)**
- `AppState` gains `token: Option<String>` and `ephemeral: EphemeralMode`
- `POST /ingest` now enforces Bearer token auth (401 on mismatch when token configured)
- `POST /ingest` returns 202 + silently drops event when `ephemeral.lock` is present
- `POST /ingest` rejects blank `content` or `source` with 400 Bad Request
- `GET /health` adds `"ephemeral": bool`; journal path removed from response (was an info leak)
- 8 test cases covering all response codes

**Config `intake_token` field (`urchin-core`)**
- `Config` struct gains `intake_token: Option<String>` (default `None`)
- TOML key: `intake_token`; env var: `URCHIN_INTAKE_TOKEN`
- Env var overrides config file (same precedence pattern as all other fields)

**MCP ephemeral file flag (`urchin-mcp`)**
- `urchin_ephemeral {action:"start"}` now calls `EphemeralMode::default().activate()`
- `urchin_ephemeral {action:"end"}` now calls `EphemeralMode::default().deactivate()`
- `urchin_ephemeral {action:"status"}` reports active if in-process bool OR flag file is present
- Both the in-process `AtomicBool` (fast gate for MCP tool calls) and the file flag are set/cleared together

**Documentation**
- `docs/ARCHITECTURE.md`: crate map, process topology, Journal/EphemeralMode/Config internals, desktop integration
- `docs/API_REFERENCE.md`: `POST /ingest` full schema, all response codes, JSONL format, SDK usage

### Changed

**Journal write safety (`urchin-core`)**
- `Journal` struct gains `write_lock: std::sync::Mutex<()>`
- `append()` acquires the lock before `OpenOptions::open + writeln!`: prevents intra-process line interleaving
- `Journal::new()` initialises the lock; external API unchanged

### Test counts

| Crate               | Tests         |
|---------------------|---------------|
| `urchin-core`       | 10 (+3 ephemeral) |
| `urchin-intake`     | 8 (+6)        |
| `urchin-mcp`        | 20 (+3)       |
| `urchin-collectors` | 52            |
| `urchin-vault`      | 3             |
| `urchin-agent`      | 15            |
| **Total**           | **108** (+12) |

---

## [0.3.1]: 2025-07-05

### Added

**`Reasoner` trait in `urchin-agent`**
- `Reasoner: Send + Sync` trait: pluggable LLM backend for the reflection loop
- `EchoReasoner`: deterministic (no network), used in all tests, default when no endpoint is configured
- `HttpReasoner`: Ollama-compat `/api/generate` HTTP backend
  - Reads `URCHIN_REASONER_URL` + `URCHIN_REASONER_MODEL` env vars at construction
  - Builds a structured prompt (goal + context) and returns `response.response`
  - Falls back silently to `EchoReasoner` when URL is not set
- `reflect::synthesise` now takes `&dyn Reasoner`: deterministic fallback when reasoner errors
- `Agent::new()` selects backend from env: `HttpReasoner` if URL set, `EchoReasoner` otherwise
- `ureq = "2"` dep added (sync HTTP, no tokio in agent crate)

### Test counts

| Crate              | Tests |
|--------------------|-------|
| `urchin-core`      | 7     |
| `urchin-intake`    | 2     |
| `urchin-mcp`       | 17    |
| `urchin-collectors`| 52    |
| `urchin-vault`     | 3     |
| `urchin-agent`     | **15** (+4) |
| **Total**          | **96** |

---

## [0.3.0]: 2025-07-05

### Added

**MCP hardening for daily Cursor/Zed use**
- Committed `.cursor/mcp.json`: drop this in any repo and Cursor picks up the 9-tool server automatically
- README IDE Setup section: config blocks for Cursor, Zed, VS Code
- MCP server test renamed to `tools_list_returns_nine_tools` to stay in sync

**`urchin-agent` skeleton crate** (`crates/urchin-agent/`)
- `Agent` struct + `AgentConfig` builder (`with_hours`, `with_limit`)
- `context::load()`: time-window + count filter over journal events
- `context::format_context()`: renders structured context block
- `reflect::synthesise()`: deterministic text pass (Phase 2 ReAct; Phase 4 slot reserved for LLM backend)
- `reflect::to_event()`: wraps reflection as `EventKind::Agent` journal event
- `run()` full loop: load → reflect → append back to journal
- 11 tests

**`urchin agent reflect` CLI subcommand**
- `urchin agent reflect "<goal>" --hours <f> --limit <n>`
- Dispatches through `agent_cmd()` in `urchin-cli`

**`urchin_agent_reflect` as 9th MCP tool**
- Exposed via MCP stdio: `{"goal": "...", "hours": 24, "limit": 30}`
- Writes the reflection back as an `Agent` event in the journal
- 17 MCP tests total (was 16)

### Test counts

| Crate              | Tests |
|--------------------|-------|
| `urchin-core`      | 7     |
| `urchin-intake`    | 2     |
| `urchin-mcp`       | 17    |
| `urchin-collectors`| 52    |
| `urchin-vault`     | 3     |
| `urchin-agent`     | 11    |
| **Total**          | **92**|

---

## [0.2.0]: 2026-07-04

### Added

**Collector trait + registry**
- Object-safe `Collector` trait: `name()`, `collect()`, `is_available()`
- `CollectorRegistry::with_defaults(repo_roots)`: wires all 8 collectors, skips unavailable
- `run_all()` returns per-collector results with name + count/error
- Adding a new collector = one `impl Collector` struct; no changes to daemon or dispatch

**Codex collector**
- Source: `~/.codex/state_5.sqlite` (`threads` table)
- Captures `first_user_message` as user intent (falls back to `title`)
- Skips archived sessions and slash-command titles (`/clear`, etc.)
- Checkpoint: JSON `{ last_ts_ms }` watermark

**OpenCode collector**
- Source: `~/.local/share/opencode/opencode.db` (`message` JOIN `session`)
- Filters for `role=user` messages only
- Extracts text from three content formats: `parts[].text`, `content` string, `content[].text` blocks
- Checkpoint: JSON `{ last_ts_ms }` watermark

**Local model collector**
- Source: `~/.local/share/urchin/local-model.jsonl` (opt-in drop file)
- Any local inference harness (Ollama, LM Studio, llama.cpp) can append JSONL records
- Fields: `prompt` (required), `model` (optional tag), `ts` (RFC3339, optional), `workspace` (optional)
- Checkpoint: byte-offset (same mechanism as shell collector)
- `is_available()` returns false when drop file is absent: zero noise

**CLI subcommands**
- `urchin collect codex`: run Codex collector
- `urchin collect opencode`: run OpenCode collector
- `urchin collect local-model`: run local model collector
- All three included in `urchin collect all` via registry

**Documentation**
- `ROADMAP.md`: architectural contract encoding Context OS phases 0–6
- `SOVEREIGNTY.md`: four sovereignty mandates as binding spec
- `.urchinignore.example`: sensible defaults (secrets, .env, gpg, pass)
- `README.md` updated: 74 tests, 8 collectors, new commands, local model format

### Changed

- Version bumped `0.1.0` → `0.2.0` across all 7 crates
- `urchin-collectors/Cargo.toml` gained `rusqlite = { version = "0.31", features = ["bundled"] }`: no system libsqlite3 required

### Test count

| Crate | Tests |
|---|---|
| urchin-core | 7 |
| urchin-intake | 2 |
| urchin-mcp | 10 |
| urchin-collectors | 52 |
| urchin-vault | 3 |
| **Total** | **74** |

---

## [0.1.0-alpha]: 2026-05-03

### Added

- `urchin-core`: `Event`, `Journal`, `Identity`, `Config`, `EventKind`, `Actor`
- `urchin-intake`: `POST /ingest`, `GET /health`, binds to `127.0.0.1:18799`
- `urchin-mcp`: MCP over stdio (JSON-RPC 2.0), 5 tools
  - `urchin_status`, `urchin_ingest`, `urchin_recent_activity`, `urchin_project_context`, `urchin_search`
- `urchin-collectors`: shell, git, claude, copilot, gemini (5 live collectors)
- `urchin-vault`: vault projection into `~/brain/daily/YYYY-MM-DD.md` inside marker guards
- `urchin-sdk`: HTTP client for daemon + cloud hub
- `urchin-cli`: single binary `urchin` with doctor, ingest, serve, mcp, collect, recent, query, vault, sync
- Daemon mode (`urchin serve`): tokio runtime, collector tick loop
- Cloud sync: `urchin sync` → `orinadus.com/api/urchin-sync`
- Systemd user service
