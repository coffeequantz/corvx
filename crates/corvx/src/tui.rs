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
    methods, AttentionItem, AttentionList, Knowledge, KnowledgeAccepted, KnowledgeInit, Project,
    ProjectList, Proposal, ProposalList, Rule, RuleList, Session, SessionList, SessionState,
};
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};
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
    /// Asking before stopping the selected session.
    ConfirmStop,
    /// Typing a message to the selected session, or to the project's lead.
    Talk,
    /// Typing a new goal for the selected project.
    AddGoal,
    /// In the selected session's pane: keys go to the session, as if attached.
    Live,
}

/// What the mouse is over.
#[derive(Clone, Debug, PartialEq)]
enum Hit {
    /// A column: 0 projects, 1 missions, 2 agents.
    Column(usize),
    Project(usize),
    /// A row of the missions column, by key.
    Mission(String),
    /// An agent tile, by session id.
    Agent(String),
    /// The live pane.
    Live,
}

/// Who a typed message goes to.
#[derive(Clone, Debug, PartialEq)]
enum TalkTo {
    /// The project's lead, started with the message if none runs.
    Lead(String),
    Session(String),
}

/// What a row of the tree is.
#[derive(Clone, Debug)]
enum RowKind {
    Project,
    /// The project's lead: the agent you talk to about the project. None until started.
    Lead(Option<Session>),
    /// An orchestrator, and the task it was given.
    Mission(Session),
    /// An agent working under a mission (or under any session).
    Agent(Session),
    /// A session started by hand.
    Loose(Session),
    /// Earlier missions, folded away; how many.
    History(usize),
}

/// One row of the tree on the left: projects, then under each its lead, its missions with
/// their agents, and its earlier missions.
#[derive(Clone, Debug)]
struct Row {
    /// Stable across refreshes, so the selection stays put: "p:<project>", "lead:<project>",
    /// "s:<session>", "h:<project>".
    key: String,
    project: String,
    depth: usize,
    kind: RowKind,
    /// It has rows under it, and whether they are shown.
    branch: bool,
    open: bool,
}

impl Row {
    fn session(&self) -> Option<&Session> {
        match &self.kind {
            RowKind::Lead(s) => s.as_ref(),
            RowKind::Mission(s) | RowKind::Agent(s) | RowKind::Loose(s) => Some(s),
            _ => None,
        }
    }
}

/// Missions that ended longer ago than this fold into the project's history.
const RECENT_SECS: i64 = 12 * 3600;

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
    RemoveProject,
    Refresh,
    RestartDaemon,
    TalkToLead,
    AddGoal,
    EditGoals,
    Inbox,
    Help,
    Quit,
}

const ACTIONS: [(Action, &str); 17] = [
    (Action::AddProject, "add project"),
    (Action::TalkToLead, "talk to the project's lead"),
    (Action::StartMission, "start a mission"),
    (Action::AddGoal, "add a goal"),
    (Action::EditGoals, "edit goals and guideposts"),
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
    (Action::Help, "help"),
    (Action::Quit, "quit"),
];

const REFRESH_EVERY: Duration = Duration::from_secs(2);
/// How often the live pane reads the screen it shows.
const PEEK_EVERY: Duration = Duration::from_millis(800);
/// How often while you type in it.
const LIVE_PEEK_EVERY: Duration = Duration::from_millis(120);
/// How often right after a key or the wheel reached the session, while it redraws.
const FAST_PEEK_EVERY: Duration = Duration::from_millis(25);
/// How long after that the pane keeps reading that often.
const FAST_PEEK_FOR: Duration = Duration::from_millis(400);
/// At most this many wheel steps go to a session at once.
const MAX_WHEEL_STEPS: i32 = 12;

/// Models an orchestrator can run on, and its agents, defaults first.
const ORCHESTRATOR_MODELS: [&str; 2] = ["opus", "sonnet"];
const AGENT_MODELS: [&str; 3] = ["sonnet", "haiku", "opus"];

#[derive(Clone, Default)]
struct KnowledgeState {
    wired: bool,
    brief_written: bool,
    /// Entries in its mission journal.
    journal: usize,
    /// Its goals (title, status), from goals.md.
    goals: Vec<(String, String)>,
    guideposts: usize,
}

