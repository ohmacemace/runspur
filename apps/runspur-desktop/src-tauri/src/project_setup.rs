use std::{
    collections::BTreeSet,
    env,
    ffi::OsStr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use scheduler_app::setup::{
    ProjectRoles, ProjectSetup, ProjectSetupPreview, desktop_parent_profile,
    desktop_worker_profile, prepare_project_configuration, read_project_at_root, seed_profiles,
};
use scheduler_core::{AgentKind, AgentProfile, Operation, ProjectRecord, fingerprint, new_id};
use serde::{Deserialize, Serialize};
use tauri::{State, WebviewWindow};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::{Mutex, oneshot};

use crate::{console, query_error, service::Service};

#[derive(Default)]
pub(crate) struct ProjectPreviews(Mutex<Option<(String, ProjectSetup)>>);

#[derive(Deserialize, Serialize)]
pub(crate) struct ProjectDraft {
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
pub(crate) struct DesktopProfile {
    profile_id: String,
    kind: AgentKind,
    parent: bool,
    worker: bool,
}

#[derive(Serialize)]
pub(crate) struct PreviewReply {
    token: String,
    allow_workspace_write: bool,
    allow_project_write: bool,
    #[serde(flatten)]
    preview: ProjectSetupPreview,
}

fn bundled_executable(name: &str) -> Result<PathBuf> {
    let path = env::current_exe()
        .context("Cannot locate Runspur Desktop")?
        .with_file_name(name);
    if !path.is_file() {
        anyhow::bail!("Runspur Desktop component is missing: {}", path.display());
    }
    Ok(path)
}

#[tauri::command]
pub(crate) async fn choose_project_folder(
    window: WebviewWindow,
) -> Result<Option<PathBuf>, String> {
    let (send, receive) = oneshot::channel();
    window
        .dialog()
        .file()
        .set_parent(&window)
        .pick_folder(move |folder| {
            let _ = send.send(folder);
        });
    receive
        .await
        .map_err(|error| error.to_string())?
        .map(|folder| folder.into_path().map_err(|error| error.to_string()))
        .transpose()
}

#[tauri::command]
pub(crate) async fn agent_profiles(
    service: State<'_, Service>,
) -> Result<Vec<DesktopProfile>, String> {
    let client = service.operator_client().map_err(query_error)?;
    let profiles = seed_profiles(&client).await.map_err(query_error)?;
    Ok(profiles
        .into_iter()
        .filter_map(|profile| {
            let parent = desktop_parent_profile(&profile);
            let worker = desktop_worker_profile(&profile);
            (parent || worker).then_some(DesktopProfile {
                profile_id: profile.profile_id,
                kind: profile.kind,
                parent,
                worker,
            })
        })
        .collect())
}

async fn read_draft(service: &Service, root: &Path) -> Result<ProjectDraft> {
    let root = tokio::fs::canonicalize(root)
        .await
        .context("Cannot open project folder")?;
    let manifest = read_project_at_root(&root).await?;
    let client = service.operator_client()?;
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

#[tauri::command]
pub(crate) async fn project_read(
    service: State<'_, Service>,
    root: PathBuf,
) -> Result<ProjectDraft, String> {
    read_draft(&service, &root).await.map_err(query_error)
}

#[tauri::command]
pub(crate) async fn launch_claude_project(
    service: State<'_, Service>,
    root: PathBuf,
    resume: Option<String>,
) -> Result<(), String> {
    let launch = async {
        let root = tokio::fs::canonicalize(root)
            .await
            .context("Cannot open project folder")?;
        let manifest = read_project_at_root(&root)
            .await?
            .context("Connect this project before opening Claude Code")?;
        let client = service.operator_client()?;
        let projects: Vec<ProjectRecord> =
            serde_json::from_value(client.call(Operation::ListProjects).await?)?;
        let registered = projects
            .iter()
            .find(|project| project.root == root)
            .context("Connect this project before opening Claude Code")?;
        if fingerprint(&manifest)? != registered.revision {
            anyhow::bail!("Save the current project configuration before opening Claude Code");
        }
        let profiles: Vec<AgentProfile> =
            serde_json::from_value(client.call(Operation::ListProfiles).await?)?;
        if !profiles.iter().any(|profile| {
            profile.profile_id == manifest.main_agent_profile_id
                && profile.kind == AgentKind::ClaudePrint
                && desktop_parent_profile(profile)
        }) {
            anyhow::bail!("This project has no available Claude Code main profile");
        }
        let program = bundled_executable("schedulerctl.exe")?;
        let mut args = vec![
            OsStr::new("--data-dir"),
            client.paths.root.as_os_str(),
            OsStr::new("claude"),
            OsStr::new("--project-root"),
            root.as_os_str(),
        ];
        let resume = resume.map(|session| format!("--resume={session}"));
        if let Some(resume) = resume.as_deref() {
            args.push(OsStr::new(resume));
        }
        console::spawn(&program, &args, &root).context("Cannot open the Claude Code console")?;
        Ok(())
    };
    launch.await.map_err(query_error)
}

#[tauri::command]
pub(crate) async fn project_preview(
    service: State<'_, Service>,
    previews: State<'_, ProjectPreviews>,
    input: ProjectDraft,
) -> Result<PreviewReply, String> {
    let mut pending = previews.0.lock().await;
    *pending = None;
    let client = service.operator_client().map_err(query_error)?;
    let mcp = bundled_executable("scheduler-mcp.exe").map_err(query_error)?;
    let program = mcp
        .to_str()
        .ok_or("The bundled MCP path is not valid Unicode")?;
    let allow_workspace_write = input.allow_workspace_write || input.allow_project_write;
    let roles = ProjectRoles {
        name: input.name,
        main_agent_profile_id: input.main_agent_profile_id,
        worker_profile_ids: input.worker_profile_ids,
        allow_workspace_write,
        allow_project_write: input.allow_project_write,
    };
    let plan =
        prepare_project_configuration(&client, &input.root, |_| roles, program, &client.paths.root)
            .await
            .map_err(query_error)?;
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

#[tauri::command]
pub(crate) async fn project_apply(
    service: State<'_, Service>,
    previews: State<'_, ProjectPreviews>,
    token: String,
) -> Result<ProjectRecord, String> {
    let client = service.operator_client().map_err(query_error)?;
    let mut pending = previews.0.lock().await;
    let Some((_, plan)) = pending.take_if(|(expected, _)| expected == &token) else {
        return Err("Preview the project changes again before confirming.".into());
    };
    plan.apply(&client).await.map_err(query_error)
}
