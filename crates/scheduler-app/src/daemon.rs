use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use scheduler_core::{
    AgentKind, AgentProfile, CredentialRef, Operation, ProcessIdentity, WIRE_VERSION,
};
use scheduler_platform::{DataPaths, LocalClient, LocalListener, is_running};
use scheduler_runtime::{Scheduler, serve_on};
use serde::Deserialize;

const HEALTH_TIMEOUT: Duration = Duration::from_secs(4);
#[cfg(windows)]
const START_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Deserialize)]
struct Health {
    version: String,
    protocol_version: u32,
}

/// Use the compatible scheduler for this state directory, starting it in the background if absent.
pub async fn connect(root: &Path) -> Result<LocalClient> {
    let root = std::path::absolute(root)?;
    if let Some(client) = probe(&root).await? {
        return Ok(client);
    }
    start_background(&root).await
}

#[cfg(windows)]
async fn start_background(root: &Path) -> Result<LocalClient> {
    use std::ffi::OsStr;

    let program = std::env::current_exe().context("Cannot locate the Runspur CLI executable")?;
    let child = scheduler_platform::spawn_background(
        &program,
        &[
            OsStr::new("start"),
            OsStr::new("--data-dir"),
            root.as_os_str(),
        ],
    )
    .context("Cannot start the Runspur scheduler in the background")?;
    let ready = tokio::time::timeout(START_TIMEOUT, async {
        loop {
            if let Some(client) = probe(root).await? {
                return Ok(client);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await;
    match ready {
        Ok(result) => result,
        Err(_) => match child.try_wait()? {
            Some(status) => bail!(
                "Runspur scheduler exited with {status} before becoming ready. Run `runspur start` to see startup errors."
            ),
            None => bail!(
                "Runspur scheduler did not become ready within {} seconds",
                START_TIMEOUT.as_secs()
            ),
        },
    }
}

/// Stop the scheduler for this directory and wait until its executable and pipe are released.
pub async fn stop(root: &Path) -> Result<()> {
    let Some(client) = probe(root).await? else {
        return Ok(());
    };
    tokio::time::timeout(STOP_TIMEOUT, async {
        let response = match client.call(Operation::Shutdown).await {
            Ok(response) => response,
            Err(error) if is_missing_pipe(&error) => return Ok(()),
            Err(error) => return Err(error),
        };
        let process: ProcessIdentity =
            serde_json::from_value(response).context("Invalid scheduler shutdown response")?;
        while is_running(&process) {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        Ok(())
    })
    .await
    .context("Runspur scheduler did not stop within 30 seconds")?
}

async fn probe(root: &Path) -> Result<Option<LocalClient>> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Cannot inspect the scheduler state directory"),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("Scheduler state must be a regular application directory");
    }
    if fs::read_dir(root)?.next().transpose()?.is_none() {
        return Ok(None);
    }
    let paths = match DataPaths::open(root) {
        Ok(paths) => paths,
        Err(_) => {
            // Another launch may be writing the state marker between our directory reads.
            tokio::time::sleep(POLL_INTERVAL).await;
            DataPaths::open(root)?
        }
    };
    let client = LocalClient {
        paths,
        credential: CredentialRef::Admin,
        origin: None,
    };
    let value = match tokio::time::timeout(HEALTH_TIMEOUT, client.call(Operation::Health))
        .await
        .context("The scheduler did not respond to its health check in time")?
    {
        Ok(value) => value,
        Err(error) if is_missing_pipe(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let health: Health = serde_json::from_value(value).context("Invalid scheduler health data")?;
    if health.protocol_version != WIRE_VERSION || health.version != env!("CARGO_PKG_VERSION") {
        bail!(
            "The running scheduler is version {} (protocol {}). This Runspur CLI requires version {} (protocol {}).",
            health.version,
            health.protocol_version,
            env!("CARGO_PKG_VERSION"),
            WIRE_VERSION,
        );
    }
    Ok(Some(client))
}

fn is_missing_pipe(error: &anyhow::Error) -> bool {
    error.to_string() == "Local pipe is unavailable"
        && error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|cause| cause.kind() == ErrorKind::NotFound)
}

#[cfg(not(windows))]
async fn start_background(_root: &Path) -> Result<LocalClient> {
    bail!("The Runspur background scheduler is supported on Windows")
}

pub async fn run(data_dir: Option<PathBuf>, max_workers: usize) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "scheduler_runtime=info,schedulerd=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let paths = DataPaths::prepare(&crate::setup::data_root(data_dir)?)?;
    // Acquire the OS endpoint before any database recovery or worker launch.
    let mut listener = LocalListener::bind(&paths.endpoint()?)?;
    let scheduler = Scheduler::new(paths.clone(), max_workers)?;
    let root_profile = AgentProfile {
        profile_id: "codex-desktop".into(),
        kind: AgentKind::CodexDesktop,
        program: None,
        model: None,
        reasoning_effort: None,
        environment_keys: Default::default(),
    };
    if scheduler
        .store
        .profile(&root_profile.profile_id)
        .await
        .is_err()
    {
        scheduler.store.put_profile(root_profile).await?;
    }
    scheduler.recover().await?;
    let background = scheduler.start();
    tracing::info!(target: "schedulerd", data_dir=%paths.root.display(), "Scheduler is accepting project-scoped requests");
    let result = {
        let service = serve_on(scheduler.clone(), &mut listener);
        tokio::pin!(service);
        tokio::select! {
            result = &mut service => result,
            _ = tokio::signal::ctrl_c() => Ok(()),
        }
    };
    scheduler.request_shutdown();
    for job in background {
        let _ = job.await;
    }
    // Keep the first pipe instance until the scheduler finishes draining its work.
    scheduler.shutdown().await;
    result
}
