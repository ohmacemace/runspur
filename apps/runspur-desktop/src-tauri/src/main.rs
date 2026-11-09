#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod console;
mod project_setup;
mod service;

use scheduler_app::operator::{self, OperatorProject, Preferences, ProjectPreviews};
use scheduler_core::{TaskDetail, WorkerAccess};
use serde_json::Value;
use service::{Service, Snapshot};

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
    operator::load_preferences(service.preferences_path()).map_err(query_error)
}

#[tauri::command]
fn set_preferences(
    service: tauri::State<'_, Service>,
    preferences: Preferences,
) -> Result<(), String> {
    operator::save_preferences(service.preferences_path(), preferences).map_err(query_error)
}

#[tauri::command]
async fn get_scheduler_settings(service: tauri::State<'_, Service>) -> Result<Value, String> {
    let client = service.operator_client().map_err(query_error)?;
    operator::scheduler_settings(&client)
        .await
        .map_err(query_error)
}

#[tauri::command]
async fn set_scheduler_settings(
    service: tauri::State<'_, Service>,
    max_concurrent_workers: Option<usize>,
    worker_access: Option<WorkerAccess>,
) -> Result<Value, String> {
    let client = service.operator_client().map_err(query_error)?;
    operator::set_scheduler_settings(&client, max_concurrent_workers, worker_access)
        .await
        .map_err(query_error)
}

fn query_error(error: anyhow::Error) -> String {
    format!("{error:#}")
}

#[tauri::command]
async fn projects_list(service: tauri::State<'_, Service>) -> Result<Vec<OperatorProject>, String> {
    let client = service.operator_client().map_err(query_error)?;
    operator::projects_list(&client).await.map_err(query_error)
}

#[tauri::command]
async fn query_sessions(
    service: tauri::State<'_, Service>,
    project_id: String,
    before: Option<i64>,
    limit: u32,
) -> Result<Value, String> {
    let client = service.operator_client().map_err(query_error)?;
    operator::query_sessions(&client, project_id, before, limit)
        .await
        .map_err(query_error)
}

#[tauri::command]
async fn query_task_history(
    service: tauri::State<'_, Service>,
    project_id: Option<String>,
    origin_id: Option<String>,
    before: Option<i64>,
    limit: u32,
) -> Result<Value, String> {
    let client = service.operator_client().map_err(query_error)?;
    operator::query_task_history(&client, project_id, origin_id, before, limit)
        .await
        .map_err(query_error)
}

#[tauri::command]
async fn query_task_detail(
    service: tauri::State<'_, Service>,
    task_id: String,
) -> Result<TaskDetail, String> {
    let client = service.operator_client().map_err(query_error)?;
    operator::query_task_detail(&client, &task_id)
        .await
        .map_err(query_error)
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Service::current_user().expect("resolve Runspur Desktop state directory"))
        .manage(ProjectPreviews::default())
        .invoke_handler(tauri::generate_handler![
            service_status,
            start_service,
            get_preferences,
            set_preferences,
            get_scheduler_settings,
            set_scheduler_settings,
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
