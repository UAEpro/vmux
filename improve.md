# vmux — Bug & Improvement Plan

Full review of all 10 source files (~26k lines) across `daemon.rs`, `ui.rs`, `main.rs`,
`model.rs`, `protocol.rs`, `relay.rs`, `cli.rs`, `config.rs`, `paths.rs`, `input.rs`.

Every finding below was verified by reading the code and its callers; the ones marked
**(verified empirically)** were additionally reproduced against the built binary.
Line numbers are from commit `5fd5707`.

**Legend:** 🔴 critical · 🟠 high · 🟡 medium · ⚪ low

---

## Summary

| Area | 🔴/🟠 | 🟡 | ⚪ | Theme |
|------|:-----:|:--:|:--:|-------|
| Relay auth | 2 | 2 | 3 | Pairing gates don't gate |
| Multi-tab state | 1 | 3 | 2 | Code uses active-tab view where it means all tabs |
| Filesystem perms | 2 | 2 | 1 | Dirs/files created world-readable |
| UI event loop | 2 | 6 | 3 | Blocking work in draw path; clicks fall through overlays |
| Data loss | 2 | 1 | — | rc file / AGENTS.md / scrollback destroyed |
| Performance | — | 4 | 3 | Full-session snapshot + clone on every read & poll |

The single most common root cause is **the "active tab" vs "all tabs" confusion** in
`model.rs`: a workspace has `tabs[]` plus a live `panes[]` view of the active tab, and at
least five call sites operate on the live view where they must operate on every tab. Fixing
that one invariant kills a high and three medium bugs at once.

---

## 🔴 P0 — Fix before anyone else runs this

### 1. `bootstrap_secret` is parsed but never enforced — and grants access on its own
`relay.rs:869-886`, `relay.rs:979-990`

The secret comparison is written and then thrown away:

```rust
if provided != *secret {
    let _ = provided;   // ← mismatch is ignored
}
```

Worse, `resolve_peer_identity` grants an identity to **any** peer in `100.64.0.0/10`
merely *because a secret is configured*, without ever seeing the header:

```rust
if let Some(secret) = &state.config.bootstrap_secret {
    if is_tailscale_cgnat(peer_ip) && !secret.is_empty() { /* accept */ }
}
```

**Failure:** an operator sets `bootstrap_secret` believing it restricts pairing. Every node
on the tailnet has a CGNAT source address, so every tailnet peer pairs without knowing the
secret. Setting the secret *widens* access instead of narrowing it.

**Fix:** compare `provided` against the secret in constant time, thread the result into
`resolve_peer_identity`, and only take the bootstrap path when the header actually matched.
Return `Forbidden` otherwise.

### 2. `allow_login` is bypassed by the localhost and CGNAT paths
`relay.rs:936-946`, `relay.rs:965-975`, checked at `relay.rs:894-909`

Both fallback paths **synthesize** the login name from the allowlist itself:

```rust
login_name: state.config.allow_login.first().cloned().unwrap_or_else(|| "tailnet".into()),
```

so the later `allow_login.contains(&identity.login_name)` check trivially matches.

**Failure:** operator sets `allow_login = ["alice@corp"]` + `allow_tailnet_cgnat = true` to
restrict the relay to one user. Every CGNAT peer is registered *as* `alice@corp` and admitted.

**Fix:** only `allow_login`-check identities that came from a real `tailscale whois`. Treat
localhost/CGNAT as a distinct trust decision, and refuse them outright when `allow_login`
is non-empty.

### 3. Closing a workspace orphans every pane in its background tabs
`daemon.rs:1498` vs `model.rs:101-103`

```rust
for pane in &closed.panes {          // ← active tab's live view only
    self.remove_pane_runtimes(pane);
}
```
while `Session::close_workspace` removes `removed.all_pane_ids()` (every tab) from
`session.panes`.

**Failure:** `vmux tab new --command "npm run dev"` → switch back to tab-1 → `workspace close`.
The dev server's child process, PTY fds, reader thread and `PaneRuntime` live on forever, and
because the pane id is gone from `session.panes`, `kill-pane` and `prune` both answer
"unknown pane". **The orphan is unkillable until the daemon restarts.**

