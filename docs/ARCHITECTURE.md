# Urchin Substrate: Architecture

> This document describes the production state of the urchin-rust workspace as of v0.3.4.
> It covers crate responsibilities, inter-process data flows, write-safety invariants,
> Bearer auth, and ephemeral mode state propagation.

---

## Crate map

```
crates/
├── urchin-core         Library. No I/O except journal reads/writes and config loading.
│                       All other crates depend on this. Defines: Event, Journal,
│                       Config, Identity, EphemeralMode, query functions.
│
├── urchin-intake       Binary. HTTP daemon on 127.0.0.1:18799 (Axum 0.8 / Tokio).
│                       Single ingestion surface for SDK and external callers.
│                       Validates → auth-checks → ephemeral-checks → writes.
│
├── urchin-mcp          Binary. MCP server over stdio (JSON-RPC 2.0).
│                       10 tools exposed to IDE integrations (Cursor, Zed, VS Code).
│                       Shares journal with intake via filesystem: NOT via IPC.
│
├── urchin-collectors   Library + sub-binaries. Pull collectors for shell, git,
│                       claude, copilot, gemini, codex, opencode, local-model.
│                       Each runs checkpoint-gated sweeps and appends to journal.
│
├── urchin-agent        Library. Reflection loop: load context → synthesise →
│                       write Agent event. Pluggable Reasoner trait (EchoReasoner
│                       default; HttpReasoner for Ollama-compat endpoints).
│
├── urchin-vault        Library. Projects events into ~/brain/daily/YYYY-MM-DD.md
│                       inside idempotent marker-guarded blocks.
│
├── urchin-sdk          Library. HTTP client for urchin-intake + Orinadus cloud hub.
│                       urchin-desktop uses this to fire events from Tauri commands.
│
└── urchin-cli          Binary `urchin`. Unified CLI: doctor, ingest, serve, mcp,
                        collect, recent, query, vault, sync, agent.
```

---

## Process topology

```
┌─────────────────────────────────────────────────────────────────────────┐
│ urchin-desktop (Tauri 2.x + Next.js 16)                                 │
│  - Glass WebViews (ChatGPT, Claude, Gemini, …) inject JS capture hooks  │
│  - Tauri Rust commands → urchin-sdk → POST /ingest                       │
└────────────────────────────────┬────────────────────────────────────────┘
                                 │ HTTP POST /ingest
                                 │ Authorization: Bearer <token>
                                 ▼
┌────────────────────────────────────────────────────────────────────────┐
│ urchin-intake (127.0.0.1:18799)                                        │
│  AppState { journal: Arc<Journal>, token: Option<String>,              │
│             ephemeral: EphemeralMode }                                  │
│                                                                        │
│  POST /ingest pipeline:                                                │
│    1. Auth check  → 401 if token mismatch                              │
│    2. Ephemeral   → 202 + silent drop if flag file present             │
│    3. Validation  → 400 if content or source empty                     │
│    4. Journal write (mutex-guarded)  → 200 { id, status: "ok" }       │
│                                                                        │
│  GET /health → { status, events, ephemeral }                           │
└────────────────────────────────┬───────────────────────────────────────┘
                                 │ fs::OpenOptions::append
                                 ▼
┌────────────────────────────────────────────────────────────────────────┐
│ Journal  ~/.local/share/urchin/journal/events.jsonl                    │
│  One JSONL line per event. Append-only, never mutated.                 │
│  Protected by write_lock: std::sync::Mutex<()>                         │
└────────────────────────────────┬───────────────────────────────────────┘
                                 │ read_all() / read_tail()
                         ┌───────┴───────────────────────────────┐
                         │                                       │
                         ▼                                       ▼
           ┌─────────────────────┐             ┌────────────────────────────┐
           │ urchin-mcp (stdio)  │             │ urchin-collectors (cron)   │
           │  10 tools           │             │  8 collectors              │
           │  writes via         │             │  checkpoint-gated sweeps   │
           │  Journal::append()  │             │  write via Journal::append │
           └─────────────────────┘             └────────────────────────────┘
```

---

## urchin-core

### Journal

`crates/urchin-core/src/journal.rs`

```rust
pub struct Journal {
    path: PathBuf,
    write_lock: std::sync::Mutex<()>,
}
```

