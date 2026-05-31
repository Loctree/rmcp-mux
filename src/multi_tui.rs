//! Multi-server TUI dashboard using ratatui.
//!
//! Provides a terminal user interface for monitoring and controlling
//! multiple MCP servers running in a single process.

use std::collections::HashMap;
use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use tokio::sync::{mpsc, watch};

use crate::multi::{MultiServerStatus, ServerCommand, StatusLevel, format_uptime};

/// TUI application state.
pub struct MultiTuiApp {
    /// Current server statuses
    statuses: Vec<MultiServerStatus>,
    /// Table selection state
    table_state: TableState,
    /// Whether to quit the app
    should_quit: bool,
}

impl MultiTuiApp {
    /// Create a new TUI app.
    pub fn new() -> Self {
        Self {
            statuses: Vec::new(),
            table_state: TableState::default(),
            should_quit: false,
        }
    }

    /// Update statuses from the watch channel.
    pub fn update_statuses(&mut self, statuses: HashMap<String, MultiServerStatus>) {
        // Sort by name for consistent display
        let mut list: Vec<_> = statuses.into_values().collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        self.statuses = list;

        // Ensure selection is valid
        if !self.statuses.is_empty() {
            let selected = self.table_state.selected().unwrap_or(0);
            if selected >= self.statuses.len() {
                self.table_state.select(Some(self.statuses.len() - 1));
            } else if self.table_state.selected().is_none() {
                self.table_state.select(Some(0));
            }
        }
    }

    /// Move selection up.
    pub fn select_previous(&mut self) {
        if self.statuses.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) => {
                if i == 0 {
                    self.statuses.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    /// Move selection down.
    pub fn select_next(&mut self) {
        if self.statuses.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) => {
                if i >= self.statuses.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    /// Get the currently selected server name.
    pub fn selected_server(&self) -> Option<&str> {
        self.table_state
            .selected()
            .and_then(|i| self.statuses.get(i))
            .map(|s| s.name.as_str())
    }

    /// Request quit.
    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    /// Check if should quit.
    pub fn should_quit(&self) -> bool {
        self.should_quit
    }
}

impl Default for MultiTuiApp {
    fn default() -> Self {
        Self::new()
    }
}

/// Get color for status level.
fn status_color(level: &StatusLevel) -> Color {
    match level {
        StatusLevel::Ok => Color::Green,
        StatusLevel::Warn => Color::Yellow,
        StatusLevel::Error => Color::Red,
        StatusLevel::Lazy => Color::DarkGray,
    }
}

/// Render the TUI.
fn render(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut MultiTuiApp,
) -> io::Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();

        // Layout: header, table, footer
        let chunks = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(area);

        // Header
        let header = Paragraph::new(Line::from(vec![
            Span::styled(
                " rmcp-mux ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Multi-Server Dashboard"),
        ]))
        .block(Block::default().borders(Borders::ALL));
        frame.render_widget(header, chunks[0]);

        // Table
        let header_cells = ["Name", "Status", "Clients", "Pending", "Restarts", "Uptime"]
            .iter()
            .map(|h| Cell::from(*h).style(Style::default().add_modifier(Modifier::BOLD)));
        let header_row = Row::new(header_cells).height(1);

        let rows = app.statuses.iter().map(|status| {
            let color = status_color(&status.level);
            let clients = format!("{}/{}", status.active_clients, status.max_active_clients);
            let uptime = format_uptime(status.uptime_ms);

            Row::new(vec![
                Cell::from(status.name.clone()),
                Cell::from(status.status_text.clone()).style(Style::default().fg(color)),
                Cell::from(clients),
                Cell::from(status.pending_requests.to_string()),
                Cell::from(status.restarts.to_string()),
                Cell::from(uptime),
            ])
            .height(1)
        });

        let table = Table::new(
            rows,
            [
                Constraint::Min(20),
                Constraint::Min(15),
                Constraint::Min(10),
                Constraint::Min(10),
                Constraint::Min(10),
                Constraint::Min(12),
            ],
        )
        .header(header_row)
        .block(Block::default().borders(Borders::ALL).title(" Servers "))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

        frame.render_stateful_widget(table, chunks[1], &mut app.table_state);

        // Footer with keybindings
        let footer = Paragraph::new(Line::from(vec![
            Span::styled(" j/k ", Style::default().fg(Color::Yellow)),
            Span::raw("navigate  "),
            Span::styled(" r ", Style::default().fg(Color::Yellow)),
            Span::raw("restart  "),
            Span::styled(" s ", Style::default().fg(Color::Yellow)),
            Span::raw("stop  "),
            Span::styled(" S ", Style::default().fg(Color::Yellow)),
            Span::raw("start  "),
            Span::styled(" q ", Style::default().fg(Color::Yellow)),
            Span::raw("quit"),
        ]))
        .block(Block::default().borders(Borders::ALL));
        frame.render_widget(footer, chunks[2]);
    })?;

