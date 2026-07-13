# Phone relay

`vmux relay` speaks the community
[Cmux Remote](https://github.com/NewTurn2017/cmux-remote) HTTP and WebSocket
protocol. Point that iPhone app at your Tailscale IP on port `4399` and you can
drive vmux workspaces and panes from your phone.

```text
iPhone (Cmux Remote)  ── Tailscale ──►  vmux relay :4399  ── Unix socket ──►  vmux daemon
```

The relay is opt-in. Starting it does not change how attach, the CLI, or the
daemon behave, and if you never run it, nothing listens on the network.

> This is a compatibility layer, not an official Manaflow product. Official cmux
> Mobile Connect is a different stack and will not work with it. Protocol drift
> in the App Store app may require relay updates.

## Turning it on

In `vmux attach`, open **⚙ set** and find the **mobile relay** section:

| Setting | Meaning |
|---------|---------|
| mobile relay | `on` / `off`. When on, attach auto-starts the relay. |
| relay bind | `auto` (Tailscale IP if online, else localhost), `tailscale`, or `local` |
| relay localhost | Allow device registration from `127.0.0.1`, for dev |

There is deliberately no "all interfaces" option. The relay refuses to bind
`0.0.0.0` or `::`, so it will not end up exposed on every NIC. Phone access goes
over Tailscale or localhost.

The same settings from the CLI:

```sh
vmux config set relay.enabled true
vmux config set relay.bind auto          # auto | tailscale | local
vmux config set relay.allow_localhost false
```

With `relay.enabled` set, the next `vmux attach` starts a managed relay process.
Turning it off stops it.

## Running it by hand

```sh
vmux relay serve

# Same-machine testing, skipping the Tailscale whois check
vmux relay serve --allow-localhost --listen 127.0.0.1:4399

vmux relay status
vmux relay devices list
vmux relay devices revoke <device_id>
```

The relay auto-starts a session daemon if one is not already up.

## Configuration

Written on first run to `~/.config/vmux/relay.json`:

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
| `listen` | Host and port. Must not be `0.0.0.0` or `::`; the relay refuses to start. |
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
3. On the host, run `vmux relay serve`.
4. In the app, set host to your `tailscale ip -4` address and port to `4399`.
5. Pair, list workspaces, open a surface.

To check the relay is reachable before you touch the phone:

```sh
curl -s http://$(tailscale ip -4):4399/v1/health
# {"ok":true,"version":"…","backend":"vmux",…}
```
