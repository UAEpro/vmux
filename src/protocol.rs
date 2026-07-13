use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use crate::cli::{BroadcastScope, SplitDirection};
use crate::model::SurfaceKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum Request {
    Ping,
    /// Fetch session snapshot.
    /// - `since`: if equal to the daemon generation, returns `{unchanged:true,generation}` only.
    /// - `full`: when false, omits heavy per-pane scrollback strings (layout/status poll).
    /// - `lean`: attach-UI poll — omits `events` and per-pane/tab scrollback
    ///   strings (except panes listed in `scrollback_panes`) while still
    ///   including live screen contents and `scrollback_lines` counts.
    Snapshot {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since: Option<u64>,
        /// Default true for backward-compatible full payloads.
        #[serde(default = "default_true")]
        full: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        lean: bool,
        /// Panes whose scrollback strings must be included even when `lean`
        /// (the client is scrolled back in them).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        scrollback_panes: Vec<String>,
    },
    List,
    Agents,
    Identify {
        pane: Option<String>,
    },
    NewPane {
        direction: SplitDirection,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        surface_kind: Option<SurfaceKind>,
    },
    DuplicatePane {
        pane: Option<String>,
        direction: SplitDirection,
    },
    OpenUrl {
        url: String,
        direction: SplitDirection,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    UrlSnapshot {
        url: String,
    },
    UrlLinks {
        url: String,
    },
    UrlForms {
        url: String,
    },
    UrlEvaluate {
        url: String,
        expression: String,
    },
    UrlConsole {
        url: String,
    },
    UrlNetwork {
        url: String,
    },
    OpenUrlLink {
        url: String,
        index: usize,
        direction: SplitDirection,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    SubmitForm {
        url: String,
        index: usize,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        fields: BTreeMap<String, String>,
        direction: SplitDirection,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    CustomActions {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    RunCustomAction {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    KillPane {
        pane: Option<String>,
    },
    Prune {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        all: bool,
    },
    RestartPane {
        pane: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        all: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
    },
    MovePane {
        pane: Option<String>,
        workspace: String,
        direction: SplitDirection,
    },
    /// Swap the pane with its layout neighbor in `direction` (no wrap).
    MovePaneInLayout {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<String>,
        direction: SplitDirection,
    },
    SwapPanes {
        first: String,
        second: String,
    },
    SetPaneTitle {
        pane: Option<String>,
        title: String,
    },
    SetPaneMetadata {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<String>,
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
    /// List workspace tabs (Workspace → Tab → Pane hierarchy).
    ListTabs {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    NewTab {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
    },
    SwitchTab {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        tab: String,
    },
    RenameTab {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        tab: String,
        title: String,
    },
    CloseTab {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        tab: String,
    },
    // --- Legacy per-pane tabs (removed). Kept so old clients get a clear error. ---
    /// @deprecated Use ListTabs
    PaneTabs {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<String>,
    },
    /// @deprecated Use NewTab
    AddPaneTab {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<String>,
        title: String,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        surface_kind: Option<SurfaceKind>,
    },
    /// @deprecated Use SwitchTab
    SwitchPaneTab {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<String>,
        tab: String,
    },
    /// @deprecated Use RenameTab
    RenamePaneTab {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<String>,
        tab: String,
        title: String,
    },
    /// @deprecated Use CloseTab
    ClosePaneTab {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<String>,
        tab: String,
    },
    WaitPane {
        pane: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        all: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    NewWorkspace {
        name: String,
        cwd: Option<String>,
    },
    SwitchWorkspace {
        workspace: String,
    },
    RenameWorkspace {
        workspace: String,
        name: String,
    },
    CloseWorkspace {
        workspace: Option<String>,
    },
    SetWorkspaceCwd {
        workspace: String,
        cwd: String,
    },
    SetWorkspacePinned {
        workspace: String,
        pinned: bool,
    },
    MoveWorkspace {
        workspace: String,
        position: usize,
    },
    FocusPane {
        pane: String,
    },
    FocusDirection {
        direction: SplitDirection,
    },
    ToggleZoom {
        pane: Option<String>,
    },
    Resize {
        direction: SplitDirection,
        amount: u16,
    },
    Input {
        pane: Option<String>,
        data: String,
    },
    SendKey {
        pane: Option<String>,
        keys: Vec<String>,
    },
    Broadcast {
        scope: BroadcastScope,
        data: String,
    },
    Notify {
        pane: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        status: Option<String>,
        color: Option<String>,
        clear: bool,
        message: String,
    },
    Notifications {
        limit: usize,
    },
    Events {
        limit: usize,
    },
    ClearNotifications,
    JumpNotification,
    Progress {
        pane: Option<String>,
        value: Option<u8>,
    },
    ReadScreen {
        pane: Option<String>,
        #[serde(default = "default_true", skip_serializing_if = "is_true")]
        scrollback: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit_bytes: Option<usize>,
    },
    Search {
        pane: Option<String>,
        query: String,
    },
    ClearPane {
        pane: Option<String>,
    },
    CopyPane {
        pane: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        scrollback: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit_bytes: Option<usize>,
    },
    Paste {
        pane: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        enter: bool,
    },
    Clipboard,
    SetClipboard {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_pane: Option<String>,
        source: String,
    },
    PaneSizes {
        panes: BTreeMap<String, PaneSize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        take_control: bool,
    },
    Shutdown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneSize {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok<T: Serialize>(data: T) -> Self {
        Self {
            ok: true,
            data: Some(serde_json::to_value(data).unwrap_or(Value::Null)),
            error: None,
        }
    }

    pub fn empty() -> Self {
        Self {
            ok: true,
            data: None,
            error: None,
        }
    }

    pub fn err(error: impl ToString) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(error.to_string()),
        }
    }
}

/// Default read/write timeout for regular request/response commands.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Extra slack added on top of a long-poll's own deadline so the daemon
/// wins the timeout race and returns a proper response/error.
const LONG_POLL_SLACK: Duration = Duration::from_secs(5);

/// Read timeout for URL/browser fetch commands. The daemon runs curl with
/// `--max-time 30`, so the client must wait past that (plus slack) or it fails
/// with a spurious "daemon unresponsive" while the daemon fetch still succeeds.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30 + 5);

/// Read timeout for a given request. Long-polling commands (`WaitPane`) run
/// arbitrarily long on the daemon, so honour their own deadline instead of the
/// default; a `None` deadline means wait indefinitely.
fn read_timeout_for(request: &Request) -> Option<Duration> {
    match request {
        Request::WaitPane {
            timeout_ms: Some(timeout_ms),
            ..
        } => Some(Duration::from_millis(*timeout_ms) + LONG_POLL_SLACK),
        Request::WaitPane {
            timeout_ms: None, ..
        } => None,
        // Browser/url fetches shell out to curl (--max-time 30) on the daemon;
        // the client must outwait that deadline.
        Request::UrlSnapshot { .. }
        | Request::UrlLinks { .. }
        | Request::UrlForms { .. }
        | Request::UrlEvaluate { .. }
        | Request::UrlConsole { .. }
        | Request::UrlNetwork { .. }
        | Request::OpenUrlLink { .. } => Some(FETCH_TIMEOUT),
        _ => Some(DEFAULT_TIMEOUT),
    }
}

/// Full snapshot request (CLI default).
pub fn snapshot_full() -> Request {
    Request::Snapshot {
        since: None,
        full: true,
        lean: false,
        scrollback_panes: Vec::new(),
    }
}

/// Extract Session JSON from a Snapshot response (new envelope or legacy flat).
pub fn session_data_from_snapshot(data: Value) -> Value {
    data.get("session").cloned().unwrap_or(data)
}

pub fn request(path: &Path, request: &Request) -> Result<Response> {
    let mut stream = UnixStream::connect(path)
        .with_context(|| format!("connect vmux socket {}", path.display()))?;
    stream
        .set_write_timeout(Some(DEFAULT_TIMEOUT))
        .context("set vmux socket write timeout")?;
    stream
        .set_read_timeout(read_timeout_for(request))
        .context("set vmux socket read timeout")?;
    serde_json::to_writer(&mut stream, request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .context("read vmux response (daemon may be unresponsive)")?;
    if line.trim().is_empty() {
        anyhow::bail!("empty vmux response; daemon may be old or crashed");
    }
    serde_json::from_str(&line).context("decode vmux response")
}

pub fn write_response(mut stream: &UnixStream, response: &Response) -> Result<()> {
    serde_json::to_writer(&mut stream, response)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_true(value: &bool) -> bool {
    *value
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_request_defaults_and_since() {
        let encoded = serde_json::to_string(&snapshot_full()).unwrap();
        assert!(encoded.contains(r#""action":"snapshot""#));
        assert!(encoded.contains(r#""full":true"#));
        let with_since = serde_json::to_string(&Request::Snapshot {
            since: Some(7),
            full: false,
            lean: false,
            scrollback_panes: Vec::new(),
        })
        .unwrap();
        assert!(with_since.contains(r#""since":7"#));
        assert!(with_since.contains(r#""full":false"#));
        // Lean fields stay off the wire unless set (old daemons keep working).
        assert!(!with_since.contains("lean"));
        assert!(!with_since.contains("scrollback_panes"));
        // Legacy clients sending bare {"action":"snapshot"} still decode.
        let bare: Request = serde_json::from_str(r#"{"action":"snapshot"}"#).unwrap();
        match bare {
            Request::Snapshot {
                since,
                full,
                lean,
                scrollback_panes,
            } => {
                assert!(since.is_none());
                assert!(full);
                assert!(!lean);
                assert!(scrollback_panes.is_empty());
            }
            _ => panic!("expected Snapshot"),
        }
    }

    #[test]
    fn session_data_from_snapshot_unwraps_envelope() {
        let nested = serde_json::json!({"generation": 3, "session": {"name": "default"}});
        let session = session_data_from_snapshot(nested);
        assert_eq!(
            session.get("name").and_then(|v| v.as_str()),
            Some("default")
        );
        let flat = serde_json::json!({"name": "legacy"});
        assert_eq!(
            session_data_from_snapshot(flat)
                .get("name")
                .and_then(|v| v.as_str()),
            Some("legacy")
        );
    }

    #[test]
    fn pane_sizes_request_uses_socket_protocol_shape() {
        let mut panes = BTreeMap::new();
        panes.insert("pane-1".to_string(), PaneSize { rows: 12, cols: 80 });
        let encoded = serde_json::to_string(&Request::PaneSizes {
            panes,
            client_id: None,
            take_control: false,
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"pane-sizes","panes":{"pane-1":{"rows":12,"cols":80}}}"#
        );
    }

    #[test]
    fn agents_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::Agents).unwrap();
        assert_eq!(encoded, r#"{"action":"agents"}"#);
    }

    #[test]
    fn events_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::Events { limit: 10 }).unwrap();
        assert_eq!(encoded, r#"{"action":"events","limit":10}"#);
    }

    #[test]
    fn identify_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::Identify {
            pane: Some("pane-1".to_string()),
        })
        .unwrap();
        assert_eq!(encoded, r#"{"action":"identify","pane":"pane-1"}"#);
    }

    #[test]
    fn close_workspace_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::CloseWorkspace {
            workspace: Some("ws-2".to_string()),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"close-workspace","workspace":"ws-2"}"#
        );
    }

    #[test]
    fn set_workspace_pinned_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::SetWorkspacePinned {
            workspace: "ws-2".to_string(),
            pinned: true,
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"set-workspace-pinned","workspace":"ws-2","pinned":true}"#
        );
    }

    #[test]
    fn move_workspace_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::MoveWorkspace {
            workspace: "ws-2".to_string(),
            position: 1,
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"move-workspace","workspace":"ws-2","position":1}"#
        );
    }

    #[test]
    fn swap_panes_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::SwapPanes {
            first: "pane-1".to_string(),
            second: "pane-2".to_string(),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"swap-panes","first":"pane-1","second":"pane-2"}"#
        );
    }

    #[test]
    fn set_pane_title_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::SetPaneTitle {
            pane: Some("pane-1".to_string()),
            title: "backend-agent".to_string(),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"set-pane-title","pane":"pane-1","title":"backend-agent"}"#
        );
    }

    #[test]
    fn set_pane_metadata_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::SetPaneMetadata {
            pane: Some("pane-1".to_string()),
            key: "task".to_string(),
            value: Some("auth-api".to_string()),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"set-pane-metadata","pane":"pane-1","key":"task","value":"auth-api"}"#
        );

        let encoded = serde_json::to_string(&Request::SetPaneMetadata {
            pane: None,
            key: "task".to_string(),
            value: None,
        })
        .unwrap();
        assert_eq!(encoded, r#"{"action":"set-pane-metadata","key":"task"}"#);
    }

    #[test]
    fn pane_tab_requests_use_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::AddPaneTab {
            pane: Some("pane-1".to_string()),
            title: "tests".to_string(),
            command: "cargo test".to_string(),
            surface_kind: Some(SurfaceKind::Terminal),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"add-pane-tab","pane":"pane-1","title":"tests","command":"cargo test","surface_kind":"terminal"}"#
        );

        let encoded = serde_json::to_string(&Request::SwitchPaneTab {
            pane: None,
            tab: "tab-2".to_string(),
        })
        .unwrap();
        assert_eq!(encoded, r#"{"action":"switch-pane-tab","tab":"tab-2"}"#);

        let encoded = serde_json::to_string(&Request::PaneTabs {
            pane: Some("pane-1".to_string()),
        })
        .unwrap();
        assert_eq!(encoded, r#"{"action":"pane-tabs","pane":"pane-1"}"#);
    }

    #[test]
    fn focus_pane_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::FocusPane {
            pane: "pane-1".to_string(),
        })
        .unwrap();
        assert_eq!(encoded, r#"{"action":"focus-pane","pane":"pane-1"}"#);
    }

    #[test]
    fn toggle_zoom_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::ToggleZoom {
            pane: Some("pane-1".to_string()),
        })
        .unwrap();
        assert_eq!(encoded, r#"{"action":"toggle-zoom","pane":"pane-1"}"#);
    }

    #[test]
    fn new_pane_request_can_include_title() {
        let encoded = serde_json::to_string(&Request::NewPane {
            direction: SplitDirection::Right,
            command: "claude".to_string(),
            title: Some("backend-agent".to_string()),
            workspace: Some("ws-2".to_string()),
            surface_kind: None,
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"new-pane","direction":"right","command":"claude","title":"backend-agent","workspace":"ws-2"}"#
        );
    }

    #[test]
    fn duplicate_pane_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::DuplicatePane {
            pane: Some("pane-1".to_string()),
            direction: SplitDirection::Down,
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"duplicate-pane","pane":"pane-1","direction":"down"}"#
        );
    }

    #[test]
    fn new_pane_request_can_mark_agent_surface() {
        let encoded = serde_json::to_string(&Request::NewPane {
            direction: SplitDirection::Right,
            command: "claude".to_string(),
            title: Some("backend-agent".to_string()),
            workspace: Some("ws-2".to_string()),
            surface_kind: Some(SurfaceKind::Agent),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"new-pane","direction":"right","command":"claude","title":"backend-agent","workspace":"ws-2","surface_kind":"agent"}"#
        );
    }

    #[test]
    fn new_pane_request_can_mark_markdown_surface() {
        let encoded = serde_json::to_string(&Request::NewPane {
            direction: SplitDirection::Right,
            command: "cat README.md".to_string(),
            title: Some("README".to_string()),
            workspace: Some("ws-2".to_string()),
            surface_kind: Some(SurfaceKind::Markdown),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"new-pane","direction":"right","command":"cat README.md","title":"README","workspace":"ws-2","surface_kind":"markdown"}"#
        );
    }

    #[test]
    fn open_url_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::OpenUrl {
            url: "https://example.com".to_string(),
            direction: SplitDirection::Right,
            title: Some("docs".to_string()),
            workspace: Some("ws-2".to_string()),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"open-url","url":"https://example.com","direction":"right","title":"docs","workspace":"ws-2"}"#
        );
    }

    #[test]
    fn url_snapshot_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::UrlSnapshot {
            url: "https://example.com".to_string(),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"url-snapshot","url":"https://example.com"}"#
        );
    }

    #[test]
    fn url_links_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::UrlLinks {
            url: "https://example.com".to_string(),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"url-links","url":"https://example.com"}"#
        );
    }

    #[test]
    fn url_forms_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::UrlForms {
            url: "https://example.com".to_string(),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"url-forms","url":"https://example.com"}"#
        );
    }

    #[test]
    fn url_evaluate_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::UrlEvaluate {
            url: "https://example.com".to_string(),
            expression: "title".to_string(),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"url-evaluate","url":"https://example.com","expression":"title"}"#
        );
    }

    #[test]
    fn url_console_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::UrlConsole {
            url: "https://example.com".to_string(),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"url-console","url":"https://example.com"}"#
        );
    }

    #[test]
    fn url_network_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::UrlNetwork {
            url: "https://example.com".to_string(),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"url-network","url":"https://example.com"}"#
        );
    }

    #[test]
    fn open_url_link_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::OpenUrlLink {
            url: "https://example.com".to_string(),
            index: 2,
            direction: SplitDirection::Right,
            title: Some("link".to_string()),
            workspace: Some("ws-2".to_string()),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"open-url-link","url":"https://example.com","index":2,"direction":"right","title":"link","workspace":"ws-2"}"#
        );
    }

    #[test]
    fn submit_form_request_uses_socket_protocol_shape() {
        let mut fields = BTreeMap::new();
        fields.insert("q".to_string(), "vmux".to_string());
        let encoded = serde_json::to_string(&Request::SubmitForm {
            url: "https://example.com/search".to_string(),
            index: 1,
            fields,
            direction: SplitDirection::Right,
            title: Some("search".to_string()),
            workspace: Some("ws-2".to_string()),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"submit-form","url":"https://example.com/search","index":1,"fields":{"q":"vmux"},"direction":"right","title":"search","workspace":"ws-2"}"#
        );
    }

    #[test]
    fn custom_actions_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::CustomActions {
            workspace: Some("ws-2".to_string()),
        })
        .unwrap();
        assert_eq!(encoded, r#"{"action":"custom-actions","workspace":"ws-2"}"#);
    }

    #[test]
    fn run_custom_action_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::RunCustomAction {
            name: "test".to_string(),
            workspace: Some("ws-2".to_string()),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"run-custom-action","name":"test","workspace":"ws-2"}"#
        );
    }

    #[test]
    fn read_screen_request_defaults_to_scrollback() {
        let encoded = serde_json::to_string(&Request::ReadScreen {
            pane: Some("pane-1".to_string()),
            scrollback: true,
            limit_bytes: None,
        })
        .unwrap();
        assert_eq!(encoded, r#"{"action":"read-screen","pane":"pane-1"}"#);
    }

    #[test]
    fn read_screen_request_can_disable_scrollback_and_set_limit() {
        let encoded = serde_json::to_string(&Request::ReadScreen {
            pane: Some("pane-1".to_string()),
            scrollback: false,
            limit_bytes: Some(4096),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"read-screen","pane":"pane-1","scrollback":false,"limit_bytes":4096}"#
        );
    }

    #[test]
    fn copy_pane_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::CopyPane {
            pane: Some("pane-1".to_string()),
            scrollback: true,
            limit_bytes: Some(4096),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"copy-pane","pane":"pane-1","scrollback":true,"limit_bytes":4096}"#
        );
    }

    #[test]
    fn paste_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::Paste {
            pane: Some("pane-1".to_string()),
            enter: true,
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"paste","pane":"pane-1","enter":true}"#
        );
    }

    #[test]
    fn clipboard_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::Clipboard).unwrap();
        assert_eq!(encoded, r#"{"action":"clipboard"}"#);
    }

    #[test]
    fn wait_pane_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::WaitPane {
            pane: Some("pane-2".to_string()),
            workspace: None,
            all: false,
            timeout_ms: Some(5000),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"wait-pane","pane":"pane-2","timeout_ms":5000}"#
        );
    }

    #[test]
    fn wait_workspace_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::WaitPane {
            pane: None,
            workspace: Some("ws-2".to_string()),
            all: false,
            timeout_ms: Some(30000),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"wait-pane","pane":null,"workspace":"ws-2","timeout_ms":30000}"#
        );
    }

    #[test]
    fn move_pane_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::MovePane {
            pane: Some("pane-1".to_string()),
            workspace: "ws-2".to_string(),
            direction: SplitDirection::Right,
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"move-pane","pane":"pane-1","workspace":"ws-2","direction":"right"}"#
        );
    }

    #[test]
    fn restart_pane_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::RestartPane {
            pane: Some("pane-1".to_string()),
            workspace: None,
            all: false,
            command: Some("claude".to_string()),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"restart-pane","pane":"pane-1","command":"claude"}"#
        );
    }

    #[test]
    fn restart_workspace_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::RestartPane {
            pane: None,
            workspace: Some("ws-2".to_string()),
            all: false,
            command: None,
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"restart-pane","pane":null,"workspace":"ws-2"}"#
        );
    }

    #[test]
    fn prune_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::Prune {
            workspace: Some("ws-2".to_string()),
            all: false,
        })
        .unwrap();
        assert_eq!(encoded, r#"{"action":"prune","workspace":"ws-2"}"#);
    }

    #[test]
    fn broadcast_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::Broadcast {
            scope: BroadcastScope::Workspace,
            data: "npm test\n".to_string(),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"broadcast","scope":"workspace","data":"npm test\n"}"#
        );
    }

    #[test]
    fn send_key_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::SendKey {
            pane: Some("pane-1".to_string()),
            keys: vec!["C-c".to_string(), "enter".to_string()],
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"send-key","pane":"pane-1","keys":["C-c","enter"]}"#
        );
    }

    #[test]
    fn workspace_notify_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::Notify {
            pane: None,
            workspace: Some("ws-2".to_string()),
            status: Some("busy".to_string()),
            color: Some("yellow".to_string()),
            clear: false,
            message: "agents running".to_string(),
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"action":"notify","pane":null,"workspace":"ws-2","status":"busy","color":"yellow","clear":false,"message":"agents running"}"#
        );
    }

    #[test]
    fn notifications_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::Notifications { limit: 10 }).unwrap();
        assert_eq!(encoded, r#"{"action":"notifications","limit":10}"#);
    }

    #[test]
    fn clear_notifications_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::ClearNotifications).unwrap();
        assert_eq!(encoded, r#"{"action":"clear-notifications"}"#);
    }

    #[test]
    fn jump_notification_request_uses_socket_protocol_shape() {
        let encoded = serde_json::to_string(&Request::JumpNotification).unwrap();
        assert_eq!(encoded, r#"{"action":"jump-notification"}"#);
    }
}
