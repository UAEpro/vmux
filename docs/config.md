# Configuration

Config is managed with `vmux config` and mirrored in the attach **Settings**
panel (**⚙ set**). Changes from either side apply to the running session.

```sh
vmux config show
vmux config init                      # write defaults, install agent hooks
vmux config set ui.prefix_key Ctrl-a
```

## Keys

| Key | Example | Notes |
|-----|---------|-------|
| `ui.prefix_key` | `Ctrl-b`, `Ctrl-a` | Prefix chord. Default `Ctrl-b`. |
| `ui.sidebar_collapsed` | `true` / `false` | Start with a compact sidebar |
| `ui.sidebar_width` | `28` | Width when expanded |
| `ui.scroll_step` | `8` | Lines per scroll step |
| `ui.scrollback_bytes` | `200000` | Output retained per pane, in bytes (~2500 lines). Clamped to 16 KB–5 MB. Takes effect on the next daemon start. |
| `ui.theme` | see below | Color theme |
| `ui.status_markers` | `emoji`, `ascii`, `off` | How agent status renders in the sidebar |
| `ui.cursor` | set in the Settings panel | Cursor style and blink |
| `agent_titles.enabled` | `true` / `false` | Name tabs after what the agent in them is doing |
| `agent_titles.llm_fallback` | `true` / `false` | Ask a model to name tabs agents don't title themselves |
| `agent_titles.llm_command` | `claude -p` | Headless command that reads a prompt and prints a title |
| `agent_titles.llm_delay_ms` | `20000` | How long to wait for the agent's own title first |

Relay keys (`relay.enabled`, `relay.bind`, `relay.allow_localhost`,
`relay.allow_paste`, `relay.allow_view_resize`) are covered in
[relay.md](relay.md).

## Automatic tab names

A tab holding **any** coding agent names itself after the work in it —
`fixing parser`, `auth middleware` — so a row of tabs reads as a list of what
is in flight. The same pipeline runs for Claude Code, Codex, Grok Build, Aider,
Cursor, Gemini, and anything else detected as a coding-agent process.

Sources, in order (first usable label wins for that update):

1. **Terminal title (OSC 0/2)** — free. Agents that retitle the terminal
   (Claude Code, Codex, …) are read directly; the title is condensed to one or
   two words and kept current as the agent moves between tasks.
2. **Hook prompt** — free. A `UserPromptSubmit` (or compatible) hook that pipes
   JSON into `vmux hooks event` carries the user prompt; vmux condenses it the
   same way. Install with `vmux hooks install` (Claude, Codex, Grok, shell).
3. **Busy status message** — free. Any agent or script can name its tab with
   `vmux set-status busy --message "fixing auth"` (boilerplate like
   `"agent working"` is ignored).
4. **LLM fallback** — one short call via `agent_titles.llm_command` after
   `agent_titles.llm_delay_ms`, only when the pane is actually on a task and
   no free source produced a title. Disable with
   `agent_titles.llm_fallback false`.

Shells never rename tabs (`user@host`, paths). Renaming a tab yourself pins it:
vmux will not rename it again.

    vmux config set agent_titles.enabled false     # off entirely

Changes take effect on the next daemon start (`vmux kill`, then attach).

## Themes

Fifteen built-in themes:

```text
midnight  daylight  contrast  nord  dracula  gruvbox  catppuccin
solarized-dark  solarized-light  tokyo-night  forest  rose-pine
ocean  ember  monokai
```

```sh
vmux config set ui.theme tokyo-night
```

## Responsive layout

Below 90 columns, the workspace sidebar auto-hides so panes get the full width.
It is on by default and can be turned off in Settings. When it is hidden, reach
workspaces through the **📱 menu** button on the control bar, or press
`Ctrl-b w` for the picker (`j`/`k` or arrows, `Enter` to switch, `Esc` to
close).

## Project actions

A `vmux.json` in a project root defines named commands, which show up under
`Ctrl-b A` and `vmux actions`:

```sh
vmux actions list
vmux actions run test
```

## Update checks

vmux checks for a new release once a day and prints a notice. Disable it by
setting `VMUX_NO_UPDATE_CHECK=1`.
