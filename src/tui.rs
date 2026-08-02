use std::io;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::app_server::CodexAppServer;
use crate::config::{Config, SummaryInput, SummaryProvider};
use crate::db::{Database, Session};
use crate::paths::AppPaths;
use crate::sync;
use crate::terminal;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditField {
    Title,
    Summary,
    Notes,
    Tags,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Confirmation {
    SummarizeSelected,
    SummarizeAll,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    Normal,
    Search,
    Edit { field: EditField, buffer: String },
    Confirm(Confirmation),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UiAction {
    None,
    Quit,
    Launch {
        thread_id: String,
    },
    Sync,
    Summarize {
        thread_id: String,
    },
    SummarizeAll,
    TogglePin {
        thread_id: String,
    },
    SaveEdit {
        thread_id: String,
        field: EditField,
        value: String,
    },
    SavePreferences {
        provider: SummaryProvider,
        cap: SummaryInput,
    },
}

#[derive(Clone, Debug)]
pub struct TrackerApp {
    pub sessions: Vec<Session>,
    pub filtered: Vec<usize>,
    pub selected: usize,
    pub query: String,
    pub mode: Mode,
    pub status: String,
    pub provider: SummaryProvider,
    pub cap: SummaryInput,
}

impl TrackerApp {
    pub fn new(sessions: Vec<Session>, config: &Config) -> Self {
        let mut app = Self {
            sessions,
            filtered: Vec::new(),
            selected: 0,
            query: String::new(),
            mode: Mode::Normal,
            status: "Ready".into(),
            provider: config.summary_provider,
            cap: config.summary_input,
        };
        app.refresh_filter();
        app
    }

    pub fn selected_session(&self) -> Option<&Session> {
        self.filtered
            .get(self.selected)
            .and_then(|index| self.sessions.get(*index))
    }

    pub fn refresh_filter(&mut self) {
        let query = self.query.trim().to_lowercase();
        self.filtered = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| query.is_empty() || session.searchable_text().contains(&query))
            .map(|(index, _)| index)
            .collect();
        if self.filtered.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.filtered.len() - 1);
        }
    }

    pub fn replace_sessions(&mut self, sessions: Vec<Session>, preferred: Option<&str>) {
        self.sessions = sessions;
        self.refresh_filter();
        if let Some(thread_id) = preferred {
            if let Some(position) = self.filtered.iter().position(|index| {
                self.sessions
                    .get(*index)
                    .is_some_and(|session| session.thread_id == thread_id)
            }) {
                self.selected = position;
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> UiAction {
        match self.mode.clone() {
            Mode::Search => self.handle_search_key(key),
            Mode::Edit { field, buffer } => self.handle_edit_key(key, field, buffer),
            Mode::Confirm(confirmation) => self.handle_confirm_key(key, confirmation),
            Mode::Normal => self.handle_normal_key(key),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> UiAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return UiAction::Quit;
        }
        match key.code {
            KeyCode::Char('q') => UiAction::Quit,
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.filtered.is_empty() {
                    self.selected = (self.selected + 1).min(self.filtered.len() - 1);
                }
                UiAction::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                UiAction::None
            }
            KeyCode::Home => {
                self.selected = 0;
                UiAction::None
            }
            KeyCode::End => {
                self.selected = self.filtered.len().saturating_sub(1);
                UiAction::None
            }
            KeyCode::Char('/') => {
                self.mode = Mode::Search;
                UiAction::None
            }
            KeyCode::Esc if !self.query.is_empty() => {
                self.query.clear();
                self.refresh_filter();
                UiAction::None
            }
            KeyCode::Enter => self
                .selected_session()
                .map(|session| UiAction::Launch {
                    thread_id: session.thread_id.clone(),
                })
                .unwrap_or(UiAction::None),
            KeyCode::Char('s') => UiAction::Sync,
            KeyCode::Char('p') => self
                .selected_session()
                .map(|session| UiAction::TogglePin {
                    thread_id: session.thread_id.clone(),
                })
                .unwrap_or(UiAction::None),
            KeyCode::Char('r') => {
                if self.selected_session().is_none() {
                    UiAction::None
                } else if self.provider.is_external() {
                    self.mode = Mode::Confirm(Confirmation::SummarizeSelected);
                    UiAction::None
                } else {
                    UiAction::Summarize {
                        thread_id: self.selected_session().unwrap().thread_id.clone(),
                    }
                }
            }
            KeyCode::Char('A') => {
                self.mode = Mode::Confirm(Confirmation::SummarizeAll);
                UiAction::None
            }
            KeyCode::Char('t') => self.begin_edit(EditField::Title),
            KeyCode::Char('y') => self.begin_edit(EditField::Summary),
            KeyCode::Char('n') => self.begin_edit(EditField::Notes),
            KeyCode::Char('g') => self.begin_edit(EditField::Tags),
            KeyCode::Char('v') => {
                let position = SummaryProvider::ALL
                    .iter()
                    .position(|provider| *provider == self.provider)
                    .unwrap_or(0);
                self.provider = SummaryProvider::ALL[(position + 1) % SummaryProvider::ALL.len()];
                UiAction::SavePreferences {
                    provider: self.provider,
                    cap: self.cap,
                }
            }
            KeyCode::Char('c') => {
                let position = SummaryInput::CYCLE
                    .iter()
                    .position(|cap| *cap == self.cap)
                    .unwrap_or(0);
                self.cap = SummaryInput::CYCLE[(position + 1) % SummaryInput::CYCLE.len()];
                UiAction::SavePreferences {
                    provider: self.provider,
                    cap: self.cap,
                }
            }
            _ => UiAction::None,
        }
    }

    fn begin_edit(&mut self, field: EditField) -> UiAction {
        let Some(session) = self.selected_session() else {
            return UiAction::None;
        };
        let buffer = match field {
            EditField::Title => session
                .manual_title
                .as_deref()
                .unwrap_or(session.title())
                .to_owned(),
            EditField::Summary => session
                .manual_summary
                .as_deref()
                .unwrap_or(session.summary())
                .to_owned(),
            EditField::Notes => session.notes.clone(),
            EditField::Tags => session.tags.join(", "),
        };
        self.mode = Mode::Edit { field, buffer };
        UiAction::None
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> UiAction {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                self.query.pop();
                self.refresh_filter();
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.query.push(character);
                self.refresh_filter();
            }
            _ => {}
        }
        UiAction::None
    }

    fn handle_edit_key(&mut self, key: KeyEvent, field: EditField, mut buffer: String) -> UiAction {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                UiAction::None
            }
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                self.selected_session()
                    .map(|session| UiAction::SaveEdit {
                        thread_id: session.thread_id.clone(),
                        field,
                        value: buffer,
                    })
                    .unwrap_or(UiAction::None)
            }
            KeyCode::Backspace => {
                buffer.pop();
                self.mode = Mode::Edit { field, buffer };
                UiAction::None
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                buffer.push(character);
                self.mode = Mode::Edit { field, buffer };
                UiAction::None
            }
            _ => UiAction::None,
        }
    }

    fn handle_confirm_key(&mut self, key: KeyEvent, confirmation: Confirmation) -> UiAction {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.mode = Mode::Normal;
                match confirmation {
                    Confirmation::SummarizeSelected => self
                        .selected_session()
                        .map(|session| UiAction::Summarize {
                            thread_id: session.thread_id.clone(),
                        })
                        .unwrap_or(UiAction::None),
                    Confirmation::SummarizeAll => UiAction::SummarizeAll,
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = Mode::Normal;
                UiAction::None
            }
            _ => UiAction::None,
        }
    }
}

