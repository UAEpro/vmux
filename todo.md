# vmux progress tracker

Last updated: 2026-07-10 (e2e restart + module split + keystroke batching).

**Gates:** `cargo fmt --check` · `cargo clippy -D warnings` · **243+ tests** · CI.

---

## Just finished (this pass)

| Item | Status |
|------|--------|
| Restart e2e (`e2e_restart_preserves_scrollback_across_save_reload`) | **Done** — save → drop lock → reload → marker intact; CI step |
| Module split | **Done (practical)** — `daemon/{mod,browser}`, `ui/{mod,input_batch}`, `relay/{mod,auth}` |
| Keystroke batching | **Done** — ~8 ms coalesce, control keys flush immediately, paste flushes first |

### Layout now

```
src/
  daemon/
    mod.rs       # core daemon (~5.2k)
    browser.rs   # URL/fetch/HTML helpers (~800 lines)
  ui/
    mod.rs       # attach TUI
    input_batch.rs
  relay/
    mod.rs
    auth.rs      # registration + Tailscale identity
  …
```

---

## Still not done (honest backlog)

These remain **optional architecture / product** work — not open correctness bugs.

1. **Deeper module splits** — further slice `daemon/mod.rs` and `ui/mod.rs` (snapshot, pty, settings, control bar).
2. **Remove active-tab live view** — only `WorkspaceTab` owns layout.
3. **Full multi-process e2e** — spawn real `vmux daemon` binary under temp XDG (current e2e is in-process Server save/reload).
4. **ANSI conversion cache** + light-only attach poll for layout.
5. **Relay HTTP stack rewrite** (hyper/axum).
6. **Config** — preserve unknown JSON fields, schema.
7. **Product** — APNs, layout presets, restart policy, completions, release matrix.

---

## Bugs.md / prior newimp

All `bugs.md` items remain fixed. Prior newimp safety work (persist, locks, event IDs, secret gate, conn caps, clippy CI) is still in place.

---

## Verify

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo test e2e_restart_preserves_scrollback -- --exact
```
