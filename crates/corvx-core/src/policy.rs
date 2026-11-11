//! Rules CORVX applies to agents' tool calls, whoever would otherwise decide them.
//!
//! An orchestrator may decide its agents' requests, and autopilot approves a session's
//! requests without asking. A rule overrides both, and Claude Code's own settings: a matching
//! request is asked of the human, allowed at once, or denied at once, as the rule says. When
//! several rules match, the strictest wins (deny, then ask, then allow). CORVX checks the
//! rules itself; they are not instructions to a model.
//!
//! The rules match what an agent asks to run, as text. They catch the usual ways of doing
//! these things, not an agent set on getting around them (a script that deletes files does
//! not mention `rm -r`).

use std::path::{Component, Path, PathBuf};

use corvx_proto::{autonomy, rule_actions, AttentionItem, Rule};
use regex::Regex;
use serde_json::Value;

use crate::store::{Result, Store};

/// Tools whose input names a file or directory.
const FILE_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "MultiEdit",
    "NotebookEdit",
    "Glob",
    "Grep",
];
/// Tools that change files.
const WRITE_TOOLS: &[&str] = &["Write", "Edit", "MultiEdit", "NotebookEdit"];

/// The rules built into CORVX: id, kind, pattern, reason.
const BUILTIN: &[(&str, &str, &str, &str)] = &[
    (
        "git-push",
        "command",
        r"\bgit\b[^;&|\n]*\spush\b",
        "pushes to a remote",
    ),
    (
        "git-discard",
        "command",
        r"\bgit\b[^;&|\n]*\s(reset\s+--hard|clean\s+-[a-zA-Z]*f[a-zA-Z]*|branch\s+-D|stash\s+(drop|clear))\b",
        "discards commits or uncommitted work",
    ),
    (
        "delete-recursive",
        "command",
        r"\brm\b[^;&|\n]*\s(-[a-zA-Z]*[rR][a-zA-Z]*|--recursive)\b|\bfind\b[^;&|\n]*\s-delete\b",
        "deletes directories",
    ),
    (
        "publish",
        "command",
        r"\b(npm|pnpm|yarn|cargo|gem|poetry|uv)\s+publish\b|\btwine\s+upload\b|\bdocker\s+push\b|\bgh\s+(release|repo\s+create|pr\s+(create|merge))\b",
        "publishes beyond this machine",
    ),
    ("sudo", "command", r"\bsudo\b", "runs as root"),
    (
        "credentials",
        "path",
        r#"(^|[/\s'"=])(\.env(\.[\w.-]+)?|\.netrc|\.npmrc|\.pypirc|id_(rsa|ed25519|ecdsa)\w*|[\w.-]+\.(pem|p12|key))($|[\s'"])|\.ssh/|\.aws/|\.gnupg/|\.config/gh/|\.docker/config\.json|\bsecurity\s+find-"#,
        "touches credentials or keys",
    ),
    (
        "corvx-control",
        "command",
        r"\bcorvx\b[^;&|\n]*\s(rules|attention\s+(approve|deny)|session\s+autopilot)\b",
        "changes CORVX's own rules or approvals",
    ),
    (
        "outside-workdir",
        "outside",
        "",
        "writes outside the agent's working directory",
    ),
];

pub fn builtin() -> Vec<Rule> {
    BUILTIN
        .iter()
        .map(|(id, kind, pattern, reason)| Rule {
            id: id.to_string(),
            kind: kind.to_string(),
            pattern: pattern.to_string(),
            reason: reason.to_string(),
            builtin: true,
            project: None,
            action: rule_actions::ASK.into(),
            set_in: "default".into(),
        })
        .collect()
}

pub fn is_builtin(id: &str) -> bool {
    BUILTIN.iter().any(|(b, ..)| *b == id)
}

/// Checks a rule a human adds: its kind, and that its pattern compiles.
pub fn validate(kind: &str, pattern: &str) -> std::result::Result<(), String> {
    match kind {
        "command" | "path" => Regex::new(pattern)
            .map(|_| ())
            .map_err(|e| format!("invalid pattern: {e}")),
        other => Err(format!("unknown kind '{other}': use command or path")),
    }
}

