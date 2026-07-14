mod agent_hooks;
mod cli;
mod config;
mod daemon;
mod input;
mod model;
mod paths;
mod protocol;
mod relay;
mod sync;
mod ui;
mod update;

use anyhow::{anyhow, Result};
use clap::Parser;
use serde::Serialize;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{IsTerminal, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command as ProcessCommand, Stdio};

#[cfg(test)]
use cli::SurfaceKindArg;
use cli::{
    ActionCommand, AgentCommand, BrowserCommand, Cli, Command, ConfigCommand, HookShell,
    HooksCommand, MarkdownCommand, MetadataCommand, PaneTabCommand, ProgressCommand, RelayCommand,
    RelayDevicesCommand, RemoteCommand, SkillsCommand, SurfaceCommand, TabCommand,
    WorkspaceCommand,
};
use model::SurfaceKind;

fn main() -> Result<()> {
    let cli = Cli::parse();
    let session = cli.session.as_str();

    match cli.command.unwrap_or(Command::Attach) {
        Command::Attach => {
            daemon::ensure_running(session)?;
            ui::attach(session)
        }
        Command::Daemon { foreground } => {
            if foreground {
                daemon::serve_foreground(session)
            } else {
                daemon::start_detached_or_daemonize(session)
            }
        }
        Command::NewPane {
            direction,
            command,
            title,
            workspace,
        }
        | Command::Split {
            direction,
            command,
            title,
            workspace,
        } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::NewPane {
                    direction,
                    command,
                    title,
                    workspace,
                    surface_kind: None,
                },
            )?;
            print_response(response)
        }
        Command::Run {
            direction,
            command,
            title,
            workspace,
            timeout,
        } => run_pane(session, direction, command, title, workspace, timeout),
        Command::OpenUrl {
            url,
            direction,
            title,
            workspace,
        } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::OpenUrl {
                    url,
                    direction,
                    title,
                    workspace,
                },
            )?;
            print_response(response)
        }
        Command::UrlSnapshot { url } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::UrlSnapshot { url },
            )?;
            print_response(response)
        }
        Command::UrlLinks { url } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::UrlLinks { url },
            )?;
            print_response(response)
        }
        Command::Browser { command } => browser_command(session, command),
        Command::Agent { command } => agent_command(session, command),
        Command::Remote { command } => remote_command(session, command),
        Command::Relay { command } => relay_command(session, command),
        Command::Markdown { command } => markdown_command(session, command),
        Command::Actions { command } => {
            daemon::ensure_running(session)?;
            let request = match command {
                ActionCommand::List { workspace } => protocol::Request::CustomActions { workspace },
                ActionCommand::Run { name, workspace } => {
                    protocol::Request::RunCustomAction { name, workspace }
                }
            };
            let response = protocol::request(&paths::socket_path(session)?, &request)?;
            print_response(response)
        }
        Command::Skills { command } => skills_command(command),
        Command::Config { command } => config_command(command),
        Command::Send { pane, enter, text } => {
            daemon::ensure_running(session)?;
            let mut data = text.join(" ");
            if enter {
                data.push('\r');
            }
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::Input { pane, data },
            )?;
            print_response(response)
        }
        Command::SendKey { pane, keys } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::SendKey { pane, keys },
            )?;
            print_response(response)
        }
        Command::SendImage { file, pane, enter } => {
            daemon::ensure_running(session)?;
            let bytes = read_image_input(&file)?;
            let ext = image_extension(&bytes).ok_or_else(|| {
                anyhow!("input does not look like an image (expected png, jpeg, gif, webp, or bmp)")
            })?;
            let path = save_send_image(&bytes, ext)?;
            let mut data = format!("{} ", path.display());
            if enter {
                data.push('\r');
            }
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::Input { pane, data },
            )?;
            if !response.ok {
                return Err(anyhow!(response
                    .error
                    .unwrap_or_else(|| "vmux command failed".to_string())));
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "ok": true,
                    "path": path.display().to_string(),
                    "bytes": bytes.len(),
                }))?
            );
            Ok(())
        }
        Command::Broadcast { scope, enter, text } => {
            daemon::ensure_running(session)?;
            let mut data = text.join(" ");
            if enter {
                data.push('\r');
            }
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::Broadcast { scope, data },
            )?;
            print_response(response)
        }
        Command::ReadScreen {
            pane,
            no_scrollback,
            limit_bytes,
        } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::ReadScreen {
                    pane,
                    scrollback: !no_scrollback,
                    limit_bytes: non_default_limit(limit_bytes),
                    ansi: false,
                    history_lines: 0,
                },
            )?;
            print_response(response)
        }
        Command::Search { pane, query } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::Search { pane, query },
            )?;
            print_response(response)
        }
        Command::ClearPane { pane } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::ClearPane { pane },
            )?;
            print_response(response)
        }
        Command::CopyPane {
            pane,
            scrollback,
            limit_bytes,
        } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::CopyPane {
                    pane,
                    scrollback,
                    limit_bytes: non_default_limit(limit_bytes),
                },
            )?;
            print_response(response)
        }
        Command::Paste { pane, enter } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::Paste { pane, enter },
            )?;
            print_response(response)
        }
        Command::Clipboard => {
            daemon::ensure_running(session)?;
            let response =
                protocol::request(&paths::socket_path(session)?, &protocol::Request::Clipboard)?;
            print_response(response)
        }
        Command::KillPane { pane } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::KillPane { pane },
            )?;
            print_response(response)
        }
        Command::DuplicatePane { pane, direction } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::DuplicatePane { pane, direction },
            )?;
            print_response(response)
        }
        Command::Prune { workspace, all } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::Prune { workspace, all },
            )?;
            print_response(response)
        }
        Command::RestartPane {
            pane,
            workspace,
            all,
            command,
        } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::RestartPane {
                    pane,
                    workspace,
                    all,
                    command,
                },
            )?;
            print_response(response)
        }
        Command::MovePane {
            pane,
            workspace,
            new_workspace,
            direction,
        } => {
            daemon::ensure_running(session)?;
            let socket = paths::socket_path(session)?;
            let workspace = move_pane_target_workspace(&socket, workspace, new_workspace)?;
            let response = protocol::request(
                &socket,
                &protocol::Request::MovePane {
                    pane,
                    workspace,
                    direction,
                },
            )?;
            print_response(response)
        }
        Command::SwapPanes { first, second } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::SwapPanes { first, second },
            )?;
            print_response(response)
        }
        Command::Title { pane, title } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::SetPaneTitle { pane, title },
            )?;
            print_response(response)
        }
        Command::Tab { command } => tab_command(session, command),
        Command::Move { direction, pane } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::MovePaneInLayout { pane, direction },
            )?;
            print_response(response)
        }
        Command::PaneTab { command } => pane_tab_command(session, command),
        Command::Metadata { command } => metadata_command(session, command),
        Command::Wait {
            pane,
            workspace,
            all,
            timeout,
        } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::WaitPane {
                    pane,
                    workspace,
                    all,
                    timeout_ms: wait_timeout_ms(timeout),
                },
            )?;
            print_response(response)
        }
        Command::Resize { direction, amount } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::Resize { direction, amount },
            )?;
            print_response(response)
        }
        Command::ViewSize {
            pane,
            cols,
            rows,
            lease_ms,
            clear,
        } => {
            daemon::ensure_running(session)?;
            let pane = pane
                .or_else(|| std::env::var("VMUX_PANE_ID").ok())
                .or_else(|| std::env::var("VMUX_SURFACE_ID").ok())
                .ok_or_else(|| anyhow!("--pane required (not running inside a vmux pane)"))?;
            let request = if clear {
                protocol::Request::ClearPaneViewSize { pane }
            } else {
                let (Some(cols), Some(rows)) = (cols, rows) else {
                    anyhow::bail!("pass --cols and --rows, or --clear");
                };
                protocol::Request::SetPaneViewSize {
                    pane,
                    cols,
                    rows,
                    lease_ms,
                }
            };
            let response = protocol::request(&paths::socket_path(session)?, &request)?;
            print_response(response)
        }
        Command::Focus { direction } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::FocusDirection { direction },
            )?;
            print_response(response)
        }
        Command::FocusPane { pane } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::FocusPane { pane },
            )?;
            print_response(response)
        }
        Command::Zoom { pane } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::ToggleZoom { pane },
            )?;
            print_response(response)
        }
        Command::Workspace { command } => {
            daemon::ensure_running(session)?;
            let socket = paths::socket_path(session)?;
            let request = match command {
                WorkspaceCommand::New {
                    name,
                    cwd,
                    command,
                    title,
                    direction,
                } => {
                    if !command.trim().is_empty() {
                        return create_workspace_with_pane(
                            &socket, name, cwd, command, title, direction, None,
                        );
                    }
                    protocol::Request::NewWorkspace { name, cwd }
                }
                WorkspaceCommand::Switch { workspace } => {
                    protocol::Request::SwitchWorkspace { workspace }
                }
                WorkspaceCommand::Next => {
                    let workspace = relative_workspace_from_socket(&socket, 1)?;
                    protocol::Request::SwitchWorkspace { workspace }
                }
                WorkspaceCommand::Previous => {
                    let workspace = relative_workspace_from_socket(&socket, -1)?;
                    protocol::Request::SwitchWorkspace { workspace }
                }
                WorkspaceCommand::Rename { workspace, name } => {
                    protocol::Request::RenameWorkspace { workspace, name }
                }
                WorkspaceCommand::Close { workspace } => {
                    protocol::Request::CloseWorkspace { workspace }
                }
                WorkspaceCommand::Cwd { workspace, cwd } => {
                    protocol::Request::SetWorkspaceCwd { workspace, cwd }
                }
                WorkspaceCommand::Pin { workspace } => protocol::Request::SetWorkspacePinned {
                    workspace,
                    pinned: true,
                },
                WorkspaceCommand::Unpin { workspace } => protocol::Request::SetWorkspacePinned {
                    workspace,
                    pinned: false,
                },
                WorkspaceCommand::Move {
                    workspace,
                    position,
                } => protocol::Request::MoveWorkspace {
                    workspace,
                    position,
                },
                WorkspaceCommand::List => protocol::Request::List,
            };
            let response = protocol::request(&socket, &request)?;
            print_response(response)
        }
        Command::Surface { command } => surface_command(session, command),
        Command::Progress { command } => {
            daemon::ensure_running(session)?;
            let (pane, value) = match command {
                ProgressCommand::Set { value, pane } => (pane, Some(value.min(100))),
                ProgressCommand::Clear { pane } => (pane, None),
            };
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::Progress { pane, value },
            )?;
            print_response(response)
        }
        Command::Hooks { command } => hooks_command(session, command),
        Command::SetProgress { value, pane } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::Progress {
                    pane,
                    value: Some(value.min(100)),
                },
            )?;
            print_response(response)
        }
        Command::SetStatus {
            status,
            pane,
            workspace,
            color,
            message,
        } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::Notify {
                    pane,
                    workspace,
                    status: Some(status),
                    color,
                    clear: false,
                    message,
                },
            )?;
            print_response(response)
        }
        Command::Notify {
            pane,
            workspace,
            status,
            color,
            clear,
            message,
        } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::Notify {
                    pane,
                    workspace,
                    status,
                    color,
                    clear,
                    message,
                },
            )?;
            print_response(response)
        }
        Command::Notifications { limit } => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::Notifications { limit },
            )?;
            print_response(response)
        }
        Command::Events {
            limit,
            follow,
            interval_ms,
        } => {
            daemon::ensure_running(session)?;
            if follow {
                return follow_events(session, limit, interval_ms);
            }
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::Events { limit },
            )?;
            print_response(response)
        }
        Command::ClearNotifications => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::ClearNotifications,
            )?;
            print_response(response)
        }
        Command::JumpNotification => {
            daemon::ensure_running(session)?;
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::JumpNotification,
            )?;
            print_response(response)
        }
        Command::Identify { pane, json: _ } => {
            daemon::ensure_running(session)?;
            let pane = pane
                .or_else(|| std::env::var("VMUX_PANE_ID").ok())
                .or_else(|| std::env::var("VMUX_SURFACE_ID").ok());
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::Identify { pane },
            )?;
            print_response(response)
        }
        Command::List => {
            daemon::ensure_running(session)?;
            let response =
                protocol::request(&paths::socket_path(session)?, &protocol::Request::List)?;
            print_response(response)
        }
        Command::Agents => {
            daemon::ensure_running(session)?;
            let response =
                protocol::request(&paths::socket_path(session)?, &protocol::Request::Agents)?;
            print_response(response)
        }
        Command::Status => {
            daemon::ensure_running(session)?;
            let response =
                protocol::request(&paths::socket_path(session)?, &protocol::snapshot_full())?;
            // Flatten {generation,session} for human-friendly status output.
            if response.ok {
                if let Some(data) = response.data {
                    let session = protocol::session_data_from_snapshot(data);
                    return print_response(protocol::Response::ok(session));
                }
            }
            print_response(response)
        }
        Command::Sessions => {
            let sessions = paths::list_sessions()?;
            println!("{}", serde_json::to_string_pretty(&sessions)?);
            Ok(())
        }
        Command::Logs { lines } => print_logs(session, lines),
        Command::Doctor => doctor(session),
        Command::Smoke { keep } => smoke(keep),
        Command::Stop => stop_session(session),
    }
}

