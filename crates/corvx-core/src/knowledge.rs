//! Project knowledge: the plain files in a project's `.corvx/` folder that every agent starts
//! from, and the proposals that change them.
//!
//! - `brief.md`: what the project is and what matters now. Yours; agents may propose edits.
//! - `goals.md`: where it is heading (goals, each with a status) and the guideposts every
//!   mission follows. Yours; agents may propose goals or mark one done.
//! - `decisions.md`, `conventions.md`: changed only through proposals the human accepts.
//! - `map.md`: generated from the project's files (stack, commands, structure, entry points).
//! - `journal/`: one file per mission, written by orchestrators (see the daemon).
//!
//! Knowledge is read from the project's own checkout, never from an agent's worktree, so it is
//! current for every agent. Accepting a proposal writes the file and commits just that file, so
//! worktrees made afterwards, and anyone reading the repository, see it too.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use corvx_proto::{
    codes, goal_status, knowledge_files, Goal, JournalEntry, Knowledge, KnowledgeAccepted,
    KnowledgeFile, KnowledgeGoalParams, KnowledgeInit, KnowledgeProposeParams, Project, Proposal,
};
use serde_json::json;

use crate::scan::scan;
use crate::service::ServiceError;
use crate::store::Store;

/// Marks the part of `CLAUDE.md` (or `AGENTS.md`) that CORVX manages.
pub const WIRE_START: &str = "<!-- corvx:knowledge -->";
const WIRE_END: &str = "<!-- /corvx:knowledge -->";

/// Longest text returned for one file; agents can read the rest from the file itself.
const MAX_FILE: usize = 30_000;
const MAX_JOURNAL: usize = 12_000;
const MAX_PROPOSAL: usize = 8_000;
const MAX_TITLE: usize = 120;

/// `.corvx/<name>.md`, relative to the project.
pub fn rel_path(name: &str) -> String {
    format!(".corvx/{name}.md")
}

fn abs(root: &Path, name: &str) -> PathBuf {
    root.join(rel_path(name))
}

fn invalid(msg: impl Into<String>) -> ServiceError {
    ServiceError::Rpc(codes::PROPOSAL_INVALID, msg.into())
}

fn io_error(path: &Path, e: std::io::Error) -> ServiceError {
    ServiceError::Rpc(
        codes::INTERNAL,
        format!("cannot write {}: {e}", path.display()),
    )
}

fn project_of(store: &Store, id: &str) -> Result<Project, ServiceError> {
    store
        .get_project(id)?
        .ok_or_else(|| ServiceError::Rpc(codes::PROJECT_NOT_FOUND, format!("no project '{id}'")))
}

fn truncate(text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n\n[… cut here; read the file for the rest]",
        &text[..cut]
    )
}

/// Everything an agent should know about a project before it works on it.
pub fn read(store: &Store, project: &str, journal: usize) -> Result<Knowledge, ServiceError> {
    let p = project_of(store, project)?;
    let root = Path::new(&p.path);
    let mut files: Vec<KnowledgeFile> = knowledge_files::ALL
        .iter()
        .map(|name| {
            let text = fs::read_to_string(abs(root, name)).ok();
            KnowledgeFile {
                name: name.to_string(),
                path: rel_path(name),
                exists: text.is_some(),
                text: truncate(text.unwrap_or_default(), MAX_FILE),
            }
        })
        .collect();
    files.push(KnowledgeFile {
        name: "map".into(),
        path: rel_path("map"),
        exists: abs(root, "map").is_file(),
        text: map_text(&p),
    });
    let goals_text = fs::read_to_string(abs(root, knowledge_files::GOALS)).unwrap_or_default();
    let (goals, guideposts) = parse_goals(&goals_text);
    let entries = journal_files(root);
    let journal_entries = entries
        .iter()
        .take(journal)
        .filter_map(|path| {
            let text = fs::read_to_string(root.join(path)).ok()?;
            Some(JournalEntry {
                path: path.clone(),
                text: truncate(text, MAX_JOURNAL),
            })
        })
        .collect();
    Ok(Knowledge {
        project: p.id.clone(),
        path: p.path.clone(),
        files,
        goals,
        guideposts,
        journal: journal_entries,
        journal_count: entries.len(),
        open: store.list_proposals(Some(&p.id), false)?,
        rejected: store.rejected_proposals(&p.id, 20)?,
        wired: wired(root),
    })
}

/// The goals and guideposts in a `goals.md`. Goals are `### Title · status` headings under
/// `## Goals` (a heading without a known status is active); guideposts are the `- ` lines
/// under `## Guideposts`. Placeholder lines in parentheses are not counted.
pub fn parse_goals(text: &str) -> (Vec<Goal>, Vec<String>) {
    let mut goals: Vec<Goal> = Vec::new();
    let mut guideposts = Vec::new();
    let mut section = String::new();
    for line in text.lines() {
        if let Some(h) = line.strip_prefix("## ") {
            section = h.trim().to_lowercase();
            continue;
        }
        if section.starts_with("guidepost") {
            if let Some(g) = line.trim().strip_prefix("- ") {
                if !g.trim().is_empty() {
                    guideposts.push(g.trim().to_string());
                }
            }
        } else if section.starts_with("goal") {
            if let Some(h) = line.strip_prefix("### ") {
                let (title, status) = split_goal_heading(h);
                goals.push(Goal {
                    title,
                    status,
                    detail: String::new(),
                });
            } else if let Some(g) = goals.last_mut() {
                let t = line.trim();
                let placeholder = t.starts_with('(') && t.ends_with(')');
                if !t.is_empty() && !placeholder {
                    if !g.detail.is_empty() {
                        g.detail.push('\n');
                    }
                    g.detail.push_str(t);
                }
            }
        }
    }
    (goals, guideposts)
}

