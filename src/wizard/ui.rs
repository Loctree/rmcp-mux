//! UI drawing functions for the wizard TUI (5-step flow).

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

use crate::scan::HostKind;

use super::types::{
    AppState, ServiceSource, SourceStatus, Strategy, SummaryAction, TrayChoice, WizardStep,
};

// ─────────────────────────────────────────────────────────────────────────────
// Top-level draw
// ─────────────────────────────────────────────────────────────────────────────

pub fn draw_ui(f: &mut Frame, app: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3), // Title bar
            Constraint::Min(10),   // Body
            Constraint::Length(3), // Status bar
        ])
        .split(f.area());

    // Title bar
    let step_info = match app.wizard_step {
        WizardStep::DiscoverySources => "Step 1/5: Discovery Sources",
        WizardStep::ServerReview => "Step 2/5: Server Review",
        WizardStep::StrategyChoice => "Step 3/5: Strategy Choice",
        WizardStep::SummaryConfirm => "Step 4/5: Summary & Confirm",
        WizardStep::ResultAndTray => "Step 5/5: Result & Tray Daemon",
    };
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            "rmcp-mux wizard",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" — "),
        Span::styled(step_info, Style::default().fg(Color::Cyan)),
    ]));
    f.render_widget(title, chunks[0]);

    // Body
    let body = chunks[1];
    match app.wizard_step {
        WizardStep::DiscoverySources => draw_step1_sources(f, app, body),
        WizardStep::ServerReview => draw_step2_review(f, app, body),
        WizardStep::StrategyChoice => draw_step3_strategy(f, app, body),
        WizardStep::SummaryConfirm => draw_step4_summary(f, app, body),
        WizardStep::ResultAndTray => draw_step5_result(f, app, body),
    }

    // Status bar
    let mut footer_spans = vec![Span::raw(&app.message)];
    if app.dry_run {
        footer_spans.push(Span::styled(
            " | DRY-RUN",
            Style::default().fg(Color::Yellow),
        ));
    }
    let status = Paragraph::new(Line::from(footer_spans))
        .wrap(Wrap { trim: true })
        .block(Block::default().borders(Borders::ALL).title("Status"));
    f.render_widget(status, chunks[2]);
}

// ─────────────────────────────────────────────────────────────────────────────
// STEP 1: Discovery sources
// ─────────────────────────────────────────────────────────────────────────────

