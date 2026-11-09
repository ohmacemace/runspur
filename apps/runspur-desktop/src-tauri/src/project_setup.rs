use std::{env, ffi::OsStr, path::PathBuf};

use anyhow::{Context, Result};
use scheduler_app::{
    operator::{self, OperatorProfile, PreviewReply, ProjectDraft, ProjectPreviews},
    setup::{desktop_parent_profile, read_project_at_root},
};
use scheduler_core::{AgentKind, AgentProfile, Operation, ProjectRecord, fingerprint};
use tauri::{State, WebviewWindow};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::oneshot;

use crate::{console, query_error, service::Service};

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
) -> Result<Vec<OperatorProfile>, String> {
    let client = service.operator_client().map_err(query_error)?;
    operator::agent_profiles(&client).await.map_err(query_error)
}

#[tauri::command]
pub(crate) async fn project_read(
    service: State<'_, Service>,
    root: PathBuf,
) -> Result<ProjectDraft, String> {
    let client = service.operator_client().map_err(query_error)?;
    operator::project_read(&client, &root)
        .await
        .map_err(query_error)
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
    let client = service.operator_client().map_err(query_error)?;
    let program = env::current_exe()
        .context("Cannot locate Runspur Desktop")
        .map_err(query_error)?
        .with_file_name("scheduler-mcp.exe");
    operator::project_preview(&client, &previews, input, &program)
        .await
        .map_err(query_error)
}

#[tauri::command]
pub(crate) async fn project_apply(
    service: State<'_, Service>,
    previews: State<'_, ProjectPreviews>,
    token: String,
) -> Result<ProjectRecord, String> {
    let client = service.operator_client().map_err(query_error)?;
    operator::project_apply(&client, &previews, &token)
        .await
        .map_err(query_error)
}
