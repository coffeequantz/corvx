//! `corvx`: command-line client and terminal UI for the CORVX daemon.
//!
//! With no arguments, opens the terminal UI. Every command here is a thin
//! wrapper over a protocol call, so scripts can do the same thing.

mod mcp;
mod tui;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use corvx_client::{ensure_daemon, Client};
use corvx_core::paths;
use corvx_proto::{methods, Project, ProjectAddParams, ProjectList, ProjectRemoveParams};
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "corvx",
    version,
    about = "CORVX: command center for AI-powered development"
)]
struct Cli {
    /// Data directory (default: ~/.local/share/corvx, or $CORVX_HOME).
    #[arg(long, global = true, value_name = "DIR")]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the terminal UI (default).
    Tui,
    /// Show daemon status.
    Status,
    /// Manage projects.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Start an orchestrator: a Claude Code session that plans the goal, delegates to
    /// agents in their own worktrees, and involves you only through the inbox.
    Orchestrate {
        /// Project id.
        project: String,
        /// The goal, in your words.
        #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
        goal: Vec<String>,
        /// Model for the orchestrator. It plans and reviews, so it gets a strong one.
        #[arg(long, default_value = DEFAULT_ORCHESTRATOR_MODEL)]
        model: String,
        /// Model its agents get unless it picks another for a task.
        #[arg(long, default_value = DEFAULT_AGENT_MODEL)]
        agent_model: String,
        /// Who decides the agents' permission requests: the orchestrator (it passes on what
        /// it should not decide), or you.
        #[arg(long, value_enum, default_value_t = Approver::Orchestrator)]
        approvals: Approver,
    },
    /// CORVX's tools for agents, as an MCP server on stdio. Started by agent sessions.
    #[command(hide = true)]
    Mcp,
    /// What needs you: agents that are blocked, finished or failed.
    Attention {
        #[command(subcommand)]
        command: Option<AttentionCommand>,
    },
    /// Run and control processes inside projects.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Rules that send an agent's permission request to you, even when its orchestrator or
    /// autopilot would decide it. Lists them when given no subcommand.
    Rules {
        #[command(subcommand)]
        command: Option<RulesCommand>,
        /// Show the rules as they apply to this project.
        #[arg(long)]
        project: Option<String>,
    },
}

