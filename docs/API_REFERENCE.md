# Urchin Intake: API Reference

> Describes the `POST /ingest` and `GET /health` routes as implemented in
> `crates/urchin-intake/src/server.rs` at v0.3.4.

---

## Server

| Property       | Value                             |
|----------------|-----------------------------------|
| Bind address   | `127.0.0.1:<intake_port>`         |
| Default port   | `18799`                           |
| Protocol       | HTTP/1.1                          |
| Content-Type   | `application/json` (required)     |

The server never binds on `0.0.0.0`. It is loopback-only by design.

---

## Authentication

Authentication is **opt-in**. If `intake_token` is not set in config, all requests are accepted.

When a token is configured, every `POST /ingest` request must include:

```
Authorization: Bearer <token>
```

- Token comparison is string equality (not constant-time; acceptable for loopback-only use).
- `Bearer ` prefix (capital B, one space) is required.
- Missing header, wrong prefix, or wrong token → `401 Unauthorized`.

**Set token via config:**
```toml
# ~/.config/urchin/config.toml
intake_token = "your-secret-token"
```

**Or via environment variable (overrides config):**
```bash
export URCHIN_INTAKE_TOKEN="your-secret-token"
```

---

## POST /ingest

Accept a single `Event` object. Write it to the journal.

### Request

```
POST /ingest HTTP/1.1
Content-Type: application/json
Authorization: Bearer <token>        ← required only if intake_token is configured
```

### Body: Event object

All fields except the required ones are optional and will be omitted from the JSONL line
if not provided (no null values written to disk).

| Field       | Type             | Required | Description                                                                      |
|-------------|------------------|----------|----------------------------------------------------------------------------------|
| `id`        | UUID string      | Yes      | RFC 4122 UUID v4. Generate client-side. Deduplication key.                       |
| `timestamp` | ISO 8601 string  | Yes      | RFC 3339 UTC timestamp. Example: `"2026-05-04T20:00:00Z"`                        |
| `source`    | string           | Yes      | Tool or system that produced the event. Must not be blank after trim.            |
| `kind`      | string (enum)    | Yes      | See valid values below.                                                          |
| `content`   | string           | Yes      | The memory payload. Must not be blank after trim.                                |
| `brain`     | string           | No       | Vault/brain identifier for multi-brain setups.                                   |
| `workspace` | string           | No       | Absolute path to the repo or workspace this event belongs to.                    |
| `session`   | string           | No       | Session identifier (e.g. Copilot session UUID, tmux session name).               |
| `title`     | string           | No       | Short human-readable title for display in vault projections.                     |
| `tags`      | array of strings | No       | Free-form tags. Omitted from JSONL if empty.                                     |
| `actor`     | Actor object     | No       | Identity envelope. See Actor schema below.                                       |

**Actor object:**

| Field       | Type   | Description                                    |
|-------------|--------|------------------------------------------------|
| `account`   | string | User account / GitHub handle                   |
| `device`    | string | Machine hostname                               |
| `workspace` | string | Workspace path (redundant with top-level field)|

All Actor fields are optional. The entire `actor` field is omitted from JSONL if not provided.

### Valid `kind` values

| Value          | Use case                                                       |
|----------------|----------------------------------------------------------------|
| `conversation` | AI chat turn, notes, anything conversational                   |
| `agent`        | Agent-generated reflection or decision                         |
| `command`      | Shell command, CLI invocation                                  |
| `commit`       | Git commit captured by collector                               |
| `file`         | File creation, edit, or deletion event                         |
| `decision`     | Architectural or product decision                              |
| `<other>`      | Any string not in the list above is accepted as `Other(value)` |

### Example request: minimal

```bash
curl -X POST http://127.0.0.1:18799/ingest \
  -H "Content-Type: application/json" \
  -d '{
    "id": "56816532-adb7-4000-8a0f-1dda8408aab5",
    "timestamp": "2026-05-04T20:00:00Z",
    "source": "copilot",
    "kind": "conversation",
    "content": "Hardened intake auth and added journal write lock."
  }'
```

