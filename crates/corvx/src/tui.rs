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

use crate::missions::Mission;
use corvx_proto::{
    methods, AttentionItem, AttentionList, Knowledge, KnowledgeAccepted, KnowledgeInit, Project,
    ProjectList, Proposal, ProposalList, Rule, RuleList, Session, SessionList, SessionState,
};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
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
    /// What adding the typed path would add: its scan, and whether to set up its knowledge.
    AddPreview,
    /// The form that starts a mission: goal, autonomy level, models, who approves.
    Mission,
    Palette,
    ConfirmRemove,
    Help,
    Inbox,
    Sessions,
    /// Typing an answer to the selected question in the inbox.
    Answer,
    /// Typing an optional reason for denying the selected permission request.
    Deny,
    /// Typing an optional reason for rejecting the selected knowledge proposal.
    Reject,
    /// CORVX's rules: what it asks you, allows or denies, for the selected project.
    Rules,
    /// The selected project's name and workspace.
    EditProject,
    /// The selected project's missions, newest first, with the selected one's journal.
    Missions,
    /// Asking before stopping a session (from the sessions or missions view).
    ConfirmStop,
}

#[derive(Clone, Copy, Debug)]
enum Action {
    AddProject,
    StartMission,
    SetUpKnowledge,
    EditBrief,
    EditDecisions,
    EditConventions,
    EditProject,
    Rescan,
    Missions,
    RemoveProject,
    Refresh,
    RestartDaemon,
    Inbox,
    Sessions,
    Help,
    Quit,
}

const ACTIONS: [(Action, &str); 16] = [
    (Action::AddProject, "add project"),
    (Action::StartMission, "start a mission"),
    (Action::Missions, "missions: history and journals"),
    (Action::SetUpKnowledge, "set up knowledge files"),
    (Action::EditBrief, "edit the brief"),
    (Action::EditDecisions, "edit the decisions"),
    (Action::EditConventions, "edit the conventions"),
    (
        Action::EditProject,
        "rename project or change its workspace",
    ),
    (Action::Rescan, "rescan project (stack, test command)"),
    (Action::RemoveProject, "remove project"),
    (Action::Refresh, "refresh"),
    (Action::RestartDaemon, "restart the daemon"),
    (Action::Inbox, "attention inbox"),
    (Action::Sessions, "sessions"),
    (Action::Help, "help"),
    (Action::Quit, "quit"),
];

const REFRESH_EVERY: Duration = Duration::from_secs(2);

/// Models an orchestrator can run on, and its agents, defaults first.
const ORCHESTRATOR_MODELS: [&str; 2] = ["opus", "sonnet"];
const AGENT_MODELS: [&str; 3] = ["sonnet", "haiku", "opus"];

#[derive(Clone, Copy, Default)]
struct KnowledgeState {
    wired: bool,
    brief_written: bool,
    /// Entries in its mission journal.
    journal: usize,
}

impl KnowledgeState {
    fn of(project: &Project) -> Self {
        let root = Path::new(&project.path);
        let brief = std::fs::read_to_string(root.join(".corvx/brief.md")).unwrap_or_default();
        KnowledgeState {
            wired: corvx_core::knowledge::wired(root),
            // The template's placeholders mean nobody has written it yet.
            brief_written: !brief.trim().is_empty() && !brief.contains("(What the project does"),
            journal: std::fs::read_dir(root.join(".corvx/journal"))
                .map(|d| {
                    d.filter_map(|e| e.ok())
                        .filter(|e| e.file_name().to_string_lossy().ends_with(".md"))
                        .count()
                })
                .unwrap_or(0),
        }
    }
}

struct AddPreview {
    path: PathBuf,
    scan: corvx_core::scan::Scan,
    git: bool,
    wired: bool,
    /// The project already registered at this path.
    existing: Option<String>,
    /// Set up its knowledge files once it is added.
    knowledge: bool,
    name: String,
    workspace: String,
    /// 0 name, 1 workspace, 2 knowledge.
    field: usize,
}

const PREVIEW_FIELDS: usize = 3;

/// Renaming a project or moving it to another workspace.
struct ProjectEdit {
    id: String,
    name: String,
    workspace: String,
    /// 0 name, 1 workspace.
    field: usize,
}

struct MissionForm {
    project: String,
    goal: String,
    /// 0 goal, 1 autonomy, 2 orchestrator model, 3 agent model, 4 approvals.
    field: usize,
    level: usize,
    model: usize,
    agent_model: usize,
    human_approves: bool,
    /// What the orchestrator will start from, in one line.
    knowledge: String,
    wired: bool,
}

const MISSION_FIELDS: usize = 5;

