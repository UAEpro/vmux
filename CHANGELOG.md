# Changelog

## v0.5.0 — 2026-07-16

Feature release: ports panel, phone multi-viewer sizing, settings that
apply cleanly, notification/command-palette UI, website, and new tabs
that inherit the directory you `cd`'d into. Upgrade with the install
one-liner, `cargo install vmux-tui`, or wait for the in-app update notice.

### Added

- **Ports subsystem** (`src/daemon/ports.rs`): Linux `/proc` scanner with
  pane attribution, open/close events + notifications, `ports.*` config,
  CLI + attach panel (`Ctrl-b o`), Tailscale TCP proxy. See `docs/ports.md`.
- **Multi-viewer phone-fit** (`daemon/view_size.rs`): when several phones
  watch the same pane, the PTY uses `min()` across live leased viewers
  via `viewer_id` on set/clear view size (no more last-writer-wins).
- **`Events { since }`** incremental poll; `vmux events --since` / follow
  for agents and scripts.
- **Settings panel rows** for relay port, CGNAT, phone-fit resize, and
  ports toggles (with deferred apply so typing a port does not thrash
  the listener until you leave the row or close Settings).
- **Config JSON Schema** at `docs/config.schema.json` covering `ui.*`,
  `relay.*`, `agent_titles.*`, and `ports.*` (with `$schema` usage notes
  in `docs/config.md`).
- **Website** at [vmux.sh](https://vmux.sh) (install, Remote, privacy,
  support pages).
- **Authenticated remote push notifications** for the phone workflow
  (pairing-backed; production host config stays private).
- **Command palette redesign**: sections, icons, clearer chrome, one
  line per command, scrollable list (`src/ui/command_palette.rs`).
- **Notifications panel redesign**: color cards, clear-all, theme
  selection, hover, and click-to-select/jump.
- **UI modules**: `theme`, `settings_panel`, `ports_panel`,
  `command_palette` extracted from the attach UI.
- **`protocol_version`** on `DaemonInfo` / Ping.
- **Attach reconnect** via `request_with_retry`.
- **Daemon connection cap** (256) + socket timeouts.
- **`cargo bench --bench hot_path`** micro-benchmarks.
- **Docs:** architecture, troubleshooting, ports, config schema.
- **Phone contract CI** workflow for the Remote wire protocol.

### Changed

- **New tabs and panes open in the live shell directory.** After you
  `cd` in a pane, a new tab (or split) starts in that path — not the
  directory the workspace was first opened in. Linux uses
  `/proc/<pid>/cwd`; macOS uses `proc_pidinfo(PROC_PIDVNODEPATHINFO)`.
- **Mobile relay enabled by default** so phone pairing works out of the
  box.
- **Relay port** configurable (`relay.port`, `vmux relay serve --port`).
- **Branding:** package `homepage` is `https://vmux.sh` (repository
  remains GitHub). Windows is explicitly unsupported; WSL is fine
  (it is Linux).
- **Scroll:** styled history cap 2500; UI clamp matches styled length.
- **Perf:** PTY batching; compact saves; agent_inside `/proc` off the
  PTY hot path.
- Repo hygiene: clean `todo.md`; scratch notes stay gitignored.

### Fixed

- **Finished Grok turns no longer flip back to busy** when a late busy
  status arrives after the turn is done.
- **Notification feed spam** reduced; jumps are tab-aware.
- **macOS CI / clippy:** Linux-only port scanner helpers and proc parsers
  gated so the macOS matrix stays clean.
- **Settings relay port** no longer rebinds on every keystroke (deferred
  apply until leave-row / close).

## v0.4.1 — 2026-07-14

Bug-fix and polish release. Upgrade with the install one-liner,
`cargo install vmux-tui`, or wait for the in-app update notice.

### Fixed

- **Fullscreen agents (Codex and similar) can scroll with the mouse wheel.**
  xterm DECSET 1007 alternate-scroll is tracked and wheel events become
  cursor keys while the app is on the alternate screen. Mouse tracking still
  wins when the app requests it; ClearPane keeps negotiated input modes so
  the child does not need to renegotiate after a clear.
- **True pane history from the live parser** (ring replay demoted to
  fallback), and orphan panes are reaped at load instead of freezing tab
  titles onto panes.

### Added

- **Automatic tab names for every coding agent.** Same free pipeline for
  Claude, Codex, Grok, Aider, Cursor, and other detected agents:
  1. OSC terminal title (when the agent sets one)
  2. `UserPromptSubmit` hook prompt via `vmux hooks event`
  3. Meaningful `set-status busy --message "…"` text
  4. Optional LLM screen summary as last resort
