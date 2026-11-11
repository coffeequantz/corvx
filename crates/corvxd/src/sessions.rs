//! Session manager: runs agents in terminals, keeps their output, tracks what they are
//! doing, and answers `session.*` requests.
//!
//! A session's terminal is owned by a backend:
//! - **tmux** (default when installed): each session is a tmux session on CORVX's own tmux
//!   server. Sessions keep running when the daemon restarts; a new daemon adopts them.
//! - **pty**: the daemon holds the pseudo-terminal itself. Sessions end with the daemon.
//!   Used for tests and where tmux is not installed.
//!
//! Every session writes an append-only transcript. Session records are saved in the store,
//! so a restarted daemon knows which sessions to look for.
//!
//! Agent status comes from runtime hooks when a runtime is known. For Claude Code, the
//! daemon writes a settings file that registers CORVX's hook command and passes it with
//! `--settings`. Hook events arrive as `session.hook` requests.

use std::collections::HashMap;
use std::fmt::Display;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use corvx_core::policy;
use corvx_core::service::{Core, ServiceError};
use corvx_core::store::now_secs;
use corvx_proto::{
    autonomy, codes, methods, rule_actions, AgentState, AttentionAskParams, AttentionDecideParams,
    AttentionItem, PermissionStatus, ProjectJournalParams, Session, SessionAutopilotParams,
    SessionHookParams, SessionIdParams, SessionInputParams, SessionList, SessionOutput,
    SessionOutputParams, SessionPermissionParams, SessionReportParams, SessionResizeParams,
    SessionResumeParams, SessionResumed, SessionScreen, SessionScreenParams, SessionStartParams,
    SessionState,
};
use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

use crate::tmux::Tmux;

/// Output kept in memory per pty session. Older bytes remain in the transcript file.
const SCROLLBACK_BYTES: usize = 256 * 1024;
/// The most output one `session.output` call returns for a tmux session.
const MAX_READ_BYTES: u64 = 1024 * 1024;
const INITIAL_COLS: u16 = 120;
const INITIAL_ROWS: u16 = 40;
/// How deep delegation may go: you → orchestrator → agent → sub-agent.
const MAX_DEPTH: usize = 3;
/// How long Claude Code waits for a decision from the inbox before its permission hook gives
/// up. The dialog in the session is shown all along, so answering there works at any time.
const APPROVAL_HOOK_TIMEOUT_SECS: u64 = 86_400;
/// A waiting permission hook asks for its decision twice a second. One that has not asked for
/// this long is gone, and a decision would reach no one.
const APPROVAL_LISTENER_GRACE: Duration = Duration::from_secs(5);
/// Lost sessions from this far back are listed after a restart, so they can be resumed.
const LOST_SESSIONS_SHOWN_SECS: i64 = 7 * 24 * 3600;

/// Which backend owns a session's terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Pty,
    Tmux,
}

impl BackendKind {
    pub fn name(self) -> &'static str {
        match self {
            BackendKind::Pty => "pty",
            BackendKind::Tmux => "tmux",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name {
            "pty" => Some(BackendKind::Pty),
            "tmux" => Some(BackendKind::Tmux),
            _ => None,
        }
    }
}

struct OutputBuffer {
    data: Vec<u8>,
    /// Absolute offset of `data[0]` in the session's whole output stream.
    start: u64,
}

impl OutputBuffer {
    fn push(&mut self, chunk: &[u8]) {
        self.data.extend_from_slice(chunk);
        if self.data.len() > SCROLLBACK_BYTES {
            let drop = self.data.len() - SCROLLBACK_BYTES;
            self.data.drain(..drop);
            self.start += drop as u64;
        }
    }

    fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }
}

#[derive(Clone)]
struct Status {
    state: SessionState,
    exit_code: Option<i32>,
    ended_at: Option<i64>,
    agent_state: AgentState,
    agent_detail: Option<String>,
    agent_source: Option<String>,
}

/// State shared with a pty session's reader and waiter threads.
struct Shared {
    output: Mutex<OutputBuffer>,
    status: Mutex<Status>,
}

/// How to reach a session's terminal.
enum Backend {
    Pty {
        writer: Box<dyn Write + Send>,
        /// Keeps the session's terminal open, and is used to resize it.
        master: Box<dyn MasterPty + Send>,
        killer: Box<dyn ChildKiller + Send + Sync>,
    },
    /// The tmux session has the same name as the CORVX session.
    Tmux,
}

struct Entry {
    /// Metadata set at start. State fields are read from `shared.status`.
    session: Session,
    shared: Arc<Shared>,
    backend: Backend,
    transcript: PathBuf,
    /// Whether the end of this session has been written to the event log.
    end_logged: bool,
    /// A one-shot run (e.g. `claude -p`): exiting cleanly means the task is done.
    one_shot: bool,
}

impl Entry {
    fn snapshot(&self) -> Session {
        let st = lock(&self.shared.status).clone();
        Session {
            state: st.state,
            exit_code: st.exit_code,
            ended_at: st.ended_at,
            agent_state: st.agent_state,
            agent_detail: st.agent_detail,
            agent_source: st.agent_source,
            ..self.session.clone()
        }
    }

    fn is_running(&self) -> bool {
        lock(&self.shared.status).state == SessionState::Running
    }

    /// Saves the session's current state, so a restarted daemon sees it.
    fn persist(&self, core: &Core) -> Result<(), ServiceError> {
        core.store().save_session(&self.snapshot(), self.one_shot)?;
        Ok(())
    }
}

/// What `recover` found after a daemon restart.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Recovery {
    /// Sessions still running in tmux, now managed by this daemon.
    pub adopted: usize,
    /// Sessions recorded as running that are gone (pty sessions end with their daemon).
    pub lost: usize,
}

pub struct SessionManager {
    data_dir: PathBuf,
    entries: HashMap<String, Entry>,
    /// Session ids in start order.
    order: Vec<String>,
    counter: u64,
    tmux: Option<Tmux>,
    default_backend: BackendKind,
    /// When the permission hook waiting on each approval item last asked for its decision.
    approval_listeners: HashMap<String, Instant>,
    /// Where Claude Code keeps its conversations (`~/.claude/projects`), for resuming.
    claude_projects: Option<PathBuf>,
}

impl SessionManager {
    /// Uses tmux by default when it is installed. `CORVX_BACKEND=pty` makes the native
    /// pty backend the default; tmux sessions are still adopted after a restart.
    pub fn new(data_dir: PathBuf) -> Self {
        let tmux = if Tmux::available() {
            Tmux::new(&data_dir).ok()
        } else {
            None
        };
        let default_backend = match std::env::var("CORVX_BACKEND").ok().as_deref() {
            Some("pty") => BackendKind::Pty,
            _ if tmux.is_some() => BackendKind::Tmux,
            _ => BackendKind::Pty,
        };
        Self::build(data_dir, tmux, default_backend)
    }

