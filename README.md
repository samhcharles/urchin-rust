# Urchin Substrate

> Local-first memory sync substrate for AI tools. Gives every tool — Claude, Copilot, Gemini, Codex, shell — the same shared memory layer.

**This is the production implementation.** The Node.js spike lives at [samhcharles/urchin](https://github.com/samhcharles/urchin).

---

## Positioning

Urchin does not own your tools. It connects them. It is additive. Nobody loses anything, every tool you already use gets smarter. The substrate earns its place by being genuinely useful, not by creating switching costs.

---

## Workspace layout

```
crates/
  urchin-core        — event model, journal, identity, config (no I/O)
  urchin-intake      — HTTP intake server (POST /ingest)
  urchin-mcp         — MCP server over stdio (5 tools)
  urchin-collectors  — one module per source: claude, copilot, gemini, shell, git
  urchin-vault       — Obsidian vault projection (reads vault contract, writes markers)
  urchin-cli         — `urchin` binary: serve | mcp | doctor | ingest
```

## Quick start

```bash
cargo build
cargo run -p urchin-cli -- doctor
cargo run -p urchin-cli -- ingest --content "hello from Rust" --workspace ~/dev/urchin-rust
```

## Key design rules

- `urchin-core` has zero I/O — pure types and logic only
- The journal is append-only. Events are never mutated.
- Vault writes go through `urchin-vault` only — marker blocks and `_urchin/` namespace only
- Every collector reads; no collector writes back to its source tool
- Single binary output from `urchin-cli`

## Related

- Node.js spike (reference): [samhcharles/urchin](https://github.com/samhcharles/urchin)
- Brain vault contract: `~/brain/_urchin/README.md`
- Test surfaces: `~/brain/_urchin/test-surfaces/`
- Substrate design: `~/brain/30-resources/dev/urchin-substrate-design.md`
- Test contract: `~/brain/30-resources/dev/urchin-test-contract.md`
