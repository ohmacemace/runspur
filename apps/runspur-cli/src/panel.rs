use std::{
    net::Ipv4Addr,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path as RoutePath, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::post,
};
use scheduler_app::{daemon, operator};
use scheduler_core::{CredentialRef, MAX_FRAME_BYTES, Operation};
use scheduler_platform::{DataPaths, LocalClient};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Mutex};
use tower_http::services::ServeDir;

struct Panel {
    state_dir: PathBuf,
    scheduler: PathBuf,
    bridge: PathBuf,
    preferences: PathBuf,
    authority: String,
    origin: String,
    authorization: String,
    previews: operator::ProjectPreviews,
    service_lock: Mutex<()>,
    folder_lock: Mutex<()>,
}

pub async fn run(native: &Path, state_dir: PathBuf, no_open: bool) -> Result<()> {
    let web = native.join("../web");
    let bridge = crate::bundled_component(native, "scheduler-mcp")?;
    let scheduler = crate::bundled_component(native, "schedulerd")?;
    ensure!(
        web.join("index.html").is_file(),
        "The Runspur browser interface is missing from the installation."
    );

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let authority = listener.local_addr()?.to_string();
    let origin = format!("http://{authority}");
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let url = format!("{origin}/#token={token}");
    let state_dir = std::path::absolute(state_dir)?;
    let panel = Arc::new(Panel {
        preferences: state_dir.with_extension("browser-settings.json"),
        state_dir,
        scheduler,
        bridge,
        authority,
        origin,
        authorization: format!("Bearer {token}"),
        previews: operator::ProjectPreviews::default(),
        service_lock: Mutex::new(()),
        folder_lock: Mutex::new(()),
    });
    let app = Router::new()
        .route("/api/{command}", post(invoke))
        .fallback_service(ServeDir::new(web))
        .layer(DefaultBodyLimit::max(MAX_FRAME_BYTES))
        .layer(axum::middleware::map_response(browser_headers))
        .with_state(panel);

    if no_open {
        println!("Runspur UI: {url}");
    } else if let Err(error) = open_browser(&url) {
        eprintln!("{error:#}. Open this address in your browser.");
        println!("Runspur UI: {url}");
    } else {
        println!("Runspur opened in your browser. Press Ctrl+C to close the panel.");
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

async fn browser_headers(mut response: axum::response::Response) -> axum::response::Response {
    use axum::http::HeaderValue;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
        ),
    );
    response
}

async fn invoke(
    State(panel): State<Arc<Panel>>,
    RoutePath(command): RoutePath<String>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> impl IntoResponse {
    let header = |name| headers.get(name).and_then(|value| value.to_str().ok());
    if header(header::HOST) != Some(panel.authority.as_str())
        || header(header::ORIGIN) != Some(panel.origin.as_str())
        || header(header::AUTHORIZATION) != Some(panel.authorization.as_str())
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "Open this panel with `runspur tui`."})),
        );
    }
    match panel.invoke(&command, args).await {
        Ok(value) => (StatusCode::OK, Json(value)),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("{error:#}")})),
        ),
    }
}

impl Panel {
    fn client(&self) -> Result<LocalClient> {
        Ok(LocalClient {
            paths: DataPaths::open(&self.state_dir)?,
            credential: CredentialRef::Admin,
            origin: None,
        })
    }