fn relay_command(session: &str, command: RelayCommand) -> Result<()> {
    match command {
        RelayCommand::Serve {
            config,
            listen,
            allow_localhost,
        } => relay::serve(
            session,
            config.map(std::path::PathBuf::from),
            listen,
            allow_localhost,
        ),
        RelayCommand::Status { config } => relay::status(config.map(std::path::PathBuf::from)),
        RelayCommand::Devices { command } => match command {
            RelayDevicesCommand::List => relay::devices_list(),
            RelayDevicesCommand::Revoke { device_id } => relay::devices_revoke(&device_id),
        },
    }
}

fn hooks_command(session: &str, command: HooksCommand) -> Result<()> {
    match command {
        HooksCommand::Shell { shell } => {
            println!("{}", shell_hooks(shell));
            Ok(())
        }
        HooksCommand::Setup { shell, dir, rc } => setup_hooks(shell, dir, rc),
        HooksCommand::Status => hooks_status_command(),
        HooksCommand::Install { agent } => hooks_install_command(agent.as_deref()),
        HooksCommand::Event {
            event,
            pane,
            workspace,
            status,
            color,
            message,
        } => {
            // A hook event only updates an already-running session. When invoked
            // outside vmux (e.g. Claude Code in a plain terminal) there is no
            // daemon — do NOT start one, just no-op, or every hook fire would
            // spawn a stray persistent daemon.
            if !daemon::is_running(session) {
                return Ok(());
            }
            let mut payload = String::new();
            if !std::io::stdin().is_terminal() {
                std::io::stdin().read_to_string(&mut payload)?;
            }
            let request =
                hook_event_request(pane, workspace, event, status, color, message, &payload)?;
            let response = protocol::request(&paths::socket_path(session)?, &request)?;
            print_response(response)
        }
    }
}

fn hooks_status_command() -> Result<()> {
    let statuses = agent_hooks::status_report();
    let items: Vec<serde_json::Value> = statuses
        .iter()
        .map(|s| {
            serde_json::json!({
                "agent": s.kind.id(),
                "label": s.kind.label(),
                "state": s.state.label(),
                "path": s.path.display().to_string(),
                "detail": s.detail,
            })
        })
        .collect();
    let all_ready = statuses
        .iter()
        .all(|s| matches!(s.state, agent_hooks::InstallState::Installed));
    print_response(protocol::Response::ok(serde_json::json!({
        "ready": all_ready,
        "integrations": items,
        "install": "vmux hooks install",
    })))
}

fn hooks_install_command(agent: Option<&str>) -> Result<()> {
    let home = agent_hooks::home_dir();
    let results = if let Some(name) = agent {
        let kind = agent_hooks::IntegrationKind::parse(name)?;
        vec![agent_hooks::install_one(kind, &home)?]
    } else {
        agent_hooks::install_all(&home)?
    };
    let items: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            serde_json::json!({
                "agent": r.kind.id(),
                "label": r.kind.label(),
                "path": r.path.display().to_string(),
                "changed": r.changed,
                "detail": r.detail,
            })
        })
        .collect();
    let status_after = agent_hooks::status_report_in(&home);
    print_response(protocol::Response::ok(serde_json::json!({
        "installed": items,
        "status": status_after.iter().map(|s| serde_json::json!({
            "agent": s.kind.id(),
            "state": s.state.label(),
            "path": s.path.display().to_string(),
        })).collect::<Vec<_>>(),
    })))
}

/// Shell hook script body (shared with agent_hooks install).
pub(crate) fn shell_hooks_bash() -> &'static str {
    BASH_HOOKS
}

/// vmux-control skill markdown (shared with agent_hooks install for Grok).
pub(crate) fn vmux_control_skill_markdown() -> &'static str {
    VMUX_CONTROL_SKILL
}

fn hook_event_request(
    pane: Option<String>,
    workspace: Option<String>,
    event: Option<String>,
    status: Option<String>,
    color: Option<String>,
    message: Option<String>,
    payload: &str,
) -> Result<protocol::Request> {
    let payload = hook_payload(payload);
    let event = non_empty(event)
        .or_else(|| {
            payload.as_ref().and_then(|value| {
                // Prefer Codex/Claude lifecycle names. Do NOT use "type" (often
                // "command") or generic "name" — those produce wrong sidebar emoji.
                hook_payload_string(
                    value,
                    &["hook_event_name", "hookEventName", "event", "hook_event"],
                )
            })
        })
        .ok_or_else(|| anyhow!("hook event requires --event or an event field in stdin JSON"))?;
    let (default_status, default_color, default_message) = hook_event_defaults(&event);
    let message = non_empty(message)
        .or_else(|| {
            payload.as_ref().and_then(|value| {
                hook_payload_string(value, &["message", "summary", "reason", "error"])
            })
        })
        .unwrap_or_else(|| default_message.to_string());
    // Blank `--pane ""` (legacy empty LMUX_PANE_ID) must not become "unknown pane ".
    Ok(protocol::Request::Notify {
        pane: non_empty(pane),
        workspace: non_empty(workspace),
        status: non_empty(status).or_else(|| Some(default_status.to_string())),
        color: non_empty(color).or_else(|| Some(default_color.to_string())),
        clear: false,
        message,
    })
}

fn hook_payload(payload: &str) -> Option<serde_json::Value> {
    let payload = payload.trim();
    if payload.is_empty() {
        return None;
    }
    serde_json::from_str(payload).ok()
}

fn hook_payload_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = value.get(*key).and_then(|value| value.as_str()) {
            return non_empty(Some(value.to_string()));
        }
    }
    None
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    })
}

fn hook_event_defaults(event: &str) -> (&'static str, &'static str, &'static str) {
    // Normalize separators so UserPromptSubmit / user_prompt_submit match.
    let key: String = event
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();

    // Exact lifecycle names first (Codex + Claude). Substring matching alone
    // mis-classifies events (e.g. SubagentStop → done while still working).
    match key.as_str() {
        // Finished turn. Note: SubagentStop is NOT here — it fires when a
        // Task-tool subagent finishes while the parent agent is still working,
        // so it must read as "busy" (below), not "done".
        "stop" | "sessionend" | "taskcompleted" => ("done", "green", "agent hook completed"),
        // Hard failure
        "stopfailure" | "posttoolusefailure" => ("error", "red", "agent hook failed"),
        // Needs user (approval / notification)
        "permissionrequest" | "notification" | "elicitation" | "elicitationresult"
        | "permissiondenied" => ("attention", "blue", "agent needs input"),
        // Actively working
        "userpromptsubmit"
        | "userpromptexpansion"
        | "pretooluse"
        | "posttooluse"
        | "posttoolbatch"
        | "sessionstart"
        | "subagentstart"
        | "subagentstop"
        | "precompact"
        | "postcompact"
        | "setup"
        | "taskcreated" => ("busy", "yellow", "agent working"),
        _ => {
            // Fuzzy fallback for custom / unknown hook names.
            if key.contains("fail") || key.contains("error") {
                ("error", "red", "agent hook failed")
            } else if key == "stop"
                || key.ends_with("stop") && !key.contains("pre")
                || key.contains("sessionend")
                || key.contains("finish")
                || key.contains("complete") && !key.contains("incomplete")
            {
                ("done", "green", "agent hook completed")
            } else if key.contains("permission")
                || key.contains("notification")
                || key.contains("approval")
                || key.contains("needsinput")
                || key.contains("attention")
                || key.contains("blocked")
                || key.contains("elicitation")
            {
                ("attention", "blue", "agent needs input")
            } else {
                ("busy", "yellow", "agent hook event")
            }
        }
    }
}

fn setup_hooks(shell: HookShell, dir: Option<String>, rc: Option<String>) -> Result<()> {
    let dir = dir
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_hooks_dir);
    fs::create_dir_all(&dir)?;
    let path = dir.join(hooks_filename(shell));
    fs::write(&path, shell_hooks(shell))?;
    let source_line = hook_source_line(&path);
    if let Some(rc) = rc {
        append_hook_source_once(&std::path::PathBuf::from(rc), &source_line)?;
    }
    print_response(protocol::Response::ok(serde_json::json!({
        "shell": shell,
        "path": path.display().to_string(),
        "source": source_line,
    })))
}

fn default_hooks_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".config")
        })
        .join("vmux")
}

fn hooks_filename(shell: HookShell) -> &'static str {
    match shell {
        HookShell::Bash => "hooks.sh",
    }
}

fn hook_source_line(path: &std::path::Path) -> String {
    format!(". {}", shell_words::quote(&path.display().to_string()))
}

fn append_hook_source_once(path: &std::path::Path, source_line: &str) -> Result<()> {
    // Delegate to the hardened installer: it refuses to rewrite a non-UTF-8 rc
    // (from_utf8_lossy used to silently mangle latin-1 files) and takes a
    // byte-for-byte backup before mutating.
    crate::agent_hooks::append_source_once(path, source_line)
}

fn skills_command(command: SkillsCommand) -> Result<()> {
    match command {
        SkillsCommand::List => print_response(protocol::Response::ok(serde_json::json!({
            "skills": builtin_skill_summaries(),
        }))),
        SkillsCommand::Show { name } => {
            let skill = builtin_skill(&name)?;
            println!("{}", skill.markdown);
            Ok(())
        }
        SkillsCommand::Install { name, dir } => {
            let skill = builtin_skill(&name)?;
            let dir = dir
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from(".vmux").join("skills"));
            fs::create_dir_all(&dir)?;
            let path = dir.join(format!("{}.md", skill.name));
            fs::write(&path, skill.markdown)?;
            print_response(protocol::Response::ok(serde_json::json!({
                "skill": skill.name,
                "path": path.display().to_string(),
            })))
        }
    }
}

fn config_command(command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Show => {
            let path = paths::config_path()?;
            let config = config::load()?;
            print_response(protocol::Response::ok(serde_json::json!({
                "path": path.display().to_string(),
                "config": config,
            })))
        }
        ConfigCommand::Init { force } => {
            let path = paths::config_path()?;
            config::write_default(&path, force)?;
            // Also install coding-agent sidebar hooks so Claude/Codex/Grok light up.
            let home = agent_hooks::home_dir();
            let hook_results = agent_hooks::install_all(&home).unwrap_or_default();
            print_response(protocol::Response::ok(serde_json::json!({
                "path": path.display().to_string(),
                "created": true,
                "agent_hooks": hook_results.iter().map(|r| serde_json::json!({
                    "agent": r.kind.id(),
                    "path": r.path.display().to_string(),
                    "changed": r.changed,
                })).collect::<Vec<_>>(),
            })))
        }
        ConfigCommand::Set { key, value } => {
            let (path, mut config) = config::load_for_mutation()?;
            config::set_value(&mut config, &key, &value)?;
            config::save_to_path(&path, &config)?;
            print_response(protocol::Response::ok(serde_json::json!({
                "path": path.display().to_string(),
                "config": config,
            })))
        }
    }
}

#[derive(Clone, Copy)]
struct BuiltinSkill {
    name: &'static str,
    description: &'static str,
    markdown: &'static str,
}

fn builtin_skills() -> Vec<BuiltinSkill> {
    vec![BuiltinSkill {
        name: "vmux-control",
        description: "Control vmux sessions, panes, agents, browser surfaces, and notifications from terminal agents.",
        markdown: VMUX_CONTROL_SKILL,
    }]
}

fn builtin_skill_summaries() -> Vec<serde_json::Value> {
    builtin_skills()
        .into_iter()
        .map(|skill| {
            serde_json::json!({
                "name": skill.name,
                "description": skill.description,
            })
        })
        .collect()
}

fn builtin_skill(name: &str) -> Result<BuiltinSkill> {
    builtin_skills()
        .into_iter()
        .find(|skill| skill.name == name)
        .ok_or_else(|| anyhow!("unknown vmux skill {name}"))
}

fn shell_hooks(shell: HookShell) -> &'static str {
    match shell {
        HookShell::Bash => BASH_HOOKS,
    }
}

const VMUX_CONTROL_SKILL: &str = r#"# vmux-control

Use this skill when an agent is running inside a vmux pane and needs to inspect or control the terminal workspace.

## Discover Context

- Run `vmux identify --json` to get the current session, workspace, pane, socket, log, and state paths.
- Use `vmux list` for the full session snapshot.
- Use `vmux sessions` to find detached or persisted sessions after reconnecting to SSH.
- Use `vmux agents` to find agent panes and their status.
- Use `vmux logs --lines 100` to inspect the daemon log when a socket command fails.

## Control Panes

- Create panes with `vmux new-pane --direction right --command "claude"`.
- Hierarchy is **Workspace → Tab → Pane**. Create a workspace tab with `vmux tab new --title tests --command "cargo test"` and switch with `vmux tab switch tab-2`.
- Split panes with `vmux new-pane --direction right` / `down`.
- Send text with `vmux send --pane PANE --enter "message"`.
- Send keys with `vmux send-key --pane PANE C-c enter`.
- Read output with `vmux read-screen --pane PANE --limit-bytes 64000`.
- Search output with `vmux search --pane PANE "needle"`.

## Report Agent State (sidebar emoji)

vmux shows workspace sidebar markers from pane agent status:

