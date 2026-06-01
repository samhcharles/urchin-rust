# Urchin: dev loop

## Repo layout

```
crates/
  urchin-core/       zero I/O: shared types: Event, Journal, Identity, Config
  urchin-intake/     axum HTTP: POST /ingest, GET /health (port 18799)
  urchin-mcp/        MCP over stdio: 5 tools, JSON-RPC 2.0
  urchin-collectors/ shell, git, claude, copilot, gemini: all live
  urchin-vault/      vault projection: atomic marker-block writes into ~/brain
  urchin-sdk/        shared types for external integrations
  urchin-cli/        single binary: urchin
```

## Build

```bash
cd ~/dev/orinadus/urchin-rust
cargo build              # dev binary → target/debug/urchin
cargo build --release    # release binary
cargo test               # 56 tests
```

## Daemon (systemd user service)

```bash
mkdir -p ~/.config/systemd/user
cp systemd/urchin.service ~/.config/systemd/user/urchin.service
cp systemd/urchin.env.example ~/.config/urchin/env
systemctl --user daemon-reload
systemctl --user start urchin        # start
systemctl --user stop urchin         # stop
systemctl --user restart urchin      # restart: picks up new binary automatically
journalctl --user -u urchin -f       # follow logs
```

The shipped service expects an installed binary at `~/.local/bin/urchin`. Use `cargo install --path crates/urchin-cli --force` after changes. For true 24/7 user-service behavior across logouts, run `loginctl enable-linger "$USER"` once.

## MCP

All AI clients (Claude, VS Code, Copilot CLI) are configured to use:
```
/home/samhc/dev/orinadus/urchin-rust/target/debug/urchin mcp
```

MCP runs as a child process launched on-demand. Rebuilding the binary is enough.

## Tests

```bash
cargo test                            # all crates
cargo test -p urchin-collectors       # single crate
cargo test -- --nocapture             # show stdout
```

Test counts: core (7), intake (2), mcp (10), collectors (34), vault (3) = **56 total**

## Health check

```bash
urchin doctor
curl -s http://127.0.0.1:18799/health
```

## Manual collector runs

```bash
urchin collect shell                  # shell history delta
urchin collect git                    # git commits delta
urchin collect claude                 # claude transcripts delta
urchin collect copilot                # copilot command history delta
urchin collect gemini                 # gemini chat sessions delta
urchin collect all                    # every collector
```

## Query the journal

```bash
urchin recent --n 20                  # last 20 events
urchin recent --source claude --n 10  # last 10 from claude
urchin query "some keyword"           # search journal
```

## Vault projection

```bash
urchin vault project                  # project today → ~/brain/daily/YYYY-MM-DD.md
urchin vault project --date 2026-05-01
```

Writes inside `<!-- URCHIN:DAILY -->` marker blocks only. Human content is never touched.

## Cloud sync

```bash
urchin sync                           # push journal events to orinadus.com
```

Config: `cloud_url` and `cloud_token` in `~/.config/urchin/config.toml`.
Live endpoint: `https://www.orinadus.com/api/urchin-sync`

## Configuration

`~/.config/urchin/config.toml`: all fields optional, env vars override.

```toml
vault_root   = "/home/samhc/brain"
intake_port  = 18799
cloud_url    = "https://www.orinadus.com/api/urchin-sync"
cloud_token  = "<token>"
```

## Adding a collector

1. Create `crates/urchin-collectors/src/<name>.rs`
2. Define `<Name>Opts { source_path, checkpoint_path }` + `impl <Name>Opts { pub fn defaults() -> Self }`
3. Implement `pub fn collect(journal: &Journal, identity: &Identity, opts: &<Name>Opts) -> Result<usize>`
4. Add one arm to `run_all()` in `lib.rs`
5. Add variant to `CollectKind` in `crates/urchin-cli/src/main.rs`
6. Add tests using `tempfile::tempdir()`: never touch the real home dir in tests
