# vmux progress tracker

Last updated: 2026-07-16 (full-roadmap: docs, schema, branding, hygiene).

**Gates:** `cargo fmt --check` · `cargo clippy -D warnings` · `cargo test` ·
`cargo run -- smoke` · CI.

Site: [https://vmux.sh](https://vmux.sh)

---

## Shipped on this branch (full-roadmap)

| Area | What landed |
|------|-------------|
| **Docs** | `docs/ports.md`, `docs/troubleshooting.md`, `docs/architecture.md`; expanded `docs/config.md` / `docs/relay.md`; README Docs index |
| **Schema** | `docs/config.schema.json` for `ui.*`, `relay.*`, `agent_titles.*`, `ports.*` |
| **Branding** | `homepage = "https://vmux.sh"` in Cargo.toml; install/docs point at the site; Windows **not** supported (WSL OK); relay port clearly configurable |
| **Ports** | New `daemon/ports.rs` (`/proc` scanner on Linux), registry + open/close events, `ports.*` config, CLI `vmux ports list|ssh-cmd|forward|unforward`, Tailscale TCP proxy forward, pane-exit cleanup |
| **Relay port** | Already configurable via `relay.port`; CLI `vmux relay serve --port N` / `--listen host:port`; docs no longer imply fixed 4399 |
| **Protocol** | `protocol_version` on `DaemonInfo` / Ping |
| **Perf** | PTY full-buffer batching, compact (non-pretty) save JSON, styled scrollback cap 2500 |
| **Scroll clamp** | UI max-scroll respects styled history length; server cap raised to 2500 |
| **Reconnect** | Attach uses `request_with_retry` (backoff on connect/read blips) |
| **Socket limits** | Daemon: 256 connection soft-cap, 120s read / 30s write timeouts |
| **Hygiene** | Branding → vmux.sh, gitignore scratch reviews, clean todo |

Earlier baselines (still true on mainline history): scrollback `ui.scrollback_bytes`,
relay paste page + phone-fit view resize, agent tab titles, release LTO, CI
gates, reserved session stems.

---

## Remaining (long-tail)

1. **Further UI/daemon splits** — theme / settings_panel / ports_panel extracted;
   `ui/mod.rs` still ~11k (draw/event loop).
2. **Dependency major bumps** — ratatui 0.26→0.30 etc. via Dependabot + phone e2e.
3. **Per-session relay ports** — deferred by design for now.

## Shipped later on this branch

- Ports TUI (`Ctrl-b o`), agent_inside off hot path
- Multi-phone view size `min()` (`daemon/view_size.rs`, `viewer_id`)
- `Events { since }` + `vmux events --since` / follow uses server cursor
- `cargo bench --bench hot_path` micro-benchmarks

## followups branch

- UI splits: `theme.rs`, `settings_panel.rs`, `ports_panel.rs`, `command_palette.rs`
- `AgentStatus` `#[serde(other)]` for forward-compatible state
- Optional `phone-contract.yml` workflow (manual dispatch) for vmux-remote e2e

Historical design notes for phone-fit sizing and port forwarding were folded
into shipped docs; scratch `bugs.md` / `newimp.md` / `review-*.md` stay
gitignored and out of the tree.

---

## Verify

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo test daemon::tests::e2e_restart_preserves_scrollback_across_save_reload -- --exact
cargo run -- smoke
```
