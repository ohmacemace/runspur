//! Operator actions used by the desktop and local browser interfaces.

use std::{
    collections::BTreeSet,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use scheduler_core::{
    AgentKind, AgentProfile, Operation, ProjectRecord, TaskDetail, WorkerAccess, new_id,
};
use scheduler_platform::LocalClient;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::setup::{
    ProjectRoles, ProjectSetup, ProjectSetupPreview, desktop_parent_profile,
    desktop_worker_profile, prepare_project_configuration, read_project_at_root, seed_profiles,
};

#[derive(Default)]
pub struct ProjectPreviews(Mutex<Option<(String, ProjectSetup)>>);

#[derive(Deserialize, Serialize)]
pub struct ProjectDraft {
    root: PathBuf,
    #[serde(default)]
    registered: bool,
    name: String,
    main_agent_profile_id: String,
    worker_profile_ids: BTreeSet<String>,
    allow_workspace_write: bool,
    #[serde(default)]
    allow_project_write: bool,
}

#[derive(Serialize)]
pub struct OperatorProfile {
    profile_id: String,
    kind: AgentKind,
    parent: bool,
    worker: bool,
}

#[derive(Serialize)]
pub struct OperatorProject {
    #[serde(flatten)]
    record: ProjectRecord,
    main_agent_kind: Option<AgentKind>,
}

#[derive(Serialize)]
pub struct PreviewReply {
    token: String,
    allow_workspace_write: bool,
    allow_project_write: bool,
    #[serde(flatten)]
    preview: ProjectSetupPreview,
}

pub async fn scheduler_settings(client: &LocalClient) -> Result<Value> {
    client.call(Operation::GetSchedulerSettings).await
}

pub async fn set_scheduler_settings(
    client: &LocalClient,
    max_concurrent_workers: Option<usize>,
    worker_access: Option<WorkerAccess>,
) -> Result<Value> {
    client
        .call(Operation::SetSchedulerSettings {
            max_concurrent_workers,
            worker_access,
        })
        .await
}

pub async fn projects_list(client: &LocalClient) -> Result<Vec<OperatorProject>> {
    let projects: Vec<ProjectRecord> =
        serde_json::from_value(client.call(Operation::ListProjects).await?)?;
    let profiles: Vec<AgentProfile> =
        serde_json::from_value(client.call(Operation::ListProfiles).await?)?;
    Ok(projects
        .into_iter()
        .map(|record| OperatorProject {
            main_agent_kind: profiles
                .iter()
                .find(|profile| profile.profile_id == record.config.main_agent_profile_id)
                .map(|profile| profile.kind.clone()),
            record,
        })
        .collect())
}

pub async fn agent_profiles(client: &LocalClient) -> Result<Vec<OperatorProfile>> {
    let profiles = seed_profiles(client).await?;
    Ok(profiles
        .into_iter()
        .filter_map(|profile| {
            let parent = desktop_parent_profile(&profile);
            let worker = desktop_worker_profile(&profile);
            (parent || worker).then_some(OperatorProfile {
                profile_id: profile.profile_id,
                kind: profile.kind,
                parent,
                worker,
            })
        })
        .collect())
}

pub async fn project_read(client: &LocalClient, root: &Path) -> Result<ProjectDraft> {
    let root = tokio::fs::canonicalize(root)
        .await
        .context("Cannot open project folder")?;
    let manifest = read_project_at_root(&root).await?;
    let records: Vec<ProjectRecord> =
        serde_json::from_value(client.call(Operation::ListProjects).await?)?;
    let registered_config = records
        .into_iter()
        .find(|record| record.root == root)
        .map(|record| record.config);
    let registered = registered_config.is_some();
    Ok(match manifest.or(registered_config) {
        Some(config) => ProjectDraft {
            root,
            registered,
            name: config.name,
            main_agent_profile_id: config.main_agent_profile_id,
            worker_profile_ids: config.allowed_worker_profile_ids,
            allow_workspace_write: config.limits.allow_workspace_write,
            allow_project_write: config.limits.allow_project_write,
        },
        None => {
            let name = root
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("Runspur project")
                .to_owned();
            ProjectDraft {
                root,
                registered: false,
                name,
                main_agent_profile_id: "codex-desktop".into(),
                worker_profile_ids: BTreeSet::new(),
                allow_workspace_write: false,
                allow_project_write: false,
            }
        }
    })
}

pub async fn project_preview(
    client: &LocalClient,
    previews: &ProjectPreviews,
    input: ProjectDraft,
    program: &Path,
) -> Result<PreviewReply> {
    let mut pending = previews.0.lock().await;
    *pending = None;
    if !program.is_file() {
        bail!("Runspur MCP executable is missing: {}", program.display());
    }
    let program = program
        .to_str()
        .context("The Runspur MCP path is not valid Unicode")?;
    let allow_workspace_write = input.allow_workspace_write || input.allow_project_write;
    let roles = ProjectRoles {
        name: input.name,
        main_agent_profile_id: input.main_agent_profile_id,
        worker_profile_ids: input.worker_profile_ids,
        allow_workspace_write,
        allow_project_write: input.allow_project_write,
    };
    let plan =
        prepare_project_configuration(client, &input.root, |_| roles, program, &client.paths.root)
            .await?;
    let token = new_id();
    let preview = plan.preview();
    *pending = Some((token.clone(), plan));
    Ok(PreviewReply {
        token,
        allow_workspace_write,
        allow_project_write: input.allow_project_write,
        preview,
    })
}

pub async fn project_apply(
    client: &LocalClient,
    previews: &ProjectPreviews,
    token: &str,
) -> Result<ProjectRecord> {
    let mut pending = previews.0.lock().await;
    let Some((_, plan)) = pending.take_if(|(expected, _)| expected == token) else {
        bail!("Preview the project changes again before confirming.");
    };
    plan.apply(client).await
}

pub async fn query_sessions(
    client: &LocalClient,
    project_id: String,
    before: Option<i64>,
    limit: u32,
) -> Result<Value> {
    client
        .call(Operation::ListSessions {
            project_id,
            before,
            limit,
        })
        .await
}

pub async fn query_task_history(
    client: &LocalClient,
    project_id: Option<String>,
    origin_id: Option<String>,
    before: Option<i64>,
    limit: u32,
) -> Result<Value> {
    let operation = if let Some(origin_id) = origin_id {
        Operation::OriginTaskHistory {
            project_id: project_id.context("An origin filter requires a project")?,
            origin_id,
            before,
            limit,
        }
    } else {
        Operation::TaskHistory {
            project_id,
            session_id: None,
            before,
            limit,
        }
    };
    client.call(operation).await
}

pub async fn query_task_detail(client: &LocalClient, task_id: &str) -> Result<TaskDetail> {
    client.task_detail(task_id).await
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub enum Language {
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "en")]
    English,
    #[serde(rename = "zh-CN")]
    SimplifiedChinese,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Preferences {
    #[serde(default)]
    pub language: Language,
}

pub fn load_preferences(path: &Path) -> Result<Preferences> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("Cannot read Runspur preferences"),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Preferences::default()),
        Err(error) => Err(error).context("Cannot open Runspur preferences"),
    }
}