**Fix:** iterate `closed.all_pane_ids()`. Audit every other tab-bearing removal path for the
same mistake.

### 4. `/tmp/vmux-$UID` fallback allows daemon-socket impersonation
`paths.rs:7-14` (verified empirically)

```rust
.unwrap_or_else(|| PathBuf::from(format!("/tmp/vmux-{}", getuid())))
.join("vmux");
fs::create_dir_all(&dir)?;   // no mode, no ownership check, no symlink check
```
Under the default `022` umask this yields `drwxr-xr-x`. A pre-existing `/tmp/vmux-1000`
(or a symlink) is silently accepted.

**Failure:** on a multi-user host without `XDG_RUNTIME_DIR` (cron, containers, some SSH
setups) a local attacker squats `/tmp/vmux-<victim-uid>` first. Owning the socket's parent
directory lets them unlink the victim's 0600 socket and bind their own — capturing every
keystroke `vmux attach` sends and forging `read-screen` replies to agents.

**Fix:** `DirBuilder::mode(0o700)`, then `lstat`-verify the directory is a non-symlink owned
by the current uid with mode `0700`, and bail otherwise (this is what tmux does).
Also: `daemon.rs:439-445` binds the socket *before* `chmod 0600` and swallows the chmod
failure with `.ok()` — set the umask before binding instead.

---

## 🟠 P1 — Data loss and silent destruction

### 5. `vmux hooks setup` can truncate the user's `~/.bashrc`
`main.rs:855`

```rust
let existing = fs::read_to_string(path).unwrap_or_default();
...
fs::write(path, updated)?;
```
`read_to_string` returns `InvalidData` on non-UTF-8 bytes — a single latin-1 comment in a
bashrc. That error becomes `""`, and the whole file is then overwritten with two vmux lines.

**Fix:** only swallow `ErrorKind::NotFound`; propagate everything else. Read as bytes if
non-UTF-8 content must be tolerated.

### 6. `vmux agent team` silently overwrites an existing `AGENTS.md`
`main.rs:1821` (opt-out only, via `--no-agents-md`, default off)

Running `vmux agent team --agents codex,claude` inside a repo that maintains its own
`AGENTS.md` destroys it with no prompt or backup.

**Fix:** skip when the file exists (or back up / append), or make writing it opt-in.

### 7. Persisted scrollback is destroyed on the first output after restart
`daemon.rs:1041-1043`, `daemon.rs:2894`, `daemon.rs:3092`

Restored scrollback is copied onto `session.panes` but never seeded into the new
`PaneRuntime`. `Snapshot` always reads `runtime.joined_output()`, and the first
`append_output` does `*pane = runtime_pane.clone()`, wiping it.

**Failure:** the daemon restarts, the shell prints its prompt within milliseconds, and the
scrollback the README promises "survives daemon restarts" is gone before you can attach.

**Fix:** seed `PaneRuntime.output` / `output_bytes` and the vt100 parser from the persisted
scrollback in `restore_saved_panes`.

### 8. `vmux daemon --foreground` steals a live daemon's socket
`daemon.rs:166-172`, `daemon.rs:436-440`

`serve_foreground` has no `is_running` guard, and `serve()` unconditionally unlinks the
existing socket before binding.

**Failure:** two daemons for one session, alternately clobbering the same state file; the
first daemon's panes become unreachable.

**Fix:** check `is_running(session)` (or take the pid-file lock) and fail instead of stealing.

---

## 🟠 P1 — Broken by default

### 9. The letter `f` is unusable in every key spec
`input.rs:68`

```rust
_ if lower.starts_with('f') => { lower[1..].parse::<u8>()... }
```
For `"f"`, `lower[1..]` is `""`, the parse fails, and the key is rejected as `unknown key f`.

**Failure:** `vmux send-key C-f` and `vmux config set ui.prefix_key Ctrl-f` both error out.
(Verified by executing the exact logic: `f`, `F`, `C-f`, `Ctrl-f` all fail.)

**Fix:** require `lower.len() > 1` and an all-digit remainder before treating it as a
function key; otherwise fall through to the single-char branch.

### 10. F1–F4 are sent with an encoding no application recognizes
`input.rs:165-172`