#[derive(Subcommand)]
enum RulesCommand {
    /// Add a rule for requests matching a regular expression.
    Add {
        /// Match shell commands.
        #[arg(long, conflicts_with = "path", required_unless_present = "path")]
        command: Option<String>,
        /// Match the paths file tools use (and shell commands that mention them).
        #[arg(long)]
        path: Option<String>,
        /// What it is about, as the inbox shows it.
        #[arg(long)]
        reason: String,
        /// What happens to a matching request.
        #[arg(long, value_enum, default_value_t = RuleAction::Ask)]
        action: RuleAction,
        /// Only for this project (default: every project).
        #[arg(long)]
        project: Option<String>,
    },
    /// Set what a rule does: ask (you decide), allow (approved at once), deny (refused at
    /// once), off, or default (drop this setting).
    Set {
        id: String,
        #[arg(value_enum)]
        action: RuleSetting,
        /// Only for this project (default: every project).
        #[arg(long)]
        project: Option<String>,
    },
    /// Remove a rule you added.
    Remove { id: String },
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum RuleAction {
    Ask,
    Allow,
    Deny,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum RuleSetting {
    Ask,
    Allow,
    Deny,
    Off,
    Default,
}

#[derive(Subcommand)]
enum AttentionCommand {
    /// Acknowledge an item: you have seen it.
    Ack { id: String },
    /// Approve an agent's permission request.
    Approve {
        id: String,
        /// Also allow this kind of request from now on (Claude Code's "don't ask again").
        #[arg(long)]
        always: bool,
    },
    /// Deny an agent's permission request, optionally telling it why or what to do instead.
    Deny {
        id: String,
        #[arg(num_args = 0.., trailing_var_arg = true)]
        message: Vec<String>,
    },
    /// Answer a question an agent asked.
    Answer {
        id: String,
        #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
        answer: Vec<String>,
    },
    /// Hide an item until later.
    Snooze {
        id: String,
        /// How long to hide it.
        #[arg(long, default_value_t = 15)]
        minutes: u32,
    },
    /// Dismiss an item, optionally with a reason.
    Dismiss {
        id: String,
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Subcommand)]
enum SessionCommand {
    /// Start a command in a project's directory, in a terminal session.
    Start {
        /// Project id.
        project: String,
        /// Command and its arguments, e.g. `corvx session start app -- cargo test`.
        #[arg(required = true, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
        /// Runtime adapter, e.g. `claude`. Enables agent status from hooks.
        #[arg(long)]
        runtime: Option<String>,
        /// Terminal backend: `tmux` (survives daemon restarts) or `pty`. Default: tmux when installed.
        #[arg(long)]
        backend: Option<String>,
        /// Work in a new git worktree on its own branch.
        #[arg(long)]
        worktree: bool,
    },
    /// List sessions.
    List,
    /// Print a session's output so far.
    Output { id: String },
    /// Attach your terminal to a session. Keys go to the session. Ctrl+\ then d detaches.
    Attach { id: String },
    /// Send text to a session's terminal.
    Input {
        id: String,
        text: String,
        /// Press Enter after the text.
        #[arg(long)]
        enter: bool,
    },
    /// Stop a running session.
    Stop { id: String },
    /// Resume an interrupted Claude Code session (e.g. after a restart), continuing its
    /// conversation. Resuming an orchestrator also resumes the lost agents it supervised.
    Resume { id: String },
    /// Turn autopilot on or off: CORVX approves the session's permission requests itself,
    /// without asking you or its orchestrator. Every approval is recorded in the event log.
    Autopilot {
        id: String,
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Forward a runtime hook event (JSON on stdin) to the daemon. Used by the hooks CORVX registers.
    #[command(hide = true)]
    Hook { id: String },
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Approver {
    Orchestrator,
    Human,
}

/// Strong models plan and review; cheaper ones execute under their supervision.
const DEFAULT_ORCHESTRATOR_MODEL: &str = "opus";
const DEFAULT_AGENT_MODEL: &str = "sonnet";

#[derive(Subcommand)]
enum ProjectCommand {
    /// Register a project directory.
    Add {
        path: PathBuf,
        /// Display name (default: directory name).
        #[arg(long)]
        name: Option<String>,
        /// Workspace to group the project under (default: "default").
        #[arg(long)]
        workspace: Option<String>,
    },
    /// List registered projects.
    List {
        /// Print the raw JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Unregister a project. Does not touch its files.
    Remove {
        /// Project id, as shown by `corvx project list`.
        id: String,
    },
    /// Show or set how much the project's agents may do without asking:
    /// supervised (you decide every request), guided (orchestrators decide, rules ask you;
    /// the default), autonomous (approved without asking, except what rules send you), full
    /// (approved without asking, except changes to CORVX's rules and what you set to ask).
    /// Rules you set yourself always win.
    Autonomy {
        id: String,
        #[arg(value_enum)]
        level: Option<Level>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Level {
    Supervised,
    Guided,
    Autonomous,
    Full,
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Level::Supervised => "supervised",
            Level::Guided => "guided",
            Level::Autonomous => "autonomous",
            Level::Full => "full",
        }
    }
}

/// What an autonomy level means, in one line.
pub(crate) fn level_meaning(level: &str) -> &'static str {
    match level {
        "supervised" => "you decide every permission request",
        "guided" => "orchestrators decide their agents' requests; rules send pushes, deletes, publishing, credentials and outside writes to you",
        "autonomous" => "requests are approved without asking, except what rules send you (pushes, deletes, publishing, credentials, outside writes, sudo)",
        "full" => "requests are approved without asking, except changes to CORVX's own rules and what you set to ask or deny",
        _ => "",
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("corvx: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let data_dir = cli.data_dir.unwrap_or_else(paths::default_data_dir);

    // Hooks run inside agent processes. Never fail there, and never print.
    if let Some(Command::Session {
        command: SessionCommand::Hook { id },
    }) = &cli.command
    {
        forward_hook(&data_dir, id);
        return Ok(());
    }

    // The MCP server talks to its agent over stdio; it connects to the daemon when needed.
    if let Some(Command::Mcp) = &cli.command {
        return mcp::serve(&data_dir);
    }

    match cli.command.unwrap_or(Command::Tui) {
        Command::Tui => {
            let client = connect(&data_dir)?;
            tui::run(client, data_dir)
        }
        Command::Status => {
            let info = connect(&data_dir)?.info()?;
            println!(
                "corvxd {} · protocol v{} · pid {}",
                info.version, info.protocol_version, info.pid
            );
            println!("data: {}", info.data_dir);
            Ok(())
        }
        Command::Project { command } => project(command, &data_dir),
        Command::Session { command } => session(command, &data_dir),
        Command::Attention { command } => attention(command, &data_dir),
        Command::Rules { command, project } => rules(command, project, &data_dir),
        Command::Orchestrate {
            project,
            goal,
            model,
            agent_model,
            approvals,
        } => orchestrate(
            &project,
            &goal.join(" "),
            &model,
            &agent_model,
            approvals == Approver::Human,
            &data_dir,
        ),
        Command::Mcp => unreachable!("handled above"),
    }
}

fn rules(
    command: Option<RulesCommand>,
    project: Option<String>,
    data_dir: &std::path::Path,
) -> Result<()> {
    use corvx_proto::{
        methods, RuleList, RulesAddParams, RulesListParams, RulesRemoveParams, RulesSetParams,
    };
    let mut client = connect(data_dir)?;
    match command {
        None => {
            let params = RulesListParams {
                project: project.clone(),
            };
            let list: RuleList = client.call(methods::RULES_LIST, &params)?;
            println!(
                "CORVX applies these rules to agents' tool calls, whoever would decide them{}.",
                project
                    .as_deref()
                    .map(|p| format!(" (as set for {p})"))
                    .unwrap_or_default()
            );
            println!("ask: you decide · allow: approved at once · deny: refused at once");
            for r in &list.rules {
                let set = match r.set_in.as_str() {
                    "project" => "this project",
                    "everywhere" => "everywhere",
                    _ => "",
                };
                let scope = match &r.project {
                    Some(p) => format!("only {p}"),
                    None if r.builtin => "built in".into(),
                    None => "added".into(),
                };
                println!(
                    "  {:<18} {:<6} {:<12} {:<12} {}",
                    r.id, r.action, set, scope, r.reason
                );
                if !r.pattern.is_empty() {
                    println!(
                        "  {:<18} {:<6} {:<12} {:<12} {} /{}/",
                        "", "", "", "", r.kind, r.pattern
                    );
                }
            }
            Ok(())
        }
        Some(RulesCommand::Add {
            command,
            path,
            reason,
            action,
            project,
        }) => {
            let (kind, pattern) = match (command, path) {
                (Some(c), _) => ("command", c),
                (None, Some(p)) => ("path", p),
                (None, None) => unreachable!("clap requires one"),
            };
            let action = match action {
                RuleAction::Ask => "ask",
                RuleAction::Allow => "allow",
                RuleAction::Deny => "deny",
            };
            let params = RulesAddParams {
                project,
                kind: kind.into(),
                pattern,
                reason,
                action: action.into(),
            };
            let reply: serde_json::Value = client.call(methods::RULES_ADD, &params)?;
            println!(
                "added rule {} ({action})",
                reply["id"].as_str().unwrap_or("?")
            );
            Ok(())
        }
        Some(RulesCommand::Set {
            id,
            action,
            project,
        }) => {
            let action = match action {
                RuleSetting::Ask => "ask",
                RuleSetting::Allow => "allow",
                RuleSetting::Deny => "deny",
                RuleSetting::Off => "off",
                RuleSetting::Default => "default",
            };
            let params = RulesSetParams {
                id: id.clone(),
                project: project.clone(),
                action: action.into(),
            };
            let _: serde_json::Value = client.call(methods::RULES_SET, &params)?;
            let scope = project
                .map(|p| format!("in {p}"))
                .unwrap_or_else(|| "everywhere".into());
            println!("rule {id}: {action} {scope}");
            Ok(())
        }
        Some(RulesCommand::Remove { id }) => {
            let params = RulesRemoveParams { id: id.clone() };
            let _: serde_json::Value = client.call(methods::RULES_REMOVE, &params)?;
            println!("removed rule {id}");
            Ok(())
        }
    }
}

fn session(command: SessionCommand, data_dir: &std::path::Path) -> Result<()> {
    use base64::Engine;
    use corvx_proto::{
        methods, Session, SessionIdParams, SessionInputParams, SessionList, SessionOutput,
        SessionOutputParams, SessionStartParams,
    };
    let mut client = connect(data_dir)?;
    match command {
        SessionCommand::Start {
            project,
            command,
            runtime,
            backend,
            worktree,
        } => {
            let mut parts = command.into_iter();
            let program = parts.next().unwrap_or_default();
            let params = SessionStartParams {
                project,
                command: program,
                args: parts.collect(),
                runtime,
                backend,
                supervisor: None,
                worktree,
                label: None,
                autopilot: false,
                agent_model: None,
                human_approves_agents: false,
            };
            let s: Session = client.call(methods::SESSION_START, &params)?;
            println!(
                "started {} (pid {})",
                s.id,
                s.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into())
            );
            Ok(())
        }
        SessionCommand::List => {
            let list: SessionList = client.call(methods::SESSION_LIST, &json!({}))?;
            if list.sessions.is_empty() {
                println!("no sessions");
                return Ok(());
            }
            println!(
                "{:<20} {:<16} {:<12} {:<26} {:<6} NAME",
                "ID", "PROJECT", "PROCESS", "AGENT", "PID"
            );
            for s in list.sessions {
                let state = match s.exit_code {
                    Some(c) if s.state == corvx_proto::SessionState::Exited => {
                        format!("exited({c})")
                    }
                    _ => format!("{:?}", s.state).to_lowercase(),
                };
                let agent = format!("{:?}", s.agent_state).to_lowercase();
                let agent = match &s.agent_detail {
                    Some(d) => format!("{agent}: {d}"),
                    None => agent,
                };
                let agent = if s.autopilot {
                    format!("{agent} [autopilot]")
                } else {
                    agent
                };
                println!(
                    "{:<20} {:<16} {:<12} {:<26} {:<6} {}",
                    s.id,
                    s.project,
                    state,
                    agent,
                    s.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                    display_name(&s)
                );
            }
            Ok(())
        }
        SessionCommand::Output { id } => {
            let mut since = 0u64;
            loop {
                let out: SessionOutput = client.call(
                    methods::SESSION_OUTPUT,
                    &SessionOutputParams {
                        id: id.clone(),
                        since,
                    },
                )?;
                let bytes = base64::engine::general_purpose::STANDARD.decode(&out.data)?;
                print!("{}", String::from_utf8_lossy(&bytes));
                since = out.next;
                if out.state != corvx_proto::SessionState::Running || bytes.is_empty() {
                    break;
                }
            }
            println!();
            Ok(())
        }
        SessionCommand::Input { id, text, enter } => {
            let data = if enter { format!("{text}\n") } else { text };
            let _: serde_json::Value =
                client.call(methods::SESSION_INPUT, &SessionInputParams { id, data })?;
            Ok(())
        }
        SessionCommand::Attach { id } => attach(&mut client, &id, data_dir),
        SessionCommand::Hook { id } => {
            forward_hook(data_dir, &id);
            Ok(())
        }
        SessionCommand::Stop { id } => {
            let _: serde_json::Value =
                client.call(methods::SESSION_STOP, &SessionIdParams { id: id.clone() })?;
            println!("stopped {id}");
            Ok(())
        }
        SessionCommand::Resume { id } => {
            let r: corvx_proto::SessionResumed = client.call(
                methods::SESSION_RESUME,
                &corvx_proto::SessionResumeParams { id: id.clone() },
            )?;
            for s in &r.sessions {
                println!("resumed {:<20} {}", s.id, display_name(s));
            }
            Ok(())
        }
        SessionCommand::Autopilot { id, state } => {
            let on = state == "on";
            let _: Session = client.call(
                methods::SESSION_AUTOPILOT,
                &corvx_proto::SessionAutopilotParams {
                    id: id.clone(),
                    on,
                    by: None,
                },
            )?;
            if on {
                println!(
                    "autopilot on for {id}: its permission requests are approved without asking"
                );
            } else {
                println!("autopilot off for {id}");
            }
            Ok(())
        }
    }
}

fn connect(data_dir: &std::path::Path) -> Result<Client> {
    ensure_daemon(data_dir, None).context("cannot reach corvxd")
}

fn project(command: ProjectCommand, data_dir: &std::path::Path) -> Result<()> {
    let mut client = connect(data_dir)?;
    match command {
        ProjectCommand::Add {
            path,
            name,
            workspace,
        } => {
            let abs = std::fs::canonicalize(&path)
                .with_context(|| format!("no such path: {}", path.display()))?;
            let params = ProjectAddParams {
                path: abs.display().to_string(),
                name,
                workspace,
            };
            let p: Project = client.call(methods::PROJECT_ADD, &params)?;
            println!("added {} ({})", p.id, p.path);
            println!(
                "  stack: {}",
                if p.stack.is_empty() {
                    "unknown".into()
                } else {
                    p.stack.join(", ")
                }
            );
            if let Some(t) = &p.test_command {
                println!("  test:  {t}");
            }
            Ok(())
        }
        ProjectCommand::List { json: as_json } => {
            let list: ProjectList = client.call(methods::PROJECT_LIST, &json!({}))?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            if list.projects.is_empty() {
                println!("no projects yet · corvx project add <path>");
                return Ok(());
            }
            println!(
                "{:<18} {:<12} {:<11} {:<18} {:<14} PATH",
                "ID", "WORKSPACE", "AUTONOMY", "STACK", "TEST"
            );
            for p in list.projects {
                println!(
                    "{:<18} {:<12} {:<11} {:<18} {:<14} {}",
                    p.id,
                    p.workspace,
                    p.autonomy,
                    if p.stack.is_empty() {
                        "-".into()
                    } else {
                        p.stack.join(",")
                    },
                    p.test_command.as_deref().unwrap_or("-"),
                    p.path
                );
            }
            Ok(())
        }
        ProjectCommand::Autonomy { id, level } => {
            let p: Project = match level {
                Some(level) => {
                    let params = corvx_proto::ProjectAutonomyParams {
                        id: id.clone(),
                        level: level.name().into(),
                    };
                    client.call(methods::PROJECT_AUTONOMY, &params)?
                }
                None => {
                    let list: ProjectList = client.call(methods::PROJECT_LIST, &json!({}))?;
                    list.projects
                        .into_iter()
                        .find(|p| p.id == id)
                        .ok_or_else(|| anyhow::anyhow!("no project '{id}'"))?
                }
            };
            println!("{}: {} · {}", p.id, p.autonomy, level_meaning(&p.autonomy));
            println!(
                "rules you set win over the level: corvx rules --project {}",
                p.id
            );
            Ok(())
        }
        ProjectCommand::Remove { id } => {
            let _: serde_json::Value = client.call(
                methods::PROJECT_REMOVE,
                &ProjectRemoveParams { id: id.clone() },
            )?;
            println!("removed {id}");
            Ok(())
        }
    }
}

/// Reads one hook event from stdin and sends it to the daemon. Best effort: silent on any failure.
fn forward_hook(data_dir: &std::path::Path, session_id: &str) {
    use corvx_client::Client;
    use corvx_proto::{methods, SessionHookParams};
    use std::io::Read;

    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        return;
    }
    let payload: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    let event = payload
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    // Connect only. Starting the daemon from a hook would be surprising.
    let timeout = std::time::Duration::from_secs(2);
    let Ok(mut client) = Client::connect_with_timeout(&paths::socket_path(data_dir), timeout)
    else {
        return;
    };
    let params = SessionHookParams {
        id: session_id.to_string(),
        event,
        payload: payload.clone(),
    };
    let reply: Result<serde_json::Value> = client.call(methods::SESSION_HOOK, &params);
    let reply = reply.ok();
    // Before a tool runs, a CORVX rule decided: ask (Claude Code must ask even if its settings
    // allow the call; the permission request that follows comes back here and goes to the
    // human), allow, or deny.
    if let Some(pre) = reply.as_ref().and_then(|r| r.get("pre")) {
        let decision = pre
            .get("decision")
            .and_then(|v| v.as_str())
            .unwrap_or("ask");
        let reason = pre
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("CORVX rule");
        let reason = if decision == "ask" {
            format!("{reason} · decided in the CORVX inbox")
        } else {
            reason.to_string()
        };
        println!(
            "{}",
            json!({"hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": decision,
                "permissionDecisionReason": reason,
            }})
        );
        return;
    }
    // Decided at once: by autopilot, or by a CORVX rule.
    if let Some(decision) = reply
        .as_ref()
        .and_then(|r| r.get("decision"))
        .and_then(|v| v.as_str())
    {
        let message = reply
            .as_ref()
            .and_then(|r| r.get("message"))
            .and_then(|v| v.as_str());
        println!("{}", permission_output(decision, message, &payload));
        return;
    }
    // A permission request: wait for a decision from the inbox, then hand it to Claude Code.
    if let Some(item) = reply.and_then(|r| {
        r.get("approval")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }) {
        if let Some(output) = wait_for_decision(&mut client, session_id, &item, &payload) {
            println!("{output}");
        }
    }
}

/// Polls the daemon until the approval item is decided. Returns the hook output for Claude
/// Code, or None when there is nothing to say: it was answered in the session, dismissed, the
/// agent moved on, or the daemon went away. The dialog in the session then stands.
fn wait_for_decision(
    client: &mut corvx_client::Client,
    session_id: &str,
    item: &str,
    request: &serde_json::Value,
) -> Option<serde_json::Value> {
    use corvx_proto::{methods, PermissionStatus, SessionPermissionParams};
    let params = SessionPermissionParams {
        id: session_id.to_string(),
        item: item.to_string(),
    };
    loop {
        let status: PermissionStatus = client.call(methods::SESSION_PERMISSION, &params).ok()?;
        match status.status.as_str() {
            "pending" => std::thread::sleep(std::time::Duration::from_millis(500)),
            "decided" => {
                return Some(permission_output(
                    status.decision.as_deref()?,
                    status.message.as_deref(),
                    request,
                ))
            }
            _ => return None,
        }
    }
}

/// Claude Code's PermissionRequest hook output for a decision.
fn permission_output(
    decision: &str,
    message: Option<&str>,
    request: &serde_json::Value,
) -> serde_json::Value {
    let decision = match decision {
        "deny" => json!({
            "behavior": "deny",
            "message": message.unwrap_or("Denied by the human from the CORVX inbox."),
        }),
        "always" => {
            // Claude Code's own "don't ask again" options for this request, minus any that
            // would change the session's permission mode.
            let rules: Vec<serde_json::Value> = request
                .get("permission_suggestions")
                .and_then(|v| v.as_array())
                .map(|list| {
                    list.iter()
                        .filter(|s| s.get("type").and_then(|t| t.as_str()) != Some("setMode"))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            if rules.is_empty() {
                json!({"behavior": "allow"})
            } else {
                json!({"behavior": "allow", "updatedPermissions": rules})
            }
        }
        _ => json!({"behavior": "allow"}),
    };
    json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": decision}})
}

/// Attaches this terminal to a session. Your keys go to the session as the bytes your
/// terminal sends. The session's output comes here, and the session is resized to match
/// this window. Ctrl+\ then d detaches; the session keeps running.
fn attach(client: &mut Client, id: &str, data_dir: &std::path::Path) -> Result<()> {
    use corvx_proto::{methods, SessionList};
    let list: SessionList = client.call(methods::SESSION_LIST, &json!({}))?;
    let session = list
        .sessions
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| anyhow::anyhow!("no session '{id}'"))?;
    if session.backend == "tmux" {
        return attach_tmux(client, id, data_dir);
    }
    attach_pty(client, id)
}

/// Attaches to a tmux-backed session with a real tmux client: tmux handles terminal modes,
/// sizing and the prefix key. Ctrl+\ then d detaches; the status bar shows the hint.
fn attach_tmux(client: &mut Client, id: &str, data_dir: &std::path::Path) -> Result<()> {
    use corvx_proto::{methods, SessionList, SessionState};
    let socket = data_dir.join("tmux.sock");
    let status = std::process::Command::new("tmux")
        .arg("-S")
        .arg(&socket)
        .args(["attach-session", "-t", id])
        .status()
        .map_err(|e| anyhow::anyhow!("cannot run tmux: {e}"))?;
    if !status.success() {
        anyhow::bail!("tmux could not attach to {id}");
    }
    let list: SessionList = client.call(methods::SESSION_LIST, &json!({}))?;
    match list.sessions.iter().find(|s| s.id == id) {
        Some(s) if s.state == SessionState::Running => {
            eprintln!("detached from {id} (still running)")
        }
        Some(s) => eprintln!(
            "session {id} ended ({})",
            format!("{:?}", s.state).to_lowercase()
        ),
        None => eprintln!("detached from {id}"),
    }
    Ok(())
}

/// Attaches to a pty-backed session by relaying raw bytes through the daemon.
fn attach_pty(client: &mut Client, id: &str) -> Result<()> {
    use ratatui::crossterm::terminal;
    use std::sync::mpsc;

    eprintln!("attached to {id} · Ctrl+\\ then d to detach");
    // Keyboard bytes are read on their own thread so the loop can also follow output.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    terminal::enable_raw_mode()?;
    let result = attach_loop(client, id, &rx);
    // The session may have turned on mouse reporting and advanced keyboard modes. Turn
    // them off, or this terminal keeps sending them to the shell after we leave.
    reset_terminal();
    terminal::disable_raw_mode()?;
    match result? {
        AttachEnd::Detached => eprintln!("\r\ndetached from {id} (still running)"),
        AttachEnd::Ended(state) => eprintln!("\r\nsession {id} ended ({state})"),
    }
    Ok(())
}

enum AttachEnd {
    Detached,
    Ended(String),
}

fn attach_loop(
    client: &mut Client,
    id: &str,
    keys: &std::sync::mpsc::Receiver<Vec<u8>>,
) -> Result<AttachEnd> {
    use base64::Engine;
    use corvx_proto::{methods, SessionOutput, SessionOutputParams, SessionState};
    use ratatui::crossterm::terminal;
    use std::io::Write;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::{Duration, Instant};

    let mut since = 0u64;
    let mut prefix = false;
    let mut size = terminal::size().ok();
    if let Some((cols, rows)) = size {
        resize_session(client, id, cols, rows);
    }
    set_title(&format!("corvx · attached to {id}"));
    let mut last_size_check = Instant::now();

    loop {
        // Output: write whatever the session has produced since last time.
        let out: SessionOutput = client.call(
            methods::SESSION_OUTPUT,
            &SessionOutputParams {
                id: id.to_string(),
                since,
            },
        )?;
        since = out.next;
        if !out.data.is_empty() {
            let bytes = base64::engine::general_purpose::STANDARD.decode(&out.data)?;
            let mut stdout = std::io::stdout();
            stdout.write_all(&bytes)?;
            stdout.flush()?;
        }
        if out.state != SessionState::Running {
            return Ok(AttachEnd::Ended(format!("{:?}", out.state).to_lowercase()));
        }

        // Window size: follow this terminal.
        if last_size_check.elapsed() >= Duration::from_millis(250) {
            last_size_check = Instant::now();
            if let Ok(now) = terminal::size() {
                if Some(now) != size {
                    size = Some(now);
                    resize_session(client, id, now.0, now.1);
                }
            }
        }

        // Keys: forward them, unless they are the detach sequence.
        match keys.recv_timeout(Duration::from_millis(20)) {
            Ok(bytes) => {
                let was_armed = prefix;
                let (forward, detach) = route_keys(&bytes, &mut prefix);
                if prefix != was_armed {
                    let (_, rows) = size.unwrap_or((80, 24));
                    show_prefix_hint(rows, prefix, id);
                }
                if !forward.is_empty() {
                    send_input(client, id, &String::from_utf8_lossy(&forward))?;
                }
                if detach {
                    return Ok(AttachEnd::Detached);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(AttachEnd::Detached),
        }
    }
}

/// The prefix key that starts a CORVX command while attached: Ctrl+\ (byte 0x1c).
/// Not Ctrl+B: Claude Code uses Ctrl+B to send a running command to the background.
const PREFIX: u8 = 0x1c;

/// Splits raw keyboard bytes into what goes to the session, and reports the detach sequence.
///
/// Ctrl+\ is 0x1c in the usual encoding. Terminals that use an advanced keyboard protocol
/// send it as an escape sequence instead, so both forms are recognised. A session may
/// enable those protocols itself, which is why the raw bytes are checked rather than
/// decoded into key events.
fn route_keys(bytes: &[u8], prefix: &mut bool) -> (Vec<u8>, bool) {
    const PREFIX_CSI_U: &[u8] = b"\x1b[92;5u";
    const PREFIX_MODIFY_OTHER: &[u8] = b"\x1b[27;5;92~";
    if bytes == PREFIX_CSI_U || bytes == PREFIX_MODIFY_OTHER {
        *prefix = true;
        return (Vec::new(), false);
    }

    let mut forward = Vec::new();
    for &b in bytes {
        if *prefix {
            *prefix = false;
            match b {
                b'd' => return (forward, true),
                // The prefix twice sends one prefix key to the session.
                PREFIX => forward.push(PREFIX),
                // Any other key after the prefix is dropped, so it is not mistaken for input.
                _ => {}
            }
            continue;
        }
        if b == PREFIX {
            *prefix = true;
        } else {
            forward.push(b);
        }
    }
    (forward, false)
}

/// Resizes the session's terminal. Failures are ignored: the session may have just ended,
/// and the next output poll reports that.
fn resize_session(client: &mut Client, id: &str, cols: u16, rows: u16) {
    use corvx_proto::{methods, SessionResizeParams};
    let _: Result<serde_json::Value, _> = client.call(
        methods::SESSION_RESIZE,
        &SessionResizeParams {
            id: id.to_string(),
            cols,
            rows,
        },
    );
}

fn send_input(client: &mut Client, id: &str, data: &str) -> Result<()> {
    use corvx_proto::{methods, SessionInputParams};
    let _: serde_json::Value = client.call(
        methods::SESSION_INPUT,
        &SessionInputParams {
            id: id.to_string(),
            data: data.to_string(),
        },
    )?;
    Ok(())
}

/// Terminal modes a session may switch on. Turned off when detaching.
const TERMINAL_RESET: &str = concat!(
    "\x1b[?1000l", // mouse: button events
    "\x1b[?1002l", // mouse: drag events
    "\x1b[?1003l", // mouse: all motion
    "\x1b[?1005l",
    "\x1b[?1006l", // mouse: SGR encoding
    "\x1b[?1015l",
    "\x1b[?1016l",
    "\x1b[?1004l", // focus reporting
    "\x1b[?2004l", // bracketed paste
    "\x1b[?1l",    // application cursor keys
    "\x1b>",       // application keypad
    "\x1b[<u",     // pop kitty keyboard protocol flags
    "\x1b[>4;0m",  // modifyOtherKeys off
    "\x1b[?1049l", // leave alternate screen
    "\x1b[?25h",   // show cursor
    "\x1b[0m",     // reset colours and style
);

fn reset_terminal() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(TERMINAL_RESET.as_bytes());
    let _ = out.write_all(b"\x1b]2;\x07");
    let _ = out.flush();
}

fn set_title(title: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]2;{title}\x07");
    let _ = out.flush();
}

/// Shows (or clears) a hint on the bottom line while the prefix is waiting for its second key.
/// The cursor position is saved and restored around it.
fn show_prefix_hint(rows: u16, armed: bool, id: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let row = rows.max(1);
    if armed {
        let _ = write!(
            out,
            "\x1b7\x1b[{row};1H\x1b[7m ctrl+\\ · press d to detach from {id} \x1b[0m\x1b8"
        );
        set_title("corvx · ctrl+\\ pressed · d to detach");
    } else {
        let _ = write!(out, "\x1b7\x1b[{row};1H\x1b[2K\x1b8");
        set_title(&format!("corvx · attached to {id}"));
    }
    let _ = out.flush();
}

fn attention(command: Option<AttentionCommand>, data_dir: &std::path::Path) -> Result<()> {
    use corvx_proto::{
        methods, AttentionDismissParams, AttentionIdParams, AttentionList, AttentionListParams,
        AttentionSnoozeParams,
    };
    let mut client = connect(data_dir)?;
    match command {
        None => {
            let list: AttentionList =
                client.call(methods::ATTENTION_LIST, &AttentionListParams::default())?;
            if list.items.is_empty() {
                println!("nothing needs you");
                return Ok(());
            }
            println!(
                "{:<26} {:<9} {:<16} {:<20} SUMMARY",
                "ID", "KIND", "PROJECT", "SESSION"
            );
            for i in list.items {
                let summary = match (&i.note, i.kind.as_str(), i.severity) {
                    (_, "approval", s) if s < 3 => format!("(orchestrator deciding) {}", i.summary),
                    (Some(note), "approval", _) => format!("{} · {note}", i.summary),
                    _ => i.summary.clone(),
                };
                println!(
                    "{:<26} {:<9} {:<16} {:<20} {}",
                    i.id, i.kind, i.project, i.session_id, summary
                );
            }
            Ok(())
        }
        Some(AttentionCommand::Approve { id, always }) => {
            let decision = if always { "always" } else { "allow" };
            let _: serde_json::Value = client.call(
                methods::ATTENTION_DECIDE,
                &corvx_proto::AttentionDecideParams {
                    id: id.clone(),
                    decision: decision.into(),
                    message: None,
                    by: None,
                },
            )?;
            println!("approved {id}");
            Ok(())
        }
        Some(AttentionCommand::Deny { id, message }) => {
            let message = message.join(" ");
            let _: serde_json::Value = client.call(
                methods::ATTENTION_DECIDE,
                &corvx_proto::AttentionDecideParams {
                    id: id.clone(),
                    decision: "deny".into(),
                    message: (!message.trim().is_empty()).then(|| message.trim().to_string()),
                    by: None,
                },
            )?;
            println!("denied {id}");
            Ok(())
        }
        Some(AttentionCommand::Answer { id, answer }) => {
            let _: serde_json::Value = client.call(
                methods::ATTENTION_ANSWER,
                &corvx_proto::AttentionAnswerParams {
                    id: id.clone(),
                    answer: answer.join(" "),
                },
            )?;
            println!("answered {id}");
            Ok(())
        }
        Some(AttentionCommand::Ack { id }) => {
            let _: serde_json::Value = client.call(
                methods::ATTENTION_ACK,
                &AttentionIdParams { id: id.clone() },
            )?;
            println!("acknowledged {id}");
            Ok(())
        }
        Some(AttentionCommand::Snooze { id, minutes }) => {
            let _: serde_json::Value = client.call(
                methods::ATTENTION_SNOOZE,
                &AttentionSnoozeParams {
                    id: id.clone(),
                    minutes,
                },
            )?;
            println!("snoozed {id} for {minutes} minutes");
            Ok(())
        }
        Some(AttentionCommand::Dismiss { id, reason }) => {
            let _: serde_json::Value = client.call(
                methods::ATTENTION_DISMISS,
                &AttentionDismissParams {
                    id: id.clone(),
                    reason,
                },
            )?;
            println!("dismissed {id}");
            Ok(())
        }
    }
}

/// The role prompt for an orchestrator session. Kept short and concrete so smaller models
/// follow it too.
const ORCHESTRATOR_PROMPT: &str = "You are an orchestrator working through CORVX. You do not write the code yourself: \
you plan, delegate to coding agents, review their work, and keep the human informed.

How to work:
1. Read the project (list_projects; read its README and any .corvx/ files) and break the goal into pieces that \
can be done independently by separate agents.
2. If agents must agree on an interface (an API, a data format), write that contract to a file in the repository \
and commit it before starting them, so they can work in parallel.
3. Start one agent per piece with start_agent. Give each a complete prompt: the goal, its exact scope, the \
contract, how to test its work, and to commit on its branch when done. Agents cannot see your conversation.
4. Loop on wait_for. When an agent is waiting, read_screen to see what it did, then send_message with the next \
instruction or a correction. When it reports done, review its result before accepting it.
5. Use ask_human only for decisions the human must make (product choices, conflicts you cannot resolve, anything \
risky). Do not ask about things you can check yourself.
6. When the goal is met, write the mission's journal entry with journal: the goal, what happened, what went wrong, \
and what was learned that the next mission on this project should know. Then summarize what each agent did, \
which branches hold the work, and what the human should review or merge, and call report_done with that summary \
(it is added to the journal too).";

/// The orchestrator's role prompt, with its model and approval policy.
fn orchestrator_prompt(agent_model: &str, human_approves_agents: bool) -> String {
    let models = format!(
        "Models: you run on a strong model so you can plan and review; agents do the execution on cheaper ones. \
start_agent uses {agent_model} unless you pass model. Pick haiku for mechanical work (renames, formatting, \
docs, simple tests) and a stronger model (opus) only for a piece that needs it (tricky design, hard debugging)."
    );
    let approvals = if human_approves_agents {
        // (The project's autonomy level, which the human can change at any time, decides how
        // many requests reach anyone at all; see below.)
        "Approvals: the human decides your agents' permission requests. When wait_for reports an approval, \
just wait; you cannot decide it.".to_string()
    } else {
        "Approvals: your agents' permission requests come to you. When wait_for reports an approval, decide it \
with decide_approval. Allow what the agent's task needs inside its worktree: running tests and builds, installing \
the project's declared dependencies, git commands on its own branch. Deny (with a message saying what to do \
instead) what does not belong to its task. Escalate to the human anything destructive or irreversible, anything \
outside the project, touching credentials or secrets, pushing or publishing, or that you are unsure about. \
When an agent's work is routine and confined to its worktree, you may set_autopilot so it stops asking; \
turn it off if it drifts. CORVX rules decide some requests whatever you or autopilot would: \
by default pushing, discarding work, deleting directories, publishing, sudo, credentials and writing outside \
the agent's worktree go to the human, and the human may set rules to allow or deny requests at once. \
wait_for reports requests sent to the human with decider \"human\" and the rule in note; wait for those, \
or plan work that does not need them. A request a rule denies fails with the rule's reason: do it another way. \
The project's autonomy level, which the human sets and may change at any time, decides how much reaches you: \
at supervised every request goes to the human and you cannot set_autopilot; at autonomous or full most \
requests are approved without asking.".to_string()
    };
    format!("{ORCHESTRATOR_PROMPT}\n\n{models}\n\n{approvals}")
}

/// A readable name for a session: its label, or one derived from its command for sessions
/// started without a label (e.g. by an older daemon). Never the whole prompt.
pub(crate) fn display_name(s: &corvx_proto::Session) -> String {
    if let Some(label) = &s.label {
        return label.clone();
    }
    let is_orchestrator = s.command.iter().any(|a| a == "--append-system-prompt");
    // A Claude session's prompt is its last argument when it does not start with a dash.
    let prompt = s
        .command
        .iter()
        .skip(1)
        .last()
        .filter(|a| !a.starts_with('-'))
        .map(|a| short_label(a));
    match (is_orchestrator, s.supervisor.is_some(), prompt) {
        (true, _, Some(p)) => format!("orchestrator · {p}"),
        (true, _, None) => "orchestrator".into(),
        (false, true, Some(p)) => format!("agent · {p}"),
        _ => short_label(&s.command.join(" ")),
    }
}

/// The first line of a text, cut to about 40 characters, for session labels.
pub(crate) fn short_label(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() <= 40 {
        line.to_string()
    } else {
        format!("{}…", line.chars().take(39).collect::<String>().trim_end())
    }
}

/// Starts an orchestrator session for a goal.
fn orchestrate(
    project: &str,
    goal: &str,
    model: &str,
    agent_model: &str,
    human_approves_agents: bool,
    data_dir: &std::path::Path,
) -> Result<()> {
    use corvx_proto::{methods, Session, SessionStartParams};
    let mut client = connect(data_dir)?;
    let args = vec![
        "--append-system-prompt".to_string(),
        orchestrator_prompt(agent_model, human_approves_agents),
        "--model".into(),
        model.to_string(),
        goal.to_string(),
    ];
    let params = SessionStartParams {
        project: project.to_string(),
        command: "claude".into(),
        args,
        runtime: Some("claude".into()),
        backend: None,
        supervisor: None,
        worktree: false,
        label: Some(format!("orchestrator · {}", short_label(goal))),
        autopilot: false,
        agent_model: Some(agent_model.to_string()),
        human_approves_agents,
    };
    let s: Session = client.call(methods::SESSION_START, &params)?;
    println!(
        "started orchestrator {} on {model}; its agents default to {agent_model}",
        s.id
    );
    if human_approves_agents {
        println!("  approvals:  you decide the agents' permission requests");
    } else {
        println!("  approvals:  the orchestrator decides its agents' requests and passes risky ones to you");
    }
    println!("  watch it:   corvx session attach {}", s.id);
    println!("  your inbox: corvx attention");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::route_keys;

    #[test]
    fn the_orchestrator_prompt_names_its_agents_model_and_who_approves() {
        let p = super::orchestrator_prompt("haiku", false);
        assert!(p.contains("start_agent uses haiku"), "{p}");
        assert!(
            p.contains("decide_approval") && p.contains("set_autopilot"),
            "{p}"
        );
        let p = super::orchestrator_prompt("sonnet", true);
        assert!(
            p.contains("the human decides your agents' permission requests"),
            "{p}"
        );
        assert!(!p.contains("set_autopilot"), "{p}");
    }

    #[test]
    fn display_names_are_short_and_never_the_whole_prompt() {
        use corvx_proto::{AgentState, Session, SessionState};
        let mk = |command: &[&str], supervisor: Option<&str>, label: Option<&str>| Session {
            id: "s1".into(),
            project: "p".into(),
            command: command.iter().map(|a| a.to_string()).collect(),
            runtime: Some("claude".into()),
            backend: "tmux".into(),
            supervisor: supervisor.map(String::from),
            worktree: None,
            label: label.map(String::from),
            state: SessionState::Running,
            pid: None,
            started_at: 0,
            ended_at: None,
            exit_code: None,
            agent_state: AgentState::Unknown,
            agent_detail: None,
            agent_source: None,
            runtime_session: None,
            autopilot: false,
            agent_model: None,
            human_approves_agents: false,
        };
        let long_role = "You are an orchestrator.\nMany more lines of instructions";
        let orch = mk(
            &[
                "claude",
                "--append-system-prompt",
                long_role,
                "--model",
                "sonnet",
                "Build Snip as described",
            ],
            None,
            None,
        );
        assert_eq!(
            super::display_name(&orch),
            "orchestrator · Build Snip as described"
        );
        let worker = mk(
            &[
                "claude",
                "--permission-mode",
                "acceptEdits",
                "You are the BACKEND agent for Snip, a tiny link shortener. Read CLAUDE.md first.",
            ],
            Some("s0"),
            None,
        );
        let name = super::display_name(&worker);
        assert!(
            name.starts_with("agent · You are the BACKEND agent"),
            "{name}"
        );
        assert!(name.chars().count() < 60, "{name}");
        let labelled = mk(&["claude"], None, Some("agent · backend"));
        assert_eq!(super::display_name(&labelled), "agent · backend");
        assert_eq!(super::display_name(&mk(&["cat"], None, None)), "cat");
    }

    #[test]
    fn ctrl_b_now_reaches_the_session() {
        // Claude Code uses Ctrl+B; it must pass through untouched.
        let mut prefix = false;
        assert_eq!(route_keys(&[0x02], &mut prefix), (vec![0x02], false));
        assert!(!prefix);
    }

    #[test]
    fn ordinary_keys_pass_through_unchanged() {
        let mut prefix = false;
        assert_eq!(
            route_keys(b"hello\r", &mut prefix),
            (b"hello\r".to_vec(), false)
        );
        assert!(!prefix);
    }

    #[test]
    fn prefix_then_d_detaches_in_the_usual_encoding() {
        let mut prefix = false;
        assert_eq!(route_keys(&[0x1c], &mut prefix), (vec![], false));
        assert!(prefix);
        assert_eq!(route_keys(b"d", &mut prefix), (vec![], true));
    }

    #[test]
    fn prefix_then_d_detaches_in_the_advanced_encodings() {
        for seq in [&b"\x1b[92;5u"[..], &b"\x1b[27;5;92~"[..]] {
            let mut prefix = false;
            assert_eq!(route_keys(seq, &mut prefix), (vec![], false));
            assert_eq!(route_keys(b"d", &mut prefix), (vec![], true));
        }
    }

    #[test]
    fn prefix_twice_sends_one_prefix_to_the_session() {
        let mut prefix = false;
        route_keys(&[0x1c], &mut prefix);
        assert_eq!(route_keys(&[0x1c], &mut prefix), (vec![0x1c], false));
        assert!(!prefix);
    }

    #[test]
    fn other_key_after_prefix_is_dropped_not_detached() {
        let mut prefix = false;
        route_keys(&[0x1c], &mut prefix);
        assert_eq!(route_keys(b"x", &mut prefix), (vec![], false));
        assert!(!prefix);
    }

    #[test]
    fn detach_can_follow_keys_in_the_same_read() {
        let mut prefix = false;
        assert_eq!(route_keys(b"ab\x1cd", &mut prefix), (b"ab".to_vec(), true));
    }
}
