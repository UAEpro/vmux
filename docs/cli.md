# CLI reference

Every command accepts `--session <name>` (or the `VMUX_SESSION` env var). Most
take `--workspace` / `--pane` when you are not already running inside a pane.

Run `vmux --help` or `vmux <command> --help` for the authoritative flags.

## Layout and panes

```sh
vmux new-pane --direction right --command "claude" --title backend --workspace ws-2
vmux split right --command "claude" --title backend
vmux focus right
vmux focus-pane --pane pane-1
vmux move left|right|up|down
vmux resize right --amount 10
vmux view-size --pane pane-1 --cols 46 --rows 22   # phone-fit: hold the PTY at min(layout, view)
vmux view-size --pane pane-1 --clear               # restore the layout size now
vmux zoom --pane pane-1
vmux swap-panes --first pane-1 --second pane-2
vmux duplicate-pane --pane pane-1 --direction down
vmux restart-pane --workspace ws-2
vmux kill-pane --pane pane-1
vmux prune --workspace ws-2
```

`vmux move` shifts a pane with its layout neighbor. It does not wrap.

`vmux view-size` is a *leased* override (default 10s): it expires unless re-sent,
so a viewer that dies can never pin a pane small. Phone-driven resizing is
additionally gated behind `relay.allow_view_resize` (default off); the CLI
command talks to the daemon directly and is not gated. The relay re-leases it
automatically for phone subscribers; the desktop UI dims the unused margin with
a "sized by phone" note while one is active. Zoomed panes refuse it. Overrides
are never persisted — a daemon restart always restores desktop sizes.

## Workspaces and tabs

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

`pane-tab` is deprecated. Use `vmux tab`.

## Agents and scripting

```sh
vmux agent new --command "claude" --title backend --workspace ws-2
vmux agent team --agents codex,claude --cwd "$PWD"
vmux agent list
vmux agent send --agent pane-1 --enter "continue"
vmux agent read --agent pane-1
vmux agent notify --agent pane-1 --status attention --message "needs input"

vmux send --enter "npm test"
vmux send-key enter
# save an image on this host and type its path into the pane — pipe a
# screenshot over SSH into Claude Code: pngpaste - | ssh box vmux send-image -
vmux send-image screenshot.png --pane pane-1
vmux broadcast --scope workspace --enter "npm test"
vmux run --command "npm test" --title tests --timeout 60
vmux wait --workspace ws-2 --timeout 30
# block until the agent needs you (or finishes / the pane exits)
vmux wait --pane pane-1 --status attention,done,error --timeout 600
vmux read-screen --pane pane-1 --limit-bytes 64000
vmux search "needle"
vmux identify --json
vmux agents
vmux events --limit 50
vmux events --since 120 --follow --interval-ms 500
```

Panes export discovery variables, so a process can find its own place in the
tree without being told:

```text
VMUX_SESSION  VMUX_WORKSPACE_ID  VMUX_PANE_ID  VMUX_SURFACE_ID  VMUX_SOCKET_PATH
```

## Notifications and events

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

Processes can also raise notifications with OSC escapes, with no vmux command in
the loop:

```sh
printf '\033]9;needs input\a'
printf '\033]777;notify;Claude;waiting for approval\a'
```

## Hooks

```sh
vmux hooks install                  # all detected agents
vmux hooks status
vmux hooks install --agent claude   # ~/.claude/settings.json
vmux hooks install --agent codex    # ~/.codex/hooks.json
vmux hooks install --agent grok     # ~/.grok/hooks/vmux.json (+ skill)
vmux hooks install --agent shell

# Screen-manifest agent detection (herdr-style primary status for Claude/Codex/…)
# Status authority: screen rules first; hooks fill in when no manifest agent is running.
# Offline explain (read screen dump from stdin or --file):
printf '%s\n' ' ❯  ' | vmux detect --agent claude
vmux detect --agent claude --file screen.txt --osc-title $'\u28FF thinking' --json
# Local rule overrides: ~/.config/vmux/agent-detection/<agent>.toml

eval "$(vmux hooks shell)"
vmux hooks setup --dir ~/.config/vmux --rc ~/.bashrc
echo '{"event":"needs-input","message":"waiting"}' | vmux hooks event --pane "$VMUX_PANE_ID"
```

The shell integration defines helpers you can call from scripts:

```sh
vmux_hook_run "tests" cargo test
vmux_hook_attention "waiting for approval"
vmux_hook_progress 50
```

## Browser, markdown, remote

```sh
vmux open-url https://example.com
vmux browser snapshot https://example.com
vmux browser screenshot https://example.com
vmux browser links https://example.com
vmux markdown open README.md
vmux remote ssh user@host --command "claude"
vmux remote tmux user@host --session work
```

## Ports

Listening ports owned by pane processes (Linux `/proc` scanner). Full guide:
[ports.md](ports.md).

```sh
vmux ports list
vmux ports list --workspace ws-2 --json
vmux ports ssh-cmd 5173
vmux config set ports.ssh_host 'user@devbox'
vmux ports forward 3000 --via tailscale
vmux ports unforward 3000
```

```sh
vmux config set ports.enabled true
vmux config set ports.ignore 5432,6379
vmux config set ports.notify toast          # toast | banner | off
```

## Relay

Default port **4399**, change anytime. See [relay.md](relay.md).

```sh
vmux config set relay.port 4400
vmux relay serve
vmux relay serve --port 4400
vmux relay serve --listen 127.0.0.1:4400
vmux relay status
vmux relay devices list
vmux relay devices revoke <device_id>
```

## Config, actions, skills

```sh
vmux config show
vmux config init
vmux config set ui.prefix_key Ctrl-a
vmux config set ui.sidebar_collapsed true
vmux config set ui.colors contrast
vmux config set ui.layout flat
vmux config set relay.port 4399
vmux actions list
vmux actions run test
vmux skills list
vmux skills install vmux-control --dir .vmux/skills
```

Project actions are defined in `vmux.json` and can also be run from the TUI with
`Ctrl-b A`. See [config.md](config.md) and [config.schema.json](config.schema.json).

## Session ops

```sh
vmux daemon                 # start the daemon without attaching
vmux sessions               # running and persisted sessions
vmux --session work attach  # reattach, e.g. after an SSH drop
vmux list
vmux status
vmux logs --lines 200
vmux doctor
vmux smoke
vmux stop
```

Most commands auto-start the daemon, wait for the socket, then run. You rarely
need `vmux daemon` by hand.

Runtime files live under `$XDG_RUNTIME_DIR/vmux`, falling back to
`/tmp/vmux-$UID/vmux`:

| File | Purpose |
|------|---------|
| `<session>.sock` | CLI control socket |
| `<session>.pid` | Daemon process id |
| `<session>.log` | Daemon stdout and stderr |

Session state is persisted separately, in `~/.local/state/vmux/`.

## Full command surface

```text
attach  daemon  new-pane  split  run  open-url  url-snapshot  url-links
browser  agent  remote  markdown  actions  skills  config  ports  relay
send  send-key  send-image  broadcast  read-screen  search  clear-pane
copy-pane  paste  clipboard  kill-pane  duplicate-pane  prune  restart-pane
move-pane  swap-panes  title  tab  move  metadata  wait  resize
focus  focus-pane  zoom  workspace  surface  progress  hooks
set-progress  set-status  notify  notifications  events
clear-notifications  jump-notification  identify  list  agents
status  sessions  logs  doctor  smoke  stop
```
