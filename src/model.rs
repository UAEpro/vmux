use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cli::SplitDirection;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    pub active_workspace: String,
    pub workspaces: Vec<Workspace>,
    pub panes: BTreeMap<String, Pane>,
    pub notifications: Vec<Notification>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<EventRecord>,
    /// Monotonic counter for [`EventRecord::id`] (persisted so IDs stay unique).
    #[serde(default)]
    pub next_event_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clipboard: Option<ClipboardItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon: Option<DaemonInfo>,
}

/// Where a pane lives in the Workspace → Tab → Pane hierarchy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneLocation {
    pub workspace_id: String,
    pub tab_id: Option<String>,
    pub pane_id: String,
}

impl Session {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            active_workspace: "ws-1".to_string(),
            workspaces: vec![Workspace::new("ws-1".to_string(), "main".to_string())],
            panes: BTreeMap::new(),
            notifications: Vec::new(),
            events: Vec::new(),
            next_event_id: 0,
            clipboard: None,
            daemon: None,
        }
    }

    /// Locate a pane across every tab of every workspace (not just active-tab live views).
    pub fn find_pane_location(&self, pane_id: &str) -> Option<PaneLocation> {
        for workspace in &self.workspaces {
            for tab in &workspace.tabs {
                if tab.panes.iter().any(|p| p == pane_id) {
                    return Some(PaneLocation {
                        workspace_id: workspace.id.clone(),
                        tab_id: Some(tab.id.clone()),
                        pane_id: pane_id.to_string(),
                    });
                }
            }
            if workspace.panes.iter().any(|p| p == pane_id) {
                return Some(PaneLocation {
                    workspace_id: workspace.id.clone(),
                    tab_id: workspace.active_tab.clone(),
                    pane_id: pane_id.to_string(),
                });
            }
        }
        None
    }

    /// Push a fresh default workspace if none exist. Guards against a
    /// corrupted or hand-edited state file that leaves `workspaces` empty,
    /// which would otherwise panic on indexed access.
    pub fn ensure_workspace(&mut self) {
        if self.workspaces.is_empty() {
            self.workspaces
                .push(Workspace::new("ws-1".to_string(), "main".to_string()));
            self.active_workspace = "ws-1".to_string();
        }
    }

    /// Migrate session state to the Workspace → Tab → Pane hierarchy.
    ///
    /// - Old workspaces (layout at workspace root, no `tabs`) get one default tab.
    /// - Workspaces that already have tabs hydrate live layout fields from the
    ///   active tab so existing code paths can keep using `workspace.panes` /
    ///   `layout` as a view of the active tab.
    /// - Per-pane `PaneTab` strips are collapsed onto the pane (active only).
    pub fn migrate_hierarchy(&mut self) {
        self.ensure_workspace();
        for workspace in &mut self.workspaces {
            workspace.migrate_hierarchy();
        }
        for pane in self.panes.values_mut() {
            collapse_pane_level_tabs(pane);
        }
    }

    /// Flush every workspace's live layout fields into its active tab record.
    /// Call before persisting so inactive tabs stay intact and the active tab
    /// matches what the UI/daemon last mutated.
    pub fn flush_tabs(&mut self) {
        for workspace in &mut self.workspaces {
            workspace.flush_active_tab();
        }
    }

    pub fn active_workspace_mut(&mut self) -> &mut Workspace {
        self.ensure_workspace();
        let active = self.active_workspace.clone();
        let index = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == active)
            .unwrap_or(0);
        self.active_workspace = self.workspaces[index].id.clone();
        &mut self.workspaces[index]
    }

    pub fn close_workspace(&mut self, workspace: Option<&str>) -> Result<Workspace, String> {
        if self.workspaces.len() <= 1 {
            return Err("cannot close the last workspace".to_string());
        }

        let target = workspace.unwrap_or(&self.active_workspace);
        let Some(index) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == target)
        else {
            return Err(format!("unknown workspace {target}"));
        };

        let removed = self.workspaces.remove(index);
        for pane in removed.all_pane_ids() {
            self.panes.remove(pane);
        }

        if self.active_workspace == removed.id
            || !self
                .workspaces
                .iter()
                .any(|workspace| workspace.id == self.active_workspace)
        {
            let next_index = index.saturating_sub(1).min(self.workspaces.len() - 1);
            self.active_workspace = self.workspaces[next_index].id.clone();
        }

        Ok(removed)
    }

    pub fn set_workspace_pinned(
        &mut self,
        workspace: &str,
        pinned: bool,
    ) -> Result<Workspace, String> {
        let Some(target) = self.workspaces.iter_mut().find(|item| item.id == workspace) else {
            return Err(format!("unknown workspace {workspace}"));
        };
        target.pinned = pinned;
        Ok(target.clone())
    }

    pub fn move_workspace(
        &mut self,
        workspace: &str,
        position: usize,
    ) -> Result<Workspace, String> {
        if position == 0 {
            return Err("workspace position is 1-based".to_string());
        }
        let Some(index) = self.workspaces.iter().position(|item| item.id == workspace) else {
            return Err(format!("unknown workspace {workspace}"));
        };
        let workspace = self.workspaces.remove(index);
        let insert_at = (position - 1).min(self.workspaces.len());
        self.workspaces.insert(insert_at, workspace.clone());
        Ok(workspace)
    }

    pub fn resolve_workspace_selector(&self, selector: &str) -> Result<String, String> {
        if self
            .workspaces
            .iter()
            .any(|workspace| workspace.id == selector)
        {
            return Ok(selector.to_string());
        }
        let matches = self
            .workspaces
            .iter()
            .filter(|workspace| workspace.name == selector)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [workspace] => Ok(workspace.id.clone()),
            [] => Err(format!("unknown workspace {selector}")),
            _ => Err(format!("workspace name {selector} is ambiguous")),
        }
    }

    pub fn move_pane(
        &mut self,
        pane: &str,
        target_workspace: &str,
        direction: SplitDirection,
    ) -> Result<Workspace, String> {
        if !self.panes.contains_key(pane) {
            return Err(format!("unknown pane {pane}"));
        }
        let Some(source_index) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.contains_pane(pane))
        else {
            return Err(format!("pane {pane} is not attached to a workspace"));
        };
        let Some(target_index) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == target_workspace)
        else {
            return Err(format!("unknown workspace {target_workspace}"));
        };
        if source_index == target_index {
            return Ok(self.workspaces[target_index].clone());
        }

        {
            let source = &mut self.workspaces[source_index];
            source.remove_pane_anywhere(pane);
        }

        {
            let target = &mut self.workspaces[target_index];
            target.ensure_layout();
            let active_before = target.active_pane.clone();
            target.panes.push(pane.to_string());
            target.layout = Some(insert_pane_in_layout(
                target.layout.take(),
                active_before.as_deref(),
                pane.to_string(),
                direction,
            ));
            target.active_pane = Some(pane.to_string());
            target.flush_active_tab();
            Ok(target.clone())
        }
    }

    pub fn swap_panes(&mut self, first: &str, second: &str) -> Result<Workspace, String> {
        if first == second {
            return Err("cannot swap a pane with itself".to_string());
        }
        if !self.panes.contains_key(first) {
            return Err(format!("unknown pane {first}"));
        }
        if !self.panes.contains_key(second) {
            return Err(format!("unknown pane {second}"));
        }
        // Prefer active tab that has both; else any tab that holds both.
        let Some(index) = self
            .workspaces
            .iter()
            .position(|workspace| {
                workspace.panes.iter().any(|pane| pane == first)
                    && workspace.panes.iter().any(|pane| pane == second)
            })
            .or_else(|| {
                self.workspaces.iter().position(|workspace| {
                    workspace.tabs.iter().any(|tab| {
                        tab.panes.iter().any(|p| p == first)
                            && tab.panes.iter().any(|p| p == second)
                    })
                })
            })
        else {
            return Err(format!(
                "panes {first} and {second} are not in the same workspace tab"
            ));
        };

        let workspace = &mut self.workspaces[index];
        // Sync the active tab record with the live view first, then swap purely
        // at the tab level. Swapping panes on a background tab must NOT change
        // which tab is active or what an attached user is viewing.
        workspace.flush_active_tab();
        let Some(tab_index) = workspace.tabs.iter().position(|tab| {
            tab.panes.iter().any(|p| p == first) && tab.panes.iter().any(|p| p == second)
        }) else {
            return Err(format!(
                "panes {first} and {second} are not in the same workspace tab"
            ));
        };
        {
            let tab = &mut workspace.tabs[tab_index];
            for pane in &mut tab.panes {
                if pane == first {
                    *pane = second.to_string();
                } else if pane == second {
                    *pane = first.to_string();
                }
            }
            if let Some(layout) = tab.layout.as_mut() {
                swap_panes_in_layout(layout, first, second);
            }
            if tab.active_pane.as_deref() == Some(first) {
                tab.active_pane = Some(second.to_string());
            } else if tab.active_pane.as_deref() == Some(second) {
                tab.active_pane = Some(first.to_string());
            }
            if tab.zoomed_pane.as_deref() == Some(first) {
                tab.zoomed_pane = Some(second.to_string());
            } else if tab.zoomed_pane.as_deref() == Some(second) {
                tab.zoomed_pane = Some(first.to_string());
            }
        }
        // If we swapped within the active tab, re-hydrate the live view from it.
        if workspace.active_tab.as_deref() == Some(workspace.tabs[tab_index].id.as_str()) {
            let tab = workspace.tabs[tab_index].clone();
            workspace.panes = tab.panes;
            workspace.active_pane = tab.active_pane;
            workspace.zoomed_pane = tab.zoomed_pane;
            workspace.layout = tab.layout;
        }
        Ok(workspace.clone())
    }

    pub fn prune_exited_panes(&mut self, workspace: Option<&str>) -> Result<Vec<Pane>, String> {
        if let Some(workspace) = workspace {
            if !self.workspaces.iter().any(|item| item.id == workspace) {
                return Err(format!("unknown workspace {workspace}"));
            }
        }

        let mut removed = Vec::new();
        let mut remove_ids = Vec::new();
        for workspace_item in &mut self.workspaces {
            if workspace
                .map(|target| target != workspace_item.id)
                .unwrap_or(false)
            {
                continue;
            }
            // Collect exited panes across every tab (not only the live view).
            let candidates: Vec<String> = workspace_item
                .all_pane_ids()
                .into_iter()
                .map(|s| s.to_string())
                .filter(|pane_id| {
                    self.panes
                        .get(pane_id)
                        .map(|pane| matches!(pane.status, PaneStatus::Exited))
                        .unwrap_or(false)
                })
                .collect();
            for pane_id in candidates {
                workspace_item.remove_pane_anywhere(&pane_id);
                remove_ids.push(pane_id);
            }
        }

        for pane_id in remove_ids {
            if let Some(pane) = self.panes.remove(&pane_id) {
                removed.push(pane);
            }
        }
        Ok(removed)
    }
}

