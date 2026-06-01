# Urchin Roadmap

> Engineer the universal substrate that synchronizes human intent with autonomous execution,
> reducing the distance between thought and reality to zero.

This is the architectural contract. Every phase describes what Urchin is building toward.
Future commits inherit this direction; nothing here is a marketing document.

---

## Phase 0: Foundation ✅

**Status:** done and stable

- Daemon binary (`urchin serve`) with tokio runtime
- Canonical journal: append-only JSONL, `Event` schema with source / kind / content / timestamp / workspace / session / actor / tags
- `Identity` envelope: account + device on every event
- HTTP intake: `POST /ingest`, `GET /health`: 127.0.0.1:18799 only
- MCP server (stdio, JSON-RPC 2.0): 5 tools: status, ingest, recent_activity, project_context, search
- TOML config + env overrides, XDG-compliant paths
- Cloud sync: shuttle pattern, `urchin sync` → orinadus.com/api/urchin-sync

---

## Phase 1: Collector Network ✅

**Status:** done and stable

The perimeter sensors. Each collector reads one data stream from the OS, extracts
semantic intent, and writes normalized Events into the journal.

| Collector | Source | Checkpoint |
|---|---|---|
| Shell | `~/.bash_history` | byte-offset |
| Git | any git repo (via `URCHIN_REPO_ROOTS` or `--repo`) | per-repo SHA |
| Claude | `~/.claude/projects/**/*.jsonl` | byte-offset |
| Copilot | `~/.copilot/command-history-state.json` | content-addressed (seen set) |
| Gemini | `~/.gemini/tmp/*/chats/*.jsonl` | partial-offset JSON |

Vault projection: `urchin vault project` writes a structured urchin block into
`~/brain/daily/YYYY-MM-DD.md` inside `<!-- URCHIN:* -->` marker guards.
Human content is never touched.

---

## Phase 2: Collector Trait + Agent Skeleton ✅

**Status:** done and stable  
**Tag:** `v0.3.0`  
**Commits:** `91716e9` (trait/registry/rusqlite), `50237ec` (codex), `05c9e84` (opencode), `5d48dd8` (local-model), `be2ba80` (MCP hardening + IDE setup docs), `02e5bed` (urchin-agent skeleton + CLI), `f862888` (urchin_agent_reflect MCP tool)

Object-safe `Collector` trait + `CollectorRegistry::with_defaults()`. Any new data source
becomes one `impl Collector` struct: no changes to daemon or dispatch logic.

| Collector | Source | Notes |
|---|---|---|
| Codex | `~/.codex/state_5.sqlite` threads | `first_user_message` as intent; watermark checkpoint |
| OpenCode | `~/.local/share/opencode/opencode.db` | message JOIN session, role=user filter |
| Local model | `~/.local/share/urchin/local-model.jsonl` | opt-in JSONL drop file, byte-offset |

**Drop file format (local-model):**
```json
{"prompt":"fix the memory leak","model":"ollama:mistral","ts":"2026-07-04T10:00:00Z","workspace":"/opt/project"}
```

92 tests (core 7, intake 2, mcp 17, collectors 52, vault 3, agent 11).

---

## Phase 3: WebView Collector + Urchin Desktop 🔲

**Status:** planned  
**Dependency:** Phase 2 stable (v0.3.0 baseline, 92 tests)

### Architecture

The Urchin Desktop is a **Tauri** application:
- Rust backend = `urchin-rust` (already built: daemon, MCP, collectors)
- Next.js frontend = Orinadus dashboard (already built)

No tech-stack pivot. The existing binaries become the Tauri backend.

### WebView intercept

The desktop contains native browser tabs (WebViews) that load claude.ai, chatgpt.com, gemini.google.com.
Because the Tauri wrapper controls the WebView container, it has root access to the network layer of
those sites. Urchin silently captures raw JSON payloads, normalizes the schema, and appends to the journal.

Zero API keys. Zero zip exports. Zero friction. The user logs in normally. Urchin writes.

The WebView intercept is just another `impl Collector`: the trait is already the right interface.

### Required frontend primitives

These are **architectural mandates**, not optional enhancements. A standard React DOM will choke on
Urchin's data volumes. These must be in the package.json before the first line of Tauri frontend is written.

#### `react-resizable-panels`: The IDE Layout Engine

Developer tools require draggable, high-density split panes. Standard CSS grid/flexbox cannot deliver this.

- The exact mathematical resizing primitive used by VS Code and Cursor
- Shadcn-native: first-class support in the shadcn component ecosystem
- Required layout: Claude WebView (left) | live terminal intercept stream (right)
- Enables the user full physical control over their workspace: drag to resize, collapse panes, snap to presets
- Without this, the UI is a website. With this, the UI is an IDE.

