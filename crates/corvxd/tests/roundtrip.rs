//! End-to-end: start the real `corvxd` binary and talk to it over the socket.

use std::fs;
use std::path::Path;
use std::process::{Child, Command};
use std::thread::sleep;
use std::time::{Duration, Instant};

use corvx_client::{ensure_daemon, Client};
use corvx_core::paths;
use corvx_proto::{methods, Project, ProjectList, PROTOCOL_VERSION};
use serde_json::json;

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_daemon(data_dir: &Path) -> Daemon {
    let child = Command::new(env!("CARGO_BIN_EXE_corvxd"))
        .arg("--data-dir")
        .arg(data_dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn corvxd");
    let socket = paths::socket_path(data_dir);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Client::connect(&socket).is_err() {
        assert!(Instant::now() < deadline, "daemon did not come up");
        sleep(Duration::from_millis(20));
    }
    Daemon { child }
}

/// Short base dir so the socket path stays under the Unix limit.
fn short_tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("corvx-t")
        .tempdir_in("/tmp")
        .expect("tempdir")
}

#[test]
fn project_lifecycle_over_the_socket() {
    let data = short_tempdir();
    let _daemon = start_daemon(data.path());

    let project_dir = short_tempdir();
    fs::write(
        project_dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\n",
    )
    .unwrap();

    let mut client =
        ensure_daemon(data.path(), Some(Path::new(env!("CARGO_BIN_EXE_corvxd")))).unwrap();
    let info = client.info().unwrap();
    assert_eq!(info.protocol_version, PROTOCOL_VERSION);

    let added: Project = client
        .call(
            methods::PROJECT_ADD,
            &json!({"path": project_dir.path(), "name": "Demo App"}),
        )
        .unwrap();
    assert_eq!(added.id, "demo-app");
    assert_eq!(added.stack, vec!["rust"]);
    assert_eq!(added.test_command.as_deref(), Some("cargo test"));

    let list: ProjectList = client.call(methods::PROJECT_LIST, &json!({})).unwrap();
    assert_eq!(list.projects.len(), 1);

    // A second client sees the same state: the daemon is the source of truth.
    let mut second = Client::connect(&paths::socket_path(data.path())).unwrap();
    let list: ProjectList = second.call(methods::PROJECT_LIST, &json!({})).unwrap();
    assert_eq!(list.projects.len(), 1);

    let err = client
        .call::<_, serde_json::Value>(methods::PROJECT_REMOVE, &json!({"id": "missing"}))
        .unwrap_err();
    assert!(err.to_string().contains("code 1001"), "got: {err}");

    client
        .call::<_, serde_json::Value>(methods::PROJECT_REMOVE, &json!({"id": "demo-app"}))
        .unwrap();
    let list: ProjectList = client.call(methods::PROJECT_LIST, &json!({})).unwrap();
    assert!(list.projects.is_empty());
}

#[test]
fn second_daemon_defers_to_the_running_one() {
    let data = short_tempdir();
    let _daemon = start_daemon(data.path());

    let status = Command::new(env!("CARGO_BIN_EXE_corvxd"))
        .arg("--data-dir")
        .arg(data.path())
        .status()
        .expect("run second corvxd");
    assert!(status.success(), "second daemon should exit cleanly");

    // The first daemon is still answering.
    let mut client = Client::connect(&paths::socket_path(data.path())).unwrap();
    assert!(client.info().is_ok());
}

#[test]
fn projects_survive_a_daemon_restart() {
    let data = short_tempdir();
    let project_dir = short_tempdir();
    fs::write(project_dir.path().join("go.mod"), "module demo\n").unwrap();

    {
        let _daemon = start_daemon(data.path());
        let mut client = Client::connect(&paths::socket_path(data.path())).unwrap();
        client
            .call::<_, Project>(methods::PROJECT_ADD, &json!({"path": project_dir.path()}))
            .unwrap();
    } // daemon killed here

    let _daemon = start_daemon(data.path());
    let mut client = Client::connect(&paths::socket_path(data.path())).unwrap();
    let list: ProjectList = client.call(methods::PROJECT_LIST, &json!({})).unwrap();
    assert_eq!(list.projects.len(), 1);
    assert_eq!(list.projects[0].stack, vec!["go"]);
}
