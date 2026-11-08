use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use sha2::{Digest, Sha256};

const MARKER: &str = ".runspur-state-version";

#[derive(Debug, Clone)]
pub struct DataPaths {
    pub root: PathBuf,
    pub database: PathBuf,
    pub secrets: PathBuf,
    pub artifacts: PathBuf,
}

impl DataPaths {
    pub fn default_root() -> Result<PathBuf> {
        Ok(ProjectDirs::from("dev", "Runspur", "Runspur")
            .context("Cannot locate the current user's application data directory")?
            .data_local_dir()
            .to_path_buf())
    }

    pub fn open(root: &Path) -> Result<Self> {
        let root = fs::canonicalize(root)
            .context("Scheduler state directory does not exist; start schedulerd first")?;
        if fs::read_to_string(root.join(MARKER)).ok().as_deref() != Some("1\n") {
            bail!("Directory is not a managed Runspur state directory");
        }
        Ok(Self::from_root(root))
    }

    pub fn prepare(root: &Path) -> Result<Self> {
        if root.exists() {
            let metadata = fs::symlink_metadata(root)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("State directory must be a regular application directory");
            }
            if !root.join(MARKER).exists() && fs::read_dir(root)?.next().is_some() {
                bail!("Refusing to change permissions on an existing unmanaged nonempty directory");
            }
        } else {
            fs::create_dir_all(root)?;
        }
        let root = fs::canonicalize(root)?;
        if root.join(MARKER).exists() && fs::read_to_string(root.join(MARKER))? != "1\n" {
            bail!("Unsupported Runspur state directory version");
        }
        #[cfg(windows)]
        crate::windows::secure_directory(&root)?;
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        if !root.join(MARKER).exists() {
            fs::write(root.join(MARKER), "1\n")?;
        }
        let paths = Self::from_root(root);
        fs::create_dir_all(&paths.secrets)?;
        fs::create_dir_all(&paths.artifacts)?;
        Ok(paths)
    }

    fn from_root(root: PathBuf) -> Self {
        Self {
            database: root.join("scheduler.sqlite3"),
            secrets: root.join("credentials"),
            artifacts: root.join("artifacts"),
            root,
        }
    }

    pub fn endpoint(&self) -> Result<String> {
        let identity = crate::process::current_principal()?;
        let digest = hex::encode(Sha256::digest(
            format!("{}\0{}", self.root.display(), identity).as_bytes(),
        ));
        #[cfg(windows)]
        {
            Ok(format!(r"\\.\pipe\runspur-{}", &digest[..24]))
        }
        #[cfg(not(windows))]
        {
            Ok(std::env::temp_dir()
                .join(format!("runspur-{}.sock", &digest[..24]))
                .to_string_lossy()
                .into())
        }
    }

    pub fn attempt_dir(&self, task_id: &str, attempt_id: &str) -> Result<PathBuf> {
        uuid::Uuid::parse_str(task_id).context("Invalid task ID")?;
        uuid::Uuid::parse_str(attempt_id).context("Invalid attempt ID")?;
        Ok(self.artifacts.join(task_id).join(attempt_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refuses_unmanaged_state_and_path_escape() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("user-work"), "keep").unwrap();
        assert!(DataPaths::prepare(dir.path()).is_err());
        let app = dir.path().join("app");
        let paths = DataPaths::prepare(&app).unwrap();
        assert_eq!(DataPaths::open(&app).unwrap().root, paths.root);
        assert!(
            paths
                .attempt_dir("../escape", &uuid::Uuid::new_v4().to_string())
                .is_err()
        );
    }
}
