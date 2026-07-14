# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
cargo build
cargo test                                    # all tests
cargo test <substring>                        # e.g. cargo test snapshot
cargo test e2e_restart_preserves_scrollback_across_save_reload -- --exact --nocapture
cargo fmt
cargo clippy --all-targets -- -D warnings     # CI gate: warnings are errors
cargo build --release
cargo run -- smoke                            # end-to-end daemon check
```

Run `cargo fmt`, `cargo clippy`, `cargo test`, and `cargo run -- smoke` before opening a PR. CI (`.github/workflows/ci.yml`) runs fmt, clippy, tests, the restart-persistence e2e, and a release build on Linux and macOS.

**Always develop against a scratch session.** `cargo run -- attach` starts or joins the *same* daemon as the user's installed release build, so a debug build with a bug can take down real work:

```sh
cargo run -- --session dev attach
```

The dev profile builds dependencies at `opt-level = 3` and this crate at `1` — vt100/serde_json/ratatui are the hot path and are 10-50x slower unoptimized.

## Naming: vmux vs lmux

The project was renamed. The checkout directory is `lmux`, the crate is `vmux-tui` (the `vmux` name was taken on crates.io), and the installed binary is `vmux`. Internally, `LmuxConfig` and the `LMUX_*` environment variables survive as **deliberate backward-compat, not leftovers**: panes get both `VMUX_PANE_ID` and `LMUX_PANE_ID`, shell hooks expand `${VMUX_PANE_ID:-${LMUX_PANE_ID:-}}`, and `agent_hooks.rs` detects hooks that only mention `LMUX_PANE_ID` to flag them stale. Do not "clean up" the legacy names — tests assert on them.

## Architecture

CONTRIBUTING.md has the daemon/client diagram and the file map. The parts that matter when you change code:

**The daemon owns all authoritative state; the attach UI is a dumb client.** `ui/mod.rs` renders a snapshot and sends RPCs — it holds no state of its own. Every CLI command speaks the same JSON-over-Unix-socket protocol the UI does, which is why agents can drive the workspace. If you find yourself keeping state in the UI, you are probably solving it in the wrong process.

**Adding a feature is a three-file move**: a variant in `cli.rs`, a `Request`/`Response` pair in `protocol.rs`, a handler arm in `Server::dispatch` (`daemon/mod.rs`). If the UI must show it, the snapshot type in `protocol.rs` grows a field and `ui/mod.rs` renders it.

**The generation counter drives all repaints.** `Server.generation` is an `AtomicU64` bumped on every session/runtime change. Clients send `Snapshot { since }` and the daemon short-circuits to `{unchanged: true, generation}` when nothing moved. A mutation that forgets to bump the generation is invisible to the UI — the screen simply never updates.

**Snapshots come in fidelity levels**, and a new pane field has to be placed deliberately in each: `full: true` materializes heavy per-pane scrollback strings (used for persistence), `full: false` omits them (layout/status polls), and `lean` is the attach-UI poll — live screen contents but no event history and no scrollback except for panes the client is currently scrolled back in (`scrollback_panes`).

**The snapshot hot path must never shell out.** `git` and `ss` are invoked only by the `refresh_workspace_meta_loop` background thread — local tools only; the loop once polled `gh pr view` and drained the user's GitHub API quota, so metered/network commands are banned here, which caches into `workspace_meta`; snapshots read the cache. `Server::serve` also spawns threads for the debounced save loop (400ms, gated on a `save_dirty` flag), the daily update check, and agent-status decay.

**`Server` is a struct of independent `Mutex`es** (`session`, `panes`, `workspace_meta`, `next_pane`, …), not one big lock. Two of them held in inconsistent order across threads deadlocks the daemon — the field comments record the intended ordering, so read them before taking a second lock. In particular: **never hold `session` and `panes` at once.** Take one, collect what you need, release it, then take the other.

Locks are taken via `MutexExt::lock_or_recover` (`src/sync.rs`), which tolerates poisoning. Both the daemon and the relay are thread-per-connection, which is exactly the shape that turns one poisoned mutex into a permanently bricked process — so **never** use `lock().unwrap()`/`.expect()` in either.

**Persistence** is the session JSON under the XDG state dir (`~/.local/state/vmux/<session>.json`), scrollback included; a corrupt file is moved aside rather than deleted. `e2e_restart_preserves_scrollback_across_save_reload` is the guard test for the whole save → drop → reload path and CI runs it explicitly. Retained output per pane is `ui.scrollback_bytes` (default 200 KB), read once at daemon start.

The daemon writes non-session files into that same state dir (`update-check.json`, `relay-devices.json`), and `list_sessions` enumerates `*.json` there — so any new one **must** be added to `RESERVED_STATE_STEMS` in `paths.rs`, or it shows up as a phantom session in `vmux sessions` and a session of that name can clobber it.

**Daemon tests write to the real XDG dirs.** Use the `TestSession` guard in `daemon/mod.rs`'s test module — it makes a collision-proof name and cleans up in `Drop`, so a failing assertion cannot leak state into the developer's live `vmux sessions`. Never hand-roll a session name from `unix_time()` alone; it has one-second granularity.

**The daemon detaches by re-exec'ing itself** with `VMUX_DAEMONIZE=1` (see `daemon::start_detached`), then ignores SIGHUP so it survives the terminal it was launched from. `paths.rs` refuses a runtime dir that is a symlink, is not owned by the current uid, or is group/other-accessible.

## Workflow

**Each new feature gets its own git worktree and branch.** Do not develop on `main` and do not stack unrelated features in one checkout. Worktrees live under `.worktrees/` inside the repo (gitignored) so they don't clutter `~/code`:

```sh
git worktree add .worktrees/<feature> -b <feature>
```

Work in that worktree, and pair it with a matching scratch daemon session (`cargo run -- --session <feature> attach`) so two in-flight features never share a daemon.

## Conventions

- **Tests live inline** in `#[cfg(test)] mod tests` at the bottom of each source file. There is no `tests/` directory. Daemon tests touch real XDG paths, so they build a unique session name and clean up their own state/lock files at the end — follow that pattern or tests will collide.
- **Config keys have four places to stay in sync**: the struct in `config.rs`, `set_value`, the `supported_*`/`*_choices` lists that feed the UI pickers and shell completion, and `docs/config.md`.
- The protocol is spoken by shipped binaries and the phone relay, so treat `protocol.rs` as a versioned surface: new fields get `#[serde(default)]`, and existing responses keep their legacy shape.
- **Touching `src/relay/` or `protocol.rs`? Run the phone app's contract suite too**: `cd ~/code/vmux-remote && npm run test:e2e`. It spawns a fresh daemon + relay from `target/debug/vmux` and drives everything the vmux Remote app depends on (pairing, tabs, screen streaming with `ansi: true` colour rows, send-key spellings, focus across tabs, attention promotion, reconnect). `cargo test` alone does not cover this wire contract — a green build here can still be a broken phone.
