//! Terminal UI, milestone M0: project rail, project details, command palette.
//!
//! Layout and keys follow `CORVX_UX.md`. The UI holds no state of its own
//! beyond the current view; projects are read from the daemon, which is the
//! source of truth. If the daemon goes away, the UI reconnects on the next
//! action or periodic refresh.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use corvx_client::{ensure_daemon, is_connection_error, Client};
use corvx_proto::{
    methods, AttentionItem, AttentionList, Project, ProjectList, Session, SessionList, SessionState,
};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use serde_json::{json, Value};

const ACCENT: Color = Color::Rgb(121, 192, 255);
const RED: Color = Color::Rgb(248, 81, 73);
const DIM: Color = Color::Rgb(125, 133, 144);
const GREEN: Color = Color::Rgb(63, 185, 80);
const AMBER: Color = Color::Rgb(227, 179, 65);
const BORDER: Color = Color::Rgb(38, 45, 54);

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Normal,
    AddPath,
    Palette,
    ConfirmRemove,
    Help,
    Inbox,
    Sessions,
}

#[derive(Clone, Copy, Debug)]
enum Action {
    AddProject,
    RemoveProject,
    Refresh,
    Inbox,
    Sessions,
    Help,
    Quit,
}

const ACTIONS: [(Action, &str); 7] = [
    (Action::AddProject, "add project"),
    (Action::RemoveProject, "remove project"),
    (Action::Refresh, "refresh"),
    (Action::Inbox, "attention inbox"),
    (Action::Sessions, "sessions"),
    (Action::Help, "help"),
    (Action::Quit, "quit"),
];

const REFRESH_EVERY: Duration = Duration::from_secs(2);

struct App {
    client: Client,
    data_dir: PathBuf,
    projects: Vec<Project>,
    /// Open attention items, most urgent first.
    attention: Vec<AttentionItem>,
    /// Every session the daemon manages, in start order.
    sessions: Vec<Session>,
    sessions_sel: usize,
    /// A session to attach to. The event loop hands it the terminal.
    pending_attach: Option<String>,
    inbox_sel: usize,
    selected: usize,
    mode: Mode,
    input: String,
    palette_sel: usize,
    flash: String,
    connected: bool,
    daemon_pid: u32,
    quit: bool,
    last_refresh: Instant,
}

pub fn run(client: Client, data_dir: PathBuf) -> Result<()> {
    let mut app = App::new(client, data_dir)?;
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|f| draw(f, app))?;
        if app.quit {
            return Ok(());
        }
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.on_key(key);
                }
            }
        }
        if let Some(id) = app.pending_attach.take() {
            // Give the terminal to the session until the user detaches, then redraw.
            ratatui::restore();
            let r = crate::attach(&mut app.client, &id, &app.data_dir);
            *terminal = ratatui::init();
            terminal.clear()?;
            if app.report(r).is_some() {
                app.flash = format!("detached from {id}");
            }
            let r = app.refresh();
            app.report(r);
        }
        if app.last_refresh.elapsed() >= REFRESH_EVERY {
            let r = app.refresh();
            app.report(r);
        }
    }
}

impl App {
    fn new(client: Client, data_dir: PathBuf) -> Result<Self> {
        let mut app = Self {
            client,
            data_dir,
            projects: Vec::new(),
            selected: 0,
            mode: Mode::Normal,
            input: String::new(),
            palette_sel: 0,
            flash: String::new(),
            connected: true,
            daemon_pid: 0,
            quit: false,
            last_refresh: Instant::now(),
            attention: Vec::new(),
            sessions: Vec::new(),
            sessions_sel: 0,
            pending_attach: None,
            inbox_sel: 0,
        };
        app.daemon_pid = app.client.info().map(|i| i.pid).unwrap_or(0);
        app.refresh()?;
        Ok(app)
    }

