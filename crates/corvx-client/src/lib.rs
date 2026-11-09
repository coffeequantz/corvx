//! Client for `corvxd`: connects to the daemon's socket and starts the daemon
//! when it is not running.
//!
//! Transport is a Unix domain socket (macOS and Linux) for now. Windows will
//! use a named pipe; until then the connect functions report that it is not
//! supported there.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use corvx_core::paths;
use corvx_proto::{methods, DaemonInfo, Request, Response, PROTOCOL_VERSION};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

/// An error returned by the daemon, as opposed to a transport failure.
#[derive(Debug, thiserror::Error)]
#[error("{message} (code {code})")]
pub struct RpcFailure {
    pub code: i64,
    pub message: String,
}

pub struct Client {
    reader: BufReader<std::os::unix::net::UnixStream>,
    writer: std::os::unix::net::UnixStream,
    next_id: u64,
}

impl Client {
    /// Connects with read and write timeouts, for callers that must never hang, such as
    /// the hook command an agent runs before each tool call.
    pub fn connect_with_timeout(socket: &Path, timeout: Duration) -> Result<Self> {
        let client = Self::connect(socket)?;
        client.writer.set_read_timeout(Some(timeout))?;
        client.writer.set_write_timeout(Some(timeout))?;
        Ok(client)
    }

    pub fn connect(socket: &Path) -> Result<Self> {
        let stream = std::os::unix::net::UnixStream::connect(socket)
            .with_context(|| format!("cannot connect to {}", socket.display()))?;
        let writer = stream.try_clone()?;
        Ok(Self {
            reader: BufReader::new(stream),
            writer,
            next_id: 1,
        })
    }

    /// Sends one request and waits for its response.
    pub fn call<P, R>(&mut self, method: &str, params: &P) -> Result<R>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let id = self.next_id;
        self.next_id += 1;

        let req = Request::new(id, method, serde_json::to_value(params)?);
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()?;

        let mut buf = String::new();
        if self.reader.read_line(&mut buf)? == 0 {
            bail!("daemon closed the connection");
        }
        let resp: Response = serde_json::from_str(&buf).context("malformed response")?;
        if let Some(e) = resp.error {
            return Err(RpcFailure {
                code: e.code,
                message: e.message,
            }
            .into());
        }
        let value = resp.result.unwrap_or(Value::Null);
        serde_json::from_value(value).context("unexpected response shape")
    }

    pub fn info(&mut self) -> Result<DaemonInfo> {
        self.call(methods::DAEMON_INFO, &serde_json::json!({}))
    }
}

/// True if the error means the connection itself is gone (the daemon was
/// restarted or stopped), so a reconnect may help.
pub fn is_connection_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<RpcFailure>().is_none()
}

/// Connects to the daemon, starting it if needed, and checks the protocol version.
///
/// `daemon_bin` overrides the binary. Otherwise `CORVXD_BIN`, then a `corvxd`
/// next to the current executable, then `corvxd` on `PATH` is used.
pub fn ensure_daemon(data_dir: &Path, daemon_bin: Option<&Path>) -> Result<Client> {
    let socket = paths::socket_path(data_dir);
    if let Ok(mut client) = Client::connect(&socket) {
        check_protocol(&mut client)?;
        return Ok(client);
    }

    paths::check_socket_path(&socket).map_err(anyhow::Error::msg)?;
    spawn_daemon(data_dir, daemon_bin)?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(mut client) = Client::connect(&socket) {
            check_protocol(&mut client)?;
            return Ok(client);
        }
        if Instant::now() >= deadline {
            bail!(
                "corvxd did not start within 5s; see {}",
                paths::log_path(data_dir).display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn check_protocol(client: &mut Client) -> Result<()> {
    let info = client.info()?;
    if info.protocol_version != PROTOCOL_VERSION {
        bail!(
            "daemon speaks protocol v{}, this client speaks v{}; restart the daemon with a matching version",
            info.protocol_version,
            PROTOCOL_VERSION
        );
    }
    Ok(())
}

fn spawn_daemon(data_dir: &Path, daemon_bin: Option<&Path>) -> Result<()> {
    fs::create_dir_all(data_dir)?;
    let bin = resolve_daemon_bin(daemon_bin);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::log_path(data_dir))?;
    let log_err = log.try_clone()?;
    let mut cmd = Command::new(&bin);
    cmd.arg("--data-dir")
        .arg(data_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    // Run the daemon in its own session, detached from this terminal, so closing the
    // terminal that happened to start it does not stop it (or the agents it runs).
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
        .map_err(|e| anyhow!("cannot start {}: {e}", bin.display()))?;
    Ok(())
}

fn resolve_daemon_bin(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Some(p) = std::env::var_os("CORVXD_BIN") {
        return PathBuf::from(p);
    }
    let name = if cfg!(windows) {
        "corvxd.exe"
    } else {
        "corvxd"
    };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(name);
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from(name)
}
