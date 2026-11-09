# CORVX

Build without boundaries.

CORVX is a local-first control plane for AI coding agents: it runs them durably, knows what each one is doing, routes decisions to you, and remembers what it learns about every project. Design: [`CORVX_SPEC.md`](CORVX_SPEC.md) (v0.3). Interaction design: [`CORVX_UX.md`](CORVX_UX.md). Clickable prototype (pre-v0.3): `prototype/index.html`.

> Status: M1 (sessions and attention) is mostly done; M2 (orchestration through MCP) is next. Not ready for general use. No license has been chosen yet, and the repository is not public.

## Layout

| Crate | What it is |
|---|---|
| `crates/corvx-proto` | Wire protocol: JSON-RPC 2.0 messages and shared types. |
| `crates/corvx-core` | Store (SQLite), project registry and scan, attention queue, event log. |
| `crates/corvx-client` | Client for the daemon. Starts `corvxd` on demand, detached from your terminal. |
| `crates/corvxd` | The daemon: sessions (tmux or native PTY backend), agent status, hooks. |
| `crates/corvx` | `corvx` command: CLI and terminal UI. |

## Requirements

- Rust (stable)
- tmux 3.x, for sessions that survive daemon restarts (`brew install tmux`). Without it, CORVX uses its native PTY backend, and sessions end when the daemon stops.
- For the end-to-end tests: Python 3 and `pip install pyte`.

## Build and try it

```sh
cargo build --workspace
cargo test --workspace
python3 tests/e2e/run_e2e.py        # end-to-end, with a stand-in agent; no model calls

./target/debug/corvx project add ./my-app
./target/debug/corvx session start my-app --runtime claude -- claude
./target/debug/corvx session attach <id>      # Ctrl+\ then d to detach
./target/debug/corvx attention                 # what needs you
./target/debug/corvx                           # terminal UI; press i for the inbox
```

`CORVX_E2E_REAL_CLAUDE=1 python3 tests/e2e/run_e2e.py` also runs one scenario against real Claude Code. It uses your Claude account.

Data lives in `~/.local/share/corvx` (owner-only). Override with `CORVX_HOME` or `--data-dir`.

## Protocol

One JSON object per line over the daemon's socket. Methods are listed in `crates/corvx-proto/src/lib.rs`. Start with `daemon.info`, which returns the protocol version.
