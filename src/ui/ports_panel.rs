//! Ports panel UI for attach.

use super::theme::ThemePalette;
use super::{panel_block, trim_label};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

#[derive(Debug, Clone)]
pub(crate) struct PortListRow {
    pub(crate) port: u16,
    pub(crate) host: String,
    pub(crate) process: Option<String>,
    pub(crate) pane: Option<String>,
    pub(crate) workspace: Option<String>,
    pub(crate) forward_url: Option<String>,
}

pub(crate) fn draw_ports(
    frame: &mut ratatui::Frame,
    area: Rect,
    ports: &[PortListRow],
    selected: usize,
    action_error: Option<&str>,
    palette: ThemePalette,
) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            " 🔌  Ports ",
            Style::default()
                .fg(palette.active)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if ports.is_empty() {
                "· none detected".to_string()
            } else {
                format!("· {} listening", ports.len())
            },
            Style::default().fg(palette.muted),
        ),
    ]));
    lines.push(Line::from(Span::raw("")));
    if ports.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No pane-owned listeners right now",
            Style::default().fg(palette.muted),
        )));
        lines.push(Line::from(Span::styled(
            "  Start a dev server in a pane, then press r to rescan",
            Style::default().fg(palette.muted),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!(
                "  {:<6} {:<14} {:<10} {:<12} {}",
                "PORT", "PROCESS", "PANE", "WORKSPACE", "FORWARD"
            ),
            Style::default().fg(palette.muted),
        )));
        for (index, row) in ports.iter().enumerate() {
            let process = row.process.as_deref().unwrap_or("-");
            let pane = row.pane.as_deref().unwrap_or("-");
            let workspace = row.workspace.as_deref().unwrap_or("-");
            let forward = row.forward_url.as_deref().unwrap_or("-");
            let text = format!(
                "  {:<6} {:<14} {:<10} {:<12} {}",
                row.port,
                trim_label(process, 14),
                trim_label(pane, 10),
                trim_label(workspace, 12),
                trim_label(forward, 40)
            );
            let style = if index == selected {
                Style::default()
                    .fg(palette.background)
                    .bg(palette.active)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.text)
            };
            lines.push(Line::from(Span::styled(text, style)));
            if index == selected {
                lines.push(Line::from(Span::styled(
                    format!("     {} · host {}", row.host, "ssh -L ready with c"),
                    Style::default().fg(palette.muted),
                )));
            }
        }
    }
    lines.push(Line::from(Span::raw("")));
    lines.push(Line::from(vec![
        Span::styled(
            " j/k",
            Style::default()
                .fg(palette.active)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" select  ", Style::default().fg(palette.muted)),
        Span::styled(
            "Enter",
            Style::default()
                .fg(palette.active)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" focus pane  ", Style::default().fg(palette.muted)),
        Span::styled(
            "c",
            Style::default()
                .fg(palette.active)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" copy ssh -L  ", Style::default().fg(palette.muted)),
        Span::styled(
            "f",
            Style::default()
                .fg(palette.active)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" forward  ", Style::default().fg(palette.muted)),
        Span::styled(
            "x",
            Style::default()
                .fg(palette.active)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" unforward  ", Style::default().fg(palette.muted)),
        Span::styled(
            "r",
            Style::default()
                .fg(palette.active)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" rescan  ", Style::default().fg(palette.muted)),
        Span::styled(
            "Esc",
            Style::default()
                .fg(palette.active)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" close", Style::default().fg(palette.muted)),
    ]));
    if let Some(err) = action_error {
        lines.push(Line::from(Span::styled(
            format!("  {err}"),
            Style::default().fg(palette.warning),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel_block(" ports ", palette))
            .style(Style::default().fg(palette.text).bg(palette.surface))
            .wrap(Wrap { trim: false }),
        area,
    );
}
