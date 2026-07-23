# vmux

A terminal multiplexer for Linux, built for working alongside AI coding agents.

**Website:** [https://vmux.sh](https://vmux.sh) · **Source:** [GitHub](https://github.com/UAEpro/vmux)

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

Docs and install options: **[https://vmux.sh](https://vmux.sh)** /
[https://vmux.sh/install.html](https://vmux.sh/install.html).

Prebuilt binary — Linux (x86_64, aarch64) and macOS (Apple silicon, Intel):

```sh
curl -fsSL https://raw.githubusercontent.com/UAEpro/vmux/main/install.sh | sh
```

This picks the build for your platform, puts `vmux` in `~/.local/bin` (override
with `VMUX_INSTALL_DIR`) and verifies the release checksum. The Linux binaries
are static, so they run on any distro.

**Windows is not supported.** vmux depends on Unix domain sockets, `fork` /
`setsid`, and POSIX signals. There is no native Windows build. **WSL is fine**
— it is Linux, so the Linux binary and source builds work there the same as on
a bare-metal distro.

From crates.io, with Rust 1.87 or newer:

```sh
cargo install vmux-tui
```

The crate is `vmux-tui` because `vmux` was already taken on crates.io. The
command it installs is still `vmux`.

vmux shells out to a few tools. `git` and `curl` are worth having. `ss` and
`tailscale` are optional, and unlock [port detection](docs/ports.md) and the
[phone relay](docs/relay.md) respectively. vmux makes no network calls of its
own beyond the once-a-day update check.

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
Listening ports owned by pane processes show up too — list or forward them with
`vmux ports` ([docs/ports.md](docs/ports.md)).

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
Claude (`~/.claude/settings.json`), Codex (`~/.codex/hooks.json`), Grok Build
(`~/.grok/hooks/vmux.json`), and a shell hook. Once installed, agent state lands
in the sidebar without you doing anything.

**Automatic tab names** work for every coding agent: OSC terminal titles when
the agent sets them, hook prompts / `set-status` messages when it does not, and
an optional LLM label as last resort (see [docs/config.md](docs/config.md)).

Done (✅) is sticky. It stays until you actually look at the pane, so an agent
finishing while you were in another workspace is still there when you get back.

To drive it yourself, from a script or an agent that has no built-in
integration:

```sh
vmux set-status busy --message "working"
vmux notify --pane pane-1 --status done --message "tests passed"
printf '\033]9;needs input\a'          # or just an OSC escape
```

## Phone relay

The [phone relay](docs/relay.md) speaks the Cmux Remote protocol so you can
drive a session from your phone over Tailscale. It is **on by default** on
attach.

Default listen port is **4399** (what the phone app expects), but it is
**configurable**:

```sh
vmux config set relay.port 4399
vmux relay serve --listen 127.0.0.1:4400
```

The relay never binds `0.0.0.0` / `::` — Tailscale or localhost only.

## Pasting screenshots over SSH

Claude Code's Ctrl+V image paste reads the clipboard of the machine it runs
on. When you SSH into a box running vmux, your screenshot is on your laptop,
so Claude says "No image found on clipboard". vmux bridges that two ways.

**The paste page — nothing to install.** With the [relay](docs/relay.md)
running (`vmux relay serve`), open `http://<host>:<port>/paste` in any browser
on your tailnet (default port **4399**) and press `Cmd+V`. The image is saved
on the host and its path is typed into the active pane, where Claude Code picks
it up as an attachment. Works from a phone too — same page, photo picker
included. Keep the tab around and pasting a screenshot is: screenshot, switch
tab, `Cmd+V`.

**`vmux send-image` — for the terminal.** Pipes image bytes over SSH and
types the saved path into a pane:

```sh
# macOS (brew install pngpaste)
pngpaste - | ssh yourbox vmux send-image -

# or with no tools at all: Cmd+Shift+4 saves to Desktop, then
ssh yourbox vmux send-image - < ~/Desktop/Screen*.png

# Linux: wl-paste --type image/png (Wayland) or
#        xclip -selection clipboard -t image/png -o (X11), piped the same way
```

For both, `--pane pane-2` / `?pane=pane-2` targets a specific pane and
`--enter` / `?enter=1` submits immediately. A plain file works too:
`vmux send-image shot.png`.

## Docs

- [CLI reference](docs/cli.md), or `vmux --help`
- [Configuration](docs/config.md) (themes, relay, ports, agent titles)
- [Config JSON Schema](docs/config.schema.json)
- [Ports](docs/ports.md) — detection, list, ssh-cmd, forward
- [Phone relay](docs/relay.md) — Cmux Remote-compatible server over Tailscale
- [Architecture](docs/architecture.md) — daemon / client / relay
- [Troubleshooting](docs/troubleshooting.md)
- [Contributing](CONTRIBUTING.md)
- Website: [https://vmux.sh](https://vmux.sh)

## Prior art

vmux exists because [cmux](https://github.com/manaflow-ai/cmux) made
agent-driven work feel good on macOS, and Linux did not have an equivalent. tmux
is excellent and is not trying to solve this. If you want vertical workspaces, a
socket agents can drive, and status that tells you who needs you, that is what
this is.

Parts of the agent-status UX — especially screen-aware busy / needs-input /
done detection and herdr-style agent fidelity — take inspiration from
**[herdr](https://github.com/ogulcancelik/herdr)** ([herdr.dev](https://herdr.dev)),
an agent multiplexer by [Oğulcan Çelik](https://github.com/ogulcancelik). Thank
you for the open-source work and for pushing the “status at a glance” model
forward.

## License

MIT. See [LICENSE](LICENSE).
