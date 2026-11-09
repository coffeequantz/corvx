//! Locations of CORVX data on disk.
//!
//! Default: `~/.local/share/corvx`. Override with the `CORVX_HOME` environment
//! variable or the `--data-dir` flag of `corvx` and `corvxd`.

use std::path::{Path, PathBuf};

pub fn default_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CORVX_HOME") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".local").join("share").join("corvx")
}

pub fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join("corvx.db")
}

/// Unix socket the daemon listens on. Unix socket paths are limited to about
/// 100 bytes, so keep the data directory short.
pub fn socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join("corvxd.sock")
}

/// Unix socket paths are limited (104 bytes on macOS, 108 on Linux).
pub fn check_socket_path(socket: &Path) -> Result<(), String> {
    const LIMIT: usize = 103;
    let len = socket.as_os_str().len();
    if len > LIMIT {
        return Err(format!(
            "socket path is {len} bytes, over the {LIMIT}-byte limit: {}. Use a shorter data directory (CORVX_HOME or --data-dir).",
            socket.display()
        ));
    }
    Ok(())
}

pub fn log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("corvxd.log")
}