/// Checks an action a human sets.
pub fn validate_action(action: &str) -> std::result::Result<(), String> {
    if rule_actions::ALL.contains(&action) {
        Ok(())
    } else {
        Err(format!(
            "unknown action '{action}': use {}",
            rule_actions::ALL.join(", ")
        ))
    }
}

/// What a built-in rule does at an autonomy level, unless you set it yourself.
pub fn level_default(level: &str, rule: &str) -> &'static str {
    match level {
        // Agents work on their own branches: discarding work there is theirs to do.
        autonomy::AUTONOMOUS if rule == "git-discard" => rule_actions::ALLOW,
        // Only changes to CORVX's own rules still ask: they are the dial itself.
        autonomy::FULL if rule != "corvx-control" => rule_actions::ALLOW,
        _ => rule_actions::ASK,
    }
}

/// Whether requests no rule covers are approved without asking anyone at this level.
pub fn approves_unasked(level: &str) -> bool {
    matches!(level, autonomy::AUTONOMOUS | autonomy::FULL)
}

/// Whether an orchestrator may decide its agents' requests, or turn on their autopilot.
pub fn orchestrator_may_decide(level: &str) -> bool {
    level != autonomy::SUPERVISED
}

pub fn validate_level(level: &str) -> std::result::Result<(), String> {
    if autonomy::ALL.contains(&level) {
        Ok(())
    } else {
        Err(format!(
            "unknown autonomy level '{level}': use {}",
            autonomy::ALL.join(", ")
        ))
    }
}

/// The rules as they apply to `project`, or, without one, as set for every project, with
/// every rule added anywhere. A project's setting wins over the one for every project, which
/// wins over the project's autonomy level, which wins over the rule's own action.
pub fn rules_for(store: &Store, project: Option<&str>) -> Result<Vec<Rule>> {
    let level = match project {
        Some(p) => store.get_project(p)?.map(|p| p.autonomy),
        None => None,
    };
    let settings = store.rule_settings()?;
    let setting = |id: &str, p: &str| {
        settings
            .iter()
            .find(|(r, sp, _)| r == id && sp == p)
            .map(|(_, _, a)| a.clone())
    };
    let mut rules = builtin();
    rules.extend(
        store.added_rules()?.into_iter().filter(|r| {
            project.is_none() || r.project.is_none() || r.project.as_deref() == project
        }),
    );
    for r in &mut rules {
        if let Some(a) = project.and_then(|p| setting(&r.id, p)) {
            r.action = a;
            r.set_in = "project".into();
        } else if let Some(a) = setting(&r.id, "") {
            r.action = a;
            r.set_in = "everywhere".into();
        } else if let Some(level) = level.as_deref().filter(|_| r.builtin) {
            let a = level_default(level, &r.id);
            if a != r.action {
                r.action = a.into();
                r.set_in = "level".into();
            }
        }
    }
    Ok(rules)
}

/// After this many approvals of the same kind of request, CORVX offers to allow it.
pub const SUGGEST_AFTER: i64 = 3;

/// Shell characters that make a command more than one command, or redirect it.
const SHELL_OPERATORS: &[char] = &[';', '&', '|', '<', '>', '`', '$', '(', ')', '\n'];

/// What kind of request an approval was, for counting: "rule:<id>" for one a CORVX rule sent
/// to the human, "cmd:<program> [<subcommand>]" for a simple shell command. None for
/// anything else (compound commands, other tools), which is not generalized.
pub fn trust_key(item: &AttentionItem) -> Option<String> {
    if let Some(rest) = item
        .note
        .as_deref()
        .and_then(|n| n.strip_prefix("CORVX rule "))
    {
        return rest.split(':').next().map(|id| format!("rule:{id}"));
    }
    let command = item.summary.strip_prefix("Bash: ")?;
    let command = command.split(" · ").next()?.trim();
    if command.contains(SHELL_OPERATORS) {
        return None;
    }
    let word = |w: &str| {
        !w.is_empty()
            && !w.starts_with('-')
            && w.chars()
                .all(|c| c.is_ascii_alphanumeric() || "_./:-@".contains(c))
    };
    let mut words = command.split_whitespace();
    let program = words.next().filter(|w| word(w))?;
    Some(match words.next().filter(|w| word(w)) {
        Some(sub) => format!("cmd:{program} {sub}"),
        None => format!("cmd:{program}"),
    })
}