pub fn run(paths: &AppPaths, mut config: Config) -> Result<()> {
    paths.ensure_dirs()?;
    let database = Database::open(paths.database())?;
    let mut initial_status = None;
    if database.count_sessions()? == 0 {
        match CodexAppServer::connect()
            .and_then(|mut server| sync::sync_all(&database, &mut server, &config, paths))
        {
            Ok(report) => {
                initial_status = Some(format!(
                    "Initial local import: {} thread(s), {} error(s); no AI used",
                    report.total(),
                    report.errors.len()
                ));
            }
            Err(error) => initial_status = Some(format!("Initial import failed: {error:#}")),
        }
    }
    let sessions = database.list_sessions()?;
    let mut app = TrackerApp::new(sessions, &config);
    if let Some(status) = initial_status {
        app.status = status;
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let loop_result = run_loop(&mut terminal, &mut app, &database, paths, &mut config);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    loop_result
}

fn run_loop(
    terminal_ui: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut TrackerApp,
    database: &Database,
    paths: &AppPaths,
    config: &mut Config,
) -> Result<()> {
    loop {
        terminal_ui.draw(|frame| draw(frame, app))?;
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != crossterm::event::KeyEventKind::Press {
            continue;
        }
        let action = app.handle_key(key);
        if execute_action(action, app, database, paths, config)? {
            return Ok(());
        }
    }
}

fn execute_action(
    action: UiAction,
    app: &mut TrackerApp,
    database: &Database,
    paths: &AppPaths,
    config: &mut Config,
) -> Result<bool> {
    let selected_before = app
        .selected_session()
        .map(|session| session.thread_id.clone());
    match action {
        UiAction::None => return Ok(false),
        UiAction::Quit => return Ok(true),
        UiAction::Launch { thread_id } => {
            let session = database
                .get_session(&thread_id)?
                .with_context(|| format!("session {thread_id} disappeared"))?;
            match terminal::launch(&config.terminal_argv, &session.cwd, &thread_id) {
                Ok(pid) => {
                    app.status = format!(
                        "Launched {} in terminal process {pid}",
                        session.resume_command
                    )
                }
                Err(error) => app.status = format!("Launch failed: {error:#}"),
            }
        }
        UiAction::Sync => match CodexAppServer::connect()
            .and_then(|mut server| sync::sync_all(database, &mut server, config, paths))
        {
            Ok(report) => {
                app.status = format!(
                    "Sync: {} imported, {} updated, {} local summaries, {} error(s)",
                    report.imported,
                    report.updated,
                    report.summarized,
                    report.errors.len()
                )
            }
            Err(error) => app.status = format!("Sync failed: {error:#}"),
        },
        UiAction::Summarize { thread_id } => {
            match CodexAppServer::connect().and_then(|mut server| {
                sync::summarize_one(
                    database,
                    &mut server,
                    config,
                    paths,
                    &thread_id,
                    app.provider,
                    app.cap,
                )
            }) {
                Ok(()) => app.status = format!("Summarized {thread_id} with {}", app.provider),
                Err(error) => app.status = format!("Summary failed: {error:#}"),
            }
        }
        UiAction::SummarizeAll => match CodexAppServer::connect().and_then(|mut server| {
            sync::summarize_all(database, &mut server, config, paths, app.provider, app.cap)
        }) {
            Ok(report) => {
                app.status = format!(
                    "Summarize all: {} complete, {} error(s)",
                    report.summarized,
                    report.errors.len()
                )
            }
            Err(error) => app.status = format!("Summarize all failed: {error:#}"),
        },
        UiAction::TogglePin { thread_id } => {
            let pinned = database.toggle_pin(&thread_id)?;
            app.status = format!("{} {thread_id}", if pinned { "Pinned" } else { "Unpinned" });
        }
        UiAction::SaveEdit {
            thread_id,
            field,
            value,
        } => {
            match field {
                EditField::Title => database.set_manual_title(&thread_id, Some(value.trim()))?,
                EditField::Summary => {
                    database.set_manual_summary(&thread_id, Some(value.trim()))?
                }
                EditField::Notes => database.set_notes(&thread_id, value.trim())?,
                EditField::Tags => {
                    let tags = value
                        .split(',')
                        .map(str::trim)
                        .filter(|tag| !tag.is_empty())
                        .map(str::to_owned)
                        .collect::<Vec<_>>();
                    database.set_tags(&thread_id, &tags)?;
                }
            }
            app.status = format!("Saved {}", edit_label(field));
        }
        UiAction::SavePreferences { provider, cap } => {
            config.summary_provider = provider;
            config.summary_input = cap;
            config.save(paths)?;
            app.status = format!("Summary selection: {provider}, {cap}");
        }
    }
    app.replace_sessions(database.list_sessions()?, selected_before.as_deref());
    Ok(false)
}

fn draw(frame: &mut Frame<'_>, app: &mut TrackerApp) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(frame.area());

    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " Codex Resume Tracker ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(if app.query.is_empty() {
            "Search: (press /)".to_owned()
        } else {
            format!("Search: {}", app.query)
        }),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(title, areas[0]);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(areas[1]);
    draw_list(frame, app, columns[0]);
    draw_detail(frame, app, columns[1]);

    let help = match &app.mode {
        Mode::Search => "Type to filter | Enter/Esc finish | Backspace",
        Mode::Edit { .. } => "Type value | Enter save | Esc cancel | empty title/summary clears override",
        Mode::Confirm(_) => "This may consume Codex/API usage. y confirm | n/Esc cancel",
        Mode::Normal => {
            "Enter resume | / search | s sync | r retry | A summarize all | t/y/n/g edit | p pin | v/c provider/cap | q quit"
        }
    };
    let footer = Paragraph::new(Text::from(vec![
        Line::from(format!(
            "{} | provider={} cap={}",
            app.status, app.provider, app.cap
        )),
        Line::from(help),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, areas[2]);

    match &app.mode {
        Mode::Edit { field, buffer } => draw_popup(
            frame,
            &format!("Edit {}", edit_label(*field)),
            buffer,
            70,
            7,
        ),
        Mode::Confirm(confirmation) => {
            let target = match confirmation {
                Confirmation::SummarizeSelected => "the selected thread",
                Confirmation::SummarizeAll => "ALL visible threads",
            };
            draw_popup(
                frame,
                "Confirm summary usage",
                &format!(
                    "Summarize {target} with provider={} and cap={}?\nThis can consume Codex or OpenAI API usage. Press y to continue.",
                    app.provider, app.cap
                ),
                64,
                8,
            );
        }
        _ => {}
    }
}

fn draw_list(frame: &mut Frame<'_>, app: &mut TrackerApp, area: Rect) {
    let items = app
        .filtered
        .iter()
        .filter_map(|index| app.sessions.get(*index))
        .map(|session| {
            let prefix = if session.pinned { "[P] " } else { "    " };
            ListItem::new(vec![
                Line::from(format!("{prefix}{}", session.title())),
                Line::styled(
                    format!("    {} | {}", session.cwd, session.source),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        })
        .collect::<Vec<_>>();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("Sessions ({})", app.filtered.len())),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    let mut state = ListState::default();
    if !app.filtered.is_empty() {
        state.select(Some(app.selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_detail(frame: &mut Frame<'_>, app: &TrackerApp, area: Rect) {
    let text = if let Some(session) = app.selected_session() {
        let timestamps = format!(
            "created {} | updated {} | ended {}",
            format_timestamp(session.created_at),
            format_timestamp(session.updated_at.or(session.recency_at)),
            format_timestamp(session.ended_at)
        );
        let mut lines = vec![
            Line::styled(
                session.title().to_owned(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
            Line::from(vec![
                Span::styled("Resume: ", label_style()),
                Span::raw(&session.resume_command),
            ]),
            Line::from(vec![
                Span::styled("Directory: ", label_style()),
                Span::raw(&session.cwd),
            ]),
            Line::from(vec![
                Span::styled("Source/model: ", label_style()),
                Span::raw(format!(
                    "{} / {} ({})",
                    session.source,
                    session.model.as_deref().unwrap_or("unknown"),
                    session
                        .model_provider
                        .as_deref()
                        .unwrap_or("unknown provider")
                )),
            ]),
            Line::from(timestamps),
            Line::from(vec![
                Span::styled("Tags: ", label_style()),
                Span::raw(if session.tags.is_empty() {
                    "-".into()
                } else {
                    session.tags.join(", ")
                }),
            ]),
            Line::raw(""),
            Line::styled("Summary", label_style()),
            Line::raw(session.summary().to_owned()),
            Line::raw(""),
            Line::styled("Notes", label_style()),
            Line::raw(if session.notes.is_empty() {
                "-".into()
            } else {
                session.notes.clone()
            }),
            Line::raw(""),
            Line::from(format!(
                "Summary status: {} | provider: {} | cap: {}",
                session.summary_status,
                session.summary_provider.as_deref().unwrap_or("-"),
                session.summary_cap.as_deref().unwrap_or("-")
            )),
        ];
        if let Some(error) = &session.summary_error {
            lines.push(Line::styled(
                format!("Error: {error}"),
                Style::default().fg(Color::Red),
            ));
        }
        Text::from(lines)
    } else {
        Text::from("No sessions match the current search. Press s to sync or Esc to clear search.")
    };
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title("Details"))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_popup(frame: &mut Frame<'_>, title: &str, body: &str, percent_x: u16, height: u16) {
    let area = centered_rect(percent_x, height, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(body.to_owned())
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(Style::default().fg(Color::Yellow)),
            ),
        area,
    );
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(area.height.saturating_sub(height) / 2),
            Constraint::Length(height.min(area.height)),
            Constraint::Min(0),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Min(0),
        ])
        .split(vertical[1])[1]
}

fn label_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

fn edit_label(field: EditField) -> &'static str {
    match field {
        EditField::Title => "title override",
        EditField::Summary => "summary override",
        EditField::Notes => "notes",
        EditField::Tags => "tags",
    }
}

fn format_timestamp(value: Option<i64>) -> String {
    value
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
        .map(|timestamp| {
            timestamp
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "-".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, title: &str, tags: &[&str]) -> Session {
        Session {
            thread_id: id.into(),
            resume_command: format!("codex resume {id}"),
            cwd: format!("/work/{id}"),
            source: "cli".into(),
            model: Some("gpt-5".into()),
            model_provider: Some("openai".into()),
            created_at: Some(1),
            updated_at: Some(2),
            recency_at: Some(2),
            ended_at: None,
            generated_title: Some(title.into()),
            generated_summary: Some("summary".into()),
            manual_title: None,
            manual_summary: None,
            notes: String::new(),
            pinned: false,
            summary_provider: Some("local".into()),
            summary_cap: Some("64k".into()),
            summary_status: "ready".into(),
            summary_error: None,
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn incremental_search_filters_all_metadata() {
        let sessions = vec![
            session("one", "Rust tracker", &["tui"]),
            session("two", "Python", &["sqlite-special"]),
        ];
        let mut app = TrackerApp::new(sessions, &Config::default());
        app.handle_key(key(KeyCode::Char('/')));
        for character in "sqlite-special".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(app.filtered.len(), 1);
        assert_eq!(app.selected_session().unwrap().thread_id, "two");
    }

    #[test]
    fn enter_emits_launch_without_closing_tracker() {
        let mut app = TrackerApp::new(vec![session("thr_1", "Title", &[])], &Config::default());
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            UiAction::Launch {
                thread_id: "thr_1".into()
            }
        );
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn editing_title_emits_persistent_override_action() {
        let mut app = TrackerApp::new(vec![session("thr_1", "Old", &[])], &Config::default());
        app.handle_key(key(KeyCode::Char('t')));
        assert!(matches!(
            app.mode,
            Mode::Edit {
                field: EditField::Title,
                ..
            }
        ));
        if let Mode::Edit { field, .. } = &mut app.mode {
            *field = EditField::Title;
        }
        // Clear the prefilled value, then type a replacement.
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Backspace));
        }
        for character in "New".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            UiAction::SaveEdit {
                thread_id: "thr_1".into(),
                field: EditField::Title,
                value: "New".into()
            }
        );
    }

    #[test]
    fn ai_retry_requires_explicit_confirmation() {
        let mut app = TrackerApp::new(vec![session("thr_1", "Title", &[])], &Config::default());
        assert_eq!(app.handle_key(key(KeyCode::Char('r'))), UiAction::None);
        assert_eq!(app.mode, Mode::Confirm(Confirmation::SummarizeSelected));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            UiAction::Summarize {
                thread_id: "thr_1".into()
            }
        );
    }

    #[test]
    fn provider_and_cap_actions_update_selection() {
        let mut app = TrackerApp::new(Vec::new(), &Config::default());
        assert_eq!(
            app.handle_key(key(KeyCode::Char('v'))),
            UiAction::SavePreferences {
                provider: SummaryProvider::Openai,
                cap: SummaryInput::SixtyFourK
            }
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('c'))),
            UiAction::SavePreferences {
                provider: SummaryProvider::Openai,
                cap: SummaryInput::Entire
            }
        );
    }
}
