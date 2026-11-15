//! CORVX wire protocol, version 0.
//!
//! JSON-RPC 2.0 messages, one JSON object per line, over a local socket.
//! Clients (TUI, CLI, scripts, plugins) and the daemon share these types.
//! The protocol is public and versioned; see `PROTOCOL_VERSION`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped on any breaking change to methods or DTOs.
pub const PROTOCOL_VERSION: u32 = 0;
pub const JSONRPC_VERSION: &str = "2.0";

/// Method names. Keep these stable; add new ones rather than changing old ones.
pub mod methods {
    pub const DAEMON_INFO: &str = "daemon.info";
    pub const PROJECT_ADD: &str = "project.add";
    pub const PROJECT_LIST: &str = "project.list";
    pub const PROJECT_REMOVE: &str = "project.remove";
    pub const SESSION_START: &str = "session.start";
    pub const SESSION_LIST: &str = "session.list";
    pub const SESSION_OUTPUT: &str = "session.output";
    pub const SESSION_INPUT: &str = "session.input";
    pub const SESSION_STOP: &str = "session.stop";
    pub const SESSION_HOOK: &str = "session.hook";
    pub const SESSION_RESIZE: &str = "session.resize";
    pub const ATTENTION_LIST: &str = "attention.list";
    pub const ATTENTION_ACK: &str = "attention.ack";
    pub const ATTENTION_SNOOZE: &str = "attention.snooze";
    pub const ATTENTION_DISMISS: &str = "attention.dismiss";
    pub const ATTENTION_ASK: &str = "attention.ask";
    pub const ATTENTION_ANSWER: &str = "attention.answer";
    pub const ATTENTION_GET: &str = "attention.get";
    pub const SESSION_SCREEN: &str = "session.screen";
    pub const SESSION_REPORT: &str = "session.report";
    /// Asked by a waiting permission hook: has the human decided yet?
    pub const SESSION_PERMISSION: &str = "session.permission";
    /// Starts an interrupted session again, continuing its conversation.
    pub const SESSION_RESUME: &str = "session.resume";
    /// Approves or denies an agent's permission request from the inbox.
    pub const ATTENTION_DECIDE: &str = "attention.decide";
    /// Turns a session's autopilot on or off: CORVX approves its permission requests itself.
    pub const SESSION_AUTOPILOT: &str = "session.autopilot";
    /// The rules that send a permission request to the human, whoever would decide it.
    pub const RULES_LIST: &str = "rules.list";
    /// Adds a rule of your own, for one project or everywhere.
    pub const RULES_ADD: &str = "rules.add";
    /// Removes a rule you added.
    pub const RULES_REMOVE: &str = "rules.remove";
    /// Sets what a rule does (ask, allow, deny, off), for one project or everywhere.
    pub const RULES_SET: &str = "rules.set";
    /// Sets how much a project's agents may do without asking (see `autonomy`).
    pub const PROJECT_AUTONOMY: &str = "project.autonomy";
    /// Appends an entry to the journal of a session's project (`.corvx/journal/`).
    pub const PROJECT_JOURNAL: &str = "project.journal";
    /// A project's knowledge: its `.corvx/` files, recent journal entries and proposals.
    pub const KNOWLEDGE_READ: &str = "knowledge.read";
    /// Creates a project's missing knowledge files and points `CLAUDE.md` at them.
    pub const KNOWLEDGE_INIT: &str = "knowledge.init";
    /// Proposes an addition or edit to a project's knowledge; it goes to the human's inbox.
    pub const KNOWLEDGE_PROPOSE: &str = "knowledge.propose";
    /// Proposals, open or decided.
    pub const KNOWLEDGE_PROPOSALS: &str = "knowledge.proposals";
    /// Accepts a proposal, optionally edited: it is written to the file and committed.
    pub const KNOWLEDGE_ACCEPT: &str = "knowledge.accept";
    /// Rejects a proposal, with a reason; it is not proposed again.
    pub const KNOWLEDGE_REJECT: &str = "knowledge.reject";
    /// Commits a knowledge file the human edited, e.g. the brief.
    pub const KNOWLEDGE_COMMIT: &str = "knowledge.commit";
    /// Renames a project, moves it to another workspace, or scans it again.
    pub const PROJECT_UPDATE: &str = "project.update";
    /// Adds a goal to a project, or changes one's status; committed.
    pub const KNOWLEDGE_GOAL: &str = "knowledge.goal";
    /// Removes the worktrees (and branches) of a finished session and its agents, where
    /// their work is merged.
    pub const SESSION_CLEANUP: &str = "session.cleanup";
}

