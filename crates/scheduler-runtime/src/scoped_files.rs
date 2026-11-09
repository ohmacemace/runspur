use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use anyhow::{Context, Result, bail, ensure};
use scheduler_core::valid_scoped_path;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_PROJECT_WRITE_FILES: usize = 64;
const MAX_PROJECT_READ_FILES: usize = 1024;
pub const BROKER_SERVER: &str = "runspur_files";
pub const BROKER_TOOLS: &[&str] = &[
    "mcp__runspur_files__read_file",
    "mcp__runspur_files__list_directory",
    "mcp__runspur_files__write_file",
];

#[derive(Deserialize, Serialize)]
#[serde(tag = "mode", content = "paths", rename_all = "snake_case")]
enum WriteScope {
    Exact(Vec<String>),
    Project,
}

#[derive(Deserialize, Serialize)]
pub struct FileBroker {
    project_root: PathBuf,
    workspace: PathBuf,
    scope: WriteScope,
    #[serde(skip)]
    project_reads: Mutex<BTreeMap<String, Option<String>>>,
}

pub struct ScopedWorkspace {
    broker: FileBroker,
    original: BTreeMap<String, Option<String>>,
}

#[derive(Serialize)]
struct FileChange<'a> {
    path: &'a str,
    before_sha256: Option<&'a str>,
    after_sha256: String,
    artifact_ref: String,
}

impl ScopedWorkspace {
    pub fn prepare(
        project_root: &Path,
        attempt_dir: &Path,
        scope: Option<&[String]>,
    ) -> Result<Self> {
        let scope = match scope {
            Some(paths) => {
                ensure!(
                    !paths.is_empty(),
                    "A scoped workspace needs at least one allowed file"
                );
                WriteScope::Exact(paths.to_vec())
            }
            None => WriteScope::Project,
        };
        let workspace = attempt_dir.join("workspace");
        fs::create_dir(&workspace)?;
        let broker = FileBroker {
            project_root: project_root.to_owned(),
            workspace,
            scope,
            project_reads: Mutex::default(),
        };
        let mut original = BTreeMap::new();
        if let WriteScope::Exact(paths) = &broker.scope {
            for name in paths {
                let source = checked_path(project_root, name, false)?;
                let bytes = read_optional(&source)?;
                original.insert(name.clone(), bytes.as_deref().map(digest));
                if let Some(bytes) = bytes {
                    let destination = checked_path(&broker.workspace, name, true)?;
                    atomic_write(&destination, &bytes)?;
                }
            }
        }
        let ticket = File::options()
            .write(true)
            .create_new(true)
            .open(Self::ticket(attempt_dir))?;
        serde_json::to_writer(ticket, &broker)?;
        Ok(Self { broker, original })
    }

    pub fn ticket(attempt_dir: &Path) -> PathBuf {
        attempt_dir.join("file-broker.json")
    }

    pub fn workspace(attempt_dir: &Path) -> PathBuf {
        attempt_dir.join("workspace")
    }

    pub fn capture_changes(
        &self,
        attempt_dir: &Path,
        artifact_prefix: &str,
    ) -> Result<Option<String>> {
        let project_original;
        let original = if matches!(self.broker.scope, WriteScope::Project) {
            project_original = self.broker.project_baselines()?;
            &project_original
        } else {
            &self.original
        };
        let mut changes = Vec::new();
        for (name, before) in original {
            let path = checked_path(&self.broker.workspace, name, false)?;
            let Some(bytes) = read_optional(&path)? else {
                ensure!(
                    matches!(self.broker.scope, WriteScope::Project) || before.is_none(),
                    "An original file disappeared from the scoped workspace"
                );
                continue;
            };
            let after_sha256 = digest(&bytes);
            if before.as_ref() != Some(&after_sha256) {
                changes.push(FileChange {
                    path: name,
                    before_sha256: before.as_deref(),
                    after_sha256,
                    artifact_ref: format!("{artifact_prefix}/workspace/{name}"),
                });
            }
        }
        if changes.is_empty() {
            return Ok(None);
        }
        let mut manifest = File::options()
            .write(true)
            .create_new(true)
            .open(attempt_dir.join("changes.json"))?;
        serde_json::to_writer_pretty(&mut manifest, &changes)?;
        manifest.sync_all()?;
        Ok(Some(format!("{artifact_prefix}/changes.json")))
    }
}