fn draw_step1_sources(f: &mut Frame, app: &AppState, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    // Left: list of candidate sources.
    let selected_count = app.sources.iter().filter(|s| s.selected).count();
    let total_count = app.sources.len();
    let title = format!("Sources [{}/{}]", selected_count, total_count);

    let items: Vec<ListItem> = app
        .sources
        .iter()
        .enumerate()
        .map(|(i, src)| {
            let checkbox = if src.selected {
                Span::styled("[x] ", Style::default().fg(Color::Green))
            } else {
                Span::styled("[ ] ", Style::default().fg(Color::DarkGray))
            };
            let kind_tag = Span::styled(
                format!("[{:<14}]", src.host_file.kind.display_name()),
                Style::default().fg(kind_color(src.host_file.kind)),
            );
            let path_span = Span::raw(src.host_file.path.display().to_string());
            let status_color = match src.status {
                SourceStatus::Ok { .. } => Color::Green,
                SourceStatus::Empty => Color::DarkGray,
                SourceStatus::InvalidFormat { .. } => Color::Red,
                SourceStatus::Missing => Color::DarkGray,
            };
            let status_span = Span::styled(
                format!("  {}", src.status.short_label()),
                Style::default().fg(status_color),
            );
            let highlight = if i == app.selected_source {
                Span::styled(" ▶ ", Style::default().fg(Color::Yellow))
            } else {
                Span::raw("   ")
            };

            ListItem::new(Line::from(vec![
                highlight,
                checkbox,
                kind_tag,
                Span::raw(" "),
                path_span,
                status_span,
            ]))
        })
        .collect();
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(title),
    );
    f.render_widget(list, columns[0]);

    // Right: custom-path input + key hints.
    let mut lines = vec![
        Line::from(Span::styled(
            "Add custom path",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    let buf_style = if app.custom_path.editing {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let buf_display = if app.custom_path.buffer.is_empty() {
        "<empty>".to_string()
    } else {
        app.custom_path.buffer.clone()
    };
    lines.push(Line::from(vec![
        Span::raw("> "),
        Span::styled(buf_display, buf_style),
        if app.custom_path.editing {
            Span::styled("_", Style::default().fg(Color::Yellow))
        } else {
            Span::raw("")
        },
    ]));
    if let Some(status) = &app.custom_path.status {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            status.clone(),
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Keys",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from("  Up/Down  navigate sources"));
    lines.push(Line::from("  Space    toggle selection"));
    lines.push(Line::from("  i        edit custom path"));
    lines.push(Line::from("  Enter    add custom path"));
    lines.push(Line::from("  n        next step"));
    lines.push(Line::from("  q        quit"));

    let panel = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title("Custom path"),
        )
        .wrap(Wrap { trim: true });
    f.render_widget(panel, columns[1]);
}

// ─────────────────────────────────────────────────────────────────────────────
// STEP 2: Server review
// ─────────────────────────────────────────────────────────────────────────────

fn draw_step2_review(f: &mut Frame, app: &AppState, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);

    // Left: per-client tree of services with checkboxes.
    let mut items: Vec<ListItem> = Vec::new();
    let mut current_kind: Option<ServiceSource> = None;
    for (idx, svc) in app.services.iter().enumerate() {
        // Group separator when the source kind changes.
        let same_group = match (&current_kind, &svc.source) {
            (Some(a), b) => a == b,
            (None, _) => false,
        };
        if !same_group {
            current_kind = Some(svc.source.clone());
            items.push(ListItem::new(Line::from(vec![Span::styled(
                format!("─ {} ", svc.source.short_label()),
                Style::default().fg(Color::DarkGray),
            )])));
        }
        let checkbox = if svc.selected {
            Span::styled("[x] ", Style::default().fg(Color::Green))
        } else {
            Span::styled("[ ] ", Style::default().fg(Color::DarkGray))
        };
        let highlight = if idx == app.selected_service {
            Span::styled("▶ ", Style::default().fg(Color::Yellow))
        } else {
            Span::raw("  ")
        };
        let kind_color = match &svc.source {
            ServiceSource::Client { kind, .. } => kind_color_value(*kind),
            ServiceSource::Default { .. } => Color::Cyan,
            ServiceSource::DetectedRunning => Color::Magenta,
        };
        let pid_span = match svc.pid {
            Some(pid) => Span::styled(
                format!(" (pid {pid})"),
                Style::default().fg(Color::DarkGray),
            ),
            None => Span::raw(""),
        };
        items.push(ListItem::new(Line::from(vec![
            highlight,
            checkbox,
            Span::styled(svc.name.clone(), Style::default().fg(kind_color)),
            pid_span,
        ])));
    }
    if items.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "No servers discovered. Go back (p) and add sources.",
            Style::default().fg(Color::Yellow),
        ))));
    }
    let selected_count = app.services.iter().filter(|s| s.selected).count();
    let title = format!("Servers [{}/{}]", selected_count, app.services.len());
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(title),
    );
    f.render_widget(list, columns[0]);

    // Right: summary + dedup + key hints.
    let unique_names: std::collections::HashSet<&str> =
        app.services.iter().map(|s| s.name.as_str()).collect();
    let mut lines = vec![
        Line::from(Span::styled(
            "Summary",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!("  Total entries  : {}", app.services.len())),
        Line::from(format!("  Unique names   : {}", unique_names.len())),
        Line::from(format!(
            "  Sources scanned: {}",
            app.sources.iter().filter(|s| s.selected).count()
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Keys",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("  Up/Down  navigate"),
        Line::from("  Space    toggle selection"),
        Line::from("  n        next step"),
        Line::from("  p        previous step"),
        Line::from("  q        quit"),
    ];

    if app.services.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "No services discovered from selected sources.",
            Style::default().fg(Color::Yellow),
        )));
    }

    let panel = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title("Review"),
        )
        .wrap(Wrap { trim: true });
    f.render_widget(panel, columns[1]);
}

