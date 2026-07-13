# vmux

A terminal multiplexer for Linux, built for working alongside AI coding agents.

```text
  ┌──────── sidebar ────────┐  ┌──────────── workspace ────────────┐
  │ 📁 agents          🔄   │  │  tab: main │ tab: tests │ +       │
  │ 📁 backend         ✅   │  │┌───────────┬─────────────────────┐│
  │ 📁 docs                 │  ││  claude   │  codex              ││
  │                         │  ││  🔄 busy  │  🙋 needs input     ││
  │                         │  │└───────────┴─────────────────────┘│
  └─────────────────────────┘  └───────────────────────────────────┘
```

Panes run inside a detached daemon, so when SSH drops or you close the terminal,
your agents keep working. Reattach and they are where you left them.

Agents talk to vmux over a Unix socket. They can split panes, open workspaces,
read a neighbour's screen, and report their own status, which shows up in the
sidebar as 🔄 busy, 🙋 needs input, ✅ done, or ❌ failed. So you can see at a
glance which of six running agents is actually waiting on you.

Written in Rust, drawn with ratatui. No Electron, no browser, no daemon in a
language you did not ask for.

## Install

Prebuilt binary — Linux (x86_64, aarch64) and macOS (Apple silicon, Intel):

```sh
curl -fsSL https://raw.githubusercontent.com/UAEpro/vmux/main/install.sh | sh
```

This picks the build for your platform, puts `vmux` in `~/.local/bin` (override
with `VMUX_INSTALL_DIR`) and verifies the release checksum. The Linux binaries
are static, so they run on any distro.

Windows is not supported: vmux is built on Unix domain sockets, `fork`/`setsid`
and POSIX signals. It runs under WSL.

From crates.io, with Rust 1.87 or newer:

```sh
cargo install vmux-tui
```

The crate is `vmux-tui` because `vmux` was already taken on crates.io. The
command it installs is still `vmux`.

vmux shells out to a few tools. `git` and `curl` are worth having. `gh`, `ss`,
and `tailscale` are optional, and unlock PR info, port detection, and the phone
relay respectively.

## Quick start

```sh
vmux attach
```

That is the whole thing: attach starts a daemon if one is not running. From
there, or from any shell:

```sh
# a workspace with an agent in it
vmux workspace new --name agents --command "claude" --title backend

# split the current pane and run tests beside it
vmux split right --command "cargo test" --title tests

# let agents report status into the sidebar
vmux hooks install

# from inside a pane: where am I?
vmux identify --json
```

After an SSH drop, or tomorrow morning:

```sh
vmux sessions               # what is still running
vmux --session work attach  # pick up where you left off
```

## Concepts

```text
Session  (default, work, …)          one daemon, one socket
 └── Workspace                       a project in the sidebar: cwd, git branch, ports
      └── Tab                        the strip above the pane grid
           └── Pane                  one PTY process
```

A workspace is the unit you think in. It has a directory, so a new pane opens
where you expect, and the sidebar shows its git branch and its agent's status.

## Keys

The prefix is `Ctrl-b`, changeable with `vmux config set ui.prefix_key`.

| Keys | |
|------|---|
| `Ctrl-b` + arrow | Move focus between panes |
| `Ctrl-b z` | Zoom the active pane |
| `Ctrl-b [` `]` `t` | Previous, next, new tab |
| `Ctrl-b w` | Workspace picker |
| `Ctrl-b B` | Toggle the sidebar |
| `Ctrl-b u` | Jump to whatever just notified you |
| `Ctrl-b P` | Command palette |
| `Ctrl-b G` | Agent status panel |

The mouse works too: wheel to scroll back, right-click a pane for copy, paste,
split, and clear.

## Agent hooks

`vmux hooks install` writes the integration for whichever agents it finds:
Claude (`~/.claude/settings.json`), Codex, Grok, and a shell hook. Once
installed, an agent's state lands in the sidebar without you doing anything.

Done (✅) is sticky. It stays until you actually look at the pane, so an agent
finishing while you were in another workspace is still there when you get back.

To drive it yourself, from a script or an agent that has no built-in
integration:

```sh
vmux set-status busy --message "working"
vmux notify --pane pane-1 --status done --message "tests passed"
printf '\033]9;needs input\a'          # or just an OSC escape
```

## Pasting screenshots over SSH

Claude Code's Ctrl+V image paste reads the clipboard of the machine it runs
on. When you SSH into a box running vmux, your screenshot is on your laptop,
so Claude says "No image found on clipboard". `vmux send-image` bridges that:
it saves image bytes on the host and types the file path into a pane, which
Claude Code picks up as an attachment.

From your laptop, one command sends the clipboard image straight into the
active pane on the server:

```sh
# macOS (brew install pngpaste)
pngpaste - | ssh yourbox vmux send-image -

# Linux, Wayland
wl-paste --type image/png | ssh yourbox vmux send-image -

# Linux, X11
xclip -selection clipboard -t image/png -o | ssh yourbox vmux send-image -
```

Add a shell alias (`alias shot='pngpaste - | ssh yourbox vmux send-image -'`)
and pasting a screenshot becomes: take screenshot, type `shot`, press Enter in
Claude. `--pane pane-2` targets a specific pane, `--enter` submits
immediately, and a plain file works too: `vmux send-image shot.png`.

## Docs

- [CLI reference](docs/cli.md), or `vmux --help`
- [Configuration](docs/config.md), including themes and project actions
- [Phone relay](docs/relay.md), a Cmux Remote-compatible server for driving vmux from an iPhone over Tailscale
- [Contributing](CONTRIBUTING.md) and architecture notes

## Prior art

vmux exists because [cmux](https://github.com/manaflow-ai/cmux) made
agent-driven work feel good on macOS, and Linux did not have an equivalent. tmux
is excellent and is not trying to solve this. If you want vertical workspaces, a
socket agents can drive, and status that tells you who needs you, that is what
this is.

## License

MIT. See [LICENSE](LICENSE).