impl KnowledgeState {
    fn of(project: &Project) -> Self {
        let root = Path::new(&project.path);
        let brief = std::fs::read_to_string(root.join(".corvx/brief.md")).unwrap_or_default();
        let goals_md = std::fs::read_to_string(root.join(".corvx/goals.md")).unwrap_or_default();
        let (goals, guideposts) = corvx_core::knowledge::parse_goals(&goals_md);
        KnowledgeState {
            goals: goals.into_iter().map(|g| (g.title, g.status)).collect(),
            guideposts: guideposts.len(),
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
    /// 0 name, 1 workspace, 2 knowledge (or, for a new project, what it is).
    field: usize,
    /// The folder does not exist yet (or is empty): a new project is started there.
    new: bool,
    /// For a new project: what it is, in your words. Becomes its brief.
    about: String,
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
    /// 0 goal, 1 the project goal it serves, 2 autonomy, 3 orchestrator model, 4 agent model,
    /// 5 approvals.
    field: usize,
    /// The project's active and next goals; `serves` indexes them, 0 meaning none.
    goals: Vec<String>,
    serves: usize,
    level: usize,
    model: usize,
    agent_model: usize,
    human_approves: bool,
    /// What the orchestrator will start from, in one line.
    knowledge: String,
    wired: bool,
}

const MISSION_FIELDS: usize = 6;

struct App {
    client: Client,
    data_dir: PathBuf,
    projects: Vec<Project>,
    /// Open attention items, most urgent first.
    attention: Vec<AttentionItem>,
    /// Every session the daemon knows, ended ones too, in start order.
    sessions: Vec<Session>,
    /// The column the keyboard is in: 0 projects, 1 the project's lead and missions,
    /// 2 agents. The right side shows the selection.
    col: usize,
    /// The selection in the missions and agents columns, by key, and where it was (for
    /// when it goes away).
    col1_key: String,
    col1_at: usize,
    col2_key: String,
    col2_at: usize,
    /// The last lines of each agent tile's screen.
    tiles: std::collections::HashMap<String, Vec<String>>,
    /// Rows opened or closed by hand; the rest follow their default.
    open: std::collections::HashMap<String, bool>,
    /// Journal entries per project, read when an ended mission is selected.
    journals: std::collections::HashMap<String, Vec<corvx_proto::JournalEntry>>,
    /// How far the detail pane is scrolled (journal entries).
    detail_scroll: u16,
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
    /// The session to stop once confirmed, and the view to go back to.
    stop_target: Option<(String, Mode)>,
    /// The daemon binary on disk is newer than the running daemon.
    newer_build: bool,
    talk_to: Option<TalkTo>,
    /// The message line was opened from the inbox, and goes back to it.
    talk_from_inbox: bool,
    /// The live pane: whose screen, and its text.
    peek: Option<(String, String)>,
    /// The session the keyboard is in (Mode::Live).
    live_id: Option<String>,
    /// The live pane's inner size (columns, rows) when last drawn.
    live_size: std::cell::Cell<(u16, u16)>,
    /// How many lines the live pane is scrolled back into the session's history.
    live_scroll: u16,
    /// The live session asked for mouse input: the wheel goes to it, to scroll its own view.
    peek_mouse: bool,
    /// Where the live pane's text was drawn, and the screen row it starts at.
    live_area: std::cell::Cell<(Rect, usize)>,
    /// The path prompt holds the suggested folder, untouched.
    input_suggested: bool,
    /// Wheel steps over the live pane not yet sent (up is positive), and where the last was.
    wheel: Option<(i32, u16, u16)>,
    /// The live pane scrolled through tmux's history and needs reading again.
    pane_stale: bool,
    /// Until when the pane reads the screen quickly, while the session redraws.
    fast_peek_until: Instant,
    last_pane_peek: Instant,
    /// Estimated spend: today, per project, and the subscription's limits.
    usage: corvx_proto::UsageSummary,
    /// Each session's estimated cost so far, and how full its context is.
    costs: std::collections::HashMap<String, (f64, Option<u8>)>,
    /// The model each session last reported running (Claude Code's status line).
    models: std::collections::HashMap<String, String>,
    /// Where things were drawn, for the mouse: each area and what it is.
    hits: std::cell::RefCell<Vec<(Rect, Hit)>>,
    last_peek: Instant,
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

/// Takes over the terminal, with the mouse: the wheel scrolls what is under the pointer, and
/// a click selects. (Hold Option, or Shift in most terminals, to select text.)
fn init_terminal() -> DefaultTerminal {
    let terminal = ratatui::init();
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::EnableMouseCapture
    );
    terminal
}

fn restore_terminal() {
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::DisableMouseCapture
    );
    ratatui::restore();
}

pub fn run(client: Client, data_dir: PathBuf) -> Result<()> {
    let mut app = App::new(client, data_dir)?;
    let mut terminal = init_terminal();
    let result = event_loop(&mut terminal, &mut app);
    restore_terminal();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|f| draw(f, app))?;
        if app.quit {
            return Ok(());
        }
        // In a session's pane, keep the screen close to what is typed.
        let live = app.mode == Mode::Live;
        let fast = Instant::now() < app.fast_peek_until;
        let wait = match (fast, live) {
            (true, _) => 10,
            (false, true) => 30,
            (false, false) => 250,
        };
        if event::poll(Duration::from_millis(wait))? {
            // Take everything that is already waiting before drawing again: a trackpad sends
            // dozens of wheel events a second, and handling them one by one falls behind.
            let mut n = 0;
            loop {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                    Event::Mouse(m) => app.on_mouse(m),
                    _ => {}
                }
                n += 1;
                if app.quit || n >= 256 || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
            app.flush_wheel();
        }
        if let Some(id) = app.pending_edit.take() {
            // Give the terminal to the editor, then accept what the human wrote.
            let text = app
                .proposals
                .get(&id)
                .map(|p| p.text.clone())
                .unwrap_or_default();
            restore_terminal();
            let edited = crate::edit_text(&text);
            *terminal = init_terminal();
            terminal.clear()?;
            match app.report(edited) {
                Some(Some(text)) => app.accept_proposal(&id, Some(text)),
                Some(None) => app.flash = "unchanged: nothing accepted".into(),
                None => {}
            }
        }
        if let Some((project, path, file)) = app.pending_file.take() {
            restore_terminal();
            let edited = crate::edit_file(&path);
            *terminal = init_terminal();
            terminal.clear()?;
            if let Some(changed) = app.report(edited) {
                app.after_editing(&project, &path, file, changed);
            }
        }
        if let Some(id) = app.pending_attach.take() {
            // Give the terminal to the session until the user detaches, then redraw.
            restore_terminal();
            // Start from a clean screen, so the session's first lines are not drawn over ours.
            print!("\x1b[2J\x1b[H");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            let r = crate::attach(&mut app.client, &id, &app.data_dir);
            *terminal = init_terminal();
            terminal.clear()?;
            // Back to the pane's size, so the preview fits again.
            let (cols, rows) = app.live_size.get();
            if cols > 10 && rows > 3 {
                let _ = app.rpc(
                    methods::SESSION_RESIZE,
                    json!({"id": id, "cols": cols, "rows": rows}),
                );
            }
            if app.report(r).is_some() {
                app.flash = format!("detached from {id}");
            }
            let r = app.refresh();
            app.report(r);
        }
        if fast && app.last_pane_peek.elapsed() >= FAST_PEEK_EVERY {
            app.refresh_pane();
        } else if app.last_peek.elapsed() >= if live { LIVE_PEEK_EVERY } else { PEEK_EVERY } {
            app.refresh_peek();
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
            col: 0,
            col1_key: String::new(),
            col1_at: 0,
            col2_key: String::new(),
            col2_at: 0,
            tiles: Default::default(),
            open: Default::default(),
            journals: Default::default(),
            detail_scroll: 0,
            pending_attach: None,
            pending_edit: None,
            proposals: Default::default(),
            pending_file: None,
            knowledge_state: Default::default(),
            preview: None,
            mission: None,
            edit: None,
            stop_target: None,
            newer_build: false,
            talk_to: None,
            talk_from_inbox: false,
            peek: None,
            live_id: None,
            live_size: std::cell::Cell::new((0, 0)),
            live_scroll: 0,
            peek_mouse: false,
            live_area: std::cell::Cell::new((Rect::default(), 0)),
            wheel: None,
            input_suggested: false,
            pane_stale: false,
            fast_peek_until: Instant::now(),
            last_pane_peek: Instant::now(),
            usage: Default::default(),
            costs: Default::default(),
            models: Default::default(),
            hits: Default::default(),
            last_peek: Instant::now(),
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
                format!("{lost} interrupted session(s) · select one in its project's missions, then R resumes it");
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
        // Ended sessions too: earlier missions are part of the tree.
        let list: SessionList =
            serde_json::from_value(self.rpc(methods::SESSION_LIST, json!({"all": true}))?)?;
        self.sessions = list.sessions;
        // (An older daemon reports no spend.)
        self.usage = self
            .rpc(methods::USAGE_SUMMARY, json!({}))
            .ok()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        self.costs = self
            .usage
            .sessions
            .iter()
            .map(|u| (u.id.clone(), (u.cost_usd, u.context_pct)))
            .collect();
        self.models = self
            .usage
            .sessions
            .iter()
            .filter_map(|u| Some((u.id.clone(), u.model.clone()?)))
            .collect();
        if self.inbox_sel >= self.attention.len() {
            self.inbox_sel = self.attention.len().saturating_sub(1);
        }
        if let Ok(info) = self.client.info() {
            self.daemon_pid = info.pid;
            self.newer_build = corvx_client::newer_build_on_disk(&info);
        }
        self.settle_selection();
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
        if self.mode == Mode::Live {
            self.key_live(key);
            return;
        }
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
            Mode::Answer => self.key_answer(key.code),
            Mode::Deny => self.key_deny(key.code),
            Mode::Reject => self.key_reject(key.code),
            Mode::Rules => self.key_rules(key.code),
            Mode::EditProject => self.key_edit(key.code),
            Mode::ConfirmStop => self.key_confirm_stop(key.code),
            Mode::Talk => self.key_talk(key.code),
            Mode::AddGoal => self.key_add_goal(key.code),
            Mode::Live => {}
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
        let row = self.selected_row();
        let session = row.as_ref().and_then(|r| r.session().cloned());
        match code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.move_row(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_row(-1),
            KeyCode::Right if self.col < 2 => self.move_col(1),
            KeyCode::Left => self.move_col(-1),
            KeyCode::Esc if self.col > 0 => self.move_col(-1),
            KeyCode::Char(' ') => {
                if let Some(r) = row.filter(|r| r.branch) {
                    self.open.insert(r.key, !r.open);
                }
            }
            KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
            KeyCode::Char('f') => match &session {
                Some(s) if s.state == SessionState::Running => {
                    self.pending_attach = Some(s.id.clone())
                }
                _ => self.flash = "f attaches full screen to a running session".into(),
            },
            KeyCode::Enter => match (&row, &session) {
                (_, Some(s)) if s.state == SessionState::Running => self.enter_live(&s.id),
                (_, Some(s)) if s.state == SessionState::Lost => {
                    self.flash =
                        format!("{} was interrupted · R resumes it", crate::display_name(s))
                }
                (_, Some(s)) => {
                    self.flash = format!(
                        "{} has ended · its journal is on the right",
                        crate::display_name(s)
                    )
                }
                (Some(r), None) if matches!(r.kind, RowKind::Lead(None)) => self.start_talk(),
                (Some(r), None) if matches!(r.kind, RowKind::Project) => self.move_col(1),
                (Some(r), None) if r.branch => {
                    self.open.insert(r.key.clone(), !r.open);
                }
                _ => {}
            },
            KeyCode::Char(c @ ('y' | 'Y' | 'n')) => {
                if !self.decide_here(c) {
                    self.flash = "nothing to approve here".into();
                }
            }
            KeyCode::Char('t') => self.start_talk(),
            KeyCode::Char('x') => match &session {
                Some(s) => self.ask_stop(s, Mode::Normal),
                None => self.flash = "select a session to stop it".into(),
            },
            KeyCode::Char('c') => match &session {
                Some(s) => self.cleanup(&s.id),
                None => self.flash = "select a mission to clean up its worktrees".into(),
            },
            KeyCode::Char('R') => match &session {
                Some(s) => self.resume(s),
                None => self.flash = "select an interrupted session to resume it".into(),
            },
            KeyCode::Char('A') => match &session {
                Some(s) => self.set_autopilot(&s.id, !s.autopilot),
                None => self.flash = "select an agent to turn its autopilot on or off".into(),
            },
            KeyCode::Char('g') => self.start_add_goal(),
            KeyCode::Char('G') => self.edit_knowledge("goals"),
            KeyCode::Char('a') => self.start_add(),
            KeyCode::Char('d') => match &row {
                Some(r) if matches!(r.kind, RowKind::Project) => self.start_remove(),
                _ => self.flash = "d removes a project: select the project's row".into(),
            },
            KeyCode::Char('r') => {
                self.journals.clear();
                let r = self.refresh();
                if self.report(r).is_some() {
                    self.flash = "refreshed".into();
                }
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('i') => self.open_inbox(),
            KeyCode::Char('l') => self.open_rules(),
            KeyCode::Char('o') => self.open_mission(),
            KeyCode::Char('b') => self.edit_knowledge("brief"),
            KeyCode::Char('K') => self.init_knowledge(),
            KeyCode::Char('e') => self.open_edit(),
            KeyCode::Char(':') => self.toggle_palette(),
            KeyCode::Esc => self.flash.clear(),
            _ => {}
        }
    }

    /// Whether a row is open: as you left it, or by default.
    fn is_open(&self, key: &str, default: bool) -> bool {
        self.open.get(key).copied().unwrap_or(default)
    }

    /// The missions column for a project: its lead (or a place for one), its current
    /// missions, sessions started by hand, and its earlier missions folded under one row.
    fn col1_rows(&self, project: &str) -> Vec<Row> {
        let now = now_secs();
        let mine: Vec<&Session> = self
            .sessions
            .iter()
            .filter(|s| s.project == project)
            .collect();
        let row = |kind: RowKind, key: String, depth: usize| Row {
            key,
            project: project.to_string(),
            depth,
            kind,
            branch: false,
            open: false,
        };
        let lead = mine
            .iter()
            .rev()
            .find(|s| crate::is_lead(s) && s.state == SessionState::Running)
            .or_else(|| {
                mine.iter()
                    .rev()
                    .find(|s| crate::is_lead(s) && s.state == SessionState::Lost)
            })
            .map(|s| (*s).clone());
        let mut rows = vec![row(RowKind::Lead(lead), "lead".into(), 0)];
        let mut missions: Vec<&Session> = mine
            .iter()
            .copied()
            .filter(|s| !crate::is_lead(s) && crate::missions::is_orchestrator(s, &self.sessions))
            .collect();
        missions.sort_by_key(|s| {
            (
                s.state != SessionState::Running,
                std::cmp::Reverse(s.started_at),
            )
        });
        let current = |s: &Session| {
            matches!(s.state, SessionState::Running | SessionState::Lost)
                || s.ended_at.unwrap_or(now) > now - RECENT_SECS
        };
        for m in missions.iter().filter(|s| current(s)) {
            rows.push(row(
                RowKind::Mission((*m).clone()),
                format!("s:{}", m.id),
                0,
            ));
        }
        for s in mine.iter().filter(|s| {
            s.supervisor.is_none()
                && matches!(s.state, SessionState::Running | SessionState::Lost)
                && !crate::is_lead(s)
                && !crate::missions::is_orchestrator(s, &self.sessions)
        }) {
            rows.push(row(RowKind::Loose((*s).clone()), format!("s:{}", s.id), 0));
        }
        let earlier: Vec<&&Session> = missions.iter().filter(|s| !current(s)).collect();
        if !earlier.is_empty() {
            let key = format!("h:{project}");
            let open = self.is_open(&key, false);
            rows.push(Row {
                branch: true,
                open,
                ..row(RowKind::History(earlier.len()), key, 0)
            });
            if open {
                for m in earlier {
                    rows.push(row(
                        RowKind::Mission((**m).clone()),
                        format!("s:{}", m.id),
                        1,
                    ));
                }
            }
        }
        rows
    }

    /// The agents column: with a project selected, every agent working on it now; with a
    /// lead, mission or session selected, the agents under it (and theirs).
    fn col2_rows(&self) -> Vec<Row> {
        let Some(p) = self.selected_project() else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        let agent_row = |s: &Session, depth: usize| Row {
            key: s.id.clone(),
            project: s.project.clone(),
            depth,
            kind: RowKind::Agent(s.clone()),
            branch: false,
            open: false,
        };
        if self.col == 0 {
            for s in self.sessions.iter().filter(|s| {
                s.project == p.id
                    && s.supervisor.is_some()
                    && s.state == SessionState::Running
                    && !crate::missions::is_orchestrator(s, &self.sessions)
            }) {
                rows.push(agent_row(s, 0));
            }
            return rows;
        }
        let Some(parent) = self.col1_row().and_then(|r| r.session().cloned()) else {
            return rows;
        };
        // A lead's missions have their own row in the missions column.
        let lead = crate::is_lead(&parent);
        fn walk<'a>(
            all: &'a [Session],
            parent: &str,
            depth: usize,
            skip_orchestrators: bool,
            out: &mut Vec<(&'a Session, usize)>,
        ) {
            let mut under: Vec<&Session> = all
                .iter()
                .filter(|a| a.supervisor.as_deref() == Some(parent))
                .filter(|a| !(skip_orchestrators && crate::missions::is_orchestrator(a, all)))
                .collect();
            under.sort_by_key(|a| (a.state != SessionState::Running, a.started_at));
            for a in under {
                out.push((a, depth));
                if depth < 5 {
                    walk(all, &a.id, depth + 1, false, out);
                }
            }
        }
        let mut found = Vec::new();
        walk(&self.sessions, &parent.id, 0, lead, &mut found);
        rows.extend(found.into_iter().map(|(s, d)| agent_row(s, d)));
        rows
    }

    fn col1_row(&self) -> Option<Row> {
        let p = self.selected_project()?;
        let rows = self.col1_rows(&p.id);
        rows.iter()
            .find(|r| r.key == self.col1_key)
            .or_else(|| rows.get(self.col1_at.min(rows.len().saturating_sub(1))))
            .cloned()
    }

    fn col2_row(&self) -> Option<Row> {
        let rows = self.col2_rows();
        rows.iter()
            .find(|r| r.key == self.col2_key)
            .or_else(|| rows.get(self.col2_at.min(rows.len().saturating_sub(1))))
            .cloned()
    }

    /// What the right side shows: the project, or the selection in the focused column.
    fn selected_row(&self) -> Option<Row> {
        let p = self.selected_project()?;
        match self.col {
            0 => Some(Row {
                key: format!("p:{}", p.id),
                project: p.id.clone(),
                depth: 0,
                kind: RowKind::Project,
                branch: false,
                open: false,
            }),
            1 => self.col1_row(),
            _ => self.col2_row(),
        }
    }

    /// Keeps each column's selection on a row that exists.
    fn settle_selection(&mut self) {
        if self.selected_project().is_none() {
            self.col = 0;
            return;
        }
        if let Some(r) = self.col1_row() {
            let rows = self.col1_rows(&r.project);
            self.col1_at = rows.iter().position(|x| x.key == r.key).unwrap_or(0);
            self.col1_key = r.key;
        }
        let rows = self.col2_rows();
        match self.col2_row() {
            Some(r) => {
                self.col2_at = rows.iter().position(|x| x.key == r.key).unwrap_or(0);
                self.col2_key = r.key;
            }
            None if self.col == 2 => self.col = 1,
            None => {}
        }
    }

    /// After the selection changed: reset the scroll, read an ended mission's journal,
    /// and show the new selection's screen.
    fn selection_changed(&mut self) {
        self.detail_scroll = 0;
        self.live_scroll = 0;
        if let Some(row) = self.selected_row() {
            if let RowKind::Mission(s) = &row.kind {
                if s.state != SessionState::Running && !self.journals.contains_key(&row.project) {
                    let project = row.project.clone();
                    let entries = self
                        .rpc(
                            methods::KNOWLEDGE_READ,
                            json!({"project": project, "journal": crate::missions::JOURNAL_ENTRIES}),
                        )
                        .ok()
                        .and_then(|v| serde_json::from_value::<Knowledge>(v).ok())
                        .map(|k| k.journal)
                        .unwrap_or_default();
                    self.journals.insert(project, entries);
                }
            }
        }
        self.refresh_peek();
    }

    fn move_row(&mut self, delta: i32) {
        let step =
            |at: usize, len: usize| (at as i32 + delta).clamp(0, len.max(1) as i32 - 1) as usize;
        match self.col {
            0 => {
                if self.projects.is_empty() {
                    return;
                }
                self.selected = step(self.selected, self.projects.len());
                self.col1_key.clear();
                self.col1_at = 0;
                self.col2_key.clear();
                self.col2_at = 0;
            }
            1 => {
                let Some(p) = self.selected_project() else {
                    return;
                };
                let rows = self.col1_rows(&p.id);
                let at = rows
                    .iter()
                    .position(|r| r.key == self.col1_key)
                    .unwrap_or(self.col1_at);
                let to = step(at, rows.len());
                if let Some(r) = rows.get(to) {
                    self.col1_key = r.key.clone();
                    self.col1_at = to;
                }
                self.col2_key.clear();
                self.col2_at = 0;
            }
            _ => {
                let rows = self.col2_rows();
                let at = rows
                    .iter()
                    .position(|r| r.key == self.col2_key)
                    .unwrap_or(self.col2_at);
                let to = step(at, rows.len());
                if let Some(r) = rows.get(to) {
                    self.col2_key = r.key.clone();
                    self.col2_at = to;
                }
            }
        }
        self.selection_changed();
    }

    /// Moves the keyboard to the next column to the right (if it has anything) or left.
    fn move_col(&mut self, delta: i32) {
        if self.selected_project().is_none() {
            return;
        }
        let to = (self.col as i32 + delta).clamp(0, 2) as usize;
        if to == 2 && self.col == 1 {
            // Into the agents of what is selected; nothing there, nothing to move to.
            let saved = self.col;
            self.col = 2;
            let empty = self.col2_rows().is_empty();
            self.col = saved;
            if empty {
                self.flash = "no agents under it".into();
                return;
            }
        }
        if to == 2 && self.col == 0 {
            return self.move_col(1);
        }
        self.col = to;
        if to == 2 {
            self.col2_key.clear();
            self.col2_at = 0;
        }
        self.selection_changed();
    }

    fn select_project(&mut self, id: &str) {
        if let Some(i) = self.projects.iter().position(|x| x.id == id) {
            self.selected = i;
        }
        self.col = 0;
        self.selection_changed();
    }

    /// Selects a session where it is shown: the lead or a mission in the missions column,
    /// an agent in the agents column under its mission.
    fn reveal(&mut self, id: &str) {
        let Some(s) = self.sessions.iter().find(|s| s.id == id).cloned() else {
            return;
        };
        if let Some(i) = self.projects.iter().position(|p| p.id == s.project) {
            self.selected = i;
        }
        // The topmost session that has a row in the missions column.
        let mut chain = vec![s.clone()];
        while let Some(sup) = chain.last().and_then(|x| x.supervisor.clone()) {
            match self.sessions.iter().find(|x| x.id == sup) {
                Some(x) => chain.push(x.clone()),
                None => break,
            }
        }
        let rows = self.col1_rows(&s.project);
        let key_of = |x: &Session| {
            if crate::is_lead(x) {
                "lead".to_string()
            } else {
                format!("s:{}", x.id)
            }
        };
        let owner = chain
            .iter()
            .find(|x| rows.iter().any(|r| r.key == key_of(x)))
            .cloned();
        let owner = match owner {
            Some(o) => o,
            None => {
                // An earlier mission: open the list it is folded in.
                self.open.insert(format!("h:{}", s.project), true);
                match chain.iter().find(|x| {
                    self.col1_rows(&s.project)
                        .iter()
                        .any(|r| r.key == key_of(x))
                }) {
                    Some(o) => o.clone(),
                    None => return,
                }
            }
        };
        self.col1_key = key_of(&owner);
        self.col1_at = 0;
        if owner.id == s.id {
            self.col = 1;
        } else {
            self.col = 2;
            self.col2_key = s.id.clone();
            self.col2_at = 0;
        }
        self.selection_changed();
    }

    /// The wheel scrolls what is under the pointer: a column moves its selection, the live
    /// pane scrolls back through the session's history. A click selects.
    fn on_mouse(&mut self, m: MouseEvent) {
        if !matches!(self.mode, Mode::Normal | Mode::Live) {
            return;
        }
        let hit = self
            .hits
            .borrow()
            .iter()
            .rev()
            .find(|(r, _)| {
                m.column >= r.x
                    && m.column < r.x + r.width
                    && m.row >= r.y
                    && m.row < r.y + r.height
            })
            .map(|(_, h)| h.clone());
        let Some(hit) = hit else {
            return;
        };
        let column = match &hit {
            Hit::Column(c) => Some(*c),
            Hit::Project(_) => Some(0),
            Hit::Mission(_) => Some(1),
            Hit::Agent(_) => Some(2),
            Hit::Live => None,
        };
        match m.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = m.kind == MouseEventKind::ScrollUp;
                match column {
                    None if self.peek_mouse && self.peek_target().is_some() => {
                        // Claude Code (and other full-screen programs) scroll their own view:
                        // hand them the wheel, at the same place on their screen. Collected
                        // here and sent together once the waiting events are read.
                        let (area, first) = self.live_area.get();
                        let x = m.column.saturating_sub(area.x) + 1;
                        let y = (m.row.saturating_sub(area.y) as usize + first + 1) as u16;
                        let steps = self.wheel.map_or(0, |w| w.0) + if up { 1 } else { -1 };
                        self.wheel = Some((steps, x, y));
                    }
                    None => {
                        self.live_scroll = if up {
                            self.live_scroll.saturating_add(3)
                        } else {
                            self.live_scroll.saturating_sub(3)
                        };
                        self.pane_stale = true;
                    }
                    Some(c) if self.mode == Mode::Normal => {
                        if c != self.col && (c < 2 || !self.col2_rows().is_empty()) {
                            self.col = c;
                        }
                        self.move_row(if up { -1 } else { 1 });
                    }
                    Some(_) => {}
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if hit != Hit::Live && self.mode == Mode::Live {
                    self.mode = Mode::Normal;
                    self.live_id = None;
                }
                match hit {
                    Hit::Project(i) => {
                        self.selected = i;
                        self.col = 0;
                        self.col1_key.clear();
                        self.col2_key.clear();
                        self.selection_changed();
                    }
                    Hit::Mission(key) => {
                        if self.col1_key == key && key.starts_with("h:") {
                            let open = self.is_open(&key, false);
                            self.open.insert(key.clone(), !open);
                        }
                        self.col = 1;
                        self.col1_key = key;
                        self.col2_key.clear();
                        self.selection_changed();
                    }
                    Hit::Agent(id) => {
                        self.col = 2;
                        self.col2_key = id;
                        self.selection_changed();
                    }
                    Hit::Live => {
                        if let Some(s) = self.selected_row().and_then(|r| r.session().cloned()) {
                            if s.state == SessionState::Running && self.mode != Mode::Live {
                                self.enter_live(&s.id);
                            }
                        }
                    }
                    Hit::Column(c) => {
                        if c != self.col {
                            self.move_col(c as i32 - self.col as i32);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Moves the keyboard into a session's pane. The session gets the pane's size, so what
    /// it draws fits.
    fn enter_live(&mut self, id: &str) {
        let (cols, rows) = self.live_size.get();
        if cols > 10 && rows > 3 {
            // (Best effort: a session that cannot be resized is still usable.)
            let _ = self.rpc(
                methods::SESSION_RESIZE,
                json!({"id": id, "cols": cols, "rows": rows}),
            );
        }
        self.live_id = Some(id.to_string());
        self.mode = Mode::Live;
        self.flash.clear();
        self.refresh_peek();
    }

    /// Keys in a session's pane go to the session; Ctrl+\ comes back to the columns.
    fn key_live(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if ctrl && matches!(key.code, KeyCode::Char('\\') | KeyCode::Char('4')) {
            self.mode = Mode::Normal;
            self.live_id = None;
            self.flash = "back in the columns · Enter goes into the pane again".into();
            return;
        }
        let Some(id) = self.live_id.clone() else {
            self.mode = Mode::Normal;
            return;
        };
        let Some(bytes) = key_bytes(key.code, ctrl, alt) else {
            return;
        };
        // Typing brings the pane back to the present.
        self.live_scroll = 0;
        let r = self.rpc(methods::SESSION_INPUT, json!({"id": id, "data": bytes}));
        if self.report(r).is_none() {
            // It ended, or the daemon went away: back to the columns.
            self.mode = Mode::Normal;
            self.live_id = None;
            return;
        }
        self.soon_redrawn();
    }

    /// The first request under the selection you can answer with y or n: a permission request
    /// or a trust question from the selected session or anything under it (on a project, any
    /// in the project).
    fn pending_decision(&self) -> Option<AttentionItem> {
        let row = self.selected_row()?;
        let decidable = |i: &&AttentionItem| i.kind == "approval" || i.kind == "trust";
        if matches!(row.kind, RowKind::Project) {
            return self
                .attention
                .iter()
                .filter(decidable)
                .find(|i| i.project == row.project)
                .cloned();
        }
        let root = row.session()?.id.clone();
        let mut ids = vec![root];
        let mut i = 0;
        while i < ids.len() {
            let parent = ids[i].clone();
            ids.extend(
                self.sessions
                    .iter()
                    .filter(|s| s.supervisor.as_deref() == Some(parent.as_str()))
                    .map(|s| s.id.clone()),
            );
            i += 1;
        }
        self.attention
            .iter()
            .filter(decidable)
            .find(|item| ids.contains(&item.session_id))
            .cloned()
    }

    /// Answers the request under the selection: y allow, Y always, n deny.
    fn decide_here(&mut self, key: char) -> bool {
        let Some(item) = self.pending_decision() else {
            return false;
        };
        let decision = match (key, item.kind.as_str()) {
            ('n', _) => "deny",
            ('Y', "approval") => "always",
            _ => "allow",
        };
        let r = self.rpc(
            methods::ATTENTION_DECIDE,
            json!({"id": item.id, "decision": decision}),
        );
        if self.report(r).is_some() {
            let who = self
                .sessions
                .iter()
                .find(|s| s.id == item.session_id)
                .map(title_of)
                .unwrap_or_default();
            self.flash = match (decision, item.kind.as_str()) {
                ("deny", "trust") => format!("{who}: folder not trusted · Claude Code exits"),
                (_, "trust") => format!("{who}: folder trusted · Claude Code starts"),
                ("deny", _) => format!(
                    "{who}: denied · {} · i inbox to add a reason next time",
                    clip(&item.summary, 50)
                ),
                ("always", _) => format!(
                    "{who}: approved, and won't ask again · {}",
                    clip(&item.summary, 50)
                ),
                _ => format!("{who}: approved · {}", clip(&item.summary, 60)),
            };
            let r = self.refresh();
            self.report(r);
            self.refresh_peek();
        }
        true
    }

    /// Whose screen the detail pane shows live: the selected running session.
    fn peek_target(&self) -> Option<String> {
        self.selected_row()
            .and_then(|r| r.session().cloned())
            .filter(|s| s.state == SessionState::Running)
            .map(|s| s.id)
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

    /// Sends the wheel steps collected over the live pane as one input, and reads the pane
    /// again: quickly for a moment, while the session redraws.
    fn flush_wheel(&mut self) {
        if let Some((steps, x, y)) = self.wheel.take() {
            if steps != 0 {
                if let Some(id) = self.peek_target() {
                    let code = if steps > 0 { 64 } else { 65 };
                    let one = format!("\x1b[<{code};{x};{y}M");
                    let data =
                        one.repeat(steps.unsigned_abs().min(MAX_WHEEL_STEPS as u32) as usize);
                    let r = self.rpc(methods::SESSION_INPUT, json!({"id": id, "data": data}));
                    self.report(r);
                    self.soon_redrawn();
                }
            }
        }
        if std::mem::take(&mut self.pane_stale) {
            self.refresh_pane();
        }
    }

    /// Something reached the session: read its screen quickly for a moment.
    fn soon_redrawn(&mut self) {
        self.fast_peek_until = Instant::now() + FAST_PEEK_FOR;
        self.refresh_pane();
    }

    fn refresh_peek(&mut self) {
        self.last_peek = Instant::now();
        if !matches!(
            self.mode,
            Mode::Normal | Mode::Talk | Mode::Live | Mode::ConfirmStop
        ) {
            return;
        }
        // The agent tiles: the last lines of each running agent's screen. Not while you type
        // in a pane, when only that screen needs to keep up.
        if self.mode != Mode::Live {
            let ids: Vec<String> = self
                .col2_rows()
                .into_iter()
                .filter_map(|r| r.session().cloned())
                .filter(|s| s.state == SessionState::Running)
                .map(|s| s.id)
                .take(8)
                .collect();
            for id in ids {
                if let Ok(v) = self.rpc(methods::SESSION_SCREEN, json!({"id": id})) {
                    let text = v["text"].as_str().unwrap_or_default();
                    self.tiles.insert(id, tile_lines(text));
                }
            }
        }
        self.refresh_pane();
    }

    /// Reads the screen the live pane shows, and nothing else.
    fn refresh_pane(&mut self) {
        self.last_pane_peek = Instant::now();
        let Some(id) = self.peek_target() else {
            self.peek = None;
            return;
        };
        // (Quietly: the pane says when it has nothing to show.) In colour, and with enough
        // history above the screen when scrolled back.
        let rows = u32::from(self.live_size.get().1.max(10));
        let history = if self.live_scroll > 0 {
            u32::from(self.live_scroll) + rows
        } else {
            0
        };
        if let Ok(v) = self.rpc(
            methods::SESSION_SCREEN,
            json!({"id": id, "ansi": true, "history": history}),
        ) {
            let text = v["text"].as_str().unwrap_or_default().to_string();
            self.peek_mouse = v["mouse"].as_bool().unwrap_or(false);
            self.peek = Some((id, text));
        }
    }

    /// Opens the message line: to the selected session, or to the project's lead.
    fn start_talk(&mut self) {
        let Some(p) = self.selected_project().cloned() else {
            self.flash = "add a project first: a".into();
            return;
        };
        let target = match self.selected_row().and_then(|r| r.session().cloned()) {
            Some(s) if s.state == SessionState::Running => TalkTo::Session(s.id),
            Some(s) if crate::is_lead(&s) => TalkTo::Lead(p.id),
            Some(s) => {
                self.flash = format!(
                    "{} is not running{}",
                    crate::display_name(&s),
                    if s.state == SessionState::Lost {
                        " · R resumes it"
                    } else {
                        ""
                    }
                );
                return;
            }
            None => TalkTo::Lead(p.id),
        };
        self.talk_to = Some(target);
        self.input.clear();
        self.mode = Mode::Talk;
        self.flash.clear();
    }

    /// Who the message line talks to, for its prompt.
    fn talk_label(&self) -> String {
        match &self.talk_to {
            Some(TalkTo::Lead(p)) => {
                match self.sessions.iter().any(|s| {
                    s.project == *p && crate::is_lead(s) && s.state == SessionState::Running
                }) {
                    true => format!("lead · {p}"),
                    false => format!("new lead · {p}"),
                }
            }
            Some(TalkTo::Session(id)) => self
                .sessions
                .iter()
                .find(|s| s.id == *id)
                .map(crate::display_name)
                .unwrap_or_else(|| id.clone()),
            None => String::new(),
        }
    }

    fn key_talk(&mut self, code: KeyCode) {
        let back = if self.talk_from_inbox {
            Mode::Inbox
        } else {
            Mode::Normal
        };
        match code {
            KeyCode::Esc => {
                self.mode = back;
                self.talk_from_inbox = false;
                self.input.clear();
                self.talk_to = None;
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.input);
                self.mode = back;
                self.talk_from_inbox = false;
                let Some(target) = self.talk_to.take() else {
                    return;
                };
                self.send_talk(target, text.trim());
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) => self.input.push(c),
            _ => {}
        }
    }

    fn send_talk(&mut self, target: TalkTo, text: &str) {
        match target {
            TalkTo::Lead(project) => {
                let r = crate::talk_to_lead(
                    &mut self.client,
                    &project,
                    crate::DEFAULT_ORCHESTRATOR_MODEL,
                    text,
                );
                match self.report(r) {
                    Some(crate::Talked::Started(id)) => {
                        self.flash = format!(
                            "started the lead of {project} ({id}) · it reads the project, then answers · t talks to it"
                        );
                    }
                    Some(crate::Talked::Sent(id)) => {
                        self.flash = format!("sent to the lead of {project} ({id})")
                    }
                    None => return,
                }
            }
            TalkTo::Session(id) => {
                if text.is_empty() {
                    self.flash = "nothing sent".into();
                    return;
                }
                let r = self.rpc(
                    methods::SESSION_INPUT,
                    json!({"id": id, "data": format!("{text}\r")}),
                );
                if self.report(r).is_none() {
                    return;
                }
                let name = self
                    .sessions
                    .iter()
                    .find(|s| s.id == id)
                    .map(crate::display_name)
                    .unwrap_or(id);
                self.flash = format!("sent to {name} · it reads it after its current step");
            }
        }
        let r = self.refresh();
        self.report(r);
        self.refresh_peek();
    }

    fn start_add_goal(&mut self) {
        if self.selected_project().is_none() {
            self.flash = "add a project first: a".into();
            return;
        }
        self.input.clear();
        self.mode = Mode::AddGoal;
        self.flash.clear();
    }

    fn key_add_goal(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.input.clear();
            }
            KeyCode::Enter => {
                let title = std::mem::take(&mut self.input);
                self.mode = Mode::Normal;
                let title = title.trim();
                let Some(p) = self.selected_project().cloned() else {
                    return;
                };
                if title.is_empty() {
                    self.flash = "no goal added".into();
                    return;
                }
                let r = self.rpc(
                    methods::KNOWLEDGE_GOAL,
                    json!({"project": p.id, "title": title}),
                );
                if self.report(r).is_some() {
                    self.flash = format!(
                        "{}: goal \"{title}\" added and committed · G edits goals and guideposts · o starts a mission for it",
                        p.id
                    );
                    let r = self.refresh();
                    self.report(r);
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) => self.input.push(c),
            _ => {}
        }
    }

    /// Resumes an interrupted session (and the agents under it).
    fn resume(&mut self, s: &Session) {
        if s.state != SessionState::Lost {
            self.flash = "only an interrupted session can be resumed".into();
            return;
        }
        let r = self.rpc(methods::SESSION_RESUME, json!({"id": s.id}));
        if self.report(r).is_some() {
            self.flash = format!("resumed {}", crate::display_name(s));
            let r = self.refresh();
            self.report(r);
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
                Some(item) => self.flash = not_here('r', item),
                None => {}
            },
            // Tell the item's agent something: what's next, or how to get unstuck.
            KeyCode::Char('t') => match self.attention.get(self.inbox_sel).cloned() {
                Some(item)
                    if self
                        .sessions
                        .iter()
                        .any(|s| s.id == item.session_id && s.state == SessionState::Running) =>
                {
                    self.talk_to = Some(TalkTo::Session(item.session_id));
                    self.talk_from_inbox = true;
                    self.input.clear();
                    self.mode = Mode::Talk;
                    self.flash.clear();
                }
                Some(_) => self.flash = "its session is not running · Enter shows it".into(),
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
                Some(item) if item.kind == "trust" => {
                    let (decision, done) = if c == 'n' {
                        ("deny", "not trusted · Claude Code exits")
                    } else {
                        ("allow", "trusted · Claude Code starts")
                    };
                    self.attention_action(
                        methods::ATTENTION_DECIDE,
                        json!({"decision": decision}),
                        done,
                    )
                }
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
                Some(item) => self.flash = not_here(c, item),
                None => {}
            },
            // Edit a proposal in $EDITOR, then accept it.
            KeyCode::Char('e') => match self.attention.get(self.inbox_sel) {
                Some(item) if item.kind == "proposal" => {
                    self.pending_edit = Some(item.id.clone());
                }
                Some(item) => self.flash = not_here('e', item),
                None => {}
            },
            // A CORVX rule sent this to you: allow that rule from now on in this project.
            KeyCode::Char('L') => {
                let Some(item) = self.attention.get(self.inbox_sel).cloned() else {
                    return;
                };
                let Some(rule) = rule_of(&item) else {
                    self.flash = not_here('L', &item);
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
                Some(item) => self.flash = not_here('A', &item),
                None => {}
            },
            KeyCode::Char('x') => self.attention_action(
                methods::ATTENTION_DISMISS,
                json!({"reason": "dismissed in the terminal UI"}),
                "dismissed",
            ),
            // Enter shows the session the item is about in the tree, with its screen live.
            KeyCode::Enter | KeyCode::Char('p') => {
                if let Some(item) = self.attention.get(self.inbox_sel).cloned() {
                    self.mode = Mode::Normal;
                    if self.sessions.iter().any(|s| s.id == item.session_id) {
                        self.reveal(&item.session_id);
                    } else {
                        let project = item.project.clone();
                        self.select_project(&project);
                    }
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
            // Typing a whole path replaces the suggested folder.
            KeyCode::Char(c @ ('/' | '~')) if self.input_suggested => {
                self.input = c.to_string();
            }
            KeyCode::Char(c) => self.input.push(c),
            _ => {}
        }
        self.input_suggested = false;
    }

    /// Looks at the typed path before adding it: what it is built with, whether it is a git
    /// repository, whether CORVX already knows it.
    fn preview_add(&mut self) {
        let expanded = expand_home(self.input.trim());
        if expanded.is_empty() {
            self.flash = "type the path of a project directory".into();
            return;
        }
        let workspace = self
            .selected_project()
            .map(|p| p.workspace.clone())
            .unwrap_or_else(|| "default".into());
        let path = match std::fs::canonicalize(&expanded) {
            Ok(p) if p.is_dir() && !is_empty_dir(&p) => p,
            Ok(p) if !p.is_dir() => {
                self.flash = format!("not a directory: {expanded}");
                return;
            }
            // A folder that is not there yet, or empty: start a new project in it.
            _ => {
                let path = PathBuf::from(expanded.trim_end_matches('/'));
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.preview = Some(AddPreview {
                    scan: Default::default(),
                    path,
                    git: false,
                    wired: false,
                    existing: None,
                    knowledge: true,
                    name,
                    workspace,
                    field: 2,
                    new: true,
                    about: String::new(),
                });
                self.mode = Mode::AddPreview;
                self.flash.clear();
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
            new: false,
            about: String::new(),
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
            KeyCode::Char(' ') if p.field == 2 && !p.new => p.knowledge = !p.knowledge,
            KeyCode::Backspace if p.field == 2 && p.new => {
                p.about.pop();
            }
            KeyCode::Char(c) if p.field == 2 && p.new => p.about.push(c),
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
                if preview.new {
                    self.create_project(&preview);
                } else {
                    self.add_project(&preview);
                }
            }
            _ => {}
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
        let mut goals: Vec<String> = Vec::new();
        for status in [
            corvx_proto::goal_status::ACTIVE,
            corvx_proto::goal_status::NEXT,
        ] {
            goals.extend(
                k.iter()
                    .flat_map(|k| k.goals.iter())
                    .filter(|g| g.status == status)
                    .map(|g| g.title.clone()),
            );
        }
        self.mission = Some(MissionForm {
            project: p.id,
            goal: String::new(),
            field: 0,
            // A mission usually serves the first active goal.
            serves: usize::from(!goals.is_empty()),
            goals,
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
                    1 => m.serves = step(m.serves, m.goals.len() + 1, back),
                    2 => m.level = step(m.level, corvx_proto::autonomy::ALL.len(), back),
                    3 => m.model = step(m.model, ORCHESTRATOR_MODELS.len(), back),
                    4 => m.agent_model = step(m.agent_model, AGENT_MODELS.len(), back),
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
        let serves = m
            .serves
            .checked_sub(1)
            .and_then(|i| m.goals.get(i))
            .cloned();
        let r = crate::start_orchestrator(
            &mut self.client,
            &crate::Mission {
                project: &m.project,
                goal: &goal,
                model,
                agent_model,
                human_approves_agents: m.human_approves,
                serves: serves.as_deref(),
                supervisor: None,
            },
        );
        let Some(started) = self.report(r) else {
            return;
        };
        let id = started.session.id;
        let r = self.refresh();
        self.report(r);
        // Select the new mission in the tree; its agents show up under it.
        self.mode = Mode::Normal;
        self.reveal(&id);
        self.flash = format!(
            "started orchestrator {id} on {model} ({level}) · live on the right · Enter attaches · t redirects it"
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
            Action::RemoveProject => self.start_remove(),
            Action::Refresh => {
                let r = self.refresh();
                self.report(r);
            }
            Action::RestartDaemon => self.restart_daemon(),
            Action::TalkToLead => {
                self.col = 0;
                self.start_talk();
            }
            Action::AddGoal => self.start_add_goal(),
            Action::EditGoals => self.edit_knowledge("goals"),
            Action::Inbox => self.open_inbox(),
            Action::Help => self.mode = Mode::Help,
            Action::Quit => self.quit = true,
        }
    }

    /// Asks for a path, starting in the folder your projects are in: an existing project
    /// is added; a folder that is not there yet becomes a new project.
    fn start_add(&mut self) {
        self.mode = Mode::AddPath;
        self.input = format!("{}/", tilde(&crate::projects_folder(&self.projects)));
        self.input_suggested = true;
        self.flash.clear();
    }

    /// Starts a new project: folder, git, registration, knowledge with the brief, and its
    /// lead, which then asks you about it. Shows the lead.
    fn create_project(&mut self, preview: &AddPreview) {
        let new = crate::NewProject {
            path: preview.path.clone(),
            name: preview.name.trim().to_string(),
            workspace: Some(preview.workspace.trim().to_string()),
            about: preview.about.trim().to_string(),
            lead: true,
        };
        let r = crate::create_project(&mut self.client, &new);
        let Some(made) = self.report(r) else {
            return;
        };
        let r = self.refresh();
        self.report(r);
        self.select_project(&made.project.id);
        if let Some(lead) = &made.lead {
            self.reveal(lead);
        }
        self.flash = format!(
            "created {} · its lead is asking you about it: Enter types to it, t sends one message",
            made.project.id
        );
    }

    fn start_remove(&mut self) {
        if self.projects.is_empty() {
            self.flash = "nothing to remove".into();
        } else {
            self.mode = Mode::ConfirmRemove;
        }
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
        "{} · {} · {} · {}",
        plural(k.guideposts.len(), "guidepost", "guideposts"),
        plural(sections("decisions"), "decision", "decisions"),
        plural(sections("conventions"), "convention", "conventions"),
        plural(k.journal_count, "past mission", "past missions")
    )
}

/// A path with your home folder written as ~.
fn tilde(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(rest) = path.strip_prefix(&home) {
            return if rest.as_os_str().is_empty() {
                "~".into()
            } else {
                format!("~/{}", rest.display())
            };
        }
    }
    path.display().to_string()
}

fn is_empty_dir(path: &Path) -> bool {
    std::fs::read_dir(path).is_ok_and(|mut d| d.next().is_none())
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
        Constraint::Length(top_height(f.area())),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(f.area());

    app.hits.borrow_mut().clear();
    draw_top(f, top, app);
    // The inbox and the rules span every project: they get all the room beside the
    // projects.
    let wide_view = matches!(
        app.mode,
        Mode::Inbox | Mode::Answer | Mode::Deny | Mode::Reject | Mode::Rules
    ) || in_inbox(app);
    let main = if wide_view {
        let [projects, rest] = Layout::horizontal([
            Constraint::Length((body.width / 8).clamp(14, 22)),
            Constraint::Min(0),
        ])
        .areas(body);
        draw_projects(f, projects, app);
        rest
    } else {
        draw_columns(f, body, app)
    };
    match app.mode {
        Mode::Inbox | Mode::Answer | Mode::Deny | Mode::Reject => draw_inbox(f, main, app),
        Mode::Talk if app.talk_from_inbox => draw_inbox(f, main, app),
        Mode::Rules => draw_rules(f, main, app),
        Mode::AddPreview if app.preview.is_some() => {
            draw_preview(f, main, app.preview.as_ref().unwrap())
        }
        _ => draw_detail(f, main, app),
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

/// The name in pixels with a shadow (two pixel rows to a line), drawn when there is room.
const LOGO: [&str; 3] = [
    "░▄▀▀▀░▄▀▀▄░█▀▀▄░█  ░█░▀▄░▄▀",
    "░█   ░█ ░█░█▀█ ░▀▄░▄▀ ░▄▀▄",
    " ░▀▀▀ ░▀▀ ░▀ ░▀  ░▀  ░▀  ░▀",
];

/// The banner's pixels are light, their shadows dim.
fn logo_spans(line: &str) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
    for c in line.chars() {
        let color = match c {
            '█' | '▀' | '▄' => Color::Rgb(220, 220, 220),
            _ => Color::Rgb(110, 110, 110),
        };
        match spans.last_mut() {
            Some(last) if last.style.fg == Some(color) => last.content.to_mut().push(c),
            _ => spans.push(Span::styled(c.to_string(), Style::default().fg(color))),
        }
    }
    spans
}

/// Rows the top takes: the logo and a blank line under it on a roomy terminal, else one.
fn top_height(area: Rect) -> u16 {
    if area.height >= 26 && area.width >= 80 {
        LOGO.len() as u16 + 1
    } else {
        1
    }
}

fn draw_top(f: &mut Frame, area: Rect, app: &App) {
    // Counts are chips: coloured when there is something, quiet when there is not.
    let chip = |n: usize, what: &str, color: Color| {
        if n > 0 {
            Span::styled(
                format!(" {n} {what} "),
                Style::default()
                    .fg(Color::Black)
                    .bg(color)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!(" {n} {what} "), Style::default().fg(DIM))
        }
    };
    let counts = vec![
        chip(count_yours_to_approve(app), "to approve", AMBER),
        Span::raw(" "),
        chip(
            count_kind(app, "blocked") + count_kind(app, "question"),
            "blocked",
            AMBER,
        ),
        Span::raw(" "),
        chip(count_your_turn(app), "waiting", ACCENT),
        Span::raw(" "),
        chip(
            count_kind(app, "done") + count_kind(app, "report"),
            "done",
            GREEN,
        ),
        Span::raw(" "),
        chip(count_kind(app, "failed"), "failed", RED),
    ];
    let with_orchestrators = app
        .attention
        .iter()
        .filter(|i| i.kind == "approval" && orchestrators(i))
        .count();
    let orchestrators_note = (with_orchestrators > 0).then(|| {
        Span::styled(
            format!("  ({with_orchestrators} with orchestrators)"),
            Style::default().fg(DIM),
        )
    });
    // Spend once there is any to show (sessions report it through Claude Code's status line).
    let mut spend = Vec::new();
    if app.usage.total_usd > 0.0 || !app.usage.limits.is_empty() {
        spend.push(Span::styled(
            format!("{} today", crate::usd(app.usage.today_usd)),
            Style::default().fg(Color::White),
        ));
    }
    for l in &app.usage.limits {
        if !spend.is_empty() {
            spend.push(Span::styled(" · ", Style::default().fg(DIM)));
        }
        spend.push(Span::styled(
            format!(
                "{} {:.0}%",
                if l.name == "five_hour" { "5h" } else { "week" },
                l.used_pct
            ),
            Style::default().fg(limit_color(l.used_pct)),
        ));
    }
    let daemon = if !app.connected {
        vec![Span::styled(
            "◌ daemon disconnected · retrying",
            Style::default().fg(AMBER),
        )]
    } else if app.newer_build {
        vec![Span::styled(
            "↻ newer build: restart it from :  ● corvxd",
            Style::default().fg(AMBER),
        )]
    } else {
        vec![
            Span::styled("● ", Style::default().fg(GREEN)),
            Span::styled(
                format!(
                    "corvxd {} · {} projects",
                    app.daemon_pid,
                    app.projects.len()
                ),
                Style::default().fg(DIM),
            ),
        ]
    };

    if area.height > 1 {
        // The logo; beside it what needs you, and under that the spend and the daemon.
        let logo_w = LOGO.iter().map(|l| l.chars().count()).max().unwrap_or(0);
        let logo: Vec<Line> = LOGO.iter().map(|l| Line::from(logo_spans(l))).collect();
        f.render_widget(Paragraph::new(logo), area);
        let x = area.x + logo_w as u16 + 4;
        let beside = Rect::new(x, area.y, (area.x + area.width).saturating_sub(x + 1), 2);
        let mut spend = if spend.is_empty() {
            Line::styled("no spend reported yet", Style::default().fg(DIM))
        } else {
            Line::from(spend)
        };
        // The note on orchestrators' approvals goes beside the counts, or under them.
        let mut counts = Line::from(counts);
        if let Some(note) = orchestrators_note {
            if counts.width() + note.width() <= beside.width as usize {
                counts.push_span(note);
            } else {
                let mut under = vec![Span::styled(
                    format!("{} · ", note.content.trim()),
                    note.style,
                )];
                under.extend(spend.spans);
                spend = Line::from(under);
            }
        }
        f.render_widget(Paragraph::new(counts), beside);
        let under = Rect::new(beside.x, beside.y + 1, beside.width, 1);
        let daemon = Line::from(daemon);
        let daemon = if spend.width() + daemon.width() + 3 <= under.width as usize {
            daemon
        } else {
            let color = if app.connected { GREEN } else { AMBER };
            Line::styled("●", Style::default().fg(color))
        };
        f.render_widget(Paragraph::new(spend), under);
        f.render_widget(Paragraph::new(daemon.right_aligned()), under);
        return;
    }

    let mut left = vec![
        Span::styled(
            " ◆ CORVX ",
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];
    left.extend(counts);
    left.extend(orchestrators_note);
    let left = Line::from(left);
    let room = (area.width as usize).saturating_sub(left.width() + 2);
    f.render_widget(Paragraph::new(left), area);
    let mut right = spend;
    right.push(Span::raw("   "));
    right.extend(daemon);
    right.push(Span::raw(" "));
    let right = Line::from(right);
    // Only what fits beside the counts; at least the dot.
    let right = if right.width() <= room {
        right
    } else {
        let color = if app.connected { GREEN } else { AMBER };
        Line::styled("● ", Style::default().fg(color))
    };
    f.render_widget(Paragraph::new(right.right_aligned()), area);
}

/// A session's estimated cost so far.
fn cost_of(app: &App, id: &str) -> f64 {
    app.costs.get(id).map_or(0.0, |c| c.0)
}

/// The model a session runs, short: what it last reported (Claude Code's status line, e.g.
/// "Opus 4.5"), or else the model it was started with (`--model sonnet`).
fn model_of(app: &App, s: &Session) -> Option<String> {
    short_model(app.models.get(&s.id).map(String::as_str), &s.command)
}

fn short_model(reported: Option<&str>, command: &[String]) -> Option<String> {
    let started = || {
        command
            .iter()
            .enumerate()
            .find_map(|(i, a)| match a.strip_prefix("--model=") {
                Some(m) => Some(m),
                None if a == "--model" => command.get(i + 1).map(String::as_str),
                None => None,
            })
    };
    let m = reported.or_else(started)?.trim();
    let m = m.strip_prefix("claude-").unwrap_or(m).to_lowercase();
    (!m.is_empty()).then_some(m)
}

/// A session's cost with everything under it: a mission's orchestrator and its agents.
fn tree_cost(app: &App, id: &str) -> f64 {
    let mut total = 0.0;
    let mut todo = vec![id.to_string()];
    while let Some(i) = todo.pop() {
        total += cost_of(app, &i);
        todo.extend(
            app.sessions
                .iter()
                .filter(|s| s.supervisor.as_deref() == Some(i.as_str()))
                .map(|s| s.id.clone()),
        );
    }
    total
}

/// A usage limit's colour: calm, then a warning, then red near the limit.
fn limit_color(pct: f64) -> Color {
    if pct >= 85.0 {
        RED
    } else if pct >= 60.0 {
        AMBER
    } else {
        DIM
    }
}

/// Cuts text to `width` characters, marking the cut.
fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The last few lines of a screen worth showing in a tile: not blank, and not the input box
/// or footer lines a Claude Code screen ends with.
fn tile_lines(screen: &str) -> Vec<String> {
    if in_background(screen) {
        return vec![
            "conversation moved to the background".into(),
            "Enter in its pane brings it back".into(),
        ];
    }
    let mut out: Vec<String> = screen
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .filter(|l| !corvx_core::claude_screen::footer_noise(l))
        .map(|l| l.trim().to_string())
        .collect();
    let keep = out.len().saturating_sub(4);
    out.drain(..keep);
    out
}

/// Claude Code moved its conversation into the background; Enter brings it back.
/// Recognised by its text (see `claude_screen`), plain or in colour.
fn in_background(screen: &str) -> bool {
    if screen.contains('\x1b') {
        let plain = screen
            .lines()
            .map(strip_escapes)
            .collect::<Vec<_>>()
            .join("\n");
        corvx_core::claude_screen::in_background(&plain)
    } else {
        corvx_core::claude_screen::in_background(screen)
    }
}

/// Text without terminal escape sequences.
fn strip_escapes(line: &str) -> String {
    ansi_line(line, usize::MAX)
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect()
}

/// One row of a terminal screen, with its colours and styles (SGR escape codes) turned into
/// styled spans, cut to `width` characters. Other escape sequences are dropped.
fn ansi_line(line: &str, width: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut style = Style::default();
    let mut text = String::new();
    let mut shown = 0usize;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    let mut params = String::new();
                    let mut end = ' ';
                    for d in chars.by_ref() {
                        if ('@'..='~').contains(&d) {
                            end = d;
                            break;
                        }
                        params.push(d);
                    }
                    if end == 'm' {
                        if !text.is_empty() {
                            spans.push(Span::styled(std::mem::take(&mut text), style));
                        }
                        style = apply_sgr(style, &params);
                    }
                }
                Some(']') => {
                    // An OSC (e.g. a link or title): up to BEL or ESC \.
                    chars.next();
                    while let Some(d) = chars.next() {
                        if d == '\x07' {
                            break;
                        }
                        if d == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            }
            continue;
        }
        if c.is_control() {
            continue;
        }
        if shown >= width {
            break;
        }
        text.push(c);
        shown += 1;
    }
    if !text.is_empty() {
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

/// A style after an SGR sequence's parameters (`1;38;5;208`, and so on).
fn apply_sgr(mut style: Style, params: &str) -> Style {
    let nums: Vec<u16> = if params.is_empty() {
        vec![0]
    } else {
        params
            .split([';', ':'])
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let basic = |n: u16| match n {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        _ => Color::Gray,
    };
    let bright = |n: u16| match n {
        0 => Color::DarkGray,
        1 => Color::LightRed,
        2 => Color::LightGreen,
        3 => Color::LightYellow,
        4 => Color::LightBlue,
        5 => Color::LightMagenta,
        6 => Color::LightCyan,
        _ => Color::White,
    };
    let mut i = 0;
    while i < nums.len() {
        let n = nums[i];
        match n {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            7 => style = style.add_modifier(Modifier::REVERSED),
            9 => style = style.add_modifier(Modifier::CROSSED_OUT),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            27 => style = style.remove_modifier(Modifier::REVERSED),
            29 => style = style.remove_modifier(Modifier::CROSSED_OUT),
            30..=37 => style = style.fg(basic(n - 30)),
            39 => style.fg = None,
            40..=47 => style = style.bg(basic(n - 40)),
            49 => style.bg = None,
            90..=97 => style = style.fg(bright(n - 90)),
            100..=107 => style = style.bg(bright(n - 100)),
            38 | 48 => {
                let color = match nums.get(i + 1) {
                    Some(5) => {
                        let c = nums.get(i + 2).copied().unwrap_or(0);
                        i += 2;
                        Some(Color::Indexed(c as u8))
                    }
                    Some(2) => {
                        let rgb = |k: usize| nums.get(i + k).copied().unwrap_or(0) as u8;
                        let c = Color::Rgb(rgb(2), rgb(3), rgb(4));
                        i += 4;
                        Some(c)
                    }
                    _ => None,
                };
                if let Some(c) = color {
                    style = if n == 38 { style.fg(c) } else { style.bg(c) };
                }
            }
            _ => {}
        }
        i += 1;
    }
    style
}

/// What a key sends to a terminal program, as the terminal would send it.
fn key_bytes(code: KeyCode, ctrl: bool, alt: bool) -> Option<String> {
    let base = match code {
        KeyCode::Char(c) if ctrl && c.is_ascii_alphabetic() => {
            ((c.to_ascii_lowercase() as u8 - b'a' + 1) as char).to_string()
        }
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "\r".into(),
        KeyCode::Backspace => "\x7f".into(),
        KeyCode::Tab => "\t".into(),
        KeyCode::BackTab => "\x1b[Z".into(),
        KeyCode::Esc => "\x1b".into(),
        KeyCode::Up => "\x1b[A".into(),
        KeyCode::Down => "\x1b[B".into(),
        KeyCode::Right => "\x1b[C".into(),
        KeyCode::Left => "\x1b[D".into(),
        KeyCode::Home => "\x1b[H".into(),
        KeyCode::End => "\x1b[F".into(),
        KeyCode::PageUp => "\x1b[5~".into(),
        KeyCode::PageDown => "\x1b[6~".into(),
        KeyCode::Delete => "\x1b[3~".into(),
        _ => return None,
    };
    Some(if alt { format!("\x1b{base}") } else { base })
}

/// The project goal a mission serves, from what it was started with.
fn serves_of(s: &Session) -> Option<String> {
    let marker = "serves the project goal \"";
    s.command.iter().find_map(|a| {
        let at = a.find(marker)? + marker.len();
        let rest = &a[at..];
        Some(rest[..rest.find('"')?].to_string())
    })
}

/// A session's title: its label without the "orchestrator · " or "agent · " in front.
fn title_of(s: &Session) -> String {
    crate::display_name(s)
        .trim_start_matches("orchestrator · ")
        .trim_start_matches("agent · ")
        .trim_start_matches(crate::LEAD_LABEL)
        .to_string()
}

fn column_block(title: String, focused: bool) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(Style::default().fg(if focused { ACCENT } else { BORDER }))
}

/// The four columns: projects, the selected project's lead and missions, the agents of
/// what is selected, and on the right the selection itself.
fn draw_columns(f: &mut Frame, area: Rect, app: &App) -> Rect {
    let w = area.width;
    let projects_w = (w / 8).clamp(14, 22);
    let missions_w = (w / 5).clamp(24, 34);
    let agents_w = (w / 5).clamp(26, 38);
    let [projects, missions, agents, rest] = Layout::horizontal([
        Constraint::Length(projects_w),
        Constraint::Length(missions_w),
        Constraint::Length(agents_w),
        Constraint::Min(0),
    ])
    .areas(area);
    draw_projects(f, projects, app);
    draw_missions_col(f, missions, app);
    draw_agents_col(f, agents, app);
    rest
}

fn draw_projects(f: &mut Frame, area: Rect, app: &App) {
    let width = area.width.saturating_sub(2) as usize;
    let mut workspaces: Vec<&str> = app.projects.iter().map(|p| p.workspace.as_str()).collect();
    workspaces.sort();
    workspaces.dedup();
    let several = workspaces.len() > 1;
    let mut lines: Vec<Line> = Vec::new();
    let mut last_ws: Option<&str> = None;
    app.hits.borrow_mut().push((area, Hit::Column(0)));
    for (i, p) in app.projects.iter().enumerate() {
        if several && last_ws != Some(p.workspace.as_str()) {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(Line::styled(
                p.workspace.to_uppercase(),
                Style::default().fg(DIM).add_modifier(Modifier::BOLD),
            ));
            last_ws = Some(p.workspace.as_str());
        }
        let y = area.y + 1 + lines.len() as u16;
        if y + 1 < area.y + area.height {
            app.hits
                .borrow_mut()
                .push((Rect::new(area.x, y, area.width, 1), Hit::Project(i)));
        }
        let selected = i == app.selected;
        let running = app
            .sessions
            .iter()
            .filter(|s| s.project == p.id && s.state == SessionState::Running)
            .count();
        // ▲ only for what needs you. While anything works, the project is working, even
        // when its lead waits (on its missions); once all is quiet, ● says it is your turn.
        let items: Vec<&AttentionItem> =
            app.attention.iter().filter(|a| a.project == p.id).collect();
        let needs = items.iter().filter(|a| needs_you(a)).count();
        let busy = app
            .sessions
            .iter()
            .filter(|s| s.project == p.id && at_work(s))
            .count();
        let turn = items.iter().filter(|a| your_turn(app, a)).count();
        let (badge, badge_color) = if needs > 0 {
            (format!("▲{needs}"), AMBER)
        } else if busy > 0 {
            (format!("{}{busy}", spinner()), GREEN)
        } else if turn > 0 {
            (format!("●{turn}"), ACCENT)
        } else if running > 0 {
            (format!("●{running}"), DIM)
        } else {
            (String::new(), DIM)
        };
        let name = clip(&p.id, width.saturating_sub(3 + badge.chars().count()));
        let pad = width.saturating_sub(2 + name.chars().count() + badge.chars().count());
        let base = if selected {
            Style::default().bg(SELECTED_BG)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(if selected { "▌ " } else { "  " }, base.fg(ACCENT)),
            Span::styled(
                name,
                if selected {
                    base.fg(Color::White).add_modifier(Modifier::BOLD)
                } else {
                    base
                },
            ),
            Span::styled(" ".repeat(pad), base),
            Span::styled(badge, base.fg(badge_color)),
        ]));
    }
    if app.projects.is_empty() {
        lines.push(Line::styled("none yet · a", Style::default().fg(DIM)));
    }
    f.render_widget(
        Paragraph::new(lines).block(column_block(" projects ".into(), app.col == 0)),
        area,
    );
}

const SELECTED_BG: Color = Color::Rgb(30, 41, 59);

/// The selected project's lead and missions, two lines each.
fn draw_missions_col(f: &mut Frame, area: Rect, app: &App) {
    let Some(p) = app.selected_project() else {
        f.render_widget(
            Paragraph::new("").block(column_block(" missions ".into(), false)),
            area,
        );
        return;
    };
    let width = area.width.saturating_sub(2) as usize;
    let rows = app.col1_rows(&p.id);
    let sel = app.col1_row().map(|r| r.key);
    let needs = |id: &str| {
        app.attention
            .iter()
            .filter(|i| i.session_id == id && needs_you(i))
            .count()
    };
    let dim = Style::default().fg(DIM);
    let mut lines: Vec<Line> = Vec::new();
    // Each item's first line, how many lines it takes, and its key: for the mouse.
    let mut items: Vec<(usize, usize, String)> = Vec::new();
    let mut heading: Option<&str> = None;
    for r in &rows {
        let selected = sel.as_deref() == Some(r.key.as_str());
        let base = if selected && app.col == 1 {
            Style::default().bg(SELECTED_BG)
        } else {
            Style::default()
        };
        let bar = if selected { "▌" } else { " " };
        let indent = "  ".repeat(r.depth);
        // Two lines: a title, and a line of facts under it.
        let item = |first: Vec<Span<'static>>, second: Vec<Span<'static>>| -> [Line<'static>; 2] {
            let mut a = vec![Span::styled(format!("{bar}{indent} "), base.fg(ACCENT))];
            a.extend(
                first
                    .into_iter()
                    .map(|s| Span::styled(s.content, s.style.patch(base))),
            );
            let used: usize = a.iter().map(|s| s.content.chars().count()).sum();
            a.push(Span::styled(" ".repeat(width.saturating_sub(used)), base));
            let mut b = vec![Span::styled(format!("{bar}{indent}   "), base.fg(ACCENT))];
            let mut room = width.saturating_sub(indent.len() + 4);
            for sp in second {
                let text = clip(&sp.content, room);
                room = room.saturating_sub(text.chars().count());
                b.push(Span::styled(text, sp.style.patch(base)));
            }
            b.push(Span::styled(" ".repeat(room), base));
            [Line::from(a), Line::from(b)]
        };
        let section = match &r.kind {
            RowKind::Mission(_) if r.depth == 0 => Some("MISSIONS"),
            RowKind::Loose(_) => Some("SESSIONS"),
            RowKind::History(_) => Some(""),
            _ => None,
        };
        if let Some(name) = section {
            if heading != Some(name) {
                lines.push(Line::from(""));
                if !name.is_empty() {
                    lines.push(Line::styled(
                        format!(" {name}"),
                        Style::default().fg(DIM).add_modifier(Modifier::BOLD),
                    ));
                }
                heading = Some(name);
            }
        }
        let start = lines.len();
        match &r.kind {
            RowKind::Lead(None) => lines.extend(item(
                vec![Span::styled("◇ lead", dim)],
                vec![Span::styled("not started · t talks to it", dim)],
            )),
            RowKind::Lead(Some(s)) => lines.extend(item(
                vec![
                    Span::styled(
                        "◇ lead",
                        Style::default().fg(LEAD).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if needs(&s.id) > 0 {
                            format!("  ▲{}", needs(&s.id))
                        } else {
                            String::new()
                        },
                        Style::default().fg(AMBER),
                    ),
                ],
                if waits_on_its_work(app, s) {
                    vec![
                        Span::styled(format!("{} ", spinner()), Style::default().fg(GREEN)),
                        Span::styled("missions working", agent_style(s)),
                    ]
                } else {
                    vec![state_glyph(s), Span::styled(short_state(s), agent_style(s))]
                }
                .into_iter()
                .chain([Span::styled(
                    format!(
                        "{}{}",
                        model_of(app, s)
                            .map(|m| format!(" · {m}"))
                            .unwrap_or_default(),
                        match cost_of(app, &s.id) {
                            c if c > 0.0 => format!(" · {}", crate::usd(c)),
                            _ => " · t talks to it".into(),
                        }
                    ),
                    dim,
                )])
                .collect(),
            )),
            RowKind::Mission(s) | RowKind::Loose(s) => {
                let under: Vec<&Session> = app
                    .sessions
                    .iter()
                    .filter(|a| a.supervisor.as_deref() == Some(s.id.as_str()))
                    .collect();
                let n = needs(&s.id) + under.iter().map(|a| needs(&a.id)).sum::<usize>();
                let running = under
                    .iter()
                    .filter(|a| a.state == SessionState::Running)
                    .count();
                let mut facts = String::new();
                if let Some(m) = model_of(app, s) {
                    facts.push_str(&format!("  {m}"));
                }
                if !under.is_empty() {
                    // A small bar: one block per agent, filled while it runs.
                    let shown = under.len().min(6);
                    let filled = running.min(shown);
                    facts.push_str(&format!(
                        "  {}{}",
                        "▰".repeat(filled),
                        "▱".repeat(shown - filled)
                    ));
                }
                let spent = tree_cost(app, &s.id);
                if spent > 0.0 {
                    facts.push_str(&format!("  {}", crate::usd(spent)));
                }
                if let Some(goal) = serves_of(s) {
                    facts.push_str(&format!("  → {goal}"));
                }
                let badge = if n > 0 {
                    format!(" ▲{n}")
                } else {
                    String::new()
                };
                let title_room = width.saturating_sub(indent.len() + 4 + badge.chars().count());
                let mission = matches!(r.kind, RowKind::Mission(_));
                lines.extend(item(
                    vec![
                        Span::styled(
                            if mission { "◆ " } else { "▪ " },
                            Style::default().fg(if s.state == SessionState::Running {
                                ACCENT
                            } else {
                                DIM
                            }),
                        ),
                        Span::styled(
                            clip(&title_of(s), title_room),
                            if selected {
                                Style::default()
                                    .fg(Color::White)
                                    .add_modifier(Modifier::BOLD)
                            } else if s.state == SessionState::Running {
                                Style::default()
                            } else {
                                dim
                            },
                        ),
                        Span::styled(badge, Style::default().fg(AMBER)),
                    ],
                    vec![
                        state_glyph(s),
                        Span::styled(short_state(s), agent_style(s)),
                        Span::styled(facts, dim),
                    ],
                ));
            }
            RowKind::History(n) => {
                lines.push(Line::from(vec![
                    Span::styled(format!("{bar} "), base.fg(ACCENT)),
                    Span::styled(
                        format!("{} earlier missions · {n}", if r.open { "▾" } else { "▸" }),
                        dim.patch(base),
                    ),
                ]));
            }
            _ => {}
        }
        items.push((start, lines.len() - start, r.key.clone()));
    }
    // Keep the selection in view.
    let height = area.height.saturating_sub(2) as usize;
    let sel_end = items
        .iter()
        .find(|(_, _, k)| Some(k.as_str()) == sel.as_deref())
        .map(|(s, t, _)| s + t)
        .unwrap_or(0);
    let skip = sel_end.saturating_sub(height);
    {
        let mut hits = app.hits.borrow_mut();
        hits.push((area, Hit::Column(1)));
        for (start, tall, key) in items {
            if start < skip || start - skip + tall > height {
                continue;
            }
            let y = area.y + 1 + (start - skip) as u16;
            hits.push((
                Rect::new(area.x, y, area.width, tall as u16),
                Hit::Mission(key),
            ));
        }
    }
    let lines: Vec<Line> = lines.into_iter().skip(skip).collect();
    f.render_widget(
        Paragraph::new(lines).block(column_block(format!(" {} ", p.name), app.col == 1)),
        area,
    );
}

/// The agents of what is selected, as tiles showing the last lines of each one's screen.
fn draw_agents_col(f: &mut Frame, area: Rect, app: &App) {
    let rows = app.col2_rows();
    let title = match app.col {
        0 => " working now ".to_string(),
        _ => match app.col1_row().and_then(|r| r.session().cloned()) {
            Some(s) if crate::is_lead(&s) => " under the lead ".to_string(),
            Some(_) => format!(" agents · {} ", rows.len()),
            None => " agents ".to_string(),
        },
    };
    let block = column_block(title, app.col == 2);
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.hits.borrow_mut().push((area, Hit::Column(2)));
    if rows.is_empty() {
        let text = match (app.col, app.col1_row().map(|r| r.kind)) {
            (0, _) => "no agents running",
            (_, Some(RowKind::Lead(_))) => "the lead's missions are in the missions column",
            (_, Some(RowKind::Mission(_))) => "no agents (yet)",
            _ => "",
        };
        f.render_widget(
            Paragraph::new(Line::styled(format!(" {text}"), Style::default().fg(DIM)))
                .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }
    const TILE: u16 = 6;
    let fits = (inner.height / TILE).max(1) as usize;
    let sel = app.col2_row().map(|r| r.key);
    let at = rows
        .iter()
        .position(|r| Some(r.key.as_str()) == sel.as_deref())
        .unwrap_or(0);
    let first = at.saturating_sub(fits.saturating_sub(1));
    for (n, r) in rows.iter().enumerate().skip(first).take(fits) {
        let RowKind::Agent(s) = &r.kind else { continue };
        let y = inner.y + ((n - first) as u16) * TILE;
        let indent = (r.depth as u16 * 2).min(inner.width / 3);
        let rect = Rect::new(
            inner.x + indent,
            y,
            inner.width - indent,
            TILE.min(inner.y + inner.height - y),
        );
        app.hits.borrow_mut().push((rect, Hit::Agent(s.id.clone())));
        let selected = Some(r.key.as_str()) == sel.as_deref() && app.col == 2;
        let w = rect.width.saturating_sub(2) as usize;
        let state = short_state(s);
        let model = model_of(app, s)
            .map(|m| format!("{m} "))
            .unwrap_or_default();
        let name = clip(
            &title_of(s),
            w.saturating_sub(state.chars().count() + model.chars().count() + 6),
        );
        // What needs you first (▲); then what is yours to look at when you like (●).
        let mine = |i: &&AttentionItem| i.session_id == s.id;
        let item = app
            .attention
            .iter()
            .filter(mine)
            .find(|i| needs_you(i))
            .map(|i| (i, "▲", AMBER))
            .or_else(|| {
                app.attention
                    .iter()
                    .filter(mine)
                    .find(|i| i.kind != "waiting" || your_turn(app, i))
                    .map(|i| (i, "●", ACCENT))
            });
        let mut body: Vec<Line> = Vec::new();
        if let Some((i, mark, color)) = item {
            body.push(Line::styled(
                clip(&format!("{mark} {}", i.summary), w),
                Style::default().fg(color),
            ));
        } else if let Some(d) = &s.agent_detail {
            body.push(Line::styled(clip(d, w), agent_style(s)));
        }
        let tail = app.tiles.get(&s.id).cloned().unwrap_or_default();
        let room = (TILE as usize).saturating_sub(2 + body.len());
        for l in tail.iter().skip(tail.len().saturating_sub(room)) {
            body.push(Line::styled(clip(l, w), Style::default().fg(DIM)));
        }
        let spent = cost_of(app, &s.id);
        let title = Line::from(vec![
            Span::raw(" "),
            state_glyph(s),
            Span::styled(
                format!("{name} "),
                if selected {
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            ),
            Span::styled(
                format!(
                    "{model}{}",
                    if spent > 0.0 {
                        format!("{} ", crate::usd(spent))
                    } else {
                        String::new()
                    }
                ),
                Style::default().fg(DIM),
            ),
        ]);
        let border = if selected {
            ACCENT
        } else if item.is_some_and(|(_, _, c)| c == AMBER) {
            AMBER
        } else {
            BORDER
        };
        f.render_widget(
            Paragraph::new(body).block(
                Block::bordered()
                    .border_type(if selected {
                        BorderType::Thick
                    } else {
                        BorderType::Rounded
                    })
                    .title(title)
                    .title(Line::styled(format!(" {state} "), agent_style(s)).right_aligned())
                    .border_style(Style::default().fg(border)),
            ),
            rect,
        );
    }
}

const LEAD: Color = Color::Rgb(188, 140, 255);

/// A glyph for a session's state: a spinner while it works, and a shape for the rest, so
/// state never rests on colour alone.
fn state_glyph(s: &Session) -> Span<'static> {
    use corvx_proto::AgentState::*;
    let (glyph, color) = if s.state == SessionState::Lost {
        ("◌", AMBER)
    } else if s.state != SessionState::Running {
        ("·", DIM)
    } else {
        match s.agent_state {
            Working | Unknown => (spinner(), GREEN),
            Waiting | Idle => ("●", ACCENT),
            Blocked => ("▲", AMBER),
            Done => ("✓", GREEN),
            Failed => ("✕", RED),
            Stopped => ("·", DIM),
        }
    };
    Span::styled(format!("{glyph} "), Style::default().fg(color))
}

/// A session's state in a word or two, for the tree.
fn short_state(s: &Session) -> String {
    use corvx_proto::AgentState::*;
    if s.state != SessionState::Running {
        return match s.state {
            SessionState::Lost => "interrupted".into(),
            _ => match s.ended_at {
                Some(t) => format!("ended {}", age_label(now_secs() - t)),
                None => "ended".into(),
            },
        };
    }
    let state = match s.agent_state {
        Unknown => "running",
        Working => "working",
        Waiting => "waiting",
        Blocked => "needs you",
        Done => "done",
        Failed => "failed",
        Idle => "idle",
        Stopped => "stopped",
    };
    if s.autopilot {
        format!("auto·{state}")
    } else {
        state.to_string()
    }
}

/// The right side: whatever the tree has selected.
fn draw_detail(f: &mut Frame, area: Rect, app: &App) {
    let Some(row) = app.selected_row() else {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(" welcome ")
            .border_style(Style::default().fg(BORDER));
        f.render_widget(Paragraph::new(welcome_lines()).block(block), area);
        return;
    };
    let Some(p) = app.projects.iter().find(|p| p.id == row.project) else {
        return;
    };
    // Something under the selection waits for a yes or no: it comes first.
    let area = match app.pending_decision() {
        Some(item) => draw_decision(f, area, app, &item),
        None => area,
    };
    match &row.kind {
        RowKind::Project => draw_project_detail(f, area, app, p),
        RowKind::History(n) => {
            let lines = vec![
                Line::styled(
                    format!(
                        "{n} earlier mission{} on {}",
                        if *n == 1 { "" } else { "s" },
                        p.id
                    ),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Line::styled(
                    "→ opens the list; select one to read its journal entry.",
                    Style::default().fg(DIM),
                ),
            ];
            f.render_widget(
                Paragraph::new(lines).block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .title(" earlier missions ")
                        .border_style(Style::default().fg(BORDER)),
                ),
                area,
            );
        }
        RowKind::Lead(None) => {
            let dim = Style::default().fg(DIM);
            let lines = vec![
                Line::styled(
                    format!("The lead of {}", p.id),
                    Style::default().fg(LEAD).add_modifier(Modifier::BOLD),
                ),
                Line::from(""),
                Line::raw("The agent you talk to about this project, in your own words."),
                Line::raw("It reads the brief, goals, decisions and past missions; it records new goals"),
                Line::raw("for you to accept, starts missions for them, passes your corrections on to"),
                Line::raw("them, and tells you how they went. It runs on a strong model until you stop it."),
                Line::from(""),
                Line::styled("t and say something to start it · Enter starts it with a status request", dim),
            ];
            f.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .title(" lead · not started ")
                        .border_style(Style::default().fg(BORDER)),
                ),
                area,
            );
        }
        _ => {
            if let Some(s) = row.session() {
                draw_session_detail(f, area, app, &row, s);
            }
        }
    }
}

/// A request waiting for a yes or no, above the rest of the detail. Returns the room left.
fn draw_decision(f: &mut Frame, area: Rect, app: &App, item: &AttentionItem) -> Rect {
    let [top, rest] = Layout::vertical([Constraint::Length(5), Constraint::Min(0)]).areas(area);
    let who = app
        .sessions
        .iter()
        .find(|s| s.id == item.session_id)
        .map(|s| {
            let what = if crate::is_lead(s) {
                "lead"
            } else if crate::missions::is_orchestrator(s, &app.sessions) {
                "orchestrator"
            } else {
                "agent"
            };
            format!("{what} · {}", title_of(s))
        })
        .unwrap_or_else(|| item.session_id.clone());
    let w = top.width.saturating_sub(4) as usize;
    let key = |k: &str, what: &str| {
        vec![
            Span::styled(
                format!(" {k} "),
                Style::default()
                    .fg(Color::Black)
                    .bg(AMBER)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {what}   "), Style::default().fg(Color::Gray)),
        ]
    };
    let mut keys = vec![Span::raw(" ")];
    keys.extend(key(
        "y",
        if item.kind == "trust" {
            "trust it"
        } else {
            "approve"
        },
    ));
    if item.kind == "approval" {
        keys.extend(key("Y", "always"));
    }
    keys.extend(key(
        "n",
        if item.kind == "trust" {
            "don't (Claude exits)"
        } else {
            "deny"
        },
    ));
    if item.kind == "approval" && orchestrators(item) {
        keys.push(Span::styled(
            "its orchestrator is deciding; you can decide first",
            Style::default().fg(DIM),
        ));
    }
    let lines = vec![
        Line::from(vec![
            Span::styled(
                " ▲ ",
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                clip(&item.summary, w.saturating_sub(3)),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::styled(
            format!("   {}", clip(&who, w.saturating_sub(3))),
            Style::default().fg(DIM),
        ),
        Line::from(keys),
    ];
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Thick)
                .title(format!(" {} ", kind_label(&item.kind)))
                .border_style(Style::default().fg(AMBER)),
        ),
        top,
    );
    rest
}

/// A lead, mission or agent: what it is and does, then its screen live (or, for an ended
/// mission, its journal entry).
fn draw_session_detail(f: &mut Frame, area: Rect, app: &App, row: &Row, s: &Session) {
    let dim = Style::default().fg(DIM);
    let what = match &row.kind {
        RowKind::Lead(_) => "lead",
        RowKind::Mission(_) => "mission",
        RowKind::Agent(_) => "agent",
        _ => "session",
    };
    let name = crate::display_name(s);
    let name = name
        .trim_start_matches("orchestrator · ")
        .trim_start_matches("agent · ")
        .trim_start_matches(crate::LEAD_LABEL)
        .to_string();
    let under: Vec<&Session> = app
        .sessions
        .iter()
        .filter(|a| a.supervisor.as_deref() == Some(s.id.as_str()))
        .collect();
    let model = model_of(app, s);
    let mut facts = vec![s.id.clone()];
    if let Some(m) = model {
        facts.push(format!("on {m}"));
    }
    if let Some(&(own, context)) = app.costs.get(&s.id) {
        let all = tree_cost(app, &s.id);
        facts.push(if all > own + 0.005 {
            format!(
                "{} · {} with its {}",
                crate::usd(own),
                crate::usd(all),
                if matches!(row.kind, RowKind::Lead(_)) {
                    "missions"
                } else {
                    "agents"
                }
            )
        } else {
            crate::usd(own)
        });
        if let (Some(c), SessionState::Running) = (context, s.state) {
            facts.push(format!("context {c}%"));
        }
    }
    if !under.is_empty() {
        let running = under
            .iter()
            .filter(|a| a.state == SessionState::Running)
            .count();
        facts.push(format!(
            "{} {} ({running} running)",
            under.len(),
            if matches!(row.kind, RowKind::Lead(_)) {
                "missions"
            } else {
                "agents"
            }
        ));
    }
    if let Some(goal) = serves_of(s) {
        facts.push(format!("serves \"{goal}\""));
    }
    if s.autopilot {
        facts.push("autopilot".into());
    }
    if let Some(w) = &s.worktree {
        facts.push(format!(
            "branch corvx/{}",
            w.rsplit('/').next().unwrap_or(&s.id)
        ));
    }
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{what}  "),
                Style::default().fg(if what == "lead" { LEAD } else { ACCENT }),
            ),
            Span::styled(name, Style::default().add_modifier(Modifier::BOLD)),
        ]),
        Line::styled(facts.join(" · "), dim),
        Line::styled(agent_label(s), agent_style(s)),
    ];
    for item in app.attention.iter().filter(|i| i.session_id == s.id) {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<8}", kind_label(&item.kind)),
                kind_style(&item.kind),
            ),
            Span::raw(item.summary.clone()),
            Span::styled("  · i inbox", dim),
        ]));
    }
    let head = lines.len() as u16 + 2;
    let [top, body] = Layout::vertical([Constraint::Length(head), Constraint::Min(0)]).areas(area);
    let typing = app.mode == Mode::Live && app.live_id.as_deref() == Some(s.id.as_str());
    let keys = match s.state {
        SessionState::Running if typing => {
            " your keys go here · Ctrl+\\ back · wheel scrolls back "
        }
        SessionState::Running => " Enter type to it · t one message · f full screen · x stop ",
        SessionState::Lost => " R resume · c clean up worktrees ",
        _ => " c clean up worktrees · PgUp/PgDn scroll ",
    };
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(format!(" {what} "))
                .border_style(Style::default().fg(BORDER)),
        ),
        top,
    );
    let inner_h = body.height.saturating_sub(2) as usize;
    let inner_w = body.width.saturating_sub(2) as usize;
    app.live_size.set((inner_w as u16, inner_h as u16));
    let mut away = false;
    let (title, text): (String, Vec<Line>) = if s.state == SessionState::Running {
        let screen = match &app.peek {
            Some((id, text)) if *id == s.id => text.clone(),
            _ => String::new(),
        };
        away = in_background(&screen);
        app.hits.borrow_mut().push((body, Hit::Live));
        let mut rows: Vec<&str> = screen.lines().collect();
        while rows
            .last()
            .is_some_and(|l| strip_escapes(l).trim().is_empty())
        {
            rows.pop();
        }
        // Scrolled back: the window ends that many lines above the bottom.
        let back = (app.live_scroll as usize).min(rows.len().saturating_sub(inner_h));
        let end = rows.len() - back;
        let start = end.saturating_sub(inner_h);
        app.live_area.set((
            Rect::new(body.x + 1, body.y + 1, inner_w as u16, inner_h as u16),
            start,
        ));
        (
            if back > 0 {
                format!(" live · ↑ {back} lines back · scroll down to return ")
            } else if away && typing {
                " live · the conversation moved to the background · Enter brings it back ".into()
            } else if away {
                " live · the conversation moved to the background · in the pane, Enter brings it back "
                    .into()
            } else if typing {
                " live · typing ".into()
            } else {
                " live ".into()
            },
            rows[start..end]
                .iter()
                .map(|l| ansi_line(l, inner_w))
                .collect(),
        )
    } else {
        let journal = app.journals.get(&row.project).and_then(|entries| {
            entries
                .iter()
                .find(|j| j.path.ends_with(&format!("-{}.md", s.id)))
        });
        match journal {
            Some(j) => (
                format!(" journal · {} ", j.path),
                j.text
                    .lines()
                    .map(|l| {
                        if l.starts_with('#') {
                            Line::styled(
                                l.to_string(),
                                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                            )
                        } else {
                            Line::raw(l.to_string())
                        }
                    })
                    .collect(),
            ),
            None => (
                " ended ".into(),
                vec![Line::styled(
                    match (&row.kind, s.exit_code) {
                        (RowKind::Mission(_), _) => "It wrote no journal entry.".to_string(),
                        (_, Some(code)) => format!("It exited with code {code}."),
                        _ => "It has ended.".to_string(),
                    },
                    dim,
                )],
            ),
        }
    };
    f.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((
                if s.state == SessionState::Running {
                    0
                } else {
                    app.detail_scroll
                },
                0,
            ))
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(if away {
                        Line::styled(title, Style::default().fg(AMBER))
                    } else {
                        Line::raw(title)
                    })
                    .title_bottom(Line::styled(
                        keys,
                        if typing {
                            Style::default().fg(GREEN)
                        } else {
                            dim
                        },
                    ))
                    .border_style(Style::default().fg(if typing { GREEN } else { BORDER })),
            ),
        body,
    );
}

