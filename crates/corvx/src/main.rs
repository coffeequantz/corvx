//! `corvx`: command-line client and terminal UI for the CORVX daemon.
//!
//! With no arguments, opens the terminal UI. Every command here is a thin
//! wrapper over a protocol call, so scripts can do the same thing.

mod mcp;
mod missions;
mod tui;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use corvx_client::{ensure_daemon, Client};
use corvx_core::paths;
use corvx_proto::{
    methods, Project, ProjectAddParams, ProjectList, ProjectRemoveParams, Session, SessionList,
    SessionState,
};
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
    /// Show daemon status (same as `corvx daemon status`).
    Status,
    /// The daemon: whether it runs, and stopping or restarting it (to load a new build).
    Daemon {
        #[command(subcommand)]
        command: Option<DaemonCommand>,
    },
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
    /// Talk to a project's lead: the long-lived agent that keeps the overview, records goals
    /// and starts missions. Starts it if it is not running; otherwise sends it your message.
    Lead {
        project: String,
        /// What to tell it. Starting it without a message asks it for a status.
        message: Vec<String>,
        /// Its model, when starting it.
        #[arg(long, default_value = DEFAULT_ORCHESTRATOR_MODEL)]
        model: String,
    },
    /// A project's goals and guideposts (.corvx/goals.md). Lists them when given no subcommand.
    #[command(args_conflicts_with_subcommands = true)]
    Goals {
        #[command(subcommand)]
        command: Option<GoalsCommand>,
        /// Project id.
        project: Option<String>,
    },
    /// A project's missions, newest first: each orchestrator, its agents and its journal entry.
    Missions {
        project: String,
        /// Print this mission's journal entry (its orchestrator's session id).
        #[arg(long)]
        show: Option<String>,
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
    /// What CORVX has learned about a project: its .corvx/ knowledge files, and the proposals
    /// agents make to change them. Shows a project's knowledge when given no subcommand.
    #[command(args_conflicts_with_subcommands = true)]
    Knowledge {
        #[command(subcommand)]
        command: Option<KnowledgeCommand>,
        /// Project id.
        project: Option<String>,
    },
}