```bash
npm install react-resizable-panels
```

#### `@tanstack/react-virtual`: The DOM Virtualizer

You cannot render 50,000 intercepted terminal logs or git diffs into a standard browser DOM.
The Tauri app will freeze and the user will uninstall it.

- Headless utility: renders only the exact pixels currently visible on screen
- Keeps live DOM at ~30 items regardless of total dataset size
- Guarantees 60fps scrolling through months of local SQLite memory history
- Required for: journal timeline, terminal log stream, git diff viewer, omni-search results
- Without this, the data views will not survive real usage.

```bash
npm install @tanstack/react-virtual
```

### Sovereignty-first build order

Phase 3 must be built in this exact sequence:
1. Tauri scaffold: `cargo tauri init`, wire existing `urchin-core` as backend
2. WebView intercept: `impl Collector` for each AI web UI, network layer interception
3. Sovereignty layer activated (`.urchinignore` runtime, burn button, ephemeral toggle in UI)
4. Layout shell: react-resizable-panels pane structure
5. Data views: @tanstack/react-virtual for journal, terminal, git streams
6. Omni-search command palette (`Ctrl+K`): queries Phase 4 vector index

Sovereignty (step 3) gates the UI (steps 4-6). The data can be collected before the UI is ready;
the UI must not ship before the sovereignty layer is enforced.

### Omni-search preview

A command palette (`Ctrl+K`) searches across all captured context. Terminal error from Tuesday +
Claude solution from Wednesday appear in the same result set. This is the Phase 4 deliverable but
the data collection is wired here.

---

## Phase 4: Omni-Search 🔲

**Status:** planned  
**Dependency:** Phase 3

Vector embeddings over the journal. Each Event is embedded at write time.
Search returns semantically relevant events, not just keyword matches.

Stack options (local-first priority):
- `candle` (Hugging Face): pure Rust, no Python runtime
- Chroma or Qdrant embedded: vector index co-located with the journal
- SQLite FTS5 as fallback for machines without GPU

The command palette in Urchin Desktop queries this index.
MCP tool `urchin_search` upgrades from keyword to semantic.

---

## Phase 5: Sovereignty Layer 🔲

**Status:** planned  
**Dependency:** Phase 3 (WebView needs governance before shipping)

### Mandate

Urchin can only be adopted by serious engineers if it is zero-trust by default.
These are architectural mandates, not settings menu options.

### Air-gapped by default

All processing happens locally. The vector index, the journal, the checkpoints: all on bare metal.
Cloud sync to Orinadus Academia requires explicit user activation. Nothing leaves the machine otherwise.

### `.urchinignore` protocol

Respects ignore rules at repo or OS level:

```
# Never capture secrets
ignore: .env*
ignore: *.pem
# Blind WebView intercept for specific domains
ignore_domain: banking.com
ignore_domain: internal.company.com
# Blind terminal capture for specific processes
ignore_process: gpg
ignore_process: pass
```

Daemon reads `~/.urchinignore` (global) and `.urchinignore` at each repo root.

### Burn button

Ephemeral mode toggle. When active:
- Daemon stops writing to the journal
- No checkpoints are advanced
- Memory dies when ephemeral session ends

API: `POST /ephemeral/start`, `POST /ephemeral/end`.
MCP tool `urchin_ephemeral` exposes this to IDE agents.

### Portability

`urchin export` produces a portable JSONL archive of the full journal.
Users own their substrate. Retention through superior routing, not data lock-in.

---

## Phase 6: Multi-Device Sync 🔲

**Status:** planned  
**Dependency:** Phase 5 (sovereignty must be solid before multi-device)

The journal is the canonical source of truth. Sync protocol:
- Deterministic content-addressed chunking: only stream deltas between devices, not full journal push
- `urchin push`: sends new event chunks to the relay
- `urchin pull`: fetches chunks from other devices, deduplicates by event ID
- Relay is end-to-end encrypted; Orinadus sees only ciphertext

---

## Non-goals (permanent)

- **No browser extension**: extensions are brittle and require user installation. Urchin wraps the web environment natively via Tauri WebView.
- **No zip exports as a feature**: if you need to export, that's a failure of the API. The API must be always-on.
- **No video capture**: Urchin is not Rewind. Screen video is heavy and unreadable by agents. Urchin captures semantic intent only.
- **No forced cloud**: air-gapped by default is permanent. The cloud is opt-in infrastructure, not the product.
- **No UI before backend**: every phase starts with the daemon, the collector, or the API surface. The UI follows.