/// A project: what it is, where it is heading, what needs you, and the next step.
fn draw_project_detail(f: &mut Frame, area: Rect, app: &App, p: &Project) {
    let k = app.knowledge_state.get(&p.id).cloned().unwrap_or_default();
    let dim = Style::default().fg(DIM);
    let label = |s: &str| Span::styled(format!("{s:<11}"), dim);
    let or = |v: &Option<String>, none: &str| v.clone().unwrap_or_else(|| none.to_string());
    let section = |s: &str| Line::styled(s.to_string(), Style::default().fg(ACCENT));
    let waiting = app.proposals.values().filter(|x| x.project == p.id).count();
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                p.name.clone(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("   {}", p.path), dim),
        ]),
        Line::from(vec![
            label("stack"),
            Span::raw(if p.stack.is_empty() {
                "not detected".into()
            } else {
                p.stack.join(", ")
            }),
            Span::styled("   test ", dim),
            Span::raw(or(&p.test_command, "not detected")),
            Span::styled("   branch ", dim),
            Span::raw(or(&p.branch, "-")),
        ]),
        Line::from(vec![
            label("autonomy"),
            Span::styled(p.autonomy.clone(), level_style(&p.autonomy)),
            Span::styled(
                format!(
                    "  {}",
                    clip(
                        crate::level_meaning(&p.autonomy),
                        area.width.saturating_sub(26) as usize
                    )
                ),
                dim,
            ),
        ]),
        Line::from({
            let spent = app.usage.projects.iter().find(|u| u.project == p.id);
            vec![
                label("spend"),
                Span::raw(format!(
                    "{} today · {} in all",
                    crate::usd(spent.map_or(0.0, |u| u.today_usd)),
                    crate::usd(spent.map_or(0.0, |u| u.total_usd))
                )),
                Span::styled("  estimated by Claude Code at list prices", dim),
            ]
        }),
        Line::from(vec![
            label("knowledge"),
            if !k.wired {
                Span::styled("not set up · K sets it up", Style::default().fg(AMBER))
            } else if !k.brief_written {
                Span::styled(
                    "set up · brief not written yet: b",
                    Style::default().fg(AMBER),
                )
            } else if waiting > 0 {
                Span::styled(
                    format!("{waiting} proposal(s) waiting in the inbox"),
                    kind_style("proposal"),
                )
            } else {
                Span::raw(format!(
                    "set up · {} mission{} in the journal",
                    k.journal,
                    if k.journal == 1 { "" } else { "s" }
                ))
            },
        ]),
        Line::from(""),
        section("GOALS"),
    ];
    if k.goals.is_empty() {
        lines.push(Line::styled("  none yet · g adds one", dim));
    }
    for (title, status) in &k.goals {
        let (glyph, style) = match status.as_str() {
            "active" => ("●", Style::default().fg(GREEN)),
            "next" => ("○", Style::default()),
            "done" => ("✓", dim),
            _ => ("×", dim),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {glyph} "), style),
            Span::styled(
                title.clone(),
                if status == "done" || status == "dropped" {
                    dim
                } else {
                    Style::default()
                },
            ),
            Span::styled(format!("  {status}"), dim),
        ]));
    }
    if k.guideposts > 0 {
        lines.push(Line::styled(
            format!(
                "  {} guidepost{} · G edits goals and guideposts",
                k.guideposts,
                if k.guideposts == 1 { "" } else { "s" }
            ),
            dim,
        ));
    }
    lines.push(Line::from(""));
    lines.push(section("NEEDS YOU"));
    // A lead waiting on its missions is not waiting on you.
    let items: Vec<&AttentionItem> = app
        .attention
        .iter()
        .filter(|i| i.project == p.id && (i.kind != "waiting" || your_turn(app, i)))
        .collect();
    if items.is_empty() {
        lines.push(Line::styled("  nothing", dim));
    }
    for item in items.iter().take(6) {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {:<9}", kind_label(&item.kind)),
                kind_style(&item.kind),
            ),
            Span::raw(clip(&item.summary, area.width.saturating_sub(15) as usize)),
        ]));
    }
    if items.len() > 6 {
        lines.push(Line::styled(
            format!("  … {} more · i inbox", items.len() - 6),
            dim,
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(next_step(app, p), Style::default().fg(GREEN)));
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(format!(" {} ", p.id))
                .title_bottom(Line::styled(
                    " o mission · t lead · g goal · b brief · e edit · l rules ",
                    dim,
                ))
                .border_style(Style::default().fg(BORDER)),
        ),
        area,
    );
}