    /// A manager whose default backend is fixed, e.g. for tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_backend(data_dir: PathBuf, backend: BackendKind) -> Self {
        let tmux = match backend {
            BackendKind::Tmux => Tmux::new(&data_dir).ok(),
            BackendKind::Pty => None,
        };
        Self::build(data_dir, tmux, backend)
    }

    fn build(data_dir: PathBuf, tmux: Option<Tmux>, default_backend: BackendKind) -> Self {
        Self {
            data_dir,
            entries: HashMap::new(),
            order: Vec::new(),
            counter: 0,
            tmux,
            default_backend,
            approval_listeners: HashMap::new(),
            claude_projects: claude_projects_dir(),
        }
    }

    pub fn default_backend(&self) -> BackendKind {
        self.default_backend
    }

    /// Adopts sessions a previous daemon left running in tmux, and closes the records of
    /// sessions that are gone. Attention items for sessions that are gone are cleared.
    pub fn recover(&mut self, core: &Core) -> Result<Recovery, ServiceError> {
        let mut report = Recovery::default();
        let panes = self.tmux.as_ref().map(Tmux::panes).unwrap_or_default();
        let logs = self.data_dir.join("sessions");
        for (mut session, one_shot) in core.store().running_sessions()? {
            let alive = session.backend == BackendKind::Tmux.name()
                && self.tmux.is_some()
                && panes.contains_key(&session.id);
            if alive {
                let status = Status {
                    state: SessionState::Running,
                    exit_code: None,
                    ended_at: None,
                    agent_state: session.agent_state,
                    agent_detail: session.agent_detail.clone(),
                    agent_source: session.agent_source.clone(),
                };
                let id = session.id.clone();
                self.order.push(id.clone());
                self.entries.insert(
                    id.clone(),
                    Entry {
                        transcript: logs.join(format!("{id}.log")),
                        session,
                        shared: new_shared(status),
                        backend: Backend::Tmux,
                        end_logged: false,
                        one_shot,
                    },
                );
                core.store()
                    .append_event("session.adopted", &json!({"id": id}))?;
                report.adopted += 1;
            } else {
                session.state = SessionState::Lost;
                session.ended_at = Some(now_secs());
                session.agent_state = AgentState::Stopped;
                session.agent_detail = None;
                core.store().save_session(&session, one_shot)?;
                sync_attention(core, &session, AgentState::Stopped, None, None)?;
                core.store().append_event(
                    "session.lost",
                    &json!({"id": session.id, "backend": session.backend}),
                )?;
                report.lost += 1;
            }
        }
        // Recently lost sessions are listed, so they can be resumed.
        for (session, one_shot) in core
            .store()
            .lost_sessions(now_secs() - LOST_SESSIONS_SHOWN_SECS)?
        {
            if self.entries.contains_key(&session.id) {
                continue;
            }
            self.insert_ended(&logs, session, one_shot);
        }
        // Items for sessions this daemon does not run are stale. Questions stay: a resumed
        // session can still take the answer.
        for item in core.store().list_attention(true, now_secs())? {
            let running = self
                .entries
                .get(&item.session_id)
                .is_some_and(Entry::is_running);
            if !running {
                core.store().set_session_attention(
                    &item.session_id,
                    &item.project,
                    "stopped",
                    "",
                    0,
                    now_secs(),
                )?;
            }
        }
        Ok(report)
    }

    /// Lists an ended session without running it.
    fn insert_ended(&mut self, logs: &Path, session: Session, one_shot: bool) {
        let id = session.id.clone();
        let status = Status {
            state: session.state,
            exit_code: session.exit_code,
            ended_at: session.ended_at,
            agent_state: session.agent_state,
            agent_detail: session.agent_detail.clone(),
            agent_source: session.agent_source.clone(),
        };
        self.order.push(id.clone());
        self.entries.insert(
            id.clone(),
            Entry {
                transcript: logs.join(format!("{id}.log")),
                session,
                shared: new_shared(status),
                backend: Backend::Tmux,
                end_logged: true,
                one_shot,
            },
        );
    }

    /// Notices sessions that ended on their own, so their status is current even when no
    /// client asks about them. Called by the daemon on a timer.
    pub fn tick(&mut self, core: &Core) -> Result<(), ServiceError> {
        self.poll_tmux();
        self.record_ends(core)
    }

    /// Marks tmux sessions whose command has exited, or which have disappeared.
    fn poll_tmux(&mut self) {
        let Some(tmux) = self.tmux.as_ref() else {
            return;
        };
        let any_running = self
            .entries
            .values()
            .any(|e| matches!(e.backend, Backend::Tmux) && e.is_running());
        if !any_running {
            return;
        }
        let panes = tmux.panes();
        for (id, entry) in self.entries.iter_mut() {
            if !matches!(entry.backend, Backend::Tmux) || !entry.is_running() {
                continue;
            }
            let (state, code) = match panes.get(id) {
                Some(pane) if !pane.dead => continue,
                Some(pane) => {
                    let code = pane.exit_status;
                    let _ = tmux.kill(id);
                    (SessionState::Exited, code)
                }
                // Gone from outside CORVX, e.g. the machine is shutting down: lost, resumable.
                None => (SessionState::Lost, None),
            };
            let mut st = lock(&entry.shared.status);
            st.state = state;
            st.exit_code = code;
            st.ended_at = Some(now_secs());
        }
    }

    /// Handles one `session.*` request. The caller holds the core lock, so
    /// events are written to the same store as project operations.
    pub fn handle(
        &mut self,
        core: &Core,
        method: &str,
        params: Value,
    ) -> Result<Value, ServiceError> {
        self.record_ends(core)?;
        match method {
            methods::SESSION_START => {
                let p: SessionStartParams = parse(params)?;
                to_value(self.start(core, p)?)
            }
            methods::SESSION_LIST => {
                let sessions = self
                    .order
                    .iter()
                    .filter_map(|id| self.entries.get(id))
                    .map(Entry::snapshot)
                    .collect();
                to_value(SessionList { sessions })
            }
            methods::SESSION_OUTPUT => {
                let p: SessionOutputParams = parse(params)?;
                to_value(self.output(&p.id, p.since)?)
            }
            methods::SESSION_INPUT => {
                let p: SessionInputParams = parse(params)?;
                self.input(&p.id, p.data.as_bytes())?;
                Ok(json!({"ok": true}))
            }
            methods::SESSION_STOP => {
                let p: SessionIdParams = parse(params)?;
                self.stop(core, &p.id)?;
                Ok(json!({"stopped": p.id}))
            }
            methods::SESSION_RESIZE => {
                let p: SessionResizeParams = parse(params)?;
                self.resize(&p)?;
                Ok(json!({"ok": true}))
            }
            methods::SESSION_SCREEN => {
                let p: SessionScreenParams = parse(params)?;
                to_value(self.screen(&p.id)?)
            }
            methods::SESSION_REPORT => {
                let p: SessionReportParams = parse(params)?;
                self.report(core, &p)?;
                Ok(json!({"ok": true}))
            }
            methods::ATTENTION_ASK => {
                let p: AttentionAskParams = parse(params)?;
                let id = self.ask(core, &p)?;
                Ok(json!({"id": id}))
            }
            methods::SESSION_HOOK => {
                let p: SessionHookParams = parse(params)?;
                self.hook(core, &p)
            }
            methods::SESSION_PERMISSION => {
                let p: SessionPermissionParams = parse(params)?;
                to_value(self.permission(core, &p)?)
            }
            methods::ATTENTION_DECIDE => {
                let p: AttentionDecideParams = parse(params)?;
                self.decide(core, &p)?;
                Ok(json!({"ok": true}))
            }
            methods::SESSION_AUTOPILOT => {
                let p: SessionAutopilotParams = parse(params)?;
                to_value(self.autopilot(core, &p)?)
            }
            methods::PROJECT_JOURNAL => {
                let p: ProjectJournalParams = parse(params)?;
                self.journal(core, &p)
            }
            methods::SESSION_RESUME => {
                let p: SessionResumeParams = parse(params)?;
                to_value(self.resume(core, &p.id)?)
            }
            other => Err(ServiceError::Rpc(
                codes::METHOD_NOT_FOUND,
                format!("unknown method: {other}"),
            )),
        }
    }

    fn new_id(&mut self) -> String {
        loop {
            self.counter += 1;
            let id = format!("s{}-{}", now_secs(), self.counter);
            if !self.entries.contains_key(&id) {
                return id;
            }
        }
    }

    fn start(&mut self, core: &Core, p: SessionStartParams) -> Result<Session, ServiceError> {
        let project = core.store().get_project(&p.project)?.ok_or_else(|| {
            ServiceError::Rpc(
                codes::PROJECT_NOT_FOUND,
                format!("no project with id '{}'", p.project),
            )
        })?;
        let backend_kind = match p.backend.as_deref() {
            None => self.default_backend,
            Some(name) => BackendKind::parse(name).ok_or_else(|| {
                ServiceError::Rpc(
                    codes::INVALID_PARAMS,
                    format!("unknown backend '{name}': use tmux or pty"),
                )
            })?,
        };

        if p.autopilot
            && p.supervisor.is_some()
            && !policy::orchestrator_may_decide(&project.autonomy)
        {
            return Err(ServiceError::Rpc(
                codes::NOT_SUPERVISOR,
                format!(
                    "{} is supervised: only the human can turn on autopilot",
                    project.id
                ),
            ));
        }
        if let Some(supervisor) = &p.supervisor {
            let depth = self.depth_of(supervisor)?;
            if depth >= MAX_DEPTH {
                return Err(ServiceError::Rpc(
                    codes::DEPTH_LIMIT,
                    format!("delegation is limited to {MAX_DEPTH} levels; '{supervisor}' is already at level {depth}"),
                ));
            }
        }

        let id = self.new_id();
        let args = self.runtime_args(&id, p.runtime.as_deref(), p.args.clone())?;
        let worktree = if p.worktree {
            Some(
                create_worktree(Path::new(&project.path), &id)
                    .map_err(|e| ServiceError::Rpc(codes::WORKTREE_FAILED, e))?,
            )
        } else {
            None
        };
        let cwd = worktree
            .clone()
            .unwrap_or_else(|| PathBuf::from(&project.path));
        let (backend, pid, shared, transcript_path) = self
            .spawn(
                &id,
                &project.id,
                backend_kind,
                &cwd,
                &p.command,
                args,
                p.label.as_deref(),
            )
            .map_err(|e| {
                ServiceError::Rpc(
                    codes::SESSION_START_FAILED,
                    format!("cannot start '{}': {e}", p.command),
                )
            })?;

        let mut command = vec![p.command.clone()];
        command.extend(p.args.iter().cloned());
        let session = Session {
            id: id.clone(),
            project: project.id.clone(),
            command,
            runtime: p.runtime.clone(),
            backend: backend_kind.name().to_string(),
            supervisor: p.supervisor.clone(),
            worktree: worktree.as_ref().map(|w| w.display().to_string()),
            label: p.label.clone(),
            state: SessionState::Running,
            pid,
            started_at: now_secs(),
            ended_at: None,
            exit_code: None,
            agent_state: AgentState::Unknown,
            agent_detail: None,
            agent_source: None,
            runtime_session: None,
            autopilot: p.autopilot,
            agent_model: p.agent_model.clone(),
            human_approves_agents: p.human_approves_agents,
        };
        let entry = Entry {
            session: session.clone(),
            shared,
            backend,
            transcript: transcript_path,
            end_logged: false,
            one_shot: is_one_shot(p.runtime.as_deref(), &p.args),
        };
        entry.persist(core)?;
        core.store().append_event(
            "session.started",
            &json!({"id": id, "project": project.id, "command": session.command,
                    "runtime": session.runtime, "backend": session.backend}),
        )?;
        self.order.push(id.clone());
        self.entries.insert(id, entry);
        Ok(session)
    }

    /// Arguments for a runtime, with CORVX's integration added. For Claude Code: a settings
    /// file that registers CORVX's hooks, and an MCP config that gives it CORVX's tools.
    fn runtime_args(
        &self,
        id: &str,
        runtime: Option<&str>,
        args: Vec<String>,
    ) -> Result<Vec<String>, ServiceError> {
        if runtime != Some("claude") {
            return Ok(args);
        }
        let logs = self.data_dir.join("sessions");
        create_private_dir(&logs).map_err(internal)?;
        let settings_path = logs.join(format!("{id}.settings.json"));
        let hook_cmd = hook_command(&self.data_dir, id);
        write_private(
            &settings_path,
            claude_settings(&hook_cmd).to_string().as_bytes(),
        )
        .map_err(internal)?;
        let mcp_path = logs.join(format!("{id}.mcp.json"));
        write_private(
            &mcp_path,
            mcp_config(&self.data_dir, id).to_string().as_bytes(),
        )
        .map_err(internal)?;
        // `--mcp-config` takes several values; `--settings` after it ends that list, so the
        // agent's own arguments (such as a first prompt) are not taken as config files.
        let mut with_settings = vec![
            "--mcp-config".to_string(),
            mcp_path.display().to_string(),
            "--settings".to_string(),
            settings_path.display().to_string(),
        ];
        with_settings.extend(args);
        Ok(with_settings)
    }

    /// Starts a session's process in a terminal. Its transcript is appended to, so a resumed
    /// session keeps one transcript.
    #[allow(clippy::too_many_arguments)]
    fn spawn(
        &self,
        id: &str,
        project: &str,
        backend_kind: BackendKind,
        cwd: &Path,
        program: &str,
        args: Vec<String>,
        label: Option<&str>,
    ) -> Result<(Backend, Option<u32>, Arc<Shared>, PathBuf), String> {
        let logs = self.data_dir.join("sessions");
        create_private_dir(&logs).map_err(|e| e.to_string())?;
        let transcript_path = logs.join(format!("{id}.log"));
        // Created owner-only before anything writes to it.
        let transcript = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&transcript_path)
            .map_err(|e| e.to_string())?;
        let env = vec![
            ("CORVX_SESSION_ID".to_string(), id.to_string()),
            ("CORVX_PROJECT".to_string(), project.to_string()),
        ];
        let shared = new_shared(Status {
            state: SessionState::Running,
            exit_code: None,
            ended_at: None,
            agent_state: AgentState::Unknown,
            agent_detail: None,
            agent_source: None,
        });

        let (backend, pid) = match backend_kind {
            BackendKind::Tmux => {
                let tmux = self
                    .tmux
                    .as_ref()
                    .ok_or("tmux is not installed; install it, or use --backend pty")?;
                drop(transcript); // tmux appends to it through pipe-pane
                let mut argv = vec![program.to_string()];
                argv.extend(args);
                let pid = tmux.start(
                    id,
                    cwd,
                    INITIAL_COLS,
                    INITIAL_ROWS,
                    &env,
                    &argv,
                    &transcript_path,
                )?;
                if let Some(label) = label {
                    let _ = tmux.set_label(id, label);
                }
                (Backend::Tmux, pid)
            }
            BackendKind::Pty => {
                let pair = native_pty_system()
                    .openpty(PtySize {
                        rows: INITIAL_ROWS,
                        cols: INITIAL_COLS,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .map_err(|e| e.to_string())?;
                let mut cmd = CommandBuilder::new(program);
                cmd.args(&args);
                cmd.cwd(cwd);
                for (k, v) in &env {
                    cmd.env(k, v);
                }
                // Agents draw their interface from TERM. Without it some CLIs fall back to a plain mode.
                cmd.env("TERM", "xterm-256color");
                // Do not pass on variables that belong to whatever started the daemon. A parent
                // Claude Code session's markers, for example, make the agent think it is a child session.
                for (key, _) in std::env::vars_os() {
                    if let Some(key) = key.to_str() {
                        if is_inherited_agent_var(key) {
                            cmd.env_remove(key);
                        }
                    }
                }
                let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
                drop(pair.slave);
                let pid = child.process_id();
                let killer = child.clone_killer();
                let reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
                let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
                spawn_reader(reader, transcript, Arc::clone(&shared));
                spawn_waiter(child, Arc::clone(&shared));
                (
                    Backend::Pty {
                        writer,
                        master: pair.master,
                        killer,
                    },
                    pid,
                )
            }
        };
        Ok((backend, pid, shared, transcript_path))
    }

    fn output(&self, id: &str, since: u64) -> Result<SessionOutput, ServiceError> {
        let entry = self.entries.get(id).ok_or_else(|| not_found(id))?;
        let state = lock(&entry.shared.status).state;
        match entry.backend {
            Backend::Pty { .. } => {
                let buf = lock(&entry.shared.output);
                let truncated = since < buf.start;
                // Clamp so a bad offset returns nothing instead of panicking.
                let from = since.clamp(buf.start, buf.end());
                let skip = (from - buf.start) as usize;
                Ok(SessionOutput {
                    data: base64::engine::general_purpose::STANDARD.encode(&buf.data[skip..]),
                    next: buf.end(),
                    truncated,
                    state,
                })
            }
            Backend::Tmux => {
                let (bytes, next) = read_from(&entry.transcript, since).map_err(internal)?;
                Ok(SessionOutput {
                    data: base64::engine::general_purpose::STANDARD.encode(bytes),
                    next,
                    truncated: false,
                    state,
                })
            }
        }
    }

    /// How many levels of supervision a session sits at: 1 when you supervise it directly.
    fn depth_of(&self, id: &str) -> Result<usize, ServiceError> {
        let mut depth = 0;
        let mut current = Some(id.to_string());
        while let Some(cur) = current {
            let entry = self.entries.get(&cur).ok_or_else(|| not_found(&cur))?;
            depth += 1;
            if depth > MAX_DEPTH + 1 {
                break; // a cycle would be a bug; stop rather than loop
            }
            current = entry.session.supervisor.clone();
        }
        Ok(depth)
    }

    /// The visible screen of a session as plain text.
    fn screen(&self, id: &str) -> Result<SessionScreen, ServiceError> {
        let entry = self.entries.get(id).ok_or_else(|| not_found(id))?;
        let state = lock(&entry.shared.status).state;
        let live = match &entry.backend {
            Backend::Tmux if state == SessionState::Running => {
                self.tmux.as_ref().and_then(|t| t.capture(id).ok())
            }
            Backend::Pty { .. } => {
                let buf = lock(&entry.shared.output);
                let from = buf.data.len().saturating_sub(16 * 1024);
                Some(strip_ansi(&String::from_utf8_lossy(&buf.data[from..])))
            }
            _ => None,
        };
        // An ended tmux session has no screen; show the end of its transcript instead.
        let text = match live {
            Some(text) => text,
            None => {
                let len = fs::metadata(&entry.transcript)
                    .map(|m| m.len())
                    .unwrap_or(0);
                let (bytes, _) = read_from(&entry.transcript, len.saturating_sub(16 * 1024))
                    .map_err(internal)?;
                strip_ansi(&String::from_utf8_lossy(&bytes))
            }
        };
        Ok(SessionScreen { text, state })
    }

    /// An agent reports the outcome of its task (the `report_done` tool).
    fn report(&mut self, core: &Core, p: &SessionReportParams) -> Result<(), ServiceError> {
        let state = match p.outcome.as_str() {
            "done" => AgentState::Done,
            "failed" => AgentState::Failed,
            other => {
                return Err(ServiceError::Rpc(
                    codes::INVALID_PARAMS,
                    format!("unknown outcome '{other}': use done or failed"),
                ))
            }
        };
        let entry = self.entries.get(&p.id).ok_or_else(|| not_found(&p.id))?;
        {
            let mut st = lock(&entry.shared.status);
            if st.state != SessionState::Running {
                return Err(ServiceError::Rpc(
                    codes::SESSION_NOT_RUNNING,
                    format!("session '{}' is not running", p.id),
                ));
            }
            st.agent_state = state;
            st.agent_detail = Some(p.summary.clone());
            st.agent_source = Some("self-report".into());
        }
        entry.persist(core)?;
        core.store().append_event(
            "session.reported",
            &json!({"id": p.id, "outcome": p.outcome, "summary": p.summary}),
        )?;
        sync_attention(core, &entry.session, state, Some(&p.summary), None)
    }

    /// A session asks a human a question. Returns the question's attention item id.
    fn ask(&self, core: &Core, p: &AttentionAskParams) -> Result<String, ServiceError> {
        let entry = self
            .entries
            .get(&p.session_id)
            .ok_or_else(|| not_found(&p.session_id))?;
        let id = core.store().ask_question(
            &p.session_id,
            &entry.session.project,
            &p.question,
            now_secs(),
        )?;
        core.store().append_event(
            "attention.asked",
            &json!({"id": id, "session": p.session_id, "question": p.question}),
        )?;
        Ok(id)
    }

    fn resize(&self, p: &SessionResizeParams) -> Result<(), ServiceError> {
        let entry = self.entries.get(&p.id).ok_or_else(|| not_found(&p.id))?;
        let (cols, rows) = (p.cols.max(1), p.rows.max(1));
        match &entry.backend {
            Backend::Pty { master, .. } => master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(internal),
            Backend::Tmux => self
                .tmux
                .as_ref()
                .ok_or_else(|| internal("tmux is not available"))?
                .resize(&p.id, cols, rows)
                .map_err(internal),
        }
    }

    fn input(&mut self, id: &str, bytes: &[u8]) -> Result<(), ServiceError> {
        let entry = self.entries.get_mut(id).ok_or_else(|| not_found(id))?;
        let state = lock(&entry.shared.status).state;
        if state != SessionState::Running {
            return Err(ServiceError::Rpc(
                codes::SESSION_NOT_RUNNING,
                format!("session '{id}' is {}", describe(state)),
            ));
        }
        match &mut entry.backend {
            Backend::Pty { writer, .. } => {
                writer.write_all(bytes).map_err(internal)?;
                writer.flush().map_err(internal)
            }
            Backend::Tmux => self
                .tmux
                .as_ref()
                .ok_or_else(|| internal("tmux is not available"))?
                .send_text(id, &String::from_utf8_lossy(bytes))
                .map_err(internal),
        }
    }

    fn stop(&mut self, core: &Core, id: &str) -> Result<(), ServiceError> {
        let entry = self.entries.get_mut(id).ok_or_else(|| not_found(id))?;
        let was_running = {
            let mut st = lock(&entry.shared.status);
            if st.state == SessionState::Running {
                st.state = SessionState::Stopped;
                st.ended_at = Some(now_secs());
                st.agent_state = AgentState::Stopped;
                st.agent_detail = None;
                true
            } else {
                false
            }
        };
        if !was_running {
            return Err(ServiceError::Rpc(
                codes::SESSION_NOT_RUNNING,
                format!("session '{id}' is not running"),
            ));
        }
        // Stopping can fail if the process has just exited; the state is already set,
        // so there is nothing more to do.
        match &mut entry.backend {
            Backend::Pty { killer, .. } => {
                let _ = killer.kill();
            }
            Backend::Tmux => {
                if let Some(tmux) = self.tmux.as_ref() {
                    let _ = tmux.kill(id);
                }
            }
        }
        entry.end_logged = true;
        entry.persist(core)?;
        sync_attention(core, &entry.session, AgentState::Stopped, None, None)?;
        core.store()
            .append_event("session.stopped", &json!({"id": id}))?;
        self.escalate_orphans(core, id)?;
        Ok(())
    }

    /// Applies one runtime hook event to the agent's status. A permission request also opens
    /// an approval item; its id is returned so the waiting hook can ask for the decision.
    fn hook(&mut self, core: &Core, p: &SessionHookParams) -> Result<Value, ServiceError> {
        // Who decides this session's permission requests: its orchestrator, when one is running
        // and has not left its agents' approvals to the human.
        let orchestrator_decides = self
            .entries
            .get(&p.id)
            .and_then(|e| e.session.supervisor.as_ref())
            .and_then(|sup| self.entries.get(sup))
            .is_some_and(|sup| sup.is_running() && !sup.session.human_approves_agents);
        let level = self
            .entries
            .get(&p.id)
            .map(|e| project_level(core, &e.session.project))
            .transpose()?
            .unwrap_or_else(|| autonomy::GUIDED.into());
        let orchestrator_decides = orchestrator_decides && policy::orchestrator_may_decide(&level);
        let entry = self
            .entries
            .get_mut(&p.id)
            .ok_or_else(|| not_found(&p.id))?;
        if lock(&entry.shared.status).state != SessionState::Running {
            // A session that has ended keeps its final state.
            return Ok(json!({"ok": true}));
        }
        // CORVX's own rules: a matching request goes to the human, whoever would decide it.
        let rule_hit = if matches!(p.event.as_str(), "PreToolUse" | "PermissionRequest") {
            rule_hit(core, &entry.session, &p.payload)?
        } else {
            None
        };
        // Every Claude Code hook names its conversation; keep it, to resume the session later.
        let conversation = p.payload.get("session_id").and_then(Value::as_str);
        if let Some(conv) = conversation {
            if entry.session.runtime_session.as_deref() != Some(conv) {
                entry.session.runtime_session = Some(conv.to_string());
                entry.persist(core)?;
            }
        }

        // A rule that allows or denies decides at once; one that asks sends it to the human.
        if let Some(h) = rule_hit.as_ref().filter(|h| h.action != rule_actions::ASK) {
            let summary = describe_request(&p.payload);
            let because = format!("{RULE_NOTE}{}: {}", h.rule, h.reason);
            let allow = h.action == rule_actions::ALLOW;
            core.store().append_event(
                if allow {
                    "attention.auto_approved"
                } else {
                    "attention.auto_denied"
                },
                &json!({"session": p.id, "request": summary, "rule": h.rule, "event": p.event}),
            )?;
            let message = if allow {
                because
            } else {
                format!("{because} · denied by CORVX; do this another way or ask the human")
            };
            return Ok(if p.event == "PermissionRequest" {
                json!({"ok": true, "decision": if allow { "allow" } else { "deny" },
                       "message": message})
            } else {
                json!({"ok": true, "pre": {"decision": h.action, "reason": message}})
            });
        }
        let rule_hit = rule_hit.filter(|h| h.action == rule_actions::ASK);

        if p.event == "PermissionRequest" {
            let summary = describe_request(&p.payload);
            let unasked = policy::approves_unasked(&level);
            if (entry.session.autopilot || unasked) && rule_hit.is_none() {
                core.store().append_event(
                    "attention.auto_approved",
                    &json!({"session": p.id, "request": summary,
                            "by": if entry.session.autopilot { "autopilot".to_string() }
                                  else { format!("autonomy {level}") }}),
                )?;
                return Ok(json!({"ok": true, "decision": "allow"}));
            }
            let orchestrator_decides = orchestrator_decides && rule_hit.is_none();
            let severity = if orchestrator_decides { 1 } else { 3 };
            let note = rule_hit
                .as_ref()
                .map(|h| format!("{RULE_NOTE}{}: {}", h.rule, h.reason));
            let detail = format!("approve? {}", truncate(&summary, 120));
            {
                let mut st = lock(&entry.shared.status);
                st.agent_state = AgentState::Blocked;
                st.agent_detail = Some(detail.clone());
                st.agent_source = Some("hooks".into());
            }
            entry.persist(core)?;
            let item = core.store().open_approval(
                &p.id,
                &entry.session.project,
                &summary,
                severity,
                note.as_deref(),
                now_secs(),
            )?;
            core.store().append_event(
                "session.status",
                &json!({"id": p.id, "agent_state": AgentState::Blocked, "detail": detail}),
            )?;
            core.store().append_event(
                "attention.approval",
                &json!({"id": item, "session": p.id, "request": summary,
                        "decider": if orchestrator_decides { "orchestrator" } else { "human" },
                        "rule": rule_hit.as_ref().map(|h| h.rule.clone())}),
            )?;
            return Ok(json!({"ok": true, "approval": item}));
        }

        // Before a tool runs: a request a rule covers must be asked about even when Claude
        // Code's own settings would allow it. Asking raises a PermissionRequest, which comes
        // back here and goes to the human.
        let reply = match &rule_hit {
            Some(h) => json!({"ok": true, "pre": {"decision": rule_actions::ASK,
                              "reason": format!("{RULE_NOTE}{}: {}", h.rule, h.reason)}}),
            None => json!({"ok": true}),
        };
        let Some((agent_state, detail)) = map_hook(&p.event, &p.payload) else {
            return Ok(reply);
        };
        // Claude Code also sends a notification when a permission dialog has waited a few
        // seconds. The approval item already covers it.
        if agent_state == AgentState::Blocked && core.store().has_open_approval(&p.id)? {
            return Ok(reply);
        }
        let changed = {
            let mut st = lock(&entry.shared.status);
            let changed = st.agent_state != agent_state;
            st.agent_state = agent_state;
            st.agent_detail = detail.clone();
            st.agent_source = Some("hooks".into());
            changed
        };
        if changed {
            entry.persist(core)?;
            core.store().append_event(
                "session.status",
                &json!({"id": p.id, "agent_state": agent_state, "detail": detail}),
            )?;
        }
        if changed || agent_state == AgentState::Blocked || core.store().has_open_approval(&p.id)? {
            sync_attention(core, &entry.session, agent_state, detail.as_deref(), None)?;
        }
        Ok(reply)
    }

    /// Asked by a waiting permission hook: whether its request has been decided.
    fn permission(
        &mut self,
        core: &Core,
        p: &SessionPermissionParams,
    ) -> Result<PermissionStatus, ServiceError> {
        let running = self.entries.get(&p.id).is_some_and(Entry::is_running);
        let state = core.store().approval_state(&p.item)?;
        let status = match state {
            Some((status, decision, message)) if status == "answered" => PermissionStatus {
                status: "decided".into(),
                decision,
                message,
            },
            Some((status, _, _)) if running && (status == "open" || status == "snoozed") => {
                self.approval_listeners
                    .insert(p.item.clone(), Instant::now());
                return Ok(PermissionStatus {
                    status: "pending".into(),
                    decision: None,
                    message: None,
                });
            }
            _ => PermissionStatus {
                status: "gone".into(),
                decision: None,
                message: None,
            },
        };
        self.approval_listeners.remove(&p.item);
        Ok(status)
    }

    /// Approves or denies a permission request from the inbox.
    fn decide(&mut self, core: &Core, p: &AttentionDecideParams) -> Result<(), ServiceError> {
        let valid = match p.decision.as_str() {
            "allow" | "always" | "deny" => true,
            // Only an orchestrator passes a request on; a human simply decides it.
            "escalate" => p.by.is_some(),
            _ => false,
        };
        if !valid {
            return Err(ServiceError::Rpc(
                codes::INVALID_PARAMS,
                format!(
                    "unknown decision '{}': use allow, always or deny{}",
                    p.decision,
                    if p.by.is_some() { ", or escalate" } else { "" }
                ),
            ));
        }
        // A suggestion: allow accepts it, deny declines it. Only the human answers them.
        if let Some(item) = core
            .store()
            .get_attention(&p.id)?
            .filter(|i| i.kind == "suggestion")
        {
            if p.by.is_some() || p.decision == "escalate" {
                return Err(ServiceError::Rpc(
                    codes::NOT_SUPERVISOR,
                    "suggestions are the human's to answer".into(),
                ));
            }
            let accept = p.decision != "deny";
            if !policy::answer_suggestion(core.store(), &item.id, accept, now_secs())? {
                return Err(ServiceError::Rpc(
                    codes::ATTENTION_NOT_FOUND,
                    format!("no open suggestion '{}'", p.id),
                ));
            }
            return Ok(());
        }
        let item = core
            .store()
            .get_attention(&p.id)?
            .filter(|i| i.kind == "approval" && (i.status == "open" || i.status == "snoozed"))
            .ok_or_else(|| {
                ServiceError::Rpc(
                    codes::ATTENTION_NOT_FOUND,
                    format!("no open approval '{}'", p.id),
                )
            })?;
        if let Some(by) = &p.by {
            self.check_supervisor(&item.session_id, by)?;
            // Escalated requests, and the ones a CORVX rule covers, are the human's.
            if item.severity >= 3 {
                return Err(ServiceError::Rpc(
                    codes::NOT_SUPERVISOR,
                    format!(
                        "'{}' is for the human to decide{}",
                        p.id,
                        item.note
                            .as_deref()
                            .map(|n| format!(" ({n})"))
                            .unwrap_or_default()
                    ),
                ));
            }
        }
        if p.decision == "escalate" {
            let reason = p
                .message
                .as_deref()
                .filter(|m| !m.trim().is_empty())
                .unwrap_or("wants you to decide");
            core.store().escalate_approval(
                &p.id,
                &format!("orchestrator: {}", truncate(reason, 300)),
                now_secs(),
            )?;
            core.store().append_event(
                "attention.escalated",
                &json!({"id": p.id, "session": item.session_id, "by": p.by, "reason": reason}),
            )?;
            return Ok(());
        }
        let running = self
            .entries
            .get(&item.session_id)
            .is_some_and(Entry::is_running);
        let listening = self
            .approval_listeners
            .get(&p.id)
            .is_some_and(|t| t.elapsed() < APPROVAL_LISTENER_GRACE);
        if !running || !listening {
            // The hook stopped waiting (it timed out, or the daemon restarted meanwhile). The
            // dialog in the session is the only way left to answer.
            core.store()
                .dismiss_attention(&p.id, "no longer waiting", now_secs())?;
            if running {
                core.store().set_session_attention(
                    &item.session_id,
                    &item.project,
                    "blocked",
                    &format!("{} · answer it in the session", item.summary),
                    3,
                    now_secs(),
                )?;
            }
            return Err(ServiceError::Rpc(
                codes::APPROVAL_GONE,
                "the agent is no longer waiting for this decision; attach to its session to answer"
                    .into(),
            ));
        }
        core.store()
            .decide_approval(&p.id, &p.decision, p.message.as_deref(), now_secs())?;
        core.store().append_event(
            "attention.decided",
            &json!({"id": p.id, "session": item.session_id, "decision": p.decision,
                    "by": p.by.as_deref().unwrap_or("human")}),
        )?;
        // What the human keeps approving, CORVX offers to allow without asking.
        if p.by.is_none() && p.decision != "deny" {
            policy::note_approval(core.store(), &item, now_secs())?;
        }
        Ok(())
    }

    /// Appends an entry to the journal of a session's project:
    /// `<project>/.corvx/journal/<date>-<session>.md`, one file per session.
    fn journal(&self, core: &Core, p: &ProjectJournalParams) -> Result<Value, ServiceError> {
        let entry = self
            .entries
            .get(&p.session)
            .ok_or_else(|| not_found(&p.session))?;
        let project = core
            .store()
            .get_project(&entry.session.project)?
            .ok_or_else(|| {
                ServiceError::Rpc(
                    codes::PROJECT_NOT_FOUND,
                    format!("no project '{}'", entry.session.project),
                )
            })?;
        let dir = Path::new(&project.path).join(".corvx").join("journal");
        fs::create_dir_all(&dir).map_err(|e| {
            ServiceError::Rpc(
                codes::INTERNAL,
                format!("cannot create {}: {e}", dir.display()),
            )
        })?;
        let started = entry.session.started_at;
        let file = dir.join(format!("{}-{}.md", date_of(started), p.session));
        let mut text = String::new();
        if !file.exists() {
            let name = entry
                .session
                .label
                .clone()
                .unwrap_or_else(|| p.session.clone());
            text.push_str(&format!(
                "# {name}\n\nSession {} · started {}\n",
                p.session,
                date_of(started)
            ));
        }
        let title = p.title.as_deref().unwrap_or("Entry");
        text.push_str(&format!(
            "\n## {title} · {}\n\n{}\n",
            time_of(now_secs()),
            p.text.trim()
        ));
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file)
            .map_err(|e| {
                ServiceError::Rpc(
                    codes::INTERNAL,
                    format!("cannot write {}: {e}", file.display()),
                )
            })?;
        f.write_all(text.as_bytes()).map_err(|e| {
            ServiceError::Rpc(
                codes::INTERNAL,
                format!("cannot write {}: {e}", file.display()),
            )
        })?;
        core.store().append_event(
            "project.journal",
            &json!({"session": p.session, "project": project.id, "file": file.display().to_string(),
                    "title": title}),
        )?;
        Ok(json!({"file": file.display().to_string()}))
    }

    /// An orchestrator may only act on the sessions it supervises.
    fn check_supervisor(&self, id: &str, by: &str) -> Result<(), ServiceError> {
        let supervisor = self
            .entries
            .get(id)
            .ok_or_else(|| not_found(id))?
            .session
            .supervisor
            .as_deref();
        if supervisor != Some(by) {
            return Err(ServiceError::Rpc(
                codes::NOT_SUPERVISOR,
                format!("'{by}' does not supervise '{id}'"),
            ));
        }
        Ok(())
    }

    /// Turns a session's autopilot on or off. Turning it on also approves the request it is
    /// waiting on, if any.
    fn autopilot(
        &mut self,
        core: &Core,
        p: &SessionAutopilotParams,
    ) -> Result<Session, ServiceError> {
        if let Some(by) = &p.by {
            self.check_supervisor(&p.id, by)?;
            let project = self
                .entries
                .get(&p.id)
                .map(|e| e.session.project.clone())
                .unwrap_or_default();
            if p.on && !policy::orchestrator_may_decide(&project_level(core, &project)?) {
                return Err(ServiceError::Rpc(
                    codes::NOT_SUPERVISOR,
                    format!("{project} is supervised: only the human can turn on autopilot"),
                ));
            }
        }
        let entry = self
            .entries
            .get_mut(&p.id)
            .ok_or_else(|| not_found(&p.id))?;
        entry.session.autopilot = p.on;
        entry.persist(core)?;
        let snapshot = entry.snapshot();
        core.store().append_event(
            "session.autopilot",
            &json!({"id": p.id, "on": p.on, "by": p.by.as_deref().unwrap_or("human")}),
        )?;
        if p.on {
            // The request waiting now is approved too, unless it is not the asker's to
            // decide: a CORVX rule covers it, or (for an orchestrator) it is the human's.
            let decidable =
                |item: &AttentionItem| !is_rule_hit(item) && (p.by.is_none() || item.severity < 3);
            if let Some(item) = core.store().open_approval_for(&p.id)?.filter(decidable) {
                let listening = self
                    .approval_listeners
                    .get(&item.id)
                    .is_some_and(|t| t.elapsed() < APPROVAL_LISTENER_GRACE);
                if listening {
                    core.store()
                        .decide_approval(&item.id, "allow", None, now_secs())?;
                    core.store().append_event(
                        "attention.auto_approved",
                        &json!({"session": p.id, "request": item.summary}),
                    )?;
                }
            }
        }
        Ok(snapshot)
    }

    /// When an orchestrator ends, the requests its agents are waiting on go to the human.
    fn escalate_orphans(&self, core: &Core, ended: &str) -> Result<(), ServiceError> {
        for entry in self.entries.values() {
            if entry.session.supervisor.as_deref() != Some(ended) || !entry.is_running() {
                continue;
            }
            if let Some(item) = core.store().open_approval_for(&entry.session.id)? {
                if item.severity < 3 {
                    core.store().escalate_approval(
                        &item.id,
                        "its orchestrator has ended",
                        now_secs(),
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Starts an interrupted agent session again, continuing its conversation, together with
    /// the lost agents it supervised. Agents in an orchestration are told what happened.
    fn resume(&mut self, core: &Core, id: &str) -> Result<SessionResumed, ServiceError> {
        let (root, _) = self.ended_record(core, id)?;
        // The lost agents under it, whole tree. Agents stopped on purpose stay stopped.
        let mut agents: Vec<Session> = Vec::new();
        let mut stopped: Vec<Session> = Vec::new();
        let mut queue = vec![root.id.clone()];
        while let Some(parent) = queue.pop() {
            for (child, _) in core.store().supervised_by(&parent)? {
                let live = self.entries.get(&child.id).map(Entry::snapshot);
                let child = live.unwrap_or(child);
                match child.state {
                    SessionState::Lost => {
                        queue.push(child.id.clone());
                        agents.push(child);
                    }
                    SessionState::Running => {}
                    _ => stopped.push(child),
                }
            }
        }

        let mut resumed = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        for agent in &agents {
            let nudge = "[CORVX] This session was interrupted (the machine or CORVX restarted) and \
                         has been resumed. Continue your task where you left off. If you had already \
                         finished, say so in one line."
                .to_string();
            match self.resume_one(core, &agent.id, Some(nudge)) {
                Ok(s) => resumed.push(s),
                Err(e) => {
                    failed.push(format!("{} ({})", agent.id, error_text(&e)));
                    core.store().append_event(
                        "session.resume_failed",
                        &json!({"id": agent.id, "error": error_text(&e)}),
                    )?;
                }
            }
        }
        let in_orchestration =
            root.supervisor.is_some() || !agents.is_empty() || !stopped.is_empty();
        let nudge = in_orchestration.then(|| {
            let mut text = "[CORVX] This session was interrupted (the machine or CORVX restarted) \
                            and has been resumed. Any tool call in progress, such as wait_for, was cut off."
                .to_string();
            let names = |list: &[Session]| {
                list.iter()
                    .map(|s| format!("{} ({})", s.id, s.label.as_deref().unwrap_or("agent")))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            if !resumed.is_empty() {
                text.push_str(&format!(" Resumed with you: {}.", names(&resumed)));
            }
            if !failed.is_empty() {
                text.push_str(&format!(" Could not be resumed: {}.", failed.join(", ")));
            }
            if !stopped.is_empty() {
                text.push_str(&format!(" Already stopped before: {}.", names(&stopped)));
            }
            text.push_str(" Check with list_agents, then continue.");
            text
        });
        let root_session = self.resume_one(core, &root.id, nudge)?;
        let mut sessions = vec![root_session];
        sessions.extend(resumed);
        Ok(SessionResumed { sessions })
    }

    /// The record of an ended session, from this daemon or the store.
    fn ended_record(&self, core: &Core, id: &str) -> Result<(Session, bool), ServiceError> {
        if let Some(entry) = self.entries.get(id) {
            return Ok((entry.snapshot(), entry.one_shot));
        }
        core.store().get_session(id)?.ok_or_else(|| not_found(id))
    }

    /// Resumes one session under its old id, so its supervisor, hooks and tools still match.
    fn resume_one(
        &mut self,
        core: &Core,
        id: &str,
        prompt: Option<String>,
    ) -> Result<Session, ServiceError> {
        let not_resumable = |why: String| ServiceError::Rpc(codes::NOT_RESUMABLE, why);
        let (mut session, one_shot) = self.ended_record(core, id)?;
        if session.state == SessionState::Running {
            return Err(not_resumable(format!("session '{id}' is still running")));
        }
        if session.runtime.as_deref() != Some("claude") || one_shot {
            return Err(not_resumable(format!(
                "session '{id}' is not an interactive Claude Code session; only those can be resumed"
            )));
        }
        let project = core.store().get_project(&session.project)?.ok_or_else(|| {
            not_resumable(format!(
                "its project '{}' is no longer registered",
                session.project
            ))
        })?;
        let cwd = session
            .worktree
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&project.path));
        if !cwd.is_dir() {
            return Err(not_resumable(format!(
                "its directory {} is gone",
                cwd.display()
            )));
        }
        let conversation = session
            .runtime_session
            .clone()
            .or_else(|| {
                let dir = self.claude_projects.as_ref()?;
                find_claude_conversation(dir, &cwd, session.started_at, session.ended_at)
            })
            .ok_or_else(|| {
                not_resumable(format!(
                    "no Claude Code conversation found for session '{id}'"
                ))
            })?;

        let mut args = resume_args(session.command.get(1..).unwrap_or_default());
        args.push("--resume".into());
        args.push(conversation.clone());
        if let Some(prompt) = prompt {
            args.push(prompt);
        }
        let args = self.runtime_args(id, Some("claude"), args)?;
        let backend_kind = match BackendKind::parse(&session.backend) {
            Some(BackendKind::Tmux) if self.tmux.is_some() => BackendKind::Tmux,
            _ => self.default_backend,
        };
        let program = session
            .command
            .first()
            .cloned()
            .unwrap_or_else(|| "claude".into());
        let (backend, pid, shared, transcript) = self
            .spawn(
                id,
                &session.project,
                backend_kind,
                &cwd,
                &program,
                args,
                session.label.as_deref(),
            )
            .map_err(|e| {
                ServiceError::Rpc(
                    codes::SESSION_START_FAILED,
                    format!("cannot resume '{id}': {e}"),
                )
            })?;
        session.state = SessionState::Running;
        session.backend = backend_kind.name().to_string();
        session.pid = pid;
        session.ended_at = None;
        session.exit_code = None;
        session.agent_state = AgentState::Unknown;
        session.agent_detail = None;
        session.agent_source = None;
        session.runtime_session = Some(conversation.clone());
        let entry = Entry {
            session: session.clone(),
            shared,
            backend,
            transcript,
            end_logged: false,
            one_shot,
        };
        entry.persist(core)?;
        core.store().append_event(
            "session.resumed",
            &json!({"id": id, "conversation": conversation}),
        )?;
        if !self.order.iter().any(|o| o == id) {
            self.order.push(id.to_string());
        }
        self.entries.insert(id.to_string(), entry);
        Ok(session)
    }

    /// Records how sessions that ended on their own finished, and sets their agent state.
    fn record_ends(&mut self, core: &Core) -> Result<(), ServiceError> {
        let mut ended = Vec::new();
        for (id, entry) in self.entries.iter_mut() {
            if entry.end_logged {
                continue;
            }
            let st = lock(&entry.shared.status).clone();
            if matches!(st.state, SessionState::Exited | SessionState::Lost) {
                entry.end_logged = true;
                {
                    let mut live = lock(&entry.shared.status);
                    // The process is gone, so a detail from its last turn no longer applies.
                    live.agent_detail = None;
                    live.agent_state = if st.state == SessionState::Lost {
                        AgentState::Stopped
                    } else {
                        final_agent_state(st.exit_code, live.agent_state, entry.one_shot)
                    };
                }
                entry.persist(core)?;
                let final_state = lock(&entry.shared.status).agent_state;
                sync_attention(core, &entry.session, final_state, None, st.exit_code)?;
                let kind = if st.state == SessionState::Lost {
                    "session.lost"
                } else {
                    "session.exited"
                };
                core.store()
                    .append_event(kind, &json!({"id": id, "exit_code": st.exit_code}))?;
                ended.push(id.clone());
            }
        }
        for id in ended {
            self.escalate_orphans(core, &id)?;
        }
        Ok(())
    }
}

fn new_shared(status: Status) -> Arc<Shared> {
    Arc::new(Shared {
        output: Mutex::new(OutputBuffer {
            data: Vec::new(),
            start: 0,
        }),
        status: Mutex::new(status),
    })
}

/// Reads a transcript from byte offset `since`, up to a limit. Returns the bytes and the
/// offset to read from next time.
fn read_from(path: &Path, since: u64) -> std::io::Result<(Vec<u8>, u64)> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let from = since.min(len);
    f.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::new();
    f.take(MAX_READ_BYTES).read_to_end(&mut buf)?;
    let next = from + buf.len() as u64;
    Ok((buf, next))
}

/// Raises, updates or clears the attention item for a session, from its agent state.
fn sync_attention(
    core: &Core,
    session: &Session,
    state: AgentState,
    detail: Option<&str>,
    exit_code: Option<i32>,
) -> Result<(), ServiceError> {
    if session.supervisor.is_some()
        && matches!(
            state,
            AgentState::Waiting | AgentState::Done | AgentState::Failed
        )
    {
        // Its supervisor sees these through `wait_for`. Your inbox only gets what needs a
        // human: permission requests (blocked) and questions.
        core.store().set_session_attention(
            &session.id,
            &session.project,
            "supervised",
            "",
            0,
            now_secs(),
        )?;
        return Ok(());
    }
    let (kind, severity, summary) = match state {
        AgentState::Blocked => (
            "blocked",
            3,
            detail.unwrap_or("needs your attention").to_string(),
        ),
        AgentState::Waiting => (
            "waiting",
            1,
            "waiting for your next instruction".to_string(),
        ),
        AgentState::Done => (
            "done",
            2,
            detail.unwrap_or("finished · ready for review").to_string(),
        ),
        AgentState::Failed => (
            "failed",
            3,
            match exit_code {
                Some(c) => format!("exited with code {c}"),
                None => "failed".to_string(),
            },
        ),
        other => (
            match other {
                AgentState::Working => "working",
                AgentState::Idle => "idle",
                AgentState::Stopped => "stopped",
                _ => "unknown",
            },
            0,
            String::new(),
        ),
    };
    core.store().set_session_attention(
        &session.id,
        &session.project,
        kind,
        &summary,
        severity,
        now_secs(),
    )?;
    Ok(())
}

/// The agent state once its process has exited.
///
/// A clean exit from a one-shot run, or from a plain command, means the task is done.
/// A clean exit while the agent was waiting on the human (blocked, or waiting for its next
/// instruction) means the human quit the session: that is a stop, and nothing is up for review.
fn final_agent_state(exit_code: Option<i32>, before: AgentState, one_shot: bool) -> AgentState {
    match exit_code {
        Some(0) if one_shot => AgentState::Done,
        Some(0) => match before {
            AgentState::Blocked | AgentState::Waiting | AgentState::Idle => AgentState::Stopped,
            _ => AgentState::Done,
        },
        _ => AgentState::Failed,
    }
}

/// Whether a session is a one-shot run rather than an interactive one.
fn is_one_shot(runtime: Option<&str>, args: &[String]) -> bool {
    runtime == Some("claude") && args.iter().any(|a| a == "-p" || a == "--print")
}

/// Environment variables an agent must not inherit from the process that started the daemon.
pub(crate) fn is_inherited_agent_var(key: &str) -> bool {
    key == "CLAUDECODE"
        || key.starts_with("CLAUDE_CODE_")
        || (key.starts_with("CORVX_") && key != "CORVX_HOME")
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents)
}

/// Maps a runtime hook event onto an agent state. Unknown events change nothing.
fn map_hook(event: &str, payload: &Value) -> Option<(AgentState, Option<String>)> {
    match event {
        "UserPromptSubmit" => Some((AgentState::Working, None)),
        "PreToolUse" | "PostToolUse" => {
            let tool = payload.get("tool_name").and_then(Value::as_str);
            Some((AgentState::Working, tool.map(|t| format!("using {t}"))))
        }
        "Notification" => {
            let message = payload
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("needs your attention");
            if is_idle_notification(payload, message) {
                Some((AgentState::Waiting, None))
            } else {
                Some((AgentState::Blocked, Some(message.to_string())))
            }
        }
        // The turn is over. Whether the task is done is for the agent or its supervisor to say.
        "Stop" => Some((AgentState::Waiting, None)),
        _ => None,
    }
}

/// Claude Code sends a notification both for permission requests and when it has been idle,
/// waiting for the next prompt. Only the first needs a decision.
fn is_idle_notification(payload: &Value, message: &str) -> bool {
    match payload.get("notification_type").and_then(Value::as_str) {
        Some(kind) => kind == "idle_prompt",
        None => message.to_lowercase().contains("waiting for your input"),
    }
}

/// A permission request in one line, e.g. "Bash: npm test · Run the tests".
fn describe_request(payload: &Value) -> String {
    let tool = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("a tool");
    let input = payload.get("tool_input").cloned().unwrap_or(Value::Null);
    let field = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_string);
    let main = [
        "command",
        "file_path",
        "notebook_path",
        "url",
        "query",
        "pattern",
        "path",
    ]
    .iter()
    .find_map(|k| field(k))
    .unwrap_or_else(|| match &input {
        Value::Null => String::new(),
        other => other.to_string(),
    });
    let mut text = if main.is_empty() {
        tool.to_string()
    } else {
        format!("{tool}: {main}")
    };
    if let Some(desc) = field("description") {
        text.push_str(" · ");
        text.push_str(&desc);
    }
    truncate(&text.replace('\n', " ⏎ "), 400)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn error_text(e: &ServiceError) -> String {
    e.to_string()
}

/// The original arguments of a Claude Code session without its first prompt, to resume it with
/// the same model, permission mode and system prompt. CORVX's own config flags are left out;
/// they are added again.
fn resume_args(args: &[String]) -> Vec<String> {
    const WITH_VALUE: &[&str] = &[
        "--model",
        "--permission-mode",
        "--append-system-prompt",
        "--system-prompt",
        "--add-dir",
        "--allowedTools",
        "--allowed-tools",
        "--disallowedTools",
        "--disallowed-tools",
        "--agent",
        "--effort",
        "--fallback-model",
        "--name",
        "-n",
    ];
    const CORVX_OWN: &[&str] = &["--mcp-config", "--settings"];
    const DROP_FLAGS: &[&str] = &["--resume", "-r", "--continue", "-c", "--session-id"];
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let flag = arg.split('=').next().unwrap_or(arg);
        let has_inline = arg.contains('=');
        if CORVX_OWN.contains(&flag) || DROP_FLAGS.contains(&flag) {
            // Skip the flag and its value. `--resume`/`--session-id` take one; `-c` none.
            let takes_value = !has_inline && !matches!(flag, "--continue" | "-c");
            i += if takes_value { 2 } else { 1 };
            continue;
        }
        if WITH_VALUE.contains(&flag) {
            out.push(arg.clone());
            if !has_inline {
                if let Some(v) = args.get(i + 1) {
                    out.push(v.clone());
                }
                i += 1;
            }
        } else if arg.starts_with('-') {
            out.push(arg.clone()); // a switch, e.g. --verbose
        }
        // Anything else is the first prompt: the conversation already has it.
        i += 1;
    }
    out
}