struct App {
    client: Client,
    data_dir: PathBuf,
    projects: Vec<Project>,
    /// Open attention items, most urgent first.
    attention: Vec<AttentionItem>,
    /// Every session the daemon manages, in start order.
    sessions: Vec<Session>,
    /// How deep each session sits under its orchestrator, parallel to `sessions`.
    session_depths: Vec<usize>,
    sessions_sel: usize,
    /// A session to attach to. The event loop hands it the terminal.
    pending_attach: Option<String>,
    /// A proposal to edit before accepting it. The event loop hands the terminal to $EDITOR.
    pending_edit: Option<String>,
    /// Open knowledge proposals, by id (the id of their inbox item).
    proposals: std::collections::HashMap<String, Proposal>,
    /// A knowledge file to open in $EDITOR (the project, its path, which file); committed
    /// if it changed.
    pending_file: Option<(String, PathBuf, &'static str)>,
    /// Per project: whether its agents read its knowledge, and whether its brief is written.
    knowledge_state: std::collections::HashMap<String, KnowledgeState>,
    /// The project the add flow would add.
    preview: Option<AddPreview>,
    mission: Option<MissionForm>,
    edit: Option<ProjectEdit>,
    /// The missions view: whose, its missions, the selected one, how far its journal scrolled.
    missions_project: String,
    missions: Vec<Mission>,
    missions_sel: usize,
    journal_scroll: u16,
    /// The session to stop once confirmed, and the view to go back to.
    stop_target: Option<(String, Mode)>,
    /// The daemon binary on disk is newer than the running daemon.
    newer_build: bool,
    inbox_sel: usize,
    /// The rules as they apply where the rules view sets them.
    rules: Vec<Rule>,
    rules_sel: usize,
    /// The rules view sets rules for every project, not the selected one.
    rules_everywhere: bool,
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
        if let Some(id) = app.pending_edit.take() {
            // Give the terminal to the editor, then accept what the human wrote.
            let text = app
                .proposals
                .get(&id)
                .map(|p| p.text.clone())
                .unwrap_or_default();
            ratatui::restore();
            let edited = crate::edit_text(&text);
            *terminal = ratatui::init();
            terminal.clear()?;
            match app.report(edited) {
                Some(Some(text)) => app.accept_proposal(&id, Some(text)),
                Some(None) => app.flash = "unchanged: nothing accepted".into(),
                None => {}
            }
        }
        if let Some((project, path, file)) = app.pending_file.take() {
            ratatui::restore();
            let edited = crate::edit_file(&path);
            *terminal = ratatui::init();
            terminal.clear()?;
            if let Some(changed) = app.report(edited) {
                app.after_editing(&project, &path, file, changed);
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
            session_depths: Vec::new(),
            sessions_sel: 0,
            pending_attach: None,
            pending_edit: None,
            proposals: Default::default(),
            pending_file: None,
            knowledge_state: Default::default(),
            preview: None,
            mission: None,
            edit: None,
            missions_project: String::new(),
            missions: Vec::new(),
            missions_sel: 0,
            journal_scroll: 0,
            stop_target: None,
            newer_build: false,
            inbox_sel: 0,
            rules: Vec::new(),
            rules_sel: 0,
            rules_everywhere: false,
        };
        app.daemon_pid = app.client.info().map(|i| i.pid).unwrap_or(0);
        app.refresh()?;
        let lost = app
            .sessions
            .iter()
            .filter(|s| s.state == SessionState::Lost && s.supervisor.is_none())
            .count();
        if lost > 0 {
            app.flash =
                format!("{lost} interrupted session(s) · press s, then R on one to resume it");
        }
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
        // Items reorder as they change (an escalated request moves up): keep the same one selected.
        let selected_item = self.attention.get(self.inbox_sel).map(|i| i.id.clone());
        self.attention = att.items;
        if let Some(i) = selected_item.and_then(|id| self.attention.iter().position(|i| i.id == id))
        {
            self.inbox_sel = i;
        }
        self.knowledge_state = self
            .projects
            .iter()
            .map(|p| (p.id.clone(), KnowledgeState::of(p)))
            .collect();
        // (An older daemon has no proposals: the inbox works without them.)
        self.proposals = self
            .rpc(methods::KNOWLEDGE_PROPOSALS, json!({}))
            .ok()
            .and_then(|v| serde_json::from_value::<ProposalList>(v).ok())
            .map(|l| l.proposals.into_iter().map(|p| (p.id.clone(), p)).collect())
            .unwrap_or_default();
        let list: SessionList =
            serde_json::from_value(self.rpc(methods::SESSION_LIST, json!({}))?)?;
        let (sessions, depths) = session_tree(list.sessions);
        self.sessions = sessions;
        self.session_depths = depths;
        if self.sessions_sel >= self.sessions.len() {
            self.sessions_sel = self.sessions.len().saturating_sub(1);
        }
        if self.inbox_sel >= self.attention.len() {
            self.inbox_sel = self.attention.len().saturating_sub(1);
        }
        if let Ok(info) = self.client.info() {
            self.daemon_pid = info.pid;
            self.newer_build = corvx_client::newer_build_on_disk(&info);
        }
        if self.mode == Mode::Missions {
            self.load_missions();
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
            Mode::AddPreview => self.key_preview(key.code),
            Mode::Mission => self.key_mission(key.code),
            Mode::Palette => self.key_palette(key.code),
            Mode::ConfirmRemove => self.key_confirm(key.code),
            Mode::Help => self.mode = Mode::Normal,
            Mode::Inbox => self.key_inbox(key.code),
            Mode::Sessions => self.key_sessions(key.code),
            Mode::Answer => self.key_answer(key.code),
            Mode::Deny => self.key_deny(key.code),
            Mode::Reject => self.key_reject(key.code),
            Mode::Rules => self.key_rules(key.code),
            Mode::EditProject => self.key_edit(key.code),
            Mode::Missions => self.key_missions(key.code),
            Mode::ConfirmStop => self.key_confirm_stop(key.code),
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
            KeyCode::Char('l') => self.open_rules(),
            KeyCode::Char('o') => self.open_mission(),
            KeyCode::Char('b') => self.edit_knowledge("brief"),
            KeyCode::Char('K') => self.init_knowledge(),
            KeyCode::Char('e') => self.open_edit(),
            KeyCode::Char('m') => self.open_missions(),
            KeyCode::Char(':') => self.toggle_palette(),
            KeyCode::Esc => self.flash.clear(),
            _ => {}
        }
    }

    fn key_answer(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.mode = Mode::Inbox;
                self.input.clear();
            }
            KeyCode::Enter => {
                let answer = std::mem::take(&mut self.input);
                self.mode = Mode::Inbox;
                if answer.trim().is_empty() {
                    self.flash = "empty answer not sent".into();
                    return;
                }
                self.attention_action(
                    methods::ATTENTION_ANSWER,
                    json!({"answer": answer.trim()}),
                    "answered",
                );
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) => self.input.push(c),
            _ => {}
        }
    }

    fn key_deny(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.mode = Mode::Inbox;
                self.input.clear();
            }
            KeyCode::Enter => {
                let reason = std::mem::take(&mut self.input);
                self.mode = Mode::Inbox;
                let mut params = json!({"decision": "deny"});
                if !reason.trim().is_empty() {
                    params["message"] = json!(reason.trim());
                }
                self.attention_action(methods::ATTENTION_DECIDE, params, "denied");
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) => self.input.push(c),
            _ => {}
        }
    }

    fn key_reject(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.mode = Mode::Inbox;
                self.input.clear();
            }
            KeyCode::Enter => {
                let reason = std::mem::take(&mut self.input);
                self.mode = Mode::Inbox;
                let reason = reason.trim();
                self.attention_action(
                    methods::KNOWLEDGE_REJECT,
                    json!({"reason": (!reason.is_empty()).then_some(reason)}),
                    "rejected; it won't be proposed again",
                );
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) => self.input.push(c),
            _ => {}
        }
    }

    /// Accepts a knowledge proposal, as proposed or as edited.
    fn accept_proposal(&mut self, id: &str, text: Option<String>) {
        let r = self.rpc(methods::KNOWLEDGE_ACCEPT, json!({"id": id, "text": text}));
        if let Some(v) = self.report(r) {
            let done: KnowledgeAccepted = serde_json::from_value(v).unwrap_or_default();
            let edited = if text.is_some() { "edited and " } else { "" };
            self.flash = format!(
                "{edited}accepted · written to {}{}",
                done.file,
                match &done.note {
                    Some(n) => format!(" · {n}"),
                    None if done.committed => " and committed".into(),
                    None => String::new(),
                }
            );
            let r = self.refresh();
            self.report(r);
        }
    }

    /// The project the rules view sets rules for; None for every project.
    fn rules_project(&self) -> Option<String> {
        if self.rules_everywhere {
            None
        } else {
            self.selected_project().map(|p| p.id.clone())
        }
    }

    fn open_rules(&mut self) {
        self.mode = Mode::Rules;
        self.flash.clear();
        self.rules_everywhere = self.selected_project().is_none();
        let r = self.load_rules();
        self.report(r);
    }

    fn load_rules(&mut self) -> Result<()> {
        let list: RuleList = serde_json::from_value(self.rpc(
            methods::RULES_LIST,
            json!({"project": self.rules_project()}),
        )?)?;
        self.rules = list.rules;
        self.rules_sel = self.rules_sel.min(self.rules.len().saturating_sub(1));
        Ok(())
    }

    /// Sets what a rule does, where the rules view sets rules.
    fn set_rule(&mut self, id: &str, project: Option<String>, action: &str) -> bool {
        let r = self.rpc(
            methods::RULES_SET,
            json!({"id": id, "project": project, "action": action}),
        );
        if self.report(r).is_none() {
            return false;
        }
        let scope = project
            .map(|p| format!("in {p}"))
            .unwrap_or_else(|| "everywhere".into());
        self.flash = format!("{id}: {action} {scope}");
        true
    }

    /// Moves the selected project's autonomy level one step up or down the dial.
    fn shift_level(&mut self, step: i32) {
        let Some(p) = self.selected_project().cloned() else {
            self.flash = "select a project first".into();
            return;
        };
        let levels = corvx_proto::autonomy::ALL;
        let at = levels.iter().position(|l| *l == p.autonomy).unwrap_or(1) as i32;
        let to = (at + step).clamp(0, levels.len() as i32 - 1) as usize;
        if to as i32 == at {
            self.flash = format!("{} is already {}", p.id, p.autonomy);
            return;
        }
        let r = self.rpc(
            methods::PROJECT_AUTONOMY,
            json!({"id": p.id, "level": levels[to]}),
        );
        if self.report(r).is_some() {
            self.flash = format!(
                "{}: {} · {}",
                p.id,
                levels[to],
                crate::level_meaning(levels[to])
            );
            let r = self.refresh();
            self.report(r);
            let r = self.load_rules();
            self.report(r);
        }
    }

    fn key_rules(&mut self, code: KeyCode) {
        let max = self.rules.len().saturating_sub(1);
        let action = match code {
            KeyCode::Esc | KeyCode::Char('l') => {
                self.mode = Mode::Normal;
                return;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.rules_sel = (self.rules_sel + 1).min(max);
                self.flash.clear();
                return;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.rules_sel = self.rules_sel.saturating_sub(1);
                self.flash.clear();
                return;
            }
            KeyCode::Char('g') => {
                if self.selected_project().is_some() {
                    self.rules_everywhere = !self.rules_everywhere;
                }
                let r = self.load_rules();
                self.report(r);
                return;
            }
            KeyCode::Char(c @ ('+' | '-' | '=')) => {
                self.shift_level(if c == '-' { -1 } else { 1 });
                return;
            }
            KeyCode::Char('h') => "ask",
            KeyCode::Char('y') => "allow",
            KeyCode::Char('n') => "deny",
            KeyCode::Char('o') => "off",
            KeyCode::Char('d') => "default",
            _ => return,
        };
        let Some(rule) = self.rules.get(self.rules_sel).cloned() else {
            return;
        };
        if self.set_rule(&rule.id, self.rules_project(), action) {
            let r = self.load_rules();
            self.report(r);
        }
    }

    fn open_sessions(&mut self) {
        self.mode = Mode::Sessions;
        self.flash.clear();
    }

    /// Resumes the selected session, and the lost agents under it.
    fn resume_selected(&mut self) {
        let Some(s) = self.sessions.get(self.sessions_sel).cloned() else {
            return;
        };
        if s.state == SessionState::Running {
            self.flash = format!("{} is running", crate::display_name(&s));
            return;
        }
        let r = self.rpc(methods::SESSION_RESUME, json!({"id": s.id}));
        if let Some(v) = self.report(r) {
            let n = v["sessions"].as_array().map(Vec::len).unwrap_or(0);
            self.flash = match n {
                0 | 1 => format!("resumed {}", crate::display_name(&s)),
                n => format!(
                    "resumed {} and {} agent(s) under it",
                    crate::display_name(&s),
                    n - 1
                ),
            };
            let r = self.refresh();
            self.report(r);
        }
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
                Some(s) if s.state == SessionState::Lost => {
                    self.flash = format!("{} was interrupted · R resumes it", s.id)
                }
                Some(s) => self.flash = format!("{} has ended; nothing to attach to", s.id),
                None => {}
            },
            KeyCode::Char('R') => self.resume_selected(),
            KeyCode::Char('a') => {
                if let Some(s) = self.sessions.get(self.sessions_sel).cloned() {
                    self.set_autopilot(&s.id, !s.autopilot);
                }
            }
            KeyCode::Char('x') => {
                if let Some(s) = self.sessions.get(self.sessions_sel).cloned() {
                    self.ask_stop(&s, Mode::Sessions);
                }
            }
            KeyCode::Char('c') => {
                if let Some(id) = self.sessions.get(self.sessions_sel).map(|s| s.id.clone()) {
                    self.cleanup(&id);
                }
            }
            _ => {}
        }
    }

    /// Asks before stopping a running session.
    fn ask_stop(&mut self, s: &Session, back: Mode) {
        if s.state != SessionState::Running {
            self.flash = format!("{} is not running", s.id);
            return;
        }
        self.stop_target = Some((s.id.clone(), back));
        self.mode = Mode::ConfirmStop;
    }

    fn key_confirm_stop(&mut self, code: KeyCode) {
        let Some((id, back)) = self.stop_target.take() else {
            self.mode = Mode::Normal;
            return;
        };
        self.mode = back;
        if code != KeyCode::Char('y') {
            self.flash = "not stopped".into();
            return;
        }
        let r = self.rpc(methods::SESSION_STOP, json!({"id": id}));
        if self.report(r).is_some() {
            self.flash = format!("stopped {id} · c removes its merged worktrees");
            let r = self.refresh();
            self.report(r);
        }
    }

    /// Removes the merged worktrees of a finished session and its agents.
    fn cleanup(&mut self, id: &str) {
        let r = self
            .rpc(methods::SESSION_CLEANUP, json!({"id": id}))
            .and_then(|v| Ok(serde_json::from_value::<corvx_proto::SessionCleanup>(v)?));
        let Some(done) = self.report(r) else {
            return;
        };
        self.flash = match (done.removed.len(), done.kept.as_slice()) {
            (0, []) => format!("{id}: no worktrees left to clean up"),
            (n, []) => format!("{id}: removed {n} merged worktree(s) and their branches"),
            (n, kept) => format!(
                "{id}: removed {n} · kept {}: {}",
                kept.len(),
                kept.join("; ")
            ),
        };
        let r = self.refresh();
        self.report(r);
    }

    fn open_missions(&mut self) {
        let Some(p) = self.selected_project().cloned() else {
            self.flash = "add a project first: a".into();
            return;
        };
        if self.missions_project != p.id {
            self.missions_sel = 0;
            self.journal_scroll = 0;
        }
        self.missions_project = p.id;
        self.mode = Mode::Missions;
        self.flash.clear();
        self.load_missions();
    }

    fn load_missions(&mut self) {
        // Keep the same mission selected as the list changes.
        let selected = self
            .missions
            .get(self.missions_sel)
            .and_then(|m| m.session.as_ref().map(|s| s.id.clone()));
        let project = self.missions_project.clone();
        let r = crate::load_missions(&mut self.client, &project);
        if let Some(list) = self.report(r) {
            self.missions = list;
        }
        if let Some(i) = selected.and_then(|id| {
            self.missions
                .iter()
                .position(|m| m.session.as_ref().is_some_and(|s| s.id == id))
        }) {
            self.missions_sel = i;
        }
        self.missions_sel = self.missions_sel.min(self.missions.len().saturating_sub(1));
    }

    fn key_missions(&mut self, code: KeyCode) {
        let max = self.missions.len().saturating_sub(1);
        let selected = self.missions.get(self.missions_sel).cloned();
        let session = selected.as_ref().and_then(|m| m.session.clone());
        match code {
            KeyCode::Esc | KeyCode::Char('m') => self.mode = Mode::Normal,
            KeyCode::Char('j') | KeyCode::Down => {
                self.missions_sel = (self.missions_sel + 1).min(max);
                self.journal_scroll = 0;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.missions_sel = self.missions_sel.saturating_sub(1);
                self.journal_scroll = 0;
            }
            KeyCode::Char('J') | KeyCode::PageDown | KeyCode::Char(' ') => {
                self.journal_scroll = self.journal_scroll.saturating_add(10)
            }
            KeyCode::Char('K') | KeyCode::PageUp => {
                self.journal_scroll = self.journal_scroll.saturating_sub(10)
            }
            KeyCode::Enter => match &session {
                Some(s) if s.state == SessionState::Running => {
                    self.pending_attach = Some(s.id.clone())
                }
                Some(s) if s.state == SessionState::Lost => {
                    self.flash = format!("{} was interrupted · R resumes it", s.id)
                }
                _ => self.flash = "it has ended · its journal entry is shown below".into(),
            },
            KeyCode::Char('R') => match &session {
                Some(s) if s.state == SessionState::Lost => {
                    let r = self.rpc(methods::SESSION_RESUME, json!({"id": s.id}));
                    if self.report(r).is_some() {
                        self.flash = format!("resumed {} and its interrupted agents", s.id);
                        let r = self.refresh();
                        self.report(r);
                    }
                }
                _ => self.flash = "only an interrupted mission can be resumed".into(),
            },
            KeyCode::Char('x') => match &session {
                Some(s) => self.ask_stop(s, Mode::Missions),
                None => self.flash = "nothing to stop".into(),
            },
            KeyCode::Char('c') => match &session {
                Some(s) => {
                    let id = s.id.clone();
                    self.cleanup(&id);
                }
                None => self.flash = "nothing to clean up".into(),
            },
            KeyCode::Char('o') => self.open_mission(),
            _ => {}
        }
    }

    /// Opens the form that renames the selected project or changes its workspace.
    fn open_edit(&mut self) {
        let Some(p) = self.selected_project().cloned() else {
            self.flash = "add a project first: a".into();
            return;
        };
        self.edit = Some(ProjectEdit {
            id: p.id,
            name: p.name,
            workspace: p.workspace,
            field: 0,
        });
        self.mode = Mode::EditProject;
        self.flash.clear();
    }

    /// The workspaces projects are in, for picking one with ←/→.
    fn workspaces(&self) -> Vec<String> {
        let mut all: Vec<String> = self.projects.iter().map(|p| p.workspace.clone()).collect();
        if !all.iter().any(|w| w == "default") {
            all.push("default".into());
        }
        all.sort();
        all.dedup();
        all
    }

    fn key_edit(&mut self, code: KeyCode) {
        let workspaces = self.workspaces();
        let Some(e) = &mut self.edit else {
            self.mode = Mode::Normal;
            return;
        };
        match code {
            KeyCode::Esc => {
                self.edit = None;
                self.mode = Mode::Normal;
                self.flash = "unchanged".into();
            }
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => e.field = 1 - e.field,
            KeyCode::Left | KeyCode::Right if e.field == 1 => {
                e.workspace = cycle(&workspaces, &e.workspace, code == KeyCode::Left);
            }
            KeyCode::Enter => self.save_edit(),
            KeyCode::Backspace => {
                if e.field == 0 {
                    e.name.pop();
                } else {
                    e.workspace.pop();
                }
            }
            KeyCode::Char(c) => {
                if e.field == 0 {
                    e.name.push(c);
                } else {
                    e.workspace.push(c);
                }
            }
            _ => {}
        }
    }

    /// Saves the edit form, and scans the project again while at it.
    fn save_edit(&mut self) {
        let Some(e) = self.edit.take() else {
            return;
        };
        if e.name.trim().is_empty() {
            self.flash = "a project needs a name".into();
            self.edit = Some(e);
            return;
        }
        self.mode = Mode::Normal;
        let before = self.projects.iter().find(|p| p.id == e.id).cloned();
        let r = self
            .rpc(
                methods::PROJECT_UPDATE,
                json!({"id": e.id, "name": e.name, "workspace": e.workspace, "rescan": true}),
            )
            .and_then(|v| Ok(serde_json::from_value::<Project>(v)?));
        if let Some(after) = self.report(r) {
            self.flash = describe_update(before.as_ref(), &after);
            let r = self.refresh();
            self.report(r);
            self.select_project(&after.id);
        }
    }

    /// Scans the selected project again: its stack, test and build commands, branch.
    fn rescan(&mut self) {
        let Some(p) = self.selected_project().cloned() else {
            self.flash = "add a project first: a".into();
            return;
        };
        let r = self
            .rpc(methods::PROJECT_UPDATE, json!({"id": p.id, "rescan": true}))
            .and_then(|v| Ok(serde_json::from_value::<Project>(v)?));
        if let Some(after) = self.report(r) {
            self.flash = describe_update(Some(&p), &after);
            let r = self.refresh();
            self.report(r);
        }
    }

    /// Stops the daemon and starts it again, to load a new build. Sessions in tmux keep
    /// running; ones without tmux would end, so it refuses while any run.
    fn restart_daemon(&mut self) {
        let native: Vec<String> = self
            .sessions
            .iter()
            .filter(|s| s.state == SessionState::Running && s.backend != "tmux")
            .map(|s| s.id.clone())
            .collect();
        if !native.is_empty() {
            self.flash = format!(
                "not restarted: {} run without tmux and would end · stop them first, or corvx daemon restart --force",
                native.join(", ")
            );
            return;
        }
        let old = self.daemon_pid;
        let r = corvx_client::stop_daemon(&self.data_dir)
            .and_then(|_| ensure_daemon(&self.data_dir, None));
        let Some(client) = self.report(r) else {
            return;
        };
        self.client = client;
        let r = self.refresh();
        if self.report(r).is_some() {
            self.flash = format!(
                "restarted the daemon: pid {old} → {} · sessions kept running",
                self.daemon_pid
            );
        }
    }

    /// Turns a session's autopilot on or off.
    fn set_autopilot(&mut self, id: &str, on: bool) {
        let name = self
            .sessions
            .iter()
            .find(|s| s.id == id)
            .map(crate::display_name)
            .unwrap_or_else(|| id.to_string());
        let r = self.rpc(methods::SESSION_AUTOPILOT, json!({"id": id, "on": on}));
        if self.report(r).is_some() {
            self.flash = if on {
                format!("autopilot on · {name} no longer asks for permission")
            } else {
                format!("autopilot off · {name} asks again")
            };
            let r = self.refresh();
            self.report(r);
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
            KeyCode::Char('j') | KeyCode::Down => {
                self.inbox_sel = (self.inbox_sel + 1).min(max);
                self.flash.clear();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.inbox_sel = self.inbox_sel.saturating_sub(1);
                self.flash.clear();
            }
            KeyCode::Char('a') => {
                self.attention_action(methods::ATTENTION_ACK, json!({}), "acknowledged")
            }
            // s goes to the sessions view, as it does from the main view.
            KeyCode::Char('s') => self.open_sessions(),
            KeyCode::Char('z') => self.attention_action(
                methods::ATTENTION_SNOOZE,
                json!({"minutes": 15}),
                "snoozed for 15 minutes",
            ),
            KeyCode::Char('r') => match self.attention.get(self.inbox_sel) {
                Some(item) if item.kind == "question" => {
                    self.mode = Mode::Answer;
                    self.input.clear();
                }
                Some(_) => self.flash = "only questions take an answer".into(),
                None => {}
            },
            KeyCode::Char(c @ ('y' | 'Y' | 'n')) => match self.attention.get(self.inbox_sel) {
                Some(item) if item.kind == "approval" => match c {
                    'y' => self.attention_action(
                        methods::ATTENTION_DECIDE,
                        json!({"decision": "allow"}),
                        "approved",
                    ),
                    'Y' => self.attention_action(
                        methods::ATTENTION_DECIDE,
                        json!({"decision": "always"}),
                        "approved, and won't ask again",
                    ),
                    _ => {
                        self.mode = Mode::Deny;
                        self.input.clear();
                    }
                },
                Some(item) if item.kind == "proposal" => {
                    if c == 'n' {
                        self.mode = Mode::Reject;
                        self.input.clear();
                    } else {
                        let id = item.id.clone();
                        self.accept_proposal(&id, None);
                    }
                }
                Some(item) if item.kind == "suggestion" => {
                    if c == 'n' {
                        self.attention_action(
                            methods::ATTENTION_DECIDE,
                            json!({"decision": "deny"}),
                            "declined; CORVX keeps asking",
                        )
                    } else {
                        self.attention_action(
                            methods::ATTENTION_DECIDE,
                            json!({"decision": "allow"}),
                            "allowed without asking from now on",
                        )
                    }
                }
                Some(_) => self.flash = "only permission requests take y / n".into(),
                None => {}
            },
            // Edit a proposal in $EDITOR, then accept it.
            KeyCode::Char('e') => match self.attention.get(self.inbox_sel) {
                Some(item) if item.kind == "proposal" => {
                    self.pending_edit = Some(item.id.clone());
                }
                Some(_) => self.flash = "e is for knowledge proposals".into(),
                None => {}
            },
            // A CORVX rule sent this to you: allow that rule from now on in this project.
            KeyCode::Char('L') => {
                let Some(item) = self.attention.get(self.inbox_sel).cloned() else {
                    return;
                };
                let Some(rule) = rule_of(&item) else {
                    self.flash = "L is for requests a CORVX rule sent you".into();
                    return;
                };
                if self.set_rule(&rule, Some(item.project.clone()), "allow") {
                    let flash = self.flash.clone();
                    self.attention_action(
                        methods::ATTENTION_DECIDE,
                        json!({"decision": "allow"}),
                        "approved",
                    );
                    self.flash = format!("approved · {flash} from now on");
                }
            }
            // Approve this request and every later one from the same agent.
            KeyCode::Char('A') => match self.attention.get(self.inbox_sel).cloned() {
                Some(item) if item.kind == "approval" => {
                    // You approve this one yourself: autopilot does not decide a request a
                    // CORVX rule sent to you.
                    self.attention_action(
                        methods::ATTENTION_DECIDE,
                        json!({"decision": "allow"}),
                        "approved",
                    );
                    self.set_autopilot(&item.session_id, true)
                }
                Some(_) => self.flash = "A is for permission requests".into(),
                None => {}
            },
            KeyCode::Char('x') => self.attention_action(
                methods::ATTENTION_DISMISS,
                json!({"reason": "dismissed in the terminal UI"}),
                "dismissed",
            ),
            // Enter attaches to the agent the item is about, e.g. to approve a request.
            KeyCode::Enter => {
                if let Some(item) = self.attention.get(self.inbox_sel) {
                    let running = self
                        .sessions
                        .iter()
                        .any(|s| s.id == item.session_id && s.state == SessionState::Running);
                    if running {
                        self.pending_attach = Some(item.session_id.clone());
                    } else {
                        self.flash = format!("{} has ended; nothing to attach to", item.session_id);
                    }
                }
            }
            KeyCode::Char('p') => {
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
            KeyCode::Enter => self.preview_add(),
            KeyCode::Tab => {
                let (completed, choices) = complete_dir(&self.input);
                self.input = completed;
                self.flash = match choices.len() {
                    0 | 1 => String::new(),
                    n => {
                        let shown: Vec<_> = choices.iter().take(6).cloned().collect();
                        let more = if n > 6 {
                            format!(" … {} more", n - 6)
                        } else {
                            String::new()
                        };
                        format!("{}{more}", shown.join("  "))
                    }
                };
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) => self.input.push(c),
            _ => {}
        }
    }

    /// Looks at the typed path before adding it: what it is built with, whether it is a git
    /// repository, whether CORVX already knows it.
    fn preview_add(&mut self) {
        let expanded = expand_home(self.input.trim());
        if expanded.is_empty() {
            self.flash = "type the path of a project directory".into();
            return;
        }
        let path = match std::fs::canonicalize(&expanded) {
            Ok(p) if p.is_dir() => p,
            Ok(_) => {
                self.flash = format!("not a directory: {expanded}");
                return;
            }
            Err(_) => {
                self.flash = format!("no such path: {expanded}");
                return;
            }
        };
        let git = path.join(".git").exists();
        let wired = corvx_core::knowledge::wired(&path);
        let existing = self
            .projects
            .iter()
            .find(|p| Path::new(&p.path) == path)
            .map(|p| p.id.clone());
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // New projects go where the selected one is, unless you pick another.
        let workspace = self
            .selected_project()
            .map(|p| p.workspace.clone())
            .unwrap_or_else(|| "default".into());
        self.preview = Some(AddPreview {
            scan: corvx_core::scan::scan(&path),
            path,
            git,
            wired,
            existing,
            knowledge: git && !wired,
            name,
            workspace,
            field: 0,
        });
        self.mode = Mode::AddPreview;
        self.flash.clear();
    }

    fn key_preview(&mut self, code: KeyCode) {
        let workspaces = self.workspaces();
        let Some(p) = &mut self.preview else {
            self.mode = Mode::Normal;
            return;
        };
        // A project already added has nothing to fill in.
        let fields = if p.existing.is_some() {
            1
        } else {
            PREVIEW_FIELDS
        };
        match code {
            KeyCode::Esc => self.mode = Mode::AddPath,
            KeyCode::Tab | KeyCode::Down => p.field = (p.field + 1) % fields,
            KeyCode::BackTab | KeyCode::Up => p.field = (p.field + fields - 1) % fields,
            KeyCode::Char(' ') if p.field == 2 => p.knowledge = !p.knowledge,
            KeyCode::Left | KeyCode::Right if p.field == 1 => {
                p.workspace = cycle(&workspaces, &p.workspace, code == KeyCode::Left);
            }
            KeyCode::Backspace if p.field == 0 => {
                p.name.pop();
            }
            KeyCode::Backspace if p.field == 1 => {
                p.workspace.pop();
            }
            KeyCode::Char(c) if p.field == 0 => p.name.push(c),
            KeyCode::Char(c) if p.field == 1 => p.workspace.push(c),
            KeyCode::Enter => {
                let Some(preview) = self.preview.take() else {
                    self.mode = Mode::Normal;
                    return;
                };
                self.mode = Mode::Normal;
                self.input.clear();
                if let Some(id) = &preview.existing {
                    self.select_project(id);
                    self.flash = format!("{id} is already added");
                    return;
                }
                self.add_project(&preview);
            }
            _ => {}
        }
    }

    fn select_project(&mut self, id: &str) {
        if let Some(i) = self.projects.iter().position(|x| x.id == id) {
            self.selected = i;
        }
    }

    /// Sets up the selected project's knowledge files and points its CLAUDE.md at them.
    fn init_knowledge(&mut self) {
        let Some(p) = self.selected_project().cloned() else {
            self.flash = "add a project first: a".into();
            return;
        };
        let r = self
            .rpc(methods::KNOWLEDGE_INIT, json!({"project": p.id}))
            .and_then(|v| Ok(serde_json::from_value::<KnowledgeInit>(v)?));
        if let Some(done) = self.report(r) {
            self.flash = knowledge_flash(&p.id, &done);
            let r = self.refresh();
            self.report(r);
        }
    }

    /// Opens one of the selected project's knowledge files (brief, decisions, conventions)
    /// in $EDITOR.
    fn edit_knowledge(&mut self, file: &'static str) {
        let Some(p) = self.selected_project().cloned() else {
            self.flash = "add a project first: a".into();
            return;
        };
        let path = Path::new(&p.path).join(corvx_core::knowledge::rel_path(file));
        if !path.is_file() {
            self.flash = format!("{} has no knowledge files yet · K sets them up", p.id);
            return;
        }
        self.pending_file = Some((p.id.clone(), path, file));
    }

    fn after_editing(&mut self, project: &str, path: &Path, file: &str, changed: bool) {
        if !changed {
            self.flash = format!("{}: unchanged", path.display());
            return;
        }
        let r = self
            .rpc(
                methods::KNOWLEDGE_COMMIT,
                json!({"project": project, "file": file}),
            )
            .and_then(|v| Ok(serde_json::from_value::<KnowledgeInit>(v)?));
        if let Some(done) = self.report(r) {
            let next = if file == "brief" {
                " · o starts a mission"
            } else {
                ""
            };
            self.flash = match (done.committed, done.note) {
                (true, _) => format!("{project}: {file} saved and committed{next}"),
                (false, Some(note)) => format!("{project}: {file} saved · {note}"),
                (false, None) => format!("{project}: {file} saved"),
            };
            let r = self.refresh();
            self.report(r);
        }
    }

    fn open_mission(&mut self) {
        let Some(p) = self.selected_project().cloned() else {
            self.flash = "add a project first: a".into();
            return;
        };
        let k: Option<Knowledge> = self
            .rpc(
                methods::KNOWLEDGE_READ,
                json!({"project": p.id, "journal": 0}),
            )
            .ok()
            .and_then(|v| serde_json::from_value(v).ok());
        let (knowledge, wired) = match &k {
            Some(k) => (knowledge_summary(k), k.wired),
            None => (
                "unknown (the daemon is older than this UI)".to_string(),
                true,
            ),
        };
        let level = corvx_proto::autonomy::ALL
            .iter()
            .position(|l| *l == p.autonomy)
            .unwrap_or(1);
        self.mission = Some(MissionForm {
            project: p.id,
            goal: String::new(),
            field: 0,
            level,
            model: 0,
            agent_model: 0,
            human_approves: false,
            knowledge,
            wired,
        });
        self.mode = Mode::Mission;
        self.flash.clear();
    }

    fn key_mission(&mut self, code: KeyCode) {
        let Some(m) = &mut self.mission else {
            self.mode = Mode::Normal;
            return;
        };
        let step = |at: usize, len: usize, back: bool| {
            if back {
                (at + len - 1) % len
            } else {
                (at + 1) % len
            }
        };
        match code {
            KeyCode::Esc => {
                self.mission = None;
                self.mode = Mode::Normal;
                self.flash = "no mission started".into();
            }
            KeyCode::Enter => self.start_mission(),
            KeyCode::Tab | KeyCode::Down => m.field = (m.field + 1) % MISSION_FIELDS,
            KeyCode::BackTab | KeyCode::Up => m.field = step(m.field, MISSION_FIELDS, true),
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if m.field > 0 => {
                let back = code == KeyCode::Left;
                match m.field {
                    1 => m.level = step(m.level, corvx_proto::autonomy::ALL.len(), back),
                    2 => m.model = step(m.model, ORCHESTRATOR_MODELS.len(), back),
                    3 => m.agent_model = step(m.agent_model, AGENT_MODELS.len(), back),
                    _ => m.human_approves = !m.human_approves,
                }
            }
            KeyCode::Backspace if m.field == 0 => {
                m.goal.pop();
            }
            KeyCode::Char(c) if m.field == 0 => m.goal.push(c),
            _ => {}
        }
    }

    fn start_mission(&mut self) {
        let Some(m) = self.mission.take() else {
            return;
        };
        let goal = m.goal.trim().to_string();
        if goal.is_empty() {
            self.flash = "say what the mission should achieve".into();
            self.mission = Some(m);
            return;
        }
        self.mode = Mode::Normal;
        let level = corvx_proto::autonomy::ALL[m.level];
        let current = self
            .projects
            .iter()
            .find(|p| p.id == m.project)
            .map(|p| p.autonomy.clone());
        if current.as_deref() != Some(level) {
            let r = self.rpc(
                methods::PROJECT_AUTONOMY,
                json!({"id": m.project, "level": level}),
            );
            if self.report(r).is_none() {
                return;
            }
        }
        let (model, agent_model) = (ORCHESTRATOR_MODELS[m.model], AGENT_MODELS[m.agent_model]);
        let r = crate::start_orchestrator(
            &mut self.client,
            &m.project,
            &goal,
            model,
            agent_model,
            m.human_approves,
        );
        let Some(started) = self.report(r) else {
            return;
        };
        let id = started.session.id;
        let r = self.refresh();
        self.report(r);
        self.open_sessions();
        if let Some(i) = self.sessions.iter().position(|s| s.id == id) {
            self.sessions_sel = i;
        }
        self.flash = format!(
            "started orchestrator {id} on {model} ({level}) · Enter watches it · i inbox · Esc back"
        );
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
            Action::StartMission => self.open_mission(),
            Action::SetUpKnowledge => self.init_knowledge(),
            Action::EditBrief => self.edit_knowledge("brief"),
            Action::EditDecisions => self.edit_knowledge("decisions"),
            Action::EditConventions => self.edit_knowledge("conventions"),
            Action::EditProject => self.open_edit(),
            Action::Rescan => self.rescan(),
            Action::Missions => self.open_missions(),
            Action::RemoveProject => self.start_remove(),
            Action::Refresh => {
                let r = self.refresh();
                self.report(r);
            }
            Action::RestartDaemon => self.restart_daemon(),
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

    fn add_project(&mut self, preview: &AddPreview) {
        let r = self
            .rpc(
                methods::PROJECT_ADD,
                json!({"path": preview.path.display().to_string(),
                       "name": preview.name.trim(), "workspace": preview.workspace.trim()}),
            )
            .and_then(|v| Ok(serde_json::from_value::<Project>(v)?));
        let Some(p) = self.report(r) else {
            return;
        };
        self.flash = format!("added {} · o starts a mission", p.id);
        if preview.knowledge {
            let r = self
                .rpc(methods::KNOWLEDGE_INIT, json!({"project": p.id}))
                .and_then(|v| Ok(serde_json::from_value::<KnowledgeInit>(v)?));
            if let Some(done) = self.report(r) {
                self.flash = format!("added {} · {}", p.id, knowledge_flash(&p.id, &done));
            }
        }
        let r = self.refresh();
        self.report(r);
        self.select_project(&p.id);
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

/// The next (or previous) choice after `current`; the first one if it is not a choice.
fn cycle(choices: &[String], current: &str, back: bool) -> String {
    if choices.is_empty() {
        return current.to_string();
    }
    let n = choices.len();
    let next = match choices.iter().position(|c| c == current) {
        Some(i) if back => (i + n - 1) % n,
        Some(i) => (i + 1) % n,
        None => 0,
    };
    choices[next].clone()
}

/// What changed in a project, for the status line.
fn describe_update(before: Option<&Project>, after: &Project) -> String {
    let mut changes = Vec::new();
    if let Some(b) = before {
        if b.name != after.name {
            changes.push(format!("renamed to \"{}\"", after.name));
        }
        if b.workspace != after.workspace {
            changes.push(format!("now in workspace {}", after.workspace));
        }
        let or = |v: &Option<String>| v.clone().unwrap_or_else(|| "none".into());
        if b.stack != after.stack {
            changes.push(format!("stack {}", after.stack.join(", ")));
        }
        if b.test_command != after.test_command {
            changes.push(format!("test: {}", or(&after.test_command)));
        }
        if b.build_command != after.build_command {
            changes.push(format!("build: {}", or(&after.build_command)));
        }
        if b.branch != after.branch {
            changes.push(format!("branch {}", or(&after.branch)));
        }
    }
    if changes.is_empty() {
        format!("{}: unchanged · the scan found nothing new", after.id)
    } else {
        format!("{}: {}", after.id, changes.join(" · "))
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

/// Completes the last part of a typed path to the directories it can name. Returns the new
/// input and the directories that matched.
fn complete_dir(input: &str) -> (String, Vec<String>) {
    let cut = input.rfind('/').map(|i| i + 1).unwrap_or(0);
    let (head, prefix) = input.split_at(cut);
    let dir = if head.is_empty() {
        ".".to_string()
    } else {
        expand_home(head)
    };
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| {
                    n.starts_with(prefix) && (prefix.starts_with('.') || !n.starts_with('.'))
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    match names.as_slice() {
        [] => (input.to_string(), names),
        [only] => (format!("{head}{only}/"), names),
        [first, rest @ ..] => {
            let mut common = first.clone();
            for n in rest {
                while !n.starts_with(&common) {
                    common.pop();
                }
            }
            (format!("{head}{common}"), names)
        }
    }
}

/// What setting up knowledge did, for the status line.
fn knowledge_flash(project: &str, done: &KnowledgeInit) -> String {
    if done.written.is_empty() {
        return format!("{project}: knowledge already set up · b edits the brief");
    }
    let committed = if done.committed {
        " and committed".to_string()
    } else {
        done.note
            .as_deref()
            .map(|n| format!(" ({n})"))
            .unwrap_or_default()
    };
    format!("knowledge set up{committed} · b writes the brief")
}

/// What an orchestrator will start from, in one line.
fn knowledge_summary(k: &Knowledge) -> String {
    let sections = |name: &str| {
        k.files
            .iter()
            .find(|f| f.name == name && f.exists)
            .map(|f| f.text.lines().filter(|l| l.starts_with("## ")).count())
            .unwrap_or(0)
    };
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    format!(
        "{} · {} · {}",
        plural(sections("decisions"), "decision", "decisions"),
        plural(sections("conventions"), "convention", "conventions"),
        plural(k.journal_count, "past mission", "past missions")
    )
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
        Mode::Inbox | Mode::Answer | Mode::Deny | Mode::Reject => draw_inbox(f, main, app),
        Mode::Sessions => draw_sessions(f, main, app),
        Mode::Rules => draw_rules(f, main, app),
        Mode::Missions => draw_missions(f, main, app),
        Mode::ConfirmStop => match app.stop_target.as_ref().map(|(_, back)| *back) {
            Some(Mode::Missions) => draw_missions(f, main, app),
            _ => draw_sessions(f, main, app),
        },
        _ => draw_main(f, main, app),
    }
    draw_status(f, status, app);
    draw_input(f, input, app);

    match app.mode {
        Mode::Palette => draw_palette(f, body, app),
        Mode::Help => draw_help(f, body),
        Mode::Mission => draw_mission(f, body, app),
        Mode::EditProject => draw_edit(f, body, app),
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
            format!("{} to approve  ", count_yours_to_approve(app)),
            Style::default().fg(AMBER),
        ),
        Span::styled(
            match count_kind(app, "approval") - count_yours_to_approve(app) {
                0 => String::new(),
                n => format!("({n} with orchestrators)  "),
            },
            Style::default().fg(DIM),
        ),
        Span::styled(
            format!(
                "{} blocked  ",
                count_kind(app, "blocked") + count_kind(app, "question")
            ),
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
        Span::styled(
            if app.newer_build && app.connected {
                "  ↻ newer build: restart it from : (sessions keep running)"
            } else {
                ""
            },
            Style::default().fg(AMBER),
        ),
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
    if app.mode == Mode::AddPreview {
        if let Some(p) = &app.preview {
            draw_preview(f, area, p);
            return;
        }
    }
    let block = Block::bordered()
        .title(" project ")
        .border_style(Style::default().fg(BORDER));
    let Some(p) = app.selected_project() else {
        f.render_widget(Paragraph::new(welcome_lines()).block(block), area);
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
        Line::from(vec![
            label("workspace"),
            Span::raw(p.workspace.clone()),
            Span::styled("  · e renames or moves it", Style::default().fg(DIM)),
        ]),
        Line::from(vec![
            label("autonomy"),
            Span::styled(p.autonomy.clone(), level_style(&p.autonomy)),
            Span::styled("  · l to change", Style::default().fg(DIM)),
        ]),
        {
            let waiting = app.proposals.values().filter(|x| x.project == p.id).count();
            let k = app.knowledge_state.get(&p.id).copied().unwrap_or_default();
            Line::from(vec![
                label("knowledge"),
                if !k.wired {
                    Span::styled("not set up · K sets it up", Style::default().fg(AMBER))
                } else if waiting > 0 {
                    Span::styled(
                        format!(
                            "{waiting} proposal{} waiting in the inbox",
                            if waiting == 1 { "" } else { "s" }
                        ),
                        kind_style("proposal"),
                    )
                } else if !k.brief_written {
                    Span::styled(
                        "set up · brief not written yet: b",
                        Style::default().fg(AMBER),
                    )
                } else {
                    Span::raw("set up · no proposals waiting")
                },
                Span::styled(
                    format!("  · corvx knowledge {}", p.id),
                    Style::default().fg(DIM),
                ),
            ])
        },
        {
            let k = app.knowledge_state.get(&p.id).copied().unwrap_or_default();
            Line::from(vec![
                label("missions"),
                Span::raw(match k.journal {
                    0 => "none yet".to_string(),
                    1 => "1 journal entry".to_string(),
                    n => format!("{n} journal entries"),
                }),
                Span::styled("  · m shows them", Style::default().fg(DIM)),
            ])
        },
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
            Span::styled(
                if p.test_command.is_none() {
                    "  · : rescan, after adding tests"
                } else {
                    ""
                },
                Style::default().fg(DIM),
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
    lines.push(Line::styled(next_step(app, p), Style::default().fg(GREEN)));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// The one thing to do next on a project, so a new one is never a dead end.
fn next_step(app: &App, p: &Project) -> String {
    let k = app.knowledge_state.get(&p.id).copied().unwrap_or_default();
    let running = app
        .sessions
        .iter()
        .any(|s| s.project == p.id && s.state == SessionState::Running);
    if !k.wired {
        "next: K sets up the knowledge files every agent reads, then o starts a mission".into()
    } else if !k.brief_written {
        "next: b writes the brief (what it is, what matters now), then o starts a mission".into()
    } else if running {
        "o starts another mission · s shows the sessions · i the inbox".into()
    } else {
        "next: o starts a mission".into()
    }
}

fn welcome_lines() -> Vec<Line<'static>> {
    let step = |key: &'static str, what: &'static str| {
        Line::from(vec![
            Span::styled(
                format!("  {key:<3}"),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(what),
        ])
    };
    vec![
        Line::styled(
            "Welcome to CORVX.",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
        Line::styled(
            "It runs coding agents on your projects and comes to you only when it needs you.",
            Style::default().fg(DIM),
        ),
        Line::from(""),
        step("a", "add a project: a git repository you work on"),
        step("b", "write its brief: what it is, and what matters now"),
        step(
            "o",
            "start a mission: a goal; an orchestrator plans it and runs agents",
        ),
        step(
            "i",
            "your inbox: approvals, questions, results, things to remember",
        ),
        Line::from(""),
        Line::styled(
            "? all keys · q quits this view (agents keep running)",
            Style::default().fg(DIM),
        ),
    ]
}

/// Splits text into lines of at most `width` characters, at spaces.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// What adding the typed path would add.
fn draw_preview(f: &mut Frame, area: Rect, p: &AddPreview) {
    let label = |s: &str| Span::styled(format!("{s:<11}"), Style::default().fg(DIM));
    let or = |v: &Option<String>, none: &str| v.clone().unwrap_or_else(|| none.to_string());
    let field = |i: usize, text: &str| {
        if p.field == i && p.existing.is_none() {
            Span::styled(
                format!("{text}█"),
                Style::default().add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw(text.to_string())
        }
    };
    let mut lines = vec![
        Line::from(vec![label("name"), field(0, &p.name)]),
        Line::from(vec![
            label("workspace"),
            field(1, &p.workspace),
            Span::styled(
                if p.field == 1 {
                    "  ←/→ existing ones"
                } else {
                    ""
                },
                Style::default().fg(DIM),
            ),
        ]),
        Line::from(vec![label("path"), Span::raw(p.path.display().to_string())]),
        Line::from(vec![
            label("stack"),
            Span::raw(if p.scan.stack.is_empty() {
                "not detected".to_string()
            } else {
                p.scan.stack.join(", ")
            }),
        ]),
        Line::from(vec![
            label("test"),
            Span::raw(or(&p.scan.test_command, "not detected")),
        ]),
        Line::from(vec![
            label("build"),
            Span::raw(or(&p.scan.build_command, "not detected")),
        ]),
        Line::from(vec![
            label("git"),
            if p.git {
                Span::raw(format!("yes, on {}", or(&p.scan.branch, "a detached HEAD")))
            } else {
                Span::styled(
                    "no: agents need git for their own worktrees (git init first)",
                    Style::default().fg(AMBER),
                )
            },
        ]),
        Line::from(""),
    ];
    if let Some(id) = &p.existing {
        lines.push(Line::styled(
            format!("Already added as {id}. Enter selects it."),
            Style::default().fg(AMBER),
        ));
    } else if p.wired {
        lines.push(Line::styled(
            "Its knowledge files are already set up.",
            Style::default().fg(GREEN),
        ));
    } else {
        let mark = if p.knowledge { "[x]" } else { "[ ]" };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{mark} "),
                Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
            ),
            if p.field == 2 {
                Span::styled(
                    "set up its knowledge files",
                    Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                )
            } else {
                Span::raw("set up its knowledge files")
            },
        ]));
        lines.push(Line::styled(
            "    .corvx/brief, decisions, conventions and map, read by every agent through CLAUDE.md;",
            Style::default().fg(DIM),
        ));
        lines.push(Line::styled(
            "    committed to the current branch. Nothing else in the project is touched.",
            Style::default().fg(DIM),
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "Enter add · Tab next field · space (on the last) knowledge on/off · Esc back",
        Style::default().fg(DIM),
    ));
    let block = Block::bordered()
        .title(" add this project? ")
        .border_style(Style::default().fg(GREEN));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// The form that starts a mission.
fn draw_mission(f: &mut Frame, area: Rect, app: &App) {
    let Some(m) = &app.mission else {
        return;
    };
    let rect = centered(area, 78, 19);
    f.render_widget(Clear, rect);
    let label = |i: usize, s: &str| {
        let style = if m.field == i {
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM)
        };
        Span::styled(
            format!("{} {s:<13}", if m.field == i { "▸" } else { " " }),
            style,
        )
    };
    let choice = |i: usize, options: &[&str], at: usize| {
        let mut spans = Vec::new();
        for (j, o) in options.iter().enumerate() {
            let style = if j == at {
                if m.field == i {
                    Style::default()
                        .fg(Color::Black)
                        .bg(GREEN)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().add_modifier(Modifier::BOLD)
                }
            } else {
                Style::default().fg(DIM)
            };
            spans.push(Span::styled(format!(" {o} "), style));
            spans.push(Span::raw(" "));
        }
        spans
    };
    let cursor = if m.field == 0 { "█" } else { "" };
    let goal = if m.goal.is_empty() && m.field != 0 {
        Span::styled("(what should be achieved)", Style::default().fg(DIM))
    } else {
        Span::raw(format!("{}{cursor}", m.goal))
    };
    let levels = corvx_proto::autonomy::ALL;
    let row = |i: usize, name: &str, options: &[&str], at: usize| {
        let mut spans = vec![label(i, name)];
        spans.extend(choice(i, options, at));
        Line::from(spans)
    };
    let indent = 16;
    let width = (rect.width as usize).saturating_sub(indent + 2).max(20);
    let indented = |text: &str, style: Style| -> Vec<Line<'static>> {
        wrap(text, width)
            .into_iter()
            .map(|l| Line::styled(format!("{:indent$}{l}", ""), style))
            .collect()
    };
    let mut lines = vec![
        Line::from(vec![label(0, "goal"), goal]),
        Line::from(""),
        row(1, "autonomy", levels, m.level),
    ];
    lines.extend(indented(
        crate::level_meaning(levels[m.level]),
        Style::default().fg(DIM),
    ));
    lines.extend([
        row(2, "orchestrator", &ORCHESTRATOR_MODELS, m.model),
        row(3, "agents", &AGENT_MODELS, m.agent_model),
        row(
            4,
            "approvals",
            &["orchestrator decides", "you decide"],
            usize::from(m.human_approves),
        ),
        Line::from(""),
        Line::from(vec![
            Span::styled("  knowledge     ", Style::default().fg(DIM)),
            Span::raw(m.knowledge.clone()),
        ]),
    ]);
    if !m.wired {
        lines.extend(indented(
            "not set up: only the orchestrator reads it · Esc, then K sets it up",
            Style::default().fg(AMBER),
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "Enter start · Tab/↑↓ field · ←/→ change · Esc cancel",
        Style::default().fg(DIM),
    ));
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(format!(" start a mission on {} ", m.project))
                .border_style(Style::default().fg(GREEN)),
        ),
        rect,
    );
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let (label, color) = match app.mode {
        Mode::Normal => ("NORMAL", ACCENT),
        Mode::AddPath => ("ADD", GREEN),
        Mode::AddPreview => ("ADD", GREEN),
        Mode::Mission => ("MISSION", GREEN),
        Mode::Palette => ("PALETTE", Color::Rgb(188, 140, 255)),
        Mode::ConfirmRemove => ("CONFIRM", AMBER),
        Mode::Help => ("HELP", ACCENT),
        Mode::Inbox => ("INBOX", ACCENT),
        Mode::Sessions => ("SESSIONS", ACCENT),
        Mode::Answer => ("ANSWER", GREEN),
        Mode::Deny => ("DENY", RED),
        Mode::Reject => ("REJECT", RED),
        Mode::Rules => ("RULES", ACCENT),
        Mode::EditProject => ("EDIT", GREEN),
        Mode::Missions => ("MISSIONS", ACCENT),
        Mode::ConfirmStop => ("CONFIRM", AMBER),
    };
    let hint = if !app.flash.is_empty() {
        app.flash.clone()
    } else {
        match app.mode {
            Mode::Normal => {
                "j/k move · a add · o mission · m missions · b brief · e edit · i inbox · s sessions · l rules · : more · ? help · q quit"
                    .into()
            }
            Mode::AddPath => {
                "type the project's path · Tab completes · Enter look at it · Esc cancel".into()
            }
            Mode::AddPreview => {
                "Enter add · Tab/↑↓ field · ←/→ pick a workspace · space knowledge on/off · Esc back"
                    .into()
            }
            Mode::EditProject => {
                "type · Tab other field · ←/→ pick a workspace · Enter save (and rescan) · Esc cancel"
                    .into()
            }
            Mode::Missions => {
                "j/k move · J/K scroll journal · Enter attach · c clean up worktrees · x stop · R resume · o new · Esc"
                    .into()
            }
            Mode::ConfirmStop => "y stop it · any other key cancels".into(),
            Mode::Mission => {
                "type the goal · Tab/↑↓ field · ←/→ change · Enter start · Esc cancel".into()
            }
            Mode::Palette => "type to filter · ↑↓ select · Enter run · Esc close".into(),
            Mode::ConfirmRemove => "y remove from CORVX · any other key cancels".into(),
            Mode::Help => "any key closes".into(),
            Mode::Inbox => match app.attention.get(app.inbox_sel).map(|i| i.kind.as_str()) {
                Some("approval") => {
                    "y approve · Y always · n deny · A autopilot · Enter attach · j/k move · Esc"
                        .into()
                }
                Some("suggestion") => {
                    "y allow from now on · n keep asking · j/k move · Esc".into()
                }
                Some("proposal") => {
                    "y accept · e edit, then accept · n reject · z snooze · j/k move · Esc".into()
                }
                Some("question") => {
                    "r answer · Enter attach · a ack · z snooze · x dismiss · j/k move · Esc".into()
                }
                _ => "j/k move · Enter attach · a ack · z snooze · x dismiss · p project · Esc"
                    .into(),
            },
            Mode::Sessions => {
                "j/k move · Enter attach (Ctrl+\\ then d to come back) · R resume · a autopilot · x stop · c clean up · Esc"
                    .into()
            }
            Mode::Answer => "type your answer · Enter send · Esc cancel".into(),
            Mode::Deny => "why, or what to do instead (optional) · Enter deny · Esc cancel".into(),
            Mode::Reject => {
                "why (optional; agents see it, so they don't propose it again) · Enter reject · Esc cancel"
                    .into()
            }
            Mode::Rules => {
                "-/+ autonomy · h ask · y allow · n deny · o off · d default · g this project/everywhere · Esc"
                    .into()
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
        Mode::Answer => Line::from(vec![
            Span::styled("answer › ", Style::default().fg(GREEN)),
            Span::raw(format!("{}█", app.input)),
        ]),
        Mode::Deny => Line::from(vec![
            Span::styled("deny › ", Style::default().fg(RED)),
            Span::raw(format!("{}█", app.input)),
        ]),
        Mode::Reject => Line::from(vec![
            Span::styled("reject › ", Style::default().fg(RED)),
            Span::raw(format!("{}█", app.input)),
        ]),
        Mode::ConfirmStop => Line::styled(
            format!(
                "stop {}? What it committed so far is kept.",
                app.stop_target
                    .as_ref()
                    .map(|(id, _)| id.as_str())
                    .unwrap_or("")
            ),
            Style::default().fg(AMBER),
        ),
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
    let rect = centered(area, 76, 25);
    f.render_widget(Clear, rect);
    let rows = [
        ("j / k, ↑ ↓", "move selection"),
        ("a", "add a project directory (Tab completes the path)"),
        ("o", "start a mission on the selected project"),
        ("m", "its missions: history, journals, clean up worktrees"),
        ("b", "edit its brief in $EDITOR (committed when saved)"),
        (
            "e",
            "rename it or move it to another workspace (rescans it)",
        ),
        ("K", "set up its knowledge files, read by every agent"),
        ("d", "remove the selected project (files untouched)"),
        ("r", "refresh from the daemon"),
        (
            ": or Ctrl+K",
            "palette: decisions, conventions, rescan, restart daemon …",
        ),
        ("i", "attention inbox: what needs you"),
        ("s", "sessions: Enter attaches, Ctrl+\\ then d returns"),
        (
            "  in inbox",
            "y approve · Y always · n deny · A autopilot · z snooze",
        ),
        (
            "  proposals",
            "y accept · e edit, then accept · n reject, with why",
        ),
        (
            "  in sessions",
            "R resume · a autopilot on/off · x stop · c clean up",
        ),
        (
            "l",
            "rules and autonomy: what CORVX asks you, allows or denies",
        ),
        ("?", "this help"),
        ("q", "quit the UI (the daemon keeps running)"),
        ("", ""),
        (
            "CLI",
            "corvx project · orchestrate · missions · knowledge · daemon",
        ),
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

/// A permission request its agent's orchestrator decides. You may still decide it.
fn orchestrators(item: &AttentionItem) -> bool {
    item.kind == "approval" && item.severity < 3
}

/// Permission requests waiting on you, not on an orchestrator.
fn count_yours_to_approve(app: &App) -> usize {
    app.attention
        .iter()
        .filter(|i| i.kind == "approval" && !orchestrators(i))
        .count()
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
        "question" => "QUESTION",
        "approval" => "APPROVE?",
        "suggestion" => "SUGGEST",
        "proposal" => "LEARN?",
        "done" => "DONE",
        "failed" => "FAILED",
        _ => "?",
    }
}

fn kind_style(kind: &str) -> Style {
    let color = match kind {
        "blocked" => AMBER,
        "waiting" => ACCENT,
        "question" => AMBER,
        "approval" => AMBER,
        "suggestion" => GREEN,
        "proposal" => Color::Rgb(188, 140, 255),
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
        let (label_style, summary) = if orchestrators(item) {
            (
                Style::default().fg(DIM),
                Span::styled(
                    format!("orchestrator deciding · {}", item.summary),
                    Style::default().fg(DIM),
                ),
            )
        } else {
            (kind_style(&item.kind), Span::raw(item.summary.clone()))
        };
        lines.push(Line::from(vec![
            Span::styled(marker, row_style),
            Span::styled(format!("{:<8}", kind_label(&item.kind)), label_style),
            Span::styled(format!("{:<16}", item.project), Style::default().fg(ACCENT)),
            Span::styled(
                format!("{:>5}  ", age_label(now - item.created_at)),
                Style::default().fg(DIM),
            ),
            summary,
        ]));
    }
    // The selected item in full: a long command or question does not fit on its row.
    let detail = app.attention.get(app.inbox_sel).filter(|i| {
        matches!(
            i.kind.as_str(),
            "approval" | "question" | "blocked" | "suggestion" | "proposal"
        ) || i.summary.len() > 60
    });
    let Some(item) = detail else {
        f.render_widget(Paragraph::new(lines).block(block), area);
        return;
    };
    if item.kind == "proposal" {
        if let Some(p) = app.proposals.get(&item.id) {
            draw_proposal(f, area, lines, block, p, app);
            return;
        }
    }
    let [list_area, detail_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(7)]).areas(area);
    f.render_widget(Paragraph::new(lines).block(block), list_area);
    let agent = app
        .sessions
        .iter()
        .find(|s| s.id == item.session_id)
        .map(crate::display_name)
        .unwrap_or_else(|| item.session_id.clone());
    let title = match item.kind.as_str() {
        "approval" if orchestrators(item) => format!(" {agent} asks its orchestrator to run "),
        "approval" => format!(" {agent} asks to run "),
        "question" => format!(" {agent} asks "),
        _ => format!(" {agent} "),
    };
    let mut text = Vec::new();
    if let Some(note) = item.note.as_deref().filter(|_| item.kind == "approval") {
        text.push(Line::styled(note.to_string(), Style::default().fg(AMBER)));
    }
    text.push(Line::raw(item.summary.clone()));
    if orchestrators(item) {
        text.push(Line::styled(
            "Its orchestrator is deciding · you can still decide it: y / Y / n",
            Style::default().fg(DIM),
        ));
    } else if item.kind == "suggestion" {
        text.push(Line::styled(
            "y allow it without asking from now on · n keep asking (not offered again)",
            Style::default().fg(DIM),
        ));
    } else if let Some(rule) = rule_of(item) {
        text.push(Line::styled(
            format!(
                "y approve · n deny · L allow {rule} in {} from now on · Enter attach",
                item.project
            ),
            Style::default().fg(DIM),
        ));
    } else if item.kind == "approval" {
        text.push(Line::styled(
            "y approve · Y always · n deny · A autopilot · Enter attach",
            Style::default().fg(DIM),
        ));
    }
    let detail = Paragraph::new(text)
        .wrap(ratatui::widgets::Wrap { trim: false })
        .block(
            Block::bordered()
                .title(title)
                .border_style(kind_style(&item.kind)),
        );
    f.render_widget(detail, detail_area);
}

/// The inbox with a knowledge proposal selected: the list, and the proposal in full below it.
fn draw_proposal(
    f: &mut Frame,
    area: Rect,
    lines: Vec<Line>,
    block: Block,
    p: &Proposal,
    app: &App,
) {
    let mut text = vec![Line::from(vec![
        Span::styled(
            format!(".corvx/{}.md  ", p.file),
            Style::default().fg(ACCENT),
        ),
        Span::styled(
            p.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ])];
    if let Some(e) = &p.evidence {
        text.push(Line::styled(
            format!("evidence: {e}"),
            Style::default().fg(DIM),
        ));
    }
    text.push(Line::from(""));
    if let Some(old) = &p.replaces {
        text.push(Line::styled("replaces:", Style::default().fg(DIM)));
        for l in old.lines() {
            text.push(Line::styled(format!("- {l}"), Style::default().fg(RED)));
        }
        text.push(Line::styled("with:", Style::default().fg(DIM)));
        for l in p.text.lines() {
            text.push(Line::styled(format!("+ {l}"), Style::default().fg(GREEN)));
        }
    } else {
        text.extend(p.text.lines().map(|l| Line::raw(l.to_string())));
    }
    text.push(Line::from(""));
    text.push(Line::styled(
        "y accept (written to the file and committed) · e edit, then accept · n reject, with why",
        Style::default().fg(DIM),
    ));
    let height = (text.len() as u16 + 2).clamp(7, area.height.saturating_sub(5).max(7));
    let [list_area, detail_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(height)]).areas(area);
    f.render_widget(Paragraph::new(lines).block(block), list_area);
    let agent = app
        .sessions
        .iter()
        .find(|s| s.id == p.by)
        .map(crate::display_name)
        .unwrap_or_else(|| p.by.clone());
    let detail = Paragraph::new(text)
        .wrap(ratatui::widgets::Wrap { trim: false })
        .block(
            Block::bordered()
                .title(format!(" {agent} proposes to remember "))
                .border_style(kind_style("proposal")),
        );
    f.render_widget(detail, detail_area);
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

/// Orders sessions as a tree: each orchestrator followed by the agents it supervises,
/// running sessions before ended ones at each level. Returns the sessions and their depths.
fn session_tree(sessions: Vec<Session>) -> (Vec<Session>, Vec<usize>) {
    use std::collections::HashSet;
    fn visit(
        parent: Option<&str>,
        depth: usize,
        all: &[Session],
        ids: &HashSet<String>,
        order: &mut Vec<Session>,
        depths: &mut Vec<usize>,
    ) {
        let mut level: Vec<&Session> = all
            .iter()
            .filter(|s| match (parent, s.supervisor.as_deref()) {
                (None, None) => true,
                // A supervisor that is not in the list (e.g. long gone) makes this a root.
                (None, Some(sup)) => !ids.contains(sup),
                (Some(p), Some(sup)) => p == sup,
                _ => false,
            })
            .collect();
        level.sort_by_key(|s| s.state != SessionState::Running);
        for s in level {
            order.push(s.clone());
            depths.push(depth);
            visit(Some(&s.id), depth + 1, all, ids, order, depths);
        }
    }
    let ids: HashSet<String> = sessions.iter().map(|s| s.id.clone()).collect();
    let mut order = Vec::with_capacity(sessions.len());
    let mut depths = Vec::with_capacity(sessions.len());
    visit(None, 0, &sessions, &ids, &mut order, &mut depths);
    (order, depths)
}

fn agent_label(s: &Session) -> String {
    let state = if s.state == SessionState::Running {
        format!("{:?}", s.agent_state).to_lowercase()
    } else {
        format!("{:?}", s.state).to_lowercase()
    };
    let state = if s.autopilot && s.state == SessionState::Running {
        format!("auto·{state}")
    } else {
        state
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
            "  none running · o starts a mission",
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
/// The CORVX rule that sent an approval to the human, from its note ("CORVX rule <id>: ...").
fn rule_of(item: &AttentionItem) -> Option<String> {
    let rest = item.note.as_deref()?.strip_prefix("CORVX rule ")?;
    rest.split(':').next().map(str::to_string)
}

fn level_style(level: &str) -> Style {
    match level {
        "supervised" => Style::default().fg(ACCENT),
        "guided" => Style::default().fg(GREEN),
        "autonomous" => Style::default().fg(AMBER),
        "full" => Style::default().fg(RED),
        _ => Style::default().fg(DIM),
    }
    .add_modifier(Modifier::BOLD)
}

fn action_style(action: &str) -> Style {
    match action {
        "allow" => Style::default().fg(GREEN),
        "deny" => Style::default().fg(RED),
        "ask" => Style::default().fg(AMBER),
        _ => Style::default().fg(DIM),
    }
}

fn draw_rules(f: &mut Frame, area: Rect, app: &App) {
    let scope = app
        .rules_project()
        .map(|p| format!("for {p}"))
        .unwrap_or_else(|| "for every project".into());
    let block = Block::bordered()
        .title(format!(" rules {scope} "))
        .border_style(Style::default().fg(BORDER));
    let mut lines = Vec::new();
    if let Some(p) = app.selected_project().filter(|_| !app.rules_everywhere) {
        let dial: Vec<Span> = corvx_proto::autonomy::ALL
            .iter()
            .flat_map(|l| {
                let style = if *l == p.autonomy {
                    level_style(l).add_modifier(Modifier::REVERSED)
                } else {
                    Style::default().fg(DIM)
                };
                [Span::styled(format!(" {l} "), style), Span::raw(" ")]
            })
            .collect();
        let mut line = vec![Span::styled("autonomy  ", Style::default().fg(DIM))];
        line.extend(dial);
        line.push(Span::styled("  - / + to move", Style::default().fg(DIM)));
        lines.push(Line::from(line));
        lines.push(Line::styled(
            format!("          {}", crate::level_meaning(&p.autonomy)),
            Style::default().fg(DIM),
        ));
        lines.push(Line::from(""));
    }
    lines.extend([
        Line::styled(
            "Applied to every agent's tool calls, whoever would decide them: ask = you decide,",
            Style::default().fg(DIM),
        ),
        Line::styled(
            "allow = approved at once, deny = refused at once. The strictest match wins.",
            Style::default().fg(DIM),
        ),
        Line::from(""),
    ]);
    for (i, r) in app.rules.iter().enumerate() {
        let selected = i == app.rules_sel;
        let marker_style = if selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        let set_in = match r.set_in.as_str() {
            "project" => "this project",
            "everywhere" => "everywhere",
            "level" => "by level",
            _ => "",
        };
        lines.push(Line::from(vec![
            Span::styled(if selected { "▸ " } else { "  " }, marker_style),
            Span::raw(format!("{:<18}", r.id)),
            Span::styled(format!("{:<7}", r.action), action_style(&r.action)),
            Span::styled(format!("{set_in:<13}"), Style::default().fg(DIM)),
            Span::raw(r.reason.clone()),
        ]));
    }
    if let Some(r) = app
        .rules
        .get(app.rules_sel)
        .filter(|r| !r.pattern.is_empty())
    {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            format!("{} /{}/", r.kind, r.pattern),
            Style::default().fg(DIM),
        ));
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .block(block),
        area,
    );
}

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
                "o on a project starts a mission: an orchestrator and its agents.",
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
            let depth = app.session_depths.get(i).copied().unwrap_or(0);
            let branch = if depth == 0 {
                String::new()
            } else {
                format!("{}└ ", "  ".repeat(depth - 1))
            };
            let name = crate::display_name(s);
            let name: String = format!("{branch}{name}").chars().take(28).collect();
            let state: String = agent_label(s).chars().take(20).collect();
            let project: String = s.project.chars().take(10).collect();
            Line::from(vec![
                Span::styled(if selected { "▸ " } else { "  " }, marker_style),
                Span::raw(format!("{name:<29}")),
                Span::styled(format!("{project:<11}"), Style::default().fg(ACCENT)),
                Span::styled(format!("{state:<21}"), agent_style(s)),
                Span::styled(s.id.clone(), Style::default().fg(DIM)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Renaming a project or moving it to another workspace.
fn draw_edit(f: &mut Frame, area: Rect, app: &App) {
    let Some(e) = &app.edit else {
        return;
    };
    let rect = centered(area, 76, 10);
    f.render_widget(Clear, rect);
    let field = |i: usize, label: &str, text: &str| {
        let style = if e.field == i {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        Line::from(vec![
            Span::styled(format!("{label:<11}"), Style::default().fg(DIM)),
            Span::styled(
                format!("{text}{}", if e.field == i { "█" } else { "" }),
                style,
            ),
        ])
    };
    let others: Vec<String> = app
        .workspaces()
        .into_iter()
        .filter(|w| *w != e.workspace)
        .collect();
    let lines = vec![
        field(0, "name", &e.name),
        field(1, "workspace", &e.workspace),
        Line::styled(
            if others.is_empty() {
                "            a new name makes a new workspace".to_string()
            } else {
                format!("            others: {} (←/→)", others.join(", "))
            },
            Style::default().fg(DIM),
        ),
        Line::from(""),
        Line::styled(
            format!(
                "The id stays {}: sessions, rules and proposals refer to it.",
                e.id
            ),
            Style::default().fg(DIM),
        ),
        Line::styled(
            "Saving also scans the project again (stack, test and build commands).",
            Style::default().fg(DIM),
        ),
        Line::from(""),
        Line::styled(
            "Enter save · Tab other field · Esc cancel",
            Style::default().fg(DIM),
        ),
    ];
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(format!(" edit {} ", e.id))
                .border_style(Style::default().fg(GREEN)),
        ),
        rect,
    );
}

fn mission_style(state: &str) -> Style {
    match state {
        "running" => Style::default().fg(ACCENT),
        "done" => Style::default().fg(GREEN),
        "failed" => Style::default().fg(RED),
        "interrupted" => Style::default().fg(AMBER),
        _ => Style::default().fg(DIM),
    }
}

/// A project's missions, newest first, and the selected one's details and journal.
fn draw_missions(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered()
        .title(format!(" missions · {} ", app.missions_project))
        .border_style(Style::default().fg(BORDER));
    if app.missions.is_empty() {
        let lines = vec![
            Line::styled(
                "No missions yet.",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                "o starts one: a goal, and an orchestrator plans it and runs agents.",
                Style::default().fg(DIM),
            ),
        ];
        f.render_widget(Paragraph::new(lines).block(block), area);
        return;
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    let list_height = (app.missions.len() as u16).clamp(1, (inner.height / 3).max(3));
    let [list, _, detail] = Layout::vertical([
        Constraint::Length(list_height),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);

    // Keep the selected row in view.
    let first = app
        .missions_sel
        .saturating_sub(list_height.saturating_sub(1) as usize);
    let rows: Vec<Line> = app
        .missions
        .iter()
        .enumerate()
        .skip(first)
        .take(list_height as usize)
        .map(|(i, m)| {
            let selected = i == app.missions_sel;
            let date = if m.started_at > 0 {
                corvx_core::knowledge::time_of(m.started_at)
                    .trim_end_matches(" UTC")
                    .to_string()
            } else {
                m.journal
                    .as_ref()
                    .and_then(|j| j.path.rsplit('/').next())
                    .and_then(|n| n.get(..10))
                    .unwrap_or("-")
                    .to_string()
            };
            let mut extra = format!("{} agent{}", m.agents, if m.agents == 1 { "" } else { "s" });
            if m.worktrees > 0 {
                extra.push_str(&format!(
                    " · {} worktree{}",
                    m.worktrees,
                    if m.worktrees == 1 { "" } else { "s" }
                ));
            }
            let width =
                (list.width as usize).saturating_sub(2 + 17 + 12 + extra.chars().count() + 2);
            let goal: String = m.goal.chars().take(width).collect();
            Line::from(vec![
                Span::styled(
                    if selected { "▸ " } else { "  " },
                    if selected {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                    },
                ),
                Span::styled(format!("{date:<17}"), Style::default().fg(DIM)),
                Span::styled(format!("{:<12}", m.state()), mission_style(m.state())),
                Span::styled(
                    format!("{goal:<width$}"),
                    if selected {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                ),
                Span::styled(format!("  {extra}"), Style::default().fg(DIM)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(rows), list);

    let Some(m) = app.missions.get(app.missions_sel) else {
        return;
    };
    let mut lines: Vec<Line> = Vec::new();
    if let Some(s) = &m.session {
        let mut facts = vec![s.id.clone()];
        if let Some(model) = &s.agent_model {
            facts.push(format!("agents on {model}"));
        }
        if m.agents_running > 0 {
            facts.push(format!("{} agent(s) running", m.agents_running));
        }
        if m.worktrees > 0 {
            facts.push(format!(
                "{} worktree(s) left · c removes the merged ones",
                m.worktrees
            ));
        }
        if m.running() {
            facts.push("Enter watches it".into());
        }
        lines.push(Line::styled(facts.join(" · "), Style::default().fg(DIM)));
    }
    match &m.journal {
        Some(j) => {
            lines.push(Line::styled(j.path.clone(), Style::default().fg(DIM)));
            lines.push(Line::from(""));
            for l in j.text.lines() {
                lines.push(if l.starts_with('#') {
                    Line::styled(
                        l.to_string(),
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    )
                } else {
                    Line::raw(l.to_string())
                });
            }
        }
        None if m.running() => lines.push(Line::styled(
            "No journal entry yet: the orchestrator writes it when the goal is met.",
            Style::default().fg(DIM),
        )),
        None => lines.push(Line::styled(
            "It wrote no journal entry.",
            Style::default().fg(DIM),
        )),
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.journal_scroll, 0)),
        detail,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_filters_by_label() {
        let all = filter_actions("");
        assert_eq!(all.len(), ACTIONS.len());
        let found = filter_actions("proj");
        assert_eq!(found.len(), 4);
        assert!(filter_actions("zzz").is_empty());
    }

    #[test]
    fn centered_stays_inside_area() {
        let area = Rect::new(0, 0, 50, 10);
        let r = centered(area, 100, 100);
        assert!(r.x + r.width <= area.width && r.y + r.height <= area.height);
    }

    #[test]
    fn sessions_are_listed_as_a_tree() {
        use corvx_proto::{AgentState, Session};
        let mk = |id: &str, sup: Option<&str>, running: bool| Session {
            id: id.into(),
            project: "p".into(),
            command: vec!["claude".into()],
            runtime: None,
            backend: "tmux".into(),
            supervisor: sup.map(String::from),
            worktree: None,
            label: None,
            state: if running {
                SessionState::Running
            } else {
                SessionState::Stopped
            },
            pid: None,
            started_at: 0,
            ended_at: None,
            exit_code: None,
            agent_state: AgentState::Unknown,
            agent_detail: None,
            agent_source: None,
            runtime_session: None,
            autopilot: false,
            agent_model: None,
            human_approves_agents: false,
        };
        let input = vec![
            mk("old", None, false),
            mk("orch", None, true),
            mk("backend", Some("orch"), true),
            mk("frontend", Some("orch"), true),
            mk("orphan", Some("gone"), true),
            mk("sub", Some("backend"), true),
        ];
        let (order, depths) = session_tree(input);
        let ids: Vec<&str> = order.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["orch", "backend", "sub", "frontend", "orphan", "old"]
        );
        assert_eq!(depths, vec![0, 1, 2, 1, 0, 0]);
    }

    #[test]
    fn home_expansion_only_touches_leading_tilde() {
        assert_eq!(expand_home("/abs/path"), "/abs/path");
        assert_eq!(expand_home("relative"), "relative");
    }
}
