# Plan: phone-fit pane sizing ("smallest client wins", auto-restore)

Deferred feature, agreed 2026-07-13. When the vmux Remote app opens a pane,
the pane's PTY temporarily shrinks to fit the phone; when the phone stops
viewing it, the pane returns to its desktop layout size. tmux semantics
(smallest attached client wins), but scoped per-pane and self-healing.

All of this is daemon/relay work in **this repo** — the app only reports its
size. Until it exists, the app soft-wraps long rows client-side, which is
already shipped and stays as the fallback for panes the user chooses not to
resize.

## Why not just resize on subscribe

A PTY has one size shared by every viewer. A naive resize-on-open would mangle
the desktop layout whenever the phone glances at a pane, and a phone that
loses signal (or crashes) would leave the pane phone-sized forever. So the
design needs (a) an explicit override concept separate from layout-computed
size, and (b) a lease so restore cannot be missed.

## Design

### Daemon

- New per-pane runtime state: `view_override: Option<ViewOverride { cols, rows,
  expires_at }>`. Runtime-only — never persisted to the session JSON, so a
  daemon restart naturally restores desktop sizes.
- Effective PTY size for a pane = `min(layout size, view_override)` per axis
  ("smallest client wins"). Applied wherever the layout engine currently calls
  `resize` on the PTY; bump `Server.generation` on every change (repaints).
- Protocol (additive, `#[serde(default)]` per CLAUDE.md):
  - `SetPaneViewSize { pane, cols, rows, lease_ms }` — set/refresh the
    override. Lease is required; a dead client must not pin a size.
  - `ClearPaneViewSize { pane }` — explicit restore.
- A daemon housekeeping tick (the existing agent-status decay thread can carry
  this) clears expired overrides and bumps the generation.
- Desktop attach UI: while an override is active the pane's cell area is
  smaller than its layout box — render the unused margin dimmed with a note
  ("pane sized by phone"), tmux-style. This is the only UI-visible part.

### Relay

- `surface.subscribe` gains optional `view_cols` / `view_rows` params. When
  present, the poller calls `SetPaneViewSize` on subscribe and re-leases it on
  every poll cycle (poll interval << lease; lease ~10s means a vanished phone
  restores within seconds).
- `surface.unsubscribe` and WS teardown call `ClearPaneViewSize` (belt) on top
  of the lease expiry (braces).

### App

- Terminal screen measures its own cols/rows from the font metrics it already
  has, passes them in `subscribe`.
- Settings toggle: "Resize pane to fit phone" (default **off** — resizing a
  shared pane surprises whoever is at the desk; wrap is the safe default).
  Per-pane override from the terminal header menu later.

## Test plan

- Daemon unit tests: override + lease expiry restores layout size; min() of
  layout/override; persistence excludes overrides.
- vmux-remote e2e: subscribe with view size → `ReadScreen` cols shrink;
  unsubscribe → cols restore; kill the websocket without unsubscribing →
  cols restore after lease expiry.

## Open questions

- Multiple phones on one pane: min() across all live overrides, or last
  writer wins? (min() is truest to tmux.)
- Should zoomed panes (`vmux zoom`) refuse overrides? Probably yes.