/// "Click stats · active" → ("Click stats", "active").
fn split_goal_heading(h: &str) -> (String, String) {
    if let Some((title, status)) = h.rsplit_once(" · ") {
        let status = status.trim().to_lowercase();
        if goal_status::ALL.contains(&status.as_str()) {
            return (title.trim().to_string(), status);
        }
    }
    (h.trim().to_string(), goal_status::ACTIVE.to_string())
}

/// Adds a goal, or changes the status (and detail) of the goal with that title. Commits
/// `goals.md`.
pub fn set_goal(store: &Store, p: &KnowledgeGoalParams) -> Result<KnowledgeAccepted, ServiceError> {
    let project = project_of(store, &p.project)?;
    let root = Path::new(&project.path);
    let title = p.title.trim();
    if title.is_empty() || title.contains('\n') || title.len() > MAX_TITLE {
        return Err(invalid(format!(
            "a goal needs a title: one line, at most {MAX_TITLE} characters"
        )));
    }
    if let Some(s) = &p.status {
        if !goal_status::ALL.contains(&s.as_str()) {
            return Err(invalid(format!(
                "unknown status '{s}': one of {}",
                goal_status::ALL.join(", ")
            )));
        }
    }
    let path = abs(root, knowledge_files::GOALS);
    let dir = root.join(".corvx");
    fs::create_dir_all(&dir).map_err(|e| io_error(&dir, e))?;
    let current =
        fs::read_to_string(&path).unwrap_or_else(|_| header(knowledge_files::GOALS, &project.name));
    let key = normalize(title);
    let mut lines: Vec<String> = current.lines().map(str::to_string).collect();
    let mut in_goals = false;
    let mut found = None;
    for (i, line) in lines.iter().enumerate() {
        if let Some(h) = line.strip_prefix("## ") {
            in_goals = h.trim().to_lowercase().starts_with("goal");
        } else if in_goals {
            if let Some(h) = line.strip_prefix("### ") {
                if normalize(&split_goal_heading(h).0) == key {
                    found = Some(i);
                }
            }
        }
    }
    let status;
    let out = match found {
        Some(i) => {
            let (old_title, old_status) = split_goal_heading(lines[i].trim_start_matches("### "));
            status = p.status.clone().unwrap_or(old_status);
            lines[i] = format!("### {old_title} · {status}");
            if let Some(detail) = p.detail.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
                // The detail runs to the next heading.
                let end = (i + 1..lines.len())
                    .find(|&j| lines[j].starts_with("#"))
                    .unwrap_or(lines.len());
                lines.splice(
                    i + 1..end,
                    ["".to_string(), detail.to_string(), "".to_string()],
                );
            }
            let mut text = lines.join("\n");
            text.push('\n');
            text
        }
        None => {
            status = p
                .status
                .clone()
                .unwrap_or_else(|| goal_status::ACTIVE.into());
            let mut text = current.clone();
            if !text.to_lowercase().contains("\n## goals")
                && !text.to_lowercase().starts_with("## goals")
            {
                text = format!("{}\n\n## Goals\n", text.trim_end());
            }
            let detail = p.detail.as_deref().map(str::trim).unwrap_or("");
            let mut section = format!("### {title} · {status}\n");
            if !detail.is_empty() {
                section.push_str(&format!("\n{detail}\n"));
            }
            let mut text = text.trim_end().to_string();
            text.push_str("\n\n");
            text.push_str(&section);
            text
        }
    };
    fs::write(&path, out).map_err(|e| io_error(&path, e))?;
    let rel = rel_path(knowledge_files::GOALS);
    let (committed, note) = commit(
        root,
        std::slice::from_ref(&rel),
        &format!("Goals: {title} ({status})"),
    );
    store.append_event(
        "knowledge.goal",
        &json!({"project": project.id, "title": title, "status": status,
                "new": found.is_none(), "committed": committed}),
    )?;
    Ok(KnowledgeAccepted {
        file: rel,
        committed,
        note,
    })
}

