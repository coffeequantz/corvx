//! `corvxd`: the CORVX daemon. One per node.
//!
//! Listens on a Unix socket in the data directory and answers JSON-RPC
//! requests (one JSON object per line). Agents and sessions will live here
//! too; for now it manages the project registry.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{bail, Context, Result};
use corvx_core::paths;
use corvx_core::service::{Core, ServiceError};
use corvx_core::store::Store;
use corvx_proto::{codes, methods, Request, Response};

mod sessions;
mod tmux;
use sessions::SessionManager;

fn main() {
    if let Err(e) = run() {
        eprintln!("corvxd: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let data_dir = parse_data_dir()?.unwrap_or_else(paths::default_data_dir);
    fs::create_dir_all(&data_dir)?;
    // Owner-only: the socket lets anyone who can reach it run commands as you, and the
    // folder holds transcripts that may contain secrets.
    fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700))?;

    let socket = paths::socket_path(&data_dir);
    paths::check_socket_path(&socket).map_err(anyhow::Error::msg)?;
    if socket.exists() {
        if UnixStream::connect(&socket).is_ok() {
            // Another daemon owns this data directory. Not an error for the caller.
            eprintln!("corvxd: already running at {}", socket.display());
            return Ok(());
        }
        // Stale socket left by a crashed daemon.
        fs::remove_file(&socket)?;
    }

    let store = Store::open(&paths::db_path(&data_dir)).context("opening database")?;
    let core = Core::new(store, data_dir.clone());
    let mut manager = SessionManager::new(data_dir.clone());
    // Adopt sessions a previous daemon left running in tmux; close the records of the rest.
    let recovery = manager
        .recover(&core)
        .map_err(|e| anyhow::anyhow!("recovering sessions: {e}"))?;
    eprintln!(
        "corvxd: session backend {}; adopted {} running session(s), {} ended with the previous daemon",
        manager.default_backend().name(),
        recovery.adopted,
        recovery.lost
    );
    let core = Arc::new(Mutex::new(core));
    let sessions = Arc::new(Mutex::new(manager));

    // Notice exited sessions every half second, whether or not a client asks.
    {
        let core = Arc::clone(&core);
        let sessions = Arc::clone(&sessions);
        thread::spawn(move || loop {
            thread::sleep(std::time::Duration::from_millis(500));
            let core = core.lock().unwrap_or_else(|p| p.into_inner());
            let mut mgr = sessions.lock().unwrap_or_else(|p| p.into_inner());
            if let Err(e) = mgr.tick(&core) {
                eprintln!("corvxd: session check failed: {e}");
            }
        });
    }

    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    eprintln!(
        "corvxd {} (protocol {}) listening on {}",
        env!("CARGO_PKG_VERSION"),
        corvx_proto::PROTOCOL_VERSION,
        socket.display()
    );

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let core = Arc::clone(&core);
                let sessions = Arc::clone(&sessions);
                thread::spawn(move || {
                    if let Err(e) = serve(stream, core, sessions) {
                        eprintln!("corvxd: connection ended: {e}");
                    }
                });
            }
            Err(e) => eprintln!("corvxd: accept failed: {e}"),
        }
    }
    Ok(())
}

/// Serves one client until it disconnects.
fn serve(
    stream: UnixStream,
    core: Arc<Mutex<Core>>,
    sessions: Arc<Mutex<SessionManager>>,
) -> Result<()> {
    let mut writer = stream.try_clone()?;
    let reader = BufReader::new(stream);

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = respond(&line, &core, &sessions);
        let mut out = serde_json::to_string(&response)?;
        out.push('\n');
        writer.write_all(out.as_bytes())?;
        writer.flush()?;
    }
    Ok(())
}

fn respond(line: &str, core: &Mutex<Core>, sessions: &Mutex<SessionManager>) -> Response {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => return Response::err(0, codes::PARSE_ERROR, format!("parse error: {e}")),
    };
    // A poisoned lock means an earlier request panicked. The data is still
    // consistent because each store call is its own transaction.
    let core = core.lock().unwrap_or_else(|p| p.into_inner());
    // Lock order is always core, then sessions.
    // Session work, and what needs to know the session asking (questions, decisions, journal
    // entries), go to the manager.
    let result = if req.method.starts_with("session.")
        || req.method == methods::ATTENTION_ASK
        || req.method == methods::ATTENTION_DECIDE
        || req.method == methods::PROJECT_JOURNAL
    {
        let mut mgr = sessions.lock().unwrap_or_else(|p| p.into_inner());
        mgr.handle(&core, &req.method, req.params)
    } else {
        core.handle(&req.method, req.params)
    };
    match result {
        Ok(value) => Response::ok(req.id, value),
        Err(ServiceError::Rpc(code, message)) => Response::err(req.id, code, message),
        Err(other) => Response::err(req.id, codes::INTERNAL, other.to_string()),
    }
}

/// Accepts `--data-dir <path>`, `--data-dir=<path>` or `--version`.
fn parse_data_dir() -> Result<Option<PathBuf>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => Ok(None),
        [flag] if flag == "--version" => {
            println!("corvxd {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        [flag, value] if flag == "--data-dir" => Ok(Some(PathBuf::from(value))),
        [flag] if flag == "--data-dir" => bail!("--data-dir needs a path"),
        [flag] if flag.starts_with("--data-dir=") => {
            Ok(Some(PathBuf::from(&flag["--data-dir=".len()..])))
        }
        other => bail!("unknown arguments: {}", other.join(" ")),
    }
}
