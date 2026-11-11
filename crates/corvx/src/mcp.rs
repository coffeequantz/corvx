//! `corvx mcp`: CORVX's tools for agents, as an MCP server over stdio.
//!
//! Every Claude Code session CORVX starts gets this server, so any agent can start and
//! supervise other agents, ask you a question, or report its task done. An orchestrator
//! is simply a session whose role prompt tells it to work this way.
//!
//! The server runs inside the agent's session; `CORVX_SESSION_ID` tells it which session
//! is calling. Sessions it starts are supervised by that session.
//!
//! Tool calls return quickly. `wait_for` blocks for at most about 50 seconds and then
//! reports that nothing happened yet, so the agent calls it again. Answers to questions
//! arrive through `wait_for` too.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use corvx_client::{ensure_daemon, Client};
use corvx_proto::{
    methods, AgentState, AttentionAskParams, AttentionDecideParams, AttentionItem, AttentionList,
    ProjectJournalParams, ProjectList, Session, SessionAutopilotParams, SessionList,
    SessionReportParams, SessionScreen, SessionStartParams, SessionState,
};
use serde_json::{json, Value};

const MAX_WAIT: Duration = Duration::from_secs(50);
const POLL: Duration = Duration::from_millis(500);

/// What `wait_for` remembers about a supervised agent between calls.
struct Watch {
    /// Its state when we last gave it work, so an unchanged state is not reported again.
    at_arm: AgentState,
    /// Whether it has been seen working (or otherwise not needing anyone) since then.
    moved: bool,
    /// The last permission request reported, so each one is reported once.
    approval: Option<String>,
}

struct Server {
    data_dir: PathBuf,
    client: Option<Client>,
    /// The calling session, when the server runs inside one.
    me: Option<String>,
    watches: HashMap<String, Watch>,
    my_questions: Vec<String>,
    delivered_answers: HashSet<String>,
}

pub fn serve(data_dir: &Path) -> Result<()> {
    let mut server = Server {
        data_dir: data_dir.to_path_buf(),
        client: None,
        me: std::env::var("CORVX_SESSION_ID")
            .ok()
            .filter(|s| !s.is_empty()),
        watches: HashMap::new(),
        my_questions: Vec::new(),
        delivered_answers: HashSet::new(),
    };
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // Notifications have no id and get no reply.
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let reply = match server.dispatch(method, &params) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id,
                             "error": {"code": -32601, "message": e.to_string()}}),
        };
        writeln!(stdout, "{reply}")?;
        stdout.flush()?;
    }
    Ok(())
}

