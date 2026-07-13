# Changelog

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
