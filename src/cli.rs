use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

#[derive(Debug, Parser)]
#[command(
    name = "vmux",
    version,
    about = "vmux — the Linux terminal born from the cmux revolution"
)]
pub struct Cli {
    #[arg(long, env = "VMUX_SESSION", default_value = "default", global = true)]
    pub session: String,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    Attach,
    Daemon {
        #[arg(long)]
        foreground: bool,
    },
    NewPane {
        #[arg(long, value_enum, default_value_t = SplitDirection::Right)]
        direction: SplitDirection,

        #[arg(long, default_value = "")]
        command: String,

        #[arg(long)]
        title: Option<String>,

        #[arg(long)]
        workspace: Option<String>,
    },
    Split {
        #[arg(value_enum, default_value_t = SplitDirection::Right)]
        direction: SplitDirection,

        #[arg(long, default_value = "")]
        command: String,

        #[arg(long)]
        title: Option<String>,

        #[arg(long)]
        workspace: Option<String>,
    },
    Run {
        #[arg(long, value_enum, default_value_t = SplitDirection::Right)]
        direction: SplitDirection,

        #[arg(long)]
        command: String,

        #[arg(long)]
        title: Option<String>,

        #[arg(long)]
        workspace: Option<String>,

        #[arg(
            long,
            help = "Seconds to wait for the pane to finish (default 300; 0 waits forever)"
        )]
        timeout: Option<u64>,
    },
    OpenUrl {
        url: String,

        #[arg(long, value_enum, default_value_t = SplitDirection::Right)]
        direction: SplitDirection,

        #[arg(long)]
        title: Option<String>,

        #[arg(long)]
        workspace: Option<String>,
    },
    UrlSnapshot {
        url: String,
    },
    UrlLinks {
        url: String,
    },
    Browser {
        #[command(subcommand)]
        command: BrowserCommand,
    },
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
    /// Opt-in cmux-remote compatible phone relay (Tailscale / LAN).
    /// Does not change daemon/attach behaviour unless you start it.
    Relay {
        #[command(subcommand)]
        command: RelayCommand,
    },
    /// Detected listening ports in panes, SSH copy helpers, Tailscale forward.
    Ports {
        #[command(subcommand)]
        command: PortsCommand,
    },
    Markdown {
        #[command(subcommand)]
        command: MarkdownCommand,
    },
    Actions {
        #[command(subcommand)]
        command: ActionCommand,
    },
    Skills {
        #[command(subcommand)]
        command: SkillsCommand,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Send {
        #[arg(long)]
        pane: Option<String>,

        #[arg(long)]
        enter: bool,

        text: Vec<String>,
    },
    SendKey {
        #[arg(long)]
        pane: Option<String>,

        keys: Vec<String>,
    },
    /// Save an image on this host and type its path into a pane.
    ///
    /// Built for pasting screenshots into agents like Claude Code over SSH,
    /// where Ctrl+V can't work because the clipboard lives on your local
    /// machine: `pngpaste - | ssh host vmux send-image -` (macOS) or
    /// `wl-paste -t image/png | ssh host vmux send-image -` (Linux).
    SendImage {
        /// Image file to read, or `-` to read image bytes from stdin.
        file: String,

        #[arg(long)]
        pane: Option<String>,

        /// Press Enter after typing the path.
        #[arg(long)]
        enter: bool,
    },
    Broadcast {
        #[arg(long, value_enum, default_value_t = BroadcastScope::Workspace)]
        scope: BroadcastScope,

        #[arg(long)]
        enter: bool,

        text: Vec<String>,
    },
    ReadScreen {
        #[arg(long)]
        pane: Option<String>,

        #[arg(long)]
        no_scrollback: bool,

        #[arg(long, default_value_t = 16_000)]
        limit_bytes: usize,
    },
    Search {
        #[arg(long)]
        pane: Option<String>,

        query: String,
    },
    ClearPane {
        #[arg(long)]
        pane: Option<String>,
    },
    CopyPane {
        #[arg(long)]
        pane: Option<String>,

        #[arg(long)]
        scrollback: bool,

        #[arg(long, default_value_t = 16_000)]
        limit_bytes: usize,
    },
    Paste {
        #[arg(long)]
        pane: Option<String>,

        #[arg(long)]
        enter: bool,
    },
    Clipboard,
    KillPane {
        #[arg(long)]
        pane: Option<String>,
    },
    DuplicatePane {
        #[arg(long)]
        pane: Option<String>,

        #[arg(long, value_enum, default_value_t = SplitDirection::Right)]
        direction: SplitDirection,
    },
    Prune {
        #[arg(long)]
        workspace: Option<String>,

        #[arg(long)]
        all: bool,
    },
    RestartPane {
        #[arg(long)]
        pane: Option<String>,

        #[arg(long)]
        workspace: Option<String>,

        #[arg(long)]
        all: bool,

        #[arg(long)]
        command: Option<String>,
    },
    MovePane {
        #[arg(long)]
        pane: Option<String>,

        #[arg(long)]
        workspace: Option<String>,

        #[arg(long)]
        new_workspace: Option<String>,

        #[arg(long, value_enum, default_value_t = SplitDirection::Right)]
        direction: SplitDirection,
    },
    SwapPanes {
        #[arg(long)]
        first: String,

        #[arg(long)]
        second: String,
    },
    Title {
        #[arg(long)]
        pane: Option<String>,

        title: String,
    },
    /// Workspace tabs (Workspace → Tab → Pane). Prefer this over the removed pane-tab API.
    Tab {
        #[command(subcommand)]
        command: TabCommand,
    },
    /// Move/swap the active (or given) pane with its layout neighbor (no wrap).
    Move {
        #[arg(value_enum)]
        direction: SplitDirection,

        #[arg(long)]
        pane: Option<String>,
    },
    /// @deprecated Use `vmux tab` (workspace tabs). Still accepted, returns a migration error.
    PaneTab {
        #[command(subcommand)]
        command: PaneTabCommand,
    },
    Metadata {
        #[command(subcommand)]
        command: MetadataCommand,
    },
    Wait {
        #[arg(long)]
        pane: Option<String>,

        #[arg(long)]
        workspace: Option<String>,

        #[arg(long)]
        all: bool,

        #[arg(
            long,
            value_delimiter = ',',
            help = "Wait for an agent status instead of process exit \
                    (comma-separated: busy, attention, done, error, idle; \
                    a pane exiting also ends the wait)"
        )]
        status: Vec<String>,

        #[arg(
            long,
            help = "Seconds to wait before giving up (default 300; 0 waits forever)"
        )]
        timeout: Option<u64>,
    },
    /// Explain herdr-style screen-manifest agent detection (offline).
    ///
    /// Reads screen text from stdin (or `--file`) and classifies idle / working
    /// / blocked using the bundled per-agent TOML rules. Hooks are not consulted
    /// — this is the same path the daemon uses as primary status for Claude,
    /// Codex, and other screen-authority agents.
    Detect {
        /// Agent kind: claude, codex, grok, cursor, gemini, opencode, amp.
        #[arg(long)]
        agent: String,

        /// Screen dump file (default: stdin).
        #[arg(long)]
        file: Option<String>,

        /// OSC window title (optional; e.g. braille spinner for Claude working).
        #[arg(long, default_value = "")]
        osc_title: String,

        /// OSC 9;4 progress body after `9;` (optional; e.g. `4;0`).
        #[arg(long, default_value = "")]
        osc_progress: String,

        /// Emit JSON instead of a short human line.
        #[arg(long)]
        json: bool,
    },
    Resize {
        #[arg(value_enum)]
        direction: SplitDirection,

        #[arg(long, default_value_t = 5)]
        amount: u16,
    },
    /// Temporarily fit a pane's PTY to a small viewer (phone-fit), or restore it.
    ///
    /// Sets a leased override: the pane runs at min(layout, view) per axis until
    /// the lease expires or --clear restores it. The relay drives this
    /// automatically for subscribed surfaces that report a view size.
    ViewSize {
        /// Pane id (defaults to the pane this command runs in).
        #[arg(long)]
        pane: Option<String>,

        #[arg(long, requires = "rows", conflicts_with = "clear")]
        cols: Option<u16>,

        #[arg(long, requires = "cols", conflicts_with = "clear")]
        rows: Option<u16>,

        /// Milliseconds before the override expires unless re-sent.
        #[arg(long, default_value_t = 10_000)]
        lease_ms: u64,

        /// Drop the override and restore the layout size now.
        #[arg(long)]
        clear: bool,
    },
    Focus {
        #[arg(value_enum)]
        direction: SplitDirection,
    },
    FocusPane {
        #[arg(long)]
        pane: String,
    },
    Zoom {
        #[arg(long)]
        pane: Option<String>,
    },
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    Surface {
        #[command(subcommand)]
        command: SurfaceCommand,
    },
    Progress {
        #[command(subcommand)]
        command: ProgressCommand,
    },
    Hooks {
        #[command(subcommand)]
        command: HooksCommand,
    },
    SetProgress {
        value: u8,

        #[arg(long)]
        pane: Option<String>,
    },
    SetStatus {
        status: String,

        #[arg(long)]
        pane: Option<String>,

        #[arg(long)]
        workspace: Option<String>,

        #[arg(long)]
        color: Option<String>,

        #[arg(long, default_value = "")]
        message: String,
    },
    Notify {
        #[arg(long)]
        pane: Option<String>,

        #[arg(long)]
        workspace: Option<String>,

        #[arg(long)]
        status: Option<String>,

        #[arg(long)]
        color: Option<String>,

        #[arg(long)]
        clear: bool,

        #[arg(long, default_value = "")]
        message: String,
    },
    Notifications {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Events {
        #[arg(long, default_value_t = 50)]
        limit: usize,

        /// Only events with id greater than this (incremental follow).
        #[arg(long)]
        since: Option<u64>,

        #[arg(long)]
        follow: bool,

        #[arg(long, default_value_t = 500)]
        interval_ms: u64,
    },
    ClearNotifications,
    JumpNotification,
    Identify {
        #[arg(long)]
        pane: Option<String>,

        #[arg(long)]
        json: bool,
    },
    List,
    Agents,
    Status,
    Sessions,
    Logs {
        #[arg(long, default_value_t = 200)]
        lines: usize,
    },
    Doctor,
    Smoke {
        #[arg(long)]
        keep: bool,
    },
    Stop,
}

