# vmux

<p align="center">
  <strong>The Linux terminal born from the cmux revolution</strong>
  <br />
  <em>Agent-first · TTY-native · Daemonized · Zero Electron</em>
</p>

<p align="center">
  <img alt="Rust" src="https://img.shields.io/badge/Rust-000000?style=for-the-badge&logo=rust&logoColor=white" />
  <img alt="License MIT" src="https://img.shields.io/badge/License-MIT-22c55e?style=for-the-badge" />
  <img alt="Platform Linux" src="https://img.shields.io/badge/Platform-Linux-0ea5e9?style=for-the-badge&logo=linux&logoColor=white" />
  <img alt="Status" src="https://img.shields.io/badge/Status-0.1.0-f59e0b?style=for-the-badge" />
</p>

```text
  ┌──────── sidebar ────────┐  ┌──────────── workspace ────────────┐
  │ 📁 agents          🔄   │  │  tab: main │ tab: tests │ +       │
  │ 📁 backend         ✅   │  │┌───────────┬─────────────────────┐│
  │ 📁 docs                 │  ││  claude   │  codex              ││
  │                         │  ││  🔄 busy  │  🙋 needs input     ││
  │                         │  │└───────────┴─────────────────────┘│
  └─────────────────────────┘  └───────────────────────────────────┘
         Workspace → Tab → Pane(s) · agents speak through the socket
```

---

## Why vmux?

When **cmux** arrived on macOS and **wmux** on Windows, they changed how people
work with AI coding agents — vertical tabs, smart splits, notifications, and a
clean CLI so agents can drive the workspace.

Linux had classic **tmux** and a few contenders, but nothing that fully captured
that agent-first spirit with native performance and minimalism.

So we built **vmux**:

| Letter | Meaning |
|:------:|---------|
| **v** | **Versatile** — servers, desktops, headless, SSH |
| **v** | **Virtual** — persistent workspaces that feel like real sessions |
| **mux** | Proud sibling of the **cmux / wmux** family |

No Electron. No extra layers. A fast Rust multiplexer designed for power users
and AI agents working side by side.

---

## Features

<table>
<tr>
<td width="50%">

### 🖥️ TTY-native UI
Ratatui attach UI with mouse, scrollback, zoom,
resizable splits, command palette, and a vertical
workspace sidebar.

</td>
<td width="50%">

### 🧬 Daemonized sessions
PTY panes live in a detached daemon. SSH drops;
your agents keep running. Reattach anytime.

</td>
</tr>
<tr>
<td width="50%">

### 🤖 Agent-first
Sidebar emoji status (🔄 🙋 ✅ ❌), Claude/Codex/Grok
hooks, `identify --json`, scriptable send/read/wait.

</td>
<td width="50%">

### 📐 Real hierarchy
`Session → Workspace → Tab → Pane(s)` —
projects in the sidebar, tabs per workspace,
tiled panes per tab.

</td>
</tr>
<tr>
<td width="50%">

### 🔌 Socket CLI
Unix socket RPCs for every workflow: split, notify,
broadcast, browser, remote SSH, and more.

</td>
<td width="50%">

### 💾 Persistence
Layout, titles, scrollback, and pane relaunch
survive daemon restarts under XDG state paths.

</td>
</tr>
</table>

---

## Hierarchy

```text
Session  (default, work, …)
 └── Workspace   ← sidebar project (cwd, git branch, ports, pin)
      └── Tab    ← strip above the pane grid
           └── Pane(s)  ← PTY + split layout tree
```

| Level | What it is | CLI |
|-------|------------|-----|
| **Session** | Named daemon + socket | `--session name` |
| **Workspace** | Sidebar project | `vmux workspace …` |
| **Tab** | Layout inside a workspace | `vmux tab …` |
| **Pane** | One PTY process | `vmux new-pane`, `split`, `focus`, … |

Move a pane with its layout neighbor (no wrap):

```sh
vmux move left|right|up|down
```

---

## Quick start

### Build

```sh
cargo build --release
# optional: install to ~/.cargo/bin
cargo install --path .
```

### Attach

```sh
cargo run -- attach
# or after install:
vmux attach
```

Most commands auto-start the daemon, wait for the socket, then run.

### First five minutes

```sh
# New workspace with an agent
vmux workspace new --name agents --command "claude" --title backend

# Split and run tests
vmux split right --command "cargo test" --title tests

# Install sidebar status hooks (Claude, Codex, Grok, shell)
vmux hooks install
vmux hooks status

# Discover context from inside a pane
vmux identify --json
```

### Daemon & reconnect

