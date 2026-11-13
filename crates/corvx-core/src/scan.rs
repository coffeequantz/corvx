//! L1 structure scan: detects what a project is built with and how to test it.
//!
//! This is deliberately simple and file-based. It reads manifests and
//! lockfiles; it never runs project code.

use std::fs;
use std::path::Path;

use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scan {
    pub stack: Vec<String>,
    pub test_command: Option<String>,
    pub build_command: Option<String>,
    pub branch: Option<String>,
}

pub fn scan(root: &Path) -> Scan {
    let mut s = Scan::default();

    if root.join("Cargo.toml").is_file() {
        s.stack.push("rust".into());
        set_if_none(&mut s.build_command, "cargo build");
        set_if_none(&mut s.test_command, "cargo test");
    }

    if root.join("package.json").is_file() {
        s.stack.push("node".into());
        let pm = if root.join("pnpm-lock.yaml").is_file() {
            "pnpm"
        } else if root.join("yarn.lock").is_file() {
            "yarn"
        } else {
            "npm"
        };
        if let Some(scripts) =
            read_json(&root.join("package.json")).and_then(|v| v.get("scripts").cloned())
        {
            if scripts.get("test").is_some() {
                set_if_none(&mut s.test_command, &format!("{pm} test"));
            }
            if scripts.get("build").is_some() {
                set_if_none(&mut s.build_command, &format!("{pm} run build"));
            }
        }
    }

    let has_manifest = root.join("pyproject.toml").is_file()
        || root.join("setup.py").is_file()
        || root.join("requirements.txt").is_file();
    if has_manifest || has_file_with_extension(root, "py") {
        s.stack.push("python".into());
        let uses_pytest = ["pyproject.toml", "requirements.txt", "setup.py"]
            .iter()
            .any(|f| {
                fs::read_to_string(root.join(f))
                    .map(|t| t.contains("pytest"))
                    .unwrap_or(false)
            });
        if uses_pytest {
            set_if_none(&mut s.test_command, "pytest");
        } else if root.join("tests").is_dir() {
            // Standard-library tests need no runner; anything else is assumed to be pytest.
            let texts: Vec<String> = fs::read_dir(root.join("tests"))
                .map(|d| {
                    d.filter_map(|e| e.ok())
                        .filter(|e| e.path().extension().is_some_and(|x| x == "py"))
                        .filter_map(|e| fs::read_to_string(e.path()).ok())
                        .collect()
                })
                .unwrap_or_default();
            let unittest = texts.iter().any(|t| t.contains("import unittest"))
                && texts.iter().all(|t| !t.contains("pytest"));
            let cmd = if unittest {
                "python3 -m unittest"
            } else {
                "pytest"
            };
            set_if_none(&mut s.test_command, cmd);
        }
    }

    if root.join("go.mod").is_file() {
        s.stack.push("go".into());
        set_if_none(&mut s.build_command, "go build ./...");
        set_if_none(&mut s.test_command, "go test ./...");
    }

    if let Ok(makefile) = fs::read_to_string(root.join("Makefile")) {
        let has_target = |name: &str| makefile.lines().any(|l| l.starts_with(&format!("{name}:")));
        if has_target("test") {
            s.stack.push("make".into());
            set_if_none(&mut s.test_command, "make test");
        }
        if has_target("build") {
            if !s.stack.iter().any(|x| x == "make") {
                s.stack.push("make".into());
            }
            set_if_none(&mut s.build_command, "make build");
        }
    }

    s.branch = git_branch(root);
    s
}

/// Whether a directory (not its subdirectories) holds a file with this extension.
fn has_file_with_extension(dir: &Path, ext: &str) -> bool {
    fs::read_dir(dir)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .any(|e| e.path().is_file() && e.path().extension().is_some_and(|x| x == ext))
        })
        .unwrap_or(false)
}

fn set_if_none(slot: &mut Option<String>, value: &str) {
    if slot.is_none() {
        *slot = Some(value.to_string());
    }
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Reads the current branch from `.git/HEAD`. Returns the short commit id when
/// HEAD is detached. Worktrees and submodules use a `.git` file, which is not
/// followed here, so they report no branch.
fn git_branch(root: &Path) -> Option<String> {
    let head = fs::read_to_string(root.join(".git").join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(r) = head.strip_prefix("ref: refs/heads/") {
        Some(r.to_string())
    } else if head.len() >= 7 {
        Some(head[..7].to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn plain_python_with_standard_library_tests() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("server.py"), "print(1)\n").unwrap();
        fs::create_dir(dir.path().join("tests")).unwrap();
        fs::write(dir.path().join("tests/test_server.py"), "import unittest\n").unwrap();
        let s = scan(dir.path());
        assert_eq!(s.stack, vec!["python"]);
        assert_eq!(s.test_command.as_deref(), Some("python3 -m unittest"));

        fs::write(dir.path().join("tests/conftest.py"), "import pytest\n").unwrap();
        assert_eq!(scan(dir.path()).test_command.as_deref(), Some("pytest"));
    }

    #[test]
    fn empty_directory_has_no_stack() {
        let dir = tempdir().unwrap();
        let s = scan(dir.path());
        assert!(s.stack.is_empty());
        assert_eq!(s.test_command, None);
        assert_eq!(s.branch, None);
    }

    #[test]
    fn rust_project_uses_cargo() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let s = scan(dir.path());
        assert_eq!(s.stack, vec!["rust"]);
        assert_eq!(s.test_command.as_deref(), Some("cargo test"));
        assert_eq!(s.build_command.as_deref(), Some("cargo build"));
    }

    #[test]
    fn node_project_uses_package_manager_from_lockfile() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"vitest","build":"tsc"}}"#,
        )
        .unwrap();
        fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();
        let s = scan(dir.path());
        assert_eq!(s.stack, vec!["node"]);
        assert_eq!(s.test_command.as_deref(), Some("pnpm test"));
        assert_eq!(s.build_command.as_deref(), Some("pnpm run build"));
    }

    #[test]
    fn node_project_without_test_script_has_no_test_command() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"dev":"vite"}}"#,
        )
        .unwrap();
        let s = scan(dir.path());
        assert_eq!(s.stack, vec!["node"]);
        assert_eq!(s.test_command, None);
    }

    #[test]
    fn mixed_project_lists_every_stack_and_keeps_first_command() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        fs::write(dir.path().join("go.mod"), "module x").unwrap();
        let s = scan(dir.path());
        assert_eq!(s.stack, vec!["rust", "go"]);
        assert_eq!(s.test_command.as_deref(), Some("cargo test"));
    }

    #[test]
    fn reads_branch_from_git_head() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(
            dir.path().join(".git").join("HEAD"),
            "ref: refs/heads/feat/pdf\n",
        )
        .unwrap();
        assert_eq!(scan(dir.path()).branch.as_deref(), Some("feat/pdf"));
    }

    #[test]
    fn detached_head_reports_short_commit() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git").join("HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(scan(dir.path()).branch.as_deref(), Some("0123456"));
    }
}