#[derive(Subcommand)]
enum GoalsCommand {
    /// Add a goal (active unless you say otherwise).
    Add {
        project: String,
        title: String,
        /// What done looks like, and why it matters.
        #[arg(long)]
        detail: Option<String>,
        #[arg(long, value_parser = ["active", "next", "done", "dropped"])]
        status: Option<String>,
    },
    /// Change a goal's status.
    Set {
        project: String,
        title: String,
        #[arg(value_parser = ["active", "next", "done", "dropped"])]
        status: String,
    },
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Whether it runs, since when, its sessions, and whether a newer build is on disk.
    Status,
    /// Stop it. Sessions in tmux keep running and are adopted by the next daemon, which
    /// any corvx command starts.
    Stop {
        /// Also when sessions without tmux are running: they end with the daemon.
        #[arg(long)]
        force: bool,
    },
    /// Stop it and start it again, e.g. after building a new version.
    Restart {
        /// Also when sessions without tmux are running: they end with the daemon.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum KnowledgeCommand {
    /// Create the project's missing knowledge files (brief, decisions, conventions, map) and
    /// point its CLAUDE.md at them, so every agent starts from them. Commits what it writes.
    Init { project: String },
    /// Proposals waiting for you.
    Proposals {
        #[arg(long)]
        project: Option<String>,
        /// Decided ones too.
        #[arg(long)]
        all: bool,
    },
    /// Edit a knowledge file in $EDITOR (brief, decisions or conventions); committed when
    /// it changed.
    Edit {
        project: String,
        #[arg(value_parser = ["brief", "goals", "decisions", "conventions"], default_value = "brief")]
        file: String,
    },
    /// Show a proposal in full.
    Show { id: String },
    /// Accept a proposal: it is written into its file and committed.
    Accept {
        id: String,
        /// Edit the text in $EDITOR first.
        #[arg(long)]
        edit: bool,
    },
    /// Reject a proposal. The reason is kept, and it is not proposed again.
    Reject { id: String, reason: Vec<String> },
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
    /// Remove the worktrees of a finished session and of the agents under it, with their
    /// branches, where the work is merged into the project's current branch. Keeps anything
    /// not merged or with uncommitted changes.
    Cleanup { id: String },
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
pub(crate) const DEFAULT_ORCHESTRATOR_MODEL: &str = "opus";
pub(crate) const DEFAULT_AGENT_MODEL: &str = "sonnet";

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
    /// Rename a project or move it to another workspace. Its id stays the same.
    Set {
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Scan a project again (stack, test and build commands, branch), e.g. after it changed.
    Rescan { id: String },
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
        Command::Status => daemon(DaemonCommand::Status, &data_dir),
        Command::Daemon { command } => daemon(command.unwrap_or(DaemonCommand::Status), &data_dir),
        Command::Project { command } => project(command, &data_dir),
        Command::Session { command } => session(command, &data_dir),
        Command::Attention { command } => attention(command, &data_dir),
        Command::Rules { command, project } => rules(command, project, &data_dir),
        Command::Knowledge { command, project } => knowledge(command, project, &data_dir),
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
        Command::Missions { project, show } => list_missions(&project, show, &data_dir),
        Command::Goals { command, project } => goals(command, project, &data_dir),
        Command::Lead {
            project,
            message,
            model,
        } => {
            let mut client = connect(&data_dir)?;
            let message = message.join(" ");
            match talk_to_lead(&mut client, &project, &model, &message)? {
                Talked::Started(id) => {
                    println!("started the lead of {project}: {id} on {model}");
                    println!("  talk to it: corvx lead {project} <message> · corvx session attach {id} · t in the TUI");
                }
                Talked::Sent(id) => println!("sent to the lead of {project} ({id})"),
            }
            Ok(())
        }
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
        SessionCommand::Cleanup { id } => {
            let done: corvx_proto::SessionCleanup = client.call(
                methods::SESSION_CLEANUP,
                &SessionIdParams { id: id.clone() },
            )?;
            print_cleanup(&id, &done);
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

fn daemon(command: DaemonCommand, data_dir: &std::path::Path) -> Result<()> {
    match command {
        DaemonCommand::Status => {
            let Some(mut client) = corvx_client::running_daemon(data_dir) else {
                println!("corvxd is not running · any corvx command starts it");
                return Ok(());
            };
            let info = client.info()?;
            let up = if info.started_at > 0 {
                format!(
                    " · up {}",
                    duration_label(corvx_core::store::now_secs() - info.started_at)
                )
            } else {
                String::new()
            };
            println!(
                "corvxd {} · protocol v{} · pid {}{up}",
                info.version, info.protocol_version, info.pid
            );
            println!("data:     {}", info.data_dir);
            let list: SessionList = client.call(methods::SESSION_LIST, &json!({}))?;
            let running: Vec<&Session> = list
                .sessions
                .iter()
                .filter(|s| s.state == SessionState::Running)
                .collect();
            let native = running.iter().filter(|s| s.backend != "tmux").count();
            println!(
                "sessions: {} running{}",
                running.len(),
                if native > 0 {
                    format!(" ({native} without tmux: they end when the daemon stops)")
                } else {
                    String::new()
                }
            );
            if corvx_client::newer_build_on_disk(&info) {
                println!("a newer build is on disk · corvx daemon restart loads it");
            }
            Ok(())
        }
        DaemonCommand::Stop { force } => {
            check_native_sessions(data_dir, force)?;
            match corvx_client::stop_daemon(data_dir)? {
                Some(pid) => println!(
                    "stopped corvxd (pid {pid}) · sessions in tmux keep running · any corvx command starts it again"
                ),
                None => println!("corvxd is not running"),
            }
            Ok(())
        }
        DaemonCommand::Restart { force } => {
            check_native_sessions(data_dir, force)?;
            let old = corvx_client::stop_daemon(data_dir)?;
            let mut client = connect(data_dir)?;
            let info = client.info()?;
            let list: SessionList = client.call(methods::SESSION_LIST, &json!({}))?;
            let running = list
                .sessions
                .iter()
                .filter(|s| s.state == SessionState::Running)
                .count();
            match old {
                Some(old) => println!(
                    "restarted corvxd: pid {old} → {} · {running} session(s) running",
                    info.pid
                ),
                None => println!(
                    "started corvxd (pid {}) · {running} session(s) running",
                    info.pid
                ),
            }
            Ok(())
        }
    }
}

/// Refuses to stop the daemon while sessions that would end with it are running, unless forced.
fn check_native_sessions(data_dir: &std::path::Path, force: bool) -> Result<()> {
    let Some(mut client) = corvx_client::running_daemon(data_dir) else {
        return Ok(());
    };
    let list: SessionList = client.call(methods::SESSION_LIST, &json!({}))?;
    let native: Vec<String> = list
        .sessions
        .iter()
        .filter(|s| s.state == SessionState::Running && s.backend != "tmux")
        .map(|s| format!("{} ({})", s.id, display_name(s)))
        .collect();
    if native.is_empty() || force {
        return Ok(());
    }
    anyhow::bail!(
        "these sessions run without tmux and would end with the daemon: {}\n\
         stop them first, or pass --force",
        native.join(", ")
    )
}

/// "45s", "12m", "3h 05m", "2d 4h".
fn duration_label(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
        s => format!("{}d {}h", s / 86_400, s % 86_400 / 3600),
    }
}

fn goals(
    command: Option<GoalsCommand>,
    project: Option<String>,
    data_dir: &std::path::Path,
) -> Result<()> {
    let mut client = connect(data_dir)?;
    let (project, title, status, detail) = match command {
        None => {
            let Some(project) = project else {
                anyhow::bail!("which project? corvx goals <project>");
            };
            let k: corvx_proto::Knowledge = client.call(
                methods::KNOWLEDGE_READ,
                &json!({"project": project, "journal": 0}),
            )?;
            print!("{}", describe_goals(&k));
            return Ok(());
        }
        Some(GoalsCommand::Add {
            project,
            title,
            detail,
            status,
        }) => (project, title, status, detail),
        Some(GoalsCommand::Set {
            project,
            title,
            status,
        }) => (project, title, Some(status), None),
    };
    let done: corvx_proto::KnowledgeAccepted = client.call(
        methods::KNOWLEDGE_GOAL,
        &corvx_proto::KnowledgeGoalParams {
            project,
            title: title.clone(),
            status,
            detail,
        },
    )?;
    println!(
        "{title}: written to {}{}",
        done.file,
        if done.committed { " and committed" } else { "" }
    );
    if let Some(note) = done.note {
        println!("{note}");
    }
    Ok(())
}

/// A project's goals and guideposts, as text.
pub(crate) fn describe_goals(k: &corvx_proto::Knowledge) -> String {
    let mut out = String::new();
    if k.guideposts.is_empty() {
        out.push_str("guideposts: none yet (corvx knowledge edit <project> goals)\n");
    } else {
        out.push_str("guideposts:\n");
        for g in &k.guideposts {
            out.push_str(&format!("  - {g}\n"));
        }
    }
    if k.goals.is_empty() {
        out.push_str(&format!(
            "goals: none yet · corvx goals add {} \"<goal>\"\n",
            k.project
        ));
    } else {
        out.push_str("goals:\n");
        for g in &k.goals {
            out.push_str(&format!("  {:<8} {}\n", g.status, g.title));
        }
    }
    out
}

/// A project's missions, from the daemon's sessions and the project's journal.
pub(crate) fn load_missions(client: &mut Client, project: &str) -> Result<Vec<missions::Mission>> {
    let k: corvx_proto::Knowledge = client.call(
        methods::KNOWLEDGE_READ,
        &json!({"project": project, "journal": missions::JOURNAL_ENTRIES}),
    )?;
    let list: SessionList = client.call(
        methods::SESSION_LIST,
        &json!({"all": true, "project": k.project}),
    )?;
    Ok(missions::missions(&k.project, &list.sessions, &k.journal))
}

fn list_missions(project: &str, show: Option<String>, data_dir: &std::path::Path) -> Result<()> {
    let mut client = connect(data_dir)?;
    let all = load_missions(&mut client, project)?;
    if let Some(id) = show {
        let m = all
            .iter()
            .find(|m| m.session.as_ref().is_some_and(|s| s.id == id))
            .ok_or_else(|| anyhow::anyhow!("no mission by '{id}' in {project}"))?;
        match &m.journal {
            Some(j) => print!("{}", j.text),
            None => println!("{id} wrote no journal entry"),
        }
        return Ok(());
    }
    if all.is_empty() {
        println!("no missions on {project} yet · corvx orchestrate {project} \"<goal>\"");
        return Ok(());
    }
    for m in &all {
        let date = if m.started_at > 0 {
            corvx_core::knowledge::time_of(m.started_at)
                .trim_end_matches(" UTC")
                .to_string()
        } else {
            m.journal
                .as_ref()
                .and_then(|j| j.path.rsplit('/').next())
                .and_then(|n| n.get(..10))
                .unwrap_or("-")
                .to_string()
        };
        let id = m.session.as_ref().map(|s| s.id.as_str()).unwrap_or("-");
        let goal: String = m.goal.chars().take(60).collect();
        println!("{date:<17} {:<12} {id:<16} {goal}", m.state());
        let mut notes = vec![format!(
            "{} agent{}",
            m.agents,
            if m.agents == 1 { "" } else { "s" }
        )];
        if m.agents_running > 0 {
            notes.push(format!("{} running", m.agents_running));
        }
        if m.worktrees > 0 {
            notes.push(format!(
                "{} worktree{} left · corvx session cleanup {id}",
                m.worktrees,
                if m.worktrees == 1 { "" } else { "s" }
            ));
        }
        notes.push(match &m.journal {
            Some(j) => j.path.clone(),
            None => "no journal entry".into(),
        });
        println!("{:<17} {}", "", notes.join(" · "));
    }
    Ok(())
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
            print_scan(&p);
            println!(
                "  next:  corvx knowledge init {}   (its knowledge files, read by every agent)",
                p.id
            );
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
        ProjectCommand::Set {
            id,
            name,
            workspace,
        } => {
            if name.is_none() && workspace.is_none() {
                anyhow::bail!("say what to change: --name and/or --workspace");
            }
            let p: Project = client.call(
                methods::PROJECT_UPDATE,
                &corvx_proto::ProjectUpdateParams {
                    id,
                    name,
                    workspace,
                    rescan: false,
                },
            )?;
            println!("{}: \"{}\" in workspace {}", p.id, p.name, p.workspace);
            Ok(())
        }
        ProjectCommand::Rescan { id } => {
            let p: Project = client.call(
                methods::PROJECT_UPDATE,
                &corvx_proto::ProjectUpdateParams {
                    id,
                    rescan: true,
                    ..Default::default()
                },
            )?;
            print_scan(&p);
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

fn print_cleanup(id: &str, done: &corvx_proto::SessionCleanup) {
    if done.removed.is_empty() && done.kept.is_empty() {
        println!("{id}: no worktrees left to clean up");
    }
    for r in &done.removed {
        println!("removed {r}");
    }
    for k in &done.kept {
        println!("kept    {k}");
    }
}

/// What the scan found, indented under a project line.
fn print_scan(p: &Project) {
    let or = |v: &Option<String>| v.clone().unwrap_or_else(|| "not detected".into());
    println!(
        "  stack: {}",
        if p.stack.is_empty() {
            "unknown".into()
        } else {
            p.stack.join(", ")
        }
    );
    println!("  test:  {}", or(&p.test_command));
    println!("  build: {}", or(&p.build_command));
    println!("  branch: {}", or(&p.branch));
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
1. Read the project: read_knowledge first (its brief, goals and guideposts, decisions, conventions, map and the latest \
mission journals), then its README and whatever else you need. Follow the guideposts. If the mission serves one of \
the goals, say which. Break the goal into pieces that can be done independently by separate \
agents. In your plan, name the decisions and conventions that shape it, and what earlier missions taught that you \
are applying.
2. If agents must agree on an interface (an API, a data format), write that contract to a file in the repository \
and commit it before starting them, so they can work in parallel.
3. Start one agent per piece with start_agent. Give each a complete prompt: the goal, its exact scope, the \
contract, the decisions and conventions that apply to its piece, how to test its work, and to commit on its \
branch when done. Agents cannot see your conversation.
4. Loop on wait_for. When an agent is waiting, read_screen to see what it did, then send_message with the next \
instruction or a correction. When it reports done, review its result before accepting it.
5. Use ask_human only for decisions the human must make (product choices, conflicts you cannot resolve, anything \
risky). Do not ask about things you can check yourself.
6. When the goal is met, write the mission's journal entry with journal: the goal, what happened, what went wrong, \
and what was learned that the next mission on this project should know. \
7. Propose what should be remembered with propose_knowledge, one point per proposal, each with its evidence: \
decisions made in this mission and why, conventions the agents should follow next time, pitfalls that cost time. \
Only what would change how later work is done, and not what the knowledge already says, is waiting, or was \
rejected. Usually zero to three proposals. If the mission completed a goal in goals.md, propose marking it done \
(file goals, replacing its heading). The human accepts, edits or rejects them in the inbox. \
8. Summarize what each agent did, which branches hold the work, which knowledge you proposed, and what the human \
should review or merge, and call report_done with that summary. It is added to the journal, and CORVX commits the journal entry, so do not \
commit it yourself.";

/// The orchestrator's role prompt, with its model and approval policy.
fn orchestrator_prompt(agent_model: &str, human_approves_agents: bool) -> String {
    let models = format!(
        "Models: you run on a strong model so you can plan and review; agents do the execution on cheaper ones. \
start_agent uses {agent_model} unless you pass model. Pick haiku for mechanical work (renames, formatting, \
docs, simple tests) and a stronger model (opus) only for a piece that needs it (tricky design, hard debugging)."
    );
    let approvals = approvals_policy(human_approves_agents);
    format!("{ORCHESTRATOR_PROMPT}\n\n{models}\n\n{approvals}")
}

/// Who decides the permission requests of the sessions an orchestrator (or lead) supervises.
fn approvals_policy(human_approves_agents: bool) -> String {
    if human_approves_agents {
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
    }
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
    let mut client = connect(data_dir)?;
    let started = start_orchestrator(
        &mut client,
        &Mission {
            project,
            goal,
            model,
            agent_model,
            human_approves_agents,
            serves: None,
            supervisor: None,
        },
    )?;
    println!(
        "started orchestrator {} on {model}; its agents default to {agent_model}",
        started.session.id
    );
    if !started.wired {
        println!(
            "  knowledge:  not set up, so only the orchestrator reads it · corvx knowledge init {project}"
        );
    }
    if human_approves_agents {
        println!("  approvals:  you decide the agents' permission requests");
    } else {
        println!("  approvals:  the orchestrator decides its agents' requests and passes risky ones to you");
    }
    println!("  watch it:   corvx session attach {}", started.session.id);
    println!("  your inbox: corvx attention");
    Ok(())
}

pub(crate) struct Started {
    pub session: corvx_proto::Session,
    /// Whether the project's agents read its knowledge files (through CLAUDE.md).
    pub wired: bool,
}

/// What a mission is started with.
pub(crate) struct Mission<'a> {
    pub project: &'a str,
    pub goal: &'a str,
    pub model: &'a str,
    pub agent_model: &'a str,
    pub human_approves_agents: bool,
    /// The project goal (from goals.md) it serves.
    pub serves: Option<&'a str>,
    /// A project lead that started it; None when you did.
    pub supervisor: Option<String>,
}

/// Starts an orchestrator session on a project. Shared by `corvx orchestrate`, the TUI and a
/// project lead's `start_mission`.
pub(crate) fn start_orchestrator(
    client: &mut corvx_client::Client,
    m: &Mission,
) -> Result<Started> {
    use corvx_proto::{methods, Session, SessionStartParams};
    let prompt = match m.serves {
        Some(goal) => format!(
            "{}\n\n(This mission serves the project goal \"{goal}\" in .corvx/goals.md.)",
            m.goal
        ),
        None => m.goal.to_string(),
    };
    let args = vec![
        "--append-system-prompt".to_string(),
        orchestrator_prompt(m.agent_model, m.human_approves_agents),
        "--model".into(),
        m.model.to_string(),
        prompt,
    ];
    let params = SessionStartParams {
        project: m.project.to_string(),
        // `CORVX_AGENT_COMMAND` replaces the agent program, e.g. with a test stand-in.
        command: std::env::var("CORVX_AGENT_COMMAND").unwrap_or_else(|_| "claude".into()),
        args,
        runtime: Some("claude".into()),
        backend: None,
        supervisor: m.supervisor.clone(),
        worktree: false,
        label: Some(format!("orchestrator · {}", short_label(m.goal))),
        autopilot: false,
        agent_model: Some(m.agent_model.to_string()),
        human_approves_agents: m.human_approves_agents,
    };
    let knowledge: Option<corvx_proto::Knowledge> = client
        .call(
            methods::KNOWLEDGE_READ,
            &corvx_proto::KnowledgeReadParams {
                project: m.project.to_string(),
                journal: Some(0),
            },
        )
        .ok();
    let session: Session = client.call(methods::SESSION_START, &params)?;
    Ok(Started {
        session,
        wired: knowledge.map_or(true, |k| k.wired),
    })
}

/// The label that marks a project's lead.
pub(crate) const LEAD_LABEL: &str = "lead · ";

/// Whether a session is a project lead: the long-lived agent you talk to about a project.
pub(crate) fn is_lead(s: &corvx_proto::Session) -> bool {
    s.label
        .as_deref()
        .is_some_and(|l| l.starts_with(LEAD_LABEL))
}

pub(crate) enum Talked {
    Started(String),
    Sent(String),
}

/// The project's running lead, if it has one.
pub(crate) fn running_lead<'a>(
    sessions: &'a [corvx_proto::Session],
    project: &str,
) -> Option<&'a corvx_proto::Session> {
    sessions
        .iter()
        .rev()
        .find(|s| s.project == project && is_lead(s) && s.state == SessionState::Running)
}

/// Sends a message to the project's lead, starting the lead (with the message as its first
/// one) when none runs. An interrupted lead is resumed rather than replaced.
pub(crate) fn talk_to_lead(
    client: &mut corvx_client::Client,
    project: &str,
    model: &str,
    message: &str,
) -> Result<Talked> {
    let list: SessionList = client.call(methods::SESSION_LIST, &json!({"project": project}))?;
    if let Some(lead) = running_lead(&list.sessions, project) {
        if message.trim().is_empty() {
            anyhow::bail!(
                "the lead of {project} is running ({}): say something to it",
                lead.id
            );
        }
        let _: serde_json::Value = client.call(
            methods::SESSION_INPUT,
            &json!({"id": lead.id, "data": format!("{}\r", message.trim())}),
        )?;
        return Ok(Talked::Sent(lead.id.clone()));
    }
    if let Some(lost) = list
        .sessions
        .iter()
        .rev()
        .find(|s| s.project == project && is_lead(s) && s.state == SessionState::Lost)
    {
        anyhow::bail!(
            "the lead of {project} ({}) was interrupted: corvx session resume {} continues its conversation",
            lost.id,
            lost.id
        );
    }
    let lead = start_lead(client, project, model, Some(message))?;
    Ok(Talked::Started(lead.id))
}

/// What a new lead is asked first, when you start it without saying anything.
pub(crate) const LEAD_FIRST: &str =
    "Read the project and give me a short status: the goals, recent missions, what is waiting for me, and what you suggest next.";

/// Starts a project's lead: a long-lived session on a strong model that you talk to. It
/// keeps the overview, records goals, and starts missions (orchestrators under it).
pub(crate) fn start_lead(
    client: &mut corvx_client::Client,
    project: &str,
    model: &str,
    first: Option<&str>,
) -> Result<corvx_proto::Session> {
    use corvx_proto::{methods, Session, SessionStartParams};
    let first = first
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .unwrap_or(LEAD_FIRST);
    let params = SessionStartParams {
        project: project.to_string(),
        command: std::env::var("CORVX_AGENT_COMMAND").unwrap_or_else(|_| "claude".into()),
        args: vec![
            "--append-system-prompt".to_string(),
            lead_prompt(),
            "--model".into(),
            model.to_string(),
            first.to_string(),
        ],
        runtime: Some("claude".into()),
        backend: None,
        supervisor: None,
        worktree: false,
        label: Some(format!("{LEAD_LABEL}{project}")),
        autopilot: false,
        // The model missions' agents get unless an orchestrator picks another.
        agent_model: Some(DEFAULT_AGENT_MODEL.to_string()),
        human_approves_agents: false,
    };
    client.call::<_, Session>(methods::SESSION_START, &params)
}

const LEAD_PROMPT: &str = "You are the lead for this project, working through CORVX. The human talks to you about \
the project: where it is heading, what to do next, how the work is going. You keep the overview and start missions. \
You do not write code, and you do not run coding agents yourself: each mission has its own orchestrator for that.

How to work:
1. Read the project first: read_knowledge (its brief, goals and guideposts, decisions, conventions, map and recent \
mission journals) and list_agents (missions you started that still run). Answer the human from that.
2. When the human gives you a goal or a direction, make sure you understand it (ask here; the human is in this \
conversation), then start a mission with start_mission. Its goal must be complete on its own (the orchestrator cannot \
see this conversation): what to achieve, what done looks like, the guideposts and decisions that matter, and anything \
the human told you. Name the project goal it serves. Start one mission at a time unless pieces are truly independent. \
Say which missions you started and why.
3. Watch your missions with wait_for, with short timeouts (about 20 seconds) so the human can reach you between \
calls. A mission's orchestrator runs its own agents and decides their requests; you see when a mission is waiting, \
done or failed. When it is waiting, read_screen to see why, and answer with send_message if you can; otherwise ask \
the human here.
4. When the human redirects work (\"you're using too many agents\", \"focus on X first\"), pass it on with \
send_message to the orchestrator concerned, as a clear instruction, and tell the human you did.
5. Goals: when the human states a new goal, or changes or drops one, record it with propose_knowledge (file goals); \
the human accepts it in the CORVX inbox. When a mission completes a goal, propose marking it done. Follow the \
guideposts in everything you start.
6. When a mission ends, tell the human in plain words what was done, what to review or merge, and what you suggest \
next. Do not call report_done and do not stop on your own: you keep running until the human stops you.";

fn lead_prompt() -> String {
    format!(
        "{LEAD_PROMPT}\n\nModels: missions' orchestrators run on opus unless you pass model, and their agents on \
{DEFAULT_AGENT_MODEL} unless you pass agent_model.\n\n{}",
        approvals_policy(false).replace("your agents'", "your missions' orchestrators'")
    )
}

/// `corvx knowledge`: a project's knowledge files and the proposals to change them.
fn knowledge(
    command: Option<KnowledgeCommand>,
    project: Option<String>,
    data_dir: &std::path::Path,
) -> Result<()> {
    use corvx_proto::{
        methods, Knowledge, KnowledgeAccepted, KnowledgeInit, Proposal, ProposalList,
    };
    let mut client = connect(data_dir)?;
    let proposal = |client: &mut corvx_client::Client, id: &str| -> Result<Proposal> {
        let list: ProposalList =
            client.call(methods::KNOWLEDGE_PROPOSALS, &json!({"all": true}))?;
        list.proposals
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| anyhow::anyhow!("no proposal '{id}'"))
    };
    match command {
        None => {
            let Some(project) = project else {
                anyhow::bail!("which project? corvx knowledge <project>");
            };
            let k: Knowledge = client.call(
                methods::KNOWLEDGE_READ,
                &json!({"project": project, "journal": 0}),
            )?;
            println!("{} · {}", k.project, k.path);
            for f in &k.files {
                let what = if f.name == "map" {
                    if f.exists {
                        "generated".to_string()
                    } else {
                        "not written yet".to_string()
                    }
                } else if !f.exists {
                    "not written yet".to_string()
                } else {
                    let sections = f.text.lines().filter(|l| l.starts_with("## ")).count();
                    format!("{sections} section{}", if sections == 1 { "" } else { "s" })
                };
                println!("  {:<24} {what}", f.path);
            }
            println!("  {:<24} {} entries", ".corvx/journal/", k.journal_count);
            println!(
                "  agents read it:          {}",
                if k.wired {
                    "yes, through CLAUDE.md".to_string()
                } else {
                    format!("only orchestrators · corvx knowledge init {}", k.project)
                }
            );
            if k.open.is_empty() {
                println!("no proposals waiting");
            } else {
                println!("waiting for you:");
                for p in &k.open {
                    println!("  {}  {}: {}", p.id, p.file, p.title);
                }
                println!("corvx knowledge show <id> · accept <id> [--edit] · reject <id> [reason]");
            }
            Ok(())
        }
        Some(KnowledgeCommand::Init { project }) => {
            let done: KnowledgeInit =
                client.call(methods::KNOWLEDGE_INIT, &json!({"project": project}))?;
            for f in &done.written {
                println!("wrote {f}");
            }
            if done.committed {
                println!("committed");
            }
            if let Some(note) = done.note {
                println!("{note}");
            }
            println!("Edit .corvx/brief.md to say what the project is and what matters now.");
            Ok(())
        }
        Some(KnowledgeCommand::Proposals { project, all }) => {
            let list: ProposalList = client.call(
                methods::KNOWLEDGE_PROPOSALS,
                &json!({"project": project, "all": all}),
            )?;
            if list.proposals.is_empty() {
                println!("no proposals{}", if all { "" } else { " waiting" });
            }
            for p in list.proposals {
                println!(
                    "{:<22} {:<9} {:<12} {:<12} {}",
                    p.id, p.status, p.project, p.file, p.title
                );
            }
            Ok(())
        }
        Some(KnowledgeCommand::Edit { project, file }) => {
            let list: ProjectList = client.call(methods::PROJECT_LIST, &json!({}))?;
            let p = list
                .projects
                .into_iter()
                .find(|p| p.id == project)
                .ok_or_else(|| anyhow::anyhow!("no project '{project}'"))?;
            let path = std::path::Path::new(&p.path).join(corvx_core::knowledge::rel_path(&file));
            if !path.is_file() {
                anyhow::bail!(
                    "{} has no {} yet · corvx knowledge init {}",
                    p.id,
                    path.display(),
                    p.id
                );
            }
            if !edit_file(&path)? {
                println!("unchanged");
                return Ok(());
            }
            let done: KnowledgeInit = client.call(
                methods::KNOWLEDGE_COMMIT,
                &json!({"project": p.id, "file": file}),
            )?;
            println!(
                "saved .corvx/{file}.md{}",
                if done.committed { " and committed" } else { "" }
            );
            if let Some(note) = done.note {
                println!("{note}");
            }
            Ok(())
        }
        Some(KnowledgeCommand::Show { id }) => {
            let p = proposal(&mut client, &id)?;
            print!("{}", describe_proposal(&p));
            Ok(())
        }
        Some(KnowledgeCommand::Accept { id, edit }) => {
            let p = proposal(&mut client, &id)?;
            let text = if edit {
                match edit_text(&p.text)? {
                    Some(t) => Some(t),
                    None => {
                        println!("unchanged: nothing accepted");
                        return Ok(());
                    }
                }
            } else {
                None
            };
            let done: KnowledgeAccepted =
                client.call(methods::KNOWLEDGE_ACCEPT, &json!({"id": id, "text": text}))?;
            println!(
                "accepted: written to {}{}",
                done.file,
                if done.committed { " and committed" } else { "" }
            );
            if let Some(note) = done.note {
                println!("{note}");
            }
            Ok(())
        }
        Some(KnowledgeCommand::Reject { id, reason }) => {
            let reason = (!reason.is_empty()).then(|| reason.join(" "));
            client.call::<_, serde_json::Value>(
                methods::KNOWLEDGE_REJECT,
                &json!({"id": id, "reason": reason}),
            )?;
            println!("rejected; it will not be proposed again");
            Ok(())
        }
    }
}

/// A proposal in full, as text.
pub(crate) fn describe_proposal(p: &corvx_proto::Proposal) -> String {
    let mut out = format!(
        "{} · {} · .corvx/{}.md\n{}\n",
        p.id, p.status, p.file, p.title
    );
    if let Some(e) = &p.evidence {
        out.push_str(&format!("evidence: {e}\n"));
    }
    out.push_str(&format!("proposed by {}\n", p.by));
    if let Some(r) = &p.replaces {
        out.push_str(&format!("\nreplaces:\n{r}\n\nwith:\n"));
    } else {
        out.push('\n');
    }
    out.push_str(&p.text);
    out.push('\n');
    if let Some(r) = &p.reason {
        out.push_str(&format!("\nrejected: {r}\n"));
    }
    out
}

/// Lets the human edit a text in $VISUAL or $EDITOR (vi if neither is set). Returns None
/// when they leave it unchanged or empty.
pub(crate) fn edit_text(initial: &str) -> Result<Option<String>> {
    let path = std::env::temp_dir().join(format!(
        "corvx-edit-{}-{}.md",
        std::process::id(),
        corvx_core::store::now_secs()
    ));
    std::fs::write(&path, initial)?;
    let edited = run_editor(&path);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    edited?;
    let text = text.trim().to_string();
    Ok((!text.is_empty() && text != initial.trim()).then_some(text))
}

/// Opens a file in $VISUAL or $EDITOR. Returns whether its content changed.
pub(crate) fn edit_file(path: &std::path::Path) -> Result<bool> {
    let before = std::fs::read(path).ok();
    run_editor(path)?;
    Ok(std::fs::read(path).ok() != before)
}

fn run_editor(path: &std::path::Path) -> Result<()> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    // The editor may come with arguments, e.g. "code --wait".
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("corvx-edit")
        .arg(path)
        .status()
        .with_context(|| format!("cannot start the editor '{editor}'"))?;
    if !status.success() {
        anyhow::bail!("the editor '{editor}' exited with {status}");
    }
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