/// Wire protocol major version for Ping / daemon identity.
///
/// Bump when request/response shapes change incompatibly. Clients and older
/// daemons still rely on `#[serde(default)]` for additive fields; this value
/// lets a caller detect "too new / too old" before relying on a feature.
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub pid: u32,
    pub socket_path: String,
    pub pid_path: String,
    pub log_path: String,
    pub state_path: String,
    pub started_at: u64,
    /// A newer published `vmux` version, if the background update check found one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_available: Option<String>,
    /// See [`PROTOCOL_VERSION`]. Absent on daemons older than this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClipboardItem {
    pub text: String,
    pub source_pane: Option<String>,
    pub source: String,
    pub copied_at: u64,
}

/// One tab inside a workspace: owns a layout of panes.
///
/// Hierarchy: Session → Workspace → **Tab** → Pane(s).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceTab {
    pub id: String,
    pub title: String,
    /// Set once the user renames this tab by hand; auto-titling then leaves it alone.
    #[serde(default)]
    pub title_locked: bool,
    #[serde(default)]
    pub panes: Vec<String>,
    #[serde(default)]
    pub active_pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zoomed_pane: Option<String>,
    #[serde(default)]
    pub layout: Option<LayoutNode>,
}

impl WorkspaceTab {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            title_locked: false,
            panes: Vec::new(),
            active_pane: None,
            zoomed_pane: None,
            layout: None,
        }
    }

    pub fn ensure_layout(&mut self) {
        self.panes.retain(|pane| !pane.is_empty());
        self.layout = normalize_layout(self.layout.take(), &self.panes);
        // Reset dangling focus the same way as zoomed_pane.
        if self
            .active_pane
            .as_ref()
            .map(|pane| !self.panes.iter().any(|item| item == pane))
            .unwrap_or(true)
        {
            self.active_pane = self.first_pane();
        }
        if self
            .zoomed_pane
            .as_ref()
            .map(|pane| !self.panes.iter().any(|item| item == pane))
            .unwrap_or(false)
        {
            self.zoomed_pane = None;
        }
    }

    pub fn first_pane(&self) -> Option<String> {
        self.layout
            .as_ref()
            .and_then(LayoutNode::first_pane)
            .or_else(|| {
                self.panes
                    .iter()
                    .find(|pane| !pane.is_empty())
                    .map(ToOwned::to_owned)
            })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    #[serde(default)]
    pub git_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<PullRequestInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<ListeningPort>,
    #[serde(default)]
    pub pinned: bool,
    /// Tabs that each own a pane layout. Prefer this as the source of truth;
    /// `panes` / `layout` / `active_pane` / `zoomed_pane` are a live view of the
    /// **active** tab so existing daemon/UI code keeps working during the
    /// hierarchy migration.
    #[serde(default)]
    pub tabs: Vec<WorkspaceTab>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tab: Option<String>,
    #[serde(default)]
    pub panes: Vec<String>,
    #[serde(default)]
    pub active_pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zoomed_pane: Option<String>,
    #[serde(default)]
    pub layout: Option<LayoutNode>,
    /// Monotonic tab-id counter. Persisted so a closed tab's id is never reused
    /// (which would silently retarget stale `tab-N` references).
    #[serde(default)]
    pub next_tab_seq: u64,
}

impl Workspace {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        let id = id.into();
        let name = name.into();
        let tab = WorkspaceTab::new("tab-1", "main");
        Self {
            id,
            name,
            cwd: default_cwd(),
            git_branch: None,
            pull_request: None,
            ports: Vec::new(),
            pinned: false,
            active_tab: Some(tab.id.clone()),
            tabs: vec![tab],
            panes: Vec::new(),
            active_pane: None,
            zoomed_pane: None,
            layout: None,
            next_tab_seq: 2,
        }
    }

    /// Ensure at least one tab exists and live layout fields match the active tab.
    pub fn migrate_hierarchy(&mut self) {
        if self.tabs.is_empty() {
            // Legacy state: layout lived on the workspace root.
            let tab = WorkspaceTab {
                id: "tab-1".to_string(),
                title: if self.name.is_empty() {
                    "main".to_string()
                } else {
                    self.name.clone()
                },
                title_locked: false,
                panes: std::mem::take(&mut self.panes),
                active_pane: self.active_pane.take(),
                zoomed_pane: self.zoomed_pane.take(),
                layout: self.layout.take(),
            };
            self.active_tab = Some(tab.id.clone());
            self.tabs.push(tab);
        }

        // Validate active_tab.
        if self
            .active_tab
            .as_ref()
            .map(|id| !self.tabs.iter().any(|tab| &tab.id == id))
            .unwrap_or(true)
        {
            self.active_tab = self.tabs.first().map(|tab| tab.id.clone());
        }

        // Hydrate live view from the active tab (tabs are authoritative when
        // both sides were present in JSON).
        if let Some(tab_id) = self.active_tab.clone() {
            // Would keeping the live fields flush a pane that another tab already
            // owns? That happens only with inconsistent on-disk state; treating
            // the live view as authoritative there duplicates the pane into two
            // tabs. In that case hydrate from the (authoritative) active tab.
            let live_dupes_other_tab = self.panes.iter().any(|pane| {
                self.tabs
                    .iter()
                    .any(|t| t.id != tab_id && t.panes.iter().any(|p| p == pane))
            });
            if let Some(tab) = self.tabs.iter().find(|tab| tab.id == tab_id) {
                // If live fields are empty but the tab has content, load tab.
                // If live fields have content and tab is empty (legacy dual-write
                // mid-migrate), prefer live fields and flush later — unless those
                // live panes already belong to another tab (stale duplicate).
                let tab_empty = tab.panes.is_empty() && tab.layout.is_none();
                let live_empty = self.panes.is_empty() && self.layout.is_none();
                if live_empty || !tab_empty || live_dupes_other_tab {
                    self.panes = tab.panes.clone();
                    self.active_pane = tab.active_pane.clone();
                    self.zoomed_pane = tab.zoomed_pane.clone();
                    self.layout = tab.layout.clone();
                }
            }
        }

        self.ensure_layout();
        self.flush_active_tab();
    }

    /// Copy live layout fields into the active tab record.
    pub fn flush_active_tab(&mut self) {
        if self.tabs.is_empty() {
            self.migrate_hierarchy();
            return;
        }
        let tab_id = self
            .active_tab
            .clone()
            .or_else(|| self.tabs.first().map(|tab| tab.id.clone()));
        let Some(tab_id) = tab_id else {
            return;
        };
        self.active_tab = Some(tab_id.clone());
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
            tab.panes = self.panes.clone();
            tab.active_pane = self.active_pane.clone();
            tab.zoomed_pane = self.zoomed_pane.clone();
            tab.layout = self.layout.clone();
            tab.ensure_layout();
        }
    }

    /// Switch the live layout view to another tab (flushes the current one first).
    pub fn switch_tab(&mut self, tab_id: &str) -> Result<(), String> {
        if !self.tabs.iter().any(|tab| tab.id == tab_id) {
            return Err(format!("unknown tab {tab_id}"));
        }
        self.flush_active_tab();
        let tab = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .cloned()
            .expect("tab checked above");
        self.panes = tab.panes;
        self.active_pane = tab.active_pane;
        self.zoomed_pane = tab.zoomed_pane;
        self.layout = tab.layout;
        self.active_tab = Some(tab.id);
        self.ensure_layout();
        Ok(())
    }

    pub fn contains_pane(&self, pane_id: &str) -> bool {
        if self.panes.iter().any(|pane| pane == pane_id) {
            return true;
        }
        self.tabs
            .iter()
            .any(|tab| tab.panes.iter().any(|pane| pane == pane_id))
    }

    pub fn all_pane_ids(&self) -> Vec<&str> {
        let mut ids = Vec::new();
        for tab in &self.tabs {
            for pane in &tab.panes {
                if !ids.contains(&pane.as_str()) {
                    ids.push(pane.as_str());
                }
            }
        }
        for pane in &self.panes {
            if !ids.contains(&pane.as_str()) {
                ids.push(pane.as_str());
            }
        }
        ids
    }

    /// Remove a pane from every tab (and the live view), then re-sync the active tab.
    pub fn remove_pane_anywhere(&mut self, pane: &str) {
        // Push live view into the active tab first so we don't re-hydrate stale tab data.
        self.flush_active_tab();
        for tab in &mut self.tabs {
            tab.panes.retain(|item| item != pane);
            tab.layout = remove_pane_from_layout(tab.layout.take(), pane);
            if tab.active_pane.as_deref() == Some(pane) {
                tab.active_pane = tab.panes.first().cloned();
            }
            if tab.zoomed_pane.as_deref() == Some(pane) {
                tab.zoomed_pane = None;
            }
        }
        // Re-hydrate live view from the updated active tab.
        if let Some(tab_id) = self.active_tab.clone() {
            if let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id).cloned() {
                self.panes = tab.panes;
                self.active_pane = tab.active_pane;
                self.zoomed_pane = tab.zoomed_pane;
                self.layout = tab.layout;
            }
        } else {
            self.panes.retain(|item| item != pane);
            self.layout = remove_pane_from_layout(self.layout.take(), pane);
            if self.active_pane.as_deref() == Some(pane) {
                self.active_pane = self.first_pane();
            }
            if self.zoomed_pane.as_deref() == Some(pane) {
                self.zoomed_pane = None;
            }
        }
        self.ensure_layout();
        self.flush_active_tab();
    }

    pub fn next_tab_id(&mut self) -> String {
        // Monotonic and never reused: seed the counter from the highest existing
        // id so old state files (no persisted counter) stay consistent, then
        // advance it. A closed tab's id is never handed out again.
        let max_existing = self
            .tabs
            .iter()
            .filter_map(|tab| tab.id.strip_prefix("tab-"))
            .filter_map(|rest| rest.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        let n = self.next_tab_seq.max(max_existing + 1);
        self.next_tab_seq = n + 1;
        format!("tab-{n}")
    }

    /// Add a new empty tab and make it active. Returns the new tab id.
    pub fn add_tab(&mut self, title: impl Into<String>) -> WorkspaceTab {
        self.flush_active_tab();
        let tab = WorkspaceTab::new(self.next_tab_id(), title);
        let id = tab.id.clone();
        self.tabs.push(tab.clone());
        self.panes.clear();
        self.active_pane = None;
        self.zoomed_pane = None;
        self.layout = None;
        self.active_tab = Some(id);
        self.flush_active_tab();
        tab
    }

    /// Close a tab. Returns pane ids that must be killed, and the remaining active tab.
    pub fn close_tab(&mut self, tab_id: &str) -> Result<(Vec<String>, WorkspaceTab), String> {
        if self.tabs.len() <= 1 {
            return Err("cannot close the last tab".to_string());
        }
        self.flush_active_tab();
        let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) else {
            return Err(format!("unknown tab {tab_id}"));
        };
        let closing_active = self.active_tab.as_deref() == Some(tab_id);
        let removed = self.tabs.remove(index);
        let pane_ids = removed.panes.clone();
        // Only move the live view when the user closed the tab they are on.
        // Closing a background tab must leave the active tab untouched.
        if closing_active {
            let next_index = index.saturating_sub(1).min(self.tabs.len() - 1);
            let next_id = self.tabs[next_index].id.clone();
            self.switch_tab(&next_id)?;
        }
        let active = self
            .tabs
            .iter()
            .find(|tab| Some(tab.id.as_str()) == self.active_tab.as_deref())
            .cloned()
            .unwrap_or_else(|| self.tabs[0].clone());
        Ok((pane_ids, active))
    }

    /// Rename a tab on the user's behalf. Pins the title so agent auto-titling
    /// cannot overwrite a name the user chose.
    pub fn rename_tab(&mut self, tab_id: &str, title: String) -> Result<WorkspaceTab, String> {
        self.flush_active_tab();
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
            return Err(format!("unknown tab {tab_id}"));
        };
        tab.title = title;
        tab.title_locked = true;
        Ok(tab.clone())
    }

    /// Rename a tab from an agent-derived title. Returns the updated tab, or
    /// `None` when the tab is missing, user-pinned, or already carries `title`.
    pub fn auto_rename_tab(&mut self, tab_id: &str, title: &str) -> Option<WorkspaceTab> {
        self.flush_active_tab();
        let tab = self.tabs.iter_mut().find(|tab| tab.id == tab_id)?;
        if tab.title_locked || tab.title == title {
            return None;
        }
        tab.title = title.to_string();
        Some(tab.clone())
    }

    pub fn ensure_layout(&mut self) {
        if self.tabs.is_empty() {
            // Keep one tab around even if caller only touched live fields.
            let tab = WorkspaceTab::new("tab-1", "main");
            self.active_tab = Some(tab.id.clone());
            self.tabs.push(tab);
        }
        self.panes.retain(|pane| !pane.is_empty());
        self.layout = normalize_layout(self.layout.take(), &self.panes);
        // Reset dangling focus the same way as zoomed_pane.
        if self
            .active_pane
            .as_ref()
            .map(|pane| !self.panes.iter().any(|item| item == pane))
            .unwrap_or(true)
        {
            self.active_pane = self.first_pane();
        }
        if self
            .zoomed_pane
            .as_ref()
            .map(|pane| !self.panes.iter().any(|item| item == pane))
            .unwrap_or(false)
        {
            self.zoomed_pane = None;
        }
        if self.cwd.is_empty() {
            self.cwd = default_cwd();
        }
        self.flush_active_tab();
    }

    pub fn first_pane(&self) -> Option<String> {
        self.layout
            .as_ref()
            .and_then(LayoutNode::first_pane)
            .or_else(|| {
                self.panes
                    .iter()
                    .find(|pane| !pane.is_empty())
                    .map(ToOwned::to_owned)
            })
    }
}