/// Where Claude Code keeps its conversations.
fn claude_projects_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".claude"),
    };
    Some(base.join("projects"))
}

/// Finds the conversation a Claude Code session started in `cwd` at `started_at`, for sessions
/// recorded before CORVX kept the conversation id: the first conversation file created there
/// after the session started (and before it ended).
fn find_claude_conversation(
    projects: &Path,
    cwd: &Path,
    started_at: i64,
    ended_at: Option<i64>,
) -> Option<String> {
    let slug: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let latest = ended_at.unwrap_or(i64::MAX).min(started_at + 3600);
    let mut best: Option<(i64, String)> = None;
    for entry in fs::read_dir(projects.join(slug)).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Some(created) = meta
            .created()
            .or_else(|_| meta.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
        else {
            continue;
        };
        if created < started_at - 5 || created > latest {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !best.as_ref().is_some_and(|(t, _)| *t <= created) {
            best = Some((created, stem.to_string()));
        }
    }
    best.map(|(_, id)| id)
}

/// Whether an approval was sent to the human by a CORVX rule.
fn is_rule_hit(item: &AttentionItem) -> bool {
    item.note
        .as_deref()
        .is_some_and(|n| n.starts_with(RULE_NOTE))
}

/// How an approval's note starts when a CORVX rule sent it to the human.
const RULE_NOTE: &str = "CORVX rule ";

/// A project's autonomy level (guided if the project is gone).
fn project_level(core: &Core, project: &str) -> Result<String, ServiceError> {
    Ok(core
        .store()
        .get_project(project)?
        .map(|p| p.autonomy)
        .unwrap_or_else(|| autonomy::GUIDED.into()))
}

/// The UTC date of a unix time, as YYYY-MM-DD.
fn date_of(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// The UTC date and time of a unix time, as YYYY-MM-DD HH:MM UTC.
fn time_of(secs: i64) -> String {
    let rem = secs.rem_euclid(86_400);
    format!(
        "{} {:02}:{:02} UTC",
        date_of(secs),
        rem / 3600,
        rem % 3600 / 60
    )
}

/// Days since 1970-01-01 to a (year, month, day) date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// The CORVX rule a tool call matches, if any. Rules apply to the session's project; the
/// working directory is its worktree, or the project itself.
fn rule_hit(
    core: &Core,
    session: &Session,
    payload: &Value,
) -> Result<Option<policy::Hit>, ServiceError> {
    let Some(tool) = payload.get("tool_name").and_then(Value::as_str) else {
        return Ok(None);
    };
    let input = payload.get("tool_input").cloned().unwrap_or(Value::Null);
    let workdir = match &session.worktree {
        Some(w) => PathBuf::from(w),
        None => match core.store().get_project(&session.project)? {
            Some(p) => PathBuf::from(p.path),
            None => return Ok(None),
        },
    };
    let rules = policy::rules_for(core.store(), Some(&session.project))?;
    Ok(policy::check(&rules, tool, &input, &workdir))
}

/// Settings for Claude Code that register CORVX's hook command for the events we use.
fn claude_settings(hook_cmd: &str) -> Value {
    // A short timeout: if the daemon is slow or gone, the agent must not stall on our hook.
    let entry = json!([{"hooks": [{"type": "command", "command": hook_cmd, "timeout": 5}]}]);
    // Waits for your decision from the inbox. The dialog shows in the session meanwhile, and
    // whichever answer comes first applies.
    let permission = json!([{"hooks": [{"type": "command", "command": hook_cmd,
                                         "timeout": APPROVAL_HOOK_TIMEOUT_SECS}]}]);
    json!({
        "permissions": {"allow": ["mcp__corvx"]},
        "hooks": {
            "PermissionRequest": permission,
            "UserPromptSubmit": entry,
            "PreToolUse": entry,
            "PostToolUse": entry,
            "Notification": entry,
            "Stop": entry,
        }
    })
}

/// MCP config that gives a Claude Code session CORVX's tools (`corvx mcp`).
fn mcp_config(data_dir: &Path, session_id: &str) -> Value {
    json!({
        "mcpServers": {
            "corvx": {
                "type": "stdio",
                "command": corvx_bin().display().to_string(),
                "args": ["--data-dir", data_dir.display().to_string(), "mcp"],
                "env": {"CORVX_SESSION_ID": session_id}
            }
        }
    })
}

/// Creates `<repo>/.corvx/worktrees/<id>` on a new branch `corvx/<id>`, and keeps the
/// worktrees folder out of `git status`.
fn create_worktree(repo: &Path, id: &str) -> Result<PathBuf, String> {
    let git = |dir: &Path, args: &[&str]| -> Result<String, String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map_err(|e| format!("cannot run git: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    };
    let top = git(repo, &["rev-parse", "--show-toplevel"])
        .map_err(|_| format!("{} is not a git repository", repo.display()))?;
    let top = PathBuf::from(top);
    git(&top, &["rev-parse", "--verify", "HEAD"])
        .map_err(|_| "the repository has no commits yet; make a first commit".to_string())?;
    let path = top.join(".corvx").join("worktrees").join(id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let branch = format!("corvx/{id}");
    git(
        &top,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            &path.to_string_lossy(),
            "HEAD",
        ],
    )?;
    if let Ok(common) = git(&top, &["rev-parse", "--git-common-dir"]) {
        let common = if Path::new(&common).is_absolute() {
            PathBuf::from(common)
        } else {
            top.join(common)
        };
        let exclude = common.join("info").join("exclude");
        let current = fs::read_to_string(&exclude).unwrap_or_default();
        if !current.lines().any(|l| l.trim() == ".corvx/worktrees/") {
            let sep = if current.is_empty() || current.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            if let Some(parent) = exclude.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(&exclude, format!("{current}{sep}.corvx/worktrees/\n"));
        }
    }
    Ok(path)
}

/// Removes terminal escape sequences, leaving readable text.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // CSI: parameters, then one final byte in @..~
                Some('[') => {
                    for n in chars.by_ref() {
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC: until BEL or ESC \
                Some(']') => {
                    while let Some(n) = chars.next() {
                        if n == '\u{7}' {
                            break;
                        }
                        if n == '\u{1b}' {
                            chars.next();
                            break;
                        }
                    }
                }
                // Two-character sequences, e.g. ESC ( B
                Some('(') | Some(')') => {
                    chars.next();
                }
                _ => {}
            },
            '\r' => {}
            c if (c as u32) < 0x20 && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
    }
    out
}

/// The command Claude Code runs for each hook. It forwards the event to the daemon.
fn hook_command(data_dir: &Path, session_id: &str) -> String {
    format!(
        "{} --data-dir {} session hook {}",
        shell_quote(&corvx_bin().display().to_string()),
        shell_quote(&data_dir.display().to_string()),
        session_id
    )
}

/// The `corvx` CLI: next to this daemon if present, otherwise from PATH.
fn corvx_bin() -> PathBuf {
    let name = if cfg!(windows) { "corvx.exe" } else { "corvx" };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(name);
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from(name)
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn spawn_reader(mut reader: Box<dyn Read + Send>, mut transcript: fs::File, shared: Arc<Shared>) {
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = &buf[..n];
                    let _ = transcript.write_all(chunk);
                    lock(&shared.output).push(chunk);
                }
            }
        }
    });
}

