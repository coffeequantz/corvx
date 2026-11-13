//! tmux session backend.
//!
//! Each CORVX session is a tmux session on CORVX's own tmux server: one server per data
//! directory, on its own socket, with CORVX's own config. That gives durable sessions (they
//! keep running when the daemon restarts), a server-side screen, and robust attach/detach.
//!
//! Output goes to the session's transcript through `pipe-pane`. To capture it from the very
//! first byte, a session starts with a placeholder command, the pipe is attached, and then
//! the real command replaces the placeholder.
//!
//! Panes stay around after their command exits (`remain-on-exit`), so the daemon can read
//! the exit status before cleaning up.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// CORVX's tmux configuration. The prefix is Ctrl+\ (Claude Code uses Ctrl+B), and a
/// one-line status bar shows how to detach, highlighted while the prefix is armed.
const CONFIG: &str = r##"# Written by corvxd. Changes are overwritten when the daemon starts.
set -g prefix 'C-\'
unbind C-b
bind 'C-\' send-prefix
bind d detach-client
set -g remain-on-exit on
set -g history-limit 50000
set -sg escape-time 10
set -g default-terminal "xterm-256color"
set -s extended-keys on
set -g focus-events on
set -g mouse off
set -g set-titles on
set -g set-titles-string "corvx · #S"
set -g status on
set -g status-position bottom
set -g status-style "bg=default,fg=colour244"
set -g status-left " corvx · #{?@corvx_label,#{@corvx_label},#S} "
set -g status-left-length 80
# Ctrl+\ then s: switch to another CORVX session, listed by label.
bind s choose-tree -Zs -F "#{?@corvx_label,#{@corvx_label},#{session_name}}"
set -g status-right "#{?client_prefix,#[reverse] ctrl+\\ · press d to detach #[noreverse],ctrl+\\ then d to detach }"
set -g status-right-length 60
set -g window-status-format ""
set -g window-status-current-format ""
"##;

/// Whether a pane's command has exited, and with what status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneState {
    pub dead: bool,
    pub exit_status: Option<i32>,
}

pub struct Tmux {
    socket: PathBuf,
    config: PathBuf,
}

impl Tmux {
    /// Whether tmux is installed.
    pub fn available() -> bool {
        Command::new("tmux")
            .arg("-V")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Prepares the backend for a data directory: writes the config next to the socket.
    pub fn new(data_dir: &Path) -> std::io::Result<Self> {
        let config = data_dir.join("tmux.conf");
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&config)?;
        f.write_all(CONFIG.as_bytes())?;
        let tmux = Self {
            socket: socket_path(data_dir),
            config,
        };
        // A server left running by a previous daemon keeps its old config unless reloaded.
        // With no server running this fails harmlessly.
        let config_path = tmux.config.to_string_lossy().to_string();
        let _ = tmux.run(["source-file", config_path.as_str()]);
        Ok(tmux)
    }

    /// A tmux command for CORVX's server. The environment is cleaned, because the server
    /// inherits it when this command happens to start the server.
    fn command(&self) -> Command {
        let mut cmd = Command::new("tmux");
        cmd.arg("-S").arg(&self.socket).arg("-f").arg(&self.config);
        for (key, _) in std::env::vars_os() {
            if let Some(key) = key.to_str() {
                if crate::sessions::is_inherited_agent_var(key) {
                    cmd.env_remove(key);
                }
            }
        }
        cmd
    }