**Thread safety:** `write_lock` is a std (not tokio) Mutex held only for the duration of
`OpenOptions::append + writeln!`. Two concurrent calls to `append()` within the same process will
block rather than interleave partial JSON. The lock is NOT held across `.await` points because
`append()` is synchronous: there is no `Send` issue with `Arc<Journal>` in async contexts.

**Cross-process:** Multiple OS processes (intake + mcp + collectors) each open the file via
`O_APPEND`. Linux guarantees atomicity for small writes (≤ PIPE_BUF ≈ 4KB) to O_APPEND files on
local filesystems. For the typical JSONL line size this is safe. The in-process Mutex prevents the
one case where it is not: two threads in the same process racing on the same file handle.

**Public API:**
```rust
Journal::new(path: PathBuf) -> Journal
Journal::default_path() -> PathBuf          // ~/.local/share/urchin/journal/events.jsonl
journal.append(&event) -> Result<()>        // mutex-guarded
journal.read_all() -> Result<Vec<Event>>    // full scan, O(n)
journal.read_tail(n) -> Result<Vec<Event>>  // reverse scan, O(n tail bytes)
journal.stats() -> Result<JournalStats>     // event_count, file_size_bytes, last_event
journal.path() -> &PathBuf
journal.exists() -> bool
```

### EphemeralMode

`crates/urchin-core/src/ephemeral.rs`

File-backed boolean flag. Path: `~/.local/share/urchin/ephemeral.lock`.

```rust
pub struct EphemeralMode { flag_path: PathBuf }

impl EphemeralMode {
    pub fn new(data_dir: &PathBuf) -> Self
    pub fn is_active(&self) -> bool      // path.exists()
    pub fn activate(&self) -> io::Result<()>    // fs::write(path, "1")
    pub fn deactivate(&self) -> io::Result<()>  // fs::remove_file(path), no-op if absent
}

impl Default for EphemeralMode { /* resolves to ~/.local/share/urchin */ }
```

**Cross-process state flow:**

```
urchin-mcp (stdio)          filesystem                 urchin-intake (HTTP)
───────────────────          ──────────────             ────────────────────
urchin_ephemeral {           ephemeral.lock             on each POST /ingest:
  action: "start"   ──────► fs::write("1")  ◄────────  EphemeralMode::is_active()
}                                                       → true → 202 drop
                             (file exists)
urchin_ephemeral {
  action: "end"     ──────► fs::remove_file ◄────────  EphemeralMode::is_active()
}                                                       → false → write proceeds
```

The MCP process also maintains an in-process `AtomicBool` (`ToolContext.ephemeral`) for fast-path
suppression of `urchin_ingest` calls within the MCP process itself. Both are set/cleared together
in `tools.rs::ephemeral()`. The file flag is authoritative for cross-process queries.

### Config

`crates/urchin-core/src/config.rs`

Load order (later layers win):
1. `~/.config/urchin/config.toml` (TOML, all keys optional)
2. Environment variables

| TOML key         | Env var                  | Default                                          |
|------------------|--------------------------|--------------------------------------------------|
| `vault_root`     | `URCHIN_VAULT_ROOT`      | `~/brain`                                        |
| `journal_path`   | `URCHIN_JOURNAL_PATH`    | `~/.local/share/urchin/journal/events.jsonl`     |
| `cache_path`     |:                        | `~/.local/share/urchin/event-cache.jsonl`        |
| `intake_port`    | `URCHIN_INTAKE_PORT`     | `18799`                                          |
| `cloud_url`      | `URCHIN_CLOUD_URL`       | `None`                                           |
| `cloud_token`    | `URCHIN_CLOUD_TOKEN`     | `None`                                           |
| `intake_token`   | `URCHIN_INTAKE_TOKEN`    | `None` (auth disabled)                           |

When `intake_token` is `None`, the intake server accepts all requests: safe because it binds
loopback only. Set the token for any multi-user or networked environment.

### Event

`crates/urchin-core/src/event.rs`

The canonical memory unit. Serialises to one JSONL line per event. See `API_REFERENCE.md` for the
full field specification and wire format.

---

## urchin-intake

`crates/urchin-intake/src/server.rs`

