//! A project's mission history: its orchestrators, newest first, with their agents and
//! journal entries. Shared by `corvx missions` and the TUI's missions view.

use std::path::Path;

use corvx_proto::{JournalEntry, Session, SessionState};

/// How many journal entries the history reads in full.
pub(crate) const JOURNAL_ENTRIES: usize = 50;

#[derive(Debug, Clone)]
pub(crate) struct Mission {
    /// The orchestrator. None for a journal entry whose session CORVX no longer knows.
    pub session: Option<Session>,
    /// When it started (Unix seconds), or 0 when only the journal is left.
    pub started_at: i64,
    /// The goal as the journal states it, or the session's (shortened) label.
    pub goal: String,
    pub agents: usize,
    pub agents_running: usize,
    /// Agents whose worktree is still on disk: what cleaning up would look at.
    pub worktrees: usize,
    /// The journal entry, relative to the project, and its text.
    pub journal: Option<JournalEntry>,
}

impl Mission {
    /// A word for where it stands: running, done, failed, stopped, interrupted.
    pub fn state(&self) -> &'static str {
        let Some(s) = &self.session else {
            return "ended";
        };
        match s.state {
            SessionState::Running => "running",
            SessionState::Lost => "interrupted",
            SessionState::Stopped => "stopped",
            SessionState::Exited => match &self.journal {
                Some(j) if j.text.contains("\n## Failed · ") => "failed",
                Some(j) if j.text.contains("\n## Done · ") => "done",
                _ if s.exit_code.unwrap_or(0) != 0 => "failed",
                _ => "ended",
            },
        }
    }
}

/// The project's missions, newest first: one per orchestrator (a top-level session that
/// starts agents), plus journal entries left from sessions CORVX no longer knows.
pub(crate) fn missions(
    project: &str,
    sessions: &[Session],
    journal: &[JournalEntry],
) -> Vec<Mission> {
    let mut out: Vec<Mission> = sessions
        .iter()
        .filter(|s| s.project == project && !crate::is_lead(s) && is_orchestrator(s, sessions))
        .map(|s| {
            let agents: Vec<&Session> = sessions
                .iter()
                .filter(|a| a.supervisor.as_deref() == Some(s.id.as_str()))
                .collect();
            let entry = journal
                .iter()
                .find(|j| journal_session(&j.path) == Some(&s.id));
            Mission {
                session: Some(s.clone()),
                started_at: s.started_at,
                goal: entry
                    .and_then(|j| goal_of(&j.text))
                    .unwrap_or_else(|| label_goal(s)),
                agents: agents.len(),
                agents_running: agents
                    .iter()
                    .filter(|a| a.state == SessionState::Running)
                    .count(),
                worktrees: agents
                    .iter()
                    .filter(|a| a.worktree.as_deref().is_some_and(|w| Path::new(w).exists()))
                    .count(),
                journal: entry.cloned(),
            }
        })
        .collect();
    for j in journal {
        let known = journal_session(&j.path).is_some_and(|id| {
            out.iter()
                .any(|m| m.session.as_ref().is_some_and(|s| s.id == id))
        });
        if !known {
            out.push(Mission {
                session: None,
                started_at: 0,
                goal: goal_of(&j.text).unwrap_or_else(|| j.path.clone()),
                agents: 0,
                agents_running: 0,
                worktrees: 0,
                journal: Some(j.clone()),
            });
        }
    }
    // Newest first; journal-only entries sort by the date in their file name.
    out.sort_by_key(|m| std::cmp::Reverse(sort_key(m)));
    out
}

fn sort_key(m: &Mission) -> String {
    match (&m.session, &m.journal) {
        (Some(s), _) => format!("{:020}", s.started_at),
        (None, Some(j)) => {
            // ".corvx/journal/2026-11-12-s123.md": the date, as seconds would sort.
            let name = j.path.rsplit('/').next().unwrap_or_default();
            format!("{:020}", date_secs(name.get(..10).unwrap_or_default()))
        }
        (None, None) => String::new(),
    }
}

