# Phone relay

`vmux relay` speaks the community
[Cmux Remote](https://github.com/NewTurn2017/cmux-remote) HTTP and WebSocket
protocol. Point that iPhone app at your Tailscale IP and the relay port (default
`4399`, **configurable**) and you can drive vmux workspaces and panes from your
phone.

```text
iPhone (Cmux Remote)  ── Tailscale ──►  vmux relay :port  ── Unix socket ──►  vmux daemon
                                         (default 4399)
```

The relay is **on by default**. On `vmux attach`, a managed relay starts if it is
not already running (bind: Tailscale IP when available, otherwise localhost).
Turn it off if you do not want anything listening for the phone app.

Starting or stopping the relay does not change how attach, the CLI, or the
daemon behave beyond that process.

> This is a compatibility layer, not an official Manaflow product. Official cmux
> Mobile Connect is a different stack and will not work with it. Protocol drift
> in the App Store app may require relay updates.

## Port (not fixed)

The historical Cmux Remote default is **4399**, and vmux uses that out of the
box — but the port is **not** hard-wired.

| How | Example |
|-----|---------|
| Config (managed relay / attach auto-start) | `vmux config set relay.port 4400` |
| CLI override | `vmux relay serve --listen 100.x.y.z:4400` |
| relay.json `listen` | `"listen": "127.0.0.1:4400"` |

Whatever you choose, the phone app (and paste-page URLs) must use the **same**
port. `relay.port` is an integer **1–65535** (`0` is rejected).

## Settings

In `vmux attach`, open **⚙ set** and find the **mobile relay** section:

| Setting | Meaning |
|---------|---------|
| mobile relay | `on` / `off` (default **on**). When on, attach auto-starts the relay. |
| relay bind | `auto` (Tailscale IP if online, else localhost), `tailscale`, or `local` |
| relay port | TCP port (default `4399`) |
| relay localhost | Allow device registration from `127.0.0.1`, for dev |
| paste page | Serve `/paste` for screenshot paste |
| phone-fit resize | Leased view-size overrides (`relay.allow_view_resize`) |

There is deliberately no "all interfaces" option. The relay refuses to bind
`0.0.0.0` or `::`, so it will not end up exposed on every NIC. Phone access goes
over Tailscale or localhost.

The same settings from the CLI:

```sh
vmux config set relay.enabled true       # default
vmux config set relay.enabled false      # disable auto-start
vmux config set relay.bind auto          # auto | tailscale | local
vmux config set relay.port 4399
vmux config set relay.allow_localhost false
vmux config set relay.allow_paste true
vmux config set relay.allow_view_resize false
```

With `relay.enabled` set (the default), the next `vmux attach` starts a managed
relay process. Turning it off stops it.

## Running it by hand

```sh
vmux relay serve

# Custom port / address
vmux relay serve --listen 127.0.0.1:4400

# Same-machine testing, skipping the Tailscale whois check
vmux relay serve --allow-localhost --listen 127.0.0.1:4399

vmux relay status
vmux relay devices list
vmux relay devices revoke <device_id>
```

The relay auto-starts a session daemon if one is not already up.

## Configuration

User-level preferences (`relay.*`) live in the main vmux config
([config.md](config.md), [config.schema.json](config.schema.json)).

Runtime relay file written on first run to `~/.config/vmux/relay.json`:

```json
{
  "listen": "127.0.0.1:4399",
  "allow_login": [],
  "allow_localhost": false,
  "allow_tailnet_cgnat": true,
  "default_fps": 15,
  "idle_fps": 5,
  "session": "default"
}
```

| Key | Meaning |
|-----|---------|
| `listen` | Host and port. Must not be `0.0.0.0` or `::`; the relay refuses to start. Change the port here or via `relay.port` / `--listen`. |
| `allow_login` | Tailscale login names allowed to pair. Empty means any successful `tailscale whois`. |
| `allow_localhost` | Allow `127.0.0.1` registration. Also settable with `VMUX_RELAY_ALLOW_LOCALHOST=1`. |
| `allow_tailnet_cgnat` | Accept `100.64.0.0/10` peers without a whois lookup. Practical with Tailscale. |
| `bootstrap_secret` | Optional shared secret for restricted pairing flows. |
| `session` | The vmux session the relay attaches to. |

Device tokens are stored in `~/.local/state/vmux/relay-devices.json`. Revoke a
lost phone with `vmux relay devices revoke`.

## Pairing a phone

1. Install Cmux Remote, or another client that speaks the same wire protocol.
2. Run Tailscale on the phone and on the Linux host, on the same tailnet.
3. On the host, run `vmux relay serve` (or attach with `relay.enabled`).
4. In the app, set host to your `tailscale ip -4` address and port to your
   relay port (**4399** unless you changed `relay.port` / `--listen`).
5. Pair, list workspaces, open a surface.

To check the relay is reachable before you touch the phone:

```sh
PORT=4399   # or whatever you configured
curl -s http://$(tailscale ip -4):${PORT}/v1/health
# {"ok":true,"version":"…","backend":"vmux",…}
```

## Phone-fit pane sizing

Off by default — a phone glancing at a pane must not resize it under whoever
is at the desk. Opt in with:

```sh
vmux config set relay.allow_view_resize true
```

Once enabled, `surface.subscribe` accepts optional `view_cols` / `view_rows`
params. When a client sends both, the relay holds a leased view-size override
on that pane:
the PTY runs at `min(desktop layout, phone view)` per axis — tmux's "smallest
client wins", scoped to the one pane being viewed. The lease is re-signed on
every poll cycle and expires ~10s after the phone vanishes (crash, signal
loss), so the pane always returns to its desktop size by itself; unsubscribing
restores it immediately. The desktop attach UI dims the pane's unused margin
with a "sized by phone" note while an override is active.

Clients that don't send a view size — and all clients while the gate is off —
get the previous behaviour: full-width rows, wrapped client-side. Zoomed panes
refuse the override.

## The paste page

The relay also serves a browser page for getting screenshots into agents when
you are SSH'd in from another machine — the case where Ctrl+V inside Claude
Code can never work, because the image is in *your laptop's* clipboard and the
agent only checks the host's.

Open `http://<host>:<port>/paste` in any browser on the tailnet (default port
**4399**), press `Cmd+V`/`Ctrl+V` (or drop an image file), and the relay saves
the image on the host and types its path into the active pane. Claude Code and
friends pick the path up as an attachment. Nothing to install on the laptop or
phone; the page pairs itself with the relay using the same device registration
as the app, and the token sticks in browser storage.

The endpoint behind it is `POST /v1/paste` (raw image bytes, `Bearer` device
token). `?pane=pane-2` targets a pane other than the active one, `?enter=1`
submits immediately. Uploads land in `~/Downloads/vmux-remote/` and are capped
at 16 MiB; bodies that are not a real png/jpeg/gif/webp/bmp are rejected.

The page is on by default (uploads still require a paired device). To turn it
off — `/paste` and `/v1/paste` then return 404 — flip "paste page" in the
Settings panel, or:

```sh
vmux config set relay.allow_paste false
```

For scripting the same thing over plain SSH, see `vmux send-image` in the
[CLI reference](cli.md).

## Troubleshooting

Pairing and reachability issues are covered in
[troubleshooting.md](troubleshooting.md).
