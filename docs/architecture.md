# Architecture

Short map of how vmux is put together — useful whether you are scripting the
CLI, pairing a phone, or reading the code. Deeper contributor notes live in
[CONTRIBUTING.md](../CONTRIBUTING.md).

## Processes

```text
┌──────────────────┐   JSON-RPC over    ┌──────────────────────────┐
│  vmux CLI        │   Unix socket      │  vmux daemon             │
│  vmux attach UI  │ ─────────────────► │  owns PTYs, layout,      │
│  agent scripts   │ ◄── snapshots ──── │  hooks, save/restore     │
└──────────────────┘                    └────────────┬─────────────┘
                                                     │
┌──────────────────┐   HTTP + WebSocket              │ same socket
│  Phone app /     │   (Tailscale or localhost)      │
│  browser /paste  │ ──── vmux relay :port ──────────┘
└──────────────────┘     (default port 4399;
                          relay.port / --listen)
```

**The daemon is authoritative.** Attach is a dumb client: it renders a
snapshot and sends RPCs. It does not keep layout or pane state of its own.
Every CLI command speaks the same protocol, which is why agents can drive
workspaces without special access.

**The relay is optional.** When `relay.enabled` is on (default), attach can
start a managed relay. The relay is just another socket client plus an HTTP
front door for Cmux Remote–compatible apps. Turning it off does not change
local attach or CLI behaviour.

**Persistence** is session JSON under the XDG state dir
(`~/.local/state/vmux/<session>.json`), including scrollback. Runtime sockets
and pid/log files live under `$XDG_RUNTIME_DIR/vmux` (or `/tmp/vmux-$UID/vmux`).

## Hierarchy

```text
Session  (default, work, …)     one daemon, one socket, one state file
 └── Workspace                  sidebar row: cwd, git branch, ports, agent status
      └── Tab                   strip above the pane grid
           └── Pane             one PTY process
```

## Snapshots and repaints

- `Server.generation` is bumped on every meaningful session/runtime change.
- Clients poll `Snapshot { since }`. If nothing moved, the daemon returns
  `{ unchanged: true, generation }` and the UI skips a repaint.
- Fidelity levels:
  - **full** — includes heavy per-pane scrollback (persistence / rare paths)
  - **normal** — layout and status without full history
  - **lean** — attach poll: live screen contents; scrollback only for panes
    the client is scrolled back in

The snapshot path must not shell out. Tools like `git` and `ss` run in a
background metadata loop and write a cache; snapshots read that cache. Port
detection is part of that loop ([ports.md](ports.md)).

## Locks

`Server` is a struct of **independent** mutexes (`session`, `panes`,
`workspace_meta`, …), not one big lock. Holding `session` and `panes` at the
same time is forbidden — take one, copy what you need, drop it, then take the
other. Locks use poison-tolerant helpers so one panicked handler thread cannot
brick the daemon or relay.

## Config surface

User config (`LmuxConfig`) covers `ui.*`, `relay.*`, `agent_titles.*`, and
`ports.*`. Schema: [config.schema.json](config.schema.json). Human docs:
[config.md](config.md).

## Source map (high level)

```text
src/
  main.rs           entry point
  cli.rs            clap commands
  daemon/mod.rs     PTY management, socket server, port/meta refresh
  daemon/browser.rs browser surfaces
  ui/mod.rs         attach TUI (ratatui)
  model.rs          Session / Workspace / Tab / Pane
  protocol.rs       versioned RPC types
  config.rs         LmuxConfig + config set
  agent_hooks.rs    Claude / Codex / Grok / shell integrations
  paths.rs          XDG paths + reserved state stems
  relay/mod.rs      phone relay (HTTP + WebSocket)
  relay/auth.rs     device pairing and tokens
```

## Adding a feature (contributor cheat sheet)

Usually three files: a variant in `cli.rs`, a `Request`/`Response` pair in
`protocol.rs` (new fields get `#[serde(default)]`), a handler arm in
`Server::dispatch`. If the UI must show it, grow the snapshot type and render
in `ui/mod.rs`. Always bump `generation` on mutations.

Develop against a scratch session:

```sh
cargo run -- --session dev attach
```