pub fn save_preferences(path: &Path, preferences: Preferences) -> Result<()> {
    let directory = path
        .parent()
        .context("Cannot locate the preferences directory")?;
    fs::create_dir_all(directory)?;
    let mut pending = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut pending, &preferences)?;
    pending.as_file().sync_all()?;
    pending
        .persist(path)
        .context("Cannot save Runspur preferences")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[tokio::test]
    async fn project_previews_replace_previous_tokens_and_apply_only_once() {
        use scheduler_core::{CredentialRef, PROJECT_FILE};
        use scheduler_platform::{DataPaths, LocalListener};
        use scheduler_runtime::{Scheduler, serve};

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("project");
        fs::create_dir(&root).unwrap();
        let paths = DataPaths::prepare(&directory.path().join("state")).unwrap();
        let listener = LocalListener::bind(&paths.endpoint().unwrap()).unwrap();
        let scheduler = Scheduler::new(paths.clone(), 1).unwrap();
        let program = std::env::current_exe().unwrap();
        for (profile_id, kind, executable) in [
            ("codex-desktop", AgentKind::CodexDesktop, None),
            (
                "claude-fixture",
                AgentKind::ClaudePrint,
                Some(program.clone()),
            ),
        ] {
            scheduler
                .store
                .put_profile(AgentProfile {
                    profile_id: profile_id.into(),
                    kind,
                    program: executable,
                    model: None,
                    reasoning_effort: None,
                    environment_keys: Default::default(),
                })
                .await
                .unwrap();
        }
        let service = tokio::spawn(serve(scheduler.clone(), listener));
        let client = LocalClient {
            paths,
            credential: CredentialRef::Admin,
            origin: None,
        };
        let previews = ProjectPreviews::default();

        let mut original = project_read(&client, &root).await.unwrap();
        assert!(!original.registered);
        original.worker_profile_ids.insert("claude-fixture".into());
        let first = project_preview(&client, &previews, original, &program)
            .await
            .unwrap();

        let mut renamed = project_read(&client, &root).await.unwrap();
        renamed.name = "Renamed project".into();
        renamed.worker_profile_ids.insert("claude-fixture".into());
        let second = project_preview(&client, &previews, renamed, &program)
            .await
            .unwrap();
        assert!(!root.join(PROJECT_FILE).exists());
        assert!(
            project_apply(&client, &previews, &first.token)
                .await
                .is_err()
        );
        let saved = project_apply(&client, &previews, &second.token)
            .await
            .unwrap();
        assert_eq!(saved.config.name, "Renamed project");
        assert!(
            project_apply(&client, &previews, &second.token)
                .await
                .is_err()
        );
        assert!(project_read(&client, &root).await.unwrap().registered);

        scheduler.request_shutdown();
        service.await.unwrap().unwrap();
    }

    #[test]
    fn older_preferences_default_to_system_language() {
        let preferences: Preferences =
            serde_json::from_str(r#"{"start_scheduler_on_launch":false}"#).unwrap();
        let saved = serde_json::to_value(preferences).unwrap();
        assert_eq!(saved, serde_json::json!({"language":"system"}));
    }

    #[test]
    fn legacy_settings_preserve_language_without_the_startup_flag() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("preferences.json");
        assert_eq!(load_preferences(&path).unwrap().language, Language::System);
        fs::write(
            &path,
            br#"{"language":"zh-CN","start_scheduler_on_launch":false}"#,
        )
        .unwrap();
        assert_eq!(
            load_preferences(&path).unwrap().language,
            Language::SimplifiedChinese
        );

        let preferences = Preferences {
            language: Language::English,
        };
        save_preferences(&path, preferences).unwrap();
        assert_eq!(load_preferences(&path).unwrap(), preferences);
        let persisted: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted, serde_json::json!({"language":"en"}));
    }
}