- **Real Grok Build lifecycle hooks** at `~/.grok/hooks/vmux.json` (sidebar
  status + prompt-based tab titles). The control skill is still installed
  alongside. Skill-only installs are reported incomplete until hooks are
  present.

### Changed

- Docs and CLI point Grok install at hooks (not skill-only).
- Feature worktrees are documented under `.worktrees/` (gitignored).

## v0.4.0 — 2026-07-14

Feature release, built around the phone (vmux Remote) workflow.

### Added

- **Phone-fit pane sizing.** A phone viewing a pane can shrink its PTY to fit
  the phone screen; the pane returns to its desktop size when the phone stops
  watching (leased overrides — a phone that loses signal restores within
  seconds). Off by default: `vmux config set relay.allow_view_resize true`.
  The desktop UI dims the pane's unused margin with a "sized by phone" note
  while active. Also exposed as `vmux view-size` for scripts and agents.
- **Scrollback on the phone.** `surface.scrollback` replays pane history with
  colour to remote viewers.
- **Tab and pane management from the phone.** Create/switch/rename/close tabs,
  rename and restart panes over the relay.

### Changed

- **The sidebar is local-only now.** vmux no longer queries GitHub for PR
  state — the background `gh pr view` polling silently exhausted the
  anonymous API quota (breaking `gh auth login` on the host) and, once
  logged in, the account's 5,000/hour quota. Branch, cwd, ports, and agent
  status remain; PR state belongs to `gh` and the browser. Old clients and
  state files still decode.

### Fixed

- Duplicate notifications are deduped.
- Two agent-hooks tests raced each other under CI's shared XDG environment.

## v0.3.1 — 2026-07-13

Bug-fix release. Upgrade with the install one-liner, `cargo install vmux-tui`,
or wait for the in-app update notice.

### Fixed

- **Running vmux commands inside a vmux pane could spawn zombie daemons.**
  Panes inherited the daemon's internal `VMUX_DAEMONIZE=1` marker, and any
  command that started a new session from inside a pane (`vmux smoke`,
  `vmux --session <new> attach`, …) silently forked itself into a detached
  daemon and exited without doing anything. The marker is now scrubbed from
  pane environments and only the `vmux daemon` subcommand honors it.
- **`vmux sessions` listed the daemon's own state files as sessions.**
  `update-check` (and `relay-devices` with the relay in use) appeared as a
  phantom session on every install once the daily update check had run.
- **Restarting a session immediately after `vmux stop`/shutdown could fail**
  with "vmux daemon helper exited" — the outgoing daemon kept the session
  lock during its exit grace period. Shutdown now releases the lock at once.
- **A panic in one relay connection could permanently break relay auth** for
  every device until restart (poisoned device-store lock). The relay now uses
  the same poison-tolerant locking as the daemon.
- `relay.json` (which can hold `bootstrap_secret`) is written with mode 0600.
- A daemon shutting down can no longer re-create its state file after a
  caller has cleaned it up.

### Added

- `ui.scrollback_bytes` config key: retained output per pane, default 200 KB
  (~2500 lines; was a fixed 16 KB ≈ 200 lines). Clamped to 16 KB–5 MB, takes
  effect on the next daemon start.

### Changed

- Release binaries are built with LTO and symbol stripping: ~23% smaller.
- CI actually runs now (the workflow file was invalid since it was added and
  every previous run executed zero jobs), and gained MSRV (1.87), security
  audit, and `--locked` checks. Releases run the test suite before publishing.

## v0.3.0 — 2026-07-13

- Multi-platform prebuilt binaries: Linux x86_64/aarch64 (static musl) and
  macOS Apple silicon/Intel, with checksums; `install.sh` picks the right one.
- Published to crates.io as `vmux-tui`.
- `vmux send-image`: paste screenshots into agents over SSH.
- Relay: browser paste page (`/paste`) for zero-install screenshot paste, with
  a settings toggle.
- Agent tabs are named after the work running in them.

## v0.2.0 — 2026-07-13

- Renamed to `vmux` (crate `vmux-tui`); prepared for crates.io.
- Pane selections copy to the system clipboard.
- Leaner attach-UI polling (scrollback dropped from the poll payload).
- Update-availability notification and `--version`.

## v0.1.0 — 2026-07-11

- First release: detached daemon with persistent sessions, workspaces/tabs/
  panes, agent status sidebar, agent hooks (`vmux hooks install`), JSON
  protocol over a Unix socket, opt-in phone relay.
