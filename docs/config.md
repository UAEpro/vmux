# Configuration

Config is managed with `vmux config` and mirrored in the attach **Settings**
panel (**⚙ set**). Changes from either side apply to the running session
unless noted.

```sh
vmux config show
vmux config init                      # write defaults, install agent hooks
vmux config set ui.prefix_key Ctrl-a
```

Path (XDG): typically `~/.config/vmux/config.json`.

### JSON Schema

Editors and CI can validate against the checked-in schema:

```json
{
  "$schema": "./config.schema.json"
}
```

Or open [config.schema.json](config.schema.json) directly (`$id`:
`https://vmux.sh/docs/config.schema.json`). The schema documents `ui.*`,
`relay.*`, `agent_titles.*`, and `ports.*`.

## Keys

### `ui.*`

| Key | Example | Notes |
|-----|---------|-------|
| `ui.prefix_key` | `Ctrl-b`, `Ctrl-a` | Prefix chord. Default `Ctrl-b`. |
| `ui.sidebar_collapsed` | `true` / `false` | Start with a compact sidebar |
| `ui.sidebar_width` | `28` | Expanded width (12–60). Max when `sidebar_fit` is on |
| `ui.sidebar_fit` | `true` / `false` | Fit width to workspace name text (default **on**) |
| `ui.sidebar_responsive` | `true` / `false` | Auto-hide sidebar on narrow terminals (default on) |
| `ui.scroll_step` | `8` | Lines per scroll step |
| `ui.scrollback_bytes` | `200000` | Output retained per pane (~2500 lines). Clamped 16 KB–5 MB. **Next daemon start** |
| `ui.layout` | `classic`, `compact`, `minimal`, `flat`, `zen` | Screen **structure** (chrome density, borders, titles) |
| `ui.colors` | see below | Color **palette** only |
| `ui.theme` | same as `ui.colors` | Legacy alias of `ui.colors` (still written/read for older tools) |
| `ui.workspace_second_line` | `path`, `branch`, … | Second line of each sidebar workspace row |
| `ui.status_markers` | `dots` (default), `emoji`, `ascii`, `off` | How agent status renders in the sidebar: colored dots (✖ error, ◉ needs input, ● busy, ○ done), emoji (❌🙋🔄✅), ASCII, or hidden |
| `ui.cursor_blink` | `true` / `false` | Soft-blink active caret while idle |
| `ui.cursor_blink_ms` | `1000` | Half-period of caret blink (200–5000 ms) |
| `ui.default_shell` | `zsh`, empty | Empty = `$SHELL` |
| `ui.default_cwd` | `launch`, `home` | New pane/workspace directory |
| `ui.resume_agents` | `true` / `false` | On daemon restart, relaunch agents resuming their conversation (`claude --resume <id>`, `codex resume <id>`) using the session id their hooks reported. Needs `vmux hooks install`. Applies on next daemon start |
| `ui.mouse` | `true` / `false` | Capture mouse in attach UI |
| `ui.tab_close_button` | `true` / `false` | Show × on tabs when more than one exists |
| `ui.bell_on_attention` | `true` / `false` | Terminal bell on attention / needs-input |

### `relay.*`

| Key | Example | Notes |
|-----|---------|-------|
| `relay.enabled` | `true` / `false` | Auto-start phone relay on attach (default **on**) |
| `relay.bind` | `auto`, `tailscale`, `local` | Never binds `0.0.0.0` |
| `relay.port` | `4399` | TCP port **1–65535** (default 4399). Also: `vmux relay serve --listen host:port` |
| `relay.allow_localhost` | `true` / `false` | Allow device registration from loopback |
| `relay.allow_tailnet_cgnat` | `true` / `false` | Accept CGNAT peers without whois |
| `relay.allow_paste` | `true` / `false` | Browser paste page (`/paste`) |
| `relay.allow_view_resize` | `true` / `false` | Phone-fit leased pane resize (default off) |

Full relay behaviour: [relay.md](relay.md).

```sh
vmux config set relay.port 4400
vmux config set relay.bind auto
vmux relay serve --listen 127.0.0.1:4400
```

### `agent_titles.*`

| Key | Example | Notes |
|-----|---------|-------|
| `agent_titles.enabled` | `true` / `false` | Name tabs after agent work |
| `agent_titles.llm_fallback` | `true` / `false` | LLM last-resort titles |
| `agent_titles.llm_command` | `claude -p` | Reads prompt on stdin, prints a short title |
| `agent_titles.llm_delay_ms` | `20000` | Wait for free sources before LLM |

