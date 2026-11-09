//! SQLite persistence and the append-only event log.
//!
//! Schema versions are tracked with `PRAGMA user_version`. Add a migration
//! step for each new version; never edit a version that has shipped.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use corvx_proto::{AgentState, AttentionItem, Project, Session, SessionState};
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

pub const SCHEMA_VERSION: i64 = 3;

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
            "INSERT INTO projects (id, name, path, workspace, added_at, stack, test_command, build_command, branch)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
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
            ],
        )?;
        Ok(())
    }

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, path, workspace, added_at, stack, test_command, build_command, branch
             FROM projects ORDER BY workspace, name, id",
        )?;
        let rows = stmt.query_map([], project_from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn get_project(&self, id: &str) -> Result<Option<Project>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, name, path, workspace, added_at, stack, test_command, build_command, branch
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
                "SELECT id, name, path, workspace, added_at, stack, test_command, build_command, branch
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
    pub fn set_session_attention(
        &self,
        session_id: &str,
        project: &str,
        state: &str,
        summary: &str,
        severity: u8,
        now: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE attention SET status = 'resolved', resolved_at = ?2, updated_at = ?2, note = 'agent moved on'
             WHERE session_id = ?1 AND status IN ('open', 'snoozed') AND kind != ?3",
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
                        new_attention_id(now),
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

    /// Resolves every open item, e.g. when the daemon restarts and its sessions are gone.
    pub fn resolve_all_attention(&self, reason: &str, now: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE attention SET status = 'resolved', resolved_at = ?2, updated_at = ?2, note = ?1
             WHERE status IN ('open', 'snoozed')",
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

    pub fn dismiss_attention(&self, id: &str, reason: &str, now: i64) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE attention SET status = 'dismissed', resolved_at = ?2, updated_at = ?2, note = ?3
             WHERE id = ?1 AND status IN ('open', 'snoozed')",
            params![id, now, reason],
        )? > 0)
    }

    /// Saves a session's current state. Called on start and whenever its state changes.
    pub fn save_session(&self, s: &Session, one_shot: bool) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sessions
             (id, project, command, runtime, backend, one_shot, state, pid, started_at, ended_at,
              exit_code, agent_state, agent_detail, agent_source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
            ],
        )?;
        Ok(())
    }

    /// Sessions recorded as running, with their one-shot flag. Used to adopt them after a restart.
    pub fn running_sessions(&self) -> Result<Vec<(Session, bool)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project, command, runtime, backend, one_shot, state, pid, started_at, ended_at,
                    exit_code, agent_state, agent_detail, agent_source
             FROM sessions WHERE state = 'running' ORDER BY started_at, id",
        )?;
        let rows = stmt.query_map([], |r| {
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
                    state: from_name(&state).unwrap_or(SessionState::Running),
                    pid: r.get(7)?,
                    started_at: r.get(8)?,
                    ended_at: r.get(9)?,
                    exit_code: r.get(10)?,
                    agent_state: from_name(&agent_state).unwrap_or(AgentState::Unknown),
                    agent_detail: r.get(12)?,
                    agent_source: r.get(13)?,
                },
                r.get::<_, bool>(5)?,
            ))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
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

    fn session(id: &str, state: SessionState) -> Session {
        Session {
            id: id.into(),
            project: "app".into(),
            command: vec!["claude".into()],
            runtime: Some("claude".into()),
            backend: "tmux".into(),
            state,
            pid: Some(42),
            started_at: 1,
            ended_at: None,
            exit_code: None,
            agent_state: AgentState::Waiting,
            agent_detail: Some("detail".into()),
            agent_source: Some("hooks".into()),
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
