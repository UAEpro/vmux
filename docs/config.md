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
| `ui.theme` | see below | Color theme |
| `ui.status_markers` | `emoji`, `ascii`, `off` | How agent status renders in the sidebar |
| `ui.cursor` | set in the Settings panel | Cursor style and blink |

Relay keys (`relay.enabled`, `relay.bind`, `relay.allow_localhost`) are covered
in [relay.md](relay.md).

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