F1–F4 go out as CSI `\x1b[P`…`\x1b[S`. Terminfo (`kf1=\EOP`) and every xterm-family app
expect SS3 `\x1bOP`…`\x1bOS`. `\x1b[P` is in fact `DCH` (delete character).

**Fix:** emit `\x1bO{P,Q,R,S}` when unmodified; keep `\x1b[1;{mod}{P..S}` for modified.

### 11. `--session` is rejected after the subcommand
`cli.rs:10` (verified empirically)

`session` is declared on the parent `Cli` without `global = true`, so `vmux sessions --session x`
fails with *"unexpected argument '--session'"*. The natural `vmux send --session dev hello`
— what most users and agents try first — is a parse error.

**Fix:** add `global = true`.

### 12. Bracketed paste is never enabled, so `Event::Paste` is dead code
`ui.rs:62-71` (guard), `ui.rs:1150` (handler)

Crossterm only emits `Event::Paste` after `EnableBracketedPaste`, which is never executed —
confirmed: the string appears nowhere in the file.

**Failure:** pasting a multiline snippet into a shell pane arrives as individual key events;
every newline becomes Enter, so **each pasted line executes immediately**. Paste also costs
one Input RPC per character.

**Fix:** `EnableBracketedPaste` in `TerminalGuard::new`, `DisableBracketedPaste` in `Drop`.

### 13. Settings panel spawns subprocesses and probes the network on every redraw
`ui.rs:7259` → `relay::runtime_status_line` → `resolve_listen` (spawns `tailscale ip -4`) +
`is_healthy` (`TcpStream::connect`, no connect timeout)

**Failure:** with Settings open, the 1s blink tick and every mouse-move redraw the panel —
several process spawns and TCP connects per second on the single-threaded event loop. If the
resolved address is unreachable the UI hangs for the OS connect timeout. `ui.rs:7183`
additionally re-reads every agent-hook config file per frame.

**Fix:** compute relay/hook status once on entering Settings (or on a slow background
refresh) and cache it. Never spawn processes or touch the network inside a draw path.

### 14. A failed relay toggle kills the whole attach client
`ui.rs:1651-1684`

`relay::apply_enabled` / `ensure_started` / `stop_managed` run synchronously (up to 20×100ms
health polls plus a 150ms kill sleep) and their `Result` is `?`-propagated out of
`handle_event`, which `run()`/`attach()` treat as fatal.

**Failure:** toggling "mobile relay" freezes the TUI for ~2s; if spawn fails (unwritable log
dir, `current_exe` error) the **entire client exits** instead of showing an error.

**Fix:** run start/stop on a background thread; route failures to `set_action_error`.

---

## 🟡 P2 — The active-tab/all-tabs invariant

These share one root cause. Fix `model.rs` once and they all close.

| # | Location | Bug |
|---|----------|-----|
| 15 | `model.rs:196` + `561` | `move_pane` finds the source via `contains_pane` (all tabs) but detaches with `remove_pane_from_active` (active tab only) → a pane in a background tab ends up referenced by **both** workspaces. Closing either then deletes the shared `Pane`, leaving the other with a dangling id. |
| 16 | `model.rs:280` | `prune_exited_panes` scans only the live view → `vmux prune --all` never reclaims dead panes in background tabs; their 16KB scrollback persists to the state file forever. |
| 17 | `daemon.rs:993-1008` | `restore_saved_panes` reads only the live view → panes on inactive tabs stay `PaneStatus::Restored` forever after a daemon restart and never relaunch. |
| 18 | `model.rs:228` | `swap_panes`' comment promises a multi-tab fallback that was never implemented; swapping two panes in a background tab errors out. |

**Fix:** give `Workspace` a single `for_each_tab_mut` / `remove_pane_anywhere` helper and
route all four through it. Then re-sync the active tab's live view.

---

## 🟡 P2 — Other correctness bugs

### 19. Clicks and wheel events fall through modal overlays
`ui.rs:2302`, `ui.rs:1119-1145`

`handle_click` never checks `mode`. With the command palette open, clicking a palette row
near the top-right hits the hidden pane's `×` control and pops a kill-pane confirmation.
The wheel scrolls invisible panes.

**Fix:** swallow main-area clicks/wheel when `mode != UiMode::Panes` (after the control-bar check).