// ─────────────────────────────────────────────────────────────────────────────
// STEP 3: Strategy
// ─────────────────────────────────────────────────────────────────────────────

fn draw_step3_strategy(f: &mut Frame, app: &AppState, area: Rect) {
    let choices = [
        (
            Strategy::Unified,
            "Unified config",
            "Write one ~/.config/mux/{config.toml,mcp.json,mcp.toml} with every selected server. Recommended.",
        ),
        (
            Strategy::PerClient,
            "Per-client configs",
            "Write a separate file per client kind (claude.json, codex.toml, junie.json, ...) under ~/.config/mux/.",
        ),
        (
            Strategy::AutoRewire,
            "[DANGER] Auto-rewire existing client configs",
            "Backup-first preview-first rewrite of your real client configs to route through rmcp-mux-proxy.",
        ),
    ];

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            "How do you want to use mux?",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    for (idx, (choice, label, description)) in choices.iter().enumerate() {
        let is_selected = *choice == app.strategy;
        let marker = if is_selected { "(•)" } else { "( )" };
        let label_style = if is_selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let danger = matches!(choice, Strategy::AutoRewire);
        let label_color = if danger {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            label_style
        };
        lines.push(Line::from(vec![
            Span::raw(format!("  {marker} ")),
            Span::styled(
                format!("{}. ", idx + 1),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(label.to_string(), label_color),
        ]));
        lines.push(Line::from(vec![
            Span::raw("       "),
            Span::styled(
                description.to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        "Up/Down to choose, Enter or n to continue, p to go back, q to quit.",
        Style::default().fg(Color::DarkGray),
    )));

    let panel = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title("Strategy"),
        )
        .wrap(Wrap { trim: true });
    f.render_widget(panel, area);
}

// ─────────────────────────────────────────────────────────────────────────────
// STEP 4: Summary + confirm
// ─────────────────────────────────────────────────────────────────────────────

fn draw_step4_summary(f: &mut Frame, app: &AppState, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(area);

    let strategy_label = match app.strategy {
        Strategy::Unified => "Unified",
        Strategy::PerClient => "Per-client configs",
        Strategy::AutoRewire => "[DANGER] Auto-rewire client configs",
    };
    let mux_dir = crate::mux_gen::default_mux_dir();
    let socket_dir = crate::mux_gen::default_socket_dir(&mux_dir);
    let selected_services: Vec<&str> = app
        .services
        .iter()
        .filter(|s| s.selected)
        .map(|s| s.name.as_str())
        .collect();

    let mut left = vec![
        Line::from(Span::styled(
            "About to:",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!("  Strategy : {strategy_label}")),
    ];

    match app.strategy {
        Strategy::Unified => {
            left.push(Line::from(format!(
                "  Outputs  : {}/config.toml",
                mux_dir.display()
            )));
            left.push(Line::from(format!(
                "             {}/mcp.json",
                mux_dir.display()
            )));
            left.push(Line::from(format!(
                "             {}/mcp.toml",
                mux_dir.display()
            )));
        }
        Strategy::PerClient => {
            left.push(Line::from(format!(
                "  Outputs  : {}/config.toml (daemon truth)",
                mux_dir.display()
            )));
            // Predict per-client filenames from the selected STEP 2 services,
            // matching what persist::selected_scans will actually write.
            for kind in selected_per_client_output_kinds(app) {
                let ext = match kind {
                    HostKind::Codex => "toml",
                    _ => "json",
                };
                left.push(Line::from(format!(
                    "             {}/{}.{}",
                    mux_dir.display(),
                    kind.as_label(),
                    ext
                )));
            }
        }
        Strategy::AutoRewire => {
            left.push(Line::from(""));
            left.push(Line::from(Span::styled(
                "  Will rewrite (with .bak per file):",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )));
            for src in app.sources.iter().filter(|s| {
                s.selected
                    && matches!(s.status, SourceStatus::Ok { .. })
                    && s.host_file.eligible_for_danger
            }) {
                left.push(Line::from(format!(
                    "    • {}",
                    src.host_file.path.display()
                )));
            }
            let ineligible: Vec<&_> = app
                .sources
                .iter()
                .filter(|s| {
                    s.selected
                        && matches!(s.status, SourceStatus::Ok { .. })
                        && !s.host_file.eligible_for_danger
                })
                .collect();
            if !ineligible.is_empty() {
                left.push(Line::from(""));
                left.push(Line::from(Span::styled(
                    "  Skipped (no strict-config flag for danger flow):",
                    Style::default().fg(Color::DarkGray),
                )));
                for src in ineligible {
                    left.push(Line::from(format!(
                        "    · {} ({})",
                        src.host_file.path.display(),
                        src.host_file.kind.display_name(),
                    )));
                }
            }
        }
    }

    left.push(Line::from(""));
    left.push(Line::from(format!("  Sockets  : {}", socket_dir.display())));
    left.push(Line::from(format!(
        "  Servers  : {} selected",
        selected_services.len()
    )));
    if app.dry_run {
        left.push(Line::from(""));
        left.push(Line::from(Span::styled(
            "  DRY-RUN: no files will be modified.",
            Style::default().fg(Color::Yellow),
        )));
    }

    let body = Paragraph::new(left)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title("Summary"),
        )
        .wrap(Wrap { trim: true });
    f.render_widget(body, columns[0]);

    // Right: action chooser.
    let actions = [
        (SummaryAction::Confirm, "Confirm", Color::Green),
        (SummaryAction::Back, "Back", Color::Cyan),
        (SummaryAction::Cancel, "Cancel", Color::Red),
    ];
    let mut right = vec![
        Line::from(Span::styled(
            "Choose action",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    for (action, label, color) in actions {
        let is_selected = action == app.summary_action;
        let marker = if is_selected { "▶" } else { " " };
        let style = if is_selected {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(color)
        };
        right.push(Line::from(vec![
            Span::raw(format!("  {marker} ")),
            Span::styled(label.to_string(), style),
        ]));
    }
    right.push(Line::from(""));
    right.push(Line::from(Span::styled(
        "Up/Down: choose, Enter: do it",
        Style::default().fg(Color::DarkGray),
    )));

    let panel = Paragraph::new(right)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title("Action"),
        )
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: true });
    f.render_widget(panel, columns[1]);
}

// ─────────────────────────────────────────────────────────────────────────────
// STEP 5: Result + tray prompt
// ─────────────────────────────────────────────────────────────────────────────

fn draw_step5_result(f: &mut Frame, app: &AppState, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(area);

    let mut left = vec![Line::from(Span::styled(
        "Result",
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    ))];
    left.push(Line::from(""));
    if let Some(result) = &app.strategy_result {
        for line in result.lines() {
            left.push(Line::from(line.to_string()));
        }
    } else {
        left.push(Line::from(Span::styled(
            "(no result captured — see status bar)",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let body = Paragraph::new(left)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title("Result"),
        )
        .wrap(Wrap { trim: true });
    f.render_widget(body, columns[0]);

    // Right: tray daemon prompt.
    let actions = [
        (TrayChoice::StartNow, "Start tray daemon now", Color::Green),
        (TrayChoice::No, "No, exit", Color::DarkGray),
    ];
    let mut right = vec![
        Line::from(Span::styled(
            "Tray daemon",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Run a multi-service tray monitor"),
        Line::from("for the sockets you just configured?"),
        Line::from(""),
    ];
    for (action, label, color) in actions {
        let is_selected = action == app.tray_choice;
        let marker = if is_selected { "▶" } else { " " };
        let style = if is_selected {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(color)
        };
        right.push(Line::from(vec![
            Span::raw(format!("  {marker} ")),
            Span::styled(label.to_string(), style),
        ]));
    }
    right.push(Line::from(""));
    right.push(Line::from(Span::styled(
        "Up/Down: choose, Enter: confirm",
        Style::default().fg(Color::DarkGray),
    )));

    let panel = Paragraph::new(right)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title("Action"),
        )
        .wrap(Wrap { trim: true });
    f.render_widget(panel, columns[1]);
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn kind_color(kind: HostKind) -> Color {
    kind_color_value(kind)
}

fn kind_color_value(kind: HostKind) -> Color {
    match kind {
        HostKind::Claude | HostKind::ClaudeDesktop => Color::Yellow,
        HostKind::Codex => Color::Blue,
        HostKind::Junie => Color::Green,
        HostKind::Gemini => Color::Red,
        HostKind::Cursor => Color::Magenta,
        HostKind::VSCode => Color::Cyan,
        HostKind::JetBrains => Color::Green,
        HostKind::Custom | HostKind::Unknown => Color::DarkGray,
    }
}

fn selected_per_client_output_kinds(app: &AppState) -> Vec<HostKind> {
    let mut kinds: Vec<HostKind> = app
        .services
        .iter()
        .filter(|svc| svc.selected)
        .filter_map(|svc| match &svc.source {
            ServiceSource::Client { kind, .. } => Some(*kind),
            ServiceSource::Default { .. } => None,
            ServiceSource::DetectedRunning => None,
        })
        .collect();
    kinds.sort_by_key(|k| k.as_label());
    kinds.dedup();
    kinds
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;
    use crate::scan::{Confidence, ConfigSchema, HostFile, HostFormat};
    use crate::wizard::types::{
        CustomPathInput, HealthStatus, ServiceEntry, SourceEntry, Strategy, SummaryAction,
        TrayChoice,
    };
    use std::path::PathBuf;

    fn host(kind: HostKind, path: &str) -> HostFile {
        HostFile {
            kind,
            path: PathBuf::from(path),
            format: HostFormat::Json,
            schema: ConfigSchema::McpServersJson,
            confidence: Confidence::High,
            writable: true,
            eligible_for_danger: true,
        }
    }

    fn service(name: &str, source: ServiceSource, selected: bool) -> ServiceEntry {
        ServiceEntry {
            name: name.into(),
            config: ServerConfig {
                socket: None,
                cmd: Some("npx".into()),
                args: Some(vec!["@modelcontextprotocol/server-memory".into()]),
                cwd: None,
                env: None,
                max_active_clients: Some(5),
                tray: Some(false),
                service_name: Some(name.into()),
                log_level: Some("info".into()),
                lazy_start: Some(false),
                max_request_bytes: Some(1_048_576),
                request_timeout_ms: Some(30_000),
                restart_backoff_ms: Some(1_000),
                restart_backoff_max_ms: Some(30_000),
                max_restarts: Some(5),
                status_file: None,
                heartbeat_interval_ms: Some(30_000),
                heartbeat_timeout_ms: Some(30_000),
                heartbeat_max_failures: Some(3),
                heartbeat_enabled: Some(false),
            },
            health: HealthStatus::Unknown,
            source,
            pid: None,
            selected,
        }
    }

    fn app_for_summary() -> AppState {
        AppState {
            wizard_step: WizardStep::SummaryConfirm,
            config_path: PathBuf::from("/tmp/mux.toml"),
            sources: vec![
                SourceEntry {
                    host_file: host(HostKind::Claude, "/tmp/claude.json"),
                    status: SourceStatus::Ok { servers_found: 1 },
                    selected: true,
                },
                SourceEntry {
                    host_file: host(HostKind::Codex, "/tmp/codex.toml"),
                    status: SourceStatus::Ok { servers_found: 1 },
                    selected: true,
                },
            ],
            selected_source: 0,
            custom_path: CustomPathInput::default(),
            services: vec![
                service(
                    "memory",
                    ServiceSource::Client {
                        kind: HostKind::Claude,
                        path: PathBuf::from("/tmp/claude.json"),
                    },
                    true,
                ),
                service(
                    "brave",
                    ServiceSource::Client {
                        kind: HostKind::Codex,
                        path: PathBuf::from("/tmp/codex.toml"),
                    },
                    false,
                ),
                service("running-only", ServiceSource::DetectedRunning, true),
            ],
            selected_service: 0,
            strategy: Strategy::PerClient,
            summary_action: SummaryAction::Confirm,
            tray_choice: TrayChoice::No,
            message: String::new(),
            dry_run: true,
            pending_action: None,
            strategy_result: None,
        }
    }

    #[test]
    fn per_client_summary_kinds_follow_selected_services() {
        let app = app_for_summary();

        assert_eq!(
            selected_per_client_output_kinds(&app),
            vec![HostKind::Claude]
        );
    }
}
