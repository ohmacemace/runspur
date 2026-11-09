use std::{
    env, fs,
    io::{self, ErrorKind},
    os::windows::{fs::MetadataExt, process::CommandExt},
    path::{Component, Path, PathBuf, Prefix},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use scheduler_core::{CredentialRef, Operation, WIRE_VERSION};
use scheduler_platform::{DataPaths, LocalClient};
use serde::{Deserialize, Serialize};
use tokio::{sync::Mutex, time};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
const HEALTH_TIMEOUT: Duration = Duration::from_secs(4);
const START_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Channel {
    Development,
    Installed,
}

#[derive(Deserialize, Serialize)]
struct Health {
    version: String,
    protocol_version: u32,
    platform: String,
    active_workers: u32,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Connection {
    Ready { health: Health },
    Offline,
    Error { detail: String },
}

impl From<Result<Option<Health>>> for Connection {
    fn from(result: Result<Option<Health>>) -> Self {
        match result {
            Ok(Some(health)) => Self::Ready { health },
            Ok(None) => Self::Offline,
            Err(error) => Self::Error {
                detail: format!("{error:#}"),
            },
        }
    }
}

#[derive(Serialize)]
pub struct Snapshot {
    channel: Channel,
    state_dir: PathBuf,
    #[serde(flatten)]
    connection: Connection,
}

pub struct Service {
    channel: Channel,
    default_state: PathBuf,
    preferences_path: PathBuf,
    launch_lock: Mutex<()>,
}

impl Service {
    pub fn current_user() -> Result<Self> {
        let channel = if tauri::is_dev() {
            Channel::Development
        } else {
            Channel::Installed
        };
        let local_app_data = env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is unavailable")?;
        Self::for_channel(channel, Path::new(&local_app_data))
    }

    fn for_channel(channel: Channel, local_app_data: &Path) -> Result<Self> {
        let desktop_state = local_app_data.join("Runspur").join("state");
        let (default_state, preferences_path) = match channel {
            Channel::Development => (
                desktop_state.join("desktop-dev"),
                desktop_state.join("desktop-dev.settings.json"),
            ),
            Channel::Installed => (
                DataPaths::default_root()?,
                desktop_state.join("desktop.settings.json"),
            ),
        };
        validate_local_path(&default_state)?;
        validate_local_path(&preferences_path)?;
        Ok(Self {
            channel,
            default_state,
            preferences_path,
            launch_lock: Mutex::new(()),
        })
    }

    pub async fn status(&self) -> Snapshot {
        self.snapshot(probe(&self.default_state).await)
    }

    pub fn preferences_path(&self) -> &Path {
        &self.preferences_path
    }

    pub fn operator_client(&self) -> Result<LocalClient> {
        validate_state_path(&self.default_state)?;
        operator_client(&self.default_state)
    }

    pub async fn start(&self) -> Snapshot {
        let _launch = self.launch_lock.lock().await;
        let result = start_default(&self.default_state).await;
        self.snapshot(result.map(Some))
    }

    fn snapshot(&self, result: Result<Option<Health>>) -> Snapshot {
        Snapshot {
            channel: self.channel,
            state_dir: self.default_state.clone(),
            connection: result.into(),
        }
    }
}

fn validate_local_path(path: &Path) -> Result<()> {
    let local_drive = matches!(
        path.components().next(),
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
    );
    if !path.is_absolute() || !local_drive {
        bail!("Enter an absolute path on a local Windows drive.");
    }
    Ok(())
}

fn validate_state_path(path: &Path) -> Result<()> {
    for directory in path.ancestors().take(3) {
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 => {
                bail!(
                    "A Runspur state directory is redirected: {}",
                    directory.display()
                );
            }
            Err(error) if error.kind() != ErrorKind::NotFound => {
                return Err(error).context("Cannot inspect the scheduler state directory");
            }
            _ => {}
        }
    }
    Ok(())
}

async fn probe(state_dir: &Path) -> Result<Option<Health>> {
    validate_state_path(state_dir)?;
    match fs::metadata(state_dir) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Cannot read the scheduler state directory"),
        Ok(metadata) if !metadata.is_dir() => bail!("The scheduler state path is not a directory."),
        Ok(_) => {}
    }
    if fs::read_dir(state_dir)?.next().is_none() {
        return Ok(None);
    }
    let client = operator_client(state_dir)?;
    let result = time::timeout(HEALTH_TIMEOUT, client.call(Operation::Health))
        .await
        .context("The scheduler did not respond to its health check in time")?;
    let value = match result {
        Ok(value) => value,
        Err(error) if is_missing_pipe(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let health: Health = serde_json::from_value(value).context("Invalid scheduler health data")?;
    if health.protocol_version != WIRE_VERSION {
        bail!("The scheduler uses an unsupported protocol version.");
    }
    if health.version != env!("CARGO_PKG_VERSION") {
        bail!(
            "The running scheduler is version {}. This application requires version {}.",
            health.version,
            env!("CARGO_PKG_VERSION")
        );
    }
    Ok(Some(health))
}

fn operator_client(state_dir: &Path) -> Result<LocalClient> {
    Ok(LocalClient {
        paths: DataPaths::open(state_dir)?,
        credential: CredentialRef::Admin,
        origin: None,
    })
}

fn is_missing_pipe(error: &anyhow::Error) -> bool {
    error.to_string() == "Local pipe is unavailable"
        && error
            .downcast_ref::<io::Error>()
            .is_some_and(|cause| cause.kind() == ErrorKind::NotFound)
}

async fn start_default(state_dir: &Path) -> Result<Health> {
    if let Some(health) = probe(state_dir).await? {
        return Ok(health);
    }

    let desktop = env::current_exe().context("Cannot locate the desktop executable")?;
    let scheduler = desktop.with_file_name("schedulerd.exe");
    validate_state_path(state_dir)?;
    let mut child = Command::new(&scheduler)
        .arg("--data-dir")
        .arg(state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .with_context(|| format!("Cannot launch {}", scheduler.display()))?;

    let ready = time::timeout(START_TIMEOUT, async {
        loop {
            if let Some(health) = probe(state_dir).await? {
                return Ok(health);
            }
            time::sleep(Duration::from_millis(175)).await;
        }
    })
    .await;
    match ready {
        Ok(result) => result,
        Err(_) => match child.try_wait()? {
            Some(exit) => bail!("The bundled scheduler exited before becoming ready ({exit})."),
            None => bail!("The scheduler is still starting. Refresh its status in a moment."),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_shares_cli_state_and_development_is_isolated() {
        let local = Path::new(r"C:\Users\user\AppData\Local");
        let installed = Service::for_channel(Channel::Installed, local).unwrap();
        let development = Service::for_channel(Channel::Development, local).unwrap();
        assert_eq!(installed.default_state, DataPaths::default_root().unwrap());
        assert_eq!(
            development.default_state,
            local.join(r"Runspur\state\desktop-dev")
        );
        assert_eq!(
            installed.preferences_path(),
            local.join(r"Runspur\state\desktop.settings.json")
        );
        assert_eq!(
            development.preferences_path(),
            local.join(r"Runspur\state\desktop-dev.settings.json")
        );
    }

    #[test]
    fn scheduler_requires_an_absolute_local_drive_path() {
        assert!(validate_local_path(Path::new(r"C:\project data\state")).is_ok());
        assert!(validate_local_path(Path::new(r"C:state")).is_err());
        assert!(validate_local_path(Path::new(r"\\host\share\state")).is_err());
    }

    #[test]
    fn missing_pipe_does_not_mask_access_or_credential_errors() {
        let missing = || io::Error::from_raw_os_error(2);
        assert!(is_missing_pipe(
            &anyhow::Error::from(missing()).context("Local pipe is unavailable")
        ));
        assert!(!is_missing_pipe(
            &anyhow::Error::from(missing()).context("Cannot load the credential")
        ));
        assert!(!is_missing_pipe(
            &anyhow::Error::from(io::Error::from_raw_os_error(5))
                .context("Local pipe is unavailable")
        ));
    }
}