/// The rule pattern that allows a "cmd:" key: that program and subcommand with any plain
/// arguments, but nothing chained, piped or redirected.
pub fn command_pattern(key: &str) -> Option<String> {
    let words: Vec<String> = key
        .strip_prefix("cmd:")?
        .split(' ')
        .map(regex::escape)
        .collect();
    Some(format!(r"^\s*{}(\s[^;&|<>`$()\n]*)?$", words.join(r"\s+")))
}

/// Counts a human's approval. When the same kind of request has been approved
/// `SUGGEST_AFTER` times in a project, opens a suggestion to allow it from now on. Returns
/// the suggestion's id when one was opened.
pub fn note_approval(store: &Store, item: &AttentionItem, now: i64) -> Result<Option<String>> {
    let Some(key) = trust_key(item) else {
        return Ok(None);
    };
    let (count, state) = store.count_approval(&item.project, &key, now)?;
    if count < SUGGEST_AFTER || state != "counting" {
        return Ok(None);
    }
    let what = match key.strip_prefix("rule:") {
        Some(rule) => {
            let reason = rules_for(store, Some(&item.project))?
                .into_iter()
                .find(|r| r.id == rule)
                .map(|r| format!(" ({})", r.reason))
                .unwrap_or_default();
            format!("requests under CORVX rule {rule}{reason}")
        }
        None => format!("`{} …`", key.trim_start_matches("cmd:")),
    };
    let summary = format!(
        "You approved {what} {count} times in {}. Allow without asking from now on?",
        item.project
    );
    let id = store.open_suggestion(&item.project, &summary, now)?;
    store.set_trust(&item.project, &key, "suggested", Some(&id), now)?;
    store.append_event(
        "trust.suggested",
        &serde_json::json!({"id": id, "project": item.project, "key": key, "approvals": count}),
    )?;
    Ok(Some(id))
}

/// The human answers a suggestion. Accepting allows that kind of request in the project: a
/// rule of its own for a command, or the rule set to allow. Returns false if `id` is not an
/// open suggestion.
pub fn answer_suggestion(store: &Store, id: &str, accept: bool, now: i64) -> Result<bool> {
    let Some((project, key)) = store.trust_of_item(id)? else {
        return Ok(false);
    };
    if !store.close_suggestion(id, if accept { "allowed" } else { "declined" }, now)? {
        return Ok(false);
    }
    let mut added = None;
    if accept {
        if let Some(rule) = key.strip_prefix("rule:") {
            store.set_rule_action(rule, &project, Some(rule_actions::ALLOW))?;
        } else if let Some(pattern) = command_pattern(&key) {
            let reason = format!("you approved `{} …` before", key.trim_start_matches("cmd:"));
            added = Some(store.add_rule(
                Some(&project),
                "command",
                &pattern,
                &reason,
                rule_actions::ALLOW,
                now,
            )?);
        }
    }
    store.set_trust(
        &project,
        &key,
        if accept { "accepted" } else { "declined" },
        None,
        now,
    )?;
    store.append_event(
        if accept {
            "trust.accepted"
        } else {
            "trust.declined"
        },
        &serde_json::json!({"id": id, "project": project, "key": key, "rule": added}),
    )?;
    Ok(true)
}

/// A suggestion dismissed from the inbox is declined: CORVX does not offer it again.
pub fn suggestion_dismissed(store: &Store, id: &str, now: i64) -> Result<()> {
    if let Some((project, key)) = store.trust_of_item(id)? {
        store.set_trust(&project, &key, "declined", None, now)?;
    }
    Ok(())
}

/// A rule a request matched, and what it does.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub rule: String,
    pub reason: String,
    /// "ask", "allow" or "deny".
    pub action: String,
}

