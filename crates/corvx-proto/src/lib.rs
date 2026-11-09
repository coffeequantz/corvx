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
    /// The agent state that raised it: "blocked", "waiting", "done" or "failed".
    pub kind: String,
    /// 3 = act now (blocked, failed); 2 = review (done); 1 = your move (waiting).
    pub severity: u8,
    pub project: String,
    pub session_id: String,
    pub summary: String,
    /// "open", "snoozed", "resolved" or "dismissed".
    pub status: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub snooze_until: Option<i64>,
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