    /// Calls the daemon. On a dropped connection, starts or reconnects to it once and retries.
    fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        match self.client.call::<Value, Value>(method, &params) {
            Ok(v) => Ok(v),
            Err(e) if is_connection_error(&e) => {
                self.client = ensure_daemon(&self.data_dir, None)?;
                self.daemon_pid = self.client.info().map(|i| i.pid).unwrap_or(0);
                self.client.call(method, &params)
            }
            Err(e) => Err(e),
        }
    }

    fn refresh(&mut self) -> Result<()> {
        let list: ProjectList =
            serde_json::from_value(self.rpc(methods::PROJECT_LIST, json!({}))?)?;
        self.projects = list.projects;
        if self.selected >= self.projects.len() {
            self.selected = self.projects.len().saturating_sub(1);
        }
        let att: AttentionList =
            serde_json::from_value(self.rpc(methods::ATTENTION_LIST, json!({}))?)?;
        self.attention = att.items;
        let list: SessionList =
            serde_json::from_value(self.rpc(methods::SESSION_LIST, json!({}))?)?;
        self.sessions = list.sessions;
        // Running sessions first (the ones you can attach to), then ended ones; start order within each.
        self.sessions
            .sort_by_key(|s| s.state != SessionState::Running);
        if self.sessions_sel >= self.sessions.len() {
            self.sessions_sel = self.sessions.len().saturating_sub(1);
        }
        if self.inbox_sel >= self.attention.len() {
            self.inbox_sel = self.attention.len().saturating_sub(1);
        }
        self.connected = true;
        self.last_refresh = Instant::now();
        Ok(())
    }

    /// Shows an error in the status line. Marks the daemon as disconnected for connection errors.
    fn report<T>(&mut self, result: Result<T>) -> Option<T> {
        match result {
            Ok(v) => Some(v),
            Err(e) => {
                if is_connection_error(&e) {
                    self.connected = false;
                }
                self.flash = format!("error: {e:#}");
                None
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('k') {
            self.toggle_palette();
            return;
        }
        match self.mode {
            Mode::Normal => self.key_normal(key.code),
            Mode::AddPath => self.key_add(key.code),
            Mode::Palette => self.key_palette(key.code),
            Mode::ConfirmRemove => self.key_confirm(key.code),
            Mode::Help => self.mode = Mode::Normal,
            Mode::Inbox => self.key_inbox(key.code),
            Mode::Sessions => self.key_sessions(key.code),
        }
    }

    fn toggle_palette(&mut self) {
        self.mode = if self.mode == Mode::Palette {
            Mode::Normal
        } else {
            Mode::Palette
        };
        self.input.clear();
        self.palette_sel = 0;
    }

    fn key_normal(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
            KeyCode::Char('a') => self.start_add(),
            KeyCode::Char('d') => self.start_remove(),
            KeyCode::Char('r') => {
                let r = self.refresh();
                if self.report(r).is_some() {
                    self.flash = "refreshed".into();
                }
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('i') => self.open_inbox(),
            KeyCode::Char('s') => self.open_sessions(),
            KeyCode::Char(':') => self.toggle_palette(),
            KeyCode::Esc => self.flash.clear(),
            _ => {}
        }
    }

    fn open_sessions(&mut self) {
        self.mode = Mode::Sessions;
        self.flash.clear();
    }

    fn key_sessions(&mut self, code: KeyCode) {
        let max = self.sessions.len().saturating_sub(1);
        match code {
            KeyCode::Esc | KeyCode::Char('s') => self.mode = Mode::Normal,
            KeyCode::Char('j') | KeyCode::Down => {
                self.sessions_sel = (self.sessions_sel + 1).min(max)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.sessions_sel = self.sessions_sel.saturating_sub(1)
            }
            KeyCode::Enter => match self.sessions.get(self.sessions_sel) {
                Some(s) if s.state == SessionState::Running => {
                    self.pending_attach = Some(s.id.clone());
                }
                Some(s) => self.flash = format!("{} has ended; nothing to attach to", s.id),
                None => {}
            },
            _ => {}
        }
    }

    fn open_inbox(&mut self) {
        self.mode = Mode::Inbox;
        self.flash.clear();
    }

    fn key_inbox(&mut self, code: KeyCode) {
        let max = self.attention.len().saturating_sub(1);
        match code {
            KeyCode::Esc | KeyCode::Char('i') => self.mode = Mode::Normal,
            KeyCode::Char('j') | KeyCode::Down => self.inbox_sel = (self.inbox_sel + 1).min(max),
            KeyCode::Char('k') | KeyCode::Up => self.inbox_sel = self.inbox_sel.saturating_sub(1),
            KeyCode::Char('a') => {
                self.attention_action(methods::ATTENTION_ACK, json!({}), "acknowledged")
            }
            KeyCode::Char('s') => self.attention_action(
                methods::ATTENTION_SNOOZE,
                json!({"minutes": 15}),
                "snoozed for 15 minutes",
            ),
            KeyCode::Char('x') => self.attention_action(
                methods::ATTENTION_DISMISS,
                json!({"reason": "dismissed in the terminal UI"}),
                "dismissed",
            ),
            KeyCode::Enter => {
                if let Some(item) = self.attention.get(self.inbox_sel) {
                    let project = item.project.clone();
                    if let Some(i) = self.projects.iter().position(|p| p.id == project) {
                        self.selected = i;
                    }
                    self.mode = Mode::Normal;
                }
            }
            _ => {}
        }
    }

    /// Runs an attention action on the selected inbox item, then refreshes.
    fn attention_action(&mut self, method: &str, extra: Value, done: &str) {
        let Some(item) = self.attention.get(self.inbox_sel).cloned() else {
            return;
        };
        let mut params = extra;
        params["id"] = json!(item.id);
        let r = self.rpc(method, params);
        if self.report(r).is_some() {
            self.flash = format!("{done} · {}", item.summary);
            let r = self.refresh();
            self.report(r);
        }
    }

    fn key_add(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.input.clear();
            }
            KeyCode::Enter => {
                let raw = std::mem::take(&mut self.input);
                self.mode = Mode::Normal;
                self.add_project(&raw);
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) => self.input.push(c),
            _ => {}
        }
    }

    fn key_confirm(&mut self, code: KeyCode) {
        self.mode = Mode::Normal;
        if code == KeyCode::Char('y') {
            self.remove_selected();
        } else {
            self.flash = "cancelled".into();
        }
    }

    fn key_palette(&mut self, code: KeyCode) {
        let matches = filter_actions(&self.input);
        match code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.input.clear();
            }
            KeyCode::Up => self.palette_sel = self.palette_sel.saturating_sub(1),
            KeyCode::Down => {
                if self.palette_sel + 1 < matches.len() {
                    self.palette_sel += 1;
                }
            }
            KeyCode::Enter => {
                let chosen = matches.get(self.palette_sel).map(|(a, _)| *a);
                self.mode = Mode::Normal;
                self.input.clear();
                if let Some(action) = chosen {
                    self.run_action(action);
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
                self.palette_sel = 0;
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                self.palette_sel = 0;
            }
            _ => {}
        }
    }

    fn run_action(&mut self, action: Action) {
        match action {
            Action::AddProject => self.start_add(),
            Action::RemoveProject => self.start_remove(),
            Action::Refresh => {
                let r = self.refresh();
                self.report(r);
            }
            Action::Inbox => self.open_inbox(),
            Action::Sessions => self.open_sessions(),
            Action::Help => self.mode = Mode::Help,
            Action::Quit => self.quit = true,
        }
    }

    fn start_add(&mut self) {
        self.mode = Mode::AddPath;
        self.input.clear();
        self.flash.clear();
    }

    fn start_remove(&mut self) {
        if self.projects.is_empty() {
            self.flash = "nothing to remove".into();
        } else {
            self.mode = Mode::ConfirmRemove;
        }
    }

    fn move_sel(&mut self, delta: i32) {
        if self.projects.is_empty() {
            self.selected = 0;
            return;
        }
        let max = self.projects.len() as i32 - 1;
        self.selected = (self.selected as i32 + delta).clamp(0, max) as usize;
    }

    fn add_project(&mut self, raw: &str) {
        let expanded = expand_home(raw.trim());
        if expanded.is_empty() {
            self.flash = "enter a path".into();
            return;
        }
        let abs = match std::fs::canonicalize(&expanded) {
            Ok(p) => p,
            Err(_) => {
                self.flash = format!("no such path: {expanded}");
                return;
            }
        };
        let r = self
            .rpc(
                methods::PROJECT_ADD,
                json!({"path": abs.display().to_string()}),
            )
            .and_then(|v| Ok(serde_json::from_value::<Project>(v)?));
        if let Some(p) = self.report(r) {
            self.flash = format!("added {}", p.id);
            let r = self.refresh();
            self.report(r);
            if let Some(i) = self.projects.iter().position(|x| x.id == p.id) {
                self.selected = i;
            }
        }
    }

    fn remove_selected(&mut self) {
        let Some(id) = self.projects.get(self.selected).map(|p| p.id.clone()) else {
            return;
        };
        let r = self.rpc(methods::PROJECT_REMOVE, json!({"id": id}));
        if self.report(r).is_some() {
            self.flash = format!("removed {id} · files untouched");
            let r = self.refresh();
            self.report(r);
        }
    }

    fn selected_project(&self) -> Option<&Project> {
        self.projects.get(self.selected)
    }
}