impl Server {
    fn dispatch(&mut self, method: &str, params: &Value) -> Result<Value> {
        match method {
            "initialize" => Ok(json!({
                "protocolVersion": params.get("protocolVersion").cloned()
                    .unwrap_or_else(|| json!("2025-06-18")),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "corvx", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "CORVX runs and supervises agent sessions. Use start_agent to delegate, \
                                 wait_for to learn when an agent needs you, ask_human when only the \
                                 human can decide, and report_done when your task is complete."
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                Ok(match self.call_tool(name, &args) {
                    Ok(v) => {
                        json!({"content": [{"type": "text", "text": pretty(&v)}], "isError": false})
                    }
                    Err(e) => {
                        json!({"content": [{"type": "text", "text": e.to_string()}], "isError": true})
                    }
                })
            }
            other => Err(anyhow!("method not found: {other}")),
        }
    }

    /// A daemon call; reconnects once if the daemon was restarted.
    fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        if self.client.is_none() {
            self.client = Some(ensure_daemon(&self.data_dir, None)?);
        }
        let first = self.client.as_mut().unwrap().call(method, &params);
        match first {
            Ok(v) => Ok(v),
            Err(e) if corvx_client::is_connection_error(&e) => {
                self.client = Some(ensure_daemon(&self.data_dir, None)?);
                self.client.as_mut().unwrap().call(method, &params)
            }
            Err(e) => Err(e),
        }
    }

    fn sessions(&mut self) -> Result<Vec<Session>> {
        let list: SessionList =
            serde_json::from_value(self.rpc(methods::SESSION_LIST, json!({}))?)?;
        Ok(list.sessions)
    }

    /// Sessions this caller supervises. Outside a session (e.g. you testing the server),
    /// that is every session you supervise directly.
    fn supervised(&mut self) -> Result<Vec<Session>> {
        let me = self.me.clone();
        Ok(self
            .sessions()?
            .into_iter()
            .filter(|s| s.supervisor == me && Some(&s.id) != me.as_ref())
            .collect())
    }

    fn arm(&mut self, id: &str, state: AgentState) {
        let approval = self.watches.remove(id).and_then(|w| w.approval);
        self.watches.insert(
            id.to_string(),
            Watch {
                at_arm: state,
                moved: false,
                approval,
            },
        );
    }

    /// The open permission requests, by session.
    fn approvals(&mut self) -> Result<HashMap<String, AttentionItem>> {
        let list: AttentionList = serde_json::from_value(
            self.rpc(methods::ATTENTION_LIST, json!({"include_snoozed": true}))?,
        )?;
        Ok(list
            .items
            .into_iter()
            .filter(|i| i.kind == "approval")
            .map(|i| (i.session_id.clone(), i))
            .collect())
    }

    /// The session's open permission request, if it is one this caller supervises.
    fn approval_of(&mut self, id: &str) -> Result<AttentionItem> {
        if !self.supervised()?.iter().any(|s| s.id == id) {
            return Err(anyhow!("'{id}' is not an agent you supervise"));
        }
        self.approvals()?
            .remove(id)
            .ok_or_else(|| anyhow!("'{id}' is not waiting for a permission decision"))
    }

    fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value> {
        let s = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_string);
        match name {
            "list_projects" => {
                let list: ProjectList =
                    serde_json::from_value(self.rpc(methods::PROJECT_LIST, json!({}))?)?;
                Ok(json!(list
                    .projects
                    .iter()
                    .map(|p| json!({
                        "id": p.id, "path": p.path, "stack": p.stack,
                        "test_command": p.test_command, "branch": p.branch
                    }))
                    .collect::<Vec<_>>()))
            }
            "start_agent" => {
                let project = s("project").ok_or_else(|| anyhow!("project is required"))?;
                let prompt = s("prompt").ok_or_else(|| anyhow!("prompt is required"))?;
                let label = format!(
                    "agent · {}",
                    crate::short_label(&s("name").unwrap_or_else(|| prompt.clone()))
                );
                // The orchestrator's default for its agents, else CORVX's.
                let default_model = match &self.me {
                    Some(me) => {
                        let me = me.clone();
                        self.sessions()?
                            .into_iter()
                            .find(|x| x.id == me)
                            .and_then(|x| x.agent_model)
                    }
                    None => None,
                }
                .unwrap_or_else(|| crate::DEFAULT_AGENT_MODEL.to_string());
                let model = s("model").unwrap_or(default_model);
                let claude_args = vec![
                    "--permission-mode".to_string(),
                    s("permission_mode").unwrap_or_else(|| "acceptEdits".into()),
                    "--model".into(),
                    model.clone(),
                    prompt,
                ];
                let autopilot = args
                    .get("autopilot")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let params = SessionStartParams {
                    project,
                    // `CORVX_AGENT_COMMAND` replaces the agent program, e.g. with a test stand-in.
                    command: std::env::var("CORVX_AGENT_COMMAND")
                        .unwrap_or_else(|_| "claude".into()),
                    args: claude_args,
                    runtime: Some("claude".into()),
                    backend: None,
                    supervisor: self.me.clone(),
                    worktree: args
                        .get("worktree")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    label: Some(label),
                    autopilot,
                    agent_model: None,
                    human_approves_agents: false,
                };
                let started: Session = serde_json::from_value(
                    self.rpc(methods::SESSION_START, serde_json::to_value(params)?)?,
                )?;
                self.arm(&started.id, AgentState::Unknown);
                Ok(
                    json!({"id": started.id, "worktree": started.worktree, "model": model,
                          "autopilot": autopilot,
                          "note": "Use wait_for to learn when it needs you."}),
                )
            }
            "list_agents" => {
                let list = self.supervised()?;
                Ok(json!(list.iter().map(describe).collect::<Vec<_>>()))
            }
            "read_screen" => {
                let id = s("id").ok_or_else(|| anyhow!("id is required"))?;
                let screen: SessionScreen =
                    serde_json::from_value(self.rpc(methods::SESSION_SCREEN, json!({"id": id}))?)?;
                Ok(json!({"state": screen.state, "screen": screen.text.trim_end()}))
            }
            "send_message" => {
                let id = s("id").ok_or_else(|| anyhow!("id is required"))?;
                let text = s("text").ok_or_else(|| anyhow!("text is required"))?;
                let state = self
                    .sessions()?
                    .into_iter()
                    .find(|x| x.id == id)
                    .map(|x| x.agent_state)
                    .unwrap_or(AgentState::Unknown);
                self.rpc(
                    methods::SESSION_INPUT,
                    json!({"id": id, "data": format!("{text}\r")}),
                )?;
                self.arm(&id, state);
                Ok(json!({"sent": true}))
            }
            "stop_agent" => {
                let id = s("id").ok_or_else(|| anyhow!("id is required"))?;
                self.rpc(methods::SESSION_STOP, json!({"id": id}))?;
                self.watches.remove(&id);
                Ok(json!({"stopped": id}))
            }
            "wait_for" => self.wait_for(args),
            "decide_approval" => {
                let me = self
                    .me
                    .clone()
                    .ok_or_else(|| anyhow!("decide_approval only works inside a CORVX session"))?;
                let id = s("id").ok_or_else(|| anyhow!("id is required"))?;
                let decision = s("decision").ok_or_else(|| anyhow!("decision is required"))?;
                let item = self.approval_of(&id)?;
                if item.severity >= 3 {
                    return Err(anyhow!(
                        "this request is the human's to decide ({}); wait for it",
                        item.note
                            .as_deref()
                            .unwrap_or("escalated, or your agents' approvals go to the human")
                    ));
                }
                let params = AttentionDecideParams {
                    id: item.id.clone(),
                    decision: decision.clone(),
                    message: s("message"),
                    by: Some(me),
                };
                self.rpc(methods::ATTENTION_DECIDE, serde_json::to_value(params)?)?;
                // The agent continues (or the human takes it); report what it does next.
                self.arm(&id, AgentState::Blocked);
                let note = if decision == "escalate" {
                    "Passed to the human. wait_for reports the agent again once it moves on."
                } else {
                    "Decided. wait_for reports the agent again when it needs you."
                };
                Ok(json!({"decided": decision, "request": item.summary, "note": note}))
            }
            "set_autopilot" => {
                let me = self
                    .me
                    .clone()
                    .ok_or_else(|| anyhow!("set_autopilot only works inside a CORVX session"))?;
                let id = s("id").ok_or_else(|| anyhow!("id is required"))?;
                let on = args
                    .get("on")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| anyhow!("on is required (true or false)"))?;
                let params = SessionAutopilotParams {
                    id: id.clone(),
                    on,
                    by: Some(me),
                };
                self.rpc(methods::SESSION_AUTOPILOT, serde_json::to_value(params)?)?;
                Ok(json!({"id": id, "autopilot": on}))
            }
            "ask_human" => {
                let me = self
                    .me
                    .clone()
                    .ok_or_else(|| anyhow!("ask_human only works inside a CORVX session"))?;
                let question = s("question").ok_or_else(|| anyhow!("question is required"))?;
                let params = AttentionAskParams {
                    session_id: me,
                    question,
                };
                let v = self.rpc(methods::ATTENTION_ASK, serde_json::to_value(params)?)?;
                let qid = v["id"].as_str().unwrap_or_default().to_string();
                self.my_questions.push(qid.clone());
                Ok(json!({"question_id": qid,
                          "note": "The human answers in the CORVX inbox. The answer arrives through wait_for."}))
            }
            "report_done" => {
                let me = self
                    .me
                    .clone()
                    .ok_or_else(|| anyhow!("report_done only works inside a CORVX session"))?;
                let summary = s("summary").ok_or_else(|| anyhow!("summary is required"))?;
                let outcome = s("outcome").unwrap_or_else(|| "done".into());
                // An orchestrator's report closes its mission's journal entry.
                let top_level = self
                    .sessions()?
                    .iter()
                    .any(|x| x.id == me && x.supervisor.is_none());
                let journal = if top_level {
                    let title = if outcome == "failed" {
                        "Failed"
                    } else {
                        "Done"
                    };
                    let params = ProjectJournalParams {
                        session: me.clone(),
                        text: summary.clone(),
                        title: Some(title.into()),
                    };
                    self.rpc(methods::PROJECT_JOURNAL, serde_json::to_value(params)?)
                        .ok()
                        .and_then(|v| v["file"].as_str().map(str::to_string))
                } else {
                    None
                };
                let params = SessionReportParams {
                    id: me,
                    outcome,
                    summary,
                };
                self.rpc(methods::SESSION_REPORT, serde_json::to_value(params)?)?;
                Ok(json!({"reported": true, "journal": journal}))
            }
            "journal" => {
                let me = self
                    .me
                    .clone()
                    .ok_or_else(|| anyhow!("journal only works inside a CORVX session"))?;
                let text = s("text").ok_or_else(|| anyhow!("text is required"))?;
                let params = ProjectJournalParams {
                    session: me,
                    text,
                    title: s("title"),
                };
                let v = self.rpc(methods::PROJECT_JOURNAL, serde_json::to_value(params)?)?;
                Ok(json!({"file": v["file"]}))
            }
            other => Err(anyhow!("unknown tool: {other}")),
        }
    }

    /// Blocks until a supervised agent needs attention or a question is answered, for at
    /// most about 50 seconds. Returns the events, or none if nothing happened yet.
    fn wait_for(&mut self, args: &Value) -> Result<Value> {
        let limit = args
            .get("timeout_seconds")
            .and_then(Value::as_u64)
            .map(Duration::from_secs)
            .unwrap_or(MAX_WAIT)
            .min(MAX_WAIT);
        let deadline = Instant::now() + limit;
        loop {
            let mut events = Vec::new();
            let approvals = self.approvals()?;

            for session in self.supervised()? {
                let needs = needs_attention(&session);
                let watch = self.watches.entry(session.id.clone()).or_insert(Watch {
                    at_arm: AgentState::Unknown,
                    moved: false,
                    approval: None,
                });
                if let Some(item) = approvals.get(&session.id) {
                    // Each permission request is reported once, even if the agent never
                    // looked unblocked in between.
                    if watch.approval.as_deref() != Some(item.id.as_str()) {
                        watch.approval = Some(item.id.clone());
                        watch.at_arm = AgentState::Blocked;
                        watch.moved = false;
                        events.push(approval_event(&session, item));
                    }
                    continue;
                }
                if !needs {
                    watch.moved = true;
                    continue;
                }
                if watch.moved || session.agent_state != watch.at_arm {
                    events.push(json!({"type": "agent", "agent": describe(&session)}));
                    // Report once; the next instruction re-arms it.
                    watch.at_arm = session.agent_state;
                    watch.moved = false;
                }
            }

            for qid in self.my_questions.clone() {
                if self.delivered_answers.contains(&qid) {
                    continue;
                }
                let item: AttentionItem =
                    serde_json::from_value(self.rpc(methods::ATTENTION_GET, json!({"id": qid}))?)?;
                match item.status.as_str() {
                    "answered" => {
                        events.push(json!({"type": "answer", "question_id": qid,
                                           "question": item.summary, "answer": item.answer}));
                        self.delivered_answers.insert(qid);
                    }
                    "dismissed" => {
                        events.push(json!({"type": "answer", "question_id": qid,
                                           "question": item.summary, "answer": null,
                                           "note": "The human dismissed this question."}));
                        self.delivered_answers.insert(qid);
                    }
                    _ => {}
                }
            }

            if !events.is_empty() {
                return Ok(json!({"events": events}));
            }
            if Instant::now() >= deadline {
                let pending = self
                    .supervised()?
                    .iter()
                    .filter(|s| !needs_attention(s))
                    .count();
                return Ok(json!({"events": [],
                                 "note": format!("Nothing yet ({pending} agent(s) still working). Call wait_for again.")}));
            }
            std::thread::sleep(POLL);
        }
    }
}

