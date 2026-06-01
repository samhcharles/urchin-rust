# Urchin Sovereignty Specification

> This document is the contract. It travels ahead of the implementation.
> Any feature that violates the principles below must not ship.

---

## The Mandate

Urchin captures terminal output, AI conversation history, git history, and (in Phase 3) raw web traffic.
This is an extraordinary amount of access to a developer's machine and mind.

For a developer to trust a system with this level of access, the system must be provably sovereign.
Not "we promise we're good." Provably sovereign: verifiable in the binary, the protocol, and the defaults.

Urchin earns trust by making sovereignty the zero-configuration default, not a setting buried in a menu.

---

## Principle 1: Air-Gapped by Default

**The rule:** Nothing leaves the local machine unless the user explicitly activates an outbound path.

**Current implementation:**
- Journal is written to `~/.local/share/urchin/journal.jsonl` (or `URCHIN_JOURNAL` override)
- HTTP intake binds to `127.0.0.1:18799` only: not `0.0.0.0`
- `urchin sync` is a manual command, not a background process
- Cloud sync requires `URCHIN_SYNC_TOKEN` to be set; without it, sync is a no-op

**Phase 3+ requirement:**
- WebView intercept writes to local journal only
- No telemetry is sent to Orinadus or any third party without explicit opt-in
- Orinadus Academia sync is an explicit config flag, not a default

**Verification test:** A user who installs Urchin and never sets `URCHIN_SYNC_TOKEN` and never
runs `urchin sync` must have zero bytes leave their machine at any point.

---

## Principle 2: The `.urchinignore` Protocol

**The rule:** Users define explicit boundaries. The daemon is blind to anything inside those boundaries.

### File locations (evaluated in order)

1. `~/.urchinignore`: global, applies to all collectors
2. `<repo>/.urchinignore`: repo-local, applies when the collector's workspace matches the repo root

### Key syntax

```
# Lines starting with # are comments

# Blind file paths and patterns from all capture
ignore: .env
ignore: .env.*
ignore: *.pem
ignore: *.key
ignore: secrets/

# Blind specific domains in WebView intercept (Phase 3+)
ignore_domain: banking.com
ignore_domain: internal.company.com

# Blind specific processes from terminal capture
ignore_process: gpg
ignore_process: pass
ignore_process: 1password
```

### Semantics

- `ignore: <pattern>`: glob pattern matched against file paths in git diffs and vault projections.
  The collector skips any event whose workspace or content references a matching path.
- `ignore_domain: <domain>`: WebView intercept does not capture network traffic to/from this domain.
  Matching is suffix-based (e.g., `banking.com` matches `app.banking.com`).
- `ignore_process: <name>`: shell collector skips history lines where the command starts with this name.

### Implementation contract

- The daemon reads `.urchinignore` files at startup and on SIGHUP.
- Collectors check applicable ignore rules before writing any event.
- If an ignore rule matches, the event is silently dropped: no error, no log at warn or above.
- Unknown keys in `.urchinignore` are silently ignored (forward compatibility).

### Phase 3+ requirement

The WebView intercept applies `ignore_domain` rules at the network layer: the response payload
is not read or deserialized for ignored domains, not just filtered after the fact.

---

## Principle 3: The Burn Button (Ephemeral Mode)

**The rule:** Users must be able to work in a mode where nothing is written, at any time,
with a single action. When ephemeral mode ends, the session is gone.

### API surface (daemon)

```
POST /ephemeral/start
  Response: 200 OK, { "ephemeral": true, "started_at": "<ISO8601>" }

POST /ephemeral/end
  Response: 200 OK, { "ephemeral": false, "events_suppressed": <count> }
```

### Behavior

- When ephemeral mode is active:
  - `journal.append()` is a no-op: the event is computed but not written
  - Checkpoints are NOT advanced: the collector will re-read from its last committed position when normal mode resumes
  - Vault projection is suppressed
  - Cloud sync is suppressed
- When ephemeral mode ends, none of the suppressed events can be recovered

### CLI surface

```bash
urchin ephemeral start   # activate burn mode
urchin ephemeral end     # deactivate, print suppressed count
urchin ephemeral status  # check current state
```

### MCP surface

`urchin_ephemeral` tool: lets IDE agents (Cursor, VS Code, Zed) toggle ephemeral mode
on behalf of the user. The tool must require explicit user confirmation before activating.

### Phase 3 (Desktop) surface

A physical, high-contrast toggle in the Urchin Desktop header. When active, the UI renders
a persistent "EPHEMERAL MODE: nothing is being written" banner. This is not a subtle indicator.

---

## Principle 4: Portability (The Exit Node)

**The rule:** Users own the substrate. A complete export is always one command away.
No export feature means no trust.

### CLI

```bash
urchin export [--output <path>] [--since <ISO8601>] [--until <ISO8601>]
```

### Output format

Newline-delimited JSON (JSONL). Each line is a complete `Event` object in canonical serialized form.
The output is self-contained and can be re-ingested by any Urchin instance:

```bash
cat export.jsonl | xargs -L1 curl -s -X POST http://127.0.0.1:18799/ingest -d
```

### Guarantees

- The export command is read-only: it never modifies the journal or any checkpoint
- The output is sorted by `event.timestamp` ascending
- All fields are included: nothing is stripped for "privacy" without user instruction
- The export format is documented and stable from the version it ships in

---

## Audit trail

This spec is version-controlled in the Urchin repository. Changes require a commit with a clear
reason. Any future Systems Engineer working on this codebase inherits this contract.

The implementation order for these principles:

| Principle | Runtime enforcement | Target phase |
|---|---|---|
| Air-gapped by default | ✅ enforced now (bind + sync-off by default) | Phase 0 |
| `.urchinignore`: file/process rules | 🔲 spec only | Phase 5 |
| `.urchinignore`: domain rules | 🔲 spec only | Phase 3 (WebView) |
| Burn button API | 🔲 spec only | Phase 5 |
| Burn button CLI | 🔲 spec only | Phase 5 |
| Burn button Desktop UI | 🔲 spec only | Phase 3 |
| Export command | 🔲 spec only | Phase 5 |
