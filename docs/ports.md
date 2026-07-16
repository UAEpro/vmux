# Ports

vmux watches processes under each pane and surfaces **listening TCP ports** they
own — the Vite server your agent just started, a local API on `3000`, whatever
is bound. Detection feeds the sidebar, notifications, CLI helpers, and optional
Tailscale forwarding.

**Linux only** for detection (reads `/proc/net/tcp{,6}` and `/proc/<pid>/fd`).
On macOS the ports list is empty; use the same CLI shape once Linux detection is
available on the host you care about. No `ss` dependency for the main scanner.

## How detection works

A dedicated background loop (default every `ports.poll_secs`, not the snapshot
hot path):

1. Collects each pane’s root PID and walks descendants via a single `/proc`
   ppid map.
2. Reads listening sockets from `/proc/net/tcp` and `/proc/net/tcp6`.
3. Matches socket inodes to pane process trees.
4. Diffs opens/closes → `port-opened` / `port-closed` events, optional
   notification, and workspace sidebar chips (`host`, `port`, `pids`,
   `process`, `pane`).

`vmux ports list` triggers an opportunistic rescan so you are not stuck waiting
for the next tick. The relay listen port (`relay.port`, default 4399) is always
ignored.

Configure filters with `ports.*` (see [config.md](config.md) and
[config.schema.json](config.schema.json)).

```sh
vmux config set ports.enabled true          # default
vmux config set ports.poll_secs 2
vmux config set ports.ignore 5432,6379      # never surface these
vmux config set ports.ignore_processes ssh,sshd
vmux config set ports.ignore_ephemeral true  # default: hide kernel ephemeral range
```

Array keys (`ports.ignore`, `ports.ignore_processes`) are comma-separated on
the CLI; edit `config.json` for complex lists.

## CLI

```sh
# List detected ports (optional workspace filter)
vmux ports list
vmux ports list --workspace ws-2 --json

# Print an ssh -L one-liner (run this on your laptop)
vmux ports ssh-cmd 5173
# Override host via config:
vmux config set ports.ssh_host 'user@devbox'
vmux ports ssh-cmd 5173

# Expose a detected port on the Tailscale interface (daemon TCP proxy)
vmux ports forward 3000
vmux ports forward 3000 --via tailscale

# Stop that forward
vmux ports unforward 3000
```

| Command | Purpose |
|---------|---------|
| `ports list` | Table of port / process / pane / workspace / host / forward URL |
| `ports ssh-cmd <port>` | Print `ssh -L port:127.0.0.1:port user@host` for local use |
| `ports forward <port>` | Bind `<tailscale-ip>:<port>` → `127.0.0.1:<port>` in the daemon |
| `ports unforward <port>` | Tear down a Tailscale forward |

```sh
vmux config set ports.auto_forward false    # default — never open tunnels alone
vmux config set ports.forward_via ask       # ask | tailscale | ssh (auto only does tailscale)
vmux config set ports.ssh_host 'user@my-devbox'
vmux config set ports.notify toast          # toast | banner | off
```

`ports.notify` controls how the attach UI surfaces **new** listeners
(notification feed / silent). Detection itself still updates the workspace row.

**SSH path:** `ssh-cmd` only prints a command — your **local** `ssh` client must
run it (or use ControlMaster `ssh -O forward -L …`). The remote daemon cannot
create `-L` forwards for you.

**Tailscale path:** `ports forward` starts an in-process proxy on the machine’s
Tailscale IPv4. Requires `tailscale` online. Never binds `0.0.0.0`. Forwards are
runtime-only (not persisted); pane exit and `unforward` stop them.

## Security

- Forwards never bind all interfaces. Only the Tailscale IP (or an already
  wildcard-local listener’s advertised URL).
- The phone **relay** is a separate surface and also refuses public binds —
  see [relay.md](relay.md). Do not confuse `relay.port` (default **4399**) with
  application ports (3000, 5173, …). Change the relay with
  `vmux config set relay.port 4400` or `vmux relay serve --port 4400`.
- `auto_forward` defaults **off**.
- Put noisy ports/processes in `ports.ignore` / `ports.ignore_processes`.

## Sidebar and attach UI

When ports are present, the workspace detail row can show short labels
(`:3000`).

In attach:

- Command palette → **ports**, or prefix **`Ctrl-b o`**
- **j/k** select · **Enter** focus pane · **c** copy `ssh -L` (OSC 52 + tools)
- **f** Tailscale forward · **x** unforward · **r** rescan · **Esc** close

CLI remains available: `vmux ports list`.

## Troubleshooting

| Symptom | Check |
|---------|--------|
| No ports ever show | Linux? `ports.enabled`? Process listening under the pane tree? |
| Wrong / noisy ports | `ignore` / `ignore_processes`; leave `ignore_ephemeral` on |
| `ssh-cmd` host wrong | `ports.ssh_host` or `$SSH_CONNECTION` / hostname |
| Forward fails | `tailscale ip -4` works? Port already detected? `AddrInUse`? |
| Port gone after stop | Expected — forwards are not restored across daemon restart |

More: [troubleshooting.md](troubleshooting.md).