impl FileBroker {
    pub fn from_ticket(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_FILE_BYTES,
            "The file broker ticket exceeds the size limit"
        );
        let broker: Self = serde_json::from_slice(&bytes)?;
        if let WriteScope::Exact(paths) = &broker.scope {
            ensure!(
                !paths.is_empty(),
                "An exact file broker ticket must authorize at least one file"
            );
        }
        Ok(broker)
    }

    pub fn write_file(&self, name: &str, text: &str) -> Result<()> {
        match &self.scope {
            WriteScope::Project => self.write_project_file(name, text),
            WriteScope::Exact(paths) => write_file(&self.workspace, paths, name, text),
        }
    }

    pub fn read_file(&self, name: &str) -> Result<String> {
        if matches!(self.scope, WriteScope::Project) {
            let mut reads = self.project_reads()?;
            let overlay = checked_path(&self.workspace, name, false)?;
            if let Some(bytes) = read_optional(&overlay)? {
                return String::from_utf8(bytes).context("Only UTF-8 text files can be read");
            }
            let source = checked_path(&self.project_root, name, false)?;
            let bytes = read_optional(&source)?;
            let key = path_key(name);
            let hash = bytes.as_deref().map(digest);
            match reads.get(&key) {
                Some(prior) => ensure!(
                    prior == &hash,
                    "Project file changed after the worker read it"
                ),
                None => {
                    ensure!(
                        reads.len() < MAX_PROJECT_READ_FILES,
                        "Project read limit reached for this task"
                    );
                    reads.insert(key, hash);
                }
            }
            return String::from_utf8(bytes.context("File not found")?)
                .context("Only UTF-8 text files can be read");
        }
        let root = if matches!(&self.scope, WriteScope::Exact(paths) if paths.iter().any(|path| path == name))
        {
            &self.workspace
        } else {
            &self.project_root
        };
        let path = checked_path(root, name, false)?;
        let bytes = read_optional(&path)?.context("File not found")?;
        String::from_utf8(bytes).context("Only UTF-8 text files can be read")
    }

    pub fn list_directory(&self, name: &str) -> Result<Vec<String>> {
        let original = directory_path(&self.project_root, name)?;
        let mut names = BTreeSet::new();
        if matches!(self.scope, WriteScope::Project) {
            let _lock = self.project_reads()?;
            let overlay = directory_path(&self.workspace, name)?;
            let overlay_exists = collect_names(&overlay, &mut names)?;
            let original_exists = collect_names(&original, &mut names)?;
            ensure!(overlay_exists || original_exists, "Directory not found");
        } else {
            ensure!(collect_names(&original, &mut names)?, "Directory not found");
        }
        Ok(names.into_iter().take(256).collect())
    }

    fn write_project_file(&self, name: &str, text: &str) -> Result<()> {
        ensure!(
            text.len() <= MAX_FILE_BYTES,
            "Write denied: the file exceeds 1 MiB"
        );
        let reads = self.project_reads()?;
        let source = checked_path(&self.project_root, name, false)?;
        let mut baselines = self.project_baselines()?;
        if !baselines.contains_key(name) {
            ensure!(
                baselines.len() < MAX_PROJECT_WRITE_FILES,
                "Project write limit reached for this task"
            );
            let key = path_key(name);
            let overlay = checked_path(&self.workspace, name, false)?;
            ensure!(
                !baselines.keys().any(|path| path_key(path) == key) && !overlay.try_exists()?,
                "Use the same project-relative spelling as the first write to this file"
            );
            let source_bytes = read_optional(&source)?;
            if let Some(bytes) = &source_bytes {
                std::str::from_utf8(bytes)
                    .context("Project-wide writes can only replace UTF-8 text files")?;
            }
            let before = source_bytes.as_deref().map(digest);
            if let Some(prior) = reads.get(&key) {
                ensure!(
                    &before == prior,
                    "Project file changed after the worker read it"
                );
            }
            baselines.insert(name.to_owned(), before);
            // Persist the baseline before writing. If the worker is interrupted between the two
            // operations, an entry with no workspace file is ignored by artifact capture.
            atomic_write(
                &self.project_baseline_path()?,
                &serde_json::to_vec(&baselines)?,
            )?;
        }
        let target = checked_path(&self.workspace, name, true)?;
        atomic_write(&target, text.as_bytes())
    }

    fn project_reads(&self) -> Result<MutexGuard<'_, BTreeMap<String, Option<String>>>> {
        self.project_reads
            .lock()
            .map_err(|_| anyhow::anyhow!("Project file tracking is unavailable"))
    }

    fn project_baseline_path(&self) -> Result<PathBuf> {
        Ok(self
            .workspace
            .parent()
            .context("Workspace has no attempt directory")?
            .join("project-baselines.json"))
    }

    fn project_baselines(&self) -> Result<BTreeMap<String, Option<String>>> {
        match read_optional(&self.project_baseline_path()?)? {
            Some(bytes) => Ok(serde_json::from_slice(&bytes)?),
            None => Ok(BTreeMap::new()),
        }
    }
}