| Status | Sidebar | When to report |
|--------|---------|----------------|
| busy / running | 🔄 | Task is in progress |
| attention / needs-input | 🙋 | Waiting for user approval/input |
| done | ✅ | Task finished successfully |
| error | ❌ | Task failed |

- Mark work with `vmux set-status busy --message "working"` (shows 🔄).
- Request input with `vmux set-status attention --message "needs review"` (shows 🙋).
- Finish with `vmux set-status done --message "complete"` and `vmux set-progress 100` (shows ✅).
- Fail with `vmux set-status error --message "failed"` (shows ❌).
- Send notifications with `vmux notify --message "needs input"` (also 🙋).
- Prefer shell helpers: `eval "$(vmux hooks shell)"` then `vmux_hook_busy`, `vmux_hook_attention`, `vmux_hook_done`.
- Wire Claude Code / Codex / Grok Stop hooks to `vmux hooks event` with JSON on stdin.
- Attach custom pane context with `vmux metadata set task auth-api --pane PANE`.
- Watch agent activity with `vmux events --limit 50` or `vmux events --follow`.

## Browser And Docs

- Open terminal browser panes with `vmux browser open URL`.
- Inspect pages with `vmux browser snapshot URL`, `vmux browser links URL`, and `vmux browser forms URL`.
- Evaluate static page data with `vmux browser evaluate URL title`, `vmux browser evaluate URL links[1].href`, or `vmux browser evaluate URL text:h1`.
- Inspect terminal-native browser diagnostics with `vmux browser console URL` and `vmux browser network URL`.
- Activate links with `vmux browser click URL --index N`.
- Fill/submit forms with `vmux browser fill URL --index N --field name=value`.
- Open markdown with `vmux markdown open README.md`.

## Multi-Agent Teams

- Create a team with `vmux agent team --agents codex,claude --cwd "$PWD"`.
- Hand off work with `vmux agent send --agent PANE --enter "from: codex; task: ..."` .
- Keep messages short and include sender, target, task, and status.

## Detached Sessions

- The vmux daemon keeps running after SSH disconnects or terminal exits.
- Reattach with `vmux attach --session SESSION` after finding the session with `vmux sessions`.
- Run `vmux smoke` to verify local daemon, pane, tab, event, metadata, and restore behavior.
"#;

const BASH_HOOKS: &str = r#"# vmux shell hooks for bash/zsh-compatible shells.
#
# Sidebar emoji (auto from set-status / notify):
#   🔄 busy / running     vmux_hook_busy "working"
#   🙋 needs input        vmux_hook_attention "approve?"
#   ✅ finished           vmux_hook_done "complete"
#   ❌ failed             vmux_hook_error "failed"
#
# Usage:
#   eval "$(vmux hooks shell)"
#   # or: vmux hooks setup --dir ~/.config/vmux --rc ~/.bashrc
#   vmux_hook_run "tests" cargo test
#   vmux_hook_attention "waiting for approval"
#
# Coding agents (Claude Code / Codex / Grok / etc.):
#   Inside a vmux pane, VMUX_PANE_ID is set automatically (LMUX_PANE_ID still works).
#   Call these from agent stop/tool hooks, or pipe JSON:
#     echo '{"event":"stop","message":"done"}' | vmux hooks event
#     echo '{"event":"needs-input","message":"approve edit"}' | vmux hooks event
#   OSC fallback (many agents emit this):
#     printf '\033]777;notify;Agent;waiting for approval\a'

# Prefer VMUX_*; fall back to legacy LMUX_* from older pane env / scripts.
_vmux_pane_id() {
  if [ -n "${VMUX_PANE_ID:-}" ]; then
    printf '%s' "$VMUX_PANE_ID"
  elif [ -n "${LMUX_PANE_ID:-}" ]; then
    printf '%s' "$LMUX_PANE_ID"
  fi
}

_vmux_bin() {
  if command -v vmux >/dev/null 2>&1; then
    printf '%s' vmux
  elif command -v lmux >/dev/null 2>&1; then
    printf '%s' lmux
  else
    printf '%s' vmux
  fi
}

vmux_hook_status() {
  local status="${1:-busy}"
  shift || true
  local message="${*:-$status}"
  local pane_args=()
  local pane
  pane="$(_vmux_pane_id)"
  if [ -n "$pane" ]; then
    pane_args=(--pane "$pane")
  fi
  "$(_vmux_bin)" set-status "$status" "${pane_args[@]}" --message "$message" >/dev/null 2>&1 || true
}

vmux_hook_busy() {
  vmux_hook_status busy "${*:-working}"
}

vmux_hook_done() {
  vmux_hook_status done "${*:-complete}"
  vmux_hook_progress 100
}

vmux_hook_error() {
  vmux_hook_status error "${*:-failed}"
}

vmux_hook_progress() {
  local value="${1:-0}"
  local pane_args=()
  local pane
  pane="$(_vmux_pane_id)"
  if [ -n "$pane" ]; then
    pane_args=(--pane "$pane")
  fi
  "$(_vmux_bin)" set-progress "$value" "${pane_args[@]}" >/dev/null 2>&1 || true
}

vmux_hook_notify() {
  local message="${*:-needs attention}"
  local pane_args=()
  local pane
  pane="$(_vmux_pane_id)"
  if [ -n "$pane" ]; then
    pane_args=(--pane "$pane")
  fi
  "$(_vmux_bin)" notify "${pane_args[@]}" --status attention --message "$message" >/dev/null 2>&1 || true
}

vmux_hook_attention() {
  vmux_hook_status attention "${*:-needs input}"
  vmux_hook_notify "${*:-needs input}"
}

vmux_hook_run() {
  local label="${1:-command}"
  shift
  vmux_hook_busy "$label"
  "$@"
  local code=$?
  if [ "$code" -eq 0 ]; then
    vmux_hook_done "$label done"
  else
    vmux_hook_error "$label failed ($code)"
  fi
  return "$code"
}

# Pipe agent hook JSON (Claude Code Stop, Codex hooks, etc.) into vmux.
# Example Stop hook command:
#   cat | vmux hooks event --pane "${VMUX_PANE_ID:-${LMUX_PANE_ID:-}}"
vmux_hook_event_stdin() {
  local pane session_args=()
  pane="$(_vmux_pane_id)"
  if [ -n "$pane" ]; then
    session_args+=(--pane "$pane")
  fi
  if [ -n "${VMUX_SESSION:-${LMUX_SESSION:-}}" ]; then
    session_args+=(--session "${VMUX_SESSION:-$LMUX_SESSION}")
  fi
  "$(_vmux_bin)" hooks event "${session_args[@]}" "$@" >/dev/null 2>&1 || true
}

# Legacy lmux_* aliases so older scripts keep working after the rename.
lmux_hook_status() { vmux_hook_status "$@"; }
lmux_hook_busy() { vmux_hook_busy "$@"; }
lmux_hook_done() { vmux_hook_done "$@"; }
lmux_hook_error() { vmux_hook_error "$@"; }
lmux_hook_progress() { vmux_hook_progress "$@"; }
lmux_hook_notify() { vmux_hook_notify "$@"; }
lmux_hook_attention() { vmux_hook_attention "$@"; }
lmux_hook_run() { vmux_hook_run "$@"; }
lmux_hook_event_stdin() { vmux_hook_event_stdin "$@"; }
"#;

fn surface_command(session: &str, command: SurfaceCommand) -> Result<()> {
    daemon::ensure_running(session)?;
    let socket = paths::socket_path(session)?;
    match command {
        SurfaceCommand::New {
            direction,
            command,
            title,
            workspace,
        } => {
            let response = protocol::request(
                &socket,
                &protocol::Request::NewPane {
                    direction,
                    command,
                    title,
                    workspace,
                    surface_kind: None,
                },
            )?;
            print_response(response)
        }
        SurfaceCommand::Send {
            surface,
            enter,
            text,
        } => {
            let mut data = text.join(" ");
            if enter {
                data.push('\r');
            }
            let response = protocol::request(
                &socket,
                &protocol::Request::Input {
                    pane: surface,
                    data,
                },
            )?;
            print_response(response)
        }
        SurfaceCommand::SendKey { surface, keys } => {
            let response = protocol::request(
                &socket,
                &protocol::Request::SendKey {
                    pane: surface,
                    keys,
                },
            )?;
            print_response(response)
        }
        SurfaceCommand::Read {
            surface,
            no_scrollback,
            limit_bytes,
        } => {
            let response = protocol::request(
                &socket,
                &protocol::Request::ReadScreen {
                    pane: surface,
                    scrollback: !no_scrollback,
                    limit_bytes: non_default_limit(limit_bytes),
                    ansi: false,
                    history_lines: 0,
                },
            )?;
            print_response(response)
        }
        SurfaceCommand::Kill { surface } => {
            let response =
                protocol::request(&socket, &protocol::Request::KillPane { pane: surface })?;
            print_response(response)
        }
        SurfaceCommand::Focus { surface } => {
            let response =
                protocol::request(&socket, &protocol::Request::FocusPane { pane: surface })?;
            print_response(response)
        }
        SurfaceCommand::Duplicate { surface, direction } => {
            let response = protocol::request(
                &socket,
                &protocol::Request::DuplicatePane {
                    pane: surface,
                    direction,
                },
            )?;
            print_response(response)
        }
        SurfaceCommand::Swap { first, second } => {
            let response =
                protocol::request(&socket, &protocol::Request::SwapPanes { first, second })?;
            print_response(response)
        }
        SurfaceCommand::List => {
            let response = protocol::request(&socket, &protocol::Request::List)?;
            print_response(response)
        }
    }
}

fn browser_command(session: &str, command: BrowserCommand) -> Result<()> {
    daemon::ensure_running(session)?;
    let socket = paths::socket_path(session)?;
    let request = browser_request(command)?;
    let response = protocol::request(&socket, &request)?;
    print_response(response)
}

fn browser_request(command: BrowserCommand) -> Result<protocol::Request> {
    let request = match command {
        BrowserCommand::Open {
            url,
            direction,
            title,
            workspace,
        } => protocol::Request::OpenUrl {
            url,
            direction,
            title,
            workspace,
        },
        BrowserCommand::Snapshot { url } | BrowserCommand::Screenshot { url } => {
            protocol::Request::UrlSnapshot { url }
        }
        BrowserCommand::Links { url } => protocol::Request::UrlLinks { url },
        BrowserCommand::Forms { url } => protocol::Request::UrlForms { url },
        BrowserCommand::Evaluate { url, expression } => {
            protocol::Request::UrlEvaluate { url, expression }
        }
        BrowserCommand::Console { url } => protocol::Request::UrlConsole { url },
        BrowserCommand::Network { url } => protocol::Request::UrlNetwork { url },
        BrowserCommand::OpenLink {
            url,
            index,
            direction,
            title,
            workspace,
        }
        | BrowserCommand::Click {
            url,
            index,
            direction,
            title,
            workspace,
        } => protocol::Request::OpenUrlLink {
            url,
            index,
            direction,
            title,
            workspace,
        },
        BrowserCommand::Submit {
            url,
            index,
            fields,
            direction,
            title,
            workspace,
        }
        | BrowserCommand::Fill {
            url,
            index,
            fields,
            direction,
            title,
            workspace,
        }
        | BrowserCommand::Type {
            url,
            index,
            fields,
            direction,
            title,
            workspace,
        } => protocol::Request::SubmitForm {
            url,
            index,
            fields: parse_field_args(fields)?,
            direction,
            title,
            workspace,
        },
    };
    Ok(request)
}

fn parse_field_args(fields: Vec<String>) -> Result<BTreeMap<String, String>> {
    let mut parsed = BTreeMap::new();
    for field in fields {
        let Some((name, value)) = field.split_once('=') else {
            return Err(anyhow!("field must use name=value syntax: {field}"));
        };
        if name.trim().is_empty() {
            return Err(anyhow!("field name cannot be empty"));
        }
        parsed.insert(name.to_string(), value.to_string());
    }
    Ok(parsed)
}

fn tab_command(session: &str, command: TabCommand) -> Result<()> {
    daemon::ensure_running(session)?;
    let socket = paths::socket_path(session)?;
    let request = match command {
        TabCommand::List { workspace } => protocol::Request::ListTabs { workspace },
        TabCommand::New {
            workspace,
            title,
            command,
        } => protocol::Request::NewTab {
            workspace,
            title,
            command,
        },
        TabCommand::Switch { workspace, tab } => protocol::Request::SwitchTab { workspace, tab },
        TabCommand::Rename {
            workspace,
            tab,
            title,
        } => protocol::Request::RenameTab {
            workspace,
            tab,
            title,
        },
        TabCommand::Close { workspace, tab } => protocol::Request::CloseTab { workspace, tab },
        TabCommand::Next { workspace } => {
            return relative_tab_switch(session, workspace, true);
        }
        TabCommand::Previous { workspace } => {
            return relative_tab_switch(session, workspace, false);
        }
    };
    let response = protocol::request(&socket, &request)?;
    print_response(response)
}

