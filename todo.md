# vmux progress tracker

Last updated: 2026-07-13 (session-listing fix, test isolation, relay lock safety, CI gates).

**Gates:** `cargo fmt --check` · `cargo clippy -D warnings` · **279 tests** · CI.

---

## Just finished (this pass)

Correctness:

| Item | Status |
|------|--------|
| `vmux sessions` listed the daemon's own state files (`update-check`, `relay-devices`) as sessions — on **every install** once the daily update check had run | **Fixed** — `RESERVED_STATE_STEMS` in `paths.rs`; also closes a collision where a session of that name could clobber the relay's device store |
| Relay `.expect()`ed on the shared device-auth mutex from per-connection threads — one panic bricked auth permanently | **Fixed** — `MutexExt::lock_or_recover` moved to `src/sync.rs` and applied to all 8 relay lock sites |
| `search_pane` / `copy_pane` took `session` while holding `panes`, against the documented lock order | **Fixed** — latent deadlock, closed |
| Shutdown's debounced save could re-create a state file a caller had just cleaned up | **Fixed** — `shutting_down` stops the save loop |
| `relay.json` (holds `bootstrap_secret`) written without an explicit mode | **Fixed** — 0600, matching the device store |

Tests — the suite was passing without guarding what it claimed to:

| Item | Status |
|------|--------|
| **CI's persistence e2e ran 0 tests and reported green** — `--exact` with a truncated name matched nothing | **Fixed** — full test path + a guard that fails if the filter stops matching |
| The same e2e asserted `output \|\| scrollback` — but the P0 it guards was *scrollback dropped while output survived*, so it could not fail | **Fixed** — both fields asserted independently |
| Restore test asserted `matches!(status, Running \| Exited)` — accepted either outcome | **Fixed** — waits for the reaper, asserts `Exited` |
| Generation counter — the documented #1 footgun ("a mutation that forgets to bump it is invisible to the UI") | **Now guarded** — mutations bump, reads don't |
| `relay/auth.rs` had **zero** tests on the pairing decision matrix | **5 tests added** — secret gate, both header forms, localhost opt-in, per-pairing device identity, CGNAT. Mutation-checked: reintroducing the old secret-gate bug fails them |
| Daemon tests leaked 500+ lock files and 9 state files into the developer's **real** XDG dirs, showing up as phantom sessions in `vmux ls` | **Fixed** — `TestSession` RAII guard, collision-proof names, cleans up on the panicking path |

Product / infra:

| Item | Status |
|------|--------|
| Scrollback was hardcoded at 16 KB (~200 lines, vs tmux's 2000) | **Configurable** — `ui.scrollback_bytes`, default 200 KB |
| No `[profile.release]` at all | **Added** — LTO + codegen-units=1 + strip: **5.90 MB → 4.56 MB** (23%). `panic` deliberately left at unwind so poison recovery still works |
| No MSRV check despite advertising 1.87; no `--locked`; release published without ever running tests | **Fixed** in CI + release workflows |
| No supply-chain gating on a crate that binds a port and mints tokens | **Added** — `cargo audit` job + Dependabot (ratatui/crossterm grouped) |
| `thiserror` declared but never used | **Removed** |
| Code comments cited `bugs.md` / `newimp.md` / `improve.md`, which are not in the repo | **Stripped** — rationale kept, dead references gone. `improve.md` untracked |

---

## Still not done (honest backlog)

Deliberately deferred — these are real, but each is a design change rather than a fix,
and none is a correctness bug today.

1. **PTY read batching.** A pane spewing output takes the global `panes` mutex once per
   4 KB chunk (~25k acquisitions/sec at 100 MB/s), and `touch()` bumps the generation on
   every chunk so the UI's `since` short-circuit never fires. Drain with a deadline and
   take the lock once per batch. Biggest single perf win available.
2. **`descendant_pids` walks all of `/proc` under the `panes` lock**, on the PTY hot path,
   triggered by any OSC title (most shells retitle every prompt). Move it to a background
   thread behind a cached flag.
3. **Lean snapshots still serialize `output` + `output_formatted` for every pane in the
   session**, visible or not, on every poll. Trim to on-screen panes; drop the client-side
   full `serde_json::Value` deep-compare.
4. **`save()` pretty-prints the whole session** and is called synchronously by ~25 handlers.
   Use `to_vec`, and route request-path saves through the 400 ms debouncer.
5. **No protocol version.** Compatibility rests on serde defaults. Add `protocol_version` to
   `Ping` and `#[serde(other)]` to the persisted enums so a downgrade degrades instead of
   quarantining the state file.
6. **The attach client exits the TUI on any transport blip** — it should retry/backoff,
   since the daemon is designed to outlive clients.
7. **Scroll clamp mismatch**: `scrollback_lines` counts raw output lines, but the styled
   path can only render `SCROLLBACK_FORMATTED_ROW_CAP` (500) rows, so scrolling past ~524
   lines silently stops moving.
8. **No daemon socket timeouts / connection cap** — one stuck peer pins a thread forever.
9. **Deeper module splits** — `ui/mod.rs` is 10.4k lines / 337 fns; `impl Server` is ~3.4k.
10. **No benchmarks**, despite the perf reasoning throughout. Nothing measures the vt100 →
    lean-snapshot path the design is built around.
11. Deps drifting: ratatui 0.26 (0.30 out), crossterm 0.27, tungstenite 0.24, dirs 5.
    Dependabot now surfaces these.

---

## Verify

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo test daemon::tests::e2e_restart_preserves_scrollback_across_save_reload -- --exact
cargo run -- smoke
```