```sh
vmux daemon                 # start only
vmux sessions               # list running / persisted
vmux --session work attach  # reattach after SSH
vmux logs --lines 100
vmux doctor
vmux stop
```

Runtime files live under `$XDG_RUNTIME_DIR/vmux` or `/tmp/vmux-$UID/vmux`:

| File | Purpose |
|------|---------|
| `<session>.sock` | CLI control socket |
| `<session>.pid` | Daemon process id |
| `<session>.log` | Daemon stdout/stderr |

Session state persists in `~/.local/state/vmux/`.

---

## Keyboard & UI

Default prefix is **`Ctrl-b`** (configurable via `vmux config set ui.prefix_key`).

| Keys / action | What it does |
|---------------|--------------|
| `Ctrl-b` then direction / mouse | Focus panes |
| `Ctrl-b z` | Zoom active pane |
| `Ctrl-b [` / `]` / `t` | Previous / next / new **workspace tab** |
| `Ctrl-b u` | Jump to notification |
| `Ctrl-b G` | Agent / status panel |
| `Ctrl-b P` | Command palette + agent controls |
| `Ctrl-b A` | Project actions (`vmux.json`) |
| Mouse wheel | Scrollback |
| Right-click pane | Context menu (copy, paste, split, clear) |
| Right-click workspace | Toggle pin |
| Control bar **Detach** | Detach (far right) without stopping the daemon |
| **⚙ set** / Settings | Theme, sidebar, cursor blink, hook install status |

---

## Agent hooks & status

Coding agents report status into the sidebar with emoji markers:

| Marker | Meaning |
|:------:|---------|
| 🔄 | Busy / running |
| 🙋 | Needs input / attention |
| ✅ | Done (sticky until you focus that pane / tab / workspace) |
| ❌ | Error / failed |

```sh
# One-shot install (also part of `vmux config init`)
vmux hooks install
vmux hooks status

# Single integration
vmux hooks install --agent claude   # ~/.claude/settings.json
vmux hooks install --agent codex    # ~/.codex/hooks.json
vmux hooks install --agent grok     # ~/.grok/skills/vmux-control
vmux hooks install --agent shell
```

In the TUI **Settings** panel each agent shows `✅ installed`, `○ missing`, or
`· not detected`. Press **Enter** on a row (or **install all hooks**) to write configs.

Shell helpers:

```sh
eval "$(vmux hooks shell)"
vmux hooks setup --dir ~/.config/vmux --rc ~/.bashrc
echo '{"event":"needs-input","message":"waiting"}' | vmux hooks event --pane "$VMUX_PANE_ID"
vmux_hook_run "tests" cargo test
vmux_hook_attention "waiting for approval"
vmux_hook_progress 50
```

Panes also export discovery env vars:

```text
VMUX_SESSION  VMUX_WORKSPACE_ID  VMUX_PANE_ID  VMUX_SURFACE_ID  VMUX_SOCKET_PATH
```

OSC notifications from processes are captured too:

```sh
printf '\033]9;needs input\a'
printf '\033]777;notify;Claude;waiting for approval\a'
```

---

## CLI cheatsheet

> Commands accept `--session <name>` (or `VMUX_SESSION`). Many take
> `--workspace` / `--pane` when you are not already inside a pane.

### Layout & panes

```sh
vmux new-pane --direction right --command "claude" --title backend --workspace ws-2
vmux split right --command "claude" --title backend
vmux focus right
vmux focus-pane --pane pane-1
vmux move left|right|up|down
vmux resize right --amount 10
vmux zoom --pane pane-1
vmux swap-panes --first pane-1 --second pane-2
vmux duplicate-pane --pane pane-1 --direction down
vmux restart-pane --workspace ws-2
vmux kill-pane --pane pane-1
vmux prune --workspace ws-2
```

### Workspaces & tabs

```sh
vmux workspace new --name agents --command "claude"
vmux workspace pin ws-2
vmux workspace move ws-2 --position 1
vmux workspace next | previous
vmux tab list
vmux tab new --title tests
vmux tab switch tab-2
vmux tab rename tab-2 --title integration
vmux tab close tab-2
```

### Agents & scripting

```sh
vmux agent new --command "claude" --title backend --workspace ws-2
vmux agent team --agents codex,claude --cwd "$PWD"
vmux agent list
vmux agent send --agent pane-1 --enter "continue"
vmux agent read --agent pane-1
vmux agent notify --agent pane-1 --status attention --message "needs input"

vmux send --enter "npm test"
vmux send-key enter
vmux broadcast --scope workspace --enter "npm test"
vmux run --command "npm test" --title tests --timeout 60
vmux wait --workspace ws-2 --timeout 30
vmux read-screen --pane pane-1 --limit-bytes 64000
vmux search "needle"
vmux identify --json
vmux agents
```