/// Error codes. JSON-RPC reserved range plus CORVX codes from 1000.
pub mod codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL: i64 = -32603;

    pub const PROJECT_NOT_FOUND: i64 = 1001;
    pub const PROJECT_EXISTS: i64 = 1002;
    pub const PATH_INVALID: i64 = 1003;
    pub const SESSION_NOT_FOUND: i64 = 1004;
    pub const SESSION_NOT_RUNNING: i64 = 1005;
    pub const SESSION_START_FAILED: i64 = 1006;
    pub const ATTENTION_NOT_FOUND: i64 = 1007;
    pub const DEPTH_LIMIT: i64 = 1008;
    pub const WORKTREE_FAILED: i64 = 1009;
    /// The agent is no longer waiting on this permission request.
    pub const APPROVAL_GONE: i64 = 1010;
    /// The session cannot be resumed (still running, not an agent, or no conversation found).
    pub const NOT_RESUMABLE: i64 = 1011;
    /// An orchestrator acted on a session it does not supervise.
    pub const NOT_SUPERVISOR: i64 = 1012;
    pub const RULE_NOT_FOUND: i64 = 1013;
    /// A rule's pattern is not a valid regular expression, or its kind is unknown.
    pub const RULE_INVALID: i64 = 1014;
    pub const PROPOSAL_NOT_FOUND: i64 = 1015;
    /// A proposal is incomplete, names an unknown file, or repeats a rejected one.
    pub const PROPOSAL_INVALID: i64 = 1016;
    /// The text a proposal would replace is no longer in the file.
    pub const KNOWLEDGE_CONFLICT: i64 = 1017;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Request {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    pub fn new(id: u64, method: &str, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            method: method.to_string(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Response {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: u64, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: u64, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonInfo {
    pub name: String,
    pub version: String,
    pub protocol_version: u32,
    pub pid: u32,
    pub data_dir: String,
    /// When the daemon started (Unix seconds); 0 from a daemon that does not say.
    #[serde(default)]
    pub started_at: i64,
}

/// A registered project. Stack and commands come from the L1 scan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    pub workspace: String,
    pub added_at: i64,
    pub stack: Vec<String>,
    pub test_command: Option<String>,
    pub build_command: Option<String>,
    pub branch: Option<String>,
    /// How much its agents may do without asking: one of `autonomy::ALL`.
    #[serde(default = "guided")]
    pub autonomy: String,
}

/// Autonomy levels: a dial from "you decide everything" to "agents decide almost everything".
/// A level sets defaults; CORVX rules you set yourself always win.
pub mod autonomy {
    /// Every permission request comes to you. Orchestrators cannot decide them or turn on
    /// autopilot.
    pub const SUPERVISED: &str = "supervised";
    /// An orchestrator decides its agents' requests; built-in rules ask you. The default.
    pub const GUIDED: &str = "guided";
    /// Requests are approved without asking, except what rules send you (pushes, publishing,
    /// credentials, deleting directories, writing outside the worktree, sudo).
    pub const AUTONOMOUS: &str = "autonomous";
    /// Everything is approved without asking, except changes to CORVX's own rules and what
    /// you set to ask or deny.
    pub const FULL: &str = "full";
    pub const ALL: &[&str] = &[SUPERVISED, GUIDED, AUTONOMOUS, FULL];
}

fn guided() -> String {
    autonomy::GUIDED.into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectAutonomyParams {
    pub id: String,
    /// One of `autonomy::ALL`.
    pub level: String,
}

/// Changes what is given; the id stays, since sessions, rules and proposals refer to it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ProjectUpdateParams {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    /// Scan the project again: stack, test and build commands, branch.
    #[serde(default)]
    pub rescan: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SessionCleanup {
    /// Worktrees removed, with their branches.
    pub removed: Vec<String>,
    /// Worktrees kept, each with why (not merged, uncommitted changes, still running).
    pub kept: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectJournalParams {
    /// The session writing (its project gets the entry).
    pub session: String,
    pub text: String,
    /// A heading for the entry, e.g. "Done".
    #[serde(default)]
    pub title: Option<String>,
    /// Commit the entry's file once it is written (at the end of a mission).
    #[serde(default)]
    pub commit: bool,
}

/// The knowledge files an agent can propose changes to. `map.md` is generated by CORVX.
pub mod knowledge_files {
    pub const BRIEF: &str = "brief";
    /// Where the project is heading (goals with a status) and what guides the work.
    pub const GOALS: &str = "goals";
    pub const DECISIONS: &str = "decisions";
    pub const CONVENTIONS: &str = "conventions";
    pub const ALL: [&str; 4] = [BRIEF, GOALS, DECISIONS, CONVENTIONS];
}

/// Statuses a goal can have in `goals.md`.
pub mod goal_status {
    /// Being worked towards now.
    pub const ACTIVE: &str = "active";
    /// Planned, after the active ones.
    pub const NEXT: &str = "next";
    pub const DONE: &str = "done";
    pub const DROPPED: &str = "dropped";
    pub const ALL: [&str; 4] = [ACTIVE, NEXT, DONE, DROPPED];
}

/// One goal from a project's `goals.md`: a `### Title · status` heading and the text below it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Goal {
    pub title: String,
    /// One of `goal_status::ALL`.
    pub status: String,
    /// What done looks like, and why it matters.
    #[serde(default)]
    pub detail: String,
}

/// Adds a goal to a project's `goals.md`, or changes the status (and detail) of one there.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct KnowledgeGoalParams {
    pub project: String,
    pub title: String,
    /// Default: active for a new goal, unchanged for an existing one.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct KnowledgeFile {
    /// "brief", "goals", "decisions", "conventions" or "map".
    pub name: String,
    /// Its path, relative to the project: `.corvx/<name>.md`.
    pub path: String,
    pub exists: bool,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct JournalEntry {
    /// Its path, relative to the project.
    pub path: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Proposal {
    /// The inbox item's id.
    pub id: String,
    pub project: String,
    /// One of `knowledge_files::ALL`.
    pub file: String,
    pub title: String,
    /// Markdown to add, or to put in place of `replaces`.
    pub text: String,
    /// For an edit: the exact text in the file that `text` replaces.
    #[serde(default)]
    pub replaces: Option<String>,
    /// Why: which mission, review finding or correction it comes from.
    #[serde(default)]
    pub evidence: Option<String>,
    /// The session that proposed it.
    pub by: String,
    /// "open", "accepted" or "rejected".
    pub status: String,
    /// Why it was rejected.
    #[serde(default)]
    pub reason: Option<String>,
    pub created_at: i64,
    #[serde(default)]
    pub decided_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Knowledge {
    pub project: String,
    pub path: String,
    /// brief, goals, decisions, conventions and map, in that order. The map is generated fresh.
    pub files: Vec<KnowledgeFile>,
    /// The goals in `goals.md`, in file order.
    #[serde(default)]
    pub goals: Vec<Goal>,
    /// The guideposts in `goals.md`: principles every mission follows.
    #[serde(default)]
    pub guideposts: Vec<String>,
    /// The most recent journal entries, newest first.
    pub journal: Vec<JournalEntry>,
    /// How many journal entries there are in all.
    pub journal_count: usize,
    /// Proposals not decided yet.
    pub open: Vec<Proposal>,
    /// Recently rejected proposals, with the reason: do not propose them again.
    pub rejected: Vec<Proposal>,
    /// Whether the project's `CLAUDE.md` points agents at the knowledge files.
    pub wired: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct KnowledgeReadParams {
    pub project: String,
    /// How many journal entries to include in full (default 2).
    #[serde(default)]
    pub journal: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct KnowledgeInitParams {
    pub project: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct KnowledgeCommitParams {
    pub project: String,
    /// One of `knowledge_files::ALL`.
    pub file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct KnowledgeInit {
    /// Files created or changed, relative to the project.
    pub written: Vec<String>,
    /// Whether they were committed, and if not, why.
    pub committed: bool,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct KnowledgeProposeParams {
    /// The proposing session; the proposal is for its project unless `project` is given.
    pub session: String,
    #[serde(default)]
    pub project: Option<String>,
    pub file: String,
    pub title: String,
    pub text: String,
    #[serde(default)]
    pub replaces: Option<String>,
    #[serde(default)]
    pub evidence: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct KnowledgeProposalsParams {
    #[serde(default)]
    pub project: Option<String>,
    /// Include decided proposals too.
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ProposalList {
    pub proposals: Vec<Proposal>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct KnowledgeAcceptParams {
    pub id: String,
    /// The text as edited by the human, in place of the proposed text.
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct KnowledgeAccepted {
    /// The file written, relative to the project.
    pub file: String,
    pub committed: bool,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct KnowledgeRejectParams {
    pub id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectAddParams {
    pub path: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectRemoveParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ProjectList {
    pub projects: Vec<Project>,
}

/// Lifecycle of a session's process. Generic states; adapters map runtime-specific ones onto these.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Running,
    /// The process exited on its own; see `exit_code`.
    Exited,
    /// Stopped on request via `session.stop`.
    Stopped,
    /// Ended from outside: its terminal disappeared, e.g. because the machine restarted.
    /// An agent session that was lost can be resumed.
    Lost,
}

/// What the agent is doing, as the status model in CORVX_SPEC.md section 8.2 defines it.
/// Reported by the runtime's own signals (hooks) when available; otherwise `unknown`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Unknown,
    Working,
    /// Needs a decision now: a permission request or a question.
    Blocked,
    /// Finished its turn and wants the next instruction.
    Waiting,
    /// The task is complete; the result is ready for review.
    Done,
    Idle,
    Failed,
    Stopped,
}

/// A process running in a pseudo-terminal, attached to a project.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub id: String,
    pub project: String,
    pub command: Vec<String>,
    /// Runtime adapter, e.g. "claude". None for a plain process.
    pub runtime: Option<String>,
    /// What owns the session's terminal: "tmux" (durable) or "pty" (ends with the daemon).
    #[serde(default)]
    pub backend: String,
    /// The session this one answers to (an orchestrator), or None when it answers to you.
    #[serde(default)]
    pub supervisor: Option<String>,
    /// The git worktree the session works in, when it has its own.
    #[serde(default)]
    pub worktree: Option<String>,
    /// A readable label, e.g. "orchestrator · Build Snip". Shown in session lists.
    #[serde(default)]
    pub label: Option<String>,
    /// Whether the process is running, has exited, or was stopped.
    pub state: SessionState,
    pub pid: Option<u32>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub exit_code: Option<i32>,
    /// What the agent is doing. Separate from the process state above.
    pub agent_state: AgentState,
    /// Short human-readable detail, e.g. the permission request or the tool in use.
    pub agent_detail: Option<String>,
    /// Where `agent_state` came from: "hooks", or None when unknown.
    pub agent_source: Option<String>,
    /// The runtime's own id for the conversation, e.g. Claude Code's session id. Used to resume.
    #[serde(default)]
    pub runtime_session: Option<String>,
    /// CORVX approves the session's permission requests itself, without asking anyone.
    #[serde(default)]
    pub autopilot: bool,
    /// For an orchestrator: the model its agents get unless it picks another.
    #[serde(default)]
    pub agent_model: Option<String>,
    /// For an orchestrator: its agents' permission requests go to you instead of to it.
    #[serde(default)]
    pub human_approves_agents: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionStartParams {
    pub project: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Runtime adapter to use, e.g. "claude". Omit for a plain process.
    #[serde(default)]
    pub runtime: Option<String>,
    /// "tmux" or "pty". Omit for the node's default (tmux when it is installed).
    #[serde(default)]
    pub backend: Option<String>,
    /// The session this one answers to. Its waiting, done and failed states go to the
    /// supervisor instead of your inbox. Omit when you are the supervisor.
    #[serde(default)]
    pub supervisor: Option<String>,
    /// Work in a new git worktree on its own branch, so parallel agents don't collide.
    #[serde(default)]
    pub worktree: bool,
    /// A readable label for session lists, e.g. "agent · Backend API".
    #[serde(default)]
    pub label: Option<String>,
    /// Approve the session's permission requests without asking anyone.
    #[serde(default)]
    pub autopilot: bool,
    /// For an orchestrator: the model its agents get unless it picks another.
    #[serde(default)]
    pub agent_model: Option<String>,
    /// For an orchestrator: send its agents' permission requests to you instead of to it.
    #[serde(default)]
    pub human_approves_agents: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionAutopilotParams {
    pub id: String,
    pub on: bool,
    /// The orchestrator asking, which must supervise the session. None for a human.
    #[serde(default)]
    pub by: Option<String>,
}

/// A rule CORVX applies to an agent's tool calls, whoever would otherwise decide them (its
/// orchestrator, autopilot, or Claude Code's own settings).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rule {
    pub id: String,
    /// "command": a regular expression on shell commands. "path": a regular expression on the
    /// paths file tools use, and on shell commands. "outside": writes outside the agent's
    /// working directory (no pattern).
    pub kind: String,
    #[serde(default)]
    pub pattern: String,
    /// What it is about, as shown in the inbox.
    pub reason: String,
    /// Built into CORVX (can be set to off, not removed).
    #[serde(default)]
    pub builtin: bool,
    /// The project a rule you added applies to; None for every project.
    #[serde(default)]
    pub project: Option<String>,
    /// What happens to a matching request (for the project asked about, if any): one of
    /// `rule_actions`.
    #[serde(default = "ask")]
    pub action: String,
    /// Where the action comes from: "project" (set for this project), "everywhere" (set for
    /// every project), "level" (the project's autonomy level), or "default" (the rule's own).
    #[serde(default)]
    pub set_in: String,
}

/// What a rule does with a matching request.
pub mod rule_actions {
    /// The human decides it.
    pub const ASK: &str = "ask";
    /// Approved at once, without asking anyone; logged.
    pub const ALLOW: &str = "allow";
    /// Refused at once; the agent is told the rule's reason.
    pub const DENY: &str = "deny";
    /// The rule does not apply.
    pub const OFF: &str = "off";
    pub const ALL: &[&str] = &[ASK, ALLOW, DENY, OFF];
}

fn ask() -> String {
    rule_actions::ASK.into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RuleList {
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RulesListParams {
    /// Show the rules as they apply to this project. None: as set for every project, with
    /// every rule you added.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RulesAddParams {
    /// None for every project.
    #[serde(default)]
    pub project: Option<String>,
    /// "command" or "path".
    pub kind: String,
    pub pattern: String,
    pub reason: String,
    /// "ask" (default), "allow" or "deny".
    #[serde(default = "ask")]
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RulesRemoveParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RulesSetParams {
    pub id: String,
    /// None for every project.
    #[serde(default)]
    pub project: Option<String>,
    /// One of `rule_actions`, or "default" to drop this setting (a project then follows
    /// the setting for every project, and that follows the rule's own).
    pub action: String,
}

/// A question from an agent to a human. It appears in the inbox until answered.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttentionAskParams {
    /// The session asking.
    pub session_id: String,
    pub question: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttentionAnswerParams {
    pub id: String,
    pub answer: String,
}

/// A decision on an agent's permission request (an inbox item of kind "approval").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttentionDecideParams {
    pub id: String,
    /// "allow" (this once), "always" (and don't ask again for this kind of request), "deny",
    /// or, for an orchestrator, "escalate" (pass it to the human).
    pub decision: String,
    /// For "deny": tells the agent why, or what to do instead. For "escalate": why the human
    /// should decide.
    #[serde(default)]
    pub message: Option<String>,
    /// The orchestrator deciding, which must supervise the session. None for a human.
    #[serde(default)]
    pub by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionPermissionParams {
    /// The session whose hook is waiting.
    pub id: String,
    /// The approval item the hook is waiting on.
    pub item: String,
}

/// Where a permission request stands.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PermissionStatus {
    /// "pending", "decided", or "gone" (answered in the session, dismissed, or the agent moved on).
    pub status: String,
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionResumeParams {
    pub id: String,
}

/// The sessions a resume started: the one asked for, and any lost agents it supervised.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SessionResumed {
    pub sessions: Vec<Session>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionScreenParams {
    pub id: String,
    /// Lines of history above the screen to include (tmux sessions; others return what
    /// they have).
    #[serde(default)]
    pub history: u32,
    /// Keep colours and styles as terminal escape codes, with the screen's rows as they are
    /// drawn (not joined). For showing the screen, not for reading it.
    #[serde(default)]
    pub ansi: bool,
}

/// The visible screen of a session, as plain text.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionScreen {
    pub text: String,
    pub state: SessionState,
    /// The program asked for mouse input (e.g. Claude Code, which then scrolls its own
    /// history with the wheel). Reported for screens read with `ansi`.
    #[serde(default)]
    pub mouse: bool,
}

/// An agent reporting on its own task, e.g. through the `report_done` tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionReportParams {
    pub id: String,
    /// "done" or "failed".
    pub outcome: String,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionResizeParams {
    pub id: String,
    pub cols: u16,
    pub rows: u16,
}

/// An event from a runtime's hook system, forwarded by `corvx session hook`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionHookParams {
    pub id: String,
    /// Hook event name, e.g. "Notification" or "Stop".
    pub event: String,
    /// The hook's JSON input, as the runtime sent it.
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SessionList {
    pub sessions: Vec<Session>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SessionListParams {
    /// Also sessions that ended before this daemon started (from the record), not only the
    /// ones it runs or recently lost.
    #[serde(default)]
    pub all: bool,
    /// Only this project's sessions.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionIdParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionOutputParams {
    pub id: String,
    /// Byte offset to read from. Start at 0; then pass the `next` value from the previous reply.
    #[serde(default)]
    pub since: u64,
}

/// Output bytes are base64 because terminal output contains escape sequences
/// and may not be valid UTF-8.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionOutput {
    pub data: String,
    pub next: u64,
    /// True if older output was dropped from the scrollback before `since`.
    pub truncated: bool,
    pub state: SessionState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionInputParams {
    pub id: String,
    pub data: String,
}

/// Something that needs the human. Created when an agent is blocked, done or failed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttentionItem {
    pub id: String,
    /// What raised it: an agent state ("blocked", "waiting", "done", "failed"), "question",
    /// "approval" (a permission request you can decide from the inbox), "suggestion" (CORVX
    /// offers to allow automatically what you keep approving; deciding it allow accepts), or
    /// "proposal" (an agent proposes a change to the project's knowledge; see `Proposal`).
    pub kind: String,
    /// 3 = act now (blocked, failed); 2 = review (done); 1 = your move (waiting). An approval
    /// at severity 1 is its agent's orchestrator's to decide; you may still decide it.
    pub severity: u8,
    pub project: String,
    pub session_id: String,
    pub summary: String,
    /// "open", "snoozed", "answered", "resolved" or "dismissed".
    pub status: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub snooze_until: Option<i64>,
    /// For questions: the answer, once given. For approvals: the decision.
    #[serde(default)]
    pub answer: Option<String>,
    /// For an approval that needs you: why (the orchestrator escalated it, or a rule matched).
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AttentionList {
    pub items: Vec<AttentionItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AttentionListParams {
    /// Include items that are snoozed and not yet due.
    #[serde(default)]
    pub include_snoozed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttentionIdParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttentionSnoozeParams {
    pub id: String,
    pub minutes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttentionDismissParams {
    pub id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_roundtrip() {
        let req = Request::new(7, methods::PROJECT_ADD, json!({"path": "./app"}));
        let line = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&line).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn error_response_omits_result() {
        let resp = Response::err(3, codes::PROJECT_NOT_FOUND, "no such project");
        let line = serde_json::to_string(&resp).unwrap();
        assert!(!line.contains("\"result\""));
        assert!(line.contains("\"code\":1001"));
    }

    #[test]
    fn missing_params_default_to_null() {
        let req: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"method":"daemon.info"}"#).unwrap();
        assert_eq!(req.params, Value::Null);
    }
}