/// Collapse legacy per-pane tabs onto the pane itself (keep active tab's surface).
fn collapse_pane_level_tabs(pane: &mut Pane) {
    if pane.tabs.is_empty() {
        pane.active_tab = None;
        return;
    }
    let active_id = pane
        .active_tab
        .clone()
        .or_else(|| pane.tabs.first().map(|tab| tab.id.clone()));
    if let Some(active_id) = active_id {
        if let Some(tab) = pane.tabs.iter().find(|tab| tab.id == active_id) {
            pane.title = tab.title.clone();
            pane.command = tab.command.clone();
            pane.surface_kind = tab.surface_kind.clone();
            if let Some(status) = tab.status.clone() {
                pane.status = status;
            }
            if let Some(agent_status) = tab.agent_status.clone() {
                pane.agent_status = agent_status;
            }
            pane.progress = tab.progress;
            pane.notification_color = tab.notification_color.clone();
            pane.notification_message = tab.notification_message.clone();
            pane.exit_code = tab.exit_code;
            if !tab.output.is_empty() {
                pane.output = tab.output.clone();
            }
            if !tab.output_formatted.is_empty() {
                pane.output_formatted = tab.output_formatted.clone();
            }
            if !tab.scrollback.is_empty() {
                pane.scrollback = tab.scrollback.clone();
            }
            if !tab.scrollback_formatted.is_empty() {
                pane.scrollback_formatted = tab.scrollback_formatted.clone();
            }
        }
    }
    pane.tabs.clear();
    pane.active_tab = None;
}