/// The journal's files, relative to the project, newest first.
fn journal_files(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root.join(".corvx").join("journal"))
        .map(|dir| {
            dir.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".md"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names.reverse();
    names
        .into_iter()
        .map(|n| format!(".corvx/journal/{n}"))
        .collect()
}

/// Whether `CLAUDE.md` or `AGENTS.md` points agents at the knowledge files.
pub fn wired(root: &Path) -> bool {
    ["CLAUDE.md", "AGENTS.md"].iter().any(|f| {
        fs::read_to_string(root.join(f))
            .map(|t| t.contains(WIRE_START))
            .unwrap_or(false)
    })
}

/// The project's map, generated from its files. Deterministic, so it changes only when the
/// project does.
pub fn map_text(p: &Project) -> String {
    let root = Path::new(&p.path);
    let s = scan(root);
    let mut out = format!(
        "# Map of {}\n\nGenerated by CORVX from the project's files. Do not edit: it is rewritten when knowledge changes.\n",
        p.name
    );
    out.push_str("\n## Stack\n\n");
    out.push_str(&if s.stack.is_empty() {
        "Not detected.\n".to_string()
    } else {
        format!("{}\n", s.stack.join(", "))
    });
    out.push_str("\n## Commands\n\n");
    let mut any = false;
    for (what, cmd) in [("test", &s.test_command), ("build", &s.build_command)] {
        if let Some(cmd) = cmd {
            out.push_str(&format!("- {what}: `{cmd}`\n"));
            any = true;
        }
    }
    if !any {
        out.push_str("None detected.\n");
    }
    let files = project_files(root);
    out.push_str("\n## Structure\n\n");
    out.push_str(&structure(&files));
    let entry: Vec<&String> = files
        .iter()
        .filter(|f| is_entry_point(f))
        .take(15)
        .collect();
    if !entry.is_empty() {
        out.push_str("\n## Entry points\n\n");
        for f in entry {
            out.push_str(&format!("- `{f}`\n"));
        }
    }
    out
}

/// The project's files, relative and sorted: git's tracked files, or a shallow walk outside
/// git. CORVX's own folder is left out.
fn project_files(root: &Path) -> Vec<String> {
    let tracked = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            o.stdout
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect::<Vec<_>>()
        });
    let mut files = tracked.unwrap_or_else(|| {
        let mut found = Vec::new();
        walk(root, root, 0, &mut found);
        found
    });
    files.retain(|f| !f.starts_with(".corvx/"));
    files.sort();
    files
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    const SKIP: [&str; 7] = [
        "target",
        "node_modules",
        "dist",
        "build",
        "venv",
        "__pycache__",
        "vendor",
    ];
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || SKIP.contains(&name.as_str()) {
            continue;
        }
        let path = e.path();
        if path.is_dir() {
            if depth < 3 {
                walk(root, &path, depth + 1, out);
            }
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_string_lossy().into_owned());
        }
    }
}

/// Top-level files, and each top-level folder with its file count and subfolders.
fn structure(files: &[String]) -> String {
    use std::collections::BTreeMap;
    let mut dirs: BTreeMap<&str, (usize, Vec<&str>)> = BTreeMap::new();
    let mut top = Vec::new();
    for f in files {
        let mut parts = f.splitn(3, '/');
        let first = parts.next().unwrap_or_default();
        match (parts.next(), parts.next()) {
            (None, _) => top.push(first),
            (Some(second), rest) => {
                let d = dirs.entry(first).or_default();
                d.0 += 1;
                if rest.is_some() && !d.1.contains(&second) {
                    d.1.push(second);
                }
            }
        }
    }
    let mut out = String::new();
    for (name, (count, subs)) in dirs.iter().take(40) {
        let s = if *count == 1 { "" } else { "s" };
        out.push_str(&format!("- `{name}/` ({count} file{s})"));
        if !subs.is_empty() {
            let shown: Vec<String> = subs.iter().take(10).map(|d| format!("`{d}/`")).collect();
            let more = if subs.len() > 10 { ", …" } else { "" };
            out.push_str(&format!(": {}{more}", shown.join(", ")));
        }
        out.push('\n');
    }
    for f in top.iter().take(40) {
        out.push_str(&format!("- `{f}`\n"));
    }
    if out.is_empty() {
        out.push_str("No files yet.\n");
    }
    out
}

fn is_entry_point(f: &str) -> bool {
    const NAMES: [&str; 12] = [
        "main.rs",
        "main.go",
        "main.py",
        "__main__.py",
        "app.py",
        "server.py",
        "manage.py",
        "index.js",
        "index.ts",
        "main.ts",
        "server.js",
        "index.html",
    ];
    let depth = f.matches('/').count();
    let name = f.rsplit('/').next().unwrap_or(f);
    depth <= 3 && NAMES.contains(&name)
}

fn header(file: &str, name: &str) -> String {
    match file {
        knowledge_files::DECISIONS => format!(
            "# Decisions\n\nArchitecture and product decisions for {name}, with dates and reasons. \
             Changed through proposals accepted in CORVX.\n"
        ),
        knowledge_files::GOALS => format!(
            "# Goals\n\nWhere {name} is heading, and what guides the work. Yours: change it any \
             time (CORVX commits it). Orchestrators read it, say which goal a mission serves, and \
             may propose new goals or marking one done.\n\n\
             ## Guideposts\n\n\
             (Principles every mission follows, one per line starting with \"- \". For example: \
             - Keep it dependency-free.)\n\n\
             ## Goals\n\n\
             (One \"### Title · status\" heading per goal; the status is active, next, done or \
             dropped. Below it: what done looks like, and why it matters.)\n"
        ),
        knowledge_files::CONVENTIONS => format!(
            "# Conventions\n\nHow things are done in {name}: style, patterns, testing rules, pitfalls. \
             Changed through proposals accepted in CORVX.\n"
        ),
        _ => format!(
            "# {name}\n\n## What it is\n\n(What the project does, and who it is for.)\n\n\
             ## Current priorities\n\n(What matters now.)\n"
        ),
    }
}