Axum 0.8 HTTP server. Binds `127.0.0.1:<cfg.intake_port>` (default `18799`). Two routes:

### `POST /ingest`

Handler execution order (strict):
1. **Axum JSON extractor**: if body is not valid JSON mapping to `Event`, returns `422 Unprocessable Entity` before handler runs.
2. **Bearer auth**: if `state.token` is `Some(t)`, requires `Authorization: Bearer <t>`. Wrong or absent → `401 Unauthorized`.
3. **Ephemeral check**: if `EphemeralMode::is_active()`, returns `202 Accepted` with `{"status": "dropped"}`. Event is permanently discarded.
4. **Payload validation**: `content.trim().is_empty()` → `400`. `source.trim().is_empty()` → `400`.
5. **Journal write**: `journal.append(&event)` (mutex-guarded). Returns `200 {"id": ..., "status": "ok"}` or `500` on write error.

### `GET /health`

Returns `{ "status": "ok", "events": <count>, "ephemeral": <bool> }`. Does not expose journal path.

### AppState

```rust
pub struct AppState {
    pub journal:      Arc<Journal>,
    pub journal_path: PathBuf,
    pub identity:     Arc<Identity>,
    pub token:        Option<String>,   // None = auth disabled
    pub ephemeral:    EphemeralMode,    // file-backed cross-process flag
}
```

`AppState` is `Clone` via `Arc<Journal>` + `Arc<Identity>`: Axum clones it per request.
`EphemeralMode::clone()` clones the `PathBuf`; every clone checks the same file.

---

## urchin-mcp

`crates/urchin-mcp/src/tools.rs`

10 tools over JSON-RPC 2.0 stdio:

| Tool                       | Side effects                                              |
|----------------------------|-----------------------------------------------------------|
| `urchin_status`            | Read-only journal stats                                   |
| `urchin_ingest`            | `journal.append()`: suppressed during ephemeral mode     |
| `urchin_remember`          | `journal.append()`: suppressed during ephemeral mode     |
| `urchin_recent_activity`   | `journal.read_tail()`                                     |
| `urchin_search`            | Full scan with substring match                            |
| `urchin_semantic_search`   | Token cosine or vector (if `URCHIN_EMBEDDER_URL` set)     |
| `urchin_workspace_context` | Path-prefix filter over full journal                      |
| `urchin_project_context`   | Project name filter over full journal                     |
| `urchin_agent_reflect`     | Load context → Reasoner → append Agent event              |
| `urchin_ephemeral`         | Writes/removes `ephemeral.lock`; toggles in-process bool  |

**Ephemeral suppression in MCP:** `ToolContext.ephemeral: Arc<AtomicBool>` gates `urchin_ingest`
and `urchin_remember` inside the MCP process. When `urchin_ephemeral {action:"start"}` is called:
- `AtomicBool` → `true` (fast in-process gate)
- `EphemeralMode::default().activate()` → writes `ephemeral.lock` (cross-process gate for intake)

When `urchin_ephemeral {action:"end"}` is called both are cleared. The `"status"` action reads
both: in-process bool OR file existence → reports active if either is true.

---

## urchin-desktop integration

`urchin-desktop` is a Tauri 2.x app. Its Rust side (`src-tauri/src/lib.rs`) uses `urchin-sdk` to
fire events into `urchin-intake` via HTTP:

```
[Glass WebView JS injection]
  → postMessage capture hook
  → Tauri command ingest_glass_capture(content, silo_id)
  → urchin_sdk::Client::ingest(event)
  → POST http://127.0.0.1:18799/ingest
  → urchin-intake validates + writes
  → events.jsonl
```

The desktop app runs as a separate OS process. It does not link against urchin-core directly. All writes go through HTTP so intake auth and ephemeral mode apply uniformly.

---

## Data directory layout

```
~/.local/share/urchin/
├── journal/
│   └── events.jsonl          ← append-only JSONL, write_lock guarded
├── ephemeral.lock            ← exists = ephemeral active; absent = inactive
├── event-cache.jsonl         ← transient SDK write-through cache
└── checkpoints/              ← per-collector byte-offset watermarks
    ├── shell.json
    ├── git.json
    └── ...

~/.config/urchin/
└── config.toml               ← optional partial config, all keys optional
```
