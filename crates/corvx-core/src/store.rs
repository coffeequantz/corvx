//! SQLite persistence and the append-only event log.
//!
//! Schema versions are tracked with `PRAGMA user_version`. Add a migration
//! step for each new version; never edit a version that has shipped.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use corvx_proto::{AgentState, AttentionItem, Project, Proposal, Session, SessionState};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, StoreError>;

pub const SCHEMA_VERSION: i64 = 11;

const SCHEMA_V1: &str = "
CREATE TABLE projects (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    path         TEXT NOT NULL UNIQUE,
    workspace    TEXT NOT NULL,
    added_at     INTEGER NOT NULL,
    stack        TEXT NOT NULL,
    test_command TEXT,
    build_command TEXT,
    branch       TEXT
);
CREATE TABLE events (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    ts      INTEGER NOT NULL,
    kind    TEXT NOT NULL,
    payload TEXT NOT NULL
);
CREATE INDEX events_kind ON events(kind);
";

/// Version 2: the attention queue.
/// Version 6: the runtime's conversation id, so an interrupted agent can be resumed. Tmux
/// sessions that vanished without an exit status (the machine restarted) are marked lost.
const SCHEMA_V6: &str = "
ALTER TABLE sessions ADD COLUMN runtime_session TEXT;
UPDATE sessions SET state = 'lost' WHERE state = 'exited' AND exit_code IS NULL AND backend = 'tmux';
";

/// Version 7: autopilot, and an orchestrator's settings for its agents (default model, who
/// approves their permission requests).
const SCHEMA_V7: &str = "
ALTER TABLE sessions ADD COLUMN autopilot INTEGER NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN agent_model TEXT;
ALTER TABLE sessions ADD COLUMN human_approves_agents INTEGER NOT NULL DEFAULT 0;
";

/// Version 8: rules CORVX applies to agents' tool calls. `rules` holds the ones a human
/// added; `rule_settings` what a rule does in a project ('' for every project), overriding
/// its own action.
const SCHEMA_V8: &str = "
CREATE TABLE rules (
    id         TEXT PRIMARY KEY,
    project    TEXT,
    kind       TEXT NOT NULL,
    pattern    TEXT NOT NULL,
    reason     TEXT NOT NULL,
    action     TEXT NOT NULL DEFAULT 'ask',
    created_at INTEGER NOT NULL
);
CREATE TABLE rule_settings (
    rule    TEXT NOT NULL,
    project TEXT NOT NULL DEFAULT '',
    action  TEXT NOT NULL,
    PRIMARY KEY (rule, project)
);
";

/// Version 9: a project's autonomy level, and what the human keeps approving, so CORVX can
/// offer to allow it automatically (`trust`: one row per project and kind of request).
const SCHEMA_V9: &str = "
ALTER TABLE projects ADD COLUMN autonomy TEXT NOT NULL DEFAULT 'guided';
CREATE TABLE trust (
    project    TEXT NOT NULL,
    key        TEXT NOT NULL,
    approvals  INTEGER NOT NULL DEFAULT 0,
    state      TEXT NOT NULL DEFAULT 'counting',
    item       TEXT,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (project, key)
);
";

/// Version 10: knowledge proposals. Each has an inbox item of kind "proposal" with the same id.
const SCHEMA_V10: &str = "
CREATE TABLE proposals (
    id         TEXT PRIMARY KEY,
    project    TEXT NOT NULL,
    file       TEXT NOT NULL,
    title      TEXT NOT NULL,
    text       TEXT NOT NULL,
    replaces   TEXT,
    evidence   TEXT,
    by_session TEXT NOT NULL,
    status     TEXT NOT NULL DEFAULT 'open',
    reason     TEXT,
    created_at INTEGER NOT NULL,
    decided_at INTEGER
);
CREATE INDEX proposals_project ON proposals(project, status);
";

/// Version 11: spend, as Claude Code estimates it. `usage` holds each session's running total
/// (`cost_run`: the last total its runtime reported, which restarts at 0 after /clear or a
/// resume); `costs` each increase, for spend over time; `limits` a subscription's usage
/// limits as last reported.
const SCHEMA_V11: &str = "
CREATE TABLE usage (
    session_id  TEXT PRIMARY KEY,
    project     TEXT NOT NULL,
    cost_usd    REAL NOT NULL DEFAULT 0,
    cost_run    REAL NOT NULL DEFAULT 0,
    context_pct INTEGER,
    model       TEXT,
    updated_at  INTEGER NOT NULL
);
CREATE TABLE costs (
    session_id TEXT NOT NULL,
    project    TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    usd        REAL NOT NULL
);
CREATE INDEX costs_ts ON costs(ts);
CREATE TABLE limits (
    name       TEXT PRIMARY KEY,
    used_pct   REAL NOT NULL,
    resets_at  INTEGER,
    updated_at INTEGER NOT NULL
);
";

/// Version 5: readable session labels.
const SCHEMA_V5: &str = "ALTER TABLE sessions ADD COLUMN label TEXT;";

/// Version 4: supervision and worktrees on sessions; answers on attention items (questions).
const SCHEMA_V4: &str = "
ALTER TABLE sessions ADD COLUMN supervisor TEXT;
ALTER TABLE sessions ADD COLUMN worktree TEXT;
ALTER TABLE attention ADD COLUMN answer TEXT;
";

/// Version 3: sessions, so a restarted daemon can find and adopt the ones still running.
const SCHEMA_V3: &str = "
CREATE TABLE sessions (
    id           TEXT PRIMARY KEY,
    project      TEXT NOT NULL,
    command      TEXT NOT NULL,
    runtime      TEXT,
    backend      TEXT NOT NULL,
    one_shot     INTEGER NOT NULL,
    state        TEXT NOT NULL,
    pid          INTEGER,
    started_at   INTEGER NOT NULL,
    ended_at     INTEGER,
    exit_code    INTEGER,
    agent_state  TEXT NOT NULL,
    agent_detail TEXT,
    agent_source TEXT
);
CREATE INDEX sessions_state ON sessions(state);
";

const SCHEMA_V2: &str = "
CREATE TABLE attention (
    id           TEXT PRIMARY KEY,
    kind         TEXT NOT NULL,
    severity     INTEGER NOT NULL,
    project      TEXT NOT NULL,
    session_id   TEXT NOT NULL,
    summary      TEXT NOT NULL,
    status       TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    snooze_until INTEGER,
    resolved_at  INTEGER,
    note         TEXT
);
CREATE INDEX attention_session ON attention(session_id, status);
";