/// Whether a supervised agent wants its supervisor: it needs a decision, wants its next
/// instruction, finished, failed, or ended.
fn needs_attention(s: &Session) -> bool {
    s.state != SessionState::Running
        || matches!(
            s.agent_state,
            AgentState::Blocked | AgentState::Waiting | AgentState::Done | AgentState::Failed
        )
}

/// An agent's permission request, for its orchestrator.
fn approval_event(session: &Session, item: &AttentionItem) -> Value {
    let yours = item.severity < 3;
    json!({
        "type": "approval",
        "agent": describe(session),
        "request": item.summary,
        "decider": if yours { "you" } else { "human" },
        "note": if yours {
            "Decide it with decide_approval: allow, always (and don't ask again for this kind of request), deny (with a message), or escalate to the human."
        } else {
            "The human decides this one. Wait for the agent to move on."
        },
    })
}

/// The model a Claude session was started with, if one was given.
fn model_of(s: &Session) -> Option<&str> {
    s.command
        .iter()
        .position(|a| a == "--model")
        .and_then(|i| s.command.get(i + 1))
        .map(String::as_str)
}

fn describe(s: &Session) -> Value {
    json!({
        "id": s.id,
        "project": s.project,
        "model": model_of(s),
        "autopilot": s.autopilot,
        "running": s.state == SessionState::Running,
        "state": if s.state == SessionState::Running { s.agent_state } else {
            match s.state { SessionState::Exited => AgentState::Done, _ => AgentState::Stopped }
        },
        "detail": s.agent_detail,
        // Ended from outside (e.g. a restart); the human can resume it.
        "lost": s.state == SessionState::Lost,
        "worktree": s.worktree,
        "exit_code": s.exit_code,
    })
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

fn tools() -> Value {
    json!([
        {
            "name": "list_projects",
            "description": "List the projects CORVX knows, with their paths, stack and test command.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "start_agent",
            "description": "Start a coding agent (Claude Code) in a project with an initial prompt. By default it gets its own git worktree on a new branch, so several agents can work in parallel. You supervise it: use wait_for to learn when it needs you.",
            "inputSchema": {"type": "object", "properties": {
                "project": {"type": "string", "description": "Project id, from list_projects."},
                "name": {"type": "string", "description": "Short name shown to the human, e.g. backend, frontend, brand reviewer."},
                "prompt": {"type": "string", "description": "The task, with everything the agent needs to know. It cannot see your conversation."},
                "model": {"type": "string", "description": "Model for this agent: haiku for mechanical work, sonnet for most coding, opus for hard pieces. Default: the one you were told."},
                "autopilot": {"type": "boolean", "description": "Approve its permission requests without asking (default false). Only for routine work confined to its worktree."},
                "worktree": {"type": "boolean", "description": "Work in a new git worktree (default true). Requires a git repository with at least one commit."},
                "permission_mode": {"type": "string", "description": "Claude Code permission mode for the agent (default acceptEdits: it may edit files in its worktree; shell commands ask for permission, which comes to you)."}
            }, "required": ["project", "prompt"]}
        },
        {
            "name": "list_agents",
            "description": "The agents you supervise, with their state (working, waiting, blocked, done, failed, stopped), detail and worktree.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "read_screen",
            "description": "Read an agent's current screen as text, to see what it has done or is asking.",
            "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}
        },
        {
            "name": "send_message",
            "description": "Send an instruction or answer to an agent you supervise, as if typed and submitted.",
            "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}, "text": {"type": "string"}}, "required": ["id", "text"]}
        },
        {
            "name": "wait_for",
            "description": "Wait until an agent you supervise needs you (a permission request to decide, waiting, blocked, done, failed or ended), or a question you asked is answered. Returns within about 50 seconds; if events is empty, call it again.",
            "inputSchema": {"type": "object", "properties": {"timeout_seconds": {"type": "integer", "description": "At most 50."}}}
        },
        {
            "name": "decide_approval",
            "description": "Decide a permission request from an agent you supervise (wait_for reports them as approvals). allow: this once; always: and don't ask again for this kind of request; deny: refuse, with a message saying why or what to do instead; escalate: pass it to the human, with a message saying why.",
            "inputSchema": {"type": "object", "properties": {
                "id": {"type": "string", "description": "The agent's id."},
                "decision": {"type": "string", "enum": ["allow", "always", "deny", "escalate"]},
                "message": {"type": "string"}
            }, "required": ["id", "decision"]}
        },
        {
            "name": "set_autopilot",
            "description": "Turn autopilot on or off for an agent you supervise. With autopilot on, its permission requests are approved without asking you; each is still logged. Turning it on also approves the request it is waiting on.",
            "inputSchema": {"type": "object", "properties": {
                "id": {"type": "string"},
                "on": {"type": "boolean"}
            }, "required": ["id", "on"]}
        },
        {
            "name": "stop_agent",
            "description": "Stop an agent you supervise. Its worktree and branch are kept.",
            "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}
        },
        {
            "name": "ask_human",
            "description": "Ask the human a question that only they can decide. It appears in their inbox; the answer arrives through wait_for. Use sparingly.",
            "inputSchema": {"type": "object", "properties": {"question": {"type": "string"}}, "required": ["question"]}
        },
        {
            "name": "journal",
            "description": "Add an entry to this mission's journal, a Markdown file in the project's .corvx/journal/ that later missions read. At the end of a mission: the goal, what happened, what went wrong, what was learned.",
            "inputSchema": {"type": "object", "properties": {
                "text": {"type": "string", "description": "Markdown."},
                "title": {"type": "string", "description": "A heading, e.g. Mission or Decision."}
            }, "required": ["text"]}
        },
        {
            "name": "report_done",
            "description": "Report that your own task is complete (or failed), with a short summary for your supervisor. An orchestrator's summary is also added to the mission's journal.",
            "inputSchema": {"type": "object", "properties": {
                "summary": {"type": "string"},
                "outcome": {"type": "string", "enum": ["done", "failed"]}
            }, "required": ["summary"]}
        }
    ])
}
