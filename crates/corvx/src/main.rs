//! `corvx`: command-line client and terminal UI for the CORVX daemon.
//!
//! With no arguments, opens the terminal UI. Every command here is a thin
//! wrapper over a protocol call, so scripts can do the same thing.

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
}

#[derive(Subcommand)]
enum AttentionCommand {
    /// Acknowledge an item: you have seen it.
    Ack { id: String },
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
    /// Forward a runtime hook event (JSON on stdin) to the daemon. Used by the hooks CORVX registers.
    #[command(hide = true)]
    Hook { id: String },
}

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
        } => {
            let mut parts = command.into_iter();
            let program = parts.next().unwrap_or_default();
            let params = SessionStartParams {
                project,
                command: program,
                args: parts.collect(),
                runtime,
                backend,
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
                "{:<20} {:<16} {:<12} {:<26} {:<6} COMMAND",
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
                println!(
                    "{:<20} {:<16} {:<12} {:<26} {:<6} {}",
                    s.id,
                    s.project,
                    state,
                    agent,
                    s.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                    s.command.join(" ")
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
                "{:<18} {:<12} {:<18} {:<14} PATH",
                "ID", "WORKSPACE", "STACK", "TEST"
            );
            for p in list.projects {
                println!(
                    "{:<18} {:<12} {:<18} {:<14} {}",
                    p.id,
                    p.workspace,
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
        payload,
    };
    let _: Result<serde_json::Value> = client.call(methods::SESSION_HOOK, &params);
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
                println!(
                    "{:<26} {:<9} {:<16} {:<20} {}",
                    i.id, i.kind, i.project, i.session_id, i.summary
                );
            }
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

#[cfg(test)]
mod tests {
    use super::route_keys;

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
