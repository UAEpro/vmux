# Contributing

## Building

vmux needs Rust 1.87 or newer.

```sh
cargo build
cargo run -- attach
cargo test
cargo fmt
cargo clippy
```

`cargo run -- attach` starts a daemon if one is not already running, so a debug
build attaches to the same session as an installed release build unless you pass
`--session`. Use a scratch session while developing:

```sh
cargo run -- --session dev attach
```

Site and user docs: [https://vmux.sh](https://vmux.sh). In-repo docs live under
[`docs/`](docs/) (including [architecture](docs/architecture.md) and
[config schema](docs/config.schema.json)).

## Architecture

```text
┌─────────────┐     Unix socket      ┌──────────────────┐
│  vmux CLI   │ ──────────────────►  │  vmux daemon     │
│  vmux attach│ ◄── snapshot/RPC ──  │  PTY panes       │
└─────────────┘                      │  layout + state  │
                                     └────────┬─────────┘
                                              │ persist
                                              ▼
                                     ~/.local/state/vmux/

┌─────────────┐   HTTP/WS :port      ┌──────────────────┐
│  phone app  │ ──────────────────►  │  vmux relay      │── same socket ──► daemon
└─────────────┘   (default 4399;     └──────────────────┘
                   relay.port / --listen)
```

The daemon owns everything that has to outlive a terminal: PTYs, the layout
tree, notifications, hook state, and save/restore. The attach UI is a client. It
renders a snapshot and sends RPCs; it holds no authoritative state. Every CLI
command is the same JSON-over-Unix-socket protocol the UI uses, which is why
agents can drive the workspace with no special access.

Workspace **port detection** runs in the daemon metadata loop (`ss -ltnp`,
filtered to pane process trees) — never on the snapshot hot path. User-facing
behaviour and `ports.*` keys are documented in [docs/ports.md](docs/ports.md).
Implementation lives in `daemon/mod.rs` (listening-port helpers + meta cache);
CLI surface for list / ssh-cmd / forward grows beside the usual
`cli.rs` → `protocol.rs` → `dispatch` path.

```text
src/
  main.rs           entry point
  cli.rs            clap command definitions
  daemon/mod.rs     PTY management + socket server + port/meta refresh
  daemon/browser.rs browser surfaces
  ui/mod.rs              attach TUI (ratatui)
  ui/theme.rs            themes + workspace second-line modes
  ui/settings_panel.rs   settings rows + draw
  ui/ports_panel.rs      ports panel draw
  ui/command_palette.rs  command palette actions + draw
  ui/input_batch.rs      input coalescing
  model.rs          Session / Workspace / Tab / Pane
  protocol.rs       RPC request and response types
  config.rs         user config (ui / relay / agent_titles / ports)
  agent_hooks.rs    Claude / Codex / Grok / shell integrations
  paths.rs          XDG runtime and state paths
  input.rs          key encoding into panes
  update.rs         daily release check
  relay/mod.rs      Cmux Remote-compatible phone relay (on by default)
  relay/auth.rs     device pairing and tokens
  sync.rs           poison-tolerant mutex helpers
```

Longer narrative: [docs/architecture.md](docs/architecture.md).

## Adding a command

A new CLI command usually touches three files: a variant in `cli.rs`, a
request/response pair in `protocol.rs`, and a handler in `daemon/mod.rs`. If it
changes what the UI shows, the snapshot type in `protocol.rs` grows a field and
`ui/mod.rs` renders it.

Config keys need four places kept in sync: the struct in `config.rs`,
`set_value`, the `supported_*` / choices lists, and `docs/config.md` (plus
`docs/config.schema.json` when the shape changes).

## Before opening a PR

```sh
cargo fmt
cargo clippy
cargo test
cargo run -- smoke      # end-to-end daemon check
```

If you touched `src/relay/` or `protocol.rs`, also run the phone app contract
suite when you have that checkout (`cd ~/code/vmux-remote && npm run test:e2e`).

Roadmap notes are in [`todo.md`](todo.md).