/// The one thing to do next on a project, so a new one is never a dead end.
fn next_step(app: &App, p: &Project) -> String {
    let k = app.knowledge_state.get(&p.id).cloned().unwrap_or_default();
    let running = app
        .sessions
        .iter()
        .any(|s| s.project == p.id && s.state == SessionState::Running);
    if !k.wired {
        "next: K sets up the knowledge files every agent reads, then o starts a mission".into()
    } else if !k.brief_written {
        "next: b writes the brief (what it is, what matters now), then o starts a mission".into()
    } else if k.goals.is_empty() {
        "next: g adds a goal (where it is heading) · t talks to its lead · o starts a mission"
            .into()
    } else if running {
        "→ moves into the missions · t talks to the lead · o starts a mission".into()
    } else {
        "next: t asks the lead what to do next, or o starts a mission".into()
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
            "g",
            "add its goals: where it is heading (G edits goals and guideposts)",
        ),
        step(
            "t",
            "talk to its lead: an agent that keeps the overview and starts missions",
        ),
        step(
            "o",
            "or start a mission yourself: an orchestrator plans it and runs agents",
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
    if p.new {
        return draw_new_project(f, area, p);
    }
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
        .border_type(BorderType::Rounded)
        .title(" add this project? ")
        .border_style(Style::default().fg(GREEN));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// The form for a project that does not exist yet.
fn draw_new_project(f: &mut Frame, area: Rect, p: &AddPreview) {
    let dim = Style::default().fg(DIM);
    let label = |s: &str| Span::styled(format!("{s:<11}"), dim);
    let field = |i: usize, text: &str| {
        if p.field == i {
            Span::styled(
                format!("{text}█"),
                Style::default().add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw(text.to_string())
        }
    };
    let lines = vec![
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
                dim,
            ),
        ]),
        Line::from(vec![
            label("folder"),
            Span::raw(tilde(&p.path)),
            Span::styled("  (new)", Style::default().fg(GREEN)),
        ]),
        Line::from(""),
        Line::from(vec![label("what it is"), field(2, &p.about)]),
        Line::styled("           what you are building, for whom", dim),
        Line::from(""),
        Line::styled("Enter creates:", dim),
        Line::styled("· the folder and a git repository", dim),
        Line::styled("· its knowledge files; what you wrote", dim),
        Line::styled("  becomes the brief", dim),
        Line::styled("· its lead, which asks you about it", dim),
        Line::styled("  and proposes the first goals", dim),
    ];
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" start a new project ")
        .border_style(Style::default().fg(GREEN));
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
}