fn relative_tab_switch(session: &str, workspace: Option<String>, going_next: bool) -> Result<()> {
    daemon::ensure_running(session)?;
    let socket = paths::socket_path(session)?;
    let list = protocol::request(
        &socket,
        &protocol::Request::ListTabs {
            workspace: workspace.clone(),
        },
    )?;
    if !list.ok {
        return print_response(list);
    }
    let tabs = list
        .data
        .as_ref()
        .and_then(|d| d.get("tabs"))
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    let active = list
        .data
        .as_ref()
        .and_then(|d| d.get("active_tab"))
        .and_then(|t| t.as_str())
        .unwrap_or("");
    if tabs.is_empty() {
        return Err(anyhow!("no tabs in workspace"));
    }
    let idx = tabs
        .iter()
        .position(|t| t.get("id").and_then(|i| i.as_str()) == Some(active))
        .unwrap_or(0);
    let next_idx = if going_next {
        (idx + 1) % tabs.len()
    } else {
        (idx + tabs.len() - 1) % tabs.len()
    };
    let tab = tabs[next_idx]
        .get("id")
        .and_then(|i| i.as_str())
        .ok_or_else(|| anyhow!("tab list missing id"))?
        .to_string();
    let response = protocol::request(&socket, &protocol::Request::SwitchTab { workspace, tab })?;
    print_response(response)
}

fn pane_tab_command(session: &str, command: PaneTabCommand) -> Result<()> {
    // Kept for scripts that still call `pane-tab`; surface a clear migration path.
    let _ = (session, command);
    Err(anyhow!(
        "per-pane tabs were removed; use workspace tabs:\n  \
         vmux tab list|new|switch|rename|close|next|previous\n  \
         hierarchy: workspace → tab → pane"
    ))
}

fn metadata_command(session: &str, command: MetadataCommand) -> Result<()> {
    daemon::ensure_running(session)?;
    let socket = paths::socket_path(session)?;
    let request = match command.clone() {
        MetadataCommand::List { pane } => protocol::Request::Identify { pane },
        MetadataCommand::Set { key, value, pane } => protocol::Request::SetPaneMetadata {
            pane,
            key,
            value: Some(value),
        },
        MetadataCommand::Clear { key, pane } => protocol::Request::SetPaneMetadata {
            pane,
            key,
            value: None,
        },
    };
    let response = protocol::request(&socket, &request)?;
    if matches!(command, MetadataCommand::List { .. }) {
        if !response.ok {
            return print_response(response);
        }
        let metadata = response
            .data
            .as_ref()
            .and_then(|data| data.get("metadata"))
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        return print_response(protocol::Response::ok(metadata));
    }
    print_response(response)
}

#[cfg(test)]
fn surface_kind_arg(kind: SurfaceKindArg) -> SurfaceKind {
    match kind {
        SurfaceKindArg::Terminal => SurfaceKind::Terminal,
        SurfaceKindArg::Browser => SurfaceKind::Browser,
        SurfaceKindArg::Agent => SurfaceKind::Agent,
        SurfaceKindArg::Markdown => SurfaceKind::Markdown,
    }
}

fn agent_command(session: &str, command: AgentCommand) -> Result<()> {
    daemon::ensure_running(session)?;
    let socket = paths::socket_path(session)?;
    match command {
        AgentCommand::New {
            direction,
            command,
            title,
            workspace,
        } => {
            let response = protocol::request(
                &socket,
                &protocol::Request::NewPane {
                    direction,
                    command,
                    title,
                    workspace,
                    surface_kind: Some(SurfaceKind::Agent),
                },
            )?;
            print_response(response)
        }
        AgentCommand::Team {
            agents,
            cwd,
            direction,
            no_agents_md,
        } => agent_team(&socket, agents, cwd, direction, !no_agents_md),
        AgentCommand::List => {
            let response = protocol::request(&socket, &protocol::Request::Agents)?;
            print_response(response)
        }
        AgentCommand::Send { agent, enter, text } => {
            let mut data = text.join(" ");
            if enter {
                data.push('\r');
            }
            let response =
                protocol::request(&socket, &protocol::Request::Input { pane: agent, data })?;
            print_response(response)
        }
        AgentCommand::Read {
            agent,
            no_scrollback,
            limit_bytes,
        } => {
            let response = protocol::request(
                &socket,
                &protocol::Request::ReadScreen {
                    pane: agent,
                    scrollback: !no_scrollback,
                    limit_bytes: non_default_limit(limit_bytes),
                    ansi: false,
                    history_lines: 0,
                },
            )?;
            print_response(response)
        }
        AgentCommand::Notify {
            agent,
            status,
            color,
            message,
        } => {
            let response = protocol::request(
                &socket,
                &protocol::Request::Notify {
                    pane: agent,
                    workspace: None,
                    status,
                    color,
                    clear: false,
                    message,
                },
            )?;
            print_response(response)
        }
    }
}

fn remote_command(session: &str, command: RemoteCommand) -> Result<()> {
    daemon::ensure_running(session)?;
    let socket = paths::socket_path(session)?;
    match command {
        RemoteCommand::Ssh {
            host,
            workspace,
            command,
            title,
            direction,
        } => {
            let workspace_name = workspace.unwrap_or_else(|| format!("ssh:{host}"));
            let pane_command = ssh_command(&host, command.as_deref());
            let pane_title = title.or_else(|| Some(format!("ssh:{host}")));
            create_workspace_with_pane(
                &socket,
                workspace_name,
                None,
                pane_command,
                pane_title,
                direction,
                Some(SurfaceKind::Terminal),
            )
        }
        RemoteCommand::Tmux {
            host,
            session,
            workspace,
            title,
            direction,
        } => {
            let workspace_name =
                workspace.unwrap_or_else(|| format!("tmux:{host}:{}", session.as_str()));
            let pane_command = remote_tmux_command(&host, &session);
            let pane_title = title.or_else(|| Some(format!("tmux:{host}:{session}")));
            create_workspace_with_pane(
                &socket,
                workspace_name,
                None,
                pane_command,
                pane_title,
                direction,
                Some(SurfaceKind::Terminal),
            )
        }
    }
}

fn markdown_command(session: &str, command: MarkdownCommand) -> Result<()> {
    match command {
        MarkdownCommand::Open {
            source,
            direction,
            title,
            workspace,
        } => {
            daemon::ensure_running(session)?;
            let pane_command = markdown_open_command(&source);
            let title = title.or_else(|| Some(markdown_title(&source)));
            let response = protocol::request(
                &paths::socket_path(session)?,
                &protocol::Request::NewPane {
                    direction,
                    command: pane_command,
                    title,
                    workspace,
                    surface_kind: Some(SurfaceKind::Markdown),
                },
            )?;
            print_response(response)
        }
        MarkdownCommand::Command { source } => {
            println!("{}", markdown_open_command(&source));
            Ok(())
        }
    }
}

fn markdown_open_command(source: &str) -> String {
    let is_url = source.starts_with("http://") || source.starts_with("https://");
    if is_url {
        if command_on_path("glow") {
            return shell_words::join(vec![
                "sh".to_string(),
                "-lc".to_string(),
                format!("curl -L -sS {} | glow -p -", shell_quote(source)),
            ]);
        }
        if command_on_path("mdcat") {
            return shell_words::join(vec![
                "sh".to_string(),
                "-lc".to_string(),
                format!("curl -L -sS {} | mdcat", shell_quote(source)),
            ]);
        }
        return shell_words::join(vec![
            "curl".to_string(),
            "-L".to_string(),
            "-sS".to_string(),
            source.to_string(),
        ]);
    }

    if command_on_path("glow") {
        return shell_words::join(vec![
            "glow".to_string(),
            "-p".to_string(),
            source.to_string(),
        ]);
    }
    if command_on_path("mdcat") {
        return shell_words::join(vec!["mdcat".to_string(), source.to_string()]);
    }
    if command_on_path("bat") {
        return shell_words::join(vec![
            "bat".to_string(),
            "--language".to_string(),
            "markdown".to_string(),
            source.to_string(),
        ]);
    }
    shell_words::join(vec!["cat".to_string(), source.to_string()])
}

fn markdown_title(source: &str) -> String {
    let trimmed = source.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or(trimmed);
    if name.is_empty() {
        "markdown".to_string()
    } else {
        format!("md:{name}")
    }
}

fn create_workspace_with_pane(
    socket: &std::path::Path,
    workspace_name: String,
    cwd: Option<String>,
    pane_command: String,
    pane_title: Option<String>,
    direction: cli::SplitDirection,
    surface_kind: Option<SurfaceKind>,
) -> Result<()> {
    let data = create_workspace_with_pane_data(
        socket,
        workspace_name,
        cwd,
        pane_command,
        pane_title,
        direction,
        surface_kind,
    )?;
    print_response(protocol::Response::ok(data))
}

fn create_workspace_with_pane_data(
    socket: &std::path::Path,
    workspace_name: String,
    cwd: Option<String>,
    pane_command: String,
    pane_title: Option<String>,
    direction: cli::SplitDirection,
    surface_kind: Option<SurfaceKind>,
) -> Result<serde_json::Value> {
    let workspace_response = protocol::request(
        socket,
        &protocol::Request::NewWorkspace {
            name: workspace_name,
            cwd,
        },
    )?;
    if !workspace_response.ok {
        return Err(anyhow!(
            "{}",
            workspace_response
                .error
                .unwrap_or_else(|| "new workspace failed".to_string())
        ));
    }
    let workspace = workspace_response
        .data
        .ok_or_else(|| anyhow!("new workspace response did not include workspace data"))?;
    let workspace_id = workspace_id_from_value(&workspace)?;

    let pane_response = protocol::request(
        socket,
        &protocol::Request::NewPane {
            direction,
            command: pane_command,
            title: pane_title,
            workspace: Some(workspace_id),
            surface_kind,
        },
    )?;
    if !pane_response.ok {
        return Err(anyhow!(
            "{}",
            pane_response
                .error
                .unwrap_or_else(|| "new pane failed".to_string())
        ));
    }

    Ok(serde_json::json!({
        "workspace": workspace,
        "pane": pane_response.data,
    }))
}

fn workspace_id_from_value(workspace: &serde_json::Value) -> Result<String> {
    workspace
        .get("id")
        .and_then(|id| id.as_str())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("new workspace response did not include workspace id"))
}

fn agent_team(
    socket: &std::path::Path,
    agents: Vec<String>,
    cwd: Option<String>,
    direction: cli::SplitDirection,
    write_agents_md: bool,
) -> Result<()> {
    let agents = normalize_agent_names(agents)?;
    let cwd_path = match cwd {
        Some(cwd) => std::path::PathBuf::from(cwd),
        None => std::env::current_dir()?,
    };
    let cwd = cwd_path.display().to_string();
    let agents_md = if write_agents_md {
        Some(write_agent_team_file(&cwd_path, &agents)?)
    } else {
        None
    };
    let mut created = Vec::new();
    for agent in &agents {
        let data = create_workspace_with_pane_data(
            socket,
            format!("agent:{agent}"),
            Some(cwd.clone()),
            agent.clone(),
            Some(format!("{agent}-agent")),
            direction,
            Some(SurfaceKind::Agent),
        )?;
        created.push(data);
    }
    print_response(protocol::Response::ok(serde_json::json!({
        "agents": agents,
        "cwd": cwd,
        "agents_md": agents_md,
        "created": created,
    })))
}

fn normalize_agent_names(agents: Vec<String>) -> Result<Vec<String>> {
    let mut normalized = Vec::new();
    for agent in agents {
        let agent = agent.trim();
        if agent.is_empty() {
            continue;
        }
        if !normalized.iter().any(|item| item == agent) {
            normalized.push(agent.to_string());
        }
    }
    if normalized.is_empty() {
        return Err(anyhow!("at least one agent is required"));
    }
    Ok(normalized)
}

fn write_agent_team_file(cwd: &std::path::Path, agents: &[String]) -> Result<String> {
    fs::create_dir_all(cwd)?;
    let path = cwd.join("AGENTS.md");
    // Never clobber an existing project AGENTS.md.
    if path.exists() {
        return Ok(path.display().to_string());
    }
    fs::write(&path, agent_team_markdown(agents))?;
    Ok(path.display().to_string())
}