    fn run<I, S>(&self, args: I) -> Result<String, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let out = self
            .command()
            .args(args)
            .output()
            .map_err(|e| format!("cannot run tmux: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    /// Starts `argv` in a new detached tmux session named `name`, with output appended to
    /// `transcript`. Returns the command's process id when tmux reports it.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &self,
        name: &str,
        cwd: &Path,
        cols: u16,
        rows: u16,
        env: &[(String, String)],
        argv: &[String],
        transcript: &Path,
    ) -> Result<Option<u32>, String> {
        let cwd = cwd.to_string_lossy().to_string();
        let mut env_args: Vec<String> = Vec::new();
        for (k, v) in env {
            env_args.push("-e".into());
            env_args.push(format!("{k}={v}"));
        }

        // 1. A placeholder holds the pane while the transcript pipe is attached.
        let mut new_session: Vec<String> = vec![
            "new-session".into(),
            "-d".into(),
            "-s".into(),
            name.into(),
            "-x".into(),
            cols.to_string(),
            "-y".into(),
            rows.to_string(),
            "-c".into(),
            cwd.clone(),
        ];
        new_session.extend(env_args.iter().cloned());
        new_session.extend(["sh".into(), "-c".into(), "sleep 86400".into()]);
        self.run(&new_session)?;

        // 2. Every byte the pane prints from now on goes to the transcript.
        let pipe = format!("cat >> {}", shell_quote(&transcript.to_string_lossy()));
        if let Err(e) = self.run(["pipe-pane", "-o", "-t", name, pipe.as_str()]) {
            let _ = self.kill(name);
            return Err(e);
        }

        // 3. The real command replaces the placeholder.
        let mut respawn: Vec<String> = vec![
            "respawn-pane".into(),
            "-k".into(),
            "-t".into(),
            name.into(),
            "-c".into(),
            cwd,
        ];
        respawn.extend(env_args);
        respawn.extend(argv.iter().cloned());
        // On macOS, replacing a pane right after it was created sometimes fails to open its
        // new pty ("fork failed: Device not configured"); a moment later it works.
        let mut attempt = 0;
        while let Err(e) = self.run(&respawn) {
            attempt += 1;
            if attempt >= 5 || !e.contains("fork failed") {
                let _ = self.kill(name);
                return Err(e);
            }
            std::thread::sleep(std::time::Duration::from_millis(100 * attempt));
        }

        let pid = self
            .run(["display-message", "-p", "-t", name, "#{pane_pid}"])
            .ok()
            .and_then(|s| s.trim().parse().ok());
        Ok(pid)
    }

    /// Sends text as typed input. Line breaks become Enter; other control characters are
    /// sent as raw key codes.
    pub fn send_text(&self, name: &str, text: &str) -> Result<(), String> {
        let mut literal = String::new();
        let flush = |literal: &mut String| -> Result<(), String> {
            if !literal.is_empty() {
                self.run(["send-keys", "-t", name, "-l", literal.as_str()])?;
                literal.clear();
            }
            Ok(())
        };
        for ch in text.chars() {
            match ch {
                '\r' | '\n' => {
                    flush(&mut literal)?;
                    self.run(["send-keys", "-t", name, "Enter"])?;
                }
                c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                    flush(&mut literal)?;
                    let hex = format!("{:02x}", c as u32);
                    self.run(["send-keys", "-t", name, "-H", hex.as_str()])?;
                }
                c => literal.push(c),
            }
        }
        flush(&mut literal)
    }

    pub fn resize(&self, name: &str, cols: u16, rows: u16) -> Result<(), String> {
        self.run([
            "resize-window",
            "-t",
            name,
            "-x",
            &cols.to_string(),
            "-y",
            &rows.to_string(),
        ])
        .map(|_| ())
    }

    /// Sets the label tmux shows for a session (status bar and session list).
    pub fn set_label(&self, name: &str, label: &str) -> Result<(), String> {
        self.run(["set-option", "-t", name, "@corvx_label", label])
            .map(|_| ())
    }

    pub fn kill(&self, name: &str) -> Result<(), String> {
        self.run(["kill-session", "-t", name]).map(|_| ())
    }

    /// The current screen of a session as plain text, with wrapped lines joined.
    pub fn capture(&self, name: &str) -> Result<String, String> {
        self.run(["capture-pane", "-p", "-J", "-t", name])
    }

    /// The screen with `history` lines above it, as drawn: one line per row, with colours
    /// as escape codes when `ansi`.
    pub fn capture_view(&self, name: &str, history: u32, ansi: bool) -> Result<String, String> {
        let start = format!("-{history}");
        let mut args = vec!["capture-pane", "-p", "-t", name];
        if history > 0 {
            args.extend(["-S", start.as_str()]);
        }
        if ansi {
            args.push("-e");
        }
        self.run(args)
    }

    /// Every pane on CORVX's server, by session name. Empty when the server is not running.
    pub fn panes(&self) -> HashMap<String, PaneState> {
        let Ok(out) = self.run([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}\t#{pane_dead}\t#{pane_dead_status}",
        ]) else {
            return HashMap::new();
        };
        out.lines()
            .filter_map(|line| {
                let mut parts = line.split('\t');
                let name = parts.next()?.to_string();
                let dead = parts.next()? == "1";
                let exit_status = parts.next().and_then(|s| s.parse().ok());
                Some((name, PaneState { dead, exit_status }))
            })
            .collect()
    }
}

/// The socket of CORVX's tmux server for a data directory. The CLI uses it to attach.
pub fn socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join("tmux.sock")
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