/// The strictest rule that is not off and matches the request: `tool` with `input`, from an
/// agent working in `workdir`.
pub fn check(rules: &[Rule], tool: &str, input: &Value, workdir: &Path) -> Option<Hit> {
    let field = |k: &str| input.get(k).and_then(Value::as_str);
    let command = (tool == "Bash").then(|| field("command")).flatten();
    let path = FILE_TOOLS
        .contains(&tool)
        .then(|| {
            ["file_path", "notebook_path", "path"]
                .iter()
                .find_map(|k| field(k))
        })
        .flatten();
    let strictness = |action: &str| match action {
        rule_actions::DENY => 3,
        rule_actions::ASK => 2,
        rule_actions::ALLOW => 1,
        _ => 0,
    };
    rules
        .iter()
        .filter(|r| strictness(&r.action) > 0)
        .filter(|r| match r.kind.as_str() {
            "command" => command.is_some_and(|c| matches(&r.pattern, c)),
            "path" => {
                command.is_some_and(|c| matches(&r.pattern, c))
                    || path.is_some_and(|p| matches(&r.pattern, p))
            }
            "outside" => {
                WRITE_TOOLS.contains(&tool) && path.is_some_and(|p| !inside(workdir, Path::new(p)))
            }
            _ => false,
        })
        // The first of the strictest, so built-in rules come before added ones.
        .fold(None::<&Rule>, |best, r| match best {
            Some(b) if strictness(&b.action) >= strictness(&r.action) => Some(b),
            _ => Some(r),
        })
        .map(|r| Hit {
            rule: r.id.clone(),
            reason: r.reason.clone(),
            action: r.action.clone(),
        })
}

fn matches(pattern: &str, text: &str) -> bool {
    // Patterns are checked when added; one that does not compile matches nothing.
    Regex::new(pattern).is_ok_and(|re| re.is_match(text))
}

/// Whether `path` (relative paths are taken from `workdir`) is in `workdir` or a temporary
/// directory. Symbolic links in the parts that exist are followed.
fn inside(workdir: &Path, path: &Path) -> bool {
    let target = resolve(&workdir.join(path));
    let mut roots = vec![resolve(workdir), resolve(&std::env::temp_dir())];
    if cfg!(unix) {
        roots.push(resolve(Path::new("/tmp")));
    }
    roots.iter().any(|root| target.starts_with(root))
}