### Example request: full

```bash
curl -X POST http://127.0.0.1:18799/ingest \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer mysecrettoken" \
  -d '{
    "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
    "timestamp": "2026-05-04T20:00:00Z",
    "source": "claude",
    "kind": "decision",
    "content": "Use file-backed ephemeral flag for cross-process suppression.",
    "workspace": "/home/user/dev/orinadus/substrate/urchin-rust",
    "session": "session-abc123",
    "title": "Cross-process ephemeral design decision",
    "tags": ["architecture", "ephemeral", "substrate"],
    "actor": {
      "account": "alice",
      "device": "workstation",
      "workspace": "/home/alice/dev/myproject"
    }
  }'
```

---

## Response codes

### 200 OK: Written

Event was validated and written to the journal.

```json
{
  "id": "56816532-adb7-4000-8a0f-1dda8408aab5",
  "status": "ok"
}
```

### 202 Accepted: Ephemeral drop

Ephemeral mode is active (`ephemeral.lock` file exists). The event was accepted but **permanently
discarded**. Nothing was written to disk.

```json
{
  "id": "56816532-adb7-4000-8a0f-1dda8408aab5",
  "status": "dropped",
  "reason": "ephemeral"
}
```

### 400 Bad Request: Validation failure

`content` or `source` are present in the JSON but blank (empty or whitespace-only).

```json
{ "error": "content must not be empty" }
```

```json
{ "error": "source must not be empty" }
```

### 401 Unauthorized: Auth failure

`intake_token` is configured and the `Authorization` header is missing, malformed, or wrong.

```json
{ "error": "unauthorized" }
```

### 422 Unprocessable Entity: Schema failure

Body is not valid JSON, or required fields (`id`, `timestamp`, `source`, `kind`, `content`) are
absent. This response is generated by Axum's JSON extractor before the handler runs. The response
body is Axum's default structured error: it is not currently normalised to the error envelope above.

### 500 Internal Server Error: Write failure

`journal.append()` failed (disk full, permission error, Mutex poisoned). Includes the error string.

```json
{ "error": "No space left on device" }
```

---

## GET /health

No auth required.

### Response

```json
{
  "status": "ok",
  "events": 1042,
  "ephemeral": false
}
```

| Field       | Type    | Description                                                   |
|-------------|---------|---------------------------------------------------------------|
| `status`    | string  | Always `"ok"` if the server is running                        |
| `events`    | integer | Current event count in the journal (from `Journal::stats()`)  |
| `ephemeral` | boolean | Whether ephemeral mode is currently active                    |

Note: `events` is `0` if the journal does not exist or `stats()` fails. The journal path is
intentionally not included in the response.

---

## JSONL on-disk format

Each successful `POST /ingest` appends one line to `events.jsonl`. The line is the JSON
serialization of the `Event` struct with `skip_serializing_if` applied:

- `None` fields are omitted (no `null` keys)
- `tags: []` is omitted
- All other fields are present as-is

Example line:
```json
{"id":"56816532-adb7-4000-8a0f-1dda8408aab5","timestamp":"2026-05-04T20:00:00Z","source":"copilot","kind":"conversation","content":"Hardened intake auth and added journal write lock.","workspace":"/home/user/dev/project","tags":["substrate"]}
```

---

## SDK usage

`urchin-sdk` wraps the HTTP calls. The `EventBuilder` pattern:

```rust
use urchin_sdk::builder::EventBuilder;
use urchin_core::event::EventKind;

let event = EventBuilder::new()
    .source("my-tool")
    .kind(EventKind::Decision)
    .content("Chose file-backed flag for cross-process ephemeral mode")
    .workspace("/home/user/project")
    .tag("architecture")
    .build();

let client = urchin_sdk::Client::new("http://127.0.0.1:18799", Some("my-token".into()));
client.ingest(event).await?;
```