### 20. Detaching leaves the terminal with mouse reporting on
`ui.rs:81`, `ui.rs:1716`

`TerminalGuard::drop` only disables mouse capture `if self.mouse` — the *initial* config
value. The Settings toggle enables capture at runtime without updating the guard.

**Failure:** start with `ui.mouse=false`, enable mouse in Settings, detach → the host shell
receives escape-sequence garbage on every mouse move.

**Fix:** issue `DisableMouseCapture` unconditionally in `Drop` (harmless if never enabled).

### 21. The ❌ error marker is erased by the very next output chunk
`model.rs:1128`

In `merge_agent_status`'s unpinned branch, Busy/Done/Attention are sticky but `Error` falls
through `(_, next) => (next, false)` and is demoted by the next Idle/Unknown inference.

**Failure:** PTY output containing "traceback" sets `AgentStatus::Error`; the next chunk (the
shell prompt redraw) resets it to Idle. The ❌ marker flashes for milliseconds — effectively
nonfunctional for panes without hooks.

**Fix:** add `(Error, Idle | Unknown) => (Error, false)` to the sticky set.

### 22. Selection copies the live screen while you're scrolled back
`ui.rs:2665`

`selected_text_from_pane` reads `pane.output` even when `scroll_offset > 0` (where the view
renders *scrollback* and the highlight is deliberately suppressed).

**Failure:** scroll up, drag over history, release → the clipboard silently gets whatever is
on the live screen at those coordinates.

**Fix:** skip or remap the copy when `scroll_offset != 0`.

### 23. Selection is off by one column per wide glyph
`ui.rs:5785`, `ui.rs:5719`

Display columns are converted to char indices with `chars().skip(from)`, ignoring
CJK/emoji widths (`invert_cell_at_column` already does this correctly).

**Failure:** in a pane showing `你好 world`, selecting "world" copies a shifted range.

**Fix:** map columns → chars via `UnicodeWidthChar`.

### 24. Clicking a mouse-mode pane doesn't focus it
`ui.rs:2402`

`handle_click` forwards the click to the app and returns before `primary_mouse_action`'s
`FocusPane` branch runs.

**Failure:** two panes, the inactive one running vim → clicking it delivers the click but
`active_pane` doesn't move, so subsequent keystrokes go to the wrong pane.

**Fix:** send `Request::FocusPane` before/alongside forwarding.

### 25. Daemon errors are invisible outside the Actions panel
`ui.rs:1190`

Every `ok:false` routed through `rpc()` lands in `action_error`, which `draw` renders only
inside the Actions panel (`ui.rs:3009`).

**Failure:** a failed kill/resize/rename produces **no feedback at all** in Panes mode.

**Fix:** surface `action_error` as a transient footer/control-bar status line in all modes.

### 26. Context-menu clicks are matched by row alone
`ui.rs:2621` — `index = row - 1`, no column or panel-rect check. Clicking a sidebar
workspace at row 2 with the menu open executes "paste" into the context pane.

### 27. Prefix-suffix dispatch ignores modifiers
`ui.rs:958` — `Ctrl-b` then `Ctrl-c` (a reflexive cancel) creates a new workspace;
`Ctrl-b Ctrl-q` detaches. Require empty modifiers when resolving suffix bindings.

### 28. `close_tab` yanks the view when closing a *background* tab
`model.rs:612` — `switch_tab` is called unconditionally with a neighbor of the *closed*
tab's index. With `[tab-1(active), tab-2, tab-3]`, `vmux tab close tab-3` jumps you to tab-2.

### 29. `vmux wait` with no timeout spins forever
`daemon.rs:2174-2206` — 50ms poll loop, no deadline and no client-liveness check, cloning
every target `Pane` (~50KB of strings) under the session lock each iteration. Ctrl-C'ing the
CLI leaves the daemon thread spinning for the daemon's lifetime.

### 30. OSC notifications are lost on chunk boundaries
`daemon.rs:2848-2853` — the tail is only retained on a literal `"\x1b]"` match, so a 4096-byte
read ending exactly after `\x1b` drops the following `]9;needs input\x07`.

### 31. Hello-handshake deadline can be held open indefinitely
`relay.rs` WS loop — the deadline is only checked on Text frames and read timeouts, so a
client that keeps sending Pings never triggers it. Thread + event-poller live on unauthenticated.
**Fix:** check the deadline unconditionally at the top of the loop.