fn agent_team_markdown(agents: &[String]) -> String {
    let roster = agents
        .iter()
        .map(|agent| format!("- `{agent}`"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "# vmux Agent Team\n\n\
Agents in this workspace:\n\n{roster}\n\n\
Use `vmux identify --json` to discover the current pane and socket context.\n\
Use `vmux agent send --agent PANE --enter \"message\"` to hand work to another pane.\n\
Use `vmux agent notify --agent PANE --status attention --message \"needs input\"` when blocked.\n\
Prefer short messages with the sender, target, task, and current status.\n"
    )
}

fn validate_move_pane_workspace_args(
    workspace: Option<String>,
    new_workspace: Option<String>,
) -> Result<MovePaneWorkspaceTarget> {
    match (workspace, new_workspace) {
        (Some(workspace), None) => Ok(MovePaneWorkspaceTarget::Existing(workspace)),
        (None, Some(name)) if !name.trim().is_empty() => Ok(MovePaneWorkspaceTarget::New(name)),
        (None, Some(_)) => Err(anyhow!("--new-workspace cannot be empty")),
        (Some(_), Some(_)) => Err(anyhow!(
            "use either --workspace or --new-workspace, not both"
        )),
        (None, None) => Err(anyhow!("move-pane requires --workspace or --new-workspace")),
    }
}

enum MovePaneWorkspaceTarget {
    Existing(String),
    New(String),
}

fn move_pane_target_workspace(
    socket: &std::path::Path,
    workspace: Option<String>,
    new_workspace: Option<String>,
) -> Result<String> {
    match validate_move_pane_workspace_args(workspace, new_workspace)? {
        MovePaneWorkspaceTarget::Existing(workspace) => Ok(workspace),
        MovePaneWorkspaceTarget::New(name) => {
            let response =
                protocol::request(socket, &protocol::Request::NewWorkspace { name, cwd: None })?;
            if !response.ok {
                return print_response(response).map(|_| String::new());
            }
            let workspace = response
                .data
                .ok_or_else(|| anyhow!("new workspace response did not include workspace data"))?;
            workspace_id_from_value(&workspace)
        }
    }
}

fn relative_workspace_from_socket(socket: &std::path::Path, delta: isize) -> Result<String> {
    let response = protocol::request(socket, &protocol::snapshot_full())?;
    if !response.ok {
        return print_response(response).map(|_| String::new());
    }
    let snapshot = response
        .data
        .ok_or_else(|| anyhow!("snapshot response did not include session data"))?;
    let snapshot = protocol::session_data_from_snapshot(snapshot);
    let snapshot = serde_json::from_value::<model::Session>(snapshot)?;
    relative_workspace_id(&snapshot, delta)
}

fn relative_workspace_id(snapshot: &model::Session, delta: isize) -> Result<String> {
    if snapshot.workspaces.is_empty() {
        return Err(anyhow!("session has no workspaces"));
    }
    let index = snapshot
        .workspaces
        .iter()
        .position(|workspace| workspace.id == snapshot.active_workspace)
        .unwrap_or(0);
    let next = (index as isize + delta).rem_euclid(snapshot.workspaces.len() as isize) as usize;
    Ok(snapshot.workspaces[next].id.clone())
}

fn ssh_command(host: &str, remote_command: Option<&str>) -> String {
    // `--` stops option injection via host.
    match remote_command {
        Some(command) if !command.trim().is_empty() => {
            format!("ssh -- {} {}", shell_quote(host), shell_quote(command))
        }
        _ => format!("ssh -- {}", shell_quote(host)),
    }
}

fn remote_tmux_command(host: &str, session: &str) -> String {
    format!(
        "ssh -- {} -t tmux attach -t {}",
        shell_quote(host),
        shell_quote(session)
    )
}

fn shell_quote(value: &str) -> String {
    if value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/' | b':' | b'@')
    }) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn run_pane(
    session: &str,
    direction: cli::SplitDirection,
    command: String,
    title: Option<String>,
    workspace: Option<String>,
    timeout: Option<u64>,
) -> Result<()> {
    daemon::ensure_running(session)?;
    let socket = paths::socket_path(session)?;
    let pane_response = protocol::request(
        &socket,
        &protocol::Request::NewPane {
            direction,
            command,
            title,
            workspace,
            surface_kind: None,
        },
    )?;
    if !pane_response.ok {
        return print_response(pane_response);
    }

    let pane = pane_response
        .data
        .ok_or_else(|| anyhow!("new-pane response did not include pane data"))?;
    let pane_id = pane
        .get("id")
        .and_then(|id| id.as_str())
        .ok_or_else(|| anyhow!("new-pane response did not include pane id"))?
        .to_string();

    let wait_response = protocol::request(
        &socket,
        &protocol::Request::WaitPane {
            pane: Some(pane_id.clone()),
            workspace: None,
            all: false,
            timeout_ms: wait_timeout_ms(timeout),
        },
    )?;
    if !wait_response.ok {
        return print_response(wait_response);
    }
    let final_pane = wait_response
        .data
        .ok_or_else(|| anyhow!("wait response did not include pane data"))?;

    let output_response = protocol::request(
        &socket,
        &protocol::Request::ReadScreen {
            pane: Some(pane_id),
            scrollback: true,
            limit_bytes: None,
            ansi: false,
            history_lines: 0,
        },
    )?;
    if !output_response.ok {
        return print_response(output_response);
    }
    let output = output_response
        .data
        .ok_or_else(|| anyhow!("read-screen response did not include output data"))?;

    print_response(protocol::Response::ok(serde_json::json!({
        "pane": final_pane,
        "output": output,
    })))
}

fn non_default_limit(limit_bytes: usize) -> Option<usize> {
    if limit_bytes == 16_000 {
        None
    } else {
        Some(limit_bytes)
    }
}

/// Default wait timeout (seconds) applied when `--timeout` is not supplied, so
/// `vmux wait`/`vmux run` never block indefinitely by accident.
const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 300;

/// Resolve a CLI `--timeout` (in seconds) into a `WaitPane` `timeout_ms`.
///
/// - `None` (flag omitted) -> default of [`DEFAULT_WAIT_TIMEOUT_SECS`].
/// - `Some(0)` -> infinite wait (`None`, no read timeout in `protocol`).
/// - `Some(secs)` -> that many seconds, in milliseconds.
fn wait_timeout_ms(timeout: Option<u64>) -> Option<u64> {
    match timeout {
        None => Some(DEFAULT_WAIT_TIMEOUT_SECS.saturating_mul(1000)),
        Some(0) => None,
        Some(secs) => Some(secs.saturating_mul(1000)),
    }
}

fn print_logs(session: &str, lines: usize) -> Result<()> {
    let path = paths::log_path(session)?;
    // Seek from the end in chunks so multi-GB logs do not load entirely.
    let text = tail_file_lines(&path, lines)
        .map_err(|err| anyhow!("read log {}: {err}", path.display()))?;
    print!("{text}");
    Ok(())
}

fn tail_file_lines(path: &std::path::Path, lines: usize) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    if lines == 0 {
        return Ok(String::new());
    }
    let mut file = std::fs::File::open(path)?;
    let len = file.seek(SeekFrom::End(0))?;
    if len == 0 {
        return Ok(String::new());
    }
    let mut pos = len;
    let mut buf = Vec::new();
    let mut found = 0usize;
    let chunk = 8192u64;
    while pos > 0 && found <= lines {
        let read_size = chunk.min(pos);
        pos -= read_size;
        file.seek(SeekFrom::Start(pos))?;
        let mut chunk_buf = vec![0u8; read_size as usize];
        file.read_exact(&mut chunk_buf)?;
        found += bytecount_newlines(&chunk_buf);
        buf.splice(0..0, chunk_buf);
        if found > lines && pos == 0 {
            break;
        }
        if found > lines {
            break;
        }
    }
    // Lossy UTF-8 is fine for log tails.
    let text = String::from_utf8_lossy(&buf);
    Ok(tail_text_lines(&text, lines))
}

fn bytecount_newlines(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&b| b == b'\n').count()
}

fn tail_text_lines(text: &str, lines: usize) -> String {
    if lines == 0 {
        return String::new();
    }
    let items = text.lines().collect::<Vec<_>>();
    let start = items.len().saturating_sub(lines);
    let mut out = items[start..].join("\n");
    if !out.is_empty() && text.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn stop_session(session: &str) -> Result<()> {
    let socket = paths::socket_path(session)?;
    let pid_path = paths::pid_path(session)?;
    match protocol::request(&socket, &protocol::Request::Shutdown) {
        Ok(response) => print_response(response),
        Err(socket_err) => {
            // No live socket: either the daemon already exited, or only a stale
            // pid file remains. Never treat "not running" as a hard failure.
            let Some(record) = paths::read_pid_record(&pid_path) else {
                std::fs::remove_file(&socket).ok();
                std::fs::remove_file(&pid_path).ok();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&protocol::Response::ok(serde_json::json!({
                        "session": session,
                        "status": "not_running",
                        "message": format!(
                            "session '{session}' is not running (no socket at {})",
                            socket.display()
                        ),
                    })))?
                );
                // Keep the original error in debug builds if needed; success for UX.
                let _ = socket_err;
                return Ok(());
            };
            let pid = record.pid;
            if !paths::process_matches_record(record)
                || !paths::process_cmdline_contains(pid, "vmux")
            {
                std::fs::remove_file(&socket).ok();
                std::fs::remove_file(&pid_path).ok();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&protocol::Response::ok(serde_json::json!({
                        "session": session,
                        "status": "not_running",
                        "stale_pid": pid,
                        "message": format!(
                            "session '{session}' is not running (stale/foreign pid {pid} cleaned up; not signalled)"
                        ),
                    })))?
                );
                return Ok(());
            }
            daemon::terminate_pid(pid)?;
            for _ in 0..20 {
                if !paths::process_exists(pid) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            // Never SIGKILL an unverified process; only re-check starttime.
            if paths::process_matches_record(record) {
                // Process ignored SIGTERM; leave pid file so doctor can report.
            } else {
                std::fs::remove_file(&pid_path).ok();
            }
            std::fs::remove_file(&socket).ok();
            println!(
                "{}",
                serde_json::to_string_pretty(&protocol::Response::ok(serde_json::json!({
                    "pid": pid,
                    "signal": "TERM"
                })))?
            );
            Ok(())
        }
    }
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    session: String,
    status: DoctorStatus,
    checks: Vec<DoctorCheck>,
    paths: DoctorPaths,
    helpers: BTreeMap<String, bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
enum DoctorStatus {
    Ok,
    Warn,
    Error,
}

#[derive(Debug, Serialize)]
struct DoctorCheck {
    name: String,
    ok: bool,
    detail: String,
}

#[derive(Debug, Serialize)]
struct DoctorPaths {
    runtime_dir: String,
    state_dir: String,
    socket_path: String,
    pid_path: String,
    log_path: String,
    state_path: String,
}

fn doctor(session: &str) -> Result<()> {
    let runtime_dir = paths::runtime_dir()?;
    let state_dir = paths::state_dir()?;
    let socket_path = paths::socket_path(session)?;
    let pid_path = paths::pid_path(session)?;
    let log_path = paths::log_path(session)?;
    let state_path = paths::state_path(session)?;
    let pid = paths::read_pid_file(&pid_path);
    let process_running = pid.map(paths::process_exists).unwrap_or(false);
    let socket_exists = socket_path.exists();
    let socket_response = if socket_exists {
        Some(protocol::request(&socket_path, &protocol::Request::Ping))
    } else {
        None
    };
    let socket_reachable = matches!(socket_response, Some(Ok(ref response)) if response.ok);
    let browser_helpers = ["w3m", "lynx", "links", "elinks", "browsh", "curl"];
    let other_helpers = ["git", "gh"];
    let mut helpers = BTreeMap::new();
    for name in browser_helpers.into_iter().chain(other_helpers) {
        helpers.insert(name.to_string(), command_on_path(name));
    }

    let mut checks = vec![
        DoctorCheck {
            name: "runtime-dir".to_string(),
            ok: runtime_dir.is_dir(),
            detail: runtime_dir.display().to_string(),
        },
        DoctorCheck {
            name: "state-dir".to_string(),
            ok: state_dir.is_dir(),
            detail: state_dir.display().to_string(),
        },
        DoctorCheck {
            name: "stdin-tty".to_string(),
            ok: std::io::stdin().is_terminal(),
            detail: "attach requires an interactive TTY".to_string(),
        },
        DoctorCheck {
            name: "stdout-tty".to_string(),
            ok: std::io::stdout().is_terminal(),
            detail: "attach renders to stdout".to_string(),
        },
        DoctorCheck {
            name: "daemon-pid".to_string(),
            ok: pid.map(paths::process_exists).unwrap_or(false),
            detail: pid
                .map(|pid| format!("pid {pid} running={process_running}"))
                .unwrap_or_else(|| "no pid file for this session".to_string()),
        },
        DoctorCheck {
            name: "daemon-socket".to_string(),
            ok: socket_reachable,
            detail: match socket_response {
                Some(Ok(response)) if response.ok => "socket responded to ping".to_string(),
                Some(Ok(response)) => response
                    .error
                    .unwrap_or_else(|| "socket returned an error response".to_string()),
                Some(Err(err)) => format!("socket exists but is not reachable: {err}"),
                None => "no socket file for this session".to_string(),
            },
        },
    ];

    let has_browser = browser_helpers
        .iter()
        .any(|name| helpers.get(*name).copied().unwrap_or(false));
    checks.push(DoctorCheck {
        name: "terminal-browser-helper".to_string(),
        ok: has_browser,
        detail: if has_browser {
            "at least one URL pane helper is installed".to_string()
        } else {
            "install w3m, lynx, links, elinks, browsh, or curl for open-url/browser panes"
                .to_string()
        },
    });

    for integration in agent_hooks::status_report() {
        let ok = matches!(
            integration.state,
            agent_hooks::InstallState::Installed | agent_hooks::InstallState::NotDetected
        );
        checks.push(DoctorCheck {
            name: format!("agent-hooks-{}", integration.kind.id()),
            ok,
            detail: format!(
                "{} ({}) — {}",
                integration.state.label(),
                integration.path.display(),
                integration.detail
            ),
        });
    }

    let status = if !runtime_dir.is_dir() || !state_dir.is_dir() {
        DoctorStatus::Error
    } else if checks.iter().any(|check| !check.ok) {
        DoctorStatus::Warn
    } else {
        DoctorStatus::Ok
    };

    print_response(protocol::Response::ok(DoctorReport {
        session: session.to_string(),
        status,
        checks,
        paths: DoctorPaths {
            runtime_dir: runtime_dir.display().to_string(),
            state_dir: state_dir.display().to_string(),
            socket_path: socket_path.display().to_string(),
            pid_path: pid_path.display().to_string(),
            log_path: log_path.display().to_string(),
            state_path: state_path.display().to_string(),
        },
        helpers,
    }))
}