#[derive(Debug, Clone, Subcommand)]
pub enum WorkspaceCommand {
    New {
        #[arg(long, default_value = "workspace")]
        name: String,

        #[arg(long)]
        cwd: Option<String>,

        #[arg(long, default_value = "")]
        command: String,

        #[arg(long)]
        title: Option<String>,

        #[arg(long, value_enum, default_value_t = SplitDirection::Right)]
        direction: SplitDirection,
    },
    Switch {
        workspace: String,
    },
    Next,
    Previous,
    Rename {
        workspace: String,
        name: String,
    },
    Close {
        workspace: Option<String>,
    },
    Cwd {
        workspace: String,
        cwd: String,
    },
    Pin {
        workspace: String,
    },
    Unpin {
        workspace: String,
    },
    Move {
        workspace: String,

        #[arg(long)]
        position: usize,
    },
    List,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ActionCommand {
    List {
        #[arg(long)]
        workspace: Option<String>,
    },
    Run {
        name: String,

        #[arg(long)]
        workspace: Option<String>,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum SkillsCommand {
    List,
    Show {
        #[arg(default_value = "vmux-control")]
        name: String,
    },
    Install {
        #[arg(default_value = "vmux-control")]
        name: String,

        #[arg(long)]
        dir: Option<String>,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum ConfigCommand {
    Show,
    Init {
        #[arg(long)]
        force: bool,
    },
    Set {
        key: String,
        value: String,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum BrowserCommand {
    Open {
        url: String,

        #[arg(long, value_enum, default_value_t = SplitDirection::Right)]
        direction: SplitDirection,

        #[arg(long)]
        title: Option<String>,

        #[arg(long)]
        workspace: Option<String>,
    },
    Snapshot {
        url: String,
    },
    Screenshot {
        url: String,
    },
    Links {
        url: String,
    },
    Forms {
        url: String,
    },
    Evaluate {
        url: String,

        #[arg(default_value = "title")]
        expression: String,
    },
    Console {
        url: String,
    },
    Network {
        url: String,
    },
    OpenLink {
        url: String,

        #[arg(long)]
        index: usize,

        #[arg(long, value_enum, default_value_t = SplitDirection::Right)]
        direction: SplitDirection,

        #[arg(long)]
        title: Option<String>,

        #[arg(long)]
        workspace: Option<String>,
    },
    Click {
        url: String,

        #[arg(long)]
        index: usize,

        #[arg(long, value_enum, default_value_t = SplitDirection::Right)]
        direction: SplitDirection,

        #[arg(long)]
        title: Option<String>,

        #[arg(long)]
        workspace: Option<String>,
    },
    Submit {
        url: String,

        #[arg(long)]
        index: usize,

        #[arg(long = "field")]
        fields: Vec<String>,

        #[arg(long, value_enum, default_value_t = SplitDirection::Right)]
        direction: SplitDirection,

        #[arg(long)]
        title: Option<String>,

        #[arg(long)]
        workspace: Option<String>,
    },
    Fill {
        url: String,

        #[arg(long)]
        index: usize,

        #[arg(long = "field")]
        fields: Vec<String>,

        #[arg(long, value_enum, default_value_t = SplitDirection::Right)]
        direction: SplitDirection,

        #[arg(long)]
        title: Option<String>,

        #[arg(long)]
        workspace: Option<String>,
    },
    Type {
        url: String,

        #[arg(long)]
        index: usize,

        #[arg(long = "field")]
        fields: Vec<String>,

        #[arg(long, value_enum, default_value_t = SplitDirection::Right)]
        direction: SplitDirection,

        #[arg(long)]
        title: Option<String>,

        #[arg(long)]
        workspace: Option<String>,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum TabCommand {
    List {
        #[arg(long)]
        workspace: Option<String>,
    },
    New {
        #[arg(long)]
        workspace: Option<String>,

        #[arg(long)]
        title: Option<String>,

        #[arg(long)]
        command: Option<String>,
    },
    Switch {
        tab: String,

        #[arg(long)]
        workspace: Option<String>,
    },
    Rename {
        tab: String,

        title: String,

        #[arg(long)]
        workspace: Option<String>,
    },
    Close {
        tab: String,

        #[arg(long)]
        workspace: Option<String>,
    },
    Next {
        #[arg(long)]
        workspace: Option<String>,
    },
    Previous {
        #[arg(long)]
        workspace: Option<String>,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum PaneTabCommand {
    List {
        #[arg(long)]
        pane: Option<String>,
    },
    Add {
        #[arg(long)]
        pane: Option<String>,

        #[arg(long)]
        title: String,

        #[arg(long, default_value = "")]
        command: String,

        #[arg(long, value_enum, default_value_t = SurfaceKindArg::Terminal)]
        kind: SurfaceKindArg,
    },
    Switch {
        tab: String,

        #[arg(long)]
        pane: Option<String>,
    },
    Rename {
        tab: String,

        title: String,

        #[arg(long)]
        pane: Option<String>,
    },
    Close {
        tab: String,

        #[arg(long)]
        pane: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SurfaceKindArg {
    Terminal,
    Browser,
    Agent,
    Markdown,
}

#[derive(Debug, Clone, Subcommand)]
pub enum MetadataCommand {
    List {
        #[arg(long)]
        pane: Option<String>,
    },
    Set {
        key: String,
        value: String,

        #[arg(long)]
        pane: Option<String>,
    },
    Clear {
        key: String,

        #[arg(long)]
        pane: Option<String>,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum AgentCommand {
    New {
        #[arg(long, value_enum, default_value_t = SplitDirection::Right)]
        direction: SplitDirection,

        #[arg(long, default_value = "claude")]
        command: String,

        #[arg(long)]
        title: Option<String>,

        #[arg(long)]
        workspace: Option<String>,
    },
    Team {
        #[arg(long, value_delimiter = ',', default_value = "codex,claude")]
        agents: Vec<String>,

        #[arg(long)]
        cwd: Option<String>,

        #[arg(long, value_enum, default_value_t = SplitDirection::Right)]
        direction: SplitDirection,

        #[arg(long)]
        no_agents_md: bool,
    },
    List,
    Send {
        #[arg(long)]
        agent: Option<String>,

        #[arg(long)]
        enter: bool,

        text: Vec<String>,
    },
    Read {
        #[arg(long)]
        agent: Option<String>,

        #[arg(long)]
        no_scrollback: bool,

        #[arg(long, default_value_t = 16_000)]
        limit_bytes: usize,
    },
    Notify {
        #[arg(long)]
        agent: Option<String>,

        #[arg(long)]
        status: Option<String>,

        #[arg(long)]
        color: Option<String>,

        #[arg(long, default_value = "")]
        message: String,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum RelayCommand {
    /// Start the phone relay (HTTP + WebSocket).
    ///
    /// Default port is 4399 (Cmux Remote). Change with `--listen host:port`
    /// or `vmux config set relay.port <n>` (attach-managed relay).
    Serve {
        #[arg(long)]
        config: Option<String>,

        #[arg(
            long,
            help = "Override listen address (host:port), e.g. 127.0.0.1:4400"
        )]
        listen: Option<String>,

        #[arg(long, help = "Override TCP port only (merged with resolved host)")]
        port: Option<u16>,

        #[arg(
            long,
            help = "Allow device registration from localhost (dev / simulator)"
        )]
        allow_localhost: bool,
    },
    /// Show relay config, paired devices, and health probe.
    Status {
        #[arg(long)]
        config: Option<String>,
    },
    /// List or revoke paired mobile devices.
    Devices {
        #[command(subcommand)]
        command: RelayDevicesCommand,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum PortsCommand {
    /// List listening ports attributed to pane processes.
    List {
        #[arg(long)]
        workspace: Option<String>,

        #[arg(long)]
        json: bool,
    },
    /// Print an `ssh -L` command to forward a port to your laptop.
    SshCmd { port: u16 },
    /// Expose a detected port on the Tailscale interface (daemon TCP proxy).
    Forward {
        port: u16,
        #[arg(long, default_value = "tailscale")]
        via: String,
    },
    /// Stop a Tailscale forward started by `ports forward`.
    Unforward { port: u16 },
}

#[derive(Debug, Clone, Subcommand)]
pub enum RelayDevicesCommand {
    List,
    Revoke { device_id: String },
}

#[derive(Debug, Clone, Subcommand)]
pub enum RemoteCommand {
    Ssh {
        host: String,

        #[arg(long)]
        workspace: Option<String>,

        #[arg(long)]
        command: Option<String>,

        #[arg(long)]
        title: Option<String>,

        #[arg(long, value_enum, default_value_t = SplitDirection::Right)]
        direction: SplitDirection,
    },
    Tmux {
        host: String,

        #[arg(long, default_value = "default")]
        session: String,

        #[arg(long)]
        workspace: Option<String>,

        #[arg(long)]
        title: Option<String>,

        #[arg(long, value_enum, default_value_t = SplitDirection::Right)]
        direction: SplitDirection,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum MarkdownCommand {
    Open {
        source: String,

        #[arg(long, value_enum, default_value_t = SplitDirection::Right)]
        direction: SplitDirection,

        #[arg(long)]
        title: Option<String>,

        #[arg(long)]
        workspace: Option<String>,
    },
    Command {
        source: String,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum SurfaceCommand {
    New {
        #[arg(long, value_enum, default_value_t = SplitDirection::Right)]
        direction: SplitDirection,

        #[arg(long, default_value = "")]
        command: String,

        #[arg(long)]
        title: Option<String>,

        #[arg(long)]
        workspace: Option<String>,
    },
    Send {
        #[arg(long)]
        surface: Option<String>,

        #[arg(long)]
        enter: bool,

        text: Vec<String>,
    },
    SendKey {
        #[arg(long)]
        surface: Option<String>,

        keys: Vec<String>,
    },
    Read {
        #[arg(long)]
        surface: Option<String>,

        #[arg(long)]
        no_scrollback: bool,

        #[arg(long, default_value_t = 16_000)]
        limit_bytes: usize,
    },
    Kill {
        #[arg(long)]
        surface: Option<String>,
    },
    Focus {
        #[arg(long)]
        surface: String,
    },
    Duplicate {
        #[arg(long)]
        surface: Option<String>,

        #[arg(long, value_enum, default_value_t = SplitDirection::Right)]
        direction: SplitDirection,
    },
    Swap {
        #[arg(long)]
        first: String,

        #[arg(long)]
        second: String,
    },
    List,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ProgressCommand {
    Set {
        value: u8,

        #[arg(long)]
        pane: Option<String>,
    },
    Clear {
        #[arg(long)]
        pane: Option<String>,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum HooksCommand {
    /// Print shell helper functions (eval "$(vmux hooks shell)").
    Shell {
        #[arg(long, value_enum, default_value_t = HookShell::Bash)]
        shell: HookShell,
    },
    /// Write shell helpers to a file and optionally source them from an rc file.
    Setup {
        #[arg(long, value_enum, default_value_t = HookShell::Bash)]
        shell: HookShell,

        #[arg(long)]
        dir: Option<String>,

        #[arg(long)]
        rc: Option<String>,
    },
    /// Show whether coding-agent hooks (shell/Claude/Codex/Grok) are installed.
    Status,
    /// Install vmux sidebar status hooks for coding agents (and shell helpers).
    ///
    /// Writes Claude Code settings, Codex hooks.json, Grok hooks
    /// (`~/.grok/hooks/vmux.json`) plus the control skill, and shell helpers.
    /// Safe to re-run (idempotent; merges without wiping existing hooks).
    Install {
        /// One of: shell, claude, codex, grok (default: all).
        #[arg(long)]
        agent: Option<String>,
    },
    /// Translate agent hook JSON (stdin) into a vmux notification / status.
    Event {
        #[arg(long)]
        event: Option<String>,

        #[arg(long)]
        pane: Option<String>,

        #[arg(long)]
        workspace: Option<String>,

        #[arg(long)]
        status: Option<String>,

        #[arg(long)]
        color: Option<String>,

        #[arg(long)]
        message: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum HookShell {
    Bash,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BroadcastScope {
    Workspace,
    Session,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SplitDirection {
    Right,
    Left,
    Up,
    Down,
}

impl std::fmt::Display for SplitDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            SplitDirection::Right => "right",
            SplitDirection::Left => "left",
            SplitDirection::Up => "up",
            SplitDirection::Down => "down",
        };
        f.write_str(value)
    }
}