/// The form that starts a mission.
fn draw_mission(f: &mut Frame, area: Rect, app: &App) {
    let Some(m) = &app.mission else {
        return;
    };
    let rect = centered(area, 78, 21);
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
    let serves: Vec<String> = std::iter::once("none".to_string())
        .chain(m.goals.iter().map(|g| g.chars().take(30).collect()))
        .collect();
    let serves: Vec<&str> = serves.iter().map(String::as_str).collect();
    // Only the chosen goal and its neighbours fit on one line.
    let window: Vec<&str> = if serves.len() <= 3 {
        serves.clone()
    } else {
        let at = m.serves.min(serves.len() - 1);
        let start = at.saturating_sub(1).min(serves.len() - 3);
        serves[start..start + 3].to_vec()
    };
    let window_at = window
        .iter()
        .position(|g| *g == serves[m.serves.min(serves.len() - 1)])
        .unwrap_or(0);
    let mut lines = vec![
        Line::from(vec![label(0, "goal"), goal]),
        Line::from(""),
        if m.goals.is_empty() {
            Line::from(vec![
                label(1, "serves"),
                Span::styled(
                    "no goals yet · g adds one to the project",
                    Style::default().fg(DIM),
                ),
            ])
        } else {
            row(1, "serves", &window, window_at)
        },
        row(2, "autonomy", levels, m.level),
    ];
    lines.extend(indented(
        crate::level_meaning(levels[m.level]),
        Style::default().fg(DIM),
    ));
    lines.extend([
        row(3, "orchestrator", &ORCHESTRATOR_MODELS, m.model),
        row(4, "agents", &AGENT_MODELS, m.agent_model),
        row(
            5,
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
                .border_type(BorderType::Rounded)
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
        Mode::Answer => ("ANSWER", GREEN),
        Mode::Deny => ("DENY", RED),
        Mode::Reject => ("REJECT", RED),
        Mode::Rules => ("RULES", ACCENT),
        Mode::EditProject => ("EDIT", GREEN),
        Mode::ConfirmStop => ("CONFIRM", AMBER),
        Mode::Talk => ("TALK", Color::Rgb(188, 140, 255)),
        Mode::AddGoal => ("GOAL", GREEN),
        Mode::Live => ("LIVE", GREEN),
    };
    let pending = if app.mode == Mode::Normal {
        app.pending_decision()
    } else {
        None
    };
    // In the inbox, what just happened shows in the inbox itself: the keys stay here, so you
    // can see what the next item takes.
    let flash = !app.flash.is_empty() && !in_inbox(app);
    let hint = if flash {
        app.flash.clone()
    } else if let Some(item) = pending {
        if item.kind == "trust" {
            "y trust the folder · n don't · j/k move · ? help".to_string()
        } else {
            "y approve · Y always · n deny · Enter type to it · j/k move · ? help".to_string()
        }
    } else {
        match app.mode {
            Mode::Normal => match app.selected_row().map(|r| r.kind) {
                Some(RowKind::Project) => {
                    "j/k project · → its missions · o mission · t lead · g goal · b brief · e edit · i inbox · : more · ? help"
                        .into()
                }
                Some(RowKind::Lead(None)) => {
                    "t talk to the lead (starts it) · Enter start it · j/k move · ← back · ? help".into()
                }
                Some(RowKind::History(_)) => "Enter opens or closes · j/k move · ← back · ? help".into(),
                Some(_) => {
                    "Enter type to it · t one message · f full screen · → its agents · ← back · x stop · c clean up · R resume · ? help"
                        .into()
                }
                None => "a adds a project · ? help · q quit".into(),
            },
            Mode::AddPath => {
                "a project's folder, or a new one to start a project there · Tab completes · Enter next · Esc cancel".into()
            }
            Mode::AddPreview if app.preview.as_ref().is_some_and(|p| p.new) => {
                "Enter create · Tab/↑↓ field · ←/→ pick a workspace · Esc back".into()
            }
            Mode::AddPreview => {
                "Enter add · Tab/↑↓ field · ←/→ pick a workspace · space knowledge on/off · Esc back"
                    .into()
            }
            Mode::EditProject => {
                "type · Tab other field · ←/→ pick a workspace · Enter save (and rescan) · Esc cancel"
                    .into()
            }
            Mode::ConfirmStop => "y stop it · any other key cancels".into(),
            Mode::Live => format!(
                "Ctrl+\\ back to the columns · wheel scrolls back · typing goes to {}",
                app.live_id
                    .as_ref()
                    .and_then(|id| app.sessions.iter().find(|s| s.id == *id))
                    .map(crate::display_name)
                    .unwrap_or_default()
            ),
            Mode::Talk => "say it in your words · Enter sends · Esc cancels".into(),
            Mode::AddGoal => {
                "the goal, in a few words · Enter adds it (active) · G edits goals.md for more · Esc"
                    .into()
            }
            Mode::Mission => {
                "type the goal · Tab/↑↓ field · ←/→ change · Enter start · Esc cancel".into()
            }
            Mode::Palette => "type to filter · ↑↓ select · Enter run · Esc close".into(),
            Mode::ConfirmRemove => "y remove from CORVX · any other key cancels".into(),
            Mode::Help => "any key closes".into(),
            Mode::Inbox => match app.attention.get(app.inbox_sel) {
                Some(item) => inbox_keys(item),
                None => "Esc back to the columns".into(),
            },
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
    ]);
    let mut line = line;
    if !flash {
        line.spans.extend(key_spans(&hint));
    } else {
        line.spans
            .push(Span::styled(hint, Style::default().fg(Color::Gray)));
    }
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
        Mode::Talk => Line::from(vec![
            Span::styled(
                format!("to {} › ", app.talk_label()),
                Style::default().fg(Color::Rgb(188, 140, 255)),
            ),
            Span::raw(format!("{}█", app.input)),
        ]),
        Mode::AddGoal => Line::from(vec![
            Span::styled("new goal › ", Style::default().fg(GREEN)),
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
                .border_type(BorderType::Rounded)
                .title(" commands ")
                .border_style(Style::default().fg(BORDER)),
        ),
        rect,
    );
}

fn draw_help(f: &mut Frame, area: Rect) {
    let rect = centered(area, 78, 29);
    f.render_widget(Clear, rect);
    let rows = [
        ("j / k, ↑ ↓", "move up and down in a column"),
        ("→ / ←", "next column (missions, agents) · back"),
        (
            "Enter",
            "type to the selected session in its pane (Ctrl+\\ returns)",
        ),
        ("f", "attach full screen (Ctrl+\\ then d returns)"),
        ("t", "message the selected session; on a project, its lead"),
        (
            "x c R A",
            "stop · clean up worktrees · resume · autopilot on/off",
        ),
        ("PgUp / PgDn", "scroll an ended mission's journal"),
        ("a", "add a project directory (Tab completes the path)"),
        ("o", "start a mission on the selected project"),
        ("g / G", "add a goal · edit goals and guideposts in $EDITOR"),
        ("b", "edit its brief in $EDITOR (committed when saved)"),
        (
            "e",
            "rename it or move it to another workspace (rescans it)",
        ),
        ("K", "set up its knowledge files, read by every agent"),
        ("d", "on a project's row: remove it (files untouched)"),
        ("r", "refresh from the daemon"),
        (
            ": or Ctrl+K",
            "palette: decisions, conventions, rescan, restart daemon …",
        ),
        ("i", "inbox: what needs you; Enter shows it in the tree"),
        (
            "  in inbox",
            "y approve · Y always · n deny · A autopilot · z snooze",
        ),
        (
            "  proposals",
            "y accept · e edit, then accept · n reject, with why",
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
            "corvx lead · goals · orchestrate · missions · knowledge · daemon",
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
                .border_type(BorderType::Rounded)
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

/// A frame of the working spinner, from the clock.
fn spinner() -> &'static str {
    const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    SPIN[(ms / 100 % 10) as usize]
}

/// A running session that is doing something, not waiting for anyone.
fn at_work(s: &Session) -> bool {
    use corvx_proto::AgentState::*;
    s.state == SessionState::Running && matches!(s.agent_state, Working | Unknown)
}

/// Whether anything under a session (its missions, their agents) is at work.
fn below_at_work(app: &App, id: &str) -> bool {
    let mut todo = vec![id.to_string()];
    let mut seen = std::collections::HashSet::new();
    while let Some(i) = todo.pop() {
        if !seen.insert(i.clone()) {
            continue;
        }
        for s in app
            .sessions
            .iter()
            .filter(|s| s.supervisor.as_deref() == Some(i.as_str()))
        {
            if at_work(s) {
                return true;
            }
            todo.push(s.id.clone());
        }
    }
    false
}

/// A session that waits while the work it started goes on: a lead whose missions run.
fn waits_on_its_work(app: &App, s: &Session) -> bool {
    use corvx_proto::AgentState::*;
    s.state == SessionState::Running
        && matches!(s.agent_state, Waiting | Idle)
        && below_at_work(app, &s.id)
}

/// An attention item that asks something of you: a decision, an answer, a failure. The
/// rest (done, reports, an agent waiting) is yours to look at when you like.
fn needs_you(item: &AttentionItem) -> bool {
    match item.kind.as_str() {
        "approval" => !orchestrators(item),
        "trust" | "blocked" | "question" | "failed" => true,
        _ => false,
    }
}

/// A session waiting for your next instruction, with nothing of its own still at work.
fn your_turn(app: &App, item: &AttentionItem) -> bool {
    item.kind == "waiting" && !below_at_work(app, &item.session_id)
}

fn count_your_turn(app: &App) -> usize {
    app.attention.iter().filter(|i| your_turn(app, i)).count()
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
        .filter(|i| (i.kind == "approval" && !orchestrators(i)) || i.kind == "trust")
        .count()
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "blocked" => "BLOCKED",
        "waiting" => "WAITING",
        "question" => "QUESTION",
        "approval" => "APPROVE?",
        "trust" => "TRUST?",
        "suggestion" => "SUGGEST",
        "proposal" => "LEARN?",
        "done" => "DONE",
        "failed" => "FAILED",
        "report" => "REPORT",
        _ => "?",
    }
}

fn kind_style(kind: &str) -> Style {
    let color = match kind {
        "blocked" => AMBER,
        "waiting" => ACCENT,
        "question" => AMBER,
        "approval" => AMBER,
        "trust" => AMBER,
        "suggestion" => GREEN,
        "proposal" => Color::Rgb(188, 140, 255),
        "done" => GREEN,
        "report" => LEAD,
        "failed" => RED,
        _ => DIM,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

/// The inbox: every open item across projects, most urgent first.
fn draw_inbox(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
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
    // What just happened, where you are looking.
    let block = if app.flash.is_empty() {
        block
    } else {
        block.title_bottom(Line::styled(
            format!(" {} ", app.flash),
            Style::default().fg(Color::Gray),
        ))
    };
    // The selected item in full: what it asks of you, and the keys that answer it.
    let Some(item) = app.attention.get(app.inbox_sel) else {
        f.render_widget(Paragraph::new(lines).block(block), area);
        return;
    };
    if item.kind == "proposal" {
        if let Some(p) = app.proposals.get(&item.id) {
            draw_proposal(f, area, lines, block, p, app);
            return;
        }
    }
    let agent = app
        .sessions
        .iter()
        .find(|s| s.id == item.session_id)
        .map(crate::display_name)
        .unwrap_or_else(|| item.session_id.clone());
    let mut text = Vec::new();
    text.push(Line::styled(
        inbox_asks(item),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    if let Some(note) = item.note.as_deref().filter(|_| item.kind == "approval") {
        text.push(Line::styled(note.to_string(), Style::default().fg(AMBER)));
    }
    text.push(Line::raw(item.summary.clone()));
    text.push(Line::from(""));
    text.push(Line::from(key_spans(&inbox_keys(item))));
    // As tall as what it says, wrapped, and no more than half the inbox.
    let width = area.width.saturating_sub(4).max(20) as usize;
    let rows: usize = text.iter().map(|l| l.width().div_ceil(width).max(1)).sum();
    let height = (rows as u16 + 2).clamp(5, (area.height / 2).max(5));
    let [list_area, detail_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(height)]).areas(area);
    f.render_widget(Paragraph::new(lines).block(block), list_area);
    let detail = Paragraph::new(text)
        .wrap(ratatui::widgets::Wrap { trim: false })
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(format!(" {agent} · {} ", kind_label(&item.kind)))
                .border_style(kind_style(&item.kind)),
        );
    f.render_widget(detail, detail_area);
}

/// Whether the inbox is on screen: browsing it, or typing an answer or message from it.
fn in_inbox(app: &App) -> bool {
    matches!(
        app.mode,
        Mode::Inbox | Mode::Answer | Mode::Deny | Mode::Reject
    ) || (app.mode == Mode::Talk && app.talk_from_inbox)
}

/// What an inbox item asks of you, in plain words.
fn inbox_asks(item: &AttentionItem) -> String {
    match item.kind.as_str() {
        "approval" if orchestrators(item) => {
            "Asks its orchestrator for permission to run this. The orchestrator decides; you may decide it first."
        }
        "approval" => "Asks your permission to run this:",
        "trust" => {
            "Claude Code asks whether to trust this folder. The agent does not start until you answer:"
        }
        "question" => "Asks you, and waits for your answer:",
        "blocked" => {
            "Is stuck at a prompt in its session that CORVX cannot answer from here. Open it and answer there:"
        }
        "waiting" => "Finished its turn and waits for what to do next. Nothing runs until you tell it:",
        "done" => "Reports that it is done. Review it, reply, or acknowledge:",
        "failed" => "Failed. Look at it, tell it what to do, or acknowledge:",
        "report" => "For your information, nothing to decide: a mission the lead started reported.",
        "suggestion" => "CORVX suggests this, because you keep approving it:",
        _ => "",
    }
    .to_string()
}

/// The keys that answer an inbox item, as the status line and the item show them.
fn inbox_keys(item: &AttentionItem) -> String {
    let rule;
    let keys = match item.kind.as_str() {
        "approval" if orchestrators(item) => "y approve · Y always · n deny · Enter show it",
        "approval" if rule_of(item).is_some() => {
            rule = format!(
                "y approve · n deny · L allow {} in {} from now on · A autopilot · Enter show it",
                rule_of(item).unwrap_or_default(),
                item.project
            );
            &rule
        }
        "approval" => "y approve · Y always · n deny · A autopilot · Enter show it",
        "trust" => "y trust the folder · n don't (Claude Code exits) · Enter show it",
        "suggestion" => "y allow it without asking from now on · n keep asking",
        "proposal" => "y accept · e edit, then accept · n reject · z snooze",
        "question" => "r answer · x dismiss (it is told) · z snooze · Enter show it",
        "blocked" => "Enter open it, answer there · t type to it · a ack",
        "waiting" => "t tell it what's next · Enter open it · a ack (it stays waiting)",
        "done" | "failed" => "Enter look at it · t reply · a ack",
        "report" => "a got it · Enter show the mission",
        _ => "Enter show it · a ack · z snooze · x dismiss",
    };
    format!("{keys} · j/k move · Esc")
}

/// What a key that does not apply to an item says.
fn not_here(key: char, item: &AttentionItem) -> String {
    format!(
        "{key} does not apply to {} {} · its keys are below",
        if item.kind == "approval" { "an" } else { "a" },
        kind_label(&item.kind)
    )
}

/// "y approve · n deny": each key stands out from what it does.
fn key_spans(hint: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, part) in hint.split(" · ").enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        let (key, what) = part.split_once(' ').unwrap_or((part, ""));
        spans.push(Span::styled(
            key.to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        if !what.is_empty() {
            spans.push(Span::styled(format!(" {what}"), Style::default().fg(DIM)));
        }
    }
    spans
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
    text.push(Line::from(key_spans(
        "y accept (written to the file and committed) · e edit, then accept · n reject, with why · z snooze",
    )));
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
                .border_type(BorderType::Rounded)
                .title(format!(" {agent} proposes to remember "))
                .border_style(kind_style("proposal")),
        );
    f.render_widget(detail, detail_area);
}

fn age_label(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 48 * 3600 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
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
        match s.agent_state {
            // No hook has reported yet: it is starting up.
            corvx_proto::AgentState::Unknown => "running".to_string(),
            other => format!("{other:?}").to_lowercase(),
        }
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
        .border_type(BorderType::Rounded)
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
                .border_type(BorderType::Rounded)
                .title(format!(" edit {} ", e.id))
                .border_style(Style::default().fg(GREEN)),
        ),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_colours_become_styles() {
        let line = ansi_line(
            "\x1b[1;32mok\x1b[0m plain \x1b[38;5;208mor\x1b[38;2;1;2;3mrgb\x1b[39m",
            80,
        );
        let parts: Vec<(&str, Style)> = line
            .spans
            .iter()
            .map(|s| (s.content.as_ref(), s.style))
            .collect();
        assert_eq!(parts[0].0, "ok");
        assert_eq!(parts[0].1.fg, Some(Color::Green));
        assert!(parts[0].1.add_modifier.contains(Modifier::BOLD));
        assert_eq!(parts[1], (" plain ", Style::default()));
        assert_eq!(parts[2].1.fg, Some(Color::Indexed(208)));
        assert_eq!(parts[3].1.fg, Some(Color::Rgb(1, 2, 3)));
        // Cut to the width, escapes not counted; other sequences dropped.
        assert_eq!(
            strip_escapes("\x1b[2K\x1b[31mabc\x1b]8;;http://x\x07def"),
            "abcdef"
        );
        let cut: String = ansi_line("\x1b[31mabcdef", 3)
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(cut, "abc");
    }

    #[test]
    fn keys_are_sent_as_a_terminal_sends_them() {
        assert_eq!(key_bytes(KeyCode::Char('a'), false, false).unwrap(), "a");
        assert_eq!(key_bytes(KeyCode::Char('c'), true, false).unwrap(), "\x03");
        assert_eq!(key_bytes(KeyCode::Enter, false, false).unwrap(), "\r");
        assert_eq!(key_bytes(KeyCode::Up, false, false).unwrap(), "\x1b[A");
        assert_eq!(key_bytes(KeyCode::Char('b'), false, true).unwrap(), "\x1bb");
        assert!(key_bytes(KeyCode::F(5), false, false).is_none());
    }

    #[test]
    fn a_sessions_model_is_what_it_reports_or_what_it_was_started_with() {
        let cmd = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let started = cmd(&["claude", "--model", "sonnet", "do it"]);
        assert_eq!(short_model(None, &started).as_deref(), Some("sonnet"));
        assert_eq!(
            short_model(None, &cmd(&["claude", "--model=claude-opus-4-5"])).as_deref(),
            Some("opus-4-5")
        );
        assert_eq!(
            short_model(Some("Opus 4.5"), &started).as_deref(),
            Some("opus 4.5"),
            "what it reports wins"
        );
        assert_eq!(short_model(None, &cmd(&["claude"])), None);
        assert_eq!(short_model(None, &cmd(&["claude", "--model"])), None);
    }

    #[test]
    fn palette_filters_by_label() {
        let all = filter_actions("");
        assert_eq!(all.len(), ACTIONS.len());
        let found = filter_actions("proj");
        assert_eq!(found.len(), 5);
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

    #[test]
    fn claude_codes_background_view_is_recognised() {
        // As Claude Code v2.1.323 drew it after Esc in an idle conversation.
        let away = "\
 0 awaiting input · 0 working · 1 completed

Your conversation moved to the background — enter opens it · esc returns to
it · ctrl+c twice quits

Completed
∙ Project state overview  adopted from previous supervisor                 5h
─────────────────────────────────────────────
❯ describe a task for a new session
─────────────────────────────────────────────
  ⏵⏵ auto mode · enter to return · space to reply · ? for shortcuts
";
        assert!(in_background(away));
        // In colour, the words are split by escape codes.
        assert!(in_background(
            "\x1b[1mYour conversation\x1b[0m moved to the \x1b[2mbackground\x1b[0m — enter"
        ));
        assert_eq!(tile_lines(away)[0], "conversation moved to the background");
        // A conversation that only mentions it is not the view.
        let here =
            "⏺ I ran the server in the background.\n❯ \n  ⏵⏵ auto mode on (shift+tab to cycle)\n";
        assert!(!in_background(here));
        assert_eq!(
            tile_lines(here),
            vec!["⏺ I ran the server in the background."]
        );
    }
}