    async fn invoke(&self, command: &str, args: Value) -> Result<Value> {
        let value = match command {
            "service_status" => snapshot(daemon::health(&self.state_dir).await),
            "start_service" => {
                let _guard = self.service_lock.lock().await;
                snapshot(
                    match daemon::connect_bundled(&self.state_dir, &self.scheduler).await {
                        Ok(client) => client.call(Operation::Health).await.map(Some),
                        Err(error) => Err(error),
                    },
                )
            }
            "stop_service" => {
                let _guard = self.service_lock.lock().await;
                snapshot(daemon::stop(&self.state_dir).await.map(|()| None))
            }
            "get_preferences" => {
                serde_json::to_value(operator::load_preferences(&self.preferences)?)?
            }
            "set_preferences" => {
                let PreferencesArgs { preferences } = serde_json::from_value(args)?;
                operator::save_preferences(&self.preferences, preferences)?;
                Value::Null
            }
            "choose_project_folder" => {
                let _guard = self
                    .folder_lock
                    .try_lock()
                    .context("A folder picker is already open")?;
                serde_json::to_value(choose_folder().await?)?
            }
            "projects_list" => {
                serde_json::to_value(operator::projects_list(&self.client()?).await?)?
            }
            "agent_profiles" => {
                serde_json::to_value(operator::agent_profiles(&self.client()?).await?)?
            }
            "project_read" => {
                let RootArgs { root } = serde_json::from_value(args)?;
                serde_json::to_value(operator::project_read(&self.client()?, &root).await?)?
            }
            "project_preview" => {
                let PreviewArgs { input } = serde_json::from_value(args)?;
                serde_json::to_value(
                    operator::project_preview(&self.client()?, &self.previews, input, &self.bridge)
                        .await?,
                )?
            }
            "project_apply" => {
                let ApplyArgs { token } = serde_json::from_value(args)?;
                serde_json::to_value(
                    operator::project_apply(&self.client()?, &self.previews, &token).await?,
                )?
            }
            "query_sessions" => {
                let SessionArgs {
                    project_id,
                    before,
                    limit,
                } = serde_json::from_value(args)?;
                operator::query_sessions(&self.client()?, project_id, before, limit).await?
            }
            "query_task_history" => {
                let HistoryArgs {
                    project_id,
                    origin_id,
                    before,
                    limit,
                } = serde_json::from_value(args)?;
                operator::query_task_history(&self.client()?, project_id, origin_id, before, limit)
                    .await?
            }
            "query_task_detail" => {
                let TaskArgs { task_id } = serde_json::from_value(args)?;
                serde_json::to_value(operator::query_task_detail(&self.client()?, &task_id).await?)?
            }
            _ => bail!("Unknown Runspur panel action"),
        };
        Ok(value)
    }
}

fn snapshot(health: Result<Option<Value>>) -> Value {
    match health {
        Ok(Some(health)) => json!({"status":"ready", "channel":"installed", "health":health}),
        Ok(None) => json!({"status":"offline", "channel":"installed"}),
        Err(error) => {
            json!({"status":"error", "channel":"installed", "detail":format!("{error:#}")})
        }
    }
}

#[derive(Deserialize)]
struct PreferencesArgs {
    preferences: operator::Preferences,
}
#[derive(Deserialize)]
struct RootArgs {
    root: PathBuf,
}
#[derive(Deserialize)]
struct PreviewArgs {
    input: operator::ProjectDraft,
}
#[derive(Deserialize)]
struct ApplyArgs {
    token: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionArgs {
    project_id: String,
    before: Option<i64>,
    limit: u32,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryArgs {
    project_id: Option<String>,
    origin_id: Option<String>,
    before: Option<i64>,
    limit: u32,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskArgs {
    task_id: String,
}

#[cfg(windows)]
async fn choose_folder() -> Result<Option<PathBuf>> {
    Ok(tokio::task::spawn_blocking(|| rfd::FileDialog::new().pick_folder()).await?)
}

#[cfg(not(windows))]
async fn choose_folder() -> Result<Option<PathBuf>> {
    bail!("The Runspur folder picker is available on Windows")
}

#[cfg(windows)]
fn open_browser(url: &str) -> Result<()> {
    use std::ptr::null;
    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

    let address: Vec<_> = url.encode_utf16().chain([0]).collect();
    // The URL is generated from a loopback listener and a random launch token.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            null(),
            address.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        )
    };
    ensure!(
        result as isize > 32,
        "Cannot open the default browser (code {})",
        result as isize
    );
    Ok(())
}

#[cfg(not(windows))]
fn open_browser(_url: &str) -> Result<()> {
    bail!("Automatic browser launch is available on Windows")
}
