# Troubleshooting

Quick fixes for the problems people hit most. For architecture context see
[architecture.md](architecture.md); for config keys see [config.md](config.md).

## Attach fails

**Symptom:** `vmux attach` errors, hangs, or exits immediately.

| Check | What to do |
|-------|------------|
| Stale lock / dead daemon | `vmux sessions`, then `vmux doctor`. If a pid file exists but the process is gone, `vmux stop` (or remove the stale runtime files under `$XDG_RUNTIME_DIR/vmux` / `/tmp/vmux-$UID/vmux`) and attach again. |
| Wrong session | `vmux --session <name> attach`. Default session is `default`. |
| Socket permissions / path | Runtime dir must be owned by you, not a symlink, and not group/other-writable. `vmux doctor` reports this. |
| Debug build vs release | `cargo run -- attach` joins the **same** daemon as an installed `vmux`. Develop against a scratch session: `cargo run -- --session dev attach`. |
| Terminal too small | Extremely small TTYs can make the layout unusable; enlarge the window. |

```sh
vmux doctor
vmux status
vmux logs --lines 200
```

## Phantom sessions in `vmux sessions`

**Symptom:** Names like `update-check` or `relay-devices` appear as sessions.

Those are daemon state files that live next to session JSON under
`~/.local/state/vmux/`. Current builds exclude them via `RESERVED_STATE_STEMS`
in `paths.rs`. If you still see them:

1. Upgrade vmux.
2. Do not create a real session named `update-check` or `relay-devices` — that
   name collides with reserved stems.
3. Remove leftover junk only if you are sure it is not a session you care
   about: files under `~/.local/state/vmux/*.json` that are not sessions.

Daemon tests must use the `TestSession` guard so they never leak names into
your live state dir.

## Relay will not pair / phone cannot connect

See also [relay.md](relay.md).

1. Tailscale up on **both** phone and host, same tailnet.
2. Relay running: `vmux relay status` or rely on auto-start
   (`relay.enabled`, default on).
3. Port: default **4399**, but it is configurable:
   ```sh
   vmux config set relay.port 4399
   vmux relay serve --listen 127.0.0.1:4400
   ```
   The phone app must use the **same** port.
4. Bind mode: `relay.bind` is `auto` | `tailscale` | `local`. Localhost-only
   binds are unreachable from a physical phone over the tailnet.
5. Health probe:
   ```sh
   curl -s http://$(tailscale ip -4):4399/v1/health
   ```
6. Pairing policy: `allow_localhost` is for same-machine/dev only.
   `allow_tailnet_cgnat` is off by default (whois required).
7. Lost phone: `vmux relay devices list` then `vmux relay devices revoke <id>`.

The relay **never** binds `0.0.0.0` / `::`. If you need LAN-wide exposure,
that is intentionally unsupported — use Tailscale.

## Agent hooks not updating the sidebar

1. `vmux hooks status` — incomplete installs (e.g. Grok skill without hooks)
   are reported.
2. Reinstall: `vmux hooks install` (or `--agent claude|codex|grok|shell`).
3. Inside panes, confirm env: `echo $VMUX_PANE_ID` (legacy `LMUX_PANE_ID` is
   still set for compatibility). Hooks that only mention `LMUX_PANE_ID` may be
   flagged stale.
4. Manual probe:
   ```sh
   vmux set-status busy --message "working"
   vmux notify --status done --message "tests passed"
   ```
5. Shell integration: `eval "$(vmux hooks shell)"` in the rc file the pane
   actually sources.

Automatic tab titles need `agent_titles.enabled` (default on) and a daemon
restart after config changes (`vmux stop`, then attach).

## Scrollback missing or shorter than expected

- Retention is **`ui.scrollback_bytes`** (default 200 KB, clamp 16 KB–5 MB),
  applied on the **next daemon start**.
  ```sh
  vmux config set ui.scrollback_bytes 500000
  vmux stop
  vmux attach
  ```
- Session state lives in `~/.local/state/vmux/<session>.json`. Corrupt files
  are moved aside, not silently deleted — look for a `.bak` / quarantine
  sibling if restore failed.
- Lean attach polls omit full history except for panes you are scrolled back
  in; scrolling should still request history for the focused pane.
- Alternate-screen full-screen apps (e.g. some agents) use their own scroll
  behaviour; mouse wheel may map to cursor keys when DECSET 1007 is active.

## Port detection empty

See [ports.md](ports.md). Short version: install `ss`, leave `ports.enabled`
on, ensure the listener is a child of a pane process, and check
`ports.ignore` / `ignore_ephemeral`.

## Config changes seem ignored

- Many keys apply immediately; **`ui.scrollback_bytes`** and some title
  settings need a daemon restart.
- Malformed config is ignored on read (defaults) with a warning; mutation
  commands refuse to overwrite a broken file — fix or delete
  `~/.config/vmux/config.json` first.
- Symlinked (dotfiles) config is written **through** the link; a broken
  symlink is refused rather than replaced.

Validate against the schema:

```sh
# editors / CI: docs/config.schema.json
vmux config show
```

## Develop without killing your real session

```sh
cargo run -- --session dev attach
```

Never point a buggy debug build at the session where your agents live.

## Still stuck

```sh
vmux doctor
vmux logs --lines 200
vmux --version
```

Open an issue at https://github.com/UAEpro/vmux with the doctor output (redact
paths/secrets). Site and install: https://vmux.sh