/// Seconds at midnight UTC of a "YYYY-MM-DD" date, or 0.
fn date_secs(date: &str) -> i64 {
    let mut parts = date.split('-').map(|p| p.parse::<i64>().ok());
    let (Some(Some(y)), Some(Some(m)), Some(Some(d))) = (parts.next(), parts.next(), parts.next())
    else {
        return 0;
    };
    // Days from civil (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) * 86_400
}

/// An orchestrator gets a model for its agents and is named so; one from before either
/// existed is known as a top-level session with agents under it. You start orchestrators,
/// or a project lead does.
pub(crate) fn is_orchestrator(s: &Session, sessions: &[Session]) -> bool {
    s.agent_model.is_some()
        || crate::display_name(s).starts_with("orchestrator")
        || (s.supervisor.is_none()
            && sessions
                .iter()
                .any(|a| a.supervisor.as_deref() == Some(s.id.as_str())))
}

/// The session a journal file belongs to: `.corvx/journal/<date>-<session>.md`.
fn journal_session(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next()?.strip_suffix(".md")?;
    // The date is YYYY-MM-DD, then a dash.
    name.get(11..).filter(|s| !s.is_empty())
}

/// The mission's goal, from the journal's "**Goal:** ..." line.
fn goal_of(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.trim().strip_prefix("**Goal:**"))
        .map(|g| g.trim().to_string())
        .filter(|g| !g.is_empty())
}

fn label_goal(s: &Session) -> String {
    let name = crate::display_name(s);
    match name.strip_prefix("orchestrator · ") {
        Some(goal) => goal.to_string(),
        None => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, supervisor: Option<&str>, started_at: i64) -> Session {
        Session {
            id: id.into(),
            project: "snip".into(),
            command: vec!["claude".into()],
            runtime: Some("claude".into()),
            backend: "tmux".into(),
            supervisor: supervisor.map(Into::into),
            worktree: None,
            label: Some(if supervisor.is_some() {
                "agent · Backend".into()
            } else {
                "orchestrator · Add expiry…".into()
            }),
            state: SessionState::Exited,
            pid: None,
            started_at,
            ended_at: None,
            exit_code: Some(0),
            agent_state: corvx_proto::AgentState::Done,
            agent_detail: None,
            agent_source: None,
            runtime_session: None,
            autopilot: false,
            agent_model: supervisor.is_none().then(|| "sonnet".into()),
            human_approves_agents: false,
        }
    }

    #[test]
    fn missions_come_from_orchestrators_and_their_journals() {
        let sessions = vec![
            session("s1", None, 1_794_500_000),
            session("s2", Some("s1"), 1_794_500_100),
            session("s3", Some("s1"), 1_794_500_200),
            session("s4", None, 1_794_600_000),
            // A plain session someone started by hand is not a mission.
            Session {
                agent_model: None,
                label: None,
                ..session("s5", None, 1_794_700_000)
            },
        ];
        let journal = vec![
            JournalEntry {
                path: ".corvx/journal/2026-11-12-s1.md".into(),
                text: "# snip mission\n\n## Mission\n\n**Goal:** Let links expire.\n\n## Done · 10:00 UTC\n\nok\n"
                    .into(),
            },
            JournalEntry {
                path: ".corvx/journal/2026-01-02-s0.md".into(),
                text: "**Goal:** An old one.\n".into(),
            },
        ];
        let m = missions("snip", &sessions, &journal);
        assert_eq!(m.len(), 3, "{m:?}");
        assert_eq!(m[0].session.as_ref().unwrap().id, "s4");
        assert_eq!(m[0].goal, "Add expiry…");
        assert_eq!(m[0].state(), "ended");
        assert_eq!(m[1].session.as_ref().unwrap().id, "s1");
        assert_eq!(m[1].goal, "Let links expire.");
        assert_eq!(m[1].agents, 2);
        assert_eq!(m[1].state(), "done");
        assert!(m[2].session.is_none());
        assert_eq!(m[2].goal, "An old one.");
    }

    #[test]
    fn journal_files_name_their_session() {
        assert_eq!(
            journal_session(".corvx/journal/2026-11-12-s179-1.md"),
            Some("s179-1")
        );
        assert_eq!(journal_session(".corvx/journal/notes.md"), None);
        assert_eq!(date_secs("1970-01-02"), 86_400);
    }
}