fn smoke(keep: bool) -> Result<()> {
    let session = smoke_session_name();
    let socket = paths::socket_path(&session)?;
    let pid_path = paths::pid_path(&session)?;
    let log_path = paths::log_path(&session)?;
    let state_path = paths::state_path(&session)?;

    let mut checks = Vec::new();
    let mut restore_pane_id = None;
    let result = (|| -> Result<serde_json::Value> {
        daemon::ensure_running(&session)?;
        checks.push(smoke_check("daemon-running", daemon::is_running(&session)));

        let ping = protocol::request(&socket, &protocol::Request::Ping)?;
        checks.push(smoke_check("socket-ping", ping.ok));

        let pane = protocol::request(
            &socket,
            &protocol::Request::NewPane {
                direction: cli::SplitDirection::Right,
                command: "/bin/sh -c 'printf vmux-smoke'".to_string(),
                title: Some("smoke".to_string()),
                workspace: None,
                surface_kind: None,
            },
        )?;
        if !pane.ok {
            return Err(anyhow!("new-pane smoke response failed: {:?}", pane.error));
        }
        let pane_data = pane
            .data
            .ok_or_else(|| anyhow!("new-pane smoke response had no pane data"))?;
        let pane_id = pane_data
            .get("id")
            .and_then(|id| id.as_str())
            .ok_or_else(|| anyhow!("new-pane smoke response had no pane id"))?
            .to_string();
        checks.push(smoke_check("new-pane", !pane_id.is_empty()));

        let wait = protocol::request(
            &socket,
            &protocol::Request::WaitPane {
                pane: Some(pane_id.clone()),
                workspace: None,
                all: false,
                timeout_ms: Some(3_000),
            },
        )?;
        checks.push(smoke_check("wait-pane", wait.ok));

        let read = protocol::request(
            &socket,
            &protocol::Request::ReadScreen {
                pane: Some(pane_id.clone()),
                scrollback: true,
                limit_bytes: Some(16_000),
                ansi: false,
                history_lines: 0,
            },
        )?;
        let output_contains_smoke = read
            .data
            .as_ref()
            .and_then(|data| {
                data.get("screen")
                    .or_else(|| data.get("scrollback"))
                    .and_then(|text| text.as_str())
            })
            .map(|text| text.contains("vmux-smoke"))
            .unwrap_or(false);
        checks.push(smoke_check("read-screen-output", output_contains_smoke));

        let notify = protocol::request(
            &socket,
            &protocol::Request::Notify {
                pane: Some(pane_id.clone()),
                workspace: None,
                status: Some("attention".to_string()),
                color: Some("blue".to_string()),
                clear: false,
                message: "smoke notification".to_string(),
            },
        )?;
        checks.push(smoke_check("notify", notify.ok));

        let snapshot = protocol::request(&socket, &protocol::snapshot_full())?;
        let session_json = snapshot
            .data
            .as_ref()
            .map(|data| protocol::session_data_from_snapshot(data.clone()));
        let notification_visible = session_json
            .as_ref()
            .and_then(|data| data.get("notifications"))
            .and_then(|notes| notes.as_array())
            .map(|notes| {
                notes.iter().any(|note| {
                    note.get("message").and_then(|item| item.as_str()) == Some("smoke notification")
                })
            })
            .unwrap_or(false);
        checks.push(smoke_check("snapshot-notification", notification_visible));

        let live_pane = protocol::request(
            &socket,
            &protocol::Request::NewPane {
                direction: cli::SplitDirection::Right,
                command: "/bin/sh -c 'printf vmux-live; sleep 20'".to_string(),
                title: Some("smoke-live".to_string()),
                workspace: None,
                surface_kind: None,
            },
        )?;
        if !live_pane.ok {
            return Err(anyhow!(
                "live new-pane smoke response failed: {:?}",
                live_pane.error
            ));
        }
        let live_pane_data = live_pane
            .data
            .ok_or_else(|| anyhow!("live new-pane smoke response had no pane data"))?;
        let live_pane_id = live_pane_data
            .get("id")
            .and_then(|id| id.as_str())
            .ok_or_else(|| anyhow!("live new-pane smoke response had no pane id"))?
            .to_string();
        checks.push(smoke_check("live-pane", !live_pane_id.is_empty()));
        let live_output = read_screen_contains(&socket, &live_pane_id, "vmux-live", 20)?;
        checks.push(smoke_check("live-pane-output", live_output));

        let metadata = protocol::request(
            &socket,
            &protocol::Request::SetPaneMetadata {
                pane: Some(live_pane_id.clone()),
                key: "task".to_string(),
                value: Some("smoke-agent".to_string()),
            },
        )?;
        checks.push(smoke_check("metadata-set", metadata.ok));
        let identify = protocol::request(
            &socket,
            &protocol::Request::Identify {
                pane: Some(live_pane_id.clone()),
            },
        )?;
        let metadata_visible = identify
            .data
            .as_ref()
            .and_then(|data| data.get("metadata"))
            .and_then(|metadata| metadata.get("task"))
            .and_then(|value| value.as_str())
            == Some("smoke-agent");
        checks.push(smoke_check("metadata-identify", metadata_visible));

        let events = protocol::request(&socket, &protocol::Request::Events { limit: 20 })?;
        let event_kinds = events
            .data
            .as_ref()
            .and_then(|data| data.get("events"))
            .and_then(|events| events.as_array())
            .cloned()
            .unwrap_or_default();
        let notification_event = event_kinds.iter().any(|event| {
            event.get("kind").and_then(|kind| kind.as_str()) == Some("notification")
                && event.get("message").and_then(|message| message.as_str())
                    == Some("smoke notification")
        });
        let metadata_event = event_kinds.iter().any(|event| {
            event.get("kind").and_then(|kind| kind.as_str()) == Some("metadata")
                && event.get("key").and_then(|key| key.as_str()) == Some("task")
                && event.get("value").and_then(|value| value.as_str()) == Some("smoke-agent")
        });
        checks.push(smoke_check("events-notification", notification_event));
        checks.push(smoke_check("events-metadata", metadata_event));
        let events_follow = events_follow_contains(&session, &socket, &live_pane_id)?;
        checks.push(smoke_check("events-follow", events_follow));

        let daemon_pid = paths::read_pid_file(&pid_path)
            .ok_or_else(|| anyhow!("smoke daemon pid file was missing"))?;
        send_hangup_signal(daemon_pid)?;
        let survived_hangup = wait_for_socket_ping(&socket, 20)?;
        checks.push(smoke_check("daemon-survives-sighup", survived_hangup));
        let live_after_hangup = read_screen_contains(&socket, &live_pane_id, "vmux-live", 20)?;
        checks.push(smoke_check("live-pane-after-sighup", live_after_hangup));

        // Workspace tabs (Workspace → Tab → Pane), not the removed per-pane tab API.
        let tab = protocol::request(
            &socket,
            &protocol::Request::NewTab {
                workspace: None,
                title: Some("smoke-tab".to_string()),
                command: Some(
                    "/bin/sh -c 'printf vmux-tab-start; sleep 1; printf vmux-tab-late; sleep 20'"
                        .to_string(),
                ),
            },
        )?;
        if !tab.ok {
            return Err(anyhow!(
                "workspace tab new smoke response failed: {:?}",
                tab.error
            ));
        }
        let tab_id = tab
            .data
            .as_ref()
            .and_then(|data| data.get("tab"))
            .and_then(|tab| tab.get("id"))
            .and_then(|id| id.as_str())
            .ok_or_else(|| anyhow!("tab new smoke response had no tab id"))?
            .to_string();
        let tab_pane_id = tab
            .data
            .as_ref()
            .and_then(|data| {
                data.get("pane")
                    .and_then(|p| p.get("id"))
                    .and_then(|id| id.as_str())
                    .or_else(|| {
                        data.get("tab")
                            .and_then(|t| t.get("active_pane"))
                            .and_then(|p| p.as_str())
                    })
            })
            .map(|s| s.to_string())
            .unwrap_or_else(|| live_pane_id.clone());
        checks.push(smoke_check("workspace-tab-add", !tab_id.is_empty()));
        let tab_start = read_screen_contains(&socket, &tab_pane_id, "vmux-tab-start", 20)?;
        checks.push(smoke_check("workspace-tab-active-output", tab_start));

        let switch_base = protocol::request(
            &socket,
            &protocol::Request::SwitchTab {
                workspace: None,
                tab: "tab-1".to_string(),
            },
        )?;
        checks.push(smoke_check("workspace-tab-switch-base", switch_base.ok));
        let base_output = read_screen_contains(&socket, &live_pane_id, "vmux-live", 20)?;
        checks.push(smoke_check("workspace-tab-base-output", base_output));

        // Background tab continues producing output while inactive.
        let inactive_tab_output = read_screen_contains(&socket, &tab_pane_id, "vmux-tab-late", 40)?;
        checks.push(smoke_check(
            "workspace-tab-inactive-output",
            inactive_tab_output,
        ));

        let switch_tab = protocol::request(
            &socket,
            &protocol::Request::SwitchTab {
                workspace: None,
                tab: tab_id.clone(),
            },
        )?;
        checks.push(smoke_check("workspace-tab-switch-back", switch_tab.ok));
        let restored_tab_output = read_screen_contains(&socket, &tab_pane_id, "vmux-tab-late", 20)?;
        checks.push(smoke_check(
            "workspace-tab-restored-output",
            restored_tab_output,
        ));

        let restore_base = protocol::request(
            &socket,
            &protocol::Request::SwitchTab {
                workspace: None,
                tab: "tab-1".to_string(),
            },
        )?;
        checks.push(smoke_check("workspace-tab-restore-base", restore_base.ok));

        if command_on_path("script") && command_on_path("timeout") {
            let attach_rendered = attach_pty_smoke(&session, "vmux-live")?;
            checks.push(smoke_check("attach-pty-render", attach_rendered));
        } else {
            checks.push(smoke_check("attach-pty-skipped", true));
        }
        restore_pane_id = Some(live_pane_id.clone());

        checks.push(smoke_check("socket-file", socket.exists()));
        checks.push(smoke_check(
            "pid-file",
            paths::read_pid_file(&pid_path)
                .map(paths::process_exists)
                .unwrap_or(false),
        ));
        checks.push(smoke_check("state-file", state_path.exists()));
        checks.push(smoke_check("log-file", log_path.exists()));
        checks.push(smoke_check(
            "session-discovery",
            paths::list_sessions()?
                .into_iter()
                .any(|item| item.name == session && item.running),
        ));

        Ok(serde_json::json!({
            "session": session,
            "pane": pane_id,
            "restore_pane": live_pane_id,
            "paths": {
                "socket": socket.display().to_string(),
                "pid": pid_path.display().to_string(),
                "log": log_path.display().to_string(),
                "state": state_path.display().to_string(),
            },
        }))
    })();

    let shutdown = protocol::request(&socket, &protocol::Request::Shutdown)
        .map(|response| response.ok)
        .unwrap_or(false);
    for _ in 0..40 {
        if !daemon::is_running(&session) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    checks.push(smoke_check(
        "shutdown-before-restore",
        shutdown && !daemon::is_running(&session),
    ));

    if result.is_ok() {
        match (|| -> Result<()> {
            daemon::ensure_running(&session)?;
            checks.push(smoke_check("restart-daemon", daemon::is_running(&session)));
            let ping = protocol::request(&socket, &protocol::Request::Ping)?;
            checks.push(smoke_check("restart-socket-ping", ping.ok));
            let pane_id = restore_pane_id
                .as_ref()
                .ok_or_else(|| anyhow!("smoke restore pane id was not recorded"))?;
            let restored_output = read_screen_contains(&socket, pane_id, "vmux-live", 30)?;
            checks.push(smoke_check("restore-pane-output", restored_output));
            Ok(())
        })() {
            Ok(()) => {}
            Err(err) => checks.push(smoke_check(&format!("restore-error:{err:#}"), false)),
        }
    }

    let final_shutdown = protocol::request(&socket, &protocol::Request::Shutdown)
        .map(|response| response.ok)
        .unwrap_or(false);
    for _ in 0..40 {
        if !daemon::is_running(&session) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    checks.push(smoke_check(
        "shutdown-after-restore",
        final_shutdown && !daemon::is_running(&session),
    ));

    if !keep {
        std::fs::remove_file(&state_path).ok();
        std::fs::remove_file(&log_path).ok();
    }

    match result {
        Ok(data)
            if checks
                .iter()
                .all(|check| check["ok"].as_bool().unwrap_or(false)) =>
        {
            print_response(protocol::Response::ok(serde_json::json!({
                "smoke": data,
                "keep": keep,
                "checks": checks,
            })))
        }
        Ok(data) => print_response(protocol::Response::err(format!(
            "smoke checks failed: {}",
            serde_json::json!({
                "smoke": data,
                "keep": keep,
                "checks": checks,
            })
        ))),
        Err(err) => print_response(protocol::Response::err(format!(
            "smoke failed: {err:#}; checks={}",
            serde_json::Value::Array(checks)
        ))),
    }
}

fn smoke_session_name() -> String {
    format!("vmux-smoke-{}-{}", std::process::id(), unix_millis())
}

fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn read_screen_contains(
    socket: &std::path::Path,
    pane_id: &str,
    needle: &str,
    attempts: usize,
) -> Result<bool> {
    for _ in 0..attempts.max(1) {
        let read = protocol::request(
            socket,
            &protocol::Request::ReadScreen {
                pane: Some(pane_id.to_string()),
                scrollback: true,
                limit_bytes: Some(16_000),
                ansi: false,
                history_lines: 0,
            },
        )?;
        let contains = read
            .data
            .as_ref()
            .and_then(|data| {
                data.get("screen")
                    .or_else(|| data.get("scrollback"))
                    .and_then(|text| text.as_str())
            })
            .map(|text| text.contains(needle))
            .unwrap_or(false);
        if contains {
            return Ok(true);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(false)
}

fn wait_for_socket_ping(socket: &std::path::Path, attempts: usize) -> Result<bool> {
    for _ in 0..attempts.max(1) {
        if protocol::request(socket, &protocol::Request::Ping)
            .map(|response| response.ok)
            .unwrap_or(false)
        {
            return Ok(true);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(false)
}

fn follow_events(session: &str, limit: usize, interval_ms: u64) -> Result<()> {
    let socket = paths::socket_path(session)?;
    let event_id = |event: &serde_json::Value| event.get("id").and_then(|v| v.as_u64());
    // Cursor: the highest event id already emitted. Dedup on the monotonic id,
    // NOT the serialized JSON — the daemon embeds mutable pane_title /
    // workspace_name resolved at query time, so a title change would otherwise
    // re-serialize a seen event and re-print it as new. (A burst larger than
    // `limit` between polls can still scroll out of the window; fully fixing
    // that needs a `since`-cursor on the Events request.)
    let mut last_id = events_from_socket(&socket, limit)?
        .iter()
        .filter_map(&event_id)
        .max()
        .unwrap_or(0);
    let interval = std::time::Duration::from_millis(interval_ms.clamp(100, 10_000));
    loop {
        let mut events = events_from_socket(&socket, limit)?;
        events.reverse();
        for event in events {
            let Some(id) = event_id(&event) else { continue };
            if id <= last_id {
                continue;
            }
            last_id = id;
            println!("{}", serde_json::to_string(&event)?);
            std::io::stdout().flush()?;
        }
        std::thread::sleep(interval);
    }
}

fn events_from_socket(socket: &std::path::Path, limit: usize) -> Result<Vec<serde_json::Value>> {
    let response = protocol::request(socket, &protocol::Request::Events { limit })?;
    if !response.ok {
        return Err(anyhow!(
            "{}",
            response
                .error
                .unwrap_or_else(|| "events request failed".to_string())
        ));
    }
    Ok(response
        .data
        .as_ref()
        .and_then(|data| data.get("events"))
        .and_then(|events| events.as_array())
        .cloned()
        .unwrap_or_default())
}

fn events_follow_contains(session: &str, socket: &std::path::Path, pane_id: &str) -> Result<bool> {
    let mut child = ProcessCommand::new(std::env::current_exe()?)
        .arg("--session")
        .arg(session)
        .arg("events")
        .arg("--follow")
        .arg("--interval-ms")
        .arg("100")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    std::thread::sleep(std::time::Duration::from_millis(250));
    let response = protocol::request(
        socket,
        &protocol::Request::Notify {
            pane: Some(pane_id.to_string()),
            workspace: None,
            status: Some("attention".to_string()),
            color: Some("blue".to_string()),
            clear: false,
            message: "smoke follow event".to_string(),
        },
    )?;
    if !response.ok {
        child.kill().ok();
        child.wait().ok();
        return Ok(false);
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    child.kill().ok();
    let output = child.wait_with_output()?;
    Ok(String::from_utf8_lossy(&output.stdout).contains("smoke follow event"))
}

#[cfg(unix)]
fn send_hangup_signal(pid: u32) -> Result<()> {
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGHUP) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).map_err(|err| anyhow!("failed to send SIGHUP: {err}"))
    }
}

#[cfg(not(unix))]
fn send_hangup_signal(_pid: u32) -> Result<()> {
    Ok(())
}

fn attach_pty_smoke(session: &str, needle: &str) -> Result<bool> {
    let exe = std::env::current_exe()?;
    let transcript = std::env::temp_dir().join(format!(
        "vmux-attach-smoke-{}-{}.log",
        session,
        unix_millis()
    ));
    let attach_command = shell_words::join(vec![
        exe.display().to_string(),
        "--session".to_string(),
        session.to_string(),
        "attach".to_string(),
    ]);
    let script_command = format!("stty rows 24 cols 100; {attach_command}");
    let output = ProcessCommand::new("timeout")
        .arg("5")
        .arg("script")
        .arg("-qfec")
        .arg(script_command)
        .arg(&transcript)
        .output()?;
    let transcript_bytes = fs::read(&transcript).unwrap_or_default();
    fs::remove_file(&transcript).ok();
    let rendered = transcript_bytes
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
        && transcript_bytes
            .windows(b"vmux".len())
            .any(|window| window == b"vmux");
    let failed_before_timeout = !output.status.success() && output.status.code() != Some(124);
    Ok(rendered && !failed_before_timeout)
}

fn smoke_check(name: &str, ok: bool) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "ok": ok,
    })
}

fn command_on_path(name: &str) -> bool {
    command_on_path_in(name, std::env::var_os("PATH"))
}

fn command_on_path_in(name: &str, paths: Option<OsString>) -> bool {
    let Some(paths) = paths else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        let path = dir.join(name);
        path.is_file()
            && path
                .metadata()
                .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
    })
}