### 32. `browser`/`url` requests time out client-side before the daemon finishes
`daemon.rs:3670-3685` (curl `--max-time 30`) vs `protocol.rs:365-386` (client `DEFAULT_TIMEOUT` 10s).
A 10–30s fetch always fails with "daemon may be unresponsive" though the daemon completed it.

### 33. Empty `XDG_RUNTIME_DIR` litters `vmux/` into the cwd
`paths.rs:8-11` (verified empirically) — a set-but-empty value is used verbatim, so the
runtime dir becomes the *relative* path `vmux/`. Client and daemon then resolve sockets
against their own cwd and never find each other.
**Fix:** treat empty/non-absolute as unset, per the XDG basedir spec.

### 34. `stop_session` reports success without confirming the daemon died
`main.rs:2092-2107` — waits 1s, then deletes the pid file and socket regardless. A slow
daemon is left running but unreachable; the next command spawns a second daemon that fights
the orphan over the state file.

---

## 🟡 P2 — Performance

### 35. The whole session (including all scrollback) is serialized on every 150ms poll
`model.rs:846`, `ui.rs:905`, `daemon.rs:3011`

`Pane` embeds `output`, `output_formatted`, `scrollback` (16KB cap) and
`scrollback_formatted`, so `Request::Snapshot` clones the entire `Session` and ships every
pane's buffers. The client then deep-compares the `serde_json::Value`, clones it, and
re-deserializes the whole `Session` — ~7×/sec **plus once per keystroke**, even when idle.

**Fix:** add a daemon-side generation counter or content hash so unchanged snapshots return
"unchanged"; fetch scrollback lazily, only for the pane being scrolled.

### 36. `append_output` amplifies memory traffic ~30–40× on every PTY read
`daemon.rs:2854-2898`

Per ≤4KB read: renders the full screen twice (`contents()` + `screen_contents_formatted`),
joins and trims 16KB of scrollback, clones the entire `Pane` (~40–50KB of strings) **twice**
(`runtime_pane` is not even used after the second), clones `tabs`/`metadata`, and re-clones
all output strings in `sync_active_pane_tab` — all while holding the session lock.

**Failure:** `cat` of a 100MB file costs several GB of memcpy and starves snapshot/save.

**Fix:** mark the runtime dirty; materialize screen/scrollback strings lazily in `snapshot()`
(which already caches).

### 37. Every OSC notification triggers a synchronous full-state disk write
`daemon.rs:3241-3265`, `daemon.rs:2934-2936`

`save()` clones the whole session, pretty-serializes it, and writes synchronously — and it
runs **on the PTY reader thread** from `append_output`, on every focus change, zoom, and notify.

**Fix:** debounce behind a dirty flag + background writer. Never save inline on the reader thread.

### 38. HTML parsing is O(tags × page size)
`daemon.rs:3713-3736`, `3927-3947`, `4062-4086` — every helper calls
`rest.to_ascii_lowercase()` *inside* its scan loop, and `fetch_url_body` buffers an unbounded
curl body. A few-MB page with thousands of links burns minutes of CPU.
**Fix:** lowercase once up front, scan by byte offset, add `--max-filesize`.

### 39. `PaneSizes` RPC is resent on every keypress
`ui.rs:1201`, `ui.rs:950` — three socket round-trips per key (Input + PaneSizes + Snapshot)
even when sizes are unchanged.

### 40. The relay polls the daemon twice per frame per surface
`relay.rs:1368,1390` — `read_surface_screen` issues a second full `Request::List` just to get
the cursor position, each serializing the entire session snapshot, at up to 15fps.
**Fix:** return the cursor from `ReadScreen`.

### 41. `vmux logs` reads the entire (never-rotated) log into memory
`main.rs:2031-2033` — `read_to_string` on a multi-GB log just to print the last N lines.
**Fix:** seek from the end in chunks. Add log rotation.

---

## ⚪ P3 — Hardening, hygiene, dead code