fn filter_actions(query: &str) -> Vec<(Action, &'static str)> {
    let q = query.to_lowercase();
    ACTIONS
        .iter()
        .copied()
        .filter(|(_, label)| label.contains(&q))
        .collect()
}

fn expand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return Path::new(&home).join(rest).display().to_string();
        }
    }
    path.to_string()
}

// ---------- drawing ----------

fn draw(f: &mut Frame, app: &App) {
    let [top, body, status, input] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(f.area());

    draw_top(f, top, app);
    let [rail, main] = Layout::horizontal([Constraint::Length(30), Constraint::Min(0)]).areas(body);
    draw_rail(f, rail, app);
    match app.mode {
        Mode::Inbox => draw_inbox(f, main, app),
        Mode::Sessions => draw_sessions(f, main, app),
        _ => draw_main(f, main, app),
    }
    draw_status(f, status, app);
    draw_input(f, input, app);

    match app.mode {
        Mode::Palette => draw_palette(f, body, app),
        Mode::Help => draw_help(f, body),
        _ => {}
    }
}

fn draw_top(f: &mut Frame, area: Rect, app: &App) {
    let daemon = if app.connected {
        Span::styled(
            format!("● corvxd pid {}", app.daemon_pid),
            Style::default().fg(GREEN),
        )
    } else {
        Span::styled(
            "◌ daemon disconnected · retrying",
            Style::default().fg(AMBER),
        )
    };
    let line = Line::from(vec![
        Span::styled(
            " CORVX ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" projects {}   ", app.projects.len()),
            Style::default().fg(DIM),
        ),
        Span::styled(
            format!("{} blocked  ", count_kind(app, "blocked")),
            Style::default().fg(AMBER),
        ),
        Span::styled(
            format!("{} waiting  ", count_kind(app, "waiting")),
            Style::default().fg(ACCENT),
        ),
        Span::styled(
            format!("{} done  ", count_kind(app, "done")),
            Style::default().fg(GREEN),
        ),
        Span::styled(
            format!("{} failed   ", count_kind(app, "failed")),
            Style::default().fg(RED),
        ),
        daemon,
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_rail(f: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = Vec::new();
    let mut last_ws: Option<&str> = None;
    for (i, p) in app.projects.iter().enumerate() {
        if last_ws != Some(p.workspace.as_str()) {
            lines.push(Line::styled(
                p.workspace.to_uppercase(),
                Style::default().fg(DIM),
            ));
            last_ws = Some(p.workspace.as_str());
        }
        let selected = i == app.selected;
        let (marker, style) = if selected {
            (
                "▸ ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )
        } else {
            ("  ", Style::default())
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker}{}", p.id), style),
            Span::styled(
                format!("  {}", p.stack.first().map(String::as_str).unwrap_or("-")),
                Style::default().fg(DIM),
            ),
            Span::styled(
                match app.attention.iter().filter(|i| i.project == p.id).count() {
                    0 => String::new(),
                    n => format!("  !{n}"),
                },
                Style::default().fg(AMBER),
            ),
        ]));
    }
    if app.projects.is_empty() {
        lines.push(Line::styled("none yet", Style::default().fg(DIM)));
    }
    let block = Block::bordered()
        .title(" projects ")
        .border_style(Style::default().fg(BORDER));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_main(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered()
        .title(" project ")
        .border_style(Style::default().fg(BORDER));
    let Some(p) = app.selected_project() else {
        let welcome = vec![
            Line::styled(
                "No projects yet.",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::from(""),
            Line::styled(
                "Press a to add a project directory, or run:",
                Style::default().fg(DIM),
            ),
            Line::from("  corvx project add ./my-app"),
        ];
        f.render_widget(Paragraph::new(welcome).block(block), area);
        return;
    };

    let label = |s: &str| Span::styled(format!("{s:<10}"), Style::default().fg(DIM));
    let mut lines = vec![
        Line::styled(
            p.name.clone(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Line::from(vec![label("id"), Span::raw(p.id.clone())]),
        Line::from(vec![label("path"), Span::raw(p.path.clone())]),
        Line::from(vec![label("workspace"), Span::raw(p.workspace.clone())]),
        Line::from(vec![
            label("branch"),
            Span::raw(p.branch.clone().unwrap_or_else(|| "-".into())),
        ]),
        Line::from(vec![
            label("stack"),
            Span::raw(if p.stack.is_empty() {
                "unknown".into()
            } else {
                p.stack.join(", ")
            }),
        ]),
        Line::from(vec![
            label("test"),
            Span::raw(
                p.test_command
                    .clone()
                    .unwrap_or_else(|| "not detected".into()),
            ),
        ]),
        Line::from(vec![
            label("build"),
            Span::raw(
                p.build_command
                    .clone()
                    .unwrap_or_else(|| "not detected".into()),
            ),
        ]),
        Line::from(""),
        Line::styled("AGENTS", Style::default().fg(ACCENT)),
    ];
    lines.extend(session_lines(app, &p.id));
    lines.push(Line::from(""));
    lines.extend(attention_lines(app, &p.id));
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "Project Model: L1 structure scanned at registration · L2+ arrive in M3",
        Style::default().fg(DIM),
    ));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let (label, color) = match app.mode {
        Mode::Normal => ("NORMAL", ACCENT),
        Mode::AddPath => ("ADD", GREEN),
        Mode::Palette => ("PALETTE", Color::Rgb(188, 140, 255)),
        Mode::ConfirmRemove => ("CONFIRM", AMBER),
        Mode::Help => ("HELP", ACCENT),
        Mode::Inbox => ("INBOX", ACCENT),
        Mode::Sessions => ("SESSIONS", ACCENT),
    };
    let hint = if !app.flash.is_empty() {
        app.flash.clone()
    } else {
        match app.mode {
            Mode::Normal => {
                "j/k move · a add · d remove · r refresh · : commands · ? help · q quit".into()
            }
            Mode::AddPath => "type a path · Enter add · Esc cancel".into(),
            Mode::Palette => "type to filter · ↑↓ select · Enter run · Esc close".into(),
            Mode::ConfirmRemove => "y remove from CORVX · any other key cancels".into(),
            Mode::Help => "any key closes".into(),
            Mode::Inbox => {
                "j/k move · a ack · s snooze · x dismiss · Enter project · Esc close".into()
            }
            Mode::Sessions => {
                "j/k move · Enter attach (Ctrl+\\ then d to come back) · Esc close".into()
            }
        }
    };
    let line = Line::from(vec![
        Span::styled(
            format!(" {label} "),
            Style::default()
                .fg(Color::Black)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(hint, Style::default().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_input(f: &mut Frame, area: Rect, app: &App) {
    let line = match app.mode {
        Mode::AddPath => Line::from(vec![
            Span::styled("path › ", Style::default().fg(GREEN)),
            Span::raw(format!("{}█", app.input)),
        ]),
        Mode::ConfirmRemove => Line::styled(
            format!(
                "remove '{}' from CORVX? Your files are not touched.",
                app.selected_project().map(|p| p.id.as_str()).unwrap_or("")
            ),
            Style::default().fg(AMBER),
        ),
        _ => Line::from(""),
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_palette(f: &mut Frame, area: Rect, app: &App) {
    let matches = filter_actions(&app.input);
    let rect = centered(area, 40, (matches.len() as u16 + 4).max(5));
    f.render_widget(Clear, rect);
    let mut lines = vec![Line::from(vec![
        Span::styled("⌘ ", Style::default().fg(DIM)),
        Span::raw(format!("{}█", app.input)),
    ])];
    for (i, (_, label)) in matches.iter().enumerate() {
        let style = if i == app.palette_sel {
            Style::default().fg(Color::Black).bg(ACCENT)
        } else {
            Style::default()
        };
        lines.push(Line::styled(format!(" {label} "), style));
    }
    if matches.is_empty() {
        lines.push(Line::styled(
            "no matching command",
            Style::default().fg(DIM),
        ));
    }
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" commands ")
                .border_style(Style::default().fg(BORDER)),
        ),
        rect,
    );
}

fn draw_help(f: &mut Frame, area: Rect) {
    let rect = centered(area, 62, 14);
    f.render_widget(Clear, rect);
    let rows = [
        ("j / k, ↑ ↓", "move selection"),
        ("a", "add a project directory"),
        ("d", "remove the selected project (files untouched)"),
        ("r", "refresh from the daemon"),
        (": or Ctrl+K", "command palette"),
        ("i", "attention inbox: what needs you"),
        ("s", "sessions: Enter attaches, Ctrl+\\ then d returns"),
        ("?", "this help"),
        ("q", "quit the UI (the daemon keeps running)"),
        ("", ""),
        ("CLI", "corvx project add|list|remove · corvx status"),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(format!("{k:<14}"), Style::default().fg(ACCENT)),
                Span::raw(*v),
            ])
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" keys ")
                .border_style(Style::default().fg(BORDER)),
        ),
        rect,
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

fn count_kind(app: &App, kind: &str) -> usize {
    app.attention.iter().filter(|i| i.kind == kind).count()
}

/// The attention items for one project, shown in the project view.
fn attention_lines(app: &App, project: &str) -> Vec<Line<'static>> {
    let mut out = vec![Line::styled("ATTENTION", Style::default().fg(ACCENT))];
    let items: Vec<&AttentionItem> = app
        .attention
        .iter()
        .filter(|i| i.project == project)
        .collect();
    if items.is_empty() {
        out.push(Line::styled(
            "  nothing needs you",
            Style::default().fg(DIM),
        ));
    }
    for item in items {
        out.push(Line::from(vec![
            Span::styled(
                format!("  {:<8}", kind_label(&item.kind)),
                kind_style(&item.kind),
            ),
            Span::raw(item.summary.clone()),
        ]));
    }
    out
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "blocked" => "BLOCKED",
        "waiting" => "WAITING",
        "done" => "DONE",
        "failed" => "FAILED",
        _ => "?",
    }
}

fn kind_style(kind: &str) -> Style {
    let color = match kind {
        "blocked" => AMBER,
        "waiting" => ACCENT,
        "done" => GREEN,
        "failed" => RED,
        _ => DIM,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

/// The inbox: every open item across projects, most urgent first.
fn draw_inbox(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered()
        .title(format!(" inbox · {} open ", app.attention.len()))
        .border_style(Style::default().fg(BORDER));
    if app.attention.is_empty() {
        let lines = vec![
            Line::styled("Nothing needs you.", Style::default().fg(GREEN)),
            Line::styled(
                "Agents that are blocked, waiting, finished or failed appear here.",
                Style::default().fg(DIM),
            ),
        ];
        f.render_widget(Paragraph::new(lines).block(block), area);
        return;
    }
    let now = now_secs();
    let mut lines: Vec<Line> = Vec::new();
    for (i, item) in app.attention.iter().enumerate() {
        let selected = i == app.inbox_sel;
        let marker = if selected { "▸ " } else { "  " };
        let row_style = if selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(marker, row_style),
            Span::styled(
                format!("{:<8}", kind_label(&item.kind)),
                kind_style(&item.kind),
            ),
            Span::styled(format!("{:<16}", item.project), Style::default().fg(ACCENT)),
            Span::styled(
                format!("{:>5}  ", age_label(now - item.created_at)),
                Style::default().fg(DIM),
            ),
            Span::raw(item.summary.clone()),
        ]));
    }
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn age_label(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h", s / 3600),
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn agent_label(s: &Session) -> String {
    let state = if s.state == SessionState::Running {
        format!("{:?}", s.agent_state).to_lowercase()
    } else {
        format!("{:?}", s.state).to_lowercase()
    };
    match &s.agent_detail {
        Some(d) => format!("{state}: {d}"),
        None => state,
    }
}

fn agent_style(s: &Session) -> Style {
    use corvx_proto::AgentState::*;
    if s.state != SessionState::Running {
        return Style::default().fg(DIM);
    }
    Style::default().fg(match s.agent_state {
        Blocked => AMBER,
        Waiting => ACCENT,
        Done => GREEN,
        Failed => RED,
        _ => Color::Reset,
    })
}

/// The sessions of one project, shown in the project view. Ended sessions are left out.
fn session_lines(app: &App, project: &str) -> Vec<Line<'static>> {
    let running: Vec<&Session> = app
        .sessions
        .iter()
        .filter(|s| s.project == project && s.state == SessionState::Running)
        .collect();
    if running.is_empty() {
        return vec![Line::styled(
            "  none running · start one with `corvx session start`",
            Style::default().fg(DIM),
        )];
    }
    running
        .into_iter()
        .map(|s| {
            Line::from(vec![
                Span::raw(format!("  {:<16}", s.id)),
                Span::styled(agent_label(s), agent_style(s)),
            ])
        })
        .collect()
}

/// Every session the daemon manages. Enter attaches to the selected one.
fn draw_sessions(f: &mut Frame, area: Rect, app: &App) {
    let running = app
        .sessions
        .iter()
        .filter(|s| s.state == SessionState::Running)
        .count();
    let block = Block::bordered()
        .title(format!(" sessions · {running} running "))
        .border_style(Style::default().fg(BORDER));
    if app.sessions.is_empty() {
        let lines = vec![
            Line::styled(
                "No sessions yet.",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                "Start one with: corvx session start <project> --runtime claude -- claude",
                Style::default().fg(DIM),
            ),
        ];
        f.render_widget(Paragraph::new(lines).block(block), area);
        return;
    }
    let lines: Vec<Line> = app
        .sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let selected = i == app.sessions_sel;
            let marker_style = if selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(if selected { "▸ " } else { "  " }, marker_style),
                Span::raw(format!("{:<16}", s.id)),
                Span::styled(format!("{:<14}", s.project), Style::default().fg(ACCENT)),
                Span::styled(format!("{:<34}", agent_label(s)), agent_style(s)),
                Span::styled(format!("{:<6}", s.backend), Style::default().fg(DIM)),
                Span::raw(s.command.join(" ")),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines).block(block), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_filters_by_label() {
        let all = filter_actions("");
        assert_eq!(all.len(), ACTIONS.len());
        let found = filter_actions("proj");
        assert_eq!(found.len(), 2);
        assert!(filter_actions("zzz").is_empty());
    }

    #[test]
    fn centered_stays_inside_area() {
        let area = Rect::new(0, 0, 50, 10);
        let r = centered(area, 100, 100);
        assert!(r.x + r.width <= area.width && r.y + r.height <= area.height);
    }

    #[test]
    fn home_expansion_only_touches_leading_tilde() {
        assert_eq!(expand_home("/abs/path"), "/abs/path");
        assert_eq!(expand_home("relative"), "relative");
    }
}