    Ok(())
}

/// Run the multi-server TUI.
///
/// # Arguments
/// * `status_rx` - Watch receiver for server status updates
/// * `command_tx` - Channel to send commands to servers
///
/// # Returns
/// Returns when the user quits or an error occurs.
pub async fn run_multi_tui(
    mut status_rx: watch::Receiver<HashMap<String, MultiServerStatus>>,
    command_tx: mpsc::UnboundedSender<(String, ServerCommand)>,
) -> io::Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = MultiTuiApp::new();

    // Initial status update
    app.update_statuses(status_rx.borrow().clone());

    loop {
        // Render
        render(&mut terminal, &mut app)?;

        // Handle events with timeout
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    app.quit();
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    app.select_next();
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    app.select_previous();
                }
                KeyCode::Char('r') => {
                    if let Some(name) = app.selected_server() {
                        let _ = command_tx.send((name.to_string(), ServerCommand::Restart));
                    }
                }
                KeyCode::Char('s') => {
                    if let Some(name) = app.selected_server() {
                        let _ = command_tx.send((name.to_string(), ServerCommand::Stop));
                    }
                }
                KeyCode::Char('S') => {
                    if let Some(name) = app.selected_server() {
                        let _ = command_tx.send((name.to_string(), ServerCommand::Start));
                    }
                }
                _ => {}
            }
        }

        // Check for status updates
        if status_rx.has_changed().unwrap_or(false) {
            app.update_statuses(status_rx.borrow_and_update().clone());
        }

        // Check if should quit
        if app.should_quit() {
            break;
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_status(name: &str, level: StatusLevel) -> MultiServerStatus {
        MultiServerStatus {
            name: name.to_string(),
            level,
            status_text: "Running".to_string(),
            connected_clients: 2,
            active_clients: 1,
            max_active_clients: 5,
            pending_requests: 0,
            restarts: 0,
            uptime_ms: 60000,
            in_backoff: false,
            heartbeat_latency_ms: None,
        }
    }

    #[test]
    fn app_updates_statuses() {
        let mut app = MultiTuiApp::new();
        let mut statuses = HashMap::new();
        statuses.insert(
            "server1".to_string(),
            test_status("server1", StatusLevel::Ok),
        );
        statuses.insert(
            "server2".to_string(),
            test_status("server2", StatusLevel::Warn),
        );

        app.update_statuses(statuses);

        assert_eq!(app.statuses.len(), 2);
        assert_eq!(app.table_state.selected(), Some(0));
    }

    #[test]
    fn app_navigation() {
        let mut app = MultiTuiApp::new();
        let mut statuses = HashMap::new();
        statuses.insert("a".to_string(), test_status("a", StatusLevel::Ok));
        statuses.insert("b".to_string(), test_status("b", StatusLevel::Ok));
        statuses.insert("c".to_string(), test_status("c", StatusLevel::Ok));
        app.update_statuses(statuses);

        assert_eq!(app.table_state.selected(), Some(0));

        app.select_next();
        assert_eq!(app.table_state.selected(), Some(1));

        app.select_next();
        assert_eq!(app.table_state.selected(), Some(2));

        app.select_next(); // Wrap around
        assert_eq!(app.table_state.selected(), Some(0));

        app.select_previous(); // Wrap around back
        assert_eq!(app.table_state.selected(), Some(2));
    }

    #[test]
    fn app_selected_server() {
        let mut app = MultiTuiApp::new();
        let mut statuses = HashMap::new();
        statuses.insert("alpha".to_string(), test_status("alpha", StatusLevel::Ok));
        statuses.insert("beta".to_string(), test_status("beta", StatusLevel::Ok));
        app.update_statuses(statuses);

        // Sorted alphabetically, so alpha is first
        assert_eq!(app.selected_server(), Some("alpha"));

        app.select_next();
        assert_eq!(app.selected_server(), Some("beta"));
    }

    #[test]
    fn status_colors() {
        assert_eq!(status_color(&StatusLevel::Ok), Color::Green);
        assert_eq!(status_color(&StatusLevel::Warn), Color::Yellow);
        assert_eq!(status_color(&StatusLevel::Error), Color::Red);
        assert_eq!(status_color(&StatusLevel::Lazy), Color::DarkGray);
    }

    #[test]
    fn app_quit() {
        let mut app = MultiTuiApp::new();
        assert!(!app.should_quit());
        app.quit();
        assert!(app.should_quit());
    }
}
