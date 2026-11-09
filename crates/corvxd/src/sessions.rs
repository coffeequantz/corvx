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

use base64::Engine;
use corvx_core::service::{Core, ServiceError};
use corvx_core::store::now_secs;
use corvx_proto::{
    codes, methods, AgentState, Session, SessionHookParams, SessionIdParams, SessionInputParams,
    SessionList, SessionOutput, SessionOutputParams, SessionResizeParams, SessionStartParams,
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
                session.state = SessionState::Stopped;
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
        // Items for sessions this daemon does not manage are stale.
        for item in core.store().list_attention(true, now_secs())? {
            if !self.entries.contains_key(&item.session_id) {
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
            let ended = match panes.get(id) {
                Some(pane) if !pane.dead => continue,
                Some(pane) => {
                    let code = pane.exit_status;
                    let _ = tmux.kill(id);
                    code
                }
                // Killed outside CORVX: the exit status is unknown.
                None => None,
            };
            let mut st = lock(&entry.shared.status);
            st.state = SessionState::Exited;
            st.exit_code = ended;
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
            methods::SESSION_HOOK => {
                let p: SessionHookParams = parse(params)?;
                self.hook(core, &p)?;
                Ok(json!({"ok": true}))
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

        let id = self.new_id();
        let start_failed = |e: &dyn Display| {
            ServiceError::Rpc(
                codes::SESSION_START_FAILED,
                format!("cannot start '{}': {e}", p.command),
            )
        };

        let logs = self.data_dir.join("sessions");
        create_private_dir(&logs).map_err(internal)?;
        let transcript_path = logs.join(format!("{id}.log"));
        // Created owner-only before anything writes to it.
        let transcript = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&transcript_path)
            .map_err(internal)?;

        // Runtime-specific setup. For Claude Code, register CORVX's hooks through a settings file.
        let mut args = p.args.clone();
        if p.runtime.as_deref() == Some("claude") {
            let settings_path = logs.join(format!("{id}.settings.json"));
            let hook_cmd = hook_command(&self.data_dir, &id);
            write_private(
                &settings_path,
                claude_settings(&hook_cmd).to_string().as_bytes(),
            )
            .map_err(internal)?;
            let mut with_settings = vec![
                "--settings".to_string(),
                settings_path.display().to_string(),
            ];
            with_settings.extend(args);
            args = with_settings;
        }
        let env = vec![
            ("CORVX_SESSION_ID".to_string(), id.clone()),
            ("CORVX_PROJECT".to_string(), project.id.clone()),
        ];
        let cwd = PathBuf::from(&project.path);

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
                let tmux = self.tmux.as_ref().ok_or_else(|| {
                    start_failed(&"tmux is not installed; install it, or use --backend pty")
                })?;
                drop(transcript); // tmux appends to it through pipe-pane
                let mut argv = vec![p.command.clone()];
                argv.extend(args);
                let pid = tmux
                    .start(
                        &id,
                        &cwd,
                        INITIAL_COLS,
                        INITIAL_ROWS,
                        &env,
                        &argv,
                        &transcript_path,
                    )
                    .map_err(|e| start_failed(&e))?;
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
                    .map_err(|e| start_failed(&e))?;
                let mut cmd = CommandBuilder::new(&p.command);
                cmd.args(&args);
                cmd.cwd(&cwd);
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
                let child = pair
                    .slave
                    .spawn_command(cmd)
                    .map_err(|e| start_failed(&e))?;
                drop(pair.slave);
                let pid = child.process_id();
                let killer = child.clone_killer();
                let reader = pair
                    .master
                    .try_clone_reader()
                    .map_err(|e| start_failed(&e))?;
                let writer = pair.master.take_writer().map_err(|e| start_failed(&e))?;
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

        let mut command = vec![p.command.clone()];
        command.extend(p.args.iter().cloned());
        let session = Session {
            id: id.clone(),
            project: project.id.clone(),
            command,
            runtime: p.runtime.clone(),
            backend: backend_kind.name().to_string(),
            state: SessionState::Running,
            pid,
            started_at: now_secs(),
            ended_at: None,
            exit_code: None,
            agent_state: AgentState::Unknown,
            agent_detail: None,
            agent_source: None,
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
        Ok(())
    }

    /// Applies one runtime hook event to the agent's status.
    fn hook(&mut self, core: &Core, p: &SessionHookParams) -> Result<(), ServiceError> {
        let entry = self.entries.get(&p.id).ok_or_else(|| not_found(&p.id))?;
        let Some((agent_state, detail)) = map_hook(&p.event, &p.payload) else {
            return Ok(());
        };
        let changed = {
            let mut st = lock(&entry.shared.status);
            // A session that has ended keeps its final state.
            if st.state != SessionState::Running {
                return Ok(());
            }
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
        if changed || agent_state == AgentState::Blocked {
            sync_attention(core, &entry.session, agent_state, detail.as_deref(), None)?;
        }
        Ok(())
    }

    /// Records how sessions that ended on their own finished, and sets their agent state.
    fn record_ends(&mut self, core: &Core) -> Result<(), ServiceError> {
        for (id, entry) in self.entries.iter_mut() {
            if entry.end_logged {
                continue;
            }
            let st = lock(&entry.shared.status).clone();
            if st.state == SessionState::Exited {
                entry.end_logged = true;
                {
                    let mut live = lock(&entry.shared.status);
                    // The process is gone, so a detail from its last turn no longer applies.
                    live.agent_detail = None;
                    live.agent_state =
                        final_agent_state(st.exit_code, live.agent_state, entry.one_shot);
                }
                entry.persist(core)?;
                let final_state = lock(&entry.shared.status).agent_state;
                sync_attention(core, &entry.session, final_state, None, st.exit_code)?;
                core.store().append_event(
                    "session.exited",
                    &json!({"id": id, "exit_code": st.exit_code}),
                )?;
            }
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
        AgentState::Done => ("done", 2, "finished · ready for review".to_string()),
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

/// Settings for Claude Code that register CORVX's hook command for the events we use.
fn claude_settings(hook_cmd: &str) -> Value {
    // A short timeout: if the daemon is slow or gone, the agent must not stall on our hook.
    let entry = json!([{"hooks": [{"type": "command", "command": hook_cmd, "timeout": 5}]}]);
    json!({
        "hooks": {
            "UserPromptSubmit": entry,
            "PreToolUse": entry,
            "PostToolUse": entry,
            "Notification": entry,
            "Stop": entry,
        }
    })
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
    }
}