fn path_key(name: &str) -> String {
    if cfg!(windows) {
        name.to_lowercase()
    } else {
        name.to_owned()
    }
}

fn directory_path(root: &Path, name: &str) -> Result<PathBuf> {
    if name == "." {
        check_root(root)?;
        Ok(root.to_owned())
    } else {
        checked_path(root, name, false)
    }
}

fn collect_names(directory: &Path, names: &mut BTreeSet<String>) -> Result<bool> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    for entry in entries.take(256) {
        let entry = entry?;
        let Some(name) = entry.file_name().into_string().ok() else {
            continue;
        };
        if valid_scoped_path(&name) && !redirected(&fs::symlink_metadata(entry.path())?) {
            names.insert(name);
        }
    }
    Ok(true)
}

fn write_file(root: &Path, scope: &[String], name: &str, text: &str) -> Result<()> {
    if !scope.iter().any(|allowed| allowed == name) {
        bail!("Write denied: the path is outside the exact task write scope");
    }
    ensure!(
        text.len() <= MAX_FILE_BYTES,
        "Write denied: the file exceeds 1 MiB"
    );
    let target = checked_path(root, name, true)?;
    atomic_write(&target, text.as_bytes())
}

fn atomic_write(target: &Path, bytes: &[u8]) -> Result<()> {
    let parent = target
        .parent()
        .context("The scoped file has no parent directory")?;
    let staging = parent.join(format!(".runspur-write-{}", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&staging, target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(staging);
    }
    Ok(result?)
}

fn checked_path(root: &Path, name: &str, create_parent: bool) -> Result<PathBuf> {
    ensure!(
        valid_scoped_path(name),
        "Invalid project-relative file path"
    );
    check_root(root)?;
    let mut target = root.to_owned();
    let mut parts = name.split('/').peekable();
    while let Some(part) = parts.next() {
        target.push(part);
        let parent = parts.peek().is_some();
        match fs::symlink_metadata(&target) {
            Ok(info) => {
                ensure!(
                    !redirected(&info),
                    "Scoped path contains a redirected component"
                );
                ensure!(
                    !parent || info.is_dir(),
                    "Scoped path parent is not a directory"
                );
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if parent && create_parent {
                    match fs::create_dir(&target) {
                        Ok(()) => {}
                        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                            let info = fs::symlink_metadata(&target)?;
                            ensure!(
                                info.is_dir() && !redirected(&info),
                                "Scoped parent is redirected or is not a directory"
                            );
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(target)
}

fn check_root(root: &Path) -> Result<()> {
    let info = fs::symlink_metadata(root)?;
    ensure!(
        info.is_dir() && !redirected(&info),
        "Scoped root is redirected"
    );
    Ok(())
}

fn redirected(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x0400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        file.metadata()?.is_file(),
        "Scoped path must identify a regular file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_FILE_BYTES, "Scoped file exceeds 1 MiB");
    Ok(Some(bytes))
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn an_empty_exact_scope_cannot_authorize_project_writes() {
        let root = tempfile::tempdir().unwrap();
        let ticket = root.path().join("file-broker.json");
        let mut config = serde_json::json!({
            "project_root": root.path(),
            "workspace": root.path().join("workspace"),
        });
        fs::write(&ticket, serde_json::to_vec(&config).unwrap()).unwrap();
        assert!(FileBroker::from_ticket(&ticket).is_err());
        for scope in [
            serde_json::json!([]),
            serde_json::json!({"mode":"exact", "paths":[]}),
        ] {
            config["scope"] = scope;
            fs::write(&ticket, serde_json::to_vec(&config).unwrap()).unwrap();
            assert!(FileBroker::from_ticket(&ticket).is_err());
        }
    }

    #[test]
    fn project_write_captures_only_changed_overlay_files() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let attempt = root.path().join("attempt");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&attempt).unwrap();
        fs::write(project.join("existing.txt"), "original").unwrap();
        fs::write(project.join("unchanged.txt"), "same").unwrap();
        fs::write(project.join("concurrent.txt"), "observed").unwrap();
        let protected = root.path().join("protected.txt");
        fs::write(&protected, "protected").unwrap();

        let workspace = ScopedWorkspace::prepare(&project, &attempt, None).unwrap();
        let broker = FileBroker::from_ticket(&ScopedWorkspace::ticket(&attempt)).unwrap();
        assert_eq!(broker.read_file("existing.txt").unwrap(), "original");
        broker.write_file("existing.txt", "modified").unwrap();
        broker.write_file("src/new.txt", "created").unwrap();
        broker.write_file("src/new.txt", "updated").unwrap();
        broker.write_file("unchanged.txt", "same").unwrap();
        assert_eq!(broker.read_file("concurrent.txt").unwrap(), "observed");
        fs::write(project.join("concurrent.txt"), "new original").unwrap();
        assert!(broker.write_file("concurrent.txt", "stale change").is_err());
        assert!(broker.write_file("../protected.txt", "changed").is_err());
        assert!(broker.write_file("existing.txt:stream", "changed").is_err());
        assert_eq!(broker.read_file("src/new.txt").unwrap(), "updated");
        assert_eq!(broker.list_directory("src").unwrap(), ["new.txt"]);

        assert_eq!(
            fs::read_to_string(project.join("existing.txt")).unwrap(),
            "original"
        );
        assert_eq!(fs::read_to_string(protected).unwrap(), "protected");
        assert!(!project.join("src").exists());
        let artifact = workspace.capture_changes(&attempt, "task/attempt").unwrap();
        assert_eq!(artifact.as_deref(), Some("task/attempt/changes.json"));
        let changes: serde_json::Value =
            serde_json::from_reader(File::open(attempt.join("changes.json")).unwrap()).unwrap();
        assert_eq!(changes.as_array().unwrap().len(), 2);
        assert_eq!(changes[0]["path"], "existing.txt");
        assert_eq!(changes[0]["before_sha256"], digest(b"original"));
        assert_eq!(changes[1]["path"], "src/new.txt");
        assert!(changes[1]["before_sha256"].is_null());
        assert_eq!(changes[1]["after_sha256"], digest(b"updated"));
    }

    #[test]
    fn writes_only_the_exact_authorized_file() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let outside = root.path().join("protected.txt");
        fs::write(&outside, "unchanged").unwrap();
        let scope = vec!["src/result.txt".into()];

        assert!(write_file(&workspace, &scope, "../protected.txt", "changed").is_err());
        assert!(write_file(&workspace, &scope, "src/unlisted.txt", "changed").is_err());
        assert!(write_file(&workspace, &scope, "src/result.txt:stream", "changed").is_err());
        write_file(&workspace, &scope, "src/result.txt", "written").unwrap();

        assert_eq!(
            fs::read_to_string(workspace.join("src/result.txt")).unwrap(),
            "written"
        );
        assert_eq!(fs::read_to_string(outside).unwrap(), "unchanged");
        assert!(!workspace.join("src/unlisted.txt").exists());
    }
}
