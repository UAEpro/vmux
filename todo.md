# vmux improvement roadmap

Ideas and fixes to make the project better. Work through later; not committed to a schedule.

---

## Hierarchy redesign (approved plan) — Workspace → Tab → Panes

Full plan: session plan file / conversation plan for hierarchy redesign.

- [x] **PR1 — Model + migration** (done)
- [x] **PR2 — Daemon + protocol** (ListTabs/NewTab/SwitchTab/RenameTab/CloseTab, MovePaneInLayout; legacy pane-tab errors)
- [x] **PR3 — CLI** (`vmux tab …`, `vmux move left|right|up|down`)
- [x] **PR4 — Attach UI** (workspace tab bar, edge-aware pane controls, control-bar spacing, close = pane)
- [x] **PR5 — Cleanup** (README hierarchy notes; legacy helpers allowed dead_code)

---

## Phase A — high ROI, low risk (1–2 days)

- [ ] CI: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`, `cargo build --release`
- [ ] Snapshot light/heavy split for attach poll (layout + status vs full scrollback)
- [ ] Input batching + adaptive UI poll (idle slower, typing faster)
- [ ] Document unbounded `vmux wait` (no timeout = wait forever); optional one-shot stderr notice
- [ ] Proper git history + `LICENSE` (MIT per Cargo.toml)

## Phase B — structure & reliability (about 1 week)

- [ ] Split `daemon.rs` into modules (`server`, `pty`, `tabs`, `workspace`, `snapshot`, `browser`, `save`)
- [ ] Split `ui.rs` into modules (`app`, `render`, `input`, `sidebar`, `palette`, `mouse`)
- [ ] Integration tests: temp socket, new-pane → send → read-screen → kill
- [ ] Concurrency tests: parallel rename tab + input + save
- [ ] Event-driven `wait` (Condvar/channel from `mark_exited` instead of 50ms spin)
- [ ] Structured event stream for agents (JSONL on socket or file)
- [ ] Shell completions (`clap_complete` for bash/zsh/fish)

## Phase C — product differentiation

- [x] **Phone relay (Cmux Remote compatible)** — opt-in `vmux relay serve`
  - HTTP `/v1/health`, `/v1/state`, device register/APNs/revoke
  - WebSocket `/v1/ws` hello + JSON-RPC (workspace/surface/screen/keys/events)
  - Maps to existing Unix-socket daemon API only (no attach/daemon behaviour change when idle)
  - Tailscale whois + localhost / CGNAT auth options; device token store
- [ ] Relay: harden Tailscale whois JSON variants; optional systemd user unit
- [ ] Relay: APNs push fanout (currently accepts token registration only)
- [ ] Named layout presets (“agents-2x2”) restore in one command
- [ ] Per-pane restart policy (`never` | `on-fail` | `always`) for agents
- [ ] Protocol versioning (`"v": 1` on requests/responses)
- [ ] `vmux wait --status attention` (wait until agent needs input, not only exit)
- [ ] Attention-focused jump UX from attach
- [ ] Optional browser private-host policy (`browser.allow_private_hosts`)
- [ ] Packaging: `cargo install` docs, release binaries, changelog

---

## Architecture

- [ ] Extract pure helpers first (layout, runtime keys, OSC parsing, UTF-8 stream)
- [ ] Keep protocol wire types thin; move handlers next to domain logic
- [ ] Use or remove empty `src/vmux/` directory (module root or delete)

## Reliability & concurrency

- [ ] Bound concurrent request work (thread pool / queue, or cap accept workers)
- [ ] Snapshot versioning: client sends `since_version`, daemon returns delta
- [ ] Separate light snapshot (layout + status) vs full (scrollback/formatted)
- [ ] UI polls light; full only for focused pane
- [ ] Graceful daemon shutdown on SIGTERM/SIGINT (kill PTYs, flush save, remove socket/pid)

## Attach TUI / UX

- [ ] Adaptive poll: idle 200–300ms, active typing 30–50ms
- [ ] Coalesce keystroke RPCs (batch 5–10ms)
- [ ] Show `action_error` more visibly (toast strip, auto-clear)
- [ ] RPC busy indicator when daemon >100ms
- [ ] Copy/selection polish (OSC 52 clipboard where available)
- [ ] Search-in-pane with next/prev highlights
- [ ] Better zoom mode status (“zoomed pane-2 · q to exit”)
- [ ] Synchronized input / broadcast visual indicator
- [ ] Sidebar: reliable drag-reorder persistence; collapse metadata until hover
- [ ] Jump-to-attention with one key from anywhere
- [ ] Empty-state help when attach has no panes
- [ ] Surface `doctor` on attach when socket is broken

## Agent / automation

- [ ] Structured pane events (JSONL stream)
- [ ] `vmux wait --status attention`
- [ ] Pane labels / roles via metadata (`role=coder|reviewer`)
- [ ] Prompt templates (`agent send --template continue`)
- [ ] Stable output rings with byte limits in API
- [ ] Idempotent command ids (CLI retries without double-send)
- [ ] Protocol version field for non-breaking evolution
- [ ] `--json` consistency across all CLI commands

## PTY / terminal fidelity

- [ ] Optional CSI u / kitty keyboard mode for better key chords
- [ ] Focus events (DECSET 1004) so apps know focused pane
- [ ] Truecolor check in `doctor`
- [ ] Configurable scrollback cap in `config.json`
- [ ] Dead-child auto-restart policy per pane
- [ ] Auto pane title from OSC 0/2

## Testing

- [ ] Integration harness on temp socket
- [ ] Tab lifecycle regression (migrate / no orphan) — unit test exists; expand
- [ ] Layout fuzz: random split/remove/focus sequences
- [ ] Expand `vmux smoke` as CI gate
- [ ] Pure UI render tests for layout rects (no real TTY)

## Repo / shipping

- [ ] Real `git init` / remote + meaningful commits
- [ ] `LICENSE` file
- [ ] Short `CONTRIBUTING.md`
- [ ] Split README: landing + `docs/` (CLI reference, protocol, config schema)
- [ ] Install script / `cargo install --path .` docs
- [ ] Version bumps + changelog
- [ ] Man page or `vmux help topics` for large command surface
- [ ] Shell completions

## Security / multi-user

- [ ] Document socket `0600` and trust model
- [ ] Optional auth token on socket for multi-user machines
- [ ] Config: `browser.allow_private_hosts` (default true for local DX)
- [ ] Avoid logging full pane output / secrets by default
- [ ] Redact secrets from logs if env is dumped

## Config & extensibility

- [ ] JSON Schema for `config.json` (editor autocomplete)
- [ ] Named layouts restore command
- [ ] Document hooks event schema; `hooks validate`
- [ ] Plugin discovery (`vmux-plugin-*` on PATH)
- [ ] Config knobs: `ui.poll_ms`, `scrollback_cap`

## Performance

- [ ] `append_output`: avoid full scrollback rebuild every chunk when possible
- [ ] Reduce Session clone cost on Snapshot (Arc / CoW / versioning)
- [ ] Cache pane `ansi_to_lines` per pane generation in UI
- [ ] Cache failed `gh`/`ss` lookups with TTL (avoid thrash)

## Small fixes (anytime)

- [ ] Default wait message if no timeout: “waiting forever (use --timeout)”
- [ ] Config: `ui.poll_ms`, `scrollback_cap`
- [ ] `doctor`: socket mode, stale pid, truecolor, `gh`/`git` present
- [ ] Remove or repurpose empty `src/vmux/`
- [ ] Clearer notification/event log rotation
- [ ] Pane title from OSC 0/2 auto-update
- [ ] `--json` everywhere for scripting

---

## Deliberately defer / avoid for now

- Full async rewrite unless attach latency becomes painful
- Replacing `portable-pty` without a strong reason
- Competing with tmux feature-for-feature
- Blocking localhost URLs by default (hurts local previews)
- Putting AI *inside* the daemon — vmux stays the stage; agents run in panes

---

## Positioning (keep clear)

> **vmux = detached PTY multiplexer + agent automation socket + Ratatui attach**

Grok / Claude Code / similar tools are agents *inside* panes. vmux is the stage they run on. Prefer improvements that make the stage more reliable, scriptable, and nice to attach to.

---

## Suggested first PR when resuming

**Light snapshot + adaptive UI poll + one integration test harness**

Improves day-to-day feel and protects future refactors.