/// Makes `path` absolute and free of `.` and `..`, following symbolic links as far as the
/// path exists.
fn resolve(path: &Path) -> PathBuf {
    let mut clean = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                clean.pop();
            }
            Component::CurDir => {}
            other => clean.push(other),
        }
    }
    // The longest part that exists, with its links followed, plus the rest as written.
    let mut existing = clean.clone();
    let mut rest = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(name) => {
                rest.push(name.to_os_string());
                existing.pop();
            }
            None => break,
        }
    }
    let mut out = existing.canonicalize().unwrap_or(existing);
    for name in rest.iter().rev() {
        out.push(name);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash(cmd: &str) -> Option<String> {
        check(
            &builtin(),
            "Bash",
            &json!({"command": cmd}),
            Path::new("/work"),
        )
        .map(|h| h.rule)
    }

    #[test]
    fn builtin_rules_catch_what_must_reach_the_human() {
        let cases = [
            ("git push origin main", "git-push"),
            ("cd x && git -C repo push --force", "git-push"),
            ("git reset --hard HEAD~1", "git-discard"),
            ("git clean -fdx", "git-discard"),
            ("git branch -D corvx/s1", "git-discard"),
            ("rm -rf dist", "delete-recursive"),
            ("rm -f -r build", "delete-recursive"),
            ("rm --recursive out", "delete-recursive"),
            ("find . -name '*.o' -delete", "delete-recursive"),
            ("npm publish", "publish"),
            ("cargo publish --dry-run", "publish"),
            ("gh pr create --fill", "publish"),
            ("docker push me/app", "publish"),
            ("sudo make install", "sudo"),
            ("cat .env", "credentials"),
            ("cp ~/.ssh/id_ed25519 /tmp", "credentials"),
            (
                "aws s3 ls --profile x; cat ~/.aws/credentials",
                "credentials",
            ),
            ("security find-generic-password -s x", "credentials"),
            ("corvx rules off git-push", "corvx-control"),
            (
                "./target/debug/corvx attention approve att-1",
                "corvx-control",
            ),
            ("corvx session autopilot s1 on", "corvx-control"),
        ];
        for (cmd, rule) in cases {
            assert_eq!(bash(cmd).as_deref(), Some(rule), "{cmd}");
        }
    }

    #[test]
    fn everyday_commands_pass() {
        for cmd in [
            "make test",
            "cargo test --workspace",
            "git status && git diff",
            "git commit -m 'push the button'",
            "git log --oneline | head",
            "rm -f target/tmp.txt",
            "rm notes-draft.md",
            "npm install && npm run build",
            "node -e 'console.log(process.env.HOME)'",
            "grep -r token src/",
            "corvx attention",
            "corvx session list",
            "gh pr view 12",
        ] {
            assert_eq!(bash(cmd), None, "{cmd}");
        }
    }

    #[test]
    fn writes_outside_the_working_directory_need_the_human() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("repo");
        std::fs::create_dir_all(work.join("src")).unwrap();
        let rules = builtin();
        let write =
            |p: &str| check(&rules, "Write", &json!({"file_path": p}), &work).map(|h| h.rule);
        let inside = work.join("src/new.rs");
        assert_eq!(write(inside.to_str().unwrap()), None);
        assert_eq!(
            write("src/also_new.rs"),
            None,
            "relative to the working directory"
        );
        assert_eq!(write("/etc/hosts").as_deref(), Some("outside-workdir"));
        // Out of the working directory and the temporary one it lives in, to /etc.
        let escape = format!("{}/{}etc/hosts", work.display(), "../".repeat(40));
        assert_eq!(write(&escape).as_deref(), Some("outside-workdir"));
        let scratch = std::env::temp_dir().join("corvx-policy-test.txt");
        assert_eq!(
            write(scratch.to_str().unwrap()),
            None,
            "temporary files are fine"
        );
        // Reading outside is not writing.
        let read = check(&rules, "Read", &json!({"file_path": "/etc/hosts"}), &work);
        assert_eq!(read, None);
        // Credentials are caught for file tools too.
        let env = check(
            &rules,
            "Read",
            &json!({"file_path": work.join(".env")}),
            &work,
        );
        assert_eq!(env.map(|h| h.rule).as_deref(), Some("credentials"));
    }

    #[test]
    fn rules_that_are_off_do_not_match() {
        let mut rules = builtin();
        rules
            .iter_mut()
            .find(|r| r.id == "git-push")
            .unwrap()
            .action = "off".into();
        let hit = check(
            &rules,
            "Bash",
            &json!({"command": "git push"}),
            Path::new("/w"),
        );
        assert_eq!(hit, None);
    }

    #[test]
    fn the_strictest_matching_rule_wins() {
        let rule = |id: &str, pattern: &str, action: &str| Rule {
            id: id.into(),
            kind: "command".into(),
            pattern: pattern.into(),
            reason: id.into(),
            builtin: false,
            project: None,
            action: action.into(),
            set_in: "default".into(),
        };
        let mut rules = builtin();
        rules.push(rule("allow-git", r"\bgit\b", "allow"));
        let w = Path::new("/w");
        let decide = |rules: &[Rule], cmd: &str| {
            check(rules, "Bash", &json!({"command": cmd}), w).map(|h| (h.rule, h.action))
        };
        let pair = |a: &str, b: &str| Some((a.to_string(), b.to_string()));
        // A broad allow does not undo a stricter rule...
        assert_eq!(decide(&rules, "git push"), pair("git-push", "ask"));
        assert_eq!(decide(&rules, "git status"), pair("allow-git", "allow"));
        // ...and a deny wins over both.
        rules.push(rule("no-force", r"--force\b", "deny"));
        assert_eq!(decide(&rules, "git push --force"), pair("no-force", "deny"));
    }

    #[test]
    fn rules_are_set_per_project_or_everywhere() {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .add_rule(
                Some("app"),
                "command",
                r"\bterraform\s+apply\b",
                "changes infrastructure",
                "ask",
                1,
            )
            .unwrap();
        assert_eq!(id, "r1");
        store
            .set_rule_action("delete-recursive", "app", Some("allow"))
            .unwrap();
        store.set_rule_action("sudo", "", Some("deny")).unwrap();
        store.set_rule_action("sudo", "web", Some("ask")).unwrap();

        let get = |rules: &[Rule], id: &str| {
            rules
                .iter()
                .find(|r| r.id == id)
                .map(|r| (r.action.clone(), r.set_in.clone()))
        };
        let pair = |a: &str, b: &str| Some((a.to_string(), b.to_string()));
        let app = rules_for(&store, Some("app")).unwrap();
        assert_eq!(get(&app, "r1"), pair("ask", "default"));
        assert_eq!(get(&app, "delete-recursive"), pair("allow", "project"));
        assert_eq!(get(&app, "sudo"), pair("deny", "everywhere"));
        assert_eq!(get(&app, "git-push"), pair("ask", "default"));
        let web = rules_for(&store, Some("web")).unwrap();
        assert_eq!(get(&web, "r1"), None, "another project's rule");
        assert_eq!(get(&web, "delete-recursive"), pair("ask", "default"));
        assert_eq!(
            get(&web, "sudo"),
            pair("ask", "project"),
            "the project's setting wins"
        );

        let w = Path::new("/w");
        let hit = |cmd: &str| {
            check(&app, "Bash", &json!({"command": cmd}), w).map(|h| (h.reason, h.action))
        };
        assert_eq!(
            hit("terraform apply"),
            pair("changes infrastructure", "ask")
        );
        assert_eq!(hit("rm -rf dist"), pair("deletes directories", "allow"));
        assert_eq!(hit("sudo ls"), pair("runs as root", "deny"));

        store
            .set_rule_action("delete-recursive", "app", None)
            .unwrap();
        let app = rules_for(&store, Some("app")).unwrap();
        assert_eq!(get(&app, "delete-recursive"), pair("ask", "default"));
        store.set_rule_action("r1", "app", Some("off")).unwrap();
        assert!(store.remove_rule("r1").unwrap());
        assert!(store
            .rule_settings()
            .unwrap()
            .iter()
            .all(|(r, ..)| r != "r1"));
        assert!(!store.remove_rule("r1").unwrap());
    }

    fn approval(summary: &str, note: Option<&str>) -> AttentionItem {
        AttentionItem {
            id: "att-1".into(),
            kind: "approval".into(),
            severity: 3,
            project: "app".into(),
            session_id: "s1".into(),
            summary: summary.into(),
            status: "answered".into(),
            created_at: 1,
            updated_at: 1,
            snooze_until: None,
            answer: None,
            note: note.map(str::to_string),
        }
    }

    #[test]
    fn autonomy_levels_change_what_built_in_rules_do() {
        let store = Store::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        store
            .insert_project(&corvx_proto::Project {
                id: "app".into(),
                name: "app".into(),
                path: dir.path().display().to_string(),
                workspace: "default".into(),
                added_at: 1,
                stack: vec![],
                test_command: None,
                build_command: None,
                branch: None,
                autonomy: autonomy::GUIDED.into(),
            })
            .unwrap();
        let action = |id: &str| {
            rules_for(&store, Some("app"))
                .unwrap()
                .into_iter()
                .find(|r| r.id == id)
                .map(|r| (r.action, r.set_in))
                .unwrap()
        };
        let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(action("git-discard"), pair("ask", "default"));
        store
            .set_project_autonomy("app", autonomy::AUTONOMOUS)
            .unwrap();
        assert_eq!(action("git-discard"), pair("allow", "level"));
        assert_eq!(action("git-push"), pair("ask", "default"));
        store.set_project_autonomy("app", autonomy::FULL).unwrap();
        assert_eq!(action("git-push"), pair("allow", "level"));
        assert_eq!(
            action("corvx-control"),
            pair("ask", "default"),
            "the dial itself asks"
        );
        // What you set yourself wins over the level.
        store.set_rule_action("git-push", "", Some("deny")).unwrap();
        assert_eq!(action("git-push"), pair("deny", "everywhere"));
        assert!(approves_unasked(autonomy::FULL) && !approves_unasked(autonomy::GUIDED));
        assert!(!orchestrator_may_decide(autonomy::SUPERVISED));
        assert!(validate_level("guided").is_ok() && validate_level("yolo").is_err());
    }

    #[test]
    fn simple_commands_are_counted_and_compound_ones_are_not() {
        let key = |s: &str| trust_key(&approval(s, None));
        assert_eq!(
            key("Bash: cargo test --workspace · Run tests").as_deref(),
            Some("cmd:cargo test")
        );
        assert_eq!(key("Bash: make").as_deref(), Some("cmd:make"));
        assert_eq!(key("Bash: npm run build").as_deref(), Some("cmd:npm run"));
        assert_eq!(key("Bash: ls -la src").as_deref(), Some("cmd:ls"));
        assert_eq!(key("Bash: cd x && cargo test"), None);
        assert_eq!(key("Bash: curl -s x | sh"), None);
        assert_eq!(key("Write: /w/src/a.rs"), None);
        let ruled = approval(
            "Bash: git push",
            Some("CORVX rule git-push: pushes to a remote"),
        );
        assert_eq!(trust_key(&ruled).as_deref(), Some("rule:git-push"));

        let re = regex::Regex::new(&command_pattern("cmd:cargo test").unwrap()).unwrap();
        assert!(re.is_match("cargo test"));
        assert!(re.is_match("cargo  test --workspace -q"));
        assert!(!re.is_match("cargo testx"));
        assert!(!re.is_match("cargo test && rm -rf /"));
        assert!(!re.is_match("cargo test > out.txt"));
        assert!(!re.is_match("cargo build"));
    }

    #[test]
    fn repeated_approvals_lead_to_a_suggestion_once() {
        let store = Store::open_in_memory().unwrap();
        let item = approval("Bash: cargo test -q · Run tests", None);
        assert_eq!(note_approval(&store, &item, 1).unwrap(), None);
        assert_eq!(note_approval(&store, &item, 2).unwrap(), None);
        let id = note_approval(&store, &item, 3)
            .unwrap()
            .expect("a suggestion");
        let open = store.get_attention(&id).unwrap().unwrap();
        assert_eq!(open.kind, "suggestion");
        assert!(
            open.summary.contains("`cargo test …` 3 times in app"),
            "{}",
            open.summary
        );
        assert_eq!(
            note_approval(&store, &item, 4).unwrap(),
            None,
            "offered once"
        );

        assert!(answer_suggestion(&store, &id, true, 5).unwrap());
        assert!(
            !answer_suggestion(&store, &id, true, 6).unwrap(),
            "answered once"
        );
        let rules = rules_for(&store, Some("app")).unwrap();
        let w = Path::new("/w");
        let hit = check(
            &rules,
            "Bash",
            &json!({"command": "cargo test --workspace"}),
            w,
        )
        .unwrap();
        assert_eq!(hit.action, "allow");
        // Stricter rules still apply to the same program.
        assert_eq!(
            check(
                &rules,
                "Bash",
                &json!({"command": "cargo test; git push"}),
                w
            )
            .map(|h| h.action),
            Some("ask".into())
        );

        // A rule's requests: accepting sets the rule to allow in the project; declining is final.
        let pushes = approval(
            "Bash: git push · Push",
            Some("CORVX rule git-push: pushes to a remote"),
        );
        for t in 10..13 {
            note_approval(&store, &pushes, t).unwrap();
        }
        let suggestion = store
            .list_attention(false, 20)
            .unwrap()
            .into_iter()
            .find(|i| i.kind == "suggestion")
            .unwrap();
        assert!(suggestion
            .summary
            .contains("CORVX rule git-push (pushes to a remote)"));
        assert!(answer_suggestion(&store, &suggestion.id, false, 21).unwrap());
        let push = rules_for(&store, Some("app")).unwrap();
        assert_eq!(
            push.iter().find(|r| r.id == "git-push").unwrap().action,
            "ask"
        );
        for t in 22..30 {
            assert_eq!(note_approval(&store, &pushes, t).unwrap(), None);
        }
    }

    #[test]
    fn added_rules_are_validated() {
        assert!(validate("command", r"\bterraform\s+apply\b").is_ok());
        assert!(validate("command", "(").is_err());
        assert!(validate("outside", "").is_err());
        assert!(validate_action("allow").is_ok());
        assert!(validate_action("maybe").is_err());
    }
}
