//! Maps protocol methods onto core operations.
//!
//! `Core::handle` is the single entry point. The daemon calls it for every
//! request, and tests call it directly without a socket.

use std::fs;
use std::path::{Path, PathBuf};

use corvx_proto::{
    codes, methods, rule_actions, AttentionAnswerParams, AttentionDismissParams, AttentionIdParams,
    AttentionList, AttentionListParams, AttentionSnoozeParams, DaemonInfo, KnowledgeAcceptParams,
    KnowledgeCommitParams, KnowledgeInitParams, KnowledgeProposalsParams, KnowledgeProposeParams,
    KnowledgeReadParams, KnowledgeRejectParams, Project, ProjectAddParams, ProjectAutonomyParams,
    ProjectList, ProjectRemoveParams, ProjectUpdateParams, ProposalList, RuleList, RulesAddParams,
    RulesListParams, RulesRemoveParams, RulesSetParams, PROTOCOL_VERSION,
};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::knowledge;
use crate::policy;
use crate::scan::scan;
use crate::store::{now_secs, Store, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// An error to return to the client, with a protocol error code.
    #[error("{1}")]
    Rpc(i64, String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl ServiceError {
    fn rpc(code: i64, message: impl Into<String>) -> Self {
        ServiceError::Rpc(code, message.into())
    }
}

pub struct Core {
    store: Store,
    data_dir: PathBuf,
    started_at: i64,
}

impl Core {
    pub fn new(store: Store, data_dir: PathBuf) -> Self {
        Self {
            store,
            data_dir,
            started_at: now_secs(),
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn handle(&self, method: &str, params: Value) -> Result<Value, ServiceError> {
        match method {
            methods::DAEMON_INFO => {
                Ok(serde_json::to_value(self.daemon_info()).expect("DaemonInfo serializes"))
            }
            methods::PROJECT_ADD => {
                let p: ProjectAddParams = parse(params)?;
                Ok(serde_json::to_value(self.project_add(p)?).expect("Project serializes"))
            }
            methods::PROJECT_LIST => Ok(serde_json::to_value(ProjectList {
                projects: self.store.list_projects()?,
            })
            .expect("ProjectList serializes")),
            methods::PROJECT_REMOVE => {
                let p: ProjectRemoveParams = parse(params)?;
                self.project_remove(&p.id)?;
                Ok(json!({"removed": p.id}))
            }
            methods::ATTENTION_LIST => {
                let p: AttentionListParams = parse(params)?;
                let items = self.store.list_attention(p.include_snoozed, now_secs())?;
                Ok(
                    serde_json::to_value(AttentionList { items })
                        .expect("AttentionList serializes"),
                )
            }
            methods::ATTENTION_ACK => {
                let p: AttentionIdParams = parse(params)?;
                self.attention_must_exist(&p.id, self.store.ack_attention(&p.id, now_secs())?)?;
                self.store
                    .append_event("attention.acknowledged", &json!({"id": p.id}))?;
                Ok(json!({"ok": true}))
            }
            methods::ATTENTION_SNOOZE => {
                let p: AttentionSnoozeParams = parse(params)?;
                let now = now_secs();
                let until = now + i64::from(p.minutes) * 60;
                self.attention_must_exist(&p.id, self.store.snooze_attention(&p.id, until, now)?)?;
                self.store.append_event(
                    "attention.snoozed",
                    &json!({"id": p.id, "minutes": p.minutes}),
                )?;
                Ok(json!({"snoozed_until": until}))
            }
            methods::ATTENTION_ANSWER => {
                let p: AttentionAnswerParams = parse(params)?;
                if !self.store.answer_question(&p.id, &p.answer, now_secs())? {
                    return Err(ServiceError::rpc(
                        codes::ATTENTION_NOT_FOUND,
                        format!("no open question '{}'", p.id),
                    ));
                }
                self.store
                    .append_event("attention.answered", &json!({"id": p.id}))?;
                Ok(json!({"ok": true}))
            }
            methods::ATTENTION_GET => {
                let p: AttentionIdParams = parse(params)?;
                let item = self.store.get_attention(&p.id)?.ok_or_else(|| {
                    ServiceError::rpc(
                        codes::ATTENTION_NOT_FOUND,
                        format!("no attention item '{}'", p.id),
                    )
                })?;
                Ok(serde_json::to_value(item).expect("AttentionItem serializes"))
            }
            methods::ATTENTION_DISMISS => {
                let p: AttentionDismissParams = parse(params)?;
                if self
                    .store
                    .get_proposal(&p.id)?
                    .is_some_and(|x| x.status == "open")
                {
                    knowledge::reject(&self.store, &p.id, p.reason.as_deref(), now_secs())?;
                    return Ok(json!({"ok": true}));
                }
                let reason = p.reason.unwrap_or_else(|| "dismissed".into());
                self.attention_must_exist(
                    &p.id,
                    self.store.dismiss_attention(&p.id, &reason, now_secs())?,
                )?;
                self.store.append_event(
                    "attention.dismissed",
                    &json!({"id": p.id, "reason": reason}),
                )?;
                policy::suggestion_dismissed(&self.store, &p.id, now_secs())?;
                Ok(json!({"ok": true}))
            }
            methods::KNOWLEDGE_READ => {
                let p: KnowledgeReadParams = parse(params)?;
                let k = knowledge::read(&self.store, &p.project, p.journal.unwrap_or(2))?;
                Ok(serde_json::to_value(k).expect("Knowledge serializes"))
            }
            methods::KNOWLEDGE_INIT => {
                let p: KnowledgeInitParams = parse(params)?;
                let done = knowledge::init(&self.store, &p.project)?;
                Ok(serde_json::to_value(done).expect("KnowledgeInit serializes"))
            }
            methods::KNOWLEDGE_PROPOSE => {
                let p: KnowledgeProposeParams = parse(params)?;
                let proposal = knowledge::propose(&self.store, &p, now_secs())?;
                Ok(serde_json::to_value(proposal).expect("Proposal serializes"))
            }
            methods::KNOWLEDGE_PROPOSALS => {
                let p: KnowledgeProposalsParams = parse(params)?;
                let proposals = self.store.list_proposals(p.project.as_deref(), p.all)?;
                Ok(serde_json::to_value(ProposalList { proposals })
                    .expect("ProposalList serializes"))
            }
            methods::KNOWLEDGE_ACCEPT => {
                let p: KnowledgeAcceptParams = parse(params)?;
                let done = knowledge::accept(&self.store, &p.id, p.text.as_deref(), now_secs())?;
                Ok(serde_json::to_value(done).expect("KnowledgeAccepted serializes"))
            }
            methods::KNOWLEDGE_COMMIT => {
                let p: KnowledgeCommitParams = parse(params)?;
                let done = knowledge::commit_file(&self.store, &p.project, &p.file)?;
                Ok(serde_json::to_value(done).expect("KnowledgeInit serializes"))
            }
            methods::KNOWLEDGE_REJECT => {
                let p: KnowledgeRejectParams = parse(params)?;
                knowledge::reject(&self.store, &p.id, p.reason.as_deref(), now_secs())?;
                Ok(json!({"rejected": p.id}))
            }
            methods::PROJECT_UPDATE => {
                let p: ProjectUpdateParams = parse(params)?;
                Ok(serde_json::to_value(self.project_update(p)?).expect("Project serializes"))
            }
            methods::PROJECT_AUTONOMY => {
                let p: ProjectAutonomyParams = parse(params)?;
                policy::validate_level(&p.level)
                    .map_err(|e| ServiceError::rpc(codes::INVALID_PARAMS, e))?;
                if !self.store.set_project_autonomy(&p.id, &p.level)? {
                    return Err(ServiceError::rpc(
                        codes::PROJECT_NOT_FOUND,
                        format!("no project '{}'", p.id),
                    ));
                }
                self.store
                    .append_event("project.autonomy", &json!({"id": p.id, "level": p.level}))?;
                let project = self.store.get_project(&p.id)?;
                Ok(serde_json::to_value(project).expect("Project serializes"))
            }
            methods::RULES_LIST => {
                let p: RulesListParams = parse(params)?;
                let rules = policy::rules_for(&self.store, p.project.as_deref())?;
                Ok(serde_json::to_value(RuleList { rules }).expect("RuleList serializes"))
            }
            methods::RULES_ADD => {
                let p: RulesAddParams = parse(params)?;
                policy::validate(&p.kind, &p.pattern)
                    .map_err(|e| ServiceError::rpc(codes::RULE_INVALID, e))?;
                if p.reason.trim().is_empty() {
                    return Err(ServiceError::rpc(
                        codes::RULE_INVALID,
                        "a rule needs a reason: the inbox shows it",
                    ));
                }
                if p.action == rule_actions::OFF {
                    return Err(ServiceError::rpc(
                        codes::RULE_INVALID,
                        "a new rule asks, allows or denies",
                    ));
                }
                policy::validate_action(&p.action)
                    .map_err(|e| ServiceError::rpc(codes::RULE_INVALID, e))?;
                if let Some(project) = &p.project {
                    self.project_must_exist(project)?;
                }
                let id = self.store.add_rule(
                    p.project.as_deref(),
                    &p.kind,
                    &p.pattern,
                    p.reason.trim(),
                    &p.action,
                    now_secs(),
                )?;
                self.store.append_event(
                    "rules.added",
                    &json!({"id": id, "project": p.project, "kind": p.kind,
                            "pattern": p.pattern, "reason": p.reason, "action": p.action}),
                )?;
                Ok(json!({"id": id}))
            }
            methods::RULES_REMOVE => {
                let p: RulesRemoveParams = parse(params)?;
                if policy::is_builtin(&p.id) {
                    return Err(ServiceError::rpc(
                        codes::RULE_INVALID,
                        format!("'{}' is built in: set it to off instead", p.id),
                    ));
                }
                if !self.store.remove_rule(&p.id)? {
                    return Err(ServiceError::rpc(
                        codes::RULE_NOT_FOUND,
                        format!("no rule '{}'", p.id),
                    ));
                }
                self.store
                    .append_event("rules.removed", &json!({"id": p.id}))?;
                Ok(json!({"ok": true}))
            }
            methods::RULES_SET => {
                let p: RulesSetParams = parse(params)?;
                let known = policy::is_builtin(&p.id)
                    || self.store.added_rules()?.iter().any(|r| r.id == p.id);
                if !known {
                    return Err(ServiceError::rpc(
                        codes::RULE_NOT_FOUND,
                        format!("no rule '{}'", p.id),
                    ));
                }
                let action = match p.action.as_str() {
                    "default" => None,
                    a => {
                        policy::validate_action(a).map_err(|e| {
                            ServiceError::rpc(codes::RULE_INVALID, format!("{e}, or default"))
                        })?;
                        Some(a)
                    }
                };
                if let Some(project) = &p.project {
                    self.project_must_exist(project)?;
                }
                self.store
                    .set_rule_action(&p.id, p.project.as_deref().unwrap_or(""), action)?;
                self.store.append_event(
                    "rules.set",
                    &json!({"id": p.id, "project": p.project, "action": p.action}),
                )?;
                Ok(json!({"ok": true}))
            }
            other => Err(ServiceError::rpc(
                codes::METHOD_NOT_FOUND,
                format!("unknown method: {other}"),
            )),
        }
    }

    fn project_must_exist(&self, id: &str) -> Result<(), ServiceError> {
        if self.store.list_projects()?.iter().any(|p| p.id == id) {
            Ok(())
        } else {
            Err(ServiceError::rpc(
                codes::PROJECT_NOT_FOUND,
                format!("no project '{id}'"),
            ))
        }
    }

    fn attention_must_exist(&self, id: &str, changed: bool) -> Result<(), ServiceError> {
        if changed {
            Ok(())
        } else {
            Err(ServiceError::rpc(
                codes::ATTENTION_NOT_FOUND,
                format!("no open attention item '{id}'"),
            ))
        }
    }

    fn daemon_info(&self) -> DaemonInfo {
        DaemonInfo {
            name: "corvxd".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id(),
            data_dir: self.data_dir.display().to_string(),
            started_at: self.started_at,
        }
    }

    fn project_add(&self, params: ProjectAddParams) -> Result<Project, ServiceError> {
        let raw = Path::new(&params.path);
        let root = fs::canonicalize(raw).map_err(|_| {
            ServiceError::rpc(
                codes::PATH_INVALID,
                format!("path does not exist: {}", params.path),
            )
        })?;
        if !root.is_dir() {
            return Err(ServiceError::rpc(
                codes::PATH_INVALID,
                format!("not a directory: {}", root.display()),
            ));
        }

        let path_str = root.display().to_string();
        if let Some(existing) = self.store.find_by_path(&path_str)? {
            return Err(ServiceError::rpc(
                codes::PROJECT_EXISTS,
                format!("already registered as '{}'", existing.id),
            ));
        }

        let name = params
            .name
            .filter(|n| !n.trim().is_empty())
            .or_else(|| root.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "project".into());
        let workspace = params
            .workspace
            .filter(|w| !w.trim().is_empty())
            .unwrap_or_else(|| "default".into());

        let found = scan(&root);
        let project = Project {
            id: self.unique_id(&slug(&name))?,
            name,
            path: path_str,
            workspace,
            added_at: now_secs(),
            stack: found.stack,
            test_command: found.test_command,
            build_command: found.build_command,
            branch: found.branch,
            autonomy: corvx_proto::autonomy::GUIDED.into(),
        };
        self.store.insert_project(&project)?;
        self.store.append_event(
            "project.added",
            &json!({"id": project.id, "path": project.path, "stack": project.stack}),
        )?;
        Ok(project)
    }

    fn project_update(&self, params: ProjectUpdateParams) -> Result<Project, ServiceError> {
        let mut p = self.store.get_project(&params.id)?.ok_or_else(|| {
            ServiceError::rpc(
                codes::PROJECT_NOT_FOUND,
                format!("no project '{}'", params.id),
            )
        })?;
        let before = p.clone();
        if let Some(name) = params.name {
            let name = name.trim();
            if name.is_empty() {
                return Err(ServiceError::rpc(
                    codes::INVALID_PARAMS,
                    "a project needs a name",
                ));
            }
            p.name = name.to_string();
        }
        if let Some(ws) = params.workspace {
            let ws = ws.trim();
            p.workspace = if ws.is_empty() { "default" } else { ws }.to_string();
        }
        if params.rescan {
            let found = scan(Path::new(&p.path));
            p.stack = found.stack;
            p.test_command = found.test_command;
            p.build_command = found.build_command;
            p.branch = found.branch;
        }
        if p != before {
            self.store.update_project(&p)?;
            self.store.append_event(
                "project.updated",
                &json!({"id": p.id, "name": p.name, "workspace": p.workspace,
                        "stack": p.stack, "test_command": p.test_command}),
            )?;
        }
        Ok(p)
    }

    fn project_remove(&self, id: &str) -> Result<(), ServiceError> {
        if !self.store.delete_project(id)? {
            return Err(ServiceError::rpc(
                codes::PROJECT_NOT_FOUND,
                format!("no project with id '{id}'"),
            ));
        }
        self.store
            .append_event("project.removed", &json!({"id": id}))?;
        Ok(())
    }

    fn unique_id(&self, base: &str) -> Result<String, ServiceError> {
        let mut candidate = base.to_string();
        let mut n = 2;
        while self.store.get_project(&candidate)?.is_some() {
            candidate = format!("{base}-{n}");
            n += 1;
        }
        Ok(candidate)
    }
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, ServiceError> {
    serde_json::from_value(params)
        .map_err(|e| ServiceError::rpc(codes::INVALID_PARAMS, format!("invalid params: {e}")))
}

/// Lowercase identifier from a name: `"My App!"` becomes `"my-app"`.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "project".into()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn core() -> Core {
        Core::new(
            Store::open_in_memory().unwrap(),
            PathBuf::from("/tmp/unused"),
        )
    }

    fn code_of(err: ServiceError) -> i64 {
        match err {
            ServiceError::Rpc(c, _) => c,
            other => panic!("expected rpc error, got {other:?}"),
        }
    }

    #[test]
    fn slug_normalises_names() {
        assert_eq!(slug("My App!"), "my-app");
        assert_eq!(slug("  --api_gateway--  "), "api-gateway");
        assert_eq!(slug("!!!"), "project");
    }

    #[test]
    fn projects_can_be_renamed_moved_and_rescanned() {
        let core = core();
        let dir = tempfile::tempdir().unwrap();
        let added: Project = serde_json::from_value(
            core.handle(
                methods::PROJECT_ADD,
                json!({"path": dir.path(), "name": "Old"}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(added.stack.is_empty());
        // The project grows a stack after it was added; a rescan finds it.
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let updated: Project = serde_json::from_value(
            core.handle(
                methods::PROJECT_UPDATE,
                json!({"id": added.id, "name": " New name ", "workspace": "work", "rescan": true}),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(updated.id, added.id, "the id stays");
        assert_eq!(updated.name, "New name");
        assert_eq!(updated.workspace, "work");
        assert!(
            updated.stack.iter().any(|s| s == "rust"),
            "{:?}",
            updated.stack
        );
        let list: ProjectList =
            serde_json::from_value(core.handle(methods::PROJECT_LIST, json!({})).unwrap()).unwrap();
        assert_eq!(list.projects[0], updated);

        // An empty name is refused; an empty workspace means the default one.
        let err = core
            .handle(
                methods::PROJECT_UPDATE,
                json!({"id": added.id, "name": "  "}),
            )
            .unwrap_err();
        assert_eq!(code_of(err), codes::INVALID_PARAMS);
        let back: Project = serde_json::from_value(
            core.handle(
                methods::PROJECT_UPDATE,
                json!({"id": added.id, "workspace": ""}),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(back.workspace, "default");
        let err = core
            .handle(methods::PROJECT_UPDATE, json!({"id": "nope", "name": "x"}))
            .unwrap_err();
        assert_eq!(code_of(err), codes::PROJECT_NOT_FOUND);
    }

    #[test]
    fn add_list_remove_roundtrip() {
        let core = core();
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "").unwrap();

        let added: Project = serde_json::from_value(
            core.handle(methods::PROJECT_ADD, json!({"path": dir.path()}))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(added.stack, vec!["rust"]);
        assert_eq!(added.workspace, "default");
        assert_eq!(added.test_command.as_deref(), Some("cargo test"));

        let list: ProjectList =
            serde_json::from_value(core.handle(methods::PROJECT_LIST, json!({})).unwrap()).unwrap();
        assert_eq!(list.projects.len(), 1);

        core.handle(methods::PROJECT_REMOVE, json!({"id": added.id}))
            .unwrap();
        let list: ProjectList =
            serde_json::from_value(core.handle(methods::PROJECT_LIST, json!({})).unwrap()).unwrap();
        assert!(list.projects.is_empty());
    }

    #[test]
    fn duplicate_path_is_rejected() {
        let core = core();
        let dir = tempdir().unwrap();
        core.handle(methods::PROJECT_ADD, json!({"path": dir.path()}))
            .unwrap();
        let err = core
            .handle(methods::PROJECT_ADD, json!({"path": dir.path()}))
            .unwrap_err();
        assert_eq!(code_of(err), codes::PROJECT_EXISTS);
    }

    #[test]
    fn same_name_gets_a_suffixed_id() {
        let core = core();
        let a = tempdir().unwrap();
        let b = tempdir().unwrap();
        let first = core
            .handle(
                methods::PROJECT_ADD,
                json!({"path": a.path(), "name": "app"}),
            )
            .unwrap();
        let second = core
            .handle(
                methods::PROJECT_ADD,
                json!({"path": b.path(), "name": "app"}),
            )
            .unwrap();
        assert_eq!(first["id"], "app");
        assert_eq!(second["id"], "app-2");
    }

    #[test]
    fn missing_path_is_a_path_error() {
        let err = core()
            .handle(
                methods::PROJECT_ADD,
                json!({"path": "/definitely/not/here"}),
            )
            .unwrap_err();
        assert_eq!(code_of(err), codes::PATH_INVALID);
    }

    #[test]
    fn removing_unknown_project_is_an_error() {
        let err = core()
            .handle(methods::PROJECT_REMOVE, json!({"id": "nope"}))
            .unwrap_err();
        assert_eq!(code_of(err), codes::PROJECT_NOT_FOUND);
    }

    #[test]
    fn bad_params_are_reported() {
        let err = core()
            .handle(methods::PROJECT_ADD, json!({"wrong": 1}))
            .unwrap_err();
        assert_eq!(code_of(err), codes::INVALID_PARAMS);
    }

    #[test]
    fn unknown_method_is_reported() {
        let err = core().handle("nope.nope", json!({})).unwrap_err();
        assert_eq!(code_of(err), codes::METHOD_NOT_FOUND);
    }

    #[test]
    fn attention_items_can_be_snoozed_acked_and_dismissed() {
        let core = core();
        // Raise an item directly through the store, as the session manager does.
        core.store()
            .set_session_attention("s1", "app", "blocked", "needs permission", 3, now_secs())
            .unwrap();
        let list: AttentionList =
            serde_json::from_value(core.handle(methods::ATTENTION_LIST, json!({})).unwrap())
                .unwrap();
        assert_eq!(list.items.len(), 1);
        let id = list.items[0].id.clone();

        core.handle(methods::ATTENTION_SNOOZE, json!({"id": id, "minutes": 15}))
            .unwrap();
        let list: AttentionList =
            serde_json::from_value(core.handle(methods::ATTENTION_LIST, json!({})).unwrap())
                .unwrap();
        assert!(list.items.is_empty(), "snoozed items are hidden");

        core.handle(
            methods::ATTENTION_DISMISS,
            json!({"id": id, "reason": "not now"}),
        )
        .unwrap();
        let err = core
            .handle(methods::ATTENTION_ACK, json!({"id": id}))
            .unwrap_err();
        assert_eq!(code_of(err), codes::ATTENTION_NOT_FOUND);
    }

    #[test]
    fn daemon_info_reports_protocol_version() {
        let info: DaemonInfo =
            serde_json::from_value(core().handle(methods::DAEMON_INFO, json!({})).unwrap())
                .unwrap();
        assert_eq!(info.protocol_version, PROTOCOL_VERSION);
        assert_eq!(info.name, "corvxd");
    }
}