### `ports.*`

Workspace listening-port detection and forward helpers. See [ports.md](ports.md).

| Key | Example | Notes |
|-----|---------|-------|
| `ports.enabled` | `true` / `false` | Detect ports via `/proc` on Linux (default **on**) |
| `ports.notify` | `toast`, `banner`, `off` | How new ports surface in the UI |
| `ports.auto_forward` | `true` / `false` | Auto Tailscale-forward new ports (default **off**) |
| `ports.forward_via` | `ask`, `tailscale`, `ssh` | Preference (CLI auto-forward uses tailscale) |
| `ports.poll_secs` | `2` | Scan interval (1–60) |
| `ports.ignore` | `5432,6379` | Never surface these ports (comma-separated) |
| `ports.ignore_processes` | `ssh,sshd` | Ignore by `/proc` comm (comma-separated) |
| `ports.ignore_ephemeral` | `true` / `false` | Hide kernel ephemeral range (default on) |
| `ports.ssh_host` | `user@host` | Default host for `vmux ports ssh-cmd` |

Takes effect on the **next daemon start** for poll interval and enable flag;
ignore lists apply on the next scan after restart.

```sh
vmux config set ports.enabled true
vmux config set ports.notify toast
vmux config set ports.auto_forward false
vmux config set ports.ignore 5432,6379
vmux config set ports.ignore_processes node_modules
vmux config set ports.ssh_host 'user@devbox'
```

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

```sh
vmux config set agent_titles.enabled false     # off entirely
```

Changes take effect on the next daemon start (`vmux kill` / `vmux stop`, then
attach).

## Layout vs colors

Structure and palette are independent settings (Settings → **layout** / **colors**).

### Layouts (`ui.layout`)

Each skin changes **sidebar**, **control bar**, **tabs**, and **pane frames** —
not just a border toggle.

| Name | Look |
|------|------|
| `classic` (default structure) | Filled active sidebar row, labeled toolbar + session footer, solid tab chips, full box panes |
| `compact` | Dense surface rail with `▎` accent, equal icon toolbar, chip tabs, full boxes |
| `minimal` | Ghost text sidebar, bare muted icons, underlined tabs, frame **only** the focused pane |
| `flat` | Soft surface rail + `●` pill selection, spaced pill buttons, underline tabs, left-edge active accent |
| `zen` | Content first: no frames, no titles, text-only sidebar, nearly invisible icon bar |

Aliases: `dense`→compact, `focus`→minimal, `product`→flat, `immersive`→zen.

```sh
vmux config set ui.layout flat
```

### Color palettes (`ui.colors` / legacy `ui.theme`)

Twenty built-in palettes. The first six are product-oriented; the rest are
familiar editor palettes. Changing colors never changes borders or bar height.

| Name | Feel |
|------|------|
| `tokyo-night` (default) | Cool blue night editor palette |
| `midnight` (alias `classic`) | Classic dark + cyan |
| `modern` | Flat slate / indigo |
| `soft` | Warm low-contrast stone |
| `neon` | Deep black + electric pink / cyan |
| `paper` | Light warm paper / ink |
| `minimal` | Near-monochrome zinc |

Also: `daylight`, `contrast`, `nord`, `dracula`, `gruvbox`, `catppuccin`,
`solarized-dark`, `solarized-light`, `forest`, `rose-pine`, `ocean`,
`ember`, `monokai`.

Layout structure defaults to **`classic`** (`ui.layout`); palette defaults to
**`tokyo-night`** (`ui.colors`). They are independent.

```sh
vmux config set ui.colors modern
# still accepted:
vmux config set ui.theme modern
```

## Responsive layout

Below 90 columns, the workspace sidebar auto-hides so panes get the full width
(`ui.sidebar_responsive`, default on). When it is hidden, reach workspaces
through the **📱 menu** button on the control bar, or press `Ctrl-b w` for the
picker (`j`/`k` or arrows, `Enter` to switch, `Esc` to close).

## Project actions

A `vmux.json` in a project root defines named commands, which show up under
`Ctrl-b A` and `vmux actions`:

```sh
vmux actions list
vmux actions run test
```

## Update checks

vmux checks for a new release once a day and prints a notice. Disable it by
setting `VMUX_NO_UPDATE_CHECK=1`. Site and releases: https://vmux.sh