**Permissions** (all default to umask-derived, i.e. `0644`/`0755`):
- `paths.rs:53-58` — state dir + session JSON, which embeds **full pane scrollback**, is
  world-readable when the daemon runs `--foreground` (only the daemonized path sets `umask 077`).
  Verified: `~/.local/state/vmux/*.json` came out `0644`. Create dir `0700`, files `0600`.
- `paths.rs:66-71`, `relay.rs:124`, `relay.rs:170-177` — `~/.config/vmux/relay.json` holds the
  plaintext `bootstrap_secret`, and `relay-devices.json` holds token hashes + APNs tokens.
  Both `0644`. Create dir `0700`, write files `0600`.

**Relay:**
- `relay.rs:216-223` — `token_hash == hash` is not constant-time. *Low, not high:* this
  compares SHA-256 **hashes**, so a timing leak reveals the stored hash, not the token, and
  inverting it is infeasible. Still worth a constant-time compare on the raw digest bytes.
- `relay.rs:997` — `try_tailscale_whois` maps "binary missing" and "whois errored" both to
  `None`, silently downgrading to the weaker CGNAT path. Distinguish the two.
- `relay.rs:1225,1301` — `let last_input = Instant::now();` is never updated, so `current_fps`
  drops to `idle_fps` after 1.5s and never recovers. The comment admits the boost was never wired up.
- `relay.rs:1085` — hand-rolled SHA-1 for the WS accept key, though tungstenite already
  provides `derive_accept_key`. Correct today; unnecessary attack surface.
- `file.upload` has no per-frame decoded-size cap and no cumulative quota — a paired device can
  fill the disk under `~/vmux-remote/`. `register_device` has no rate limit.
- The event-poller thread is spawned per connection at upgrade time (before hello), polling the
  daemon at 2Hz for the connection's lifetime.

**Robustness:**
- `daemon.rs:2575-2587,2623-2643` — `search_pane`/`copy_pane` hold the `panes` guard while
  locking `session`, violating the lock-order contract documented at `daemon.rs:337`. No
  deadlock today; any future handler taking them in the documented order will hang the daemon.
- `daemon.rs:1066-1099` — `new_pane` spawns the child, then errors via `?` if the workspace
  vanished meanwhile, leaving a live process attached to nothing.
- `daemon.rs:4513` — `find_vmux_config` walks ancestors all the way to `/`, so a workspace under
  `/tmp/project` picks up an attacker-planted `/tmp/vmux.json` whose `commands` are executed
  verbatim by `run_custom_action`. Stop the walk at `$HOME` (or at non-user-owned dirs).
- `main.rs:1908-1923` — `shell_quote` blocks shell injection but not **ssh option** injection:
  `vmux remote ssh '-oProxyCommand=touch /tmp/pwned'` executes a command. Insert `--` before the host.
- `daemon.rs:530` — no read/write timeout and no connection cap on accepted socket streams
  (relay.rs already sets both). A client that connects and never writes parks a thread and fd forever.
- `protocol.rs:343` — `Response::ok` turns a `to_value` failure into `{"ok":true}` with null data.
- `config.rs:219-226` — `config.json` is written non-atomically; a crash mid-write silently
  resets all settings to defaults on next load. Use tmp+rename like `daemon.rs:3256-3260`.
- `config.rs:85-101` — `prefix_key` is never validated. `"Hyper-Q"` loads silently, the UI
  advertises "Hyper-Q", and the real prefix stays Ctrl-b.
- `main.rs:1730-1747` — a failed pane request leaves the just-created workspace orphaned.
- `main.rs:917` — `vmux config init` swallows hook-install failures and reports `"created": true`.
- `main.rs:2686-2702` — `follow_events` dedupes on serialized JSON, but `EventRecord.time` has
  1-second resolution, so two identical events in the same second collapse to one. The `seen`
  set also grows unboundedly. Use a monotonic sequence id.
- `input.rs:26-31` — `-` and `+` are unrepresentable as keys, and `C--` silently parses as a
  bare `C`. Treat a trailing empty segment as the literal separator (as tmux does).
- `model.rs:1161` — `is_coding_agent_command` substring-matches "continue"/"agent"/"cursor"
  anywhere, so `git rebase --continue` and `ssh-agent bash` are classified as coding agents and
  pin a 🔄 spinner that never clears. Match the basename of the first token.