/// Screenshots are a few MB; anything past this is not a paste gone right.
const MAX_SEND_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

fn read_image_input(file: &str) -> Result<Vec<u8>> {
    let bytes = if file == "-" {
        let mut buf = Vec::new();
        std::io::stdin()
            .lock()
            .take(MAX_SEND_IMAGE_BYTES + 1)
            .read_to_end(&mut buf)?;
        buf
    } else {
        let meta = fs::metadata(file).map_err(|err| anyhow!("read {file}: {err}"))?;
        if meta.len() > MAX_SEND_IMAGE_BYTES {
            return Err(anyhow!(
                "{file} is {} bytes; refusing images over {} bytes",
                meta.len(),
                MAX_SEND_IMAGE_BYTES
            ));
        }
        fs::read(file).map_err(|err| anyhow!("read {file}: {err}"))?
    };
    if bytes.is_empty() {
        return Err(anyhow!(
            "no image data on stdin (is the clipboard empty, or holding text instead of an image?)"
        ));
    }
    if bytes.len() as u64 > MAX_SEND_IMAGE_BYTES {
        return Err(anyhow!(
            "stdin exceeded {MAX_SEND_IMAGE_BYTES} bytes; refusing"
        ));
    }
    Ok(bytes)
}

/// Sniff the image type from magic bytes. Agents only attach real images, so
/// reject unrecognized data instead of typing a junk path into the pane.
/// Shared with the relay's browser paste endpoint.
pub(crate) fn image_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Some("png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("jpg");
    }
    if bytes.starts_with(b"GIF8") {
        return Some("gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    if bytes.starts_with(b"BM") {
        return Some("bmp");
    }
    None
}