/// An approval item's status, decision and (for a denial) message.
pub type ApprovalState = (String, Option<String>, Option<String>);

/// One entry of the event log.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEvent {
    pub seq: i64,
    pub ts: i64,
    pub kind: String,
    pub payload: Value,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 1 {
            conn.execute_batch(SCHEMA_V1)?;
        }
        if version < 2 {
            conn.execute_batch(SCHEMA_V2)?;
        }
        if version < 3 {
            conn.execute_batch(SCHEMA_V3)?;
        }
        if version < 4 {
            conn.execute_batch(SCHEMA_V4)?;
        }
        if version < 5 {
            conn.execute_batch(SCHEMA_V5)?;
        }
        if version < 6 {
            conn.execute_batch(SCHEMA_V6)?;
        }
        if version < 7 {
            conn.execute_batch(SCHEMA_V7)?;
        }
        if version < 8 {
            conn.execute_batch(SCHEMA_V8)?;
        }
        if version < 9 {
            conn.execute_batch(SCHEMA_V9)?;
        }
        if version < 10 {
            conn.execute_batch(SCHEMA_V10)?;
        }
        if version < 11 {
            conn.execute_batch(SCHEMA_V11)?;
        }
        if version < SCHEMA_VERSION {
            conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
        }
        Ok(Self { conn })
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    pub fn insert_project(&self, p: &Project) -> Result<()> {
        self.conn.execute(
            "INSERT INTO projects (id, name, path, workspace, added_at, stack, test_command, build_command, branch, autonomy)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                p.id,
                p.name,
                p.path,
                p.workspace,
                p.added_at,
                serde_json::to_string(&p.stack)?,
                p.test_command,
                p.build_command,
                p.branch,
                p.autonomy,
            ],
        )?;
        Ok(())
    }

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, path, workspace, added_at, stack, test_command, build_command, branch, autonomy
             FROM projects ORDER BY workspace, name, id",
        )?;
        let rows = stmt.query_map([], project_from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn get_project(&self, id: &str) -> Result<Option<Project>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, name, path, workspace, added_at, stack, test_command, build_command, branch, autonomy
                 FROM projects WHERE id = ?1",
                params![id],
                project_from_row,
            )
            .optional()?)
    }

    pub fn find_by_path(&self, path: &str) -> Result<Option<Project>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, name, path, workspace, added_at, stack, test_command, build_command, branch, autonomy
                 FROM projects WHERE path = ?1",
                params![path],
                project_from_row,
            )
            .optional()?)
    }

    /// Returns true if a project was removed.
    pub fn delete_project(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM projects WHERE id = ?1", params![id])?
            > 0)
    }

    pub fn append_event(&self, kind: &str, payload: &Value) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO events (ts, kind, payload) VALUES (?1, ?2, ?3)",
            params![now_secs(), kind, serde_json::to_string(payload)?],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Most recent events first.
    /// Creates, updates or resolves the attention item for a session, given its new agent state.
    ///
    /// An agent can have at most one open item per kind. Items of other kinds are
    /// resolved, because the agent has moved on. Working and idle states resolve everything.
    /// Waiting after a report keeps the open done or failed item: the agent's turn ends once it
    /// has reported, and the report is what the human should see.
    pub fn set_session_attention(
        &self,
        session_id: &str,
        project: &str,
        state: &str,
        summary: &str,
        severity: u8,
        now: i64,
    ) -> Result<()> {
        if state == "waiting" {
            let reported: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM attention WHERE session_id = ?1
                               AND kind IN ('done', 'failed') AND status IN ('open', 'snoozed'))",
                params![session_id],
                |r| r.get(0),
            )?;
            if reported {
                return Ok(());
            }
        }
        self.conn.execute(
            "UPDATE attention SET status = 'resolved', resolved_at = ?2, updated_at = ?2, note = 'agent moved on'
             WHERE session_id = ?1 AND status IN ('open', 'snoozed') AND kind != ?3
                   AND kind NOT IN ('question', 'proposal', 'report')",
            params![session_id, now, state],
        )?;
        if !matches!(state, "blocked" | "waiting" | "done" | "failed") {
            return Ok(());
        }
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM attention WHERE session_id = ?1 AND kind = ?2 AND status IN ('open', 'snoozed')",
                params![session_id, state],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            Some(id) => {
                self.conn.execute(
                    "UPDATE attention SET summary = ?2, updated_at = ?3 WHERE id = ?1",
                    params![id, summary, now],
                )?;
            }
            None => {
                self.conn.execute(
                    "INSERT INTO attention (id, kind, severity, project, session_id, summary, status, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'open', ?7, ?7)",
                    params![
                        self.fresh_attention_id(now)?,
                        state,
                        severity,
                        project,
                        session_id,
                        summary,
                        now
                    ],
                )?;
            }
        }
        Ok(())
    }

    /// Resolves every open item about a session, e.g. when the daemon restarts and its
    /// sessions are gone. Suggestions and proposals stay: they do not depend on a session.
    pub fn resolve_all_attention(&self, reason: &str, now: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE attention SET status = 'resolved', resolved_at = ?2, updated_at = ?2, note = ?1
             WHERE status IN ('open', 'snoozed') AND kind NOT IN ('suggestion', 'proposal')",
            params![reason, now],
        )?)
    }

    /// Open items, most severe first, then oldest. Snoozed items return once they are due.
    pub fn list_attention(&self, include_snoozed: bool, now: i64) -> Result<Vec<AttentionItem>> {
        let sql = if include_snoozed {
            "SELECT * FROM attention WHERE status = 'open' OR status = 'snoozed'
             ORDER BY severity DESC, created_at ASC, id ASC"
        } else {
            "SELECT * FROM attention WHERE status = 'open' OR (status = 'snoozed' AND snooze_until <= ?1)
             ORDER BY severity DESC, created_at ASC, id ASC"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = if include_snoozed {
            stmt.query_map([], attention_from_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![now], attention_from_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        Ok(rows
            .into_iter()
            .map(|mut item| {
                // A snooze that has come due reads as open.
                if item.status == "snoozed" && item.snooze_until.is_some_and(|t| t <= now) {
                    item.status = "open".into();
                }
                item
            })
            .collect())
    }

    pub fn get_attention(&self, id: &str) -> Result<Option<AttentionItem>> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM attention WHERE id = ?1",
                params![id],
                attention_from_row,
            )
            .optional()?)
    }

    /// Marks an item resolved by the human. Returns false if there is no such item.
    pub fn ack_attention(&self, id: &str, now: i64) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE attention SET status = 'resolved', resolved_at = ?2, updated_at = ?2, note = 'acknowledged'
             WHERE id = ?1 AND status IN ('open', 'snoozed')",
            params![id, now],
        )? > 0)
    }

    pub fn snooze_attention(&self, id: &str, until: i64, now: i64) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE attention SET status = 'snoozed', snooze_until = ?2, updated_at = ?3
             WHERE id = ?1 AND status IN ('open', 'snoozed')",
            params![id, until, now],
        )? > 0)
    }

    /// Records a question from a session for a human. Returns the new item's id.
    pub fn ask_question(
        &self,
        session_id: &str,
        project: &str,
        question: &str,
        now: i64,
    ) -> Result<String> {
        let id = self.fresh_attention_id(now)?;
        self.conn.execute(
            "INSERT INTO attention (id, kind, severity, project, session_id, summary, status, created_at, updated_at)
             VALUES (?1, 'question', 3, ?2, ?3, ?4, 'open', ?5, ?5)",
            params![id, project, session_id, question, now],
        )?;
        Ok(id)
    }

    /// Answers an open question. Returns false if there is no such open question.
    pub fn answer_question(&self, id: &str, answer: &str, now: i64) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE attention SET status = 'answered', answer = ?2, resolved_at = ?3, updated_at = ?3
             WHERE id = ?1 AND kind = 'question' AND status IN ('open', 'snoozed')",
            params![id, answer, now],
        )? > 0)
    }

    /// Answered questions that have not reached their agent yet: neither through `wait_for`
    /// nor typed into its session. Oldest first.
    pub fn undelivered_answers(&self) -> Result<Vec<AttentionItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM attention WHERE kind = 'question' AND status = 'answered'
                    AND note IS NULL ORDER BY resolved_at",
        )?;
        let rows = stmt.query_map([], attention_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Records that a question's answer reached its agent, so it is not delivered twice.
    /// Returns false if there is no such answered question.
    pub fn mark_answer_delivered(&self, id: &str, how: &str, now: i64) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE attention SET note = ?2, updated_at = ?3
             WHERE id = ?1 AND kind = 'question' AND status = 'answered'",
            params![id, how, now],
        )? > 0)
    }

    /// Records a permission request from a session as an inbox item of kind "approval".
    /// Severity 3 is for a human to decide; severity 1, for the session's orchestrator.
    /// Other items for the session are resolved (it has moved on), questions excepted. An
    /// older approval still open for the session is superseded. Returns the new item's id.
    pub fn open_approval(
        &self,
        session_id: &str,
        project: &str,
        summary: &str,
        severity: u8,
        note: Option<&str>,
        now: i64,
    ) -> Result<String> {
        self.conn.execute(
            "UPDATE attention SET status = 'resolved', resolved_at = ?2, updated_at = ?2,
                    note = CASE kind WHEN 'approval' THEN 'superseded' ELSE 'agent moved on' END
             WHERE session_id = ?1 AND status IN ('open', 'snoozed') AND kind NOT IN ('question', 'proposal')",
            params![session_id, now],
        )?;
        let id = self.fresh_attention_id(now)?;
        self.conn.execute(
            "INSERT INTO attention (id, kind, severity, project, session_id, summary, status, created_at, updated_at, note)
             VALUES (?1, 'approval', ?6, ?2, ?3, ?4, 'open', ?5, ?5, ?7)",
            params![id, project, session_id, summary, now, severity, note],
        )?;
        Ok(id)
    }

    /// Whether a session has a permission request open in the inbox.
    pub fn has_open_approval(&self, session_id: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM attention WHERE session_id = ?1 AND kind = 'approval'
                           AND status IN ('open', 'snoozed'))",
            params![session_id],
            |r| r.get(0),
        )?)
    }

    /// The open approval of a session, if any.
    pub fn open_approval_for(&self, session_id: &str) -> Result<Option<AttentionItem>> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM attention WHERE session_id = ?1 AND kind = 'approval'
                 AND status IN ('open', 'snoozed') ORDER BY created_at DESC LIMIT 1",
                params![session_id],
                attention_from_row,
            )
            .optional()?)
    }

    /// The open item of a kind for a session, if any.
    pub fn open_item(&self, session_id: &str, kind: &str) -> Result<Option<AttentionItem>> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM attention WHERE session_id = ?1 AND kind = ?2
                 AND status IN ('open', 'snoozed') ORDER BY created_at DESC LIMIT 1",
                params![session_id, kind],
                attention_from_row,
            )
            .optional()?)
    }

    /// Opens a "trust" item: Claude Code asks whether to trust the session's folder, before
    /// its hooks run. Returns the new item's id.
    pub fn open_trust(
        &self,
        session_id: &str,
        project: &str,
        summary: &str,
        now: i64,
    ) -> Result<String> {
        let id = self.fresh_attention_id(now)?;
        self.conn.execute(
            "INSERT INTO attention (id, kind, severity, project, session_id, summary, status, created_at, updated_at)
             VALUES (?1, 'trust', 3, ?2, ?3, ?4, 'open', ?5, ?5)",
            params![id, project, session_id, summary, now],
        )?;
        Ok(id)
    }

    /// Opens a "report" item: for your information, a mission someone else supervises (the
    /// project's lead) has finished. It stays until you acknowledge or dismiss it, whatever
    /// the session does next. Returns the new item's id.
    pub fn open_report(
        &self,
        session_id: &str,
        project: &str,
        summary: &str,
        severity: u8,
        now: i64,
    ) -> Result<String> {
        let id = self.fresh_attention_id(now)?;
        self.conn.execute(
            "INSERT INTO attention (id, kind, severity, project, session_id, summary, status, created_at, updated_at)
             VALUES (?1, 'report', ?2, ?3, ?4, ?5, 'open', ?6, ?6)",
            params![id, severity, project, session_id, summary, now],
        )?;
        Ok(id)
    }

    /// Closes an item that was answered (in the inbox, or in the session itself).
    pub fn resolve_item(&self, id: &str, note: &str, now: i64) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE attention SET status = 'resolved', resolved_at = ?2, updated_at = ?2, note = ?3
             WHERE id = ?1 AND status IN ('open', 'snoozed')",
            params![id, now, note],
        )? > 0)
    }

    /// Adds a rule; returns its id (`r1`, `r2`, ...).
    pub fn add_rule(
        &self,
        project: Option<&str>,
        kind: &str,
        pattern: &str,
        reason: &str,
        action: &str,
        now: i64,
    ) -> Result<String> {
        let n: i64 =
            self.conn
                .query_row("SELECT COALESCE(MAX(rowid), 0) + 1 FROM rules", [], |r| {
                    r.get(0)
                })?;
        let id = format!("r{n}");
        self.conn.execute(
            "INSERT INTO rules (id, project, kind, pattern, reason, action, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, project, kind, pattern, reason, action, now],
        )?;
        Ok(id)
    }

    /// Removes a rule a human added, with its settings. Returns false if there is none.
    pub fn remove_rule(&self, id: &str) -> Result<bool> {
        let removed = self
            .conn
            .execute("DELETE FROM rules WHERE id = ?1", params![id])?
            > 0;
        if removed {
            self.conn
                .execute("DELETE FROM rule_settings WHERE rule = ?1", params![id])?;
        }
        Ok(removed)
    }

    /// The rules humans added, oldest first, with their own actions.
    pub fn added_rules(&self) -> Result<Vec<corvx_proto::Rule>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project, kind, pattern, reason, action FROM rules ORDER BY rowid",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(corvx_proto::Rule {
                id: r.get(0)?,
                project: r.get(1)?,
                kind: r.get(2)?,
                pattern: r.get(3)?,
                reason: r.get(4)?,
                action: r.get(5)?,
                builtin: false,
                set_in: "default".into(),
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Sets what a rule does in a project ('' for every project); None drops the setting.
    pub fn set_rule_action(&self, rule: &str, project: &str, action: Option<&str>) -> Result<()> {
        match action {
            Some(a) => self.conn.execute(
                "INSERT OR REPLACE INTO rule_settings (rule, project, action) VALUES (?1, ?2, ?3)",
                params![rule, project, a],
            )?,
            None => self.conn.execute(
                "DELETE FROM rule_settings WHERE rule = ?1 AND project = ?2",
                params![rule, project],
            )?,
        };
        Ok(())
    }

    /// Every rule setting: (rule, project, action), project '' meaning every project.
    pub fn rule_settings(&self) -> Result<Vec<(String, String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT rule, project, action FROM rule_settings")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Sets a project's autonomy level. Returns false if there is no such project.
    /// Replaces a project's name, workspace and scan results. Returns true if it exists.
    pub fn update_project(&self, p: &Project) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE projects SET name = ?2, workspace = ?3, stack = ?4, test_command = ?5,
                    build_command = ?6, branch = ?7
             WHERE id = ?1",
            params![
                p.id,
                p.name,
                p.workspace,
                serde_json::to_string(&p.stack)?,
                p.test_command,
                p.build_command,
                p.branch,
            ],
        )? > 0)
    }

    pub fn set_project_autonomy(&self, id: &str, level: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE projects SET autonomy = ?2 WHERE id = ?1",
            params![id, level],
        )? > 0)
    }

    /// Counts one more human approval of `key` in `project`. Returns the new count and the
    /// row's state ("counting", "suggested", "accepted" or "declined").
    pub fn count_approval(&self, project: &str, key: &str, now: i64) -> Result<(i64, String)> {
        self.conn.execute(
            "INSERT INTO trust (project, key, approvals, updated_at) VALUES (?1, ?2, 1, ?3)
             ON CONFLICT (project, key) DO UPDATE SET approvals = approvals + 1, updated_at = ?3",
            params![project, key, now],
        )?;
        Ok(self.conn.query_row(
            "SELECT approvals, state FROM trust WHERE project = ?1 AND key = ?2",
            params![project, key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// Sets a trust row's state, and the suggestion item that offered it.
    pub fn set_trust(
        &self,
        project: &str,
        key: &str,
        state: &str,
        item: Option<&str>,
        now: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE trust SET state = ?3, item = COALESCE(?4, item), updated_at = ?5
             WHERE project = ?1 AND key = ?2",
            params![project, key, state, item, now],
        )?;
        Ok(())
    }

    /// The trust row a suggestion item offered: (project, key).
    pub fn trust_of_item(&self, item: &str) -> Result<Option<(String, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT project, key FROM trust WHERE item = ?1",
                params![item],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Opens a suggestion: an item for the human, tied to a project rather than a session
    /// (its session is "-"), so agents' state changes do not close it.
    pub fn open_suggestion(&self, project: &str, summary: &str, now: i64) -> Result<String> {
        let id = self.fresh_attention_id(now)?;
        self.conn.execute(
            "INSERT INTO attention (id, kind, severity, project, session_id, summary, status, created_at, updated_at)
             VALUES (?1, 'suggestion', 1, ?2, '-', ?3, 'open', ?4, ?4)",
            params![id, project, summary, now],
        )?;
        Ok(id)
    }

    /// Closes a suggestion with what the human said ("allowed" or "declined").
    pub fn close_suggestion(&self, id: &str, answer: &str, now: i64) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE attention SET status = 'answered', answer = ?2, resolved_at = ?3, updated_at = ?3
             WHERE id = ?1 AND kind = 'suggestion' AND status IN ('open', 'snoozed')",
            params![id, answer, now],
        )? > 0)
    }

    /// Passes an open approval to a human: it becomes severity 3, with a note saying why.
    /// Returns false if there is no such open approval.
    pub fn escalate_approval(&self, id: &str, note: &str, now: i64) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE attention SET severity = 3, note = ?2, updated_at = ?3
             WHERE id = ?1 AND kind = 'approval' AND status IN ('open', 'snoozed')",
            params![id, note, now],
        )? > 0)
    }

    /// Decides an open approval. `message` is kept for a denial. Returns false if there is no
    /// such open approval.
    pub fn decide_approval(
        &self,
        id: &str,
        decision: &str,
        message: Option<&str>,
        now: i64,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE attention SET status = 'answered', answer = ?2, note = ?3, resolved_at = ?4, updated_at = ?4
             WHERE id = ?1 AND kind = 'approval' AND status IN ('open', 'snoozed')",
            params![id, decision, message, now],
        )? > 0)
    }

    /// The status, decision and message of an approval item.
    pub fn approval_state(&self, id: &str) -> Result<Option<ApprovalState>> {
        Ok(self
            .conn
            .query_row(
                "SELECT status, answer, note FROM attention WHERE id = ?1 AND kind = 'approval'",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }

    pub fn dismiss_attention(&self, id: &str, reason: &str, now: i64) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE attention SET status = 'dismissed', resolved_at = ?2, updated_at = ?2, note = ?3
             WHERE id = ?1 AND status IN ('open', 'snoozed')",
            params![id, now, reason],
        )? > 0)
    }

    /// Records the total cost a session's runtime reports, and how full its context is.
    /// Returns the session's cost so far. A total lower than the last one is a new run (the
    /// conversation was cleared or resumed), which adds to what came before.
    pub fn record_usage(
        &self,
        session_id: &str,
        project: &str,
        run_total: f64,
        context_pct: Option<u8>,
        model: Option<&str>,
        now: i64,
    ) -> Result<f64> {
        let (total, last): (f64, f64) = self
            .conn
            .query_row(
                "SELECT cost_usd, cost_run FROM usage WHERE session_id = ?1",
                params![session_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((0.0, 0.0));
        let run_total = run_total.max(0.0);
        let added = if run_total >= last {
            run_total - last
        } else {
            run_total
        };
        let total = total + added;
        self.conn.execute(
            "INSERT INTO usage (session_id, project, cost_usd, cost_run, context_pct, model, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(session_id) DO UPDATE SET cost_usd = ?3, cost_run = ?4,
                 context_pct = COALESCE(?5, context_pct), model = COALESCE(?6, model), updated_at = ?7",
            params![session_id, project, total, run_total, context_pct, model, now],
        )?;
        if added > 0.0 {
            self.conn.execute(
                "INSERT INTO costs (session_id, project, ts, usd) VALUES (?1, ?2, ?3, ?4)",
                params![session_id, project, now, added],
            )?;
        }
        Ok(total)
    }

    /// Records a subscription usage limit ("five_hour", "seven_day") as last reported.
    pub fn record_limit(
        &self,
        name: &str,
        used_pct: f64,
        resets_at: Option<i64>,
        now: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO limits (name, used_pct, resets_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
            params![name, used_pct, resets_at, now],
        )?;
        Ok(())
    }

    /// Spend since local midnight, in all, per project and per session, and the usage limits
    /// still current at `now`.
    pub fn usage_summary(&self, now: i64) -> Result<corvx_proto::UsageSummary> {
        use corvx_proto::{ProjectUsage, SessionUsage, UsageLimit, UsageSummary};
        let midnight: i64 = self.conn.query_row(
            "SELECT CAST(strftime('%s', ?1, 'unixepoch', 'localtime', 'start of day', 'utc') AS INTEGER)",
            params![now],
            |r| r.get(0),
        )?;
        let mut projects: Vec<ProjectUsage> = self
            .conn
            .prepare(
                "SELECT project, SUM(CASE WHEN ts >= ?1 THEN usd ELSE 0.0 END), SUM(usd)
                 FROM costs GROUP BY project ORDER BY project",
            )?
            .query_map(params![midnight], |r| {
                Ok(ProjectUsage {
                    project: r.get(0)?,
                    today_usd: r.get(1)?,
                    total_usd: r.get(2)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        projects.retain(|p| p.total_usd > 0.0);
        let sessions: Vec<SessionUsage> = self
            .conn
            .prepare(
                "SELECT session_id, project, cost_usd, context_pct, model FROM usage ORDER BY session_id",
            )?
            .query_map([], |r| {
                Ok(SessionUsage {
                    id: r.get(0)?,
                    project: r.get(1)?,
                    cost_usd: r.get(2)?,
                    context_pct: r.get(3)?,
                    model: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        let limits: Vec<UsageLimit> = self
            .conn
            .prepare(
                "SELECT name, used_pct, resets_at, updated_at FROM limits
                 WHERE resets_at IS NULL OR resets_at > ?1 ORDER BY name",
            )?
            .query_map(params![now], |r| {
                Ok(UsageLimit {
                    name: r.get(0)?,
                    used_pct: r.get(1)?,
                    resets_at: r.get(2)?,
                    updated_at: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(UsageSummary {
            today_usd: projects.iter().map(|p| p.today_usd).sum(),
            total_usd: projects.iter().map(|p| p.total_usd).sum(),
            projects,
            sessions,
            limits,
        })
    }

    /// Saves a session's current state. Called on start and whenever its state changes.
    pub fn save_session(&self, s: &Session, one_shot: bool) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sessions
             (id, project, command, runtime, backend, one_shot, state, pid, started_at, ended_at,
              exit_code, agent_state, agent_detail, agent_source, supervisor, worktree, label,
              runtime_session, autopilot, agent_model, human_approves_agents)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
                     ?19, ?20, ?21)",
            params![
                s.id,
                s.project,
                serde_json::to_string(&s.command)?,
                s.runtime,
                s.backend,
                one_shot,
                enum_name(&s.state)?,
                s.pid,
                s.started_at,
                s.ended_at,
                s.exit_code,
                enum_name(&s.agent_state)?,
                s.agent_detail,
                s.agent_source,
                s.supervisor,
                s.worktree,
                s.label,
                s.runtime_session,
                s.autopilot,
                s.agent_model,
                s.human_approves_agents,
            ],
        )?;
        Ok(())
    }

    /// Sessions recorded as running, with their one-shot flag. Used to adopt them after a restart.
    pub fn running_sessions(&self) -> Result<Vec<(Session, bool)>> {
        self.load_sessions("state = 'running'", params![])
    }

    /// Sessions lost since `since` (their terminal disappeared), oldest first.
    pub fn lost_sessions(&self, since: i64) -> Result<Vec<(Session, bool)>> {
        self.load_sessions(
            "state = 'lost' AND COALESCE(ended_at, started_at) >= ?1",
            params![since],
        )
    }

    /// One session record, whatever its state.
    pub fn get_session(&self, id: &str) -> Result<Option<(Session, bool)>> {
        Ok(self.load_sessions("id = ?1", params![id])?.pop())
    }

    /// Every session recorded for a project (or for all projects), whatever its state.
    pub fn project_sessions(&self, project: Option<&str>) -> Result<Vec<(Session, bool)>> {
        match project {
            Some(p) => self.load_sessions("project = ?1", params![p]),
            None => self.load_sessions("1", params![]),
        }
    }

    /// The sessions a session supervises, whatever their state.
    pub fn supervised_by(&self, id: &str) -> Result<Vec<(Session, bool)>> {
        self.load_sessions("supervisor = ?1", params![id])
    }

    fn load_sessions(
        &self,
        filter: &str,
        args: &[&dyn rusqlite::ToSql],
    ) -> Result<Vec<(Session, bool)>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, project, command, runtime, backend, one_shot, state, pid, started_at, ended_at,
                    exit_code, agent_state, agent_detail, agent_source, supervisor, worktree, label,
                    runtime_session, autopilot, agent_model, human_approves_agents
             FROM sessions WHERE {filter} ORDER BY started_at, id"
        ))?;
        let rows = stmt.query_map(args, |r| {
            let command: String = r.get(2)?;
            let state: String = r.get(6)?;
            let agent_state: String = r.get(11)?;
            Ok((
                Session {
                    id: r.get(0)?,
                    project: r.get(1)?,
                    command: serde_json::from_str(&command).unwrap_or_default(),
                    runtime: r.get(3)?,
                    backend: r.get(4)?,
                    state: from_name(&state).unwrap_or(SessionState::Stopped),
                    pid: r.get(7)?,
                    started_at: r.get(8)?,
                    ended_at: r.get(9)?,
                    exit_code: r.get(10)?,
                    agent_state: from_name(&agent_state).unwrap_or(AgentState::Unknown),
                    agent_detail: r.get(12)?,
                    agent_source: r.get(13)?,
                    supervisor: r.get(14)?,
                    worktree: r.get(15)?,
                    label: r.get(16)?,
                    runtime_session: r.get(17)?,
                    autopilot: r.get(18)?,
                    agent_model: r.get(19)?,
                    human_approves_agents: r.get(20)?,
                },
                r.get::<_, bool>(5)?,
            ))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Records a knowledge proposal and opens its inbox item (severity 2: review). The item
    /// belongs to the proposing session but is not closed when that session moves on.
    pub fn insert_proposal(&self, p: &Proposal, now: i64) -> Result<String> {
        let id = self.fresh_attention_id(now)?;
        self.conn.execute(
            "INSERT INTO proposals (id, project, file, title, text, replaces, evidence, by_session, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'open', ?9)",
            params![id, p.project, p.file, p.title, p.text, p.replaces, p.evidence, p.by, now],
        )?;
        self.conn.execute(
            "INSERT INTO attention (id, kind, severity, project, session_id, summary, status, created_at, updated_at)
             VALUES (?1, 'proposal', 2, ?2, ?3, ?4, 'open', ?5, ?5)",
            params![id, p.project, p.by, format!("{}: {}", p.file, p.title), now],
        )?;
        Ok(id)
    }

    pub fn get_proposal(&self, id: &str) -> Result<Option<Proposal>> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM proposals WHERE id = ?1",
                params![id],
                proposal_from_row,
            )
            .optional()?)
    }

    /// Proposals, newest first: open ones only unless `all`, for one project or all of them.
    pub fn list_proposals(&self, project: Option<&str>, all: bool) -> Result<Vec<Proposal>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM proposals WHERE (?1 IS NULL OR project = ?1) AND (?2 OR status = 'open')
             ORDER BY created_at DESC, id DESC",
        )?;
        let rows = stmt.query_map(params![project, all], proposal_from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// The most recent rejected proposals of a project.
    pub fn rejected_proposals(&self, project: &str, limit: i64) -> Result<Vec<Proposal>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM proposals WHERE project = ?1 AND status = 'rejected'
             ORDER BY decided_at DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![project, limit], proposal_from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Decides an open proposal: "accepted" (with the text as written) or "rejected" (with a
    /// reason). Closes its inbox item. Returns false if it is not open.
    pub fn decide_proposal(
        &self,
        id: &str,
        status: &str,
        text: Option<&str>,
        reason: Option<&str>,
        now: i64,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE proposals SET status = ?2, text = COALESCE(?3, text), reason = ?4, decided_at = ?5
             WHERE id = ?1 AND status = 'open'",
            params![id, status, text, reason, now],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        self.conn.execute(
            "UPDATE attention SET status = 'answered', answer = ?2, note = ?3, resolved_at = ?4, updated_at = ?4
             WHERE id = ?1",
            params![id, status, reason, now],
        )?;
        Ok(true)
    }

    pub fn recent_events(&self, limit: i64) -> Result<Vec<StoredEvent>> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, ts, kind, payload FROM events ORDER BY seq DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit], |row| {
            let payload: String = row.get(3)?;
            Ok(StoredEvent {
                seq: row.get(0)?,
                ts: row.get(1)?,
                kind: row.get(2)?,
                payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

fn attention_from_row(row: &Row<'_>) -> rusqlite::Result<AttentionItem> {
    Ok(AttentionItem {
        id: row.get("id")?,
        kind: row.get("kind")?,
        severity: row.get::<_, i64>("severity")? as u8,
        project: row.get("project")?,
        session_id: row.get("session_id")?,
        summary: row.get("summary")?,
        status: row.get("status")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        snooze_until: row.get("snooze_until")?,
        answer: row.get("answer")?,
        note: row.get("note")?,
    })
}

fn proposal_from_row(row: &Row<'_>) -> rusqlite::Result<Proposal> {
    Ok(Proposal {
        id: row.get("id")?,
        project: row.get("project")?,
        file: row.get("file")?,
        title: row.get("title")?,
        text: row.get("text")?,
        replaces: row.get("replaces")?,
        evidence: row.get("evidence")?,
        by: row.get("by_session")?,
        status: row.get("status")?,
        reason: row.get("reason")?,
        created_at: row.get("created_at")?,
        decided_at: row.get("decided_at")?,
    })
}

/// The serialized name of a unit enum, e.g. `SessionState::Running` -> "running".
fn enum_name<T: serde::Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_value(value)?
        .as_str()
        .unwrap_or_default()
        .to_string())
}

fn from_name<T: serde::de::DeserializeOwned>(name: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(name.to_string())).ok()
}

fn new_attention_id(now: i64) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("att-{now}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

impl Store {
    /// An id no item has yet. The counter starts again with each daemon, so within the
    /// second a daemon restarts, an earlier daemon's id may already be taken.
    fn fresh_attention_id(&self, now: i64) -> Result<String> {
        loop {
            let id = new_attention_id(now);
            let taken: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM attention WHERE id = ?1)",
                params![id],
                |r| r.get(0),
            )?;
            if !taken {
                return Ok(id);
            }
        }
    }
}

fn project_from_row(row: &Row<'_>) -> rusqlite::Result<Project> {
    let stack: String = row.get(5)?;
    Ok(Project {
        id: row.get(0)?,
        name: row.get(1)?,
        path: row.get(2)?,
        workspace: row.get(3)?,
        added_at: row.get(4)?,
        stack: serde_json::from_str(&stack).unwrap_or_default(),
        test_command: row.get(6)?,
        build_command: row.get(7)?,
        branch: row.get(8)?,
        autonomy: row.get(9)?,
    })
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample(id: &str, path: &str, workspace: &str) -> Project {
        Project {
            id: id.into(),
            name: id.into(),
            path: path.into(),
            workspace: workspace.into(),
            added_at: 1,
            stack: vec!["rust".into()],
            test_command: Some("cargo test".into()),
            build_command: None,
            branch: Some("main".into()),
            autonomy: "guided".into(),
        }
    }

    #[test]
    fn schema_is_created_at_current_version() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn project_roundtrip_and_ordering() {
        let store = Store::open_in_memory().unwrap();
        store.insert_project(&sample("b", "/x/b", "Work")).unwrap();
        store.insert_project(&sample("a", "/x/a", "Work")).unwrap();
        store.insert_project(&sample("c", "/x/c", "Home")).unwrap();

        let ids: Vec<_> = store
            .list_projects()
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids, vec!["c", "a", "b"]);

        let got = store.get_project("a").unwrap().unwrap();
        assert_eq!(got.stack, vec!["rust"]);
        assert_eq!(got.test_command.as_deref(), Some("cargo test"));
    }

    #[test]
    fn path_is_unique() {
        let store = Store::open_in_memory().unwrap();
        store.insert_project(&sample("a", "/x/a", "Work")).unwrap();
        assert!(store.insert_project(&sample("b", "/x/a", "Work")).is_err());
    }

    #[test]
    fn delete_reports_whether_anything_was_removed() {
        let store = Store::open_in_memory().unwrap();
        store.insert_project(&sample("a", "/x/a", "Work")).unwrap();
        assert!(store.delete_project("a").unwrap());
        assert!(!store.delete_project("a").unwrap());
    }

    #[test]
    fn events_are_appended_and_returned_newest_first() {
        let store = Store::open_in_memory().unwrap();
        store
            .append_event("project.added", &json!({"id": "a"}))
            .unwrap();
        store
            .append_event("project.removed", &json!({"id": "a"}))
            .unwrap();
        let events = store.recent_events(10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "project.removed");
        assert_eq!(events[1].payload, json!({"id": "a"}));
    }
}

#[cfg(test)]
mod attention_tests {
    use super::*;

    fn raise(store: &Store, session: &str, state: &str, now: i64) {
        store
            .set_session_attention(
                session,
                "app",
                state,
                &format!("{state} for {session}"),
                3,
                now,
            )
            .unwrap();
    }

    #[test]
    fn one_open_item_per_session_and_kind() {
        let store = Store::open_in_memory().unwrap();
        raise(&store, "s1", "blocked", 10);
        raise(&store, "s1", "blocked", 20);
        let items = store.list_attention(false, 30).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].created_at, 10, "keeps the original time");
        assert_eq!(items[0].updated_at, 20);
    }

    #[test]
    fn moving_on_resolves_other_items_for_the_session() {
        let store = Store::open_in_memory().unwrap();
        raise(&store, "s1", "blocked", 10);
        raise(&store, "s1", "done", 20);
        let kinds: Vec<_> = store
            .list_attention(false, 30)
            .unwrap()
            .into_iter()
            .map(|i| i.kind)
            .collect();
        assert_eq!(kinds, vec!["done"]);

        // Its turn ends after the report: the report stays, no "waiting" item.
        raise(&store, "s1", "waiting", 30);
        let kinds: Vec<_> = store
            .list_attention(false, 35)
            .unwrap()
            .into_iter()
            .map(|i| i.kind)
            .collect();
        assert_eq!(kinds, vec!["done"]);

        store
            .set_session_attention("s1", "app", "working", "", 0, 40)
            .unwrap();
        assert!(store.list_attention(false, 50).unwrap().is_empty());
    }

    #[test]
    fn most_severe_first_then_oldest() {
        let store = Store::open_in_memory().unwrap();
        store
            .set_session_attention("a", "app", "done", "review", 2, 5)
            .unwrap();
        store
            .set_session_attention("b", "app", "failed", "boom", 3, 9)
            .unwrap();
        store
            .set_session_attention("c", "app", "blocked", "ask", 3, 7)
            .unwrap();
        let order: Vec<_> = store
            .list_attention(false, 10)
            .unwrap()
            .into_iter()
            .map(|i| i.session_id)
            .collect();
        assert_eq!(order, vec!["c", "b", "a"]);
    }

    #[test]
    fn snooze_hides_until_due() {
        let store = Store::open_in_memory().unwrap();
        raise(&store, "s1", "blocked", 10);
        let id = store.list_attention(false, 10).unwrap()[0].id.clone();
        assert!(store.snooze_attention(&id, 100, 20).unwrap());
        assert!(store.list_attention(false, 50).unwrap().is_empty());
        assert_eq!(store.list_attention(true, 50).unwrap()[0].status, "snoozed");
        let due = store.list_attention(false, 100).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].status, "open");
    }

    #[test]
    fn ack_and_dismiss_remove_items() {
        let store = Store::open_in_memory().unwrap();
        raise(&store, "s1", "done", 10);
        raise(&store, "s2", "failed", 10);
        let items = store.list_attention(false, 20).unwrap();
        let ack_id = items
            .iter()
            .find(|i| i.session_id == "s1")
            .unwrap()
            .id
            .clone();
        let dis_id = items
            .iter()
            .find(|i| i.session_id == "s2")
            .unwrap()
            .id
            .clone();
        assert!(store.ack_attention(&ack_id, 30).unwrap());
        assert!(store.dismiss_attention(&dis_id, "known flaky", 30).unwrap());
        assert!(store.list_attention(true, 40).unwrap().is_empty());
        assert_eq!(
            store.get_attention(&dis_id).unwrap().unwrap().status,
            "dismissed"
        );
        assert!(!store.ack_attention("nope", 40).unwrap());
    }

    #[test]
    fn resolve_all_clears_everything_open() {
        let store = Store::open_in_memory().unwrap();
        raise(&store, "s1", "blocked", 10);
        raise(&store, "s2", "done", 10);
        assert_eq!(
            store.resolve_all_attention("daemon restarted", 20).unwrap(),
            2
        );
        assert!(store.list_attention(true, 30).unwrap().is_empty());
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;

    pub(super) fn session(id: &str, state: SessionState) -> Session {
        Session {
            id: id.into(),
            project: "app".into(),
            command: vec!["claude".into()],
            runtime: Some("claude".into()),
            backend: "tmux".into(),
            supervisor: Some("s0".into()),
            worktree: Some("/tmp/wt".into()),
            label: Some("agent · test".into()),
            state,
            pid: Some(42),
            started_at: 1,
            ended_at: None,
            exit_code: None,
            agent_state: AgentState::Waiting,
            agent_detail: Some("detail".into()),
            agent_source: Some("hooks".into()),
            runtime_session: Some("conv-1".into()),
            autopilot: true,
            agent_model: Some("sonnet".into()),
            human_approves_agents: true,
        }
    }

    #[test]
    fn running_sessions_round_trip() {
        let store = Store::open_in_memory().unwrap();
        store
            .save_session(&session("s1", SessionState::Running), true)
            .unwrap();
        store
            .save_session(&session("s2", SessionState::Stopped), false)
            .unwrap();
        let running = store.running_sessions().unwrap();
        assert_eq!(running.len(), 1);
        let (s, one_shot) = &running[0];
        assert_eq!(s, &session("s1", SessionState::Running));
        assert!(*one_shot);
    }

    #[test]
    fn lost_is_stored_and_read_back() {
        let store = Store::open_in_memory().unwrap();
        store
            .save_session(&session("s1", SessionState::Lost), false)
            .unwrap();
        let (s, _) = store.get_session("s1").unwrap().unwrap();
        assert_eq!(s.state, SessionState::Lost);
    }

    #[test]
    fn saving_again_updates_the_session() {
        let store = Store::open_in_memory().unwrap();
        store
            .save_session(&session("s1", SessionState::Running), false)
            .unwrap();
        store
            .save_session(&session("s1", SessionState::Exited), false)
            .unwrap();
        assert!(store.running_sessions().unwrap().is_empty());
    }
}

#[cfg(test)]
mod lost_session_tests {
    use super::*;

    #[test]
    fn lost_sessions_and_supervised_ones_are_found() {
        let store = Store::open_in_memory().unwrap();
        let mut s = super::session_tests::session("s1", SessionState::Lost);
        s.ended_at = Some(100);
        store.save_session(&s, false).unwrap();
        let mut old = super::session_tests::session("s0", SessionState::Lost);
        old.ended_at = Some(10);
        store.save_session(&old, false).unwrap();
        let lost = store.lost_sessions(50).unwrap();
        assert_eq!(lost.len(), 1);
        assert_eq!(lost[0].0.id, "s1");
        assert_eq!(lost[0].0.runtime_session.as_deref(), Some("conv-1"));
        // Both fixtures answer to s0.
        assert_eq!(store.supervised_by("s0").unwrap().len(), 2);
        assert!(store.get_session("nope").unwrap().is_none());
    }
}

#[cfg(test)]
mod approval_tests {
    use super::*;

    #[test]
    fn an_approval_is_decided_once() {
        let store = Store::open_in_memory().unwrap();
        store
            .set_session_attention("s1", "app", "waiting", "", 1, 5)
            .unwrap();
        let id = store
            .open_approval("s1", "app", "Bash: npm test", 3, None, 10)
            .unwrap();
        let open = store.list_attention(false, 11).unwrap();
        assert_eq!(open.len(), 1, "the waiting item is resolved: {open:?}");
        assert_eq!(open[0].kind, "approval");
        assert!(store.has_open_approval("s1").unwrap());

        assert!(store
            .decide_approval(&id, "deny", Some("use make test"), 12)
            .unwrap());
        assert!(!store.decide_approval(&id, "allow", None, 13).unwrap());
        assert_eq!(
            store.approval_state(&id).unwrap(),
            Some((
                "answered".into(),
                Some("deny".into()),
                Some("use make test".into())
            ))
        );
        assert!(!store.has_open_approval("s1").unwrap());
    }

    #[test]
    fn a_new_request_supersedes_the_old_one_and_moving_on_resolves_it() {
        let store = Store::open_in_memory().unwrap();
        let first = store
            .open_approval("s1", "app", "Bash: a", 3, None, 10)
            .unwrap();
        let second = store
            .open_approval("s1", "app", "Bash: b", 3, None, 11)
            .unwrap();
        assert_eq!(store.approval_state(&first).unwrap().unwrap().0, "resolved");
        store
            .set_session_attention("s1", "app", "working", "", 0, 12)
            .unwrap();
        assert_eq!(
            store.approval_state(&second).unwrap().unwrap().0,
            "resolved"
        );
    }

    #[test]
    fn an_orchestrators_approval_can_be_escalated_to_the_human() {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .open_approval("s1", "app", "Bash: rm -rf build", 1, None, 10)
            .unwrap();
        let item = store.open_approval_for("s1").unwrap().unwrap();
        assert_eq!((item.id.as_str(), item.severity), (id.as_str(), 1));
        assert!(store.escalate_approval(&id, "deletes files", 11).unwrap());
        let item = store.get_attention(&id).unwrap().unwrap();
        assert_eq!(item.severity, 3);
        assert_eq!(item.note.as_deref(), Some("deletes files"));
        assert_eq!(item.status, "open", "escalating does not decide it");
        store.decide_approval(&id, "allow", None, 12).unwrap();
        assert!(!store.escalate_approval(&id, "too late", 13).unwrap());
        assert!(store.open_approval_for("s1").unwrap().is_none());
    }
}

#[cfg(test)]
mod question_tests {
    use super::*;

    #[test]
    fn questions_are_answered_once_and_survive_state_changes() {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .ask_question("s1", "app", "Which database?", 10)
            .unwrap();
        // The asking agent moving on does not resolve its question.
        store
            .set_session_attention("s1", "app", "working", "", 0, 11)
            .unwrap();
        let open = store.list_attention(false, 12).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].kind, "question");
        assert_eq!(open[0].summary, "Which database?");

        assert!(store.answer_question(&id, "SQLite", 13).unwrap());
        assert!(
            !store.answer_question(&id, "Postgres", 14).unwrap(),
            "answered only once"
        );
        assert!(store.list_attention(true, 15).unwrap().is_empty());
        let item = store.get_attention(&id).unwrap().unwrap();
        assert_eq!(item.status, "answered");
        assert_eq!(item.answer.as_deref(), Some("SQLite"));
    }
}