fn spawn_waiter(mut child: Box<dyn Child + Send + Sync>, shared: Arc<Shared>) {
    thread::spawn(move || {
        let code = child.wait().ok().map(|s| s.exit_code() as i32);
        let mut st = lock(&shared.status);
        // A stop request has already set the state; keep it.
        if st.state == SessionState::Running {
            st.state = SessionState::Exited;
        }
        st.exit_code = code;
        st.ended_at = Some(now_secs());
    });
}

fn describe(state: SessionState) -> &'static str {
    match state {
        SessionState::Running => "running",
        SessionState::Exited => "exited",
        SessionState::Stopped => "stopped",
        SessionState::Lost => "lost",
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn not_found(id: &str) -> ServiceError {
    ServiceError::Rpc(codes::SESSION_NOT_FOUND, format!("no session '{id}'"))
}

fn internal(e: impl Display) -> ServiceError {
    ServiceError::Rpc(codes::INTERNAL, e.to_string())
}

fn parse<T: DeserializeOwned>(v: Value) -> Result<T, ServiceError> {
    serde_json::from_value(v)
        .map_err(|e| ServiceError::Rpc(codes::INVALID_PARAMS, format!("invalid params: {e}")))
}

fn to_value<T: Serialize>(v: T) -> Result<Value, ServiceError> {
    serde_json::to_value(v).map_err(internal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use corvx_core::store::Store;
    use corvx_proto::{methods, ProjectList};
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    /// Each session holds a pseudo-terminal, and macOS has a small supply of them. Session
    /// tests take this lock so they run one at a time rather than exhausting the supply.
    static PTY_SERIAL: Mutex<()> = Mutex::new(());

    struct Fixture {
        _serial: MutexGuard<'static, ()>,
        core: Core,
        mgr: SessionManager,
        data: TempDir,
        project_id: String,
        _project_dir: TempDir,
    }

    impl Drop for Fixture {
        /// Each test's tmux server lives in its own data directory; shut it down.
        fn drop(&mut self) {
            let _ = std::process::Command::new("tmux")
                .arg("-S")
                .arg(crate::tmux::socket_path(self.data.path()))
                .arg("kill-server")
                .output();
        }
    }

    fn fixture() -> Fixture {
        fixture_with(BackendKind::Pty)
    }

    /// A fixture on the tmux backend, or None when tmux is not installed.
    fn tmux_fixture() -> Option<Fixture> {
        if Tmux::available() {
            Some(fixture_with(BackendKind::Tmux))
        } else {
            eprintln!("tmux is not installed: skipping");
            None
        }
    }

    fn fixture_with(backend: BackendKind) -> Fixture {
        let serial = PTY_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let data = TempDir::new().unwrap();
        let project_dir = TempDir::new().unwrap();
        let core = Core::new(Store::open_in_memory().unwrap(), data.path().to_path_buf());
        let added = core
            .handle(
                methods::PROJECT_ADD,
                json!({"path": project_dir.path(), "name": "p"}),
            )
            .unwrap();
        let project_id = added["id"].as_str().unwrap().to_string();
        let mgr = SessionManager::with_backend(data.path().to_path_buf(), backend);
        Fixture {
            core,
            mgr,
            data,
            project_id,
            _project_dir: project_dir,
            _serial: serial,
        }
    }

    fn start(f: &mut Fixture, command: &str, args: &[&str]) -> String {
        let v = f
            .mgr
            .handle(
                &f.core,
                methods::SESSION_START,
                json!({"project": f.project_id, "command": command, "args": args}),
            )
            .unwrap();
        v["id"].as_str().unwrap().to_string()
    }

    fn output_text(f: &mut Fixture, id: &str) -> (String, SessionState) {
        let v = f
            .mgr
            .handle(
                &f.core,
                methods::SESSION_OUTPUT,
                json!({"id": id, "since": 0}),
            )
            .unwrap();
        let out: SessionOutput = serde_json::from_value(v).unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(out.data)
            .unwrap();
        (String::from_utf8_lossy(&bytes).into_owned(), out.state)
    }

    fn session_state(f: &mut Fixture, id: &str) -> Session {
        let v = f
            .mgr
            .handle(&f.core, methods::SESSION_LIST, json!({}))
            .unwrap();
        let list: SessionList = serde_json::from_value(v).unwrap();
        list.sessions.into_iter().find(|s| s.id == id).unwrap()
    }

    fn send_hook(f: &mut Fixture, id: &str, event: &str, payload: Value) {
        f.mgr
            .handle(
                &f.core,
                methods::SESSION_HOOK,
                json!({"id": id, "event": event, "payload": payload}),
            )
            .unwrap();
    }

    /// Polls until `done` holds or the timeout passes.
    fn wait_until(mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting");
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn captures_output_and_exit_code() {
        let mut f = fixture();
        let id = start(&mut f, "sh", &["-c", "echo hello-corvx; exit 3"]);
        wait_until(|| {
            let (text, state) = output_text(&mut f, &id);
            text.contains("hello-corvx") && state == SessionState::Exited
        });
        let s = session_state(&mut f, &id);
        assert_eq!(s.state, SessionState::Exited);
        assert_eq!(s.exit_code, Some(3));
        assert!(s.ended_at.is_some());
    }

    #[test]
    fn exit_is_written_to_the_event_log() {
        let mut f = fixture();
        let id = start(&mut f, "sh", &["-c", "exit 0"]);
        wait_until(|| session_state(&mut f, &id).state == SessionState::Exited);
        session_state(&mut f, &id);
        let kinds: Vec<String> = f
            .core
            .store()
            .recent_events(10)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(kinds.contains(&"session.started".to_string()));
        assert!(kinds.contains(&"session.exited".to_string()));
    }

    #[test]
    fn transcript_file_keeps_the_output() {
        let mut f = fixture();
        let id = start(&mut f, "sh", &["-c", "echo transcript-line"]);
        wait_until(|| output_text(&mut f, &id).0.contains("transcript-line"));
        let log =
            fs::read_to_string(f.data.path().join("sessions").join(format!("{id}.log"))).unwrap();
        assert!(log.contains("transcript-line"));
    }

    #[test]
    fn input_reaches_the_process() {
        let mut f = fixture();
        let id = start(&mut f, "cat", &[]);
        f.mgr
            .handle(
                &f.core,
                methods::SESSION_INPUT,
                json!({"id": id, "data": "ping-123\n"}),
            )
            .unwrap();
        wait_until(|| output_text(&mut f, &id).0.contains("ping-123"));
        assert_eq!(session_state(&mut f, &id).state, SessionState::Running);
    }

    #[test]
    fn stop_ends_a_running_session() {
        let mut f = fixture();
        let id = start(&mut f, "sleep", &["30"]);
        f.mgr
            .handle(&f.core, methods::SESSION_STOP, json!({"id": id}))
            .unwrap();
        let s = session_state(&mut f, &id);
        assert_eq!(s.state, SessionState::Stopped);
        assert_eq!(s.agent_state, AgentState::Stopped);

        let err = f
            .mgr
            .handle(&f.core, methods::SESSION_STOP, json!({"id": id}))
            .unwrap_err();
        assert!(matches!(err, ServiceError::Rpc(c, _) if c == codes::SESSION_NOT_RUNNING));
        let err = f
            .mgr
            .handle(
                &f.core,
                methods::SESSION_INPUT,
                json!({"id": id, "data": "x"}),
            )
            .unwrap_err();
        assert!(matches!(err, ServiceError::Rpc(c, _) if c == codes::SESSION_NOT_RUNNING));
    }

    #[test]
    fn output_offsets_resume_where_they_left_off() {
        let mut f = fixture();
        let id = start(&mut f, "sh", &["-c", "echo first; exit 0"]);
        wait_until(|| session_state(&mut f, &id).state == SessionState::Exited);
        let v = f
            .mgr
            .handle(
                &f.core,
                methods::SESSION_OUTPUT,
                json!({"id": id, "since": 0}),
            )
            .unwrap();
        let first: SessionOutput = serde_json::from_value(v).unwrap();
        let v = f
            .mgr
            .handle(
                &f.core,
                methods::SESSION_OUTPUT,
                json!({"id": id, "since": first.next}),
            )
            .unwrap();
        let again: SessionOutput = serde_json::from_value(v).unwrap();
        assert!(again.data.is_empty());
        assert!(!again.truncated);
    }

    #[test]
    fn unknown_project_and_session_are_reported() {
        let mut f = fixture();
        let err = f
            .mgr
            .handle(
                &f.core,
                methods::SESSION_START,
                json!({"project": "nope", "command": "sh"}),
            )
            .unwrap_err();
        assert!(matches!(err, ServiceError::Rpc(c, _) if c == codes::PROJECT_NOT_FOUND));

        let err = f
            .mgr
            .handle(&f.core, methods::SESSION_OUTPUT, json!({"id": "ghost"}))
            .unwrap_err();
        assert!(matches!(err, ServiceError::Rpc(c, _) if c == codes::SESSION_NOT_FOUND));
    }

    #[test]
    fn missing_command_fails_to_start() {
        let mut f = fixture();
        let err = f
            .mgr
            .handle(
                &f.core,
                methods::SESSION_START,
                json!({"project": f.project_id, "command": "/no/such/binary"}),
            )
            .unwrap_err();
        assert!(matches!(err, ServiceError::Rpc(c, _) if c == codes::SESSION_START_FAILED));
    }

    #[test]
    fn project_list_still_works_alongside_sessions() {
        let mut f = fixture();
        start(&mut f, "sh", &["-c", "exit 0"]);
        let v = f.core.handle(methods::PROJECT_LIST, json!({})).unwrap();
        let list: ProjectList = serde_json::from_value(v).unwrap();
        assert_eq!(list.projects.len(), 1);
    }

    #[test]
    fn hooks_drive_agent_state() {
        let mut f = fixture();
        let id = start(&mut f, "sleep", &["30"]);
        assert_eq!(session_state(&mut f, &id).agent_state, AgentState::Unknown);

        send_hook(&mut f, &id, "UserPromptSubmit", json!({}));
        assert_eq!(session_state(&mut f, &id).agent_state, AgentState::Working);

        send_hook(&mut f, &id, "PreToolUse", json!({"tool_name": "Bash"}));
        let s = session_state(&mut f, &id);
        assert_eq!(s.agent_state, AgentState::Working);
        assert_eq!(s.agent_detail.as_deref(), Some("using Bash"));
        assert_eq!(s.agent_source.as_deref(), Some("hooks"));

        send_hook(
            &mut f,
            &id,
            "Notification",
            json!({"message": "Claude needs your permission to use Bash"}),
        );
        let s = session_state(&mut f, &id);
        assert_eq!(s.agent_state, AgentState::Blocked);
        assert_eq!(
            s.agent_detail.as_deref(),
            Some("Claude needs your permission to use Bash")
        );

        send_hook(&mut f, &id, "Stop", json!({}));
        assert_eq!(session_state(&mut f, &id).agent_state, AgentState::Waiting);

        // Unknown events change nothing.
        send_hook(&mut f, &id, "SomethingNew", json!({}));
        assert_eq!(session_state(&mut f, &id).agent_state, AgentState::Waiting);

        f.mgr
            .handle(&f.core, methods::SESSION_STOP, json!({"id": id}))
            .unwrap();
    }

    #[test]
    fn hook_for_unknown_session_is_an_error() {
        let mut f = fixture();
        let err = f
            .mgr
            .handle(
                &f.core,
                methods::SESSION_HOOK,
                json!({"id": "ghost", "event": "Stop"}),
            )
            .unwrap_err();
        assert!(matches!(err, ServiceError::Rpc(c, _) if c == codes::SESSION_NOT_FOUND));
    }

    #[test]
    fn nonzero_exit_marks_agent_failed() {
        let mut f = fixture();
        let id = start(&mut f, "sh", &["-c", "exit 2"]);
        wait_until(|| session_state(&mut f, &id).state == SessionState::Exited);
        assert_eq!(session_state(&mut f, &id).agent_state, AgentState::Failed);
    }

    #[test]
    fn claude_sessions_get_a_settings_file_that_registers_hooks() {
        let mut f = fixture();
        let v = f
            .mgr
            .handle(
                &f.core,
                methods::SESSION_START,
                json!({"project": f.project_id, "command": "sh", "args": ["-c", "sleep 30"], "runtime": "claude"}),
            )
            .unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["runtime"], "claude");

        let settings_path = f
            .data
            .path()
            .join("sessions")
            .join(format!("{id}.settings.json"));
        let settings: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        for event in [
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "Notification",
            "Stop",
        ] {
            let cmd = settings["hooks"][event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap();
            assert!(cmd.contains("session hook"), "{event}: {cmd}");
            assert!(cmd.contains(&id), "{event}: {cmd}");
        }
        // The command is `sh --settings ...`, which exits at once; this test covers the settings file only.
    }

    #[test]
    fn map_hook_covers_the_status_model() {
        assert_eq!(
            map_hook("UserPromptSubmit", &json!({})).unwrap().0,
            AgentState::Working
        );
        assert_eq!(map_hook("Stop", &json!({})).unwrap().0, AgentState::Waiting);
        let (state, detail) = map_hook("Notification", &json!({})).unwrap();
        assert_eq!(state, AgentState::Blocked);
        assert_eq!(detail.as_deref(), Some("needs your attention"));
        assert!(map_hook("SessionEnd", &json!({})).is_none());
    }

    #[test]
    fn shell_quote_survives_spaces_and_quotes() {
        assert_eq!(shell_quote("/a b/c"), "'/a b/c'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    fn open_kinds(f: &Fixture, session: &str) -> Vec<String> {
        f.core
            .store()
            .list_attention(false, now_secs())
            .unwrap()
            .into_iter()
            .filter(|i| i.session_id == session)
            .map(|i| i.kind)
            .collect()
    }

    #[test]
    fn agent_states_raise_and_clear_attention() {
        let mut f = fixture();
        let id = start(&mut f, "sleep", &["30"]);

        send_hook(
            &mut f,
            &id,
            "Notification",
            json!({"message": "needs permission"}),
        );
        assert_eq!(open_kinds(&f, &id), vec!["blocked"]);

        send_hook(&mut f, &id, "PreToolUse", json!({"tool_name": "Bash"}));
        assert!(
            open_kinds(&f, &id).is_empty(),
            "working clears the blocked item"
        );

        send_hook(&mut f, &id, "Stop", json!({}));
        assert_eq!(open_kinds(&f, &id), vec!["waiting"]);

        f.mgr
            .handle(&f.core, methods::SESSION_STOP, json!({"id": id}))
            .unwrap();
        assert!(open_kinds(&f, &id).is_empty(), "stopping clears it");
    }

    #[test]
    fn blocked_item_carries_the_permission_message() {
        let mut f = fixture();
        let id = start(&mut f, "sleep", &["30"]);
        send_hook(
            &mut f,
            &id,
            "Notification",
            json!({"message": "Allow rm -rf dist/?"}),
        );
        let item = f
            .core
            .store()
            .list_attention(false, now_secs())
            .unwrap()
            .remove(0);
        assert_eq!(item.summary, "Allow rm -rf dist/?");
        assert_eq!(item.severity, 3);
        f.mgr
            .handle(&f.core, methods::SESSION_STOP, json!({"id": id}))
            .unwrap();
    }

    #[test]
    fn failed_exit_raises_a_failed_item_and_success_a_done_one() {
        let mut f = fixture();
        let bad = start(&mut f, "sh", &["-c", "exit 4"]);
        let good = start(&mut f, "sh", &["-c", "exit 0"]);
        wait_until(|| {
            session_state(&mut f, &bad).state == SessionState::Exited
                && session_state(&mut f, &good).state == SessionState::Exited
        });
        // Any later request records the ends.
        session_state(&mut f, &bad);
        assert_eq!(open_kinds(&f, &bad), vec!["failed"]);
        assert_eq!(open_kinds(&f, &good), vec!["done"]);
        let failed = f
            .core
            .store()
            .list_attention(false, now_secs())
            .unwrap()
            .into_iter()
            .find(|i| i.session_id == bad)
            .unwrap();
        assert_eq!(failed.summary, "exited with code 4");
    }

    #[test]
    fn quitting_while_blocked_is_a_stop_not_a_finish() {
        let mut f = fixture();
        let id = start(&mut f, "sh", &["-c", "sleep 0.3; exit 0"]);
        send_hook(
            &mut f,
            &id,
            "Notification",
            json!({"message": "needs permission"}),
        );
        wait_until(|| session_state(&mut f, &id).state == SessionState::Exited);
        assert_eq!(session_state(&mut f, &id).agent_state, AgentState::Stopped);
        assert!(open_kinds(&f, &id).is_empty(), "no done item for a quit");
    }

    #[test]
    fn ending_a_session_clears_the_agent_detail() {
        let mut f = fixture();
        let id = start(&mut f, "sh", &["-c", "sleep 0.3; exit 0"]);
        send_hook(&mut f, &id, "PreToolUse", json!({"tool_name": "Bash"}));
        assert_eq!(
            session_state(&mut f, &id).agent_detail.as_deref(),
            Some("using Bash")
        );
        wait_until(|| session_state(&mut f, &id).state == SessionState::Exited);
        let s = session_state(&mut f, &id);
        assert_eq!(s.agent_state, AgentState::Done);
        assert_eq!(s.agent_detail, None);
    }

    #[test]
    fn idle_notification_means_waiting_not_blocked() {
        let idle_typed = json!({"notification_type": "idle_prompt", "message": "Claude is waiting for your input"});
        assert_eq!(
            map_hook("Notification", &idle_typed).unwrap().0,
            AgentState::Waiting
        );
        let idle_untyped = json!({"message": "Claude is waiting for your input"});
        assert_eq!(
            map_hook("Notification", &idle_untyped).unwrap().0,
            AgentState::Waiting
        );
        let permission = json!({"notification_type": "permission_prompt", "message": "Claude needs your permission to use Bash"});
        let (state, detail) = map_hook("Notification", &permission).unwrap();
        assert_eq!(state, AgentState::Blocked);
        assert_eq!(
            detail.as_deref(),
            Some("Claude needs your permission to use Bash")
        );
    }

    #[test]
    fn exit_rules_follow_the_status_model() {
        use AgentState::*;
        // One-shot runs that exit cleanly are done, whatever came before.
        assert_eq!(final_agent_state(Some(0), Waiting, true), Done);
        // Interactive sessions the human quit while it waited on them are stopped.
        assert_eq!(final_agent_state(Some(0), Waiting, false), Stopped);
        assert_eq!(final_agent_state(Some(0), Blocked, false), Stopped);
        // Plain commands (no hooks) that exit cleanly are done.
        assert_eq!(final_agent_state(Some(0), Unknown, false), Done);
        assert_eq!(final_agent_state(Some(2), Working, false), Failed);
        assert_eq!(final_agent_state(None, Working, true), Failed);

        assert!(is_one_shot(Some("claude"), &["-p".into(), "do it".into()]));
        assert!(!is_one_shot(Some("claude"), &[]));
        assert!(!is_one_shot(None, &["-p".into()]));
    }

    #[test]
    fn quitting_after_a_finished_turn_raises_nothing() {
        let mut f = fixture();
        let id = start(&mut f, "sh", &["-c", "sleep 0.3; exit 0"]);
        send_hook(&mut f, &id, "Stop", json!({}));
        assert_eq!(open_kinds(&f, &id), vec!["waiting"]);
        wait_until(|| session_state(&mut f, &id).state == SessionState::Exited);
        assert_eq!(session_state(&mut f, &id).agent_state, AgentState::Stopped);
        assert!(open_kinds(&f, &id).is_empty());
    }

    #[test]
    fn agents_do_not_inherit_the_daemons_agent_variables() {
        assert!(is_inherited_agent_var("CLAUDECODE"));
        assert!(is_inherited_agent_var("CLAUDE_CODE_ENTRYPOINT"));
        assert!(!is_inherited_agent_var("CORVX_HOME"));
        assert!(!is_inherited_agent_var("PATH"));

        std::env::set_var("CLAUDE_CODE_CORVX_TEST_MARKER", "leaked");
        let mut f = fixture();
        let id = start(
            &mut f,
            "sh",
            &["-c", "echo \"[${CLAUDE_CODE_CORVX_TEST_MARKER:-clean}]\""],
        );
        wait_until(|| output_text(&mut f, &id).0.contains(']'));
        std::env::remove_var("CLAUDE_CODE_CORVX_TEST_MARKER");
        assert!(output_text(&mut f, &id).0.contains("[clean]"));
    }

    #[test]
    fn transcripts_and_settings_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let mut f = fixture();
        let v = f
            .mgr
            .handle(
                &f.core,
                methods::SESSION_START,
                json!({"project": f.project_id, "command": "sh", "args": ["-c", "exit 0"], "runtime": "claude"}),
            )
            .unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        let dir = f.data.path().join("sessions");
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(format!("{id}.log"))), 0o600);
        assert_eq!(mode(&dir.join(format!("{id}.settings.json"))), 0o600);
    }

    /// Runs the timer step until `done` holds, as the daemon does every half second.
    fn tick_until(f: &mut Fixture, mut done: impl FnMut(&mut Fixture) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            f.mgr.tick(&f.core).unwrap();
            if done(f) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting");
            thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn tmux_session_captures_output_and_exit_code() {
        let Some(mut f) = tmux_fixture() else { return };
        let id = start(&mut f, "sh", &["-c", "echo tmux-hello; sleep 0.3; exit 5"]);
        assert_eq!(session_state(&mut f, &id).backend, "tmux");
        tick_until(&mut f, |f| {
            session_state(f, &id).state == SessionState::Exited
        });
        assert!(output_text(&mut f, &id).0.contains("tmux-hello"));
        let s = session_state(&mut f, &id);
        assert_eq!(s.exit_code, Some(5));
        assert_eq!(s.agent_state, AgentState::Failed);
        assert_eq!(open_kinds(&f, &id), vec!["failed"]);
        // The finished pane is cleaned up.
        assert!(!f.mgr.tmux.as_ref().unwrap().panes().contains_key(&id));
    }

    #[test]
    fn tmux_session_takes_input_and_stops() {
        let Some(mut f) = tmux_fixture() else { return };
        let id = start(&mut f, "cat", &[]);
        f.mgr
            .handle(
                &f.core,
                methods::SESSION_INPUT,
                json!({"id": id, "data": "ping-tmux\n"}),
            )
            .unwrap();
        tick_until(&mut f, |f| output_text(f, &id).0.contains("ping-tmux"));
        f.mgr
            .handle(&f.core, methods::SESSION_STOP, json!({"id": id}))
            .unwrap();
        assert_eq!(session_state(&mut f, &id).state, SessionState::Stopped);
        assert!(!f.mgr.tmux.as_ref().unwrap().panes().contains_key(&id));
    }

    #[test]
    fn tmux_sessions_are_adopted_by_a_new_daemon() {
        let Some(mut f) = tmux_fixture() else { return };
        let id = start(&mut f, "sleep", &["30"]);
        send_hook(
            &mut f,
            &id,
            "Notification",
            json!({"message": "needs permission"}),
        );
        assert_eq!(open_kinds(&f, &id), vec!["blocked"]);

        // A new daemon on the same data directory: the old manager's state is gone.
        f.mgr = SessionManager::with_backend(f.data.path().to_path_buf(), BackendKind::Tmux);
        let report = f.mgr.recover(&f.core).unwrap();
        assert_eq!(
            report,
            Recovery {
                adopted: 1,
                lost: 0
            }
        );

        let s = session_state(&mut f, &id);
        assert_eq!(s.state, SessionState::Running);
        assert_eq!(
            s.agent_state,
            AgentState::Blocked,
            "agent state survives the restart"
        );
        assert_eq!(
            open_kinds(&f, &id),
            vec!["blocked"],
            "its attention item is kept"
        );
        f.mgr
            .handle(&f.core, methods::SESSION_STOP, json!({"id": id}))
            .unwrap();
    }

    #[test]
    fn pty_sessions_are_recorded_as_lost_after_a_restart() {
        let mut f = fixture();
        let id = start(&mut f, "sleep", &["30"]);
        send_hook(
            &mut f,
            &id,
            "Notification",
            json!({"message": "needs permission"}),
        );
        assert_eq!(open_kinds(&f, &id), vec!["blocked"]);

        f.mgr = SessionManager::with_backend(f.data.path().to_path_buf(), BackendKind::Pty);
        let report = f.mgr.recover(&f.core).unwrap();
        assert_eq!(
            report,
            Recovery {
                adopted: 0,
                lost: 1
            }
        );
        assert!(open_kinds(&f, &id).is_empty(), "stale items are cleared");
        assert!(f.core.store().running_sessions().unwrap().is_empty());
        // Still listed, as lost, so it can be resumed.
        assert_eq!(session_state(&mut f, &id).state, SessionState::Lost);
    }

    fn hook_reply(f: &mut Fixture, id: &str, event: &str, payload: Value) -> Value {
        f.mgr
            .handle(
                &f.core,
                methods::SESSION_HOOK,
                json!({"id": id, "event": event, "payload": payload}),
            )
            .unwrap()
    }

    fn poll(f: &mut Fixture, id: &str, item: &str) -> PermissionStatus {
        serde_json::from_value(
            f.mgr
                .handle(
                    &f.core,
                    methods::SESSION_PERMISSION,
                    json!({"id": id, "item": item}),
                )
                .unwrap(),
        )
        .unwrap()
    }

    fn decide(f: &mut Fixture, item: &str, decision: &str) -> Result<Value, ServiceError> {
        f.mgr.handle(
            &f.core,
            methods::ATTENTION_DECIDE,
            json!({"id": item, "decision": decision}),
        )
    }

    fn bash_request(command: &str) -> Value {
        json!({"session_id": "conv-1", "tool_name": "Bash",
               "tool_input": {"command": command, "description": "Run the tests"},
               "permission_suggestions": []})
    }

    fn start_supervised(f: &mut Fixture, supervisor: &str, extra: Value) -> String {
        let mut params = json!({"project": f.project_id, "command": "sleep", "args": ["30"],
                                "supervisor": supervisor});
        for (k, v) in extra.as_object().unwrap() {
            params[k] = v.clone();
        }
        let v = f
            .mgr
            .handle(&f.core, methods::SESSION_START, params)
            .unwrap();
        v["id"].as_str().unwrap().to_string()
    }

    fn decide_as(
        f: &mut Fixture,
        item: &str,
        decision: &str,
        by: &str,
        message: Option<&str>,
    ) -> Result<Value, ServiceError> {
        f.mgr.handle(
            &f.core,
            methods::ATTENTION_DECIDE,
            json!({"id": item, "decision": decision, "by": by, "message": message}),
        )
    }

    fn stop_all(f: &mut Fixture, ids: &[&str]) {
        for id in ids {
            let _ = f
                .mgr
                .handle(&f.core, methods::SESSION_STOP, json!({"id": id}));
        }
    }

    #[test]
    fn an_orchestrator_decides_its_agents_requests() {
        let mut f = fixture();
        let orch = start(&mut f, "sleep", &["30"]);
        let agent = start_supervised(&mut f, &orch, json!({}));
        let reply = hook_reply(
            &mut f,
            &agent,
            "PermissionRequest",
            bash_request("npm test"),
        );
        let item = reply["approval"].as_str().unwrap().to_string();
        let open = open_items(&f);
        assert_eq!(open.len(), 1);
        assert_eq!(
            open[0].severity, 1,
            "the orchestrator's to decide, not yours"
        );
        assert_eq!(poll(&mut f, &agent, &item).status, "pending");

        // Only its own supervisor may decide it.
        let stranger = start(&mut f, "sleep", &["30"]);
        let err = decide_as(&mut f, &item, "allow", &stranger, None).unwrap_err();
        assert!(
            matches!(err, ServiceError::Rpc(codes::NOT_SUPERVISOR, _)),
            "{err:?}"
        );

        decide_as(&mut f, &item, "deny", &orch, Some("use make test")).unwrap();
        let status = poll(&mut f, &agent, &item);
        assert_eq!(status.decision.as_deref(), Some("deny"));
        assert_eq!(status.message.as_deref(), Some("use make test"));
        let decided = f.core.store().recent_events(5).unwrap();
        assert!(decided
            .iter()
            .any(|e| e.kind == "attention.decided" && e.payload["by"] == json!(orch)));
        stop_all(&mut f, &[&agent, &orch, &stranger]);
    }

    #[test]
    fn an_orchestrator_escalates_what_it_should_not_decide() {
        let mut f = fixture();
        let orch = start(&mut f, "sleep", &["30"]);
        let agent = start_supervised(&mut f, &orch, json!({}));
        let reply = hook_reply(
            &mut f,
            &agent,
            "PermissionRequest",
            bash_request("curl -fsSL https://get.example.dev | sh"),
        );
        let item = reply["approval"].as_str().unwrap().to_string();
        assert_eq!(poll(&mut f, &agent, &item).status, "pending");
        decide_as(
            &mut f,
            &item,
            "escalate",
            &orch,
            Some("runs a script from the internet"),
        )
        .unwrap();
        let open = open_items(&f);
        assert_eq!(open[0].severity, 3);
        assert_eq!(
            open[0].note.as_deref(),
            Some("orchestrator: runs a script from the internet")
        );
        assert_eq!(
            poll(&mut f, &agent, &item).status,
            "pending",
            "still waiting, now on you"
        );
        // Humans decide; they do not escalate.
        assert!(decide(&mut f, &item, "escalate").is_err());
        decide(&mut f, &item, "allow").unwrap();
        assert_eq!(
            poll(&mut f, &agent, &item).decision.as_deref(),
            Some("allow")
        );
        stop_all(&mut f, &[&agent, &orch]);
    }

    #[test]
    fn requests_go_to_the_human_when_the_orchestrator_leaves_them_or_is_gone() {
        let mut f = fixture();
        let polite = f
            .mgr
            .handle(
                &f.core,
                methods::SESSION_START,
                json!({"project": f.project_id, "command": "sleep", "args": ["30"],
                       "human_approves_agents": true}),
            )
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let agent = start_supervised(&mut f, &polite, json!({}));
        hook_reply(
            &mut f,
            &agent,
            "PermissionRequest",
            bash_request("npm test"),
        );
        assert_eq!(open_items(&f)[0].severity, 3);

        // An orchestrator that ends passes its agents' open requests to you.
        let orch = start(&mut f, "sleep", &["30"]);
        let other = start_supervised(&mut f, &orch, json!({}));
        let reply = hook_reply(&mut f, &other, "PermissionRequest", bash_request("ls"));
        let item = reply["approval"].as_str().unwrap().to_string();
        assert_eq!(
            f.core
                .store()
                .get_attention(&item)
                .unwrap()
                .unwrap()
                .severity,
            1
        );
        stop_all(&mut f, &[&orch]);
        let escalated = f.core.store().get_attention(&item).unwrap().unwrap();
        assert_eq!(escalated.severity, 3);
        assert_eq!(
            escalated.note.as_deref(),
            Some("its orchestrator has ended")
        );
        // Later requests go straight to you.
        hook_reply(&mut f, &other, "PermissionRequest", bash_request("ls -la"));
        assert_eq!(
            f.core
                .store()
                .open_approval_for(&other)
                .unwrap()
                .unwrap()
                .severity,
            3
        );
        stop_all(&mut f, &[&agent, &polite, &other]);
    }

    #[test]
    fn corvx_rules_send_requests_to_the_human() {
        let mut f = fixture();
        let orch = start(&mut f, "sleep", &["30"]);
        let agent = start_supervised(&mut f, &orch, json!({}));
        f.mgr
            .handle(
                &f.core,
                methods::SESSION_AUTOPILOT,
                json!({"id": agent, "on": true, "by": orch}),
            )
            .unwrap();

        // Before the tool runs: Claude Code is told to ask, even if its settings allow it.
        let pre = hook_reply(
            &mut f,
            &agent,
            "PreToolUse",
            bash_request("git push origin main"),
        );
        assert_eq!(
            pre["pre"],
            json!({"decision": "ask", "reason": "CORVX rule git-push: pushes to a remote"}),
            "{pre}"
        );
        let pre = hook_reply(&mut f, &agent, "PreToolUse", bash_request("npm test"));
        assert!(pre.get("pre").is_none(), "{pre}");

        // The request goes to the human: not decided by autopilot, not the orchestrator's.
        let reply = hook_reply(
            &mut f,
            &agent,
            "PermissionRequest",
            bash_request("git push origin main"),
        );
        assert!(
            reply.get("decision").is_none(),
            "autopilot must not decide it: {reply}"
        );
        let item = reply["approval"].as_str().unwrap().to_string();
        let open = f.core.store().get_attention(&item).unwrap().unwrap();
        assert_eq!(open.severity, 3);
        assert_eq!(
            open.note.as_deref(),
            Some("CORVX rule git-push: pushes to a remote")
        );
        assert_eq!(poll(&mut f, &agent, &item).status, "pending");
        let err = decide_as(&mut f, &item, "allow", &orch, None).unwrap_err();
        assert!(
            matches!(err, ServiceError::Rpc(codes::NOT_SUPERVISOR, _)),
            "{err}"
        );
        let err = decide_as(&mut f, &item, "escalate", &orch, Some("hm")).unwrap_err();
        assert!(
            matches!(err, ServiceError::Rpc(codes::NOT_SUPERVISOR, _)),
            "{err}"
        );
        // Turning autopilot on again does not approve it either.
        f.mgr
            .handle(
                &f.core,
                methods::SESSION_AUTOPILOT,
                json!({"id": agent, "on": true, "by": orch}),
            )
            .unwrap();
        assert_eq!(poll(&mut f, &agent, &item).status, "pending");
        decide(&mut f, &item, "allow").unwrap();
        assert_eq!(
            poll(&mut f, &agent, &item).decision.as_deref(),
            Some("allow")
        );

        // Writing outside the agent's working directory.
        let write = json!({"tool_name": "Write", "tool_input": {"file_path": "/etc/hosts"}});
        let reply = hook_reply(&mut f, &agent, "PermissionRequest", write);
        assert!(reply.get("approval").is_some(), "{reply}");

        // Off for this project: the rule no longer applies, and autopilot approves.
        let set = |f: &mut Fixture, id: &str, action: &str| {
            f.core
                .handle(
                    methods::RULES_SET,
                    json!({"id": id, "project": f.project_id, "action": action}),
                )
                .unwrap();
        };
        set(&mut f, "git-push", "off");
        let reply = hook_reply(
            &mut f,
            &agent,
            "PermissionRequest",
            bash_request("git push origin main"),
        );
        assert_eq!(reply["decision"], json!("allow"), "{reply}");

        // Autopilot off, and the rule set to allow: approved at once, before Claude asks.
        f.mgr
            .handle(
                &f.core,
                methods::SESSION_AUTOPILOT,
                json!({"id": agent, "on": false}),
            )
            .unwrap();
        set(&mut f, "git-push", "allow");
        let pre = hook_reply(&mut f, &agent, "PreToolUse", bash_request("git push"));
        assert_eq!(pre["pre"]["decision"], json!("allow"), "{pre}");
        let reply = hook_reply(
            &mut f,
            &agent,
            "PermissionRequest",
            bash_request("git push"),
        );
        assert_eq!(reply["decision"], json!("allow"), "{reply}");
        assert!(reply.get("approval").is_none(), "{reply}");

        // Set to deny: refused at once, with the reason.
        set(&mut f, "git-push", "deny");
        let pre = hook_reply(&mut f, &agent, "PreToolUse", bash_request("git push"));
        assert_eq!(pre["pre"]["decision"], json!("deny"), "{pre}");
        let reason = pre["pre"]["reason"].as_str().unwrap();
        assert!(
            reason.starts_with("CORVX rule git-push: pushes to a remote"),
            "{reason}"
        );
        let reply = hook_reply(
            &mut f,
            &agent,
            "PermissionRequest",
            bash_request("git push"),
        );
        assert_eq!(reply["decision"], json!("deny"), "{reply}");
        let events = f.core.store().recent_events(20).unwrap();
        assert!(events.iter().any(|e| e.kind == "attention.auto_denied"));
        assert!(
            events
                .iter()
                .any(|e| e.kind == "attention.auto_approved"
                    && e.payload["rule"] == json!("git-push"))
        );
        stop_all(&mut f, &[&agent, &orch]);
    }

    fn set_level(f: &mut Fixture, level: &str) {
        f.core
            .handle(
                methods::PROJECT_AUTONOMY,
                json!({"id": f.project_id, "level": level}),
            )
            .unwrap();
    }

    #[test]
    fn autonomy_levels_move_the_dial() {
        let mut f = fixture();
        let orch = start(&mut f, "sleep", &["30"]);
        let agent = start_supervised(&mut f, &orch, json!({}));

        // Supervised: requests come to you, and the orchestrator cannot turn on autopilot.
        set_level(&mut f, "supervised");
        let reply = hook_reply(
            &mut f,
            &agent,
            "PermissionRequest",
            bash_request("npm test"),
        );
        let item = reply["approval"].as_str().unwrap().to_string();
        assert_eq!(
            f.core
                .store()
                .get_attention(&item)
                .unwrap()
                .unwrap()
                .severity,
            3
        );
        let err = f
            .mgr
            .handle(
                &f.core,
                methods::SESSION_AUTOPILOT,
                json!({"id": agent, "on": true, "by": orch}),
            )
            .unwrap_err();
        assert!(
            matches!(err, ServiceError::Rpc(codes::NOT_SUPERVISOR, _)),
            "{err}"
        );
        let err = f
            .mgr
            .handle(
                &f.core,
                methods::SESSION_START,
                json!({"project": f.project_id, "command": "sleep", "args": ["30"],
                       "supervisor": orch, "autopilot": true}),
            )
            .unwrap_err();
        assert!(
            matches!(err, ServiceError::Rpc(codes::NOT_SUPERVISOR, _)),
            "{err}"
        );

        // Guided: the orchestrator decides.
        set_level(&mut f, "guided");
        let reply = hook_reply(
            &mut f,
            &agent,
            "PermissionRequest",
            bash_request("npm test"),
        );
        let item = reply["approval"].as_str().unwrap().to_string();
        assert_eq!(
            f.core
                .store()
                .get_attention(&item)
                .unwrap()
                .unwrap()
                .severity,
            1
        );

        // Autonomous: approved without asking, except what the rules send you.
        set_level(&mut f, "autonomous");
        let reply = hook_reply(
            &mut f,
            &agent,
            "PermissionRequest",
            bash_request("npm test"),
        );
        assert_eq!(reply["decision"], json!("allow"), "{reply}");
        let reply = hook_reply(
            &mut f,
            &agent,
            "PermissionRequest",
            bash_request("git push"),
        );
        assert!(reply.get("approval").is_some(), "pushes still ask: {reply}");
        let pre = hook_reply(
            &mut f,
            &agent,
            "PreToolUse",
            bash_request("git reset --hard"),
        );
        assert_eq!(pre["pre"]["decision"], json!("allow"), "{pre}");

        // Full: pushes too; the rules themselves still ask.
        set_level(&mut f, "full");
        let pre = hook_reply(&mut f, &agent, "PreToolUse", bash_request("git push"));
        assert_eq!(pre["pre"]["decision"], json!("allow"), "{pre}");
        let pre = hook_reply(
            &mut f,
            &agent,
            "PreToolUse",
            bash_request("corvx rules set git-push ask"),
        );
        assert_eq!(pre["pre"]["decision"], json!("ask"), "{pre}");
        let err = f
            .core
            .handle(
                methods::PROJECT_AUTONOMY,
                json!({"id": f.project_id, "level": "yolo"}),
            )
            .unwrap_err();
        assert!(matches!(err, ServiceError::Rpc(codes::INVALID_PARAMS, _)));
        stop_all(&mut f, &[&agent, &orch]);
    }

    #[test]
    fn what_you_keep_approving_is_offered_to_allow() {
        let mut f = fixture();
        let id = start(&mut f, "sleep", &["30"]);
        for _ in 0..3 {
            let reply = hook_reply(
                &mut f,
                &id,
                "PermissionRequest",
                bash_request("npm test -- --ci"),
            );
            let item = reply["approval"].as_str().unwrap().to_string();
            poll(&mut f, &id, &item);
            decide(&mut f, &item, "allow").unwrap();
        }
        let suggestion = open_items(&f)
            .into_iter()
            .find(|i| i.kind == "suggestion")
            .expect("a suggestion after three approvals");
        assert!(
            suggestion.summary.contains("`npm test …` 3 times"),
            "{}",
            suggestion.summary
        );
        // An orchestrator cannot answer it; you accept it with allow.
        let err = f
            .mgr
            .handle(
                &f.core,
                methods::ATTENTION_DECIDE,
                json!({"id": suggestion.id, "decision": "allow", "by": id}),
            )
            .unwrap_err();
        assert!(matches!(err, ServiceError::Rpc(codes::NOT_SUPERVISOR, _)));
        decide(&mut f, &suggestion.id, "allow").unwrap();
        let pre = hook_reply(&mut f, &id, "PreToolUse", bash_request("npm test"));
        assert_eq!(pre["pre"]["decision"], json!("allow"), "{pre}");
        let pre = hook_reply(
            &mut f,
            &id,
            "PreToolUse",
            bash_request("npm test && git push"),
        );
        assert_eq!(
            pre["pre"]["decision"],
            json!("ask"),
            "chained commands still ask: {pre}"
        );
        stop_all(&mut f, &[&id]);
    }

    #[test]
    fn journal_entries_go_to_the_project() {
        let mut f = fixture();
        let id = start(&mut f, "sleep", &["30"]);
        let reply = f
            .mgr
            .handle(
                &f.core,
                methods::PROJECT_JOURNAL,
                json!({"session": id, "text": "Goal: add CSV export.\nWhat happened: done.",
                       "title": "Mission"}),
            )
            .unwrap();
        f.mgr
            .handle(
                &f.core,
                methods::PROJECT_JOURNAL,
                json!({"session": id, "text": "Merged corvx/s2.", "title": "Done"}),
            )
            .unwrap();
        let file = PathBuf::from(reply["file"].as_str().unwrap());
        assert!(
            file.starts_with(f._project_dir.path().canonicalize().unwrap())
                || file.starts_with(f._project_dir.path()),
            "{}",
            file.display()
        );
        assert!(file.to_string_lossy().contains(".corvx/journal/"));
        let text = fs::read_to_string(&file).unwrap();
        assert!(text.contains("## Mission · "), "{text}");
        assert!(text.contains("Goal: add CSV export."), "{text}");
        assert!(
            text.contains("## Done · ") && text.contains("Merged corvx/s2."),
            "{text}"
        );
        assert_eq!(
            text.matches("# ").count() - text.matches("## ").count(),
            1,
            "one title: {text}"
        );
        stop_all(&mut f, &[&id]);
    }

    #[test]
    fn dates_are_utc_calendar_dates() {
        assert_eq!(date_of(0), "1970-01-01");
        assert_eq!(date_of(951_782_400), "2000-02-29");
        assert_eq!(time_of(1_794_412_999), "2026-11-11 16:03 UTC");
    }

    #[test]
    fn autopilot_approves_without_asking() {
        let mut f = fixture();
        let orch = start(&mut f, "sleep", &["30"]);
        let agent = start_supervised(&mut f, &orch, json!({}));
        let reply = hook_reply(
            &mut f,
            &agent,
            "PermissionRequest",
            bash_request("npm test"),
        );
        let item = reply["approval"].as_str().unwrap().to_string();
        assert_eq!(poll(&mut f, &agent, &item).status, "pending");

        // Turning it on approves the request the agent is waiting on.
        f.mgr
            .handle(
                &f.core,
                methods::SESSION_AUTOPILOT,
                json!({"id": agent, "on": true, "by": orch}),
            )
            .unwrap();
        assert_eq!(
            poll(&mut f, &agent, &item).decision.as_deref(),
            Some("allow")
        );
        assert!(session_state(&mut f, &agent).autopilot);

        let reply = hook_reply(
            &mut f,
            &agent,
            "PermissionRequest",
            bash_request("npm run build"),
        );
        assert_eq!(reply["decision"], json!("allow"));
        assert!(reply.get("approval").is_none());
        assert!(open_items(&f).is_empty(), "nothing to decide");
        let events = f.core.store().recent_events(10).unwrap();
        assert!(events.iter().any(|e| e.kind == "attention.auto_approved"
            && e.payload["request"] == json!("Bash: npm run build · Run the tests")));

        // An orchestrator cannot put itself on autopilot; you can.
        let err = f
            .mgr
            .handle(
                &f.core,
                methods::SESSION_AUTOPILOT,
                json!({"id": orch, "on": true, "by": orch}),
            )
            .unwrap_err();
        assert!(matches!(err, ServiceError::Rpc(codes::NOT_SUPERVISOR, _)));
        f.mgr
            .handle(
                &f.core,
                methods::SESSION_AUTOPILOT,
                json!({"id": agent, "on": false}),
            )
            .unwrap();
        let reply = hook_reply(&mut f, &agent, "PermissionRequest", bash_request("rm x"));
        assert!(reply.get("approval").is_some(), "asks again: {reply}");
        stop_all(&mut f, &[&agent, &orch]);
    }

    #[test]
    fn permission_requests_are_decided_from_the_inbox() {
        let mut f = fixture();
        let id = start(&mut f, "sleep", &["30"]);
        let reply = hook_reply(&mut f, &id, "PermissionRequest", bash_request("npm test"));
        let item = reply["approval"].as_str().unwrap().to_string();
        let open = open_items(&f);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].kind, "approval");
        assert_eq!(open[0].summary, "Bash: npm test · Run the tests");
        assert_eq!(session_state(&mut f, &id).agent_state, AgentState::Blocked);
        assert_eq!(poll(&mut f, &id, &item).status, "pending");

        // Claude Code's own reminder a few seconds later does not replace the approval.
        send_hook(
            &mut f,
            &id,
            "Notification",
            json!({"message": "Claude needs your permission to use Bash",
                   "notification_type": "permission_prompt"}),
        );
        assert_eq!(open_kinds(&f, &id), vec!["approval"]);

        decide(&mut f, &item, "allow").unwrap();
        let status = poll(&mut f, &id, &item);
        assert_eq!(status.status, "decided");
        assert_eq!(status.decision.as_deref(), Some("allow"));
        assert!(open_kinds(&f, &id).is_empty());
        // Hooks name the conversation, which is kept for resuming.
        assert_eq!(
            session_state(&mut f, &id).runtime_session.as_deref(),
            Some("conv-1")
        );
        f.mgr
            .handle(&f.core, methods::SESSION_STOP, json!({"id": id}))
            .unwrap();
    }

    #[test]
    fn an_approval_answered_in_the_session_goes_away() {
        let mut f = fixture();
        let id = start(&mut f, "sleep", &["30"]);
        let reply = hook_reply(&mut f, &id, "PermissionRequest", bash_request("make"));
        let item = reply["approval"].as_str().unwrap().to_string();
        assert_eq!(poll(&mut f, &id, &item).status, "pending");
        // The human answered the dialog in the session; the tool ran.
        send_hook(&mut f, &id, "PostToolUse", json!({"tool_name": "Bash"}));
        assert!(open_kinds(&f, &id).is_empty());
        assert_eq!(poll(&mut f, &id, &item).status, "gone");
        assert!(decide(&mut f, &item, "allow").is_err());
        f.mgr
            .handle(&f.core, methods::SESSION_STOP, json!({"id": id}))
            .unwrap();
    }

    #[test]
    fn a_decision_nobody_waits_for_falls_back_to_the_session() {
        let mut f = fixture();
        let id = start(&mut f, "sleep", &["30"]);
        let reply = hook_reply(&mut f, &id, "PermissionRequest", bash_request("make"));
        let item = reply["approval"].as_str().unwrap().to_string();
        // No hook is polling for it (it timed out, or the daemon restarted).
        match decide(&mut f, &item, "allow") {
            Err(ServiceError::Rpc(code, _)) => assert_eq!(code, codes::APPROVAL_GONE),
            other => panic!("expected APPROVAL_GONE, got {other:?}"),
        }
        let open = open_items(&f);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].kind, "blocked");
        assert!(open[0].summary.ends_with("answer it in the session"));
        f.mgr
            .handle(&f.core, methods::SESSION_STOP, json!({"id": id}))
            .unwrap();
    }

    #[test]
    fn claude_sessions_register_the_permission_hook_with_a_long_timeout() {
        let settings = claude_settings("corvx session hook s1");
        let hook = &settings["hooks"]["PermissionRequest"][0]["hooks"][0];
        assert_eq!(hook["command"], "corvx session hook s1");
        assert!(hook["timeout"].as_u64().unwrap() >= 3600);
        assert_eq!(settings["hooks"]["Stop"][0]["hooks"][0]["timeout"], 5);
    }

    /// A stand-in agent that records its arguments, one per line, then waits.
    fn recording_agent(f: &Fixture) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = f.data.path().join("agent.sh");
        fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.$CORVX_SESSION_ID.args\"\nexec sleep 30\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn recorded_args(script: &Path, id: &str) -> String {
        fs::read_to_string(format!("{}.{id}.args", script.display())).unwrap_or_default()
    }

    #[test]
    fn lost_sessions_resume_their_conversation_with_their_lost_agents() {
        let mut f = fixture();
        let agent = recording_agent(&f);
        let root = start_with(
            &mut f,
            json!({"command": agent, "args": ["--model", "sonnet", "the goal"],
                   "runtime": "claude", "label": "orchestrator · goal"}),
        )
        .unwrap();
        let worker = start_with(
            &mut f,
            json!({"command": agent, "args": ["--permission-mode", "acceptEdits", "do x"],
                   "runtime": "claude", "supervisor": root}),
        )
        .unwrap();
        let stopped = start_with(
            &mut f,
            json!({"command": agent, "args": ["review"], "runtime": "claude", "supervisor": root}),
        )
        .unwrap();
        f.mgr
            .handle(&f.core, methods::SESSION_STOP, json!({"id": stopped}))
            .unwrap();
        send_hook(
            &mut f,
            &root,
            "UserPromptSubmit",
            json!({"session_id": "conv-root"}),
        );
        send_hook(
            &mut f,
            &worker,
            "UserPromptSubmit",
            json!({"session_id": "conv-worker"}),
        );
        wait_until(|| recorded_args(&agent, &root).contains("the goal"));
        wait_until(|| recorded_args(&agent, &worker).contains("do x"));

        // The machine restarts: the pty sessions are gone.
        f.mgr = SessionManager::with_backend(f.data.path().to_path_buf(), BackendKind::Pty);
        f.mgr.recover(&f.core).unwrap();
        assert_eq!(session_state(&mut f, &root).state, SessionState::Lost);

        let v = f
            .mgr
            .handle(&f.core, methods::SESSION_RESUME, json!({"id": root}))
            .unwrap();
        let resumed: SessionResumed = serde_json::from_value(v).unwrap();
        let ids: Vec<&str> = resumed.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![root.as_str(), worker.as_str()],
            "the stopped agent stays stopped"
        );

        wait_until(|| recorded_args(&agent, &root).contains("--resume"));
        wait_until(|| recorded_args(&agent, &worker).contains("--resume"));
        let root_args = recorded_args(&agent, &root);
        assert!(
            root_args.contains("--model\nsonnet\n--resume\nconv-root\n"),
            "{root_args}"
        );
        assert!(
            !root_args.contains("the goal"),
            "the first prompt is not sent again"
        );
        assert!(
            root_args.contains("--settings"),
            "CORVX's hooks are registered again"
        );
        let nudge = root_args.lines().last().unwrap();
        assert!(
            nudge.starts_with("[CORVX]") && nudge.contains(&worker) && nudge.contains(&stopped)
        );
        let worker_args = recorded_args(&agent, &worker);
        assert!(worker_args.contains("--permission-mode\nacceptEdits\n--resume\nconv-worker\n"));
        assert!(!worker_args.contains("do x"));

        let again = f
            .mgr
            .handle(&f.core, methods::SESSION_RESUME, json!({"id": root}));
        assert!(matches!(again, Err(ServiceError::Rpc(c, _)) if c == codes::NOT_RESUMABLE));
        for id in [&root, &worker] {
            assert_eq!(session_state(&mut f, id).state, SessionState::Running);
            f.mgr
                .handle(&f.core, methods::SESSION_STOP, json!({"id": id}))
                .unwrap();
        }
    }

    #[test]
    fn plain_commands_are_not_resumable() {
        let mut f = fixture();
        let id = start(&mut f, "sleep", &["30"]);
        f.mgr
            .handle(&f.core, methods::SESSION_STOP, json!({"id": id}))
            .unwrap();
        let r = f
            .mgr
            .handle(&f.core, methods::SESSION_RESUME, json!({"id": id}));
        assert!(matches!(r, Err(ServiceError::Rpc(c, _)) if c == codes::NOT_RESUMABLE));
    }

    #[test]
    fn resume_args_keep_flags_and_drop_the_first_prompt() {
        let args: Vec<String> = [
            "--mcp-config",
            "/m.json",
            "--settings",
            "/s.json",
            "--append-system-prompt",
            "be an orchestrator",
            "--model=opus",
            "--verbose",
            "--resume",
            "old",
            "Build Snip",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            resume_args(&args),
            vec![
                "--append-system-prompt",
                "be an orchestrator",
                "--model=opus",
                "--verbose"
            ]
        );
    }

    #[test]
    fn requests_are_described_in_one_line() {
        assert_eq!(
            describe_request(&json!({"tool_name": "Write",
                                     "tool_input": {"file_path": "/a/b.py", "content": "x"}})),
            "Write: /a/b.py"
        );
        assert_eq!(
            describe_request(&json!({"tool_name": "Bash",
                                     "tool_input": {"command": "a &&\nb"}})),
            "Bash: a && ⏎ b"
        );
        assert_eq!(describe_request(&json!({})), "a tool");
    }

    #[test]
    fn conversations_are_found_by_directory_and_start_time() {
        let projects = TempDir::new().unwrap();
        let cwd = Path::new("/Users/me/dev/snip/.corvx/worktrees/s1");
        let dir = projects
            .path()
            .join("-Users-me-dev-snip--corvx-worktrees-s1");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("abc-123.jsonl"), "{}").unwrap();
        fs::write(dir.join("notes.txt"), "").unwrap();
        let now = now_secs();
        assert_eq!(
            find_claude_conversation(projects.path(), cwd, now - 10, None).as_deref(),
            Some("abc-123")
        );
        assert_eq!(
            find_claude_conversation(projects.path(), cwd, now + 100, None),
            None,
            "a conversation from before the session started is someone else's"
        );
        assert_eq!(
            find_claude_conversation(projects.path(), Path::new("/elsewhere"), now - 10, None),
            None
        );
    }

    fn start_with(f: &mut Fixture, params: Value) -> Result<String, ServiceError> {
        let mut params = params;
        params["project"] = json!(f.project_id);
        let v = f.mgr.handle(&f.core, methods::SESSION_START, params)?;
        Ok(v["id"].as_str().unwrap().to_string())
    }

    fn open_items(f: &Fixture) -> Vec<corvx_proto::AttentionItem> {
        f.core.store().list_attention(false, now_secs()).unwrap()
    }

    #[test]
    fn supervised_agents_report_to_their_supervisor_not_the_inbox() {
        let mut f = fixture();
        let sup = start(&mut f, "sleep", &["30"]);
        let worker = start_with(
            &mut f,
            json!({"command": "sleep", "args": ["30"], "supervisor": sup}),
        )
        .unwrap();
        assert_eq!(
            session_state(&mut f, &worker).supervisor.as_deref(),
            Some(sup.as_str())
        );

        send_hook(&mut f, &worker, "Stop", json!({}));
        assert_eq!(
            session_state(&mut f, &worker).agent_state,
            AgentState::Waiting
        );
        assert!(
            open_kinds(&f, &worker).is_empty(),
            "waiting goes to the orchestrator"
        );

        // Permission requests still need a human.
        send_hook(
            &mut f,
            &worker,
            "Notification",
            json!({"notification_type": "permission_prompt", "message": "Allow rm?"}),
        );
        assert_eq!(open_kinds(&f, &worker), vec!["blocked"]);
        for id in [&worker, &sup] {
            f.mgr
                .handle(&f.core, methods::SESSION_STOP, json!({"id": id}))
                .unwrap();
        }
    }

    #[test]
    fn delegation_depth_is_limited() {
        let mut f = fixture();
        let l1 = start(&mut f, "sleep", &["30"]);
        let l2 = start_with(
            &mut f,
            json!({"command": "sleep", "args": ["30"], "supervisor": l1}),
        )
        .unwrap();
        let l3 = start_with(
            &mut f,
            json!({"command": "sleep", "args": ["30"], "supervisor": l2}),
        )
        .unwrap();
        let err = start_with(
            &mut f,
            json!({"command": "sleep", "args": ["30"], "supervisor": l3}),
        )
        .unwrap_err();
        assert!(matches!(err, ServiceError::Rpc(c, _) if c == codes::DEPTH_LIMIT));
        let err = start_with(
            &mut f,
            json!({"command": "sleep", "args": ["30"], "supervisor": "ghost"}),
        )
        .unwrap_err();
        assert!(matches!(err, ServiceError::Rpc(c, _) if c == codes::SESSION_NOT_FOUND));
        for id in [&l3, &l2, &l1] {
            f.mgr
                .handle(&f.core, methods::SESSION_STOP, json!({"id": id}))
                .unwrap();
        }
    }

    #[test]
    fn report_done_raises_a_review_item_with_the_summary() {
        let mut f = fixture();
        let id = start(&mut f, "sleep", &["30"]);
        f.mgr
            .handle(
                &f.core,
                methods::SESSION_REPORT,
                json!({"id": id, "outcome": "done", "summary": "API and UI built on corvx/s1"}),
            )
            .unwrap();
        let s = session_state(&mut f, &id);
        assert_eq!(s.agent_state, AgentState::Done);
        assert_eq!(s.agent_source.as_deref(), Some("self-report"));
        let item = open_items(&f)
            .into_iter()
            .find(|i| i.session_id == id)
            .unwrap();
        assert_eq!(item.kind, "done");
        assert_eq!(item.summary, "API and UI built on corvx/s1");
        let err = f
            .mgr
            .handle(
                &f.core,
                methods::SESSION_REPORT,
                json!({"id": id, "outcome": "maybe", "summary": "x"}),
            )
            .unwrap_err();
        assert!(matches!(err, ServiceError::Rpc(c, _) if c == codes::INVALID_PARAMS));
        f.mgr
            .handle(&f.core, methods::SESSION_STOP, json!({"id": id}))
            .unwrap();
    }

    #[test]
    fn questions_reach_the_inbox_and_take_an_answer() {
        let mut f = fixture();
        let id = start(&mut f, "sleep", &["30"]);
        let v = f
            .mgr
            .handle(
                &f.core,
                methods::ATTENTION_ASK,
                json!({"session_id": id, "question": "SQLite or Postgres?"}),
            )
            .unwrap();
        let qid = v["id"].as_str().unwrap().to_string();
        let item = open_items(&f).into_iter().find(|i| i.id == qid).unwrap();
        assert_eq!(item.kind, "question");
        f.core
            .handle(
                methods::ATTENTION_ANSWER,
                json!({"id": qid, "answer": "SQLite"}),
            )
            .unwrap();
        let answered: corvx_proto::AttentionItem = serde_json::from_value(
            f.core
                .handle(methods::ATTENTION_GET, json!({"id": qid}))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(answered.answer.as_deref(), Some("SQLite"));
        f.mgr
            .handle(&f.core, methods::SESSION_STOP, json!({"id": id}))
            .unwrap();
    }

    #[test]
    fn worktree_sessions_get_their_own_branch() {
        let mut f = fixture();
        let repo = Path::new(
            &f.core
                .store()
                .get_project(&f.project_id)
                .unwrap()
                .unwrap()
                .path,
        )
        .to_path_buf();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(
                ok.status.success(),
                "{}",
                String::from_utf8_lossy(&ok.stderr)
            );
            String::from_utf8_lossy(&ok.stdout).trim().to_string()
        };
        // Without a commit there is nothing to branch from.
        git(&["init", "-q"]);
        let err = start_with(
            &mut f,
            json!({"command": "sleep", "args": ["30"], "worktree": true}),
        )
        .unwrap_err();
        assert!(matches!(err, ServiceError::Rpc(c, _) if c == codes::WORKTREE_FAILED));

        git(&["commit", "-q", "--allow-empty", "-m", "init"]);
        let id = start_with(&mut f, json!({"command": "pwd", "worktree": true})).unwrap();
        let wt = session_state(&mut f, &id).worktree.unwrap();
        assert!(wt.ends_with(&format!(".corvx/worktrees/{id}")), "{wt}");
        assert!(Path::new(&wt).is_dir());
        assert!(git(&["branch", "--list", &format!("corvx/{id}")]).contains(&id));
        assert_eq!(
            git(&["status", "--porcelain"]),
            "",
            "worktrees stay out of git status"
        );
        // The agent runs inside its worktree.
        wait_until(|| output_text(&mut f, &id).0.contains(".corvx/worktrees"));
    }

    #[test]
    fn claude_sessions_get_corvx_tools() {
        let mut f = fixture();
        let id = start_with(
            &mut f,
            json!({"command": "sh", "args": ["-c", "exit 0"], "runtime": "claude"}),
        )
        .unwrap();
        let dir = f.data.path().join("sessions");
        let mcp: Value =
            serde_json::from_str(&fs::read_to_string(dir.join(format!("{id}.mcp.json"))).unwrap())
                .unwrap();
        let server = &mcp["mcpServers"]["corvx"];
        assert_eq!(server["args"].as_array().unwrap().last().unwrap(), "mcp");
        assert_eq!(server["env"]["CORVX_SESSION_ID"], id.as_str());
        let settings: Value = serde_json::from_str(
            &fs::read_to_string(dir.join(format!("{id}.settings.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(settings["permissions"]["allow"][0], "mcp__corvx");
    }

    #[test]
    fn screen_shows_the_session_as_text() {
        let mut f = fixture();
        let id = start(
            &mut f,
            "sh",
            &["-c", "printf '\\033[1mbold-text\\033[0m\\n'; sleep 5"],
        );
        wait_until(|| output_text(&mut f, &id).0.contains("bold-text"));
        let v = f
            .mgr
            .handle(&f.core, methods::SESSION_SCREEN, json!({"id": id}))
            .unwrap();
        let screen: SessionScreen = serde_json::from_value(v).unwrap();
        assert!(screen.text.contains("bold-text"));
        assert!(!screen.text.contains('\u{1b}'), "escape codes are removed");
        f.mgr
            .handle(&f.core, methods::SESSION_STOP, json!({"id": id}))
            .unwrap();
    }

    #[test]
    fn strip_ansi_keeps_text_only() {
        assert_eq!(
            strip_ansi("\u{1b}[31mred\u{1b}[0m\r\n\u{1b}]0;title\u{7}ok"),
            "red\nok"
        );
    }
}