- `agent_hooks.rs:411-443` — `install_claude`/`install_codex` read-modify-write the agent's
  `settings.json` with no lock, racing the agent's own writes.

**Dead code / duplication:**
- `ui.rs:5076` — the entire per-pane tab-strip subsystem (`TabCell`, `pane_tab_cells`,
  `draw_pane_tab_strip`, `pane_tab_at`, `relative_pane_tab`, `agent_panel_lines`, …) is
  `#[cfg(test)]`-only, maintained solely by its own tests. It also hides a latent panic:
  after `start -= 1`, line 5077 reads `widths[start - 1]` → underflow when `start == 0`.
  **Delete it.**
- `ui.rs:6015` — six functions (`pane_at`, `pane_area_at`, `pane_area_by_id`, `pane_title_at`,
  `pane_control_at_layout`, `split_axis_at`) each re-implement the same ~40-line ratio-clamp /
  split-rect recursion. Extract one `split_child_areas(axis, ratio, area)` helper; today a
  change to the 15–85 clamp must be made in six places or hit-test and render silently diverge.
- `model.rs:629` — `Workspace::ensure_layout`/`first_pane` duplicate
  `WorkspaceTab::ensure_layout`/`first_pane` almost verbatim, and have **already drifted**.
  `rename_tab` (`model.rs:623-625`) carries an empty dead `if` block.
- `cli.rs:69-86` — `OpenUrl`/`UrlSnapshot`/`UrlLinks` duplicate `browser open/snapshot/links`,
  and `main.rs:80-113` re-implements the mapping already centralized in `browser_request`.
- `config.rs:9` — the type is named `LmuxConfig` (24 references) in a project that is
  otherwise consistently `vmux`. Rename to `VmuxConfig`.
- `ui.rs:4272` — `rename_target_at` hardcodes `"emoji"` markers and `true` for the close button
  while the drawn bar uses the configured values, so with `ui.status_markers=ascii` a
  double-click renames the wrong tab.
- `ui.rs:7888` — `+ 0` no-op; `ui.rs:7354` — items declared after the test module.

---

## Corrections to note

Two plausible-looking findings did **not** survive verification:

1. **"The relay binds `0.0.0.0` by default."** False. `DEFAULT_LISTEN` is `127.0.0.1:4399`
   (`relay.rs:36`) and `serve()` actively refuses unsafe listen addresses, remapping them
   (`relay.rs:279-286`). It is the **README** (line 451) that is stale — it documents
   `"listen": "0.0.0.0:4399"` as the generated default. **Fix the README, not the code.**
2. **"Token comparison is a high-severity timing attack."** Downgraded to low — see P3. The
   compared values are SHA-256 hashes, not the tokens themselves.

Also confirmed **not** broken, despite being likely places for it:
`$HOME`-missing does not panic (`paths.rs` uses `unwrap_or_else` chains); a malformed
`config.json` correctly falls back to defaults with a warning (`config.rs:358-370`); session
names are properly validated against path traversal including via `VMUX_SESSION`
(`paths.rs:22-36`, verified); and the daemon socket **is** chmod'd `0600` after bind
(`daemon.rs:441-445`) — the gap is only the pre-chmod window and the parent directory.

---

## Suggested order of work

1. **P0 #1–#4** — relay auth (`#1`, `#2`) is a genuine remote-access hole; `#3` and `#4` are
   local. Do these first; they're small, self-contained diffs.
2. **#5, #6** — one-line guards that stop destroying user files.
3. **#15–#18** — the `model.rs` tab-view invariant. One helper closes four bugs, and `#7`/`#17`
   overlap here.
4. **#9, #11, #12** — trivially reproducible, user-visible breakage. Cheap wins.
5. **#13, #14, #19, #20** — the UI event loop; `#13` and `#14` are the ones users will feel.
6. **#35, #36, #37** — the snapshot/append_output/save hot path. Do `#35` first: a generation
   counter also removes most of the client-side cost in one change.
7. Everything else as capacity allows. Start by deleting the dead tab-strip subsystem — it's
   ~400 lines of `ui.rs` that only its own tests exercise.

There are currently **no tests** covering the relay auth paths, the multi-tab pane lifecycle,
or terminal-guard restoration — the three areas with the most severe findings. Add those
alongside the fixes.
