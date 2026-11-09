#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod console;
mod project_setup;
mod service;
mod settings;

use scheduler_core::{AgentKind, AgentProfile, Operation, ProjectRecord, TaskDetail};
use serde::Serialize;
use serde_json::Value;
use service::{Service, Snapshot};
use settings::Preferences;

#[tauri::command]
async fn service_status(service: tauri::State<'_, Service>) -> Result<Snapshot, ()> {
    Ok(service.status().await)
}

#[tauri::command]
async fn start_service(service: tauri::State<'_, Service>) -> Result<Snapshot, ()> {
    Ok(service.start().await)
}

#[tauri::command]
fn get_preferences(service: tauri::State<'_, Service>) -> Result<Preferences, String> {
    settings::load(service.preferences_path()).map_err(|error| format!("{error:#}"))
}

#[tauri::command]
fn set_preferences(
    service: tauri::State<'_, Service>,
    preferences: Preferences,
) -> Result<(), String> {
    settings::save(service.preferences_path(), preferences).map_err(|error| format!("{error:#}"))
}

fn query_error(error: anyhow::Error) -> String {
    format!("{error:#}")
}

async fn query(service: &Service, operation: Operation) -> Result<Value, String> {
    service
        .operator_client()
        .map_err(query_error)?
        .call(operation)
        .await
        .map_err(query_error)
}

#[tauri::command]
async fn projects_list(service: tauri::State<'_, Service>) -> Result<Vec<DesktopProject>, String> {
    let projects: Vec<ProjectRecord> =
        serde_json::from_value(query(&service, Operation::ListProjects).await?)
            .map_err(|error| error.to_string())?;
    let profiles: Vec<AgentProfile> =
        serde_json::from_value(query(&service, Operation::ListProfiles).await?)
            .map_err(|error| error.to_string())?;
    Ok(projects
        .into_iter()
        .map(|record| DesktopProject {
            main_agent_kind: profiles
                .iter()
                .find(|profile| profile.profile_id == record.config.main_agent_profile_id)
                .map(|profile| profile.kind.clone()),
            record,
        })
        .collect())
}

#[derive(Serialize)]
struct DesktopProject {
    #[serde(flatten)]
    record: ProjectRecord,
    main_agent_kind: Option<AgentKind>,
}

#[tauri::command]
async fn query_sessions(
    service: tauri::State<'_, Service>,
    project_id: String,
    before: Option<i64>,
    limit: u32,
) -> Result<Value, String> {
    query(
        &service,
        Operation::ListSessions {
            project_id,
            before,
            limit,
        },
    )
    .await
}

#[tauri::command]
async fn query_task_history(
    service: tauri::State<'_, Service>,
    project_id: Option<String>,
    origin_id: Option<String>,
    before: Option<i64>,
    limit: u32,
) -> Result<Value, String> {
    let operation = if let Some(origin_id) = origin_id {
        Operation::OriginTaskHistory {
            project_id: project_id.ok_or("An origin filter requires a project")?,
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
    query(&service, operation).await
}

#[tauri::command]
async fn query_task_detail(
    service: tauri::State<'_, Service>,
    task_id: String,
) -> Result<TaskDetail, String> {
    service
        .operator_client()
        .map_err(query_error)?
        .task_detail(&task_id)
        .await
        .map_err(query_error)
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Service::current_user().expect("resolve Runspur Desktop state directory"))
        .manage(project_setup::ProjectPreviews::default())
        .invoke_handler(tauri::generate_handler![
            service_status,
            start_service,
            get_preferences,
            set_preferences,
            projects_list,
            query_sessions,
            query_task_history,
            query_task_detail,
            project_setup::choose_project_folder,
            project_setup::agent_profiles,
            project_setup::project_read,
            project_setup::launch_claude_project,
            project_setup::project_preview,
            project_setup::project_apply
        ])
        .run(tauri::generate_context!())
        .expect("run Runspur Desktop");
}