/// Directory the user was in when they started the daemon / first attached.
///
/// The daemon process chdirs to `/` after daemonize (so it does not hold a
/// mount point open). Pane shells must still start in the user's launch
/// directory, which is passed as `VMUX_LAUNCH_CWD` from the client spawn.
pub fn launch_cwd() -> PathBuf {
    if let Ok(cwd) = std::env::var("VMUX_LAUNCH_CWD") {
        let path = PathBuf::from(cwd.trim());
        if !path.as_os_str().is_empty() && path.is_dir() {
            return path;
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

pub fn default_cwd() -> String {
    // Honors ui.default_cwd (launch | home) and VMUX_LAUNCH_CWD.
    crate::config::resolve_default_cwd_path()
        .display()
        .to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
/// No longer populated: vmux stopped querying GitHub for PR state (the
/// background `gh pr view` polling billed the user's API quota — the sidebar
/// is local-only now). The type and the `Workspace.pull_request` field stay
/// so old state files, old clients, and the phone protocol keep decoding.
pub struct PullRequestInfo {
    pub number: u64,
    pub state: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub draft: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListeningPort {
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pids: Vec<u32>,
    /// Process name when known (`ss` / `/proc`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
    /// Owning pane id when attribution succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum LayoutNode {
    Pane {
        pane: String,
    },
    Split {
        axis: SplitAxis,
        ratio: u16,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

impl LayoutNode {
    pub fn first_pane(&self) -> Option<String> {
        match self {
            LayoutNode::Pane { pane } => Some(pane.clone()),
            LayoutNode::Split { first, .. } => first.first_pane(),
        }
    }

    pub fn panes_in_order(&self, out: &mut Vec<String>) {
        match self {
            LayoutNode::Pane { pane } => out.push(pane.clone()),
            LayoutNode::Split { first, second, .. } => {
                first.panes_in_order(out);
                second.panes_in_order(out);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pane {
    pub id: String,
    #[serde(default = "default_surface_kind")]
    pub surface_kind: SurfaceKind,
    pub command: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<PaneTab>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tab: Option<String>,
    pub direction: SplitDirection,
    pub status: PaneStatus,
    pub agent_status: AgentStatus,
    /// When true, hook/CLI-set status sticks across PTY noise until a stronger
    /// signal (new Busy/Attention), a new user turn, or the user acknowledges
    /// Done (click workspace / tab / pane — see [`acknowledge_done_status`]).
    #[serde(default)]
    pub agent_status_pinned: bool,
    /// Unix time when `agent_status` last changed.
    #[serde(default)]
    pub agent_status_at: u64,
    #[serde(default)]
    pub progress: Option<u8>,
    #[serde(default)]
    pub notification_color: Option<String>,
    #[serde(default)]
    pub notification_message: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub output: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output_formatted: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mouse_protocol_mode: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mouse_protocol_encoding: String,
    /// The child enabled xterm alternate-scroll mode (DECSET 1007) while its
    /// alternate screen is active. Terminals translate wheel events into
    /// cursor-up/down input in this mode instead of scrolling host history.
    #[serde(default, skip_serializing_if = "is_false")]
    pub alternate_scroll_mode: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_row: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_col: Option<u16>,
    /// PTY grid size (from vt100), used to map cursor into a smaller pane view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_rows: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_cols: Option<u16>,
    /// Live phone-fit override: while set, the PTY is held at
    /// `min(layout size, view_size)` per axis because a small remote viewer is
    /// watching (see `Request::SetPaneViewSize`). Runtime-only — stripped from
    /// persistence and cleared on daemon load, so a restart always restores
    /// desktop sizes. The attach UI uses it to explain the shrunken grid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_size: Option<PaneViewSize>,
    #[serde(default)]
    pub scrollback: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scrollback_formatted: String,
    /// Line count of `scrollback`, present in lean snapshots so the client can
    /// clamp scrolling without receiving (or scanning) the scrollback text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrollback_lines: Option<usize>,
}

impl Pane {
    pub fn new(id: String, command: String, direction: SplitDirection) -> Self {
        let title = command
            .split_whitespace()
            .next()
            .unwrap_or("shell")
            .rsplit('/')
            .next()
            .unwrap_or("shell")
            .to_string();
        let now = unix_time();
        Self {
            id,
            surface_kind: SurfaceKind::Terminal,
            command: command.clone(),
            title,
            tabs: Vec::new(),
            active_tab: None,
            direction,
            status: PaneStatus::Starting,
            agent_status: infer_agent_status("", &command),
            agent_status_pinned: false,
            agent_status_at: now,
            progress: None,
            notification_color: None,
            notification_message: None,
            metadata: BTreeMap::new(),
            exit_code: None,
            pid: None,
            created_at: now,
            updated_at: now,
            output: String::new(),
            output_formatted: String::new(),
            mouse_protocol_mode: String::new(),
            mouse_protocol_encoding: String::new(),
            alternate_scroll_mode: false,
            cursor_row: None,
            cursor_col: None,
            screen_rows: None,
            screen_cols: None,
            view_size: None,
            scrollback: String::new(),
            scrollback_formatted: String::new(),
            scrollback_lines: None,
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Cols/rows a remote viewer asked a pane to fit (`Pane::view_size`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneViewSize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneTab {
    pub id: String,
    pub title: String,
    pub command: String,
    #[serde(default = "default_surface_kind")]
    pub surface_kind: SurfaceKind,
    #[serde(default)]
    pub status: Option<PaneStatus>,
    #[serde(default)]
    pub agent_status: Option<AgentStatus>,
    #[serde(default)]
    pub progress: Option<u8>,
    #[serde(default)]
    pub notification_color: Option<String>,
    #[serde(default)]
    pub notification_message: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub output: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output_formatted: String,
    #[serde(default)]
    pub scrollback: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scrollback_formatted: String,
    pub created_at: u64,
    pub updated_at: u64,
}

impl PaneTab {
    pub fn from_pane(id: String, pane: &Pane) -> Self {
        let now = unix_time();
        Self {
            id,
            title: pane.title.clone(),
            command: pane.command.clone(),
            surface_kind: pane.surface_kind.clone(),
            status: Some(pane.status.clone()),
            agent_status: Some(pane.agent_status.clone()),
            progress: pane.progress,
            notification_color: pane.notification_color.clone(),
            notification_message: pane.notification_message.clone(),
            exit_code: pane.exit_code,
            output: pane.output.clone(),
            output_formatted: pane.output_formatted.clone(),
            scrollback: pane.scrollback.clone(),
            scrollback_formatted: pane.scrollback_formatted.clone(),
            created_at: now,
            updated_at: now,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SurfaceKind {
    Terminal,
    Browser,
    Agent,
    Markdown,
}

fn default_surface_kind() -> SurfaceKind {
    SurfaceKind::Terminal
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PaneStatus {
    Starting,
    Running,
    Exited,
    Restored,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AgentStatus {
    Idle,
    Busy,
    Attention,
    Done,
    Error,
    /// Unknown / future variants deserialize here so upgrades keep loading state.
    /// Must stay last for `#[serde(other)]`.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub time: u64,
    pub pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub status: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub clear: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    /// Monotonic id (unique per session); used for CLI follow and relay dedupe.
    #[serde(default)]
    pub id: u64,
    pub time: u64,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

pub fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn infer_agent_status(output: &str, command: &str) -> AgentStatus {
    // Only scan the output chunk — do not mix in the command name, which can
    // create false matches and thrash status on every redraw.
    let haystack = output.to_lowercase();
    if !haystack.trim().is_empty() {
        if haystack.contains("traceback")
            || haystack.contains("exception")
            || haystack.contains("fatal error")
            || haystack.contains("command failed")
        {
            return AgentStatus::Error;
        }
        if haystack.contains("needs input")
            || haystack.contains("need input")
            || haystack.contains("waiting for input")
            || haystack.contains("waiting for approval")
            || haystack.contains("needs approval")
            || haystack.contains("approval required")
            || haystack.contains("permission required")
            || haystack.contains("allow this")
            || haystack.contains("do you want to")
            || haystack.contains("press enter to")
            || haystack.contains("awaiting user")
            || haystack.contains("user input")
            || haystack.contains("needs-input")
        {
            return AgentStatus::Attention;
        }
        // Agent-specific activity phrases only. Generic words like "executing",
        // "generating", "in progress", "streaming", and "working on" appear
        // constantly in ordinary build/log output and produced false 🔄. A
        // genuinely stuck heuristic Busy is also self-healed by decay_stale_busy.
        if haystack.contains("thinking")
            || haystack.contains("tool call")
            || haystack.contains("calling tool")
            || haystack.contains("running tool")
        {
            return AgentStatus::Busy;
        }
        if haystack.contains("all tasks complete")
            || haystack.contains("successfully completed")
            || haystack.contains("turn complete")
        {
            return AgentStatus::Done;
        }
    }
    if is_coding_agent_command(command) {
        // Agent process is up but quiet — idle (waiting for a prompt).
        AgentStatus::Idle
    } else {
        AgentStatus::Unknown
    }
}

/// Merge a freshly inferred status with the current one.
///
/// Hook/CLI updates set `pinned` so PTY redraw noise cannot demote Done/Busy
/// into Idle. Strong signals (new work, needs input) still update.
/// Clear a finished-turn ✅ once the user has seen it (click / focus).
///
/// Stays on the sidebar until the user selects the workspace, its tab, or the
/// pane — so if you were away, the checkmark is still there when you return.
/// Busy (🔄) and Attention (🙋) are not cleared here.
///
/// The resulting Idle/Unknown stays **pinned** so a late PreToolUse/PostToolUse
/// "agent working" hook (common with Grok after Stop) cannot re-raise 🔄 after
/// the user has already dismissed the checkmark. A real new turn still wins via
/// UserPromptSubmit (title), a custom busy message, or keystrokes.
pub fn acknowledge_done_status(pane: &mut Pane) -> bool {
    if !matches!(pane.agent_status, AgentStatus::Done) {
        return false;
    }
    pane.agent_status = if is_coding_agent_command(&pane.command) {
        AgentStatus::Idle
    } else {
        AgentStatus::Unknown
    };
    // Pinned settled-idle: ignore boilerplate busy until a strong new-turn signal.
    pane.agent_status_pinned = true;
    pane.agent_status_at = unix_time();
    pane.notification_message = None;
    pane.notification_color = None;
    true
}

/// Record that `agent_status` just changed.
pub fn touch_agent_status(pane: &mut Pane, status: AgentStatus, pinned: bool) {
    pane.agent_status = status;
    pane.agent_status_pinned = pinned;
    pane.agent_status_at = unix_time();
}

/// Self-heal a stale, *unpinned* Busy spinner. A heuristic (output-keyword)
/// Busy that has seen no activity for `timeout_secs` is demoted, so a false or
/// finished-but-unsignalled 🔄 clears itself instead of sticking forever.
/// Pinned Busy is an authoritative hook/CLI signal (e.g. an agent thinking
/// silently mid-turn) and is left untouched. `now`/`updated_at` are unix
/// seconds; `updated_at` reflects the pane's last output. Returns true if the
/// status changed.
pub fn decay_stale_busy(pane: &mut Pane, now: u64, timeout_secs: u64) -> bool {
    if !matches!(pane.agent_status, AgentStatus::Busy) || pane.agent_status_pinned {
        return false;
    }
    if now.saturating_sub(pane.updated_at) < timeout_secs {
        return false;
    }
    let demoted = if is_coding_agent_command(&pane.command) {
        AgentStatus::Idle
    } else {
        AgentStatus::Unknown
    };
    touch_agent_status(pane, demoted, false);
    true
}

pub fn merge_agent_status(
    current: AgentStatus,
    pinned: bool,
    inferred: AgentStatus,
) -> (AgentStatus, bool) {
    if !pinned {
        // Unpinned: keep Busy sticky while a coding agent is still redrawing
        // quiet frames (Idle), so the spinner does not flicker off mid-turn.
        return match (current, inferred) {
            (AgentStatus::Busy, AgentStatus::Idle | AgentStatus::Unknown) => {
                (AgentStatus::Busy, false)
            }
            (AgentStatus::Done, AgentStatus::Idle | AgentStatus::Unknown) => {
                (AgentStatus::Done, false)
            }
            (AgentStatus::Attention, AgentStatus::Idle | AgentStatus::Unknown) => {
                (AgentStatus::Attention, false)
            }
            // Keep ❌ sticky through idle redraws (shell prompt after traceback).
            (AgentStatus::Error, AgentStatus::Idle | AgentStatus::Unknown) => {
                (AgentStatus::Error, false)
            }
            (_, next) => (next, false),
        };
    }

    match (current, inferred) {
        // Finished turn holds against PTY keyword "busy" — late screen redraws
        // after Stop must not resurrect 🔄. Real new work arrives via hooks
        // (`notify` / UserPromptSubmit) or keystrokes (`mark_coding_agent_busy`).
        (AgentStatus::Done, AgentStatus::Busy) => (AgentStatus::Done, true),
        // Settled idle (user acknowledged ✅): same protection.
        (AgentStatus::Idle, AgentStatus::Busy) => (AgentStatus::Idle, true),
        (AgentStatus::Unknown, AgentStatus::Busy) => (AgentStatus::Unknown, true),
        // Error can recover into busy when the agent retries.
        (AgentStatus::Error, AgentStatus::Busy) => (AgentStatus::Busy, true),
        (
            AgentStatus::Done | AgentStatus::Idle | AgentStatus::Unknown | AgentStatus::Error,
            AgentStatus::Attention,
        ) => (AgentStatus::Attention, true),
        // Authoritative pin holds through idle/unknown redraws.
        (AgentStatus::Done, _) => (AgentStatus::Done, true),
        (AgentStatus::Error, _) => (AgentStatus::Error, true),
        (AgentStatus::Attention, AgentStatus::Busy) => (AgentStatus::Busy, true),
        (AgentStatus::Attention, AgentStatus::Done) => (AgentStatus::Done, true),
        (AgentStatus::Attention, _) => (AgentStatus::Attention, true),
        (AgentStatus::Busy, AgentStatus::Attention) => (AgentStatus::Attention, true),
        (AgentStatus::Busy, AgentStatus::Done) => (AgentStatus::Done, true),
        (AgentStatus::Busy, AgentStatus::Error) => (AgentStatus::Error, true),
        (AgentStatus::Busy, _) => (AgentStatus::Busy, true),
        // Settled idle/unknown pin holds through quiet redraws.
        (AgentStatus::Idle, _) => (AgentStatus::Idle, true),
        (AgentStatus::Unknown, _) => (AgentStatus::Unknown, true),
    }
}

/// True when the pane command looks like a coding agent CLI (Claude Code,
/// Codex, Grok Build, Aider, Cursor, Gemini, etc.).
///
/// Matches the **basename of the first token** only — avoids false positives
/// like `git rebase --continue` or `ssh-agent bash`.
pub fn is_coding_agent_command(command: &str) -> bool {
    let mut tokens = command.split_whitespace();
    let Some(first) = tokens.next() else {
        return false;
    };
    if is_agent_binary(&token_basename(first)) {
        return true;
    }
    // Agents installed through npm/pip run under an interpreter: the process is
    // `node /usr/lib/node_modules/.../bin/claude`, where the first token names
    // the interpreter and the script names the agent. Only the script argument
    // is considered, so `node build.js --agent claude` cannot match.
    if !is_agent_interpreter(&token_basename(first)) {
        return false;
    }
    let Some(script) = tokens.find(|token| !token.starts_with('-')) else {
        return false;
    };
    // The script itself may be the agent (`node .../bin/claude`), or an entry
    // point inside the agent's package (`node .../claude-code/cli.js`), where the
    // package directory is what carries the name.
    is_agent_binary(&token_basename(script)) || is_agent_binary(&script_parent_name(script))
}

/// Name of the directory holding a script, unquoted and lowercased.
fn script_parent_name(token: &str) -> String {
    let token = token.trim_matches(|c| c == '\'' || c == '"');
    std::path::Path::new(token)
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Basename of one command-line token, unquoted and lowercased.
fn token_basename(token: &str) -> String {
    let token = token.trim_matches(|c| c == '\'' || c == '"');
    std::path::Path::new(token)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(token)
        .to_ascii_lowercase()
}

/// True when a binary name is a coding agent CLI.
fn is_agent_binary(base: &str) -> bool {
    matches!(
        base,
        "claude"
            | "codex"
            | "grok"
            | "aider"
            | "cursor"
            | "gemini"
            | "openai"
            | "copilot"
            | "opencode"
            | "goose"
            | "devin"
            | "amp"
            | "claude-code"
            | "cursor-agent"
    ) || base.starts_with("claude")
        || base.starts_with("codex")
        || base.starts_with("grok")
}

/// Interpreters an agent CLI is commonly launched through.
fn is_agent_interpreter(base: &str) -> bool {
    matches!(
        base,
        "node" | "nodejs" | "bun" | "deno" | "npx" | "python" | "python3" | "uv" | "uvx"
    )
}

/// Longest auto-generated tab title, in characters.
pub const AUTO_TITLE_MAX_CHARS: usize = 20;

/// Words that carry no meaning in a two-word tab label.
const TITLE_STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "at", "be", "can", "could", "do", "does", "for", "from", "i", "in",
    "into", "is", "it", "its", "let", "make", "me", "my", "of", "on", "onto", "or", "our", "out",
    "please", "should", "some", "that", "the", "then", "this", "to", "up", "us", "we", "will",
    "with", "would", "you", "your",
];

/// Titles an agent-run pane may emit that describe the tool, not the task.
const TITLE_NOISE_WORDS: &[&str] = &[
    "agent",
    "aider",
    "amp",
    "assistant",
    "bash",
    "busy",
    "claude",
    "code",
    "codex",
    "complete",
    "completed",
    "copilot",
    "cursor",
    "devin",
    "done",
    "error",
    "failed",
    "gemini",
    "goose",
    "grok",
    "idle",
    "opencode",
    "ready",
    "running",
    "session",
    "sh",
    "shell",
    "terminal",
    "working",
    "zsh",
];

/// Lifecycle / set-status boilerplate that must never rename a tab.
/// Matched case-insensitively against the whole message (after trim).
const TITLE_STATUS_BOILERPLATE: &[&str] = &[
    "agent hook completed",
    "agent hook event",
    "agent hook failed",
    "agent needs input",
    "agent working",
    "busy",
    "complete",
    "completed",
    "done",
    "error",
    "failed",
    "needs input",
    "needs review",
    "running",
    "waiting for approval",
    "waiting for input",
    "working",
];

/// Condense a terminal title an agent set (OSC 0/2) into a one-or-two word tab
/// name: `"✳ Fixing the parser bug"` → `"fixing parser"`.
///
/// Returns `None` for titles that describe the shell rather than a task
/// (`user@host`, a bare path) or that hold nothing but the agent's own name —
/// the caller then leaves the tab title alone (or asks an LLM instead).
///
/// Used for **every** coding agent: Claude, Codex, Grok, Aider, Cursor, and
/// anything else that shows up in a pane — the source of the string does not
/// matter (OSC title, UserPromptSubmit prompt, or a meaningful status message).
pub fn condense_agent_title(raw: &str) -> Option<String> {
    let cleaned = raw.replace(|c: char| c.is_control(), " ");
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return None;
    }
    // Shell-style titles (`user@host:~/dir`, `~/code/vmux`) describe a location,
    // not a task. A real task title can still mention a path, so only reject
    // when the whole title looks like one.
    if cleaned.contains('@') || cleaned.starts_with('/') || cleaned.starts_with('~') {
        return None;
    }
    let words: Vec<String> = cleaned
        .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
        .map(|word| word.trim_matches(|c| c == '-' || c == '_'))
        .filter(|word| !word.is_empty())
        .map(|word| word.to_lowercase())
        .collect();
    // Percentages, step counters and spinner frames ("3/7") carry no meaning.
    let meaningful: Vec<String> = words
        .into_iter()
        .filter(|word| !word.chars().all(|c| c.is_numeric()))
        .filter(|word| !TITLE_NOISE_WORDS.contains(&word.as_str()))
        .collect();
    if meaningful.is_empty() {
        return None;
    }
    let mut picked: Vec<&str> = meaningful
        .iter()
        .map(String::as_str)
        .filter(|word| !TITLE_STOPWORDS.contains(word))
        .take(2)
        .collect();
    // A title of nothing but stopwords is not worth a rename.
    if picked.is_empty() {
        return None;
    }
    // Keep the label short enough to read in a tab strip.
    if picked.len() == 2
        && picked[0].chars().count() + picked[1].chars().count() + 1 > AUTO_TITLE_MAX_CHARS
    {
        picked.truncate(1);
    }
    let mut title = picked.join(" ");
    if title.chars().count() > AUTO_TITLE_MAX_CHARS {
        title = title.chars().take(AUTO_TITLE_MAX_CHARS).collect();
    }
    Some(title)
}

/// Turn a busy-status message into a tab title when it names real work.
///
/// Agent-agnostic: any tool that reports `vmux set-status busy --message "…"`
/// (or a Notify with status busy) can name its tab. Boilerplate like
/// `"agent working"` / `"done"` is rejected so hooks do not thrash titles.
pub fn title_from_status_message(message: &str) -> Option<String> {
    let message = message.trim();
    if message.is_empty() {
        return None;
    }
    let lower: String = message.chars().map(|c| c.to_ascii_lowercase()).collect();
    if is_boilerplate_status_message(&lower) {
        return None;
    }
    // Idle / "waiting for your input" notices from Claude-style Notification
    // hooks are not tasks.
    if lower.contains("waiting for your input") || lower.contains("waiting for input") {
        return None;
    }
    condense_agent_title(message)
}

fn is_boilerplate_status_message(lower: &str) -> bool {
    // Exact match only — "working on the parser" is a real task message and
    // must not be treated as the bare lifecycle word "working".
    TITLE_STATUS_BOILERPLATE.contains(&lower)
}

pub fn direction_axis(direction: SplitDirection) -> SplitAxis {
    match direction {
        SplitDirection::Left | SplitDirection::Right => SplitAxis::Horizontal,
        SplitDirection::Up | SplitDirection::Down => SplitAxis::Vertical,
    }
}

pub fn insert_pane_in_layout(
    layout: Option<LayoutNode>,
    active_pane: Option<&str>,
    new_pane: String,
    direction: SplitDirection,
) -> LayoutNode {
    let Some(layout) = layout else {
        return LayoutNode::Pane { pane: new_pane };
    };
    let axis = direction_axis(direction);
    let insert_before = matches!(direction, SplitDirection::Left | SplitDirection::Up);
    let (layout, inserted) =
        insert_near_active(layout, active_pane, new_pane.clone(), axis, insert_before);
    if inserted {
        layout
    } else {
        // The active pane wasn't found in the tree (e.g. it was stale or the
        // caller passed an id that isn't in this layout). Never drop the pane:
        // fall back to appending it at the root so it always lands in the tree.
        LayoutNode::Split {
            axis,
            ratio: 50,
            first: Box::new(layout),
            second: Box::new(LayoutNode::Pane { pane: new_pane }),
        }
    }
}

pub fn remove_pane_from_layout(layout: Option<LayoutNode>, pane_id: &str) -> Option<LayoutNode> {
    match layout? {
        LayoutNode::Pane { pane } => {
            if pane == pane_id {
                None
            } else {
                Some(LayoutNode::Pane { pane })
            }
        }
        LayoutNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let first = remove_pane_from_layout(Some(*first), pane_id);
            let second = remove_pane_from_layout(Some(*second), pane_id);
            match (first, second) {
                (Some(first), Some(second)) => Some(LayoutNode::Split {
                    axis,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            }
        }
    }
}

pub fn resize_layout(layout: &mut Option<LayoutNode>, axis: SplitAxis, delta: i16) -> bool {
    let Some(layout) = layout else {
        return false;
    };
    resize_node(layout, axis, delta)
}

pub fn next_pane_in_layout(
    layout: Option<&LayoutNode>,
    panes: &[String],
    active: Option<&str>,
    direction: SplitDirection,
) -> Option<String> {
    let mut ordered = Vec::new();
    if let Some(layout) = layout {
        layout.panes_in_order(&mut ordered);
    }
    if ordered.is_empty() {
        ordered = panes.to_vec();
    }
    ordered.retain(|pane| panes.iter().any(|known| known == pane));
    if ordered.is_empty() {
        return None;
    }
    let active_index = active
        .and_then(|active| ordered.iter().position(|pane| pane == active))
        .unwrap_or(0);
    let next = match direction {
        SplitDirection::Right | SplitDirection::Down => (active_index + 1) % ordered.len(),
        SplitDirection::Left | SplitDirection::Up => {
            (active_index + ordered.len() - 1) % ordered.len()
        }
    };
    ordered.get(next).cloned()
}

/// Adjacent pane in `direction` on the layout tree, **without wrap-around**.
/// Used for edge-aware move controls (only show move when a neighbor exists).
pub fn adjacent_pane_in_layout(
    layout: Option<&LayoutNode>,
    pane: &str,
    direction: SplitDirection,
) -> Option<String> {
    let layout = layout?;
    let want_axis = direction_axis(direction);
    let want_first = matches!(direction, SplitDirection::Left | SplitDirection::Up);
    find_adjacent(layout, pane, want_axis, want_first)
}

fn find_adjacent(
    node: &LayoutNode,
    pane: &str,
    want_axis: SplitAxis,
    want_first: bool,
) -> Option<String> {
    match node {
        LayoutNode::Pane { .. } => None,
        LayoutNode::Split {
            axis,
            first,
            second,
            ..
        } => {
            if layout_contains(Some(first), pane) {
                if *axis == want_axis && !want_first {
                    // pane is in first; looking toward second (Right/Down)
                    return second.first_pane().or_else(|| {
                        let mut panes = Vec::new();
                        second.panes_in_order(&mut panes);
                        panes.into_iter().next()
                    });
                }
                return find_adjacent(first, pane, want_axis, want_first);
            }
            if layout_contains(Some(second), pane) {
                if *axis == want_axis && want_first {
                    // pane is in second; looking toward first (Left/Up)
                    return first.first_pane().or_else(|| {
                        let mut panes = Vec::new();
                        first.panes_in_order(&mut panes);
                        panes.into_iter().next()
                    });
                }
                return find_adjacent(second, pane, want_axis, want_first);
            }
            None
        }
    }
}

/// Whether `pane` has a layout neighbor in `direction` (no wrap).
pub fn can_move_pane(layout: Option<&LayoutNode>, pane: &str, direction: SplitDirection) -> bool {
    adjacent_pane_in_layout(layout, pane, direction).is_some()
}

fn insert_near_active(
    node: LayoutNode,
    active_pane: Option<&str>,
    new_pane: String,
    axis: SplitAxis,
    insert_before: bool,
) -> (LayoutNode, bool) {
    match node {
        LayoutNode::Pane { pane } => {
            let should_insert = active_pane.map(|active| active == pane).unwrap_or(true);
            if !should_insert {
                return (LayoutNode::Pane { pane }, false);
            }
            let existing = LayoutNode::Pane { pane };
            let new = LayoutNode::Pane { pane: new_pane };
            let (first, second) = if insert_before {
                (new, existing)
            } else {
                (existing, new)
            };
            (
                LayoutNode::Split {
                    axis,
                    ratio: 50,
                    first: Box::new(first),
                    second: Box::new(second),
                },
                true,
            )
        }
        LayoutNode::Split {
            axis: current_axis,
            ratio,
            first,
            second,
        } => {
            let (new_first, inserted) =
                insert_near_active(*first, active_pane, new_pane.clone(), axis, insert_before);
            if inserted {
                return (
                    LayoutNode::Split {
                        axis: current_axis,
                        ratio,
                        first: Box::new(new_first),
                        second,
                    },
                    true,
                );
            }
            let (new_second, inserted) =
                insert_near_active(*second, active_pane, new_pane, axis, insert_before);
            (
                LayoutNode::Split {
                    axis: current_axis,
                    ratio,
                    first: Box::new(new_first),
                    second: Box::new(new_second),
                },
                inserted,
            )
        }
    }
}

fn resize_node(node: &mut LayoutNode, axis: SplitAxis, delta: i16) -> bool {
    match node {
        LayoutNode::Pane { .. } => false,
        LayoutNode::Split {
            axis: current_axis,
            ratio,
            first,
            second,
        } => {
            if *current_axis == axis {
                let next = (*ratio as i16 + delta).clamp(15, 85);
                *ratio = next as u16;
                true
            } else {
                resize_node(first, axis, delta) || resize_node(second, axis, delta)
            }
        }
    }
}

fn swap_panes_in_layout(node: &mut LayoutNode, first: &str, second: &str) {
    match node {
        LayoutNode::Pane { pane } => {
            if pane == first {
                *pane = second.to_string();
            } else if pane == second {
                *pane = first.to_string();
            }
        }
        LayoutNode::Split {
            first: left,
            second: right,
            ..
        } => {
            swap_panes_in_layout(left, first, second);
            swap_panes_in_layout(right, first, second);
        }
    }
}

fn normalize_layout(layout: Option<LayoutNode>, panes: &[String]) -> Option<LayoutNode> {
    let mut layout = layout.and_then(|node| prune_unknown_panes(node, panes));
    for pane in panes {
        if !layout_contains(layout.as_ref(), pane) {
            layout = Some(append_pane(layout, pane.clone()));
        }
    }
    layout
}

fn append_pane(layout: Option<LayoutNode>, pane: String) -> LayoutNode {
    match layout {
        None => LayoutNode::Pane { pane },
        Some(existing) => LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: 50,
            first: Box::new(existing),
            second: Box::new(LayoutNode::Pane { pane }),
        },
    }
}

fn prune_unknown_panes(node: LayoutNode, panes: &[String]) -> Option<LayoutNode> {
    match node {
        LayoutNode::Pane { pane } => panes
            .iter()
            .any(|known| known == &pane)
            .then_some(LayoutNode::Pane { pane }),
        LayoutNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let first = prune_unknown_panes(*first, panes);
            let second = prune_unknown_panes(*second, panes);
            match (first, second) {
                (Some(first), Some(second)) => Some(LayoutNode::Split {
                    axis,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            }
        }
    }
}

fn layout_contains(layout: Option<&LayoutNode>, pane: &str) -> bool {
    match layout {
        None => false,
        Some(LayoutNode::Pane { pane: current }) => current == pane,
        Some(LayoutNode::Split { first, second, .. }) => {
            layout_contains(Some(first), pane) || layout_contains(Some(second), pane)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn active_workspace_mut_recreates_missing_workspace() {
        let mut session = Session::new("test");
        session.workspaces.clear();
        let workspace = session.active_workspace_mut();
        assert_eq!(workspace.id, "ws-1");
        assert_eq!(session.workspaces.len(), 1);
        assert_eq!(session.active_workspace, "ws-1");
    }

    #[test]
    fn migrate_hierarchy_creates_default_tab_from_legacy_workspace_layout() {
        // Simulate pre-tabs state JSON: layout fields on the workspace root,
        // empty tabs array after serde default.
        let mut session = Session::new("test");
        let ws = &mut session.workspaces[0];
        ws.tabs.clear();
        ws.active_tab = None;
        ws.panes = vec!["pane-1".to_string(), "pane-2".to_string()];
        ws.active_pane = Some("pane-2".to_string());
        ws.layout = Some(LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: 50,
            first: Box::new(LayoutNode::Pane {
                pane: "pane-1".to_string(),
            }),
            second: Box::new(LayoutNode::Pane {
                pane: "pane-2".to_string(),
            }),
        });

        session.migrate_hierarchy();

        let ws = &session.workspaces[0];
        assert_eq!(ws.tabs.len(), 1);
        assert_eq!(ws.active_tab.as_deref(), Some("tab-1"));
        assert_eq!(ws.tabs[0].panes, vec!["pane-1", "pane-2"]);
        assert_eq!(ws.tabs[0].active_pane.as_deref(), Some("pane-2"));
        assert!(ws.tabs[0].layout.is_some());
        // Live view still usable by existing code paths.
        assert_eq!(ws.panes, vec!["pane-1", "pane-2"]);
        assert_eq!(ws.active_pane.as_deref(), Some("pane-2"));
    }

    #[test]
    fn find_pane_location_searches_all_tabs() {
        let mut session = Session::new("test");
        let mut ws = session.workspaces.remove(0);
        ws.tabs = vec![
            WorkspaceTab {
                id: "tab-1".into(),
                title: "main".into(),
                title_locked: false,
                panes: vec!["pane-1".into()],
                active_pane: Some("pane-1".into()),
                zoomed_pane: None,
                layout: Some(LayoutNode::Pane {
                    pane: "pane-1".into(),
                }),
            },
            WorkspaceTab {
                id: "tab-2".into(),
                title: "bg".into(),
                title_locked: false,
                panes: vec!["pane-2".into()],
                active_pane: Some("pane-2".into()),
                zoomed_pane: None,
                layout: Some(LayoutNode::Pane {
                    pane: "pane-2".into(),
                }),
            },
        ];
        ws.active_tab = Some("tab-1".into());
        ws.panes = vec!["pane-1".into()];
        ws.active_pane = Some("pane-1".into());
        session.workspaces.push(ws);
        let loc = session.find_pane_location("pane-2").expect("bg pane");
        assert_eq!(loc.workspace_id, "ws-1");
        assert_eq!(loc.tab_id.as_deref(), Some("tab-2"));
        assert_eq!(loc.pane_id, "pane-2");
    }

    #[test]
    fn migrate_hierarchy_collapses_legacy_pane_tabs_onto_pane() {
        let mut session = Session::new("test");
        let mut pane = Pane::new(
            "pane-1".to_string(),
            "sh".to_string(),
            SplitDirection::Right,
        );
        pane.title = "shell".to_string();
        pane.tabs = vec![
            PaneTab {
                id: "tab-1".to_string(),
                title: "old".to_string(),
                command: "sh".to_string(),
                surface_kind: SurfaceKind::Terminal,
                status: Some(PaneStatus::Running),
                agent_status: Some(AgentStatus::Idle),
                progress: None,
                notification_color: None,
                notification_message: None,
                exit_code: None,
                output: "a".to_string(),
                output_formatted: String::new(),
                scrollback: "hist-a".to_string(),
                scrollback_formatted: String::new(),
                created_at: 1,
                updated_at: 1,
            },
            PaneTab {
                id: "tab-2".to_string(),
                title: "tests".to_string(),
                command: "cargo test".to_string(),
                surface_kind: SurfaceKind::Terminal,
                status: Some(PaneStatus::Running),
                agent_status: Some(AgentStatus::Busy),
                progress: Some(50),
                notification_color: None,
                notification_message: None,
                exit_code: None,
                output: "test out".to_string(),
                output_formatted: String::new(),
                scrollback: "hist-tests".to_string(),
                scrollback_formatted: String::new(),
                created_at: 1,
                updated_at: 1,
            },
        ];
        pane.active_tab = Some("tab-2".to_string());
        session.panes.insert("pane-1".to_string(), pane);

        session.migrate_hierarchy();

        let pane = session.panes.get("pane-1").unwrap();
        assert!(pane.tabs.is_empty());
        assert!(pane.active_tab.is_none());
        assert_eq!(pane.title, "tests");
        assert_eq!(pane.command, "cargo test");
        assert_eq!(pane.scrollback, "hist-tests");
        assert_eq!(pane.progress, Some(50));
    }

    #[test]
    fn switch_tab_swaps_live_layout_and_preserves_previous() {
        let mut ws = Workspace::new("ws-1", "main");
        ws.panes = vec!["pane-1".to_string()];
        ws.active_pane = Some("pane-1".to_string());
        ws.layout = Some(LayoutNode::Pane {
            pane: "pane-1".to_string(),
        });
        ws.flush_active_tab();

        let mut tab2 = WorkspaceTab::new("tab-2", "tests");
        tab2.panes = vec!["pane-2".to_string()];
        tab2.active_pane = Some("pane-2".to_string());
        tab2.layout = Some(LayoutNode::Pane {
            pane: "pane-2".to_string(),
        });
        ws.tabs.push(tab2);

        ws.switch_tab("tab-2").unwrap();
        assert_eq!(ws.active_tab.as_deref(), Some("tab-2"));
        assert_eq!(ws.panes, vec!["pane-2"]);
        assert_eq!(ws.active_pane.as_deref(), Some("pane-2"));

        // First tab still has pane-1.
        let tab1 = ws.tabs.iter().find(|t| t.id == "tab-1").unwrap();
        assert_eq!(tab1.panes, vec!["pane-1"]);

        ws.switch_tab("tab-1").unwrap();
        assert_eq!(ws.panes, vec!["pane-1"]);
    }

    #[test]
    fn new_session_has_workspace_with_default_tab() {
        let session = Session::new("test");
        assert_eq!(session.workspaces.len(), 1);
        assert_eq!(session.workspaces[0].tabs.len(), 1);
        assert_eq!(session.workspaces[0].active_tab.as_deref(), Some("tab-1"));
    }

    #[test]
    fn default_cwd_prefers_vmux_launch_cwd_env() {
        let dir = std::env::temp_dir().join(format!(
            "vmux-launch-cwd-{}-{}",
            std::process::id(),
            unix_time()
        ));
        fs::create_dir_all(&dir).unwrap();
        let previous = std::env::var_os("VMUX_LAUNCH_CWD");
        std::env::set_var("VMUX_LAUNCH_CWD", &dir);
        assert_eq!(default_cwd(), dir.display().to_string());
        match previous {
            Some(value) => std::env::set_var("VMUX_LAUNCH_CWD", value),
            None => std::env::remove_var("VMUX_LAUNCH_CWD"),
        }
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn infer_agent_status_detects_attention() {
        assert_eq!(
            infer_agent_status("Claude is waiting for approval", "claude"),
            AgentStatus::Attention
        );
        assert_eq!(
            infer_agent_status("needs input before continuing", "codex"),
            AgentStatus::Attention
        );
        assert_eq!(
            infer_agent_status("Allow this tool call?", "grok"),
            AgentStatus::Attention
        );
    }

    #[test]
    fn merge_agent_status_keeps_pinned_done_across_idle_redraws() {
        let (status, pinned) = merge_agent_status(AgentStatus::Done, true, AgentStatus::Idle);
        assert_eq!(status, AgentStatus::Done);
        assert!(pinned);
    }

    #[test]
    fn merge_agent_status_sticky_busy_without_pin() {
        let (status, pinned) = merge_agent_status(AgentStatus::Busy, false, AgentStatus::Idle);
        assert_eq!(status, AgentStatus::Busy);
        assert!(!pinned);
    }

    #[test]
    fn merge_agent_status_pinned_done_holds_against_pty_busy() {
        // PTY keyword inference must not resurrect 🔄 after Stop. Hooks and
        // keystrokes set Busy through notify / mark_coding_agent_busy instead.
        let (status, pinned) = merge_agent_status(AgentStatus::Done, true, AgentStatus::Busy);
        assert_eq!(status, AgentStatus::Done);
        assert!(pinned);
        let (status, pinned) = merge_agent_status(AgentStatus::Idle, true, AgentStatus::Busy);
        assert_eq!(status, AgentStatus::Idle);
        assert!(pinned);
    }

    #[test]
    fn acknowledge_done_status_clears_checkmark() {
        let mut pane = Pane::new("p1".into(), "claude".into(), SplitDirection::Right);
        pane.agent_status = AgentStatus::Done;
        pane.agent_status_pinned = true;
        assert!(acknowledge_done_status(&mut pane));
        assert_eq!(pane.agent_status, AgentStatus::Idle);
        // Stays pinned so late "agent working" hooks cannot re-raise 🔄.
        assert!(pane.agent_status_pinned);
        assert!(!acknowledge_done_status(&mut pane));
    }

    #[test]
    fn decay_stale_busy_demotes_only_stale_unpinned() {
        let mk = |cmd: &str| Pane::new("p1".into(), cmd.into(), SplitDirection::Right);

        // Unpinned Busy, no output past the timeout → demote (coding agent → Idle).
        let mut agent = mk("claude");
        agent.agent_status = AgentStatus::Busy;
        agent.agent_status_pinned = false;
        agent.updated_at = 100;
        assert!(decay_stale_busy(&mut agent, 100 + 30, 20));
        assert_eq!(agent.agent_status, AgentStatus::Idle);
        assert!(!agent.agent_status_pinned);

        // Pinned Busy (hook / silent think) is authoritative → never decays.
        let mut pinned = mk("claude");
        pinned.agent_status = AgentStatus::Busy;
        pinned.agent_status_pinned = true;
        pinned.updated_at = 100;
        assert!(!decay_stale_busy(&mut pinned, 100 + 9999, 20));
        assert_eq!(pinned.agent_status, AgentStatus::Busy);

        // Recent output → not yet.
        let mut recent = mk("claude");
        recent.agent_status = AgentStatus::Busy;
        recent.agent_status_pinned = false;
        recent.updated_at = 100;
        assert!(!decay_stale_busy(&mut recent, 100 + 5, 20));
        assert_eq!(recent.agent_status, AgentStatus::Busy);

        // Non-agent command demotes to Unknown (no marker).
        let mut shell = mk("bash");
        shell.agent_status = AgentStatus::Busy;
        shell.agent_status_pinned = false;
        shell.updated_at = 0;
        assert!(decay_stale_busy(&mut shell, 100, 20));
        assert_eq!(shell.agent_status, AgentStatus::Unknown);
    }

    #[test]
    fn is_coding_agent_command_detects_common_clis() {
        assert!(is_coding_agent_command("claude"));
        assert!(is_coding_agent_command("codex --yolo"));
        assert!(is_coding_agent_command("/usr/bin/grok"));
        assert!(is_coding_agent_command("aider --model gpt"));
        assert!(!is_coding_agent_command("bash"));
        assert!(!is_coding_agent_command("cargo test"));
    }

    #[test]
    fn is_coding_agent_command_sees_through_interpreters() {
        // How an npm-installed Claude Code actually appears in the process tree.
        assert!(is_coding_agent_command(
            "node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js"
        ));
        assert!(is_coding_agent_command("node /home/me/.local/bin/claude"));
        assert!(is_coding_agent_command("python3 /usr/bin/aider"));
        // The interpreter alone, or an agent named only in a flag, is not enough.
        assert!(!is_coding_agent_command("node"));
        assert!(!is_coding_agent_command("node build.js --agent claude"));
        assert!(!is_coding_agent_command("npm run claude"));
    }

    #[test]
    fn condense_agent_title_keeps_one_or_two_meaningful_words() {
        assert_eq!(
            condense_agent_title("✳ Fixing the parser bug").as_deref(),
            Some("fixing parser")
        );
        assert_eq!(
            condense_agent_title("Claude Code — reviewing auth middleware").as_deref(),
            Some("reviewing auth")
        );
        // Long pairs collapse to a single word rather than overflow the tab strip.
        let title = condense_agent_title("investigating authentication regressions").unwrap();
        assert_eq!(title, "investigating");
        assert!(title.chars().count() <= AUTO_TITLE_MAX_CHARS);
    }

    #[test]
    fn condense_agent_title_rejects_shell_and_agent_only_titles() {
        // Shell prompt titles describe a location, not a task.
        assert_eq!(condense_agent_title("mayed@host:~/code/vmux"), None);
        assert_eq!(condense_agent_title("~/code/vmux"), None);
        // Nothing but the agent's own name, spinner frames, or stopwords.
        assert_eq!(condense_agent_title("claude"), None);
        assert_eq!(condense_agent_title("codex 3/7"), None);
        assert_eq!(condense_agent_title("grok"), None);
        assert_eq!(condense_agent_title("working"), None);
        assert_eq!(condense_agent_title("  "), None);
    }

    #[test]
    fn title_from_status_message_accepts_real_work_for_any_agent() {
        // Any agent: set-status busy --message "…"
        assert_eq!(
            title_from_status_message("fixing the parser bug").as_deref(),
            Some("fixing parser")
        );
        assert_eq!(
            title_from_status_message("auth middleware").as_deref(),
            Some("auth middleware")
        );
        // Hook / lifecycle boilerplate never renames tabs.
        assert_eq!(title_from_status_message("agent working"), None);
        assert_eq!(title_from_status_message("working"), None);
        assert_eq!(title_from_status_message("done"), None);
        assert_eq!(title_from_status_message("agent hook completed"), None);
        assert_eq!(
            title_from_status_message("Claude is waiting for your input"),
            None
        );
    }

    #[test]
    fn manual_rename_pins_the_title_against_auto_titling() {
        let mut ws = Workspace::new("ws-1", "work");
        let tab = ws.add_tab("main");
        // Agent titles apply while the tab is unpinned.
        assert_eq!(
            ws.auto_rename_tab(&tab.id, "fixing parser")
                .map(|tab| tab.title),
            Some("fixing parser".to_string())
        );
        // Re-applying the same title is not a rename.
        assert!(ws.auto_rename_tab(&tab.id, "fixing parser").is_none());
        // Once the user renames by hand, the agent stops overriding it.
        ws.rename_tab(&tab.id, "payments".to_string()).unwrap();
        assert!(ws.auto_rename_tab(&tab.id, "fixing parser").is_none());
        let renamed = ws.tabs.iter().find(|item| item.id == tab.id).unwrap();
        assert_eq!(renamed.title, "payments");
    }

    #[test]
    fn inserts_directional_splits_near_active_pane() {
        let layout = insert_pane_in_layout(None, None, "pane-1".to_string(), SplitDirection::Right);
        let layout = insert_pane_in_layout(
            Some(layout),
            Some("pane-1"),
            "pane-2".to_string(),
            SplitDirection::Right,
        );
        let layout = insert_pane_in_layout(
            Some(layout),
            Some("pane-2"),
            "pane-3".to_string(),
            SplitDirection::Down,
        );

        let LayoutNode::Split {
            axis,
            first,
            second,
            ..
        } = layout
        else {
            panic!("expected root split");
        };
        assert_eq!(axis, SplitAxis::Horizontal);
        assert!(matches!(*first, LayoutNode::Pane { ref pane } if pane == "pane-1"));
        assert!(matches!(
            *second,
            LayoutNode::Split {
                axis: SplitAxis::Vertical,
                ..
            }
        ));
    }

    #[test]
    fn insert_places_pane_even_when_active_missing_from_tree() {
        // A single-pane layout whose only pane is NOT the active pane the caller
        // names. insert_near_active would report `inserted == false`; the pane
        // must still be placed in the tree rather than silently dropped.
        let layout = LayoutNode::Pane {
            pane: "pane-1".to_string(),
        };
        let result = insert_pane_in_layout(
            Some(layout),
            Some("pane-missing"),
            "pane-2".to_string(),
            SplitDirection::Right,
        );

        let mut panes = Vec::new();
        result.panes_in_order(&mut panes);
        assert!(panes.contains(&"pane-1".to_string()));
        assert!(
            panes.contains(&"pane-2".to_string()),
            "inserted pane must be present in the layout tree"
        );
    }

    #[test]
    fn resize_clamps_ratio() {
        let mut layout = Some(LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: 50,
            first: Box::new(LayoutNode::Pane {
                pane: "pane-1".to_string(),
            }),
            second: Box::new(LayoutNode::Pane {
                pane: "pane-2".to_string(),
            }),
        });

        assert!(resize_layout(&mut layout, SplitAxis::Horizontal, 100));
        assert!(matches!(layout, Some(LayoutNode::Split { ratio: 85, .. })));
        assert!(resize_layout(&mut layout, SplitAxis::Horizontal, -100));
        assert!(matches!(layout, Some(LayoutNode::Split { ratio: 15, .. })));
    }

    #[test]
    fn removing_pane_collapses_parent_split() {
        let layout = Some(LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: 50,
            first: Box::new(LayoutNode::Pane {
                pane: "pane-1".to_string(),
            }),
            second: Box::new(LayoutNode::Pane {
                pane: "pane-2".to_string(),
            }),
        });

        let layout = remove_pane_from_layout(layout, "pane-2");
        assert!(matches!(
            layout,
            Some(LayoutNode::Pane { ref pane }) if pane == "pane-1"
        ));
    }

    #[test]
    fn next_pane_follows_layout_order_and_wraps() {
        let panes = vec![
            "pane-1".to_string(),
            "pane-2".to_string(),
            "pane-3".to_string(),
        ];
        let layout = LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: 50,
            first: Box::new(LayoutNode::Pane {
                pane: "pane-1".to_string(),
            }),
            second: Box::new(LayoutNode::Split {
                axis: SplitAxis::Vertical,
                ratio: 50,
                first: Box::new(LayoutNode::Pane {
                    pane: "pane-2".to_string(),
                }),
                second: Box::new(LayoutNode::Pane {
                    pane: "pane-3".to_string(),
                }),
            }),
        };

        assert_eq!(
            next_pane_in_layout(Some(&layout), &panes, Some("pane-1"), SplitDirection::Right)
                .as_deref(),
            Some("pane-2")
        );
        assert_eq!(
            next_pane_in_layout(Some(&layout), &panes, Some("pane-1"), SplitDirection::Left)
                .as_deref(),
            Some("pane-3")
        );
    }

    #[test]
    fn closing_active_workspace_removes_panes_and_selects_neighbor() {
        let mut session = Session::new("test");
        session.panes.insert(
            "pane-1".to_string(),
            Pane::new(
                "pane-1".to_string(),
                "sh".to_string(),
                SplitDirection::Right,
            ),
        );
        session.panes.insert(
            "pane-2".to_string(),
            Pane::new(
                "pane-2".to_string(),
                "sh".to_string(),
                SplitDirection::Right,
            ),
        );
        session.workspaces[0].panes.push("pane-1".to_string());
        session.workspaces.push(Workspace {
            next_tab_seq: 0,
            id: "ws-2".to_string(),
            name: "agents".to_string(),
            cwd: default_cwd(),
            git_branch: None,
            pull_request: None,
            ports: Vec::new(),
            pinned: false,
            tabs: vec![WorkspaceTab::new("tab-1", "main")],
            active_tab: Some("tab-1".to_string()),
            panes: vec!["pane-2".to_string()],
            active_pane: Some("pane-2".to_string()),
            zoomed_pane: None,
            layout: Some(LayoutNode::Pane {
                pane: "pane-2".to_string(),
            }),
        });
        session.active_workspace = "ws-2".to_string();

        let closed = session.close_workspace(None).unwrap();

        assert_eq!(closed.id, "ws-2");
        assert_eq!(session.active_workspace, "ws-1");
        assert!(!session.panes.contains_key("pane-2"));
        assert!(session.panes.contains_key("pane-1"));
    }

    #[test]
    fn closing_last_workspace_is_rejected() {
        let mut session = Session::new("test");
        let err = session.close_workspace(None).unwrap_err();

        assert_eq!(err, "cannot close the last workspace");
        assert_eq!(session.workspaces.len(), 1);
    }

    #[test]
    fn setting_workspace_pinned_updates_workspace() {
        let mut session = Session::new("test");
        let workspace = session.set_workspace_pinned("ws-1", true).unwrap();

        assert!(workspace.pinned);
        assert!(session.workspaces[0].pinned);
    }

    #[test]
    fn moving_workspace_uses_one_based_position() {
        let mut session = Session::new("test");
        session.workspaces.push(Workspace {
            next_tab_seq: 0,
            id: "ws-2".to_string(),
            name: "agents".to_string(),
            cwd: default_cwd(),
            git_branch: None,
            pull_request: None,
            ports: Vec::new(),
            pinned: false,
            tabs: vec![WorkspaceTab::new("tab-1", "main")],
            active_tab: Some("tab-1".to_string()),
            panes: Vec::new(),
            active_pane: None,
            zoomed_pane: None,
            layout: None,
        });
        session.workspaces.push(Workspace {
            next_tab_seq: 0,
            id: "ws-3".to_string(),
            name: "tests".to_string(),
            cwd: default_cwd(),
            git_branch: None,
            pull_request: None,
            ports: Vec::new(),
            pinned: false,
            tabs: vec![WorkspaceTab::new("tab-1", "main")],
            active_tab: Some("tab-1".to_string()),
            panes: Vec::new(),
            active_pane: None,
            zoomed_pane: None,
            layout: None,
        });

        let moved = session.move_workspace("ws-3", 1).unwrap();

        assert_eq!(moved.id, "ws-3");
        assert_eq!(
            session
                .workspaces
                .iter()
                .map(|workspace| workspace.id.as_str())
                .collect::<Vec<_>>(),
            vec!["ws-3", "ws-1", "ws-2"]
        );
    }

    #[test]
    fn resolves_workspace_selector_by_id_or_unique_name() {
        let mut session = Session::new("test");
        session.workspaces.push(Workspace {
            next_tab_seq: 0,
            id: "ws-2".to_string(),
            name: "agents".to_string(),
            cwd: default_cwd(),
            git_branch: None,
            pull_request: None,
            ports: Vec::new(),
            pinned: false,
            tabs: vec![WorkspaceTab::new("tab-1", "main")],
            active_tab: Some("tab-1".to_string()),
            panes: Vec::new(),
            active_pane: None,
            zoomed_pane: None,
            layout: None,
        });

        assert_eq!(session.resolve_workspace_selector("ws-2").unwrap(), "ws-2");
        assert_eq!(
            session.resolve_workspace_selector("agents").unwrap(),
            "ws-2"
        );
        assert!(session.resolve_workspace_selector("missing").is_err());

        session.workspaces.push(Workspace {
            next_tab_seq: 0,
            id: "ws-3".to_string(),
            name: "agents".to_string(),
            cwd: default_cwd(),
            git_branch: None,
            pull_request: None,
            ports: Vec::new(),
            pinned: false,
            tabs: vec![WorkspaceTab::new("tab-1", "main")],
            active_tab: Some("tab-1".to_string()),
            panes: Vec::new(),
            active_pane: None,
            zoomed_pane: None,
            layout: None,
        });
        assert!(session.resolve_workspace_selector("agents").is_err());
    }

    #[test]
    fn moving_pane_updates_source_and_target_workspaces() {
        let mut session = Session::new("test");
        session.panes.insert(
            "pane-1".to_string(),
            Pane::new(
                "pane-1".to_string(),
                "sh".to_string(),
                SplitDirection::Right,
            ),
        );
        session.workspaces[0].panes.push("pane-1".to_string());
        session.workspaces[0].active_pane = Some("pane-1".to_string());
        session.workspaces[0].layout = Some(LayoutNode::Pane {
            pane: "pane-1".to_string(),
        });
        session.workspaces.push(Workspace {
            next_tab_seq: 0,
            id: "ws-2".to_string(),
            name: "agents".to_string(),
            cwd: default_cwd(),
            git_branch: None,
            pull_request: None,
            ports: Vec::new(),
            pinned: false,
            tabs: vec![WorkspaceTab::new("tab-1", "main")],
            active_tab: Some("tab-1".to_string()),
            panes: Vec::new(),
            active_pane: None,
            zoomed_pane: None,
            layout: None,
        });

        let target = session
            .move_pane("pane-1", "ws-2", SplitDirection::Right)
            .unwrap();

        assert_eq!(target.id, "ws-2");
        assert_eq!(target.active_pane.as_deref(), Some("pane-1"));
        assert!(session.workspaces[0].panes.is_empty());
        assert!(session.workspaces[0].layout.is_none());
        assert_eq!(session.workspaces[1].panes, vec!["pane-1"]);
        assert!(matches!(
            session.workspaces[1].layout,
            Some(LayoutNode::Pane { ref pane }) if pane == "pane-1"
        ));
    }

    #[test]
    fn swapping_panes_updates_workspace_layout_and_focus_state() {
        let mut session = Session::new("test");
        session.panes.insert(
            "pane-1".to_string(),
            Pane::new(
                "pane-1".to_string(),
                "left".to_string(),
                SplitDirection::Right,
            ),
        );
        session.panes.insert(
            "pane-2".to_string(),
            Pane::new(
                "pane-2".to_string(),
                "right".to_string(),
                SplitDirection::Right,
            ),
        );
        session.workspaces[0].panes = vec!["pane-1".to_string(), "pane-2".to_string()];
        session.workspaces[0].active_pane = Some("pane-1".to_string());
        session.workspaces[0].zoomed_pane = Some("pane-2".to_string());
        session.workspaces[0].layout = Some(LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: 50,
            first: Box::new(LayoutNode::Pane {
                pane: "pane-1".to_string(),
            }),
            second: Box::new(LayoutNode::Pane {
                pane: "pane-2".to_string(),
            }),
        });

        let workspace = session.swap_panes("pane-1", "pane-2").unwrap();

        assert_eq!(workspace.panes, vec!["pane-2", "pane-1"]);
        assert_eq!(workspace.active_pane.as_deref(), Some("pane-2"));
        assert_eq!(workspace.zoomed_pane.as_deref(), Some("pane-1"));
        assert!(matches!(
            workspace.layout,
            Some(LayoutNode::Split { first, second, .. })
                if matches!(*first, LayoutNode::Pane { ref pane } if pane == "pane-2")
                    && matches!(*second, LayoutNode::Pane { ref pane } if pane == "pane-1")
        ));
    }

    #[test]
    fn ensure_layout_clears_missing_zoomed_pane() {
        let mut session = Session::new("test");
        session.workspaces[0].panes = vec!["pane-1".to_string()];
        session.workspaces[0].active_pane = Some("pane-1".to_string());
        session.workspaces[0].zoomed_pane = Some("pane-2".to_string());
        session.workspaces[0].layout = Some(LayoutNode::Pane {
            pane: "pane-1".to_string(),
        });

        session.workspaces[0].ensure_layout();

        assert_eq!(session.workspaces[0].zoomed_pane, None);
    }

    #[test]
    fn pruning_exited_panes_keeps_running_panes_and_collapses_layout() {
        let mut session = Session::new("test");
        let mut running = Pane::new(
            "pane-1".to_string(),
            "sh".to_string(),
            SplitDirection::Right,
        );
        running.status = PaneStatus::Running;
        let mut exited = Pane::new(
            "pane-2".to_string(),
            "done".to_string(),
            SplitDirection::Right,
        );
        exited.status = PaneStatus::Exited;
        session.panes.insert("pane-1".to_string(), running);
        session.panes.insert("pane-2".to_string(), exited);
        session.workspaces[0].panes = vec!["pane-1".to_string(), "pane-2".to_string()];
        session.workspaces[0].active_pane = Some("pane-2".to_string());
        session.workspaces[0].layout = Some(LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: 50,
            first: Box::new(LayoutNode::Pane {
                pane: "pane-1".to_string(),
            }),
            second: Box::new(LayoutNode::Pane {
                pane: "pane-2".to_string(),
            }),
        });

        let removed = session.prune_exited_panes(Some("ws-1")).unwrap();

        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].id, "pane-2");
        assert!(session.panes.contains_key("pane-1"));
        assert!(!session.panes.contains_key("pane-2"));
        assert_eq!(session.workspaces[0].panes, vec!["pane-1"]);
        assert_eq!(session.workspaces[0].active_pane.as_deref(), Some("pane-1"));
        assert!(matches!(
            session.workspaces[0].layout,
            Some(LayoutNode::Pane { ref pane }) if pane == "pane-1"
        ));
    }
}