fn wiring(for_claude: bool) -> String {
    let files = if for_claude {
        knowledge_files::ALL
            .iter()
            .chain(["map"].iter())
            .map(|f| format!("@{}", rel_path(f)))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        knowledge_files::ALL
            .iter()
            .chain(["map"].iter())
            .map(|f| format!("- `{}`", rel_path(f)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let intro = if for_claude {
        "Where this project is heading and what has been learned about it. Follow the \
         guideposts in goals.md, and cite the decisions and conventions that shape your plan."
    } else {
        "Read these before you start: where this project is heading and what has been learned \
         about it. Follow the guideposts in goals.md, and cite the decisions and conventions \
         that shape your plan."
    };
    format!(
        "{WIRE_START}\n## Project knowledge\n\n{intro}\n\n{files}\n\nPast missions are in \
         `.corvx/journal/`. To change these files, propose the change with CORVX's \
         `propose_knowledge` tool; do not edit them yourself.\n{WIRE_END}\n"
    )
}

/// The text with CORVX's block replaced by `block`, or None if it is already the same.
fn replace_block(text: &str, block: &str) -> Option<String> {
    let start = text.find(WIRE_START)?;
    let end = text[start..].find(WIRE_END)? + start + WIRE_END.len();
    let end = if text[end..].starts_with('\n') {
        end + 1
    } else {
        end
    };
    if &text[start..end] == block {
        return None;
    }
    Some(format!("{}{}{}", &text[..start], block, &text[end..]))
}

/// Appends `text` to a file, starting on a new line.
fn append(path: &Path, current: Option<String>, text: &str) -> Result<(), ServiceError> {
    let mut out = current.unwrap_or_default();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(text);
    fs::write(path, out).map_err(|e| io_error(path, e))
}

/// Creates the missing knowledge files, refreshes the map, and points `CLAUDE.md` (and an
/// existing `AGENTS.md`) at them. Commits what it wrote.
pub fn init(store: &Store, project: &str) -> Result<KnowledgeInit, ServiceError> {
    let p = project_of(store, project)?;
    let root = Path::new(&p.path);
    let dir = root.join(".corvx");
    fs::create_dir_all(&dir).map_err(|e| io_error(&dir, e))?;
    let mut written = Vec::new();
    for name in knowledge_files::ALL {
        let path = abs(root, name);
        if !path.exists() {
            fs::write(&path, header(name, &p.name)).map_err(|e| io_error(&path, e))?;
            written.push(rel_path(name));
        }
    }
    if write_map(&p)? {
        written.push(rel_path("map"));
    }
    for (file, for_claude) in [("CLAUDE.md", true), ("AGENTS.md", false)] {
        let path = root.join(file);
        let current = fs::read_to_string(&path).ok();
        if let Some(text) = current.as_deref().filter(|t| t.contains(WIRE_START)) {
            // Set up before: bring the block up to date (e.g. a knowledge file added since).
            if let Some(updated) = replace_block(text, &wiring(for_claude)) {
                fs::write(&path, updated).map_err(|e| io_error(&path, e))?;
                written.push(file.to_string());
            }
            continue;
        }
        match current {
            None if for_claude => {
                let text = format!("# {}\n\n{}", p.name, wiring(true));
                fs::write(&path, text).map_err(|e| io_error(&path, e))?;
            }
            None => continue,
            Some(text) => append(&path, Some(text), &wiring(for_claude))?,
        }
        written.push(file.to_string());
    }
    let (committed, note) = if written.is_empty() {
        (false, Some("nothing to do: already set up".to_string()))
    } else {
        commit(root, &written, "CORVX: project knowledge files")
    };
    store.append_event(
        "knowledge.init",
        &json!({"project": p.id, "written": written, "committed": committed}),
    )?;
    Ok(KnowledgeInit {
        written,
        committed,
        note,
    })
}

/// Commits a knowledge file the human edited by hand (the brief, usually), if it changed.
pub fn commit_file(
    store: &Store,
    project: &str,
    file: &str,
) -> Result<KnowledgeInit, ServiceError> {
    let p = project_of(store, project)?;
    if !knowledge_files::ALL.contains(&file) {
        return Err(invalid(format!("unknown file '{file}'")));
    }
    let root = Path::new(&p.path);
    let rel = rel_path(file);
    let changed = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain", "--", &rel])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false);
    if !changed {
        return Ok(KnowledgeInit {
            written: vec![],
            committed: false,
            note: Some(format!("{rel} has no changes to commit")),
        });
    }
    let (committed, note) = commit(
        root,
        std::slice::from_ref(&rel),
        &format!("Knowledge: update {file}"),
    );
    store.append_event(
        "knowledge.edited",
        &json!({"project": p.id, "file": file, "committed": committed}),
    )?;
    Ok(KnowledgeInit {
        written: vec![rel],
        committed,
        note,
    })
}

/// Rewrites `map.md` if its content changed. Returns whether it did.
fn write_map(p: &Project) -> Result<bool, ServiceError> {
    let path = abs(Path::new(&p.path), "map");
    let text = map_text(p);
    if fs::read_to_string(&path).ok().as_deref() == Some(text.as_str()) {
        return Ok(false);
    }
    fs::write(&path, text).map_err(|e| io_error(&path, e))?;
    Ok(true)
}

/// Commits exactly these paths in the project's checkout, leaving anything else (staged or
/// not) alone. Returns whether it committed, and why not.
pub fn commit(root: &Path, paths: &[String], message: &str) -> (bool, Option<String>) {
    let git = |args: &[&str]| Command::new("git").arg("-C").arg(root).args(args).output();
    match git(&["rev-parse", "--is-inside-work-tree"]) {
        Ok(o) if o.status.success() => {}
        _ => return (false, Some("not a git repository: not committed".into())),
    }
    let mut add = vec!["add", "--"];
    add.extend(paths.iter().map(String::as_str));
    let mut commit = vec!["commit", "-q", "-m", message, "--"];
    commit.extend(paths.iter().map(String::as_str));
    for args in [add, commit] {
        match git(&args) {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                let out = String::from_utf8_lossy(&o.stdout);
                let why = format!("{} {}", err.trim(), out.trim());
                return (
                    false,
                    Some(format!("written, not committed: {}", why.trim())),
                );
            }
            Err(e) => return (false, Some(format!("written, not committed: {e}"))),
        }
    }
    (true, None)
}

/// Lowercase letters and digits only, so near-identical titles compare equal.
fn normalize(title: &str) -> String {
    title
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Records a proposal from a session and puts it in the human's inbox.
pub fn propose(
    store: &Store,
    p: &KnowledgeProposeParams,
    now: i64,
) -> Result<Proposal, ServiceError> {
    let (session, _) = store.get_session(&p.session)?.ok_or_else(|| {
        ServiceError::Rpc(
            codes::SESSION_NOT_FOUND,
            format!("no session '{}'", p.session),
        )
    })?;
    let project = project_of(store, p.project.as_deref().unwrap_or(&session.project))?;
    if !knowledge_files::ALL.contains(&p.file.as_str()) {
        return Err(invalid(format!(
            "unknown file '{}': one of {}",
            p.file,
            knowledge_files::ALL.join(", ")
        )));
    }
    let title = p.title.trim();
    let text = p.text.trim();
    if title.is_empty() || title.contains('\n') || title.len() > MAX_TITLE {
        return Err(invalid(format!(
            "a title is required: one line, at most {MAX_TITLE} characters"
        )));
    }
    if text.is_empty() || text.len() > MAX_PROPOSAL {
        return Err(invalid(format!(
            "text is required, at most {MAX_PROPOSAL} characters: keep it to what later work needs"
        )));
    }
    let replaces = p
        .replaces
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty());
    if let Some(old) = replaces {
        let current =
            fs::read_to_string(abs(Path::new(&project.path), &p.file)).unwrap_or_default();
        if current.matches(old).count() != 1 {
            return Err(ServiceError::Rpc(
                codes::KNOWLEDGE_CONFLICT,
                format!(
                    "the text to replace must appear exactly once in {}",
                    rel_path(&p.file)
                ),
            ));
        }
    }
    let key = normalize(title);
    if let Some(r) = store
        .rejected_proposals(&project.id, 1000)?
        .into_iter()
        .find(|r| r.file == p.file && normalize(&r.title) == key)
    {
        return Err(invalid(format!(
            "the human rejected this before ({}): do not propose it again",
            r.reason.as_deref().unwrap_or("no reason given")
        )));
    }
    if store
        .list_proposals(Some(&project.id), false)?
        .iter()
        .any(|o| o.file == p.file && normalize(&o.title) == key)
    {
        return Err(invalid("already proposed and waiting for the human"));
    }
    let mut proposal = Proposal {
        id: String::new(),
        project: project.id.clone(),
        file: p.file.clone(),
        title: title.to_string(),
        text: text.to_string(),
        replaces: replaces.map(str::to_string),
        evidence: p
            .evidence
            .as_deref()
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(str::to_string),
        by: p.session.clone(),
        status: "open".into(),
        reason: None,
        created_at: now,
        decided_at: None,
    };
    proposal.id = store.insert_proposal(&proposal, now)?;
    store.append_event(
        "knowledge.proposed",
        &json!({"id": proposal.id, "project": proposal.project, "file": proposal.file,
                "title": proposal.title, "by": proposal.by}),
    )?;
    Ok(proposal)
}

fn open_proposal(store: &Store, id: &str) -> Result<Proposal, ServiceError> {
    store
        .get_proposal(id)?
        .filter(|p| p.status == "open")
        .ok_or_else(|| {
            ServiceError::Rpc(
                codes::PROPOSAL_NOT_FOUND,
                format!("no open proposal '{id}'"),
            )
        })
}

/// Accepts a proposal, with the human's edits if any: writes it into its file and commits.
pub fn accept(
    store: &Store,
    id: &str,
    edited: Option<&str>,
    now: i64,
) -> Result<KnowledgeAccepted, ServiceError> {
    let p = open_proposal(store, id)?;
    let project = project_of(store, &p.project)?;
    let root = Path::new(&project.path);
    let text = edited.map(str::trim).unwrap_or(&p.text).to_string();
    if text.is_empty() {
        return Err(invalid(
            "the accepted text is empty: reject the proposal instead",
        ));
    }
    let dir = root.join(".corvx");
    fs::create_dir_all(&dir).map_err(|e| io_error(&dir, e))?;
    let path = abs(root, &p.file);
    let current = fs::read_to_string(&path).ok();
    match &p.replaces {
        Some(old) => {
            let current = current.unwrap_or_default();
            if current.matches(old.as_str()).count() != 1 {
                return Err(ServiceError::Rpc(
                    codes::KNOWLEDGE_CONFLICT,
                    format!(
                        "{} changed: the text this proposal replaces is no longer there once. \
                         Reject it, or accept an edited version as an addition.",
                        rel_path(&p.file)
                    ),
                ));
            }
            fs::write(&path, current.replacen(old.as_str(), &text, 1))
                .map_err(|e| io_error(&path, e))?;
        }
        None if p.file == knowledge_files::GOALS => {
            // A proposed goal becomes a goal heading; it is active once you accept it.
            let current = current.unwrap_or_else(|| header(&p.file, &project.name));
            let section = format!("### {} · {}\n\n{}\n", p.title, goal_status::ACTIVE, text);
            append(&path, Some(current), &section)?;
        }
        None => {
            let current = current.unwrap_or_else(|| header(&p.file, &project.name));
            let mut section = format!("## {}\n\n{}\n", p.title, text);
            if p.file != knowledge_files::BRIEF {
                section.push_str(&format!("\n_Accepted {}", date_of(now)));
                if let Some(e) = &p.evidence {
                    section.push_str(&format!(". Evidence: {}", e.replace('\n', " ")));
                }
                section.push_str("_\n");
            }
            append(&path, Some(current), &section)?;
        }
    }
    let mut paths = vec![rel_path(&p.file)];
    if abs(root, "map").is_file() && write_map(&project)? {
        paths.push(rel_path("map"));
    }
    let (committed, note) = commit(root, &paths, &format!("Knowledge: {}", p.title));
    let edited = edited.is_some_and(|e| e.trim() != p.text);
    store.decide_proposal(id, "accepted", edited.then_some(text.as_str()), None, now)?;
    store.append_event(
        "knowledge.accepted",
        &json!({"id": id, "project": p.project, "file": p.file, "title": p.title,
                "edited": edited, "committed": committed}),
    )?;
    Ok(KnowledgeAccepted {
        file: rel_path(&p.file),
        committed,
        note,
    })
}

/// Rejects a proposal. The reason is kept, and the same proposal is refused from then on.
pub fn reject(store: &Store, id: &str, reason: Option<&str>, now: i64) -> Result<(), ServiceError> {
    let p = open_proposal(store, id)?;
    let reason = reason.map(str::trim).filter(|r| !r.is_empty());
    store.decide_proposal(id, "rejected", None, reason, now)?;
    store.append_event(
        "knowledge.rejected",
        &json!({"id": id, "project": p.project, "file": p.file, "title": p.title,
                "reason": reason}),
    )?;
    Ok(())
}

/// The UTC date of a unix time, as YYYY-MM-DD.
pub fn date_of(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// The UTC date and time of a unix time, as YYYY-MM-DD HH:MM UTC.
pub fn time_of(secs: i64) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use corvx_proto::{AgentState, Session, SessionState};
    use tempfile::tempdir;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    /// A store with a git project "app" and a session "s1" in it.
    fn setup() -> (tempfile::TempDir, Store) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"app\"\n").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("README.md"), "# App\n").unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", "init"]);
        let store = Store::open_in_memory().unwrap();
        store
            .insert_project(&Project {
                id: "app".into(),
                name: "App".into(),
                path: root.display().to_string(),
                workspace: "default".into(),
                added_at: 1,
                stack: vec!["rust".into()],
                test_command: Some("cargo test".into()),
                build_command: None,
                branch: Some("main".into()),
                autonomy: "guided".into(),
            })
            .unwrap();
        let session = Session {
            id: "s1".into(),
            project: "app".into(),
            command: vec!["claude".into()],
            runtime: Some("claude".into()),
            backend: "tmux".into(),
            state: SessionState::Running,
            pid: None,
            started_at: 1,
            ended_at: None,
            exit_code: None,
            agent_state: AgentState::Working,
            agent_detail: None,
            agent_source: None,
            supervisor: None,
            worktree: None,
            label: None,
            runtime_session: None,
            autopilot: false,
            agent_model: None,
            human_approves_agents: false,
        };
        store.save_session(&session, false).unwrap();
        (dir, store)
    }

    fn propose_params(file: &str, title: &str, text: &str) -> KnowledgeProposeParams {
        KnowledgeProposeParams {
            session: "s1".into(),
            project: None,
            file: file.into(),
            title: title.into(),
            text: text.into(),
            replaces: None,
            evidence: Some("mission s1: the review found it".into()),
        }
    }

    fn last_commit(root: &Path) -> String {
        let o = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["log", "-1", "--format=%s", "--name-only"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    #[test]
    fn init_creates_the_files_wires_claude_md_and_commits() {
        let (dir, store) = setup();
        let root = dir.path();
        let done = init(&store, "app").unwrap();
        assert!(done.committed, "{done:?}");
        for f in [
            ".corvx/brief.md",
            ".corvx/decisions.md",
            ".corvx/conventions.md",
            ".corvx/map.md",
            "CLAUDE.md",
        ] {
            assert!(done.written.contains(&f.to_string()), "{f}: {done:?}");
            assert!(last_commit(root).contains(f), "{f} committed");
        }
        let claude = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
        assert!(claude.contains("@.corvx/decisions.md"), "{claude}");
        assert!(wired(root));
        let map = fs::read_to_string(root.join(".corvx/map.md")).unwrap();
        assert!(map.contains("- test: `cargo test`"), "{map}");
        assert!(map.contains("- `src/` (1 file)"), "{map}");
        assert!(map.contains("- `src/main.rs`"), "entry point: {map}");

        // Again: nothing to do, and an existing CLAUDE.md is not wired twice.
        // (The map now lists CLAUDE.md, committed by the first run, so it is refreshed once.)
        init(&store, "app").unwrap();
        let again = init(&store, "app").unwrap();
        assert!(again.written.is_empty(), "{again:?}");
        let claude2 = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
        assert_eq!(claude, claude2);
    }

    #[test]
    fn goals_are_parsed_added_and_changed() {
        let (dir, store) = setup();
        let root = dir.path();
        init(&store, "app").unwrap();
        // The template has placeholders only.
        let k = read(&store, "app", 0).unwrap();
        assert!(
            k.goals.is_empty() && k.guideposts.is_empty(),
            "{:?} {:?}",
            k.goals,
            k.guideposts
        );
        assert_eq!(k.files[1].name, "goals");

        let add = |title: &str, status: Option<&str>, detail: Option<&str>| {
            set_goal(
                &store,
                &KnowledgeGoalParams {
                    project: "app".into(),
                    title: title.into(),
                    status: status.map(Into::into),
                    detail: detail.map(Into::into),
                },
            )
            .unwrap()
        };
        let done = add(
            "Click stats",
            None,
            Some("Each link shows its clicks per day."),
        );
        assert!(done.committed, "{done:?}");
        assert!(last_commit(root).starts_with("Goals: Click stats (active)"));
        add("Rate limiting", Some("next"), None);
        // The same title (any case) changes the goal instead of adding another.
        add("click STATS", Some("done"), None);
        let goals_md = fs::read_to_string(root.join(".corvx/goals.md")).unwrap();
        fs::write(
            root.join(".corvx/goals.md"),
            goals_md.replace(
                "## Guideposts\n",
                "## Guideposts\n\n- No new dependencies.\n- Keep the API stable.\n",
            ),
        )
        .unwrap();
        let k = read(&store, "app", 0).unwrap();
        let summary: Vec<(String, String)> = k
            .goals
            .iter()
            .map(|g| (g.title.clone(), g.status.clone()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("Click stats".to_string(), "done".to_string()),
                ("Rate limiting".to_string(), "next".to_string())
            ]
        );
        assert_eq!(k.goals[0].detail, "Each link shows its clicks per day.");
        assert_eq!(
            k.guideposts,
            vec!["No new dependencies.", "Keep the API stable."]
        );

        let bad = set_goal(
            &store,
            &KnowledgeGoalParams {
                project: "app".into(),
                title: "X".into(),
                status: Some("someday".into()),
                detail: None,
            },
        );
        assert!(bad.is_err());

        // An accepted goal proposal becomes an active goal.
        let p = propose(
            &store,
            &propose_params("goals", "Custom codes", "Users pick a code."),
            5,
        )
        .unwrap();
        accept(&store, &p.id, None, 6).unwrap();
        let k = read(&store, "app", 0).unwrap();
        assert!(
            k.goals
                .iter()
                .any(|g| g.title == "Custom codes" && g.status == "active"),
            "{:?}",
            k.goals
        );
    }

    #[test]
    fn init_brings_an_old_block_up_to_date() {
        let (dir, store) = setup();
        let root = dir.path();
        init(&store, "app").unwrap();
        // As set up before goals existed.
        let claude = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
        fs::write(
            root.join("CLAUDE.md"),
            claude.replace("@.corvx/goals.md\n", ""),
        )
        .unwrap();
        fs::remove_file(root.join(".corvx/goals.md")).unwrap();
        git(root, &["commit", "-qam", "old"]);
        let done = init(&store, "app").unwrap();
        assert!(done.written.contains(&"CLAUDE.md".to_string()), "{done:?}");
        assert!(
            done.written.contains(&".corvx/goals.md".to_string()),
            "{done:?}"
        );
        let claude = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
        assert!(claude.contains("@.corvx/goals.md"), "{claude}");
        assert_eq!(claude.matches(WIRE_START).count(), 1, "{claude}");
    }

    #[test]
    fn an_existing_claude_md_keeps_its_text_and_gets_the_block() {
        let (dir, store) = setup();
        let root = dir.path();
        fs::write(root.join("CLAUDE.md"), "# Mine\n\nUse tabs.").unwrap();
        fs::write(root.join("AGENTS.md"), "# Agents\n").unwrap();
        init(&store, "app").unwrap();
        let claude = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
        assert!(
            claude.starts_with("# Mine\n\nUse tabs.\n\n<!-- corvx:knowledge -->"),
            "{claude}"
        );
        let agents = fs::read_to_string(root.join("AGENTS.md")).unwrap();
        assert!(agents.contains("- `.corvx/conventions.md`"), "{agents}");
    }

    #[test]
    fn accepting_a_proposal_writes_and_commits_only_that_file() {
        let (dir, store) = setup();
        let root = dir.path();
        // Unrelated work in progress stays as it is.
        fs::write(root.join("README.md"), "# App\n\nchanged\n").unwrap();
        let p = propose(
            &store,
            &propose_params(
                "conventions",
                "Errors use problem+json",
                "API errors are RFC 7807.",
            ),
            100,
        )
        .unwrap();
        let item = store.get_attention(&p.id).unwrap().unwrap();
        assert_eq!(item.kind, "proposal");
        assert_eq!(item.summary, "conventions: Errors use problem+json");

        // The agent moving on does not close it.
        store
            .set_session_attention("s1", "app", "working", "", 0, 110)
            .unwrap();
        assert_eq!(store.get_attention(&p.id).unwrap().unwrap().status, "open");

        let done = accept(&store, &p.id, None, 1_794_527_068).unwrap();
        assert!(done.committed, "{done:?}");
        let text = fs::read_to_string(root.join(".corvx/conventions.md")).unwrap();
        assert!(text.starts_with("# Conventions"), "{text}");
        assert!(
            text.contains("## Errors use problem+json\n\nAPI errors are RFC 7807.\n\n_Accepted 2026-11-12. Evidence: mission s1: the review found it_"),
            "{text}"
        );
        let log = last_commit(root);
        assert!(
            log.starts_with("Knowledge: Errors use problem+json"),
            "{log}"
        );
        assert!(!log.contains("README.md"), "{log}");
        assert_eq!(
            store.get_attention(&p.id).unwrap().unwrap().status,
            "answered"
        );
        assert!(accept(&store, &p.id, None, 200).is_err(), "only once");

        let k = read(&store, "app", 2).unwrap();
        assert!(k.files[3].text.contains("RFC 7807"));
        assert!(k.open.is_empty());
    }

    #[test]
    fn the_human_can_edit_before_accepting() {
        let (dir, store) = setup();
        let p = propose(
            &store,
            &propose_params("decisions", "Use SQLite", "Use SQLite."),
            100,
        )
        .unwrap();
        accept(&store, &p.id, Some("Use SQLite, one file, WAL mode."), 200).unwrap();
        let text = fs::read_to_string(dir.path().join(".corvx/decisions.md")).unwrap();
        assert!(text.contains("Use SQLite, one file, WAL mode."), "{text}");
        assert_eq!(
            store.get_proposal(&p.id).unwrap().unwrap().text,
            "Use SQLite, one file, WAL mode."
        );
    }

    #[test]
    fn edits_replace_text_that_is_still_there() {
        let (dir, store) = setup();
        let path = dir.path().join(".corvx/conventions.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "# Conventions\n\n## Tests\n\nRun pytest.\n").unwrap();
        let mut params = propose_params(
            "conventions",
            "Tests run with unittest",
            "Run python3 -m unittest.",
        );
        params.replaces = Some("Run pytest.".into());
        let p = propose(&store, &params, 100).unwrap();
        accept(&store, &p.id, None, 200).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# Conventions\n\n## Tests\n\nRun python3 -m unittest.\n"
        );

        // Text that is not there cannot be replaced.
        params.title = "Other".into();
        params.replaces = Some("Run pytest.".into());
        let err = propose(&store, &params, 300).unwrap_err();
        assert!(matches!(err, ServiceError::Rpc(c, _) if c == codes::KNOWLEDGE_CONFLICT));
    }

    #[test]
    fn rejected_proposals_are_not_proposed_again() {
        let (_dir, store) = setup();
        let p = propose(
            &store,
            &propose_params("decisions", "Use Redis", "Cache in Redis."),
            100,
        )
        .unwrap();
        // Proposed twice while open: refused.
        assert!(propose(&store, &propose_params("decisions", "use redis!", "x"), 110).is_err());
        reject(&store, &p.id, Some("no external services"), 120).unwrap();
        let err = propose(
            &store,
            &propose_params("decisions", "Use  Redis", "Again."),
            130,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no external services"), "{err}");
        let k = read(&store, "app", 2).unwrap();
        assert_eq!(k.rejected.len(), 1);
        assert_eq!(
            k.rejected[0].reason.as_deref(),
            Some("no external services")
        );
    }

    #[test]
    fn proposals_need_a_known_file_a_title_and_text() {
        let (_dir, store) = setup();
        for (file, title, text) in [
            ("map", "t", "x"),
            ("secrets", "t", "x"),
            ("decisions", "", "x"),
            ("decisions", "two\nlines", "x"),
            ("decisions", "t", "  "),
        ] {
            assert!(
                propose(&store, &propose_params(file, title, text), 100).is_err(),
                "{file} {title:?} {text:?}"
            );
        }
    }

    #[test]
    fn read_lists_the_newest_journal_entries() {
        let (dir, store) = setup();
        let j = dir.path().join(".corvx/journal");
        fs::create_dir_all(&j).unwrap();
        fs::write(j.join("2026-11-10-s1.md"), "old").unwrap();
        fs::write(j.join("2026-11-12-s2.md"), "new").unwrap();
        fs::write(j.join("2026-11-11-s3.md"), "mid").unwrap();
        let k = read(&store, "app", 2).unwrap();
        assert_eq!(k.journal_count, 3);
        let texts: Vec<_> = k.journal.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["new", "mid"]);
        assert!(!k.wired);
        assert!(!k.files[0].exists);
        assert!(k.files[4].text.contains("# Map of App"));
    }

    #[test]
    fn a_brief_edited_by_hand_is_committed() {
        let (dir, store) = setup();
        init(&store, "app").unwrap();
        let none = commit_file(&store, "app", "brief").unwrap();
        assert!(!none.committed, "{none:?}");
        fs::write(dir.path().join(".corvx/brief.md"), "# App\n\nA test app.\n").unwrap();
        let done = commit_file(&store, "app", "brief").unwrap();
        assert!(done.committed, "{done:?}");
        assert!(last_commit(dir.path()).starts_with("Knowledge: update brief"));
    }

    #[test]
    fn dates_are_utc_calendar_dates() {
        assert_eq!(date_of(0), "1970-01-01");
        assert_eq!(date_of(951_782_400), "2000-02-29");
        assert_eq!(time_of(1_794_527_068), "2026-11-12 23:44 UTC");
    }
}