fn save_send_image(bytes: &[u8], ext: &str) -> Result<std::path::PathBuf> {
    let dir = paths::state_dir()?.join("images");
    fs::create_dir_all(&dir)?;
    let pid = std::process::id();
    for attempt in 0..8u32 {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = dir.join(format!("image-{stamp}-{pid}-{attempt}.{ext}"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(bytes)?;
                return Ok(path);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        }
    }
    Err(anyhow!(
        "could not create a unique image path under {}",
        dir.display()
    ))
}

fn print_response(response: protocol::Response) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&response)?);
    if response.ok {
        Ok(())
    } else {
        Err(anyhow!(
            "{}",
            response
                .error
                .unwrap_or_else(|| "vmux command failed".to_string())
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn command_on_path_requires_executable_file() {
        let dir = std::env::temp_dir().join(format!("vmux-path-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let helper = dir.join("helper");
        fs::write(&helper, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!command_on_path_in("helper", Some(dir.as_os_str().into())));

        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(command_on_path_in("helper", Some(dir.as_os_str().into())));

        fs::remove_file(&helper).ok();
        fs::remove_dir(&dir).ok();
    }

    #[test]
    fn remote_commands_quote_shell_arguments() {
        assert_eq!(
            ssh_command("user@example.com", None),
            "ssh -- user@example.com"
        );
        assert_eq!(
            ssh_command("host", Some("cd /srv/app && claude")),
            "ssh -- host 'cd /srv/app && claude'"
        );
        assert_eq!(
            remote_tmux_command("build host", "agent's"),
            "ssh -- 'build host' -t tmux attach -t 'agent'\\''s'"
        );
    }

    #[test]
    fn shell_hooks_include_status_progress_and_notification_helpers() {
        let hooks = shell_hooks(HookShell::Bash);
        assert!(hooks.contains("vmux_hook_status()"));
        assert!(hooks.contains("set-status"));
        assert!(hooks.contains("set-progress"));
        assert!(hooks.contains("notify"));
        assert!(hooks.contains("VMUX_PANE_ID"));
        assert!(hooks.contains("LMUX_PANE_ID")); // legacy fallback
        assert!(hooks.contains("_vmux_pane_id"));
        assert!(hooks.contains("vmux_hook_run()"));
        assert!(hooks.contains("lmux_hook_done()")); // legacy aliases
    }

    #[test]
    fn image_extension_sniffs_common_formats() {
        assert_eq!(
            image_extension(&[0x89, b'P', b'N', b'G', 0x0D]),
            Some("png")
        );
        assert_eq!(image_extension(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(image_extension(b"GIF89a"), Some("gif"));
        assert_eq!(
            image_extension(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some("webp")
        );
        assert_eq!(image_extension(b"BM\x00\x00"), Some("bmp"));
        assert_eq!(image_extension(b"hello, not an image"), None);
        assert_eq!(image_extension(b""), None);
        // RIFF without the WEBP tag (e.g. a .wav file) is not an image.
        assert_eq!(image_extension(b"RIFF\x00\x00\x00\x00WAVEfmt "), None);
    }

    #[test]
    fn send_image_cli_parses() {
        let cli = Cli::try_parse_from(["vmux", "send-image", "-", "--enter"]).unwrap();
        match cli.command {
            Some(Command::SendImage { file, pane, enter }) => {
                assert_eq!(file, "-");
                assert_eq!(pane, None);
                assert!(enter);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn save_send_image_writes_unique_files() {
        let first = save_send_image(&[0x89, b'P', b'N', b'G'], "png").unwrap();
        let second = save_send_image(&[0x89, b'P', b'N', b'G'], "png").unwrap();
        assert_ne!(first, second);
        assert!(first.exists());
        assert_eq!(fs::read(&first).unwrap(), vec![0x89, b'P', b'N', b'G']);
        fs::remove_file(&first).ok();
        fs::remove_file(&second).ok();
    }

    #[test]
    fn hooks_status_and_install_cli_parse() {
        let status = Cli::try_parse_from(["vmux", "hooks", "status"]).unwrap();
        assert!(matches!(
            status.command,
            Some(Command::Hooks {
                command: HooksCommand::Status
            })
        ));
        let install =
            Cli::try_parse_from(["vmux", "hooks", "install", "--agent", "claude"]).unwrap();
        match install.command {
            Some(Command::Hooks {
                command: HooksCommand::Install { agent },
            }) => assert_eq!(agent.as_deref(), Some("claude")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn hook_event_defaults_map_agent_lifecycle_events() {
        assert_eq!(hook_event_defaults("Notification").0, "attention");
        assert_eq!(hook_event_defaults("PermissionRequest").0, "attention");
        assert_eq!(hook_event_defaults("Stop").0, "done");
        assert_eq!(hook_event_defaults("StopFailure").0, "error");
        assert_eq!(hook_event_defaults("UserPromptSubmit").0, "busy");
        assert_eq!(hook_event_defaults("PreToolUse").0, "busy");
        assert_eq!(hook_event_defaults("PostToolUse").0, "busy");
        // Must not treat SubagentStop / tool events as false statuses.
        assert_eq!(hook_event_defaults("SubagentStart").0, "busy");
    }

    #[test]
    fn smoke_session_names_are_disposable() {
        let name = smoke_session_name();
        assert!(name.starts_with("vmux-smoke-"));
        assert!(name.contains(&std::process::id().to_string()));
    }

    #[test]
    fn events_follow_cli_flags_are_parsed() {
        let cli = Cli::try_parse_from([
            "vmux",
            "events",
            "--limit",
            "25",
            "--follow",
            "--interval-ms",
            "250",
        ])
        .unwrap();
        match cli.command.unwrap() {
            Command::Events {
                limit,
                follow,
                interval_ms,
            } => {
                assert_eq!(limit, 25);
                assert!(follow);
                assert_eq!(interval_ms, 250);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn hook_source_line_quotes_paths() {
        let line = hook_source_line(std::path::Path::new("/tmp/vmux hooks/hooks.sh"));
        assert_eq!(line, ". '/tmp/vmux hooks/hooks.sh'");
    }

    #[test]
    fn append_hook_source_once_is_idempotent() {
        let dir = std::env::temp_dir().join(format!(
            "vmux-hooks-test-{}-{}",
            std::process::id(),
            crate::model::unix_time()
        ));
        fs::create_dir_all(&dir).unwrap();
        let rc = dir.join(".bashrc");
        fs::write(&rc, "export TEST=1\n").unwrap();
        append_hook_source_once(&rc, ". /tmp/vmux/hooks.sh").unwrap();
        append_hook_source_once(&rc, ". /tmp/vmux/hooks.sh").unwrap();
        let content = fs::read_to_string(&rc).unwrap();
        assert_eq!(content.matches(". /tmp/vmux/hooks.sh").count(), 1);
        assert!(content.contains("# vmux shell hooks"));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn hook_event_request_maps_json_payload_to_notify() {
        let request = hook_event_request(
            Some("pane-1".to_string()),
            None,
            None,
            None,
            None,
            None,
            r#"{"event":"needs-input","message":"waiting for review"}"#,
        )
        .unwrap();
        match request {
            protocol::Request::Notify {
                pane,
                workspace,
                status,
                color,
                clear,
                message,
            } => {
                assert_eq!(pane.as_deref(), Some("pane-1"));
                assert_eq!(workspace, None);
                assert_eq!(status.as_deref(), Some("attention"));
                assert_eq!(color.as_deref(), Some("blue"));
                assert!(!clear);
                assert_eq!(message, "waiting for review");
            }
            other => panic!("unexpected request: {other:?}"),
        }
    }

    #[test]
    fn hook_event_request_allows_explicit_overrides() {
        let request = hook_event_request(
            None,
            Some("ws-1".to_string()),
            Some("stop".to_string()),
            Some("done".to_string()),
            Some("green".to_string()),
            Some("finished tests".to_string()),
            "",
        )
        .unwrap();
        match request {
            protocol::Request::Notify {
                pane,
                workspace,
                status,
                color,
                message,
                ..
            } => {
                assert_eq!(pane, None);
                assert_eq!(workspace.as_deref(), Some("ws-1"));
                assert_eq!(status.as_deref(), Some("done"));
                assert_eq!(color.as_deref(), Some("green"));
                assert_eq!(message, "finished tests");
            }
            other => panic!("unexpected request: {other:?}"),
        }
    }

    #[test]
    fn builtin_skill_summaries_include_vmux_control() {
        let summaries = builtin_skill_summaries();
        assert!(summaries.iter().any(|summary| {
            summary.get("name").and_then(|name| name.as_str()) == Some("vmux-control")
        }));
    }

    #[test]
    fn builtin_skill_lookup_and_markdown_cover_core_commands() {
        let skill = builtin_skill("vmux-control").unwrap();
        assert_eq!(skill.name, "vmux-control");
        assert!(skill.markdown.contains("vmux identify --json"));
        assert!(skill.markdown.contains("vmux new-pane"));
        assert!(skill.markdown.contains("vmux browser click"));
        assert!(skill.markdown.contains("vmux agent team"));
        assert!(skill.markdown.contains("vmux metadata set"));
        assert!(skill.markdown.contains("vmux events --follow"));
        assert!(skill.markdown.contains("vmux sessions"));
        assert!(builtin_skill("missing").is_err());
    }

    #[test]
    fn parse_field_args_requires_name_value_pairs() {
        let fields =
            parse_field_args(vec!["q=vmux".to_string(), "note=hello=world".to_string()]).unwrap();
        assert_eq!(fields.get("q").map(String::as_str), Some("vmux"));
        assert_eq!(fields.get("note").map(String::as_str), Some("hello=world"));
        assert!(parse_field_args(vec!["bad".to_string()]).is_err());
        assert!(parse_field_args(vec!["=empty".to_string()]).is_err());
    }

    #[test]
    fn browser_screenshot_alias_uses_snapshot_request() {
        let request = browser_request(BrowserCommand::Screenshot {
            url: "https://example.com".to_string(),
        })
        .unwrap();
        match request {
            protocol::Request::UrlSnapshot { url } => {
                assert_eq!(url, "https://example.com");
            }
            other => panic!("unexpected request: {other:?}"),
        }
    }

    #[test]
    fn browser_click_alias_uses_open_link_request() {
        let request = browser_request(BrowserCommand::Click {
            url: "https://example.com".to_string(),
            index: 2,
            direction: cli::SplitDirection::Down,
            title: Some("docs".to_string()),
            workspace: Some("ws-2".to_string()),
        })
        .unwrap();
        match request {
            protocol::Request::OpenUrlLink {
                url,
                index,
                direction,
                title,
                workspace,
            } => {
                assert_eq!(url, "https://example.com");
                assert_eq!(index, 2);
                assert_eq!(direction, cli::SplitDirection::Down);
                assert_eq!(title.as_deref(), Some("docs"));
                assert_eq!(workspace.as_deref(), Some("ws-2"));
            }
            other => panic!("unexpected request: {other:?}"),
        }
    }

    #[test]
    fn browser_evaluate_uses_url_evaluate_request() {
        let request = browser_request(BrowserCommand::Evaluate {
            url: "https://example.com".to_string(),
            expression: "links[1].href".to_string(),
        })
        .unwrap();
        match request {
            protocol::Request::UrlEvaluate { url, expression } => {
                assert_eq!(url, "https://example.com");
                assert_eq!(expression, "links[1].href");
            }
            other => panic!("unexpected request: {other:?}"),
        }
    }

    #[test]
    fn browser_console_and_network_use_browser_requests() {
        let console = browser_request(BrowserCommand::Console {
            url: "https://example.com".to_string(),
        })
        .unwrap();
        assert!(matches!(console, protocol::Request::UrlConsole { .. }));

        let network = browser_request(BrowserCommand::Network {
            url: "https://example.com".to_string(),
        })
        .unwrap();
        assert!(matches!(network, protocol::Request::UrlNetwork { .. }));
    }

    #[test]
    fn surface_kind_arg_maps_to_protocol_surface_kind() {
        assert_eq!(
            surface_kind_arg(SurfaceKindArg::Terminal),
            SurfaceKind::Terminal
        );
        assert_eq!(
            surface_kind_arg(SurfaceKindArg::Browser),
            SurfaceKind::Browser
        );
        assert_eq!(surface_kind_arg(SurfaceKindArg::Agent), SurfaceKind::Agent);
        assert_eq!(
            surface_kind_arg(SurfaceKindArg::Markdown),
            SurfaceKind::Markdown
        );
    }

    #[test]
    fn browser_fill_alias_uses_submit_form_request() {
        let request = browser_request(BrowserCommand::Fill {
            url: "https://example.com/login".to_string(),
            index: 1,
            fields: vec!["user=me".to_string(), "token=abc".to_string()],
            direction: cli::SplitDirection::Right,
            title: Some("login".to_string()),
            workspace: Some("ws-2".to_string()),
        })
        .unwrap();
        assert_submit_form_alias_request(request);
    }

    #[test]
    fn browser_type_alias_uses_submit_form_request() {
        let request = browser_request(BrowserCommand::Type {
            url: "https://example.com/login".to_string(),
            index: 1,
            fields: vec!["user=me".to_string(), "token=abc".to_string()],
            direction: cli::SplitDirection::Right,
            title: Some("login".to_string()),
            workspace: Some("ws-2".to_string()),
        })
        .unwrap();
        assert_submit_form_alias_request(request);
    }

    fn assert_submit_form_alias_request(request: protocol::Request) {
        match request {
            protocol::Request::SubmitForm {
                url,
                index,
                fields,
                direction,
                title,
                workspace,
            } => {
                assert_eq!(url, "https://example.com/login");
                assert_eq!(index, 1);
                assert_eq!(fields.get("user").map(String::as_str), Some("me"));
                assert_eq!(fields.get("token").map(String::as_str), Some("abc"));
                assert_eq!(direction, cli::SplitDirection::Right);
                assert_eq!(title.as_deref(), Some("login"));
                assert_eq!(workspace.as_deref(), Some("ws-2"));
            }
            other => panic!("unexpected request: {other:?}"),
        }
    }

    #[test]
    fn normalize_agent_names_deduplicates_and_rejects_empty() {
        assert_eq!(
            normalize_agent_names(vec![
                " codex ".to_string(),
                "claude".to_string(),
                "codex".to_string(),
                "".to_string(),
            ])
            .unwrap(),
            vec!["codex".to_string(), "claude".to_string()]
        );
        assert!(normalize_agent_names(vec![" ".to_string()]).is_err());
    }

    #[test]
    fn agent_team_markdown_lists_agents_and_vmux_commands() {
        let markdown = agent_team_markdown(&["codex".to_string(), "claude".to_string()]);
        assert!(markdown.contains("`codex`"));
        assert!(markdown.contains("`claude`"));
        assert!(markdown.contains("vmux identify --json"));
        assert!(markdown.contains("vmux agent send"));
        assert!(markdown.contains("vmux agent notify"));
    }

    #[test]
    fn markdown_title_uses_source_basename() {
        assert_eq!(markdown_title("README.md"), "md:README.md");
        assert_eq!(
            markdown_title("https://example.test/docs/guide.md"),
            "md:guide.md"
        );
        assert_eq!(markdown_title("https://example.test/docs/"), "md:docs");
    }

    #[test]
    fn markdown_command_has_cat_or_curl_fallback() {
        let local = markdown_open_command("README.md");
        assert!(
            local.contains("README.md"),
            "local markdown command should mention source: {local}"
        );
        let remote = markdown_open_command("https://example.test/README.md");
        assert!(
            remote.contains("https://example.test/README.md"),
            "remote markdown command should mention source: {remote}"
        );
    }

    #[test]
    fn workspace_id_from_value_requires_string_id() {
        assert_eq!(
            workspace_id_from_value(&serde_json::json!({ "id": "ws-2" })).unwrap(),
            "ws-2"
        );
        assert!(workspace_id_from_value(&serde_json::json!({ "name": "agents" })).is_err());
        assert!(workspace_id_from_value(&serde_json::json!({ "id": 2 })).is_err());
    }

    #[test]
    fn validate_move_pane_workspace_args_requires_one_target() {
        assert!(matches!(
            validate_move_pane_workspace_args(Some("ws-2".to_string()), None).unwrap(),
            MovePaneWorkspaceTarget::Existing(workspace) if workspace == "ws-2"
        ));
        assert!(matches!(
            validate_move_pane_workspace_args(None, Some("agents".to_string())).unwrap(),
            MovePaneWorkspaceTarget::New(name) if name == "agents"
        ));
        assert!(validate_move_pane_workspace_args(None, None).is_err());
        assert!(validate_move_pane_workspace_args(
            Some("ws-2".to_string()),
            Some("agents".to_string())
        )
        .is_err());
        assert!(validate_move_pane_workspace_args(None, Some("   ".to_string())).is_err());
    }

    #[test]
    fn relative_workspace_id_wraps_workspace_order() {
        let mut session = model::Session::new("test");
        session.workspaces.push(model::Workspace {
            next_tab_seq: 0,
            id: "ws-2".to_string(),
            name: "agents".to_string(),
            cwd: model::default_cwd(),
            git_branch: None,
            pull_request: None,
            ports: Vec::new(),
            pinned: false,
            tabs: vec![model::WorkspaceTab::new("tab-1", "main")],
            active_tab: Some("tab-1".to_string()),
            panes: Vec::new(),
            active_pane: None,
            zoomed_pane: None,
            layout: None,
        });
        session.workspaces.push(model::Workspace {
            next_tab_seq: 0,
            id: "ws-3".to_string(),
            name: "tests".to_string(),
            cwd: model::default_cwd(),
            git_branch: None,
            pull_request: None,
            ports: Vec::new(),
            pinned: false,
            tabs: vec![model::WorkspaceTab::new("tab-1", "main")],
            active_tab: Some("tab-1".to_string()),
            panes: Vec::new(),
            active_pane: None,
            zoomed_pane: None,
            layout: None,
        });
        session.active_workspace = "ws-2".to_string();

        assert_eq!(relative_workspace_id(&session, 1).unwrap(), "ws-3");
        assert_eq!(relative_workspace_id(&session, -1).unwrap(), "ws-1");
        assert_eq!(relative_workspace_id(&session, 2).unwrap(), "ws-1");

        session.active_workspace = "missing".to_string();
        assert_eq!(relative_workspace_id(&session, 1).unwrap(), "ws-2");
    }

    #[test]
    fn tail_text_lines_returns_requested_suffix() {
        assert_eq!(tail_text_lines("one\ntwo\nthree\n", 2), "two\nthree\n");
        assert_eq!(tail_text_lines("one\ntwo\nthree", 2), "two\nthree");
        assert_eq!(tail_text_lines("one\ntwo", 10), "one\ntwo");
        assert_eq!(tail_text_lines("one\ntwo", 0), "");
    }
}
