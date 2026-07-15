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
```

The daemon owns everything that has to outlive a terminal: PTYs, the layout
tree, notifications, hook state, and save/restore. The attach UI is a client. It
renders a snapshot and sends RPCs; it holds no authoritative state. Every CLI
command is the same JSON-over-Unix-socket protocol the UI uses, which is why
agents can drive the workspace with no special access.

```text
src/
  main.rs           entry point
  cli.rs            clap command definitions
  daemon/mod.rs     PTY management + socket server
  daemon/browser.rs browser surfaces
  ui/mod.rs         attach TUI (ratatui)
  ui/input_batch.rs input coalescing
  model.rs          Session / Workspace / Tab / Pane
  protocol.rs       RPC request and response types
  config.rs         user config
  agent_hooks.rs    Claude / Codex / Grok / shell integrations
  paths.rs          XDG runtime and state paths
  input.rs          key encoding into panes
  update.rs         daily release check
  relay/mod.rs      Cmux Remote-compatible phone relay (on by default)
  relay/auth.rs     device pairing and tokens
```

## Adding a command

A new CLI command usually touches three files: a variant in `cli.rs`, a
request/response pair in `protocol.rs`, and a handler in `daemon/mod.rs`. If it
changes what the UI shows, the snapshot type in `protocol.rs` grows a field and
`ui/mod.rs` renders it.

## Before opening a PR

```sh
cargo fmt
cargo clippy
cargo test
cargo run -- smoke      # end-to-end daemon check
```

Roadmap notes are in [`todo.md`](todo.md).