### Notifications & events

```sh
vmux notify --message "build finished"
vmux notify --pane pane-1 --status done --color green --message "agent done"
vmux notifications --limit 10
vmux events --limit 50
vmux events --follow --interval-ms 250
vmux clear-notifications
vmux jump-notification
vmux set-status busy --message "working"
vmux set-progress 75
```

### Browser, markdown, remote

```sh
vmux open-url https://example.com
vmux browser snapshot https://example.com
vmux browser screenshot https://example.com
vmux browser links https://example.com
vmux markdown open README.md
vmux remote ssh user@host --command "claude"
vmux remote tmux user@host --session work
```

### Config, actions, skills

```sh
vmux config show
vmux config init
vmux config set ui.prefix_key Ctrl-a
vmux config set ui.sidebar_collapsed true
vmux config set ui.theme contrast
vmux actions list
vmux actions run test
vmux skills list
vmux skills install vmux-control --dir .vmux/skills
```

### Session ops

```sh
vmux list
vmux status
vmux sessions
vmux logs --lines 200
vmux doctor
vmux smoke
vmux stop
```

<details>
<summary><strong>Full command surface</strong> (expand)</summary>

```text
attach  daemon  new-pane  split  run  open-url  url-snapshot  url-links
browser  agent  remote  markdown  actions  skills  config
send  send-key  broadcast  read-screen  search  clear-pane
copy-pane  paste  clipboard  kill-pane  duplicate-pane  prune  restart-pane
move-pane  swap-panes  title  tab  move  metadata  wait  resize
focus  focus-pane  zoom  workspace  surface  progress  hooks
set-progress  set-status  notify  notifications  events
clear-notifications  jump-notification  identify  list  agents
status  sessions  logs  doctor  smoke  stop
```

`pane-tab` is deprecated — use `vmux tab` (workspace tabs).

</details>

---

## Configuration

User config is managed by `vmux config` and mirrored in the attach **Settings** UI.

| Key | Example | Notes |
|-----|---------|-------|
| `ui.prefix_key` | `Ctrl-b`, `Ctrl-a` | Prefix chord |
| `ui.sidebar_collapsed` | `true` / `false` | Compact sidebar |
| `ui.sidebar_width` | `28` | Expanded width |
| `ui.scroll_step` | `8` | Scroll lines |
| `ui.theme` | `default`, `contrast`, … | Color theme |
| `ui.status_markers` | `emoji`, `ascii`, `off` | Sidebar markers |
| `ui.cursor` | (settings panel) | Cursor style / blink |

```sh
vmux config show
vmux config set ui.prefix_key Ctrl-a
```

Project actions live in `vmux.json` and are runnable via `vmux actions` or `Ctrl-b A`.

---

## Architecture (short)

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

- **Attach UI** — Ratatui; paints VT100-parsed panes, sidebar, tabs, control bar
- **Daemon** — owns PTYs, layout tree, notifications, hooks, save/restore
- **Protocol** — JSON over Unix socket for every CLI command
- **Hooks** — map Claude/Codex/Grok/shell events → sidebar `AgentStatus`

---

## cmux family mapping

| cmux idea | vmux equivalent |
|-----------|-----------------|
| Vertical tabs | Workspace sidebar |
| Splits | PTY panes + layout tree |
| Notifications | Sidebar markers + `notify` / OSC |
| Browser surfaces | `browser` / `open-url` terminal panes |
| Agent surfaces | `agent` + hooks + status |
| Socket API | `vmux …` Unix socket CLI |
| Detach / reconnect | Daemon + `sessions` / `attach` |

---

## Development

```sh
cargo build
cargo run -- attach
cargo test
cargo fmt
cargo clippy
```

```text
src/
  main.rs          entry
  cli.rs           clap commands
  daemon.rs        PTY + socket server
  ui.rs            attach TUI
  model.rs         Session / Workspace / Tab / Pane
  protocol.rs      RPC types
  config.rs        user config
  agent_hooks.rs   Claude / Codex / Grok / shell
  paths.rs         XDG runtime + state
  input.rs         key encoding into panes
```

Roadmap notes live in [`todo.md`](todo.md).

---

## License

MIT — see [`LICENSE`](LICENSE).

---

<p align="center">
  <sub>Built for Linux · Inspired by cmux & wmux · Made for humans and agents</sub>
</p>
