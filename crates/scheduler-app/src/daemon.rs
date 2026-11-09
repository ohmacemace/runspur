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

#[derive(Clone, Copy)]
enum ProbeVersion {
    CurrentRelease,
    CompatibleProtocol,
}

impl Health {
    fn validate(&self, version: ProbeVersion) -> Result<()> {
        if self.protocol_version != WIRE_VERSION {
            bail!(
                "The running scheduler uses protocol {}. This Runspur CLI requires protocol {}.",
                self.protocol_version,
                WIRE_VERSION,
            );
        }
        if matches!(version, ProbeVersion::CurrentRelease)
            && self.version != env!("CARGO_PKG_VERSION")
        {
            bail!(
                "The running scheduler is version {}. This Runspur CLI requires version {}. When active tasks have finished, run `runspur stop` and retry.",
                self.version,
                env!("CARGO_PKG_VERSION"),
            );
        }
        Ok(())
    }
}

/// Use the compatible scheduler for this state directory, starting it in the background if absent.
pub async fn connect(root: &Path) -> Result<LocalClient> {
    connect_with(root, None).await
}

pub async fn connect_bundled(root: &Path, program: &Path) -> Result<LocalClient> {
    connect_with(root, Some(program)).await
}

pub async fn health(root: &Path) -> Result<Option<serde_json::Value>> {
    Ok(probe(root, ProbeVersion::CurrentRelease)
        .await?
        .map(|(_, health)| health))
}

async fn connect_with(root: &Path, program: Option<&Path>) -> Result<LocalClient> {
    let root = std::path::absolute(root)?;
    if let Some((client, _)) = probe(&root, ProbeVersion::CurrentRelease).await? {
        return Ok(client);
    }
    start_background(&root, program).await
}

#[cfg(windows)]
async fn start_background(root: &Path, bundled: Option<&Path>) -> Result<LocalClient> {
    use std::ffi::OsStr;

    let current_exe;
    let mut args = Vec::with_capacity(3);
    let program = match bundled {
        Some(program) => program,
        None => {
            current_exe =
                std::env::current_exe().context("Cannot locate the Runspur executable")?;
            args.push(OsStr::new("start"));
            &current_exe
        }
    };
    args.extend([OsStr::new("--data-dir"), root.as_os_str()]);
    let child = scheduler_platform::spawn_background(program, &args)
        .context("Cannot start the Runspur scheduler in the background")?;
    let ready = tokio::time::timeout(START_TIMEOUT, async {
        loop {
            if let Some((client, _)) = probe(root, ProbeVersion::CurrentRelease).await? {
                return Ok(client);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await;
    match ready {
        Ok(result) => result,
        Err(_) => match child.try_wait()? {
            Some(status) => bail!("Runspur scheduler exited with {status} before becoming ready."),
            None => bail!(
                "Runspur scheduler did not become ready within {} seconds",
                START_TIMEOUT.as_secs()
            ),
        },
    }
}

/// Stop the scheduler for this directory and wait until its executable and pipe are released.
pub async fn stop(root: &Path) -> Result<()> {
    let Some((client, _)) = probe(root, ProbeVersion::CompatibleProtocol).await? else {
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

async fn probe(
    root: &Path,
    version: ProbeVersion,
) -> Result<Option<(LocalClient, serde_json::Value)>> {
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
    let health = Health::deserialize(&value).context("Invalid scheduler health data")?;
    health.validate(version)?;
    Ok(Some((client, value)))
}

fn is_missing_pipe(error: &anyhow::Error) -> bool {
    error.to_string() == "Local pipe is unavailable"
        && error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|cause| cause.kind() == ErrorKind::NotFound)
}

#[cfg(not(windows))]
async fn start_background(_root: &Path, _program: Option<&Path>) -> Result<LocalClient> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_accepts_an_older_release_with_the_same_protocol() {
        let mut health = Health {
            version: "0.1.2".into(),
            protocol_version: WIRE_VERSION,
        };
        assert!(health.validate(ProbeVersion::CompatibleProtocol).is_ok());
        assert!(health.validate(ProbeVersion::CurrentRelease).is_err());
        health.protocol_version += 1;
        assert!(health.validate(ProbeVersion::CompatibleProtocol).is_err());
    }
}
