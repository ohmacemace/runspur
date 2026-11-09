use std::{
    collections::BTreeSet,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use scheduler_core::*;
use scheduler_platform::{DataPaths, LocalClient};
use serde_json::{Value, json};

mod bridge;
mod claude;
mod codex;
mod skill;

pub fn data_root(value: Option<PathBuf>) -> Result<PathBuf> {
    value.map_or_else(DataPaths::default_root, Ok)
}

pub async fn read_project(root: &Path) -> Result<(PathBuf, ProjectConfig)> {
    let root = tokio::fs::canonicalize(root).await?;
    for directory in root.ancestors() {
        let manifest = directory.join(PROJECT_FILE);
        if manifest.is_file() {
            let bytes = tokio::fs::read(manifest).await?;
            if bytes.len() > MAX_FRAME_BYTES {
                bail!("Project file exceeds 1 MiB");
            }
            let config: ProjectConfig = serde_json::from_slice(&bytes)?;
            validate_project(&config)?;
            return Ok((directory.into(), config));
        }
    }
    bail!("No .agent-scheduler/project.json found; initialize this project first")
}

pub async fn read_project_at_root(root: &Path) -> Result<Option<ProjectConfig>> {
    let root = project_directory(root).await?;
    Ok(read_project_manifest(&root)
        .await?
        .map(|(config, _)| config))
}

async fn project_directory(root: &Path) -> Result<PathBuf> {
    let root = tokio::fs::canonicalize(root)
        .await
        .context("Project directory does not exist")?;
    if !root.is_dir() {
        bail!("Project root is not a directory");
    }
    Ok(root)
}

async fn read_project_manifest(root: &Path) -> Result<Option<(ProjectConfig, Vec<u8>)>> {
    let Some(bytes) = read_optional_file(&root.join(PROJECT_FILE)).await? else {
        return Ok(None);
    };
    if bytes.len() > MAX_FRAME_BYTES {
        bail!("Project file exceeds 1 MiB");
    }
    let config = serde_json::from_slice(&bytes).context("Cannot parse the project manifest")?;
    validate_project(&config)?;
    Ok(Some((config, bytes)))
}

pub async fn seed_profiles(client: &LocalClient) -> Result<Vec<AgentProfile>> {
    let existing: Vec<AgentProfile> =
        serde_json::from_value(client.call(Operation::ListProfiles).await?)?;
    let mut defaults = vec![AgentProfile {
        profile_id: "codex-desktop".into(),
        kind: AgentKind::CodexDesktop,
        program: None,
        model: None,
        reasoning_effort: None,
        environment_keys: BTreeSet::new(),
    }];
    if let Some(home) = std::env::var_os("USERPROFILE").map(PathBuf::from) {
        let claude = home.join(".local/bin/claude.exe");
        if claude.is_file() {
            defaults.push(AgentProfile {
                profile_id: "claude-cli".into(),
                kind: AgentKind::ClaudePrint,
                program: Some(claude),
                model: None,
                reasoning_effort: None,
                environment_keys: [
                    "ANTHROPIC_API_KEY".into(),
                    "ANTHROPIC_AUTH_TOKEN".into(),
                    "ANTHROPIC_BASE_URL".into(),
                ]
                .into(),
            });
        }
    }
    #[cfg(windows)]
    if let Some(program) = find_codex_worker_program() {
        defaults.push(AgentProfile {
            profile_id: "codex-cli".into(),
            kind: AgentKind::CodexExec,
            program: Some(program),
            model: None,
            reasoning_effort: None,
            environment_keys: [
                "OPENAI_API_KEY".into(),
                "CODEX_API_KEY".into(),
                "CODEX_ACCESS_TOKEN".into(),
            ]
            .into(),
        });
    }
    for default in defaults {
        if let Some(profile) = profile_to_seed(&existing, default) {
            client.call(Operation::PutProfile(profile)).await?;
        }
    }
    let mut profiles: Vec<AgentProfile> =
        serde_json::from_value(client.call(Operation::ListProfiles).await?)?;
    profiles.retain(|profile| profile.kind != AgentKind::Probe);
    Ok(profiles)
}

#[cfg(windows)]
fn find_codex_worker_program() -> Option<PathBuf> {
    let architecture = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    let triple = if cfg!(target_arch = "aarch64") {
        "aarch64-pc-windows-msvc"
    } else {
        "x86_64-pc-windows-msvc"
    };
    if let Some(app_data) = std::env::var_os("APPDATA") {
        let npm = PathBuf::from(app_data).join("npm/node_modules/@openai");
        let relative = format!("codex-win32-{architecture}/vendor/{triple}/bin/codex.exe");
        for base in [npm.join("codex/node_modules/@openai"), npm] {
            let program = base.join(&relative);
            if program.is_file() {
                return Some(program);
            }
        }
    }
    let local = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    std::fs::read_dir(local.join("OpenAI/Codex/bin"))
        .ok()?
        .flatten()
        .map(|entry| entry.path().join("codex.exe"))
        .filter(|path| path.is_file())
        .max_by_key(|path| {
            path.metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
        })
}

fn profile_to_seed(existing: &[AgentProfile], default: AgentProfile) -> Option<AgentProfile> {
    let Some(previous) = existing
        .iter()
        .find(|profile| profile.profile_id == default.profile_id)
    else {
        return Some(default);
    };
    // Reserved workers may still point to an uninstalled or replaced package.
    let program_missing = previous
        .program
        .as_ref()
        .is_none_or(|path| path.is_absolute() && matches!(path.try_exists(), Ok(false)));
    if !matches!(default.profile_id.as_str(), "codex-cli" | "claude-cli")
        || previous.kind != default.kind
        || !program_missing
    {
        return None;
    }
    let mut repaired = previous.clone();
    repaired.program = default.program;
    Some(repaired)
}

pub async fn initialize_project(
    client: &LocalClient,
    root: &Path,
    name: Option<String>,
    main: Option<String>,
    workers: Option<Vec<String>>,
    permission: ExecutionMode,
) -> Result<ProjectRecord> {
    // Keep existing manifests and profile registrations while presenting the product name.
    let main = main.map(|profile| {
        if profile == "codex" {
            "codex-desktop".into()
        } else {
            profile
        }
    });
    let root = project_directory(root).await?;
    let manifest = root.join(PROJECT_FILE);
    let config = if manifest.exists() {
        let bytes = tokio::fs::read(&manifest).await?;
        let config: ProjectConfig = serde_json::from_slice(&bytes)?;
        if name.as_ref().is_some_and(|v| v != &config.name)
            || main
                .as_ref()
                .is_some_and(|v| v != &config.main_agent_profile_id)
            || workers.as_ref().is_some_and(|v| {
                v.iter().cloned().collect::<BTreeSet<_>>() != config.allowed_worker_profile_ids
            })
            || !config.limits.permits_execution(permission)
        {
            bail!(
                "Existing project configuration differs; edit the versioned manifest and re-import it explicitly"
            );
        }
        config
    } else {
        let config = ProjectConfig {
            schema_version: SCHEMA_VERSION,
            project_id: new_id(),
            name: name.unwrap_or_else(|| {
                root.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into()
            }),
            main_agent_profile_id: main.unwrap_or_else(|| "codex-desktop".into()),
            allowed_worker_profile_ids: workers.unwrap_or_default().into_iter().collect(),
            integrations: IntegrationConfig::default(),
            limits: DispatchLimits {
                allow_workspace_write: permission == ExecutionMode::WorkspaceWrite,
                allow_project_write: permission == ExecutionMode::ProjectWrite,
                ..Default::default()
            },
        };
        validate_project(&config)?;
        write_new_file(
            &manifest,
            format!("{}\n", serde_json::to_string_pretty(&config)?).as_bytes(),
        )
        .await?;
        config
    };
    validate_project(&config)?;
    // Same project ID can be reused from a checkout/worktree without moving its primary registry root.
    let existing: Vec<ProjectRecord> =
        serde_json::from_value(client.call(Operation::ListProjects).await?)?;
    let primary = existing
        .into_iter()
        .find(|p| p.config.project_id == config.project_id)
        .map_or(root, |p| p.root);
    Ok(serde_json::from_value(
        client
            .call(Operation::PutProject(ProjectRecord {
                root: primary,
                revision: String::new(),
                config,
            }))
            .await?,
    )?)
}

pub struct ProjectRoles {
    pub name: String,
    pub main_agent_profile_id: String,
    pub worker_profile_ids: BTreeSet<String>,
    pub allow_workspace_write: bool,
    pub allow_project_write: bool,
}

fn native_agent_program(profile: &AgentProfile) -> bool {
    profile.program.as_ref().is_some_and(|path| {
        path.is_absolute()
            && path.is_file()
            && (!cfg!(windows)
                || path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("exe")))
    })
}

pub fn desktop_parent_profile(profile: &AgentProfile) -> bool {
    profile.kind == AgentKind::CodexDesktop
        || (profile.kind == AgentKind::ClaudePrint && native_agent_program(profile))
}

pub fn desktop_worker_profile(profile: &AgentProfile) -> bool {
    matches!(profile.kind, AgentKind::CodexExec | AgentKind::ClaudePrint)
        && native_agent_program(profile)
}

#[derive(serde::Serialize)]
pub struct ProjectSetupPreview {
    pub manifest_path: PathBuf,
    pub skill_path: PathBuf,
    pub skill_content: String,
    pub mcp_path: PathBuf,
    pub mcp_content: String,
    pub hook_path: Option<PathBuf>,
    pub hook_content: Option<String>,
    pub cleanup: Vec<ProjectCleanupPreview>,
}

#[derive(serde::Serialize)]
pub struct ProjectCleanupPreview {
    path: PathBuf,
    entry: &'static str,
}

pub struct ProjectSetup {
    root: PathBuf,
    record: ProjectRecord,
    registered_revision: Option<String>,
    manifest: PathBuf,
    original: Option<Vec<u8>>,
    adapter: ProjectAdapter,
    previous_adapter: Option<ProjectAdapter>,
    integration: ProjectIntegration,
    cleanup: Vec<CleanupEdit>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProjectAdapter {
    Codex,
    Claude,
}

impl ProjectAdapter {
    fn for_kind(kind: &AgentKind) -> Result<Self> {
        match kind {
            AgentKind::CodexDesktop => Ok(Self::Codex),
            AgentKind::ClaudePrint => Ok(Self::Claude),
            _ => bail!("No project integration adapter for main agent {kind:?}"),
        }
    }

    fn for_profile(id: &str, profiles: &[AgentProfile]) -> Result<Self> {
        match profiles.iter().find(|profile| profile.profile_id == id) {
            Some(profile) => Self::for_kind(&profile.kind),
            None if id == "codex-desktop" => Ok(Self::Codex),
            None if id == "claude-cli" => Ok(Self::Claude),
            None => bail!("Main agent profile unavailable for project integration: {id}"),
        }
    }

    fn paths(self) -> &'static [&'static str] {
        match self {
            Self::Codex => &[codex::SKILL_PATH, codex::MCP_PATH],
            Self::Claude => &[claude::SKILL_PATH, claude::MCP_PATH, claude::HOOK_PATH],
        }
    }

    async fn prepare(
        self,
        root: &Path,
        config: &ProjectConfig,
        program: &str,
        data_dir: Option<&Path>,
        profiles: &[AgentProfile],
    ) -> Result<ProjectIntegration> {
        match self {
            Self::Codex => codex::prepare(root, config, program, data_dir, profiles).await,
            Self::Claude => claude::prepare(root, config, program, data_dir, profiles).await,
        }
    }

    async fn cleanup(
        self,
        root: &Path,
        program: &str,
        data_dir: Option<&Path>,
    ) -> Result<Vec<CleanupEdit>> {
        match self {
            Self::Codex => codex::cleanup(root, program, data_dir).await,
            Self::Claude => claude::cleanup(root, program, data_dir).await,
        }
    }
}

pub async fn prepare_project_configuration(
    client: &LocalClient,
    root: &Path,
    configure: impl FnOnce(&ProjectConfig) -> ProjectRoles,
    program: &str,
    data_dir: &Path,
) -> Result<ProjectSetup> {
    let root = project_directory(root).await?;
    let registered: Vec<ProjectRecord> =
        serde_json::from_value(client.call(Operation::ListProjects).await?)?;
    let profiles: Vec<AgentProfile> =
        serde_json::from_value(client.call(Operation::ListProfiles).await?)?;
    let snapshot = read_project_snapshot(&root, &registered).await?;
    let roles = configure(&snapshot.config);
    let adapter = project_adapter(
        &roles.main_agent_profile_id,
        &roles.worker_profile_ids,
        roles.allow_workspace_write || roles.allow_project_write,
        &profiles,
    )
    .await?;
    prepare_project_setup(snapshot, roles, program, data_dir, &profiles, adapter).await
}

async fn project_adapter(
    main: &str,
    workers: &BTreeSet<String>,
    allow_writes: bool,
    profiles: &[AgentProfile],
) -> Result<ProjectAdapter> {
    if workers.is_empty() {
        bail!("Select at least one installed worker Agent from a different provider.");
    }
    let main = profiles
        .iter()
        .find(|profile| profile.profile_id == main && desktop_parent_profile(profile))
        .context("Select an available main agent.")?;
    let adapter = ProjectAdapter::for_kind(&main.kind)?;
    let mut has_writable_worker = false;
    for id in workers {
        let worker = profiles
            .iter()
            .find(|profile| {
                &profile.profile_id == id
                    && !matches!(
                        (adapter, &profile.kind),
                        (ProjectAdapter::Codex, AgentKind::CodexExec)
                            | (ProjectAdapter::Claude, AgentKind::ClaudePrint)
                    )
                    && desktop_worker_profile(profile)
            })
            .with_context(|| format!("Worker profile is unavailable for this project: {id}"))?;
        has_writable_worker |= worker.kind == AgentKind::ClaudePrint;
    }
    if allow_writes && !has_writable_worker {
        bail!("The selected workers support read-only tasks. Select read-only permission.");
    }
    if adapter == ProjectAdapter::Claude {
        let program = main
            .program
            .as_deref()
            .context("Claude Code executable is missing")?;
        scheduler_runtime::check_claude_version(program, false).await?;
    }
    Ok(adapter)
}

impl ProjectSetup {
    pub fn config(&self) -> &ProjectConfig {
        &self.record.config
    }

    pub fn preview(&self) -> ProjectSetupPreview {
        let hook = self.integration.hook.as_ref();
        let mut cleanup: Vec<_> = self
            .cleanup
            .iter()
            .flat_map(|edit| {
                edit.entries.iter().map(|entry| ProjectCleanupPreview {
                    path: edit.file.path.clone(),
                    entry,
                })
            })
            .collect();
        if let Some(hook) = hook.filter(|hook| hook.content.is_none()) {
            cleanup.push(ProjectCleanupPreview {
                path: hook.file.path.clone(),
                entry: "claude_origin_hook",
            });
        }
        ProjectSetupPreview {
            manifest_path: self.manifest.clone(),
            skill_path: self.integration.skill_path.clone(),
            skill_content: self.integration.skill_content.clone(),
            mcp_path: self.integration.config_path.clone(),
            mcp_content: self.integration.runspur_entry.clone(),
            hook_path: hook
                .filter(|hook| hook.content.is_some())
                .map(|hook| hook.file.path.clone()),
            hook_content: hook.and_then(|hook| hook.content.clone()),
            cleanup,
        }
    }

    async fn validate_snapshot(&self) -> Result<()> {
        validate_project_targets(&self.root, Some(self.adapter)).await?;
        if let Some(previous) = self.previous_adapter {
            validate_project_targets(&self.root, Some(previous)).await?;
        }
        if self.original != read_optional_file(&self.manifest).await? {
            bail!("Project manifest changed after preview. Preview it again before confirming.");
        }
        self.integration.validate_snapshot().await?;
        for edit in &self.cleanup {
            edit.file.validate_snapshot().await?;
        }
        Ok(())
    }

    pub async fn apply(self, client: &LocalClient) -> Result<ProjectRecord> {
        let registered: Vec<ProjectRecord> =
            serde_json::from_value(client.call(Operation::ListProjects).await?)?;
        let project_id = &self.record.config.project_id;
        let current = registered
            .iter()
            .find(|record| &record.config.project_id == project_id)
            .map(|record| (&record.root, &record.revision));
        let expected = self
            .registered_revision
            .as_ref()
            .map(|revision| (&self.record.root, revision));
        if current != expected
            || registered
                .iter()
                .any(|record| record.root == self.root && &record.config.project_id != project_id)
        {
            bail!(
                "Project registration changed after preview. Preview it again before confirming."
            );
        }
        let profiles: Vec<AgentProfile> =
            serde_json::from_value(client.call(Operation::ListProfiles).await?)?;
        let adapter = project_adapter(
            &self.record.config.main_agent_profile_id,
            &self.record.config.allowed_worker_profile_ids,
            self.record.config.limits.allow_workspace_write
                || self.record.config.limits.allow_project_write,
            &profiles,
        )
        .await?;
        if adapter != self.adapter
            || skill::render_skill(
                &self.record.config,
                &profiles,
                adapter,
                Some(&client.paths.root),
            )? != self.integration.skill_content
        {
            bail!("Agent profiles changed after preview. Preview it again before confirming.");
        }
        self.validate_snapshot().await?;

        let mut files = self.integration.edits(&self.record.config);
        files.extend(self.cleanup.into_iter().map(|edit| edit.file));
        files.push(ConfigEdit {
            path: self.manifest,
            before: self.original,
            after: Some(
                format!("{}\n", serde_json::to_string_pretty(&self.record.config)?).into_bytes(),
            ),
        });
        let new_revision = fingerprint(&self.record.config)?;
        apply_files(&files).await?;
        match client
            .call(Operation::PutProject(self.record.clone()))
            .await
        {
            Ok(value) => Ok(serde_json::from_value(value)?),
            Err(error) => {
                // A local transport failure may occur after the daemon committed the change.
                if error.downcast_ref::<SchedulerError>().is_none() {
                    let records: Vec<ProjectRecord> = serde_json::from_value(
                        client.call(Operation::ListProjects).await
                            .context("Could not confirm project registration after saving. Check the scheduler before retrying.")?
                    )?;
                    if let Some(saved) = records.into_iter().find(|record| {
                        record.config.project_id == self.record.config.project_id
                            && record.revision == new_revision
                    }) {
                        return Ok(saved);
                    }
                }
                rollback_files(&files)
                    .await
                    .context("Could not restore the previous project configuration")?;
                Err(error)
                    .context("Project update rejected. The previous configuration was restored.")
            }
        }
    }
}

async fn validate_project_targets(root: &Path, adapter: Option<ProjectAdapter>) -> Result<()> {
    let metadata = tokio::fs::symlink_metadata(root).await?;
    if !metadata.is_dir() || is_redirected(&metadata) {
        bail!("Project root is not a regular directory");
    }
    let integration_paths = adapter.into_iter().flat_map(ProjectAdapter::paths);
    for relative in std::iter::once(PROJECT_FILE).chain(integration_paths.copied()) {
        let target = root.join(relative);
        for path in target.ancestors().take_while(|path| *path != root) {
            match tokio::fs::symlink_metadata(path).await {
                Ok(metadata) if is_redirected(&metadata) => {
                    bail!("Project integration path is redirected: {}", path.display());
                }
                Err(error) if error.kind() != ErrorKind::NotFound => return Err(error.into()),
                _ => {}
            }
        }
    }
    Ok(())
}

fn is_redirected(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    metadata.file_type().is_symlink()
}

struct ProjectSnapshot<'a> {
    root: &'a Path,
    config: ProjectConfig,
    original: Option<Vec<u8>>,
    registration: Option<&'a ProjectRecord>,
}

async fn read_project_snapshot<'a>(
    root: &'a Path,
    registered: &'a [ProjectRecord],
) -> Result<ProjectSnapshot<'a>> {
    validate_project_targets(root, None).await?;
    let existing = read_project_manifest(root).await?;
    let registered_at_root = registered.iter().find(|record| record.root == root);
    let (config, original) = match existing {
        Some((config, bytes)) => {
            if registered_at_root
                .is_some_and(|record| record.config.project_id != config.project_id)
            {
                bail!("This directory is registered with a different project ID.");
            }
            (config, Some(bytes))
        }
        None => {
            let config = registered_at_root
                .map(|record| record.config.clone())
                .unwrap_or_else(|| ProjectConfig {
                    schema_version: SCHEMA_VERSION,
                    project_id: new_id(),
                    name: String::new(),
                    main_agent_profile_id: String::new(),
                    allowed_worker_profile_ids: BTreeSet::new(),
                    integrations: IntegrationConfig::default(),
                    limits: DispatchLimits::default(),
                });
            (config, None)
        }
    };
    let registration = registered
        .iter()
        .find(|record| record.config.project_id == config.project_id);
    Ok(ProjectSnapshot {
        root,
        config,
        original,
        registration,
    })
}

async fn prepare_project_setup(
    snapshot: ProjectSnapshot<'_>,
    roles: ProjectRoles,
    program: &str,
    data_dir: &Path,
    profiles: &[AgentProfile],
    adapter: ProjectAdapter,
) -> Result<ProjectSetup> {
    let ProjectSnapshot {
        root,
        mut config,
        original,
        registration,
    } = snapshot;
    validate_project_targets(root, Some(adapter)).await?;
    let old_main = registration.map_or(&config.main_agent_profile_id, |record| {
        &record.config.main_agent_profile_id
    });
    if old_main != &roles.main_agent_profile_id
        && registration.is_some_and(|record| record.root != root)
    {
        bail!("Change the main agent from the project's registered directory.");
    }
    let previous_adapter = if old_main.is_empty() || old_main == &roles.main_agent_profile_id {
        None
    } else {
        Some(ProjectAdapter::for_profile(old_main, profiles)?).filter(|old| *old != adapter)
    };
    let cleanup = if let Some(previous) = previous_adapter {
        validate_project_targets(root, Some(previous)).await?;
        previous.cleanup(root, program, Some(data_dir)).await?
    } else {
        Vec::new()
    };
    config.name = roles.name.trim().to_owned();
    config.main_agent_profile_id = roles.main_agent_profile_id;
    config.allowed_worker_profile_ids = roles.worker_profile_ids;
    config.limits.allow_workspace_write = roles.allow_workspace_write && !roles.allow_project_write;
    config.limits.allow_project_write = roles.allow_project_write;
    validate_project(&config)?;
    let primary = registration.map_or_else(|| root.to_path_buf(), |record| record.root.clone());
    let integration = adapter
        .prepare(root, &config, program, Some(data_dir), profiles)
        .await?;
    Ok(ProjectSetup {
        root: root.to_path_buf(),
        record: ProjectRecord {
            root: primary,
            revision: registration
                .map(|record| record.revision.clone())
                .unwrap_or_default(),
            config,
        },
        registered_revision: registration.map(|record| record.revision.clone()),
        manifest: root.join(PROJECT_FILE),
        original,
        adapter,
        previous_adapter,
        integration,
        cleanup,
    })
}

pub async fn install_project(
    root: &Path,
    config: &ProjectConfig,
    program: &str,
    data_dir: Option<&Path>,
    profiles: &[AgentProfile],
) -> Result<Value> {
    let adapter = ProjectAdapter::for_profile(&config.main_agent_profile_id, profiles)?;
    validate_project_targets(root, Some(adapter)).await?;
    adapter
        .prepare(root, config, program, data_dir, profiles)
        .await?
        .install(config)
        .await
}

struct ProjectIntegration {
    skill_path: PathBuf,
    skill_content: String,
    config_path: PathBuf,
    skill_old: Option<Vec<u8>>,
    previous: Option<Vec<u8>>,
    document: String,
    runspur_entry: String,
    hook: Option<HookEdit>,
}

#[derive(Clone)]
struct ConfigEdit {
    path: PathBuf,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
}

struct HookEdit {
    file: ConfigEdit,
    content: Option<String>,
}

struct CleanupEdit {
    file: ConfigEdit,
    entries: Vec<&'static str>,
}

fn mcp_arguments(data_dir: Option<&Path>) -> Vec<String> {
    let mut args = vec!["--project-root".into(), ".".into()];
    if let Some(path) = data_dir {
        args.extend(["--data-dir".into(), path.to_string_lossy().into()]);
    }
    args
}

impl ConfigEdit {
    async fn validate_snapshot(&self) -> Result<()> {
        if self.before != read_optional_file(&self.path).await? {
            bail!(
                "Configuration changed after preview: {}",
                self.path.display()
            );
        }
        Ok(())
    }

    async fn write(&self, expected: Option<&[u8]>, value: Option<&[u8]>) -> Result<()> {
        if expected == value {
            return Ok(());
        }
        if let Some(bytes) = value {
            replace_file(&self.path, bytes, expected).await?;
        } else {
            if read_optional_file(&self.path).await?.as_deref() != expected {
                bail!(
                    "Configuration changed before cleanup: {}",
                    self.path.display()
                );
            }
            tokio::fs::remove_file(&self.path).await?;
        }
        Ok(())
    }
}

async fn apply_files(files: &[ConfigEdit]) -> Result<()> {
    for file in files {
        file.validate_snapshot().await?;
    }
    for (index, file) in files.iter().enumerate() {
        if let Err(error) = file
            .write(file.before.as_deref(), file.after.as_deref())
            .await
        {
            rollback_files(&files[..index]).await.with_context(|| {
                format!("Could not restore previous configuration after {error}")
            })?;
            return Err(error).with_context(|| format!("Cannot update {}", file.path.display()));
        }
    }
    Ok(())
}

async fn rollback_files(files: &[ConfigEdit]) -> Result<()> {
    let mut failure = None;
    for file in files.iter().rev() {
        if let Err(error) = file
            .write(file.after.as_deref(), file.before.as_deref())
            .await
        {
            failure = Some(error.context(format!("Cannot restore {}", file.path.display())));
        }
    }
    failure.map_or(Ok(()), Err)
}

impl ProjectIntegration {
    async fn validate_snapshot(&self) -> Result<()> {
        if self.skill_old != read_optional_file(&self.skill_path).await? {
            bail!("Skill changed during integration update");
        }
        if self.previous != read_optional_file(&self.config_path).await? {
            bail!("MCP configuration changed during integration update");
        }
        if let Some(hook) = &self.hook {
            hook.file.validate_snapshot().await?;
        }
        Ok(())
    }

    fn edits(&self, config: &ProjectConfig) -> Vec<ConfigEdit> {
        let mut edits = vec![
            ConfigEdit {
                path: self.skill_path.clone(),
                before: self.skill_old.clone(),
                after: config
                    .integrations
                    .skill
                    .then(|| self.skill_content.as_bytes().to_vec()),
            },
            ConfigEdit {
                path: self.config_path.clone(),
                before: self.previous.clone(),
                after: Some(self.document.as_bytes().to_vec()),
            },
        ];
        if let Some(hook) = &self.hook {
            edits.push(hook.file.clone());
        }
        edits
    }

    async fn install(self, config: &ProjectConfig) -> Result<Value> {
        apply_files(&self.edits(config)).await?;
        Ok(
            json!({"project_id":config.project_id,"skill_path":self.skill_path,"mcp_config_path":self.config_path,
            "session_id_in_config":false,"state":"saved_requires_host_reload"}),
        )
    }
}

async fn read_optional_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Cannot read {}", path.display())),
    }
}

async fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    tokio::fs::create_dir_all(path.parent().context("Configuration has no parent")?).await?;
    let staging = path
        .parent()
        .context("Configuration has no parent")?
        .join(format!(".runspur-{}.tmp", new_id()));
    let result = async {
        let mut file = tokio::fs::File::create_new(&staging).await?;
        file.write_all(bytes).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::hard_link(&staging, path).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let _ = tokio::fs::remove_file(&staging).await;
    result
}
async fn replace_file(path: &Path, bytes: &[u8], expected: Option<&[u8]>) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let Some(expected) = expected else {
        return write_new_file(path, bytes).await;
    };
    if read_optional_file(path).await?.as_deref() != Some(expected) {
        bail!("Configuration changed while preparing installation");
    }
    let parent = path.parent().context("Configuration has no parent")?;
    tokio::fs::create_dir_all(parent).await?;
    let staging = parent.join(format!(".runspur-{}.tmp", new_id()));
    let result = async {
        let mut file = tokio::fs::File::create_new(&staging).await?;
        file.write_all(bytes).await?;
        file.sync_all().await?;
        drop(file);
        if read_optional_file(path).await?.as_deref() != Some(expected) {
            bail!("Configuration changed before installation");
        }
        tokio::fs::rename(&staging, path).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&staging).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::codex::{LAST_STATIC_SKILL, MCP_PATH, PREVIOUS_SKILL, SKILL_PATH};
    use super::*;

    fn config() -> ProjectConfig {
        ProjectConfig {
            schema_version: 1,
            project_id: new_id(),
            name: "project".into(),
            main_agent_profile_id: "codex-desktop".into(),
            allowed_worker_profile_ids: ["probe".into()].into(),
            integrations: IntegrationConfig::default(),
            limits: DispatchLimits::default(),
        }
    }

    #[tokio::test]
    async fn configuration_rejects_write_permissions_for_codex_only_workers() {
        let executable = std::env::current_exe().unwrap();
        let profiles = [
            AgentProfile {
                profile_id: "claude-cli".into(),
                kind: AgentKind::ClaudePrint,
                program: Some(executable.clone()),
                model: None,
                reasoning_effort: None,
                environment_keys: Default::default(),
            },
            AgentProfile {
                profile_id: "codex-cli".into(),
                kind: AgentKind::CodexExec,
                program: Some(executable),
                model: None,
                reasoning_effort: None,
                environment_keys: Default::default(),
            },
        ];
        let result =
            project_adapter("claude-cli", &["codex-cli".into()].into(), true, &profiles).await;
        assert!(result.is_err_and(|error| error.to_string().contains("read-only")));
    }

    #[tokio::test]
    async fn claude_main_installs_claude_integration_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut project = config();
        project.main_agent_profile_id = "claude-cli".into();
        let profiles = [AgentProfile {
            profile_id: project.main_agent_profile_id.clone(),
            kind: AgentKind::ClaudePrint,
            program: None,
            model: None,
            reasoning_effort: None,
            environment_keys: BTreeSet::new(),
        }];

        install_project(dir.path(), &project, "scheduler-mcp", None, &profiles)
            .await
            .unwrap();

        let mcp: Value = serde_json::from_slice(
            &tokio::fs::read(dir.path().join(claude::MCP_PATH))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            mcp["mcpServers"]["runspur_scheduler"]["args"],
            json!(["--project-root", ".", "--host", "claude-code"])
        );
        assert!(dir.path().join(claude::SKILL_PATH).is_file());
        assert!(!dir.path().join(MCP_PATH).exists());
        assert!(!dir.path().join(SKILL_PATH).exists());
    }

    #[tokio::test]
    async fn switching_adapters_removes_managed_entries_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let bundle = root.join("desktop bundle");
        tokio::fs::create_dir(&bundle).await.unwrap();
        for binary in [
            "runspur-desktop",
            "schedulerd",
            "schedulerctl",
            "scheduler-mcp",
        ] {
            tokio::fs::write(
                bundle.join(format!("{binary}{}", std::env::consts::EXE_SUFFIX)),
                b"fixture",
            )
            .await
            .unwrap();
        }
        let desktop_bridge = bundle.join(format!("scheduler-mcp{}", std::env::consts::EXE_SUFFIX));
        let desktop_bridge = desktop_bridge.to_str().unwrap();
        let data_dir = DataPaths::default_root().unwrap();
        let mut config = config();
        let original_id = config.project_id.clone();
        let codex_path = root.join(MCP_PATH);
        let claude_path = root.join(claude::MCP_PATH);
        let agent_path = root.join("AGENTS.md");
        write_new_file(
            &codex_path,
            b"# keep me\n[mcp_servers.other]\ncommand = 'codex-other'\n",
        )
        .await
        .unwrap();
        write_new_file(
            &claude_path,
            br#"{"mcpServers":{"other":{"command":"claude-other"}}}"#,
        )
        .await
        .unwrap();
        write_new_file(&agent_path, b"# Keep project instructions\n")
            .await
            .unwrap();
        for (adapter, main, old_skill, old_mcp, old_bridge, old_data, new_bridge) in [
            (
                ProjectAdapter::Claude,
                "claude-cli",
                SKILL_PATH,
                MCP_PATH,
                "scheduler-mcp",
                None,
                desktop_bridge,
            ),
            (
                ProjectAdapter::Codex,
                "codex-desktop",
                claude::SKILL_PATH,
                claude::MCP_PATH,
                desktop_bridge,
                Some(data_dir.as_path()),
                "scheduler-mcp",
            ),
        ] {
            let old_adapter =
                ProjectAdapter::for_profile(&config.main_agent_profile_id, &[]).unwrap();
            old_adapter
                .prepare(root, &config, old_bridge, old_data, &[])
                .await
                .unwrap()
                .install(&config)
                .await
                .unwrap();
            tokio::fs::create_dir_all(root.join(PROJECT_FILE).parent().unwrap())
                .await
                .unwrap();
            tokio::fs::write(
                root.join(PROJECT_FILE),
                serde_json::to_vec(&config).unwrap(),
            )
            .await
            .unwrap();
            let setup = prepare_project_setup(
                read_project_snapshot(root, &[]).await.unwrap(),
                ProjectRoles {
                    name: config.name.clone(),
                    main_agent_profile_id: main.into(),
                    worker_profile_ids: ["probe".into()].into(),
                    allow_workspace_write: false,
                    allow_project_write: false,
                },
                new_bridge,
                &data_dir,
                &[],
                adapter,
            )
            .await
            .unwrap();
            assert_eq!(setup.record.config.project_id, original_id);
            assert!(
                setup.preview().cleanup.iter().any(
                    |item| item.path == root.join(old_mcp) && item.entry == "runspur_scheduler"
                )
            );
            assert!(root.join(old_skill).is_file());
            let mut edits = setup.integration.edits(&setup.record.config);
            edits.extend(setup.cleanup.into_iter().map(|edit| edit.file));
            apply_files(&edits).await.unwrap();
            assert!(!root.join(old_skill).exists());
            assert!(
                !tokio::fs::read_to_string(root.join(old_mcp))
                    .await
                    .unwrap()
                    .contains("runspur_scheduler")
            );
            assert!(
                tokio::fs::read_to_string(&codex_path)
                    .await
                    .unwrap()
                    .contains("codex-other")
            );
            assert!(
                tokio::fs::read_to_string(&claude_path)
                    .await
                    .unwrap()
                    .contains("claude-other")
            );
            assert_eq!(
                tokio::fs::read(&agent_path).await.unwrap(),
                b"# Keep project instructions\n"
            );
            config = setup.record.config;
        }
    }

    #[tokio::test]
    async fn cross_adapter_switch_does_not_claim_an_arbitrary_named_bridge() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let config = config();
        let program = root.join(format!("scheduler-mcp{}", std::env::consts::EXE_SUFFIX));
        tokio::fs::write(&program, b"custom program").await.unwrap();
        install_project(root, &config, program.to_str().unwrap(), Some(root), &[])
            .await
            .unwrap();
        let path = root.join(MCP_PATH);
        let saved = tokio::fs::read(&path).await.unwrap();
        let error = ProjectAdapter::Codex
            .cleanup(root, "scheduler-mcp", Some(root))
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "Existing Codex Runspur MCP entry has user changes"
        );
        assert_eq!(tokio::fs::read(path).await.unwrap(), saved);
    }

    #[tokio::test]
    async fn same_adapter_keeps_the_cli_bridge_and_rejects_a_different_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let config = config();
        install_project(root, &config, "scheduler-mcp", None, &[])
            .await
            .unwrap();
        let path = root.join(MCP_PATH);
        let original = tokio::fs::read_to_string(&path).await.unwrap();
        let desktop = root.join("desktop bridge.exe");
        let desktop = desktop.to_str().unwrap();
        let default = DataPaths::default_root().unwrap();
        install_project(root, &config, desktop, Some(&default), &[])
            .await
            .unwrap();
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), original);

        let other_state = root.join("other state");
        let error = install_project(root, &config, desktop, Some(&other_state), &[])
            .await
            .unwrap_err();
        assert!(error.to_string().starts_with("[runspur_state_conflict]"));
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), original);
    }

    #[test]
    fn profile_seeding_repairs_a_missing_reserved_worker_executable() {
        let dir = tempfile::tempdir().unwrap();
        let bundled = dir.path().join("codex.exe");
        std::fs::write(&bundled, []).unwrap();
        let default = AgentProfile {
            profile_id: "codex-cli".into(),
            kind: AgentKind::CodexExec,
            program: Some(bundled.clone()),
            model: None,
            reasoning_effort: None,
            environment_keys: BTreeSet::new(),
        };
        let old = AgentProfile {
            program: Some(dir.path().join("removed-cli.exe")),
            environment_keys: ["CUSTOM_FLAG".into()].into(),
            ..default.clone()
        };

        let repaired = profile_to_seed(std::slice::from_ref(&old), default.clone()).unwrap();
        assert_eq!(repaired.program, Some(bundled));
        assert_eq!(repaired.environment_keys, old.environment_keys);
        assert!(profile_to_seed(std::slice::from_ref(&repaired), default.clone()).is_none());

        let relative = AgentProfile {
            program: Some("custom-probe.exe".into()),
            ..old.clone()
        };
        assert!(profile_to_seed(&[relative], default.clone()).is_none());
        let custom = AgentProfile {
            kind: AgentKind::ClaudePrint,
            ..old
        };
        assert!(profile_to_seed(&[custom], default).is_none());
    }
    #[tokio::test]
    async fn installation_preserves_other_mcp_settings_and_never_pins_a_session() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(dir.path().join(".codex"))
            .await
            .unwrap();
        let file = dir.path().join(".codex/config.toml");
        tokio::fs::write(
            &file,
            "# user comment\n[mcp_servers.other]\ncommand = \"other-server\"\n",
        )
        .await
        .unwrap();
        install_project(dir.path(), &config(), "scheduler-mcp", None, &[])
            .await
            .unwrap();
        let text = tokio::fs::read_to_string(&file).await.unwrap();
        assert!(text.contains("# user comment"));
        assert!(text.contains("other-server"));
        assert!(text.contains("runspur_scheduler"));
        assert!(!text.contains("--session"));
        install_project(dir.path(), &config(), "scheduler-mcp", None, &[])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn integration_refreshes_skill_for_selected_workers_and_removes_old_startup() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join(MCP_PATH);
        let skill = dir.path().join(SKILL_PATH);
        let agents = dir.path().join("AGENTS.md");
        write_new_file(
            &settings,
            b"developer_instructions = \"\"\"Project-owned instructions.\n\n<!-- runspur:startup -->\nOld Runspur instructions.\n<!-- /runspur:startup -->\"\"\"\n",
        )
        .await
        .unwrap();
        write_new_file(&skill, LAST_STATIC_SKILL.as_bytes())
            .await
            .unwrap();
        write_new_file(&agents, b"# Project-owned rules\n")
            .await
            .unwrap();
        let mut config = config();
        config.allowed_worker_profile_ids = ["claude-cli".into()].into();
        let description = |skill: &str| {
            skill
                .lines()
                .find(|line| line.starts_with("description:"))
                .unwrap()
                .to_owned()
        };

        install_project(dir.path(), &config, "scheduler-mcp", None, &[])
            .await
            .unwrap();
        let generated = tokio::fs::read_to_string(&skill).await.unwrap();
        assert!(description(&generated).contains("Claude Code (CC)"));
        assert!(description(&generated).contains("built-in subagents"));
        assert!(!description(&generated).contains("Probe"));
        assert!(
            generated.contains("consider both built-in subagents and permitted Runspur workers")
        );
        assert!(generated.contains("use both for distinct work"));
        assert!(generated.contains("You are accountable for the final deliverable"));
        assert!(generated.contains("Verify material claims"));
        assert!(generated.contains("Omit `timeout_seconds` by default"));
        assert!(generated.contains("# Worker routing notes"));
        assert!(generated.contains("claude-cli.md"));
        assert!(generated.contains("Only update a note if the evidence changes future routing"));
        assert!(generated.contains("A timeout alone is inconclusive"));
        install_project(dir.path(), &config, "scheduler-mcp", None, &[])
            .await
            .unwrap();
        assert_eq!(tokio::fs::read_to_string(&skill).await.unwrap(), generated);

        config.allowed_worker_profile_ids = ["probe".into()].into();
        install_project(dir.path(), &config, "scheduler-mcp", None, &[])
            .await
            .unwrap();
        let generated = tokio::fs::read_to_string(&skill).await.unwrap();
        assert!(!description(&generated).contains("Claude Code (CC)"));
        assert!(description(&generated).contains("Probe"));
        assert!(!generated.contains("# Worker routing notes"));

        let text = tokio::fs::read_to_string(&settings).await.unwrap();
        assert_eq!(
            text.parse::<toml_edit::DocumentMut>().unwrap()["developer_instructions"].as_str(),
            Some("Project-owned instructions.")
        );
        assert_eq!(
            tokio::fs::read(&agents).await.unwrap(),
            b"# Project-owned rules\n"
        );

        let edited = generated.replace("Probe", "My custom Probe");
        tokio::fs::write(&skill, &edited).await.unwrap();
        assert!(
            install_project(dir.path(), &config, "scheduler-mcp", None, &[])
                .await
                .is_err()
        );
        assert_eq!(tokio::fs::read_to_string(&skill).await.unwrap(), edited);
    }
    #[tokio::test]
    async fn conflicting_integration_does_not_overwrite_user_work() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(dir.path().join(".codex"))
            .await
            .unwrap();
        let file = dir.path().join(".codex/config.toml");
        let original = "[mcp_servers.runspur_scheduler]\ncommand = \"user-owned\"\nargs = []\n";
        tokio::fs::write(&file, original).await.unwrap();
        assert!(
            install_project(dir.path(), &config(), "scheduler-mcp", None, &[])
                .await
                .is_err()
        );
        assert_eq!(tokio::fs::read_to_string(file).await.unwrap(), original);
        assert!(
            !dir.path()
                .join(".agents/skills/agent-scheduler/SKILL.md")
                .exists()
        );
    }

    #[tokio::test]
    async fn desktop_preview_upgrades_the_previous_generated_skill_only_on_install() {
        let dir = tempfile::tempdir().unwrap();
        let skill_path = dir.path().join(SKILL_PATH);
        write_new_file(&skill_path, PREVIOUS_SKILL.as_bytes())
            .await
            .unwrap();
        let roles = ProjectRoles {
            name: "Preview".into(),
            main_agent_profile_id: "codex-desktop".into(),
            worker_profile_ids: ["probe".into()].into(),
            allow_workspace_write: false,
            allow_project_write: false,
        };
        let setup = prepare_project_setup(
            read_project_snapshot(dir.path(), &[]).await.unwrap(),
            roles,
            "scheduler-mcp",
            dir.path(),
            &[],
            ProjectAdapter::Codex,
        )
        .await
        .unwrap();

        let expected = setup.preview().skill_content;
        assert!(expected.contains("Probe"));
        assert_eq!(
            tokio::fs::read(&skill_path).await.unwrap(),
            PREVIOUS_SKILL.as_bytes()
        );
        setup
            .integration
            .install(&setup.record.config)
            .await
            .unwrap();
        let installed = tokio::fs::read(&skill_path).await.unwrap();
        assert!(
            installed != PREVIOUS_SKILL.as_bytes(),
            "installation should upgrade the previous generated skill"
        );
        assert_eq!(installed, expected.as_bytes());
    }

    #[tokio::test]
    async fn installation_preserves_edits_to_the_previous_generated_skill() {
        let dir = tempfile::tempdir().unwrap();
        let skill_path = dir.path().join(SKILL_PATH);
        let edited = format!("{PREVIOUS_SKILL}\nUser instructions.\n");
        write_new_file(&skill_path, edited.as_bytes())
            .await
            .unwrap();

        let error = install_project(dir.path(), &config(), "scheduler-mcp", None, &[])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Existing project Skill differs"));
        assert_eq!(
            tokio::fs::read(&skill_path).await.unwrap(),
            edited.as_bytes()
        );
        assert!(!dir.path().join(MCP_PATH).exists());
    }

    #[tokio::test]
    async fn refuses_existing_mcp_args_with_a_non_string_entry() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(".codex/config.toml");
        tokio::fs::create_dir(file.parent().unwrap()).await.unwrap();
        let original = "[mcp_servers.runspur_scheduler]\ncommand = 'scheduler-mcp'\nargs = ['--project-root', '.', 42]\n";
        tokio::fs::write(&file, original).await.unwrap();

        assert!(
            install_project(dir.path(), &config(), "scheduler-mcp", None, &[])
                .await
                .is_err()
        );
        assert_eq!(tokio::fs::read_to_string(file).await.unwrap(), original);
        assert!(!dir.path().join(".agents").exists());
    }

    #[tokio::test]
    async fn editing_project_roles_preserves_identity_and_limits() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(PROJECT_FILE);
        let mut original = config();
        original.limits.max_concurrent_workers = 4;
        original.integrations.mcp = false;
        original.integrations.skill = false;
        write_new_file(&file, &serde_json::to_vec(&original).unwrap())
            .await
            .unwrap();
        let roles = ProjectRoles {
            name: "Renamed project".into(),
            main_agent_profile_id: "codex-desktop".into(),
            worker_profile_ids: ["claude-cli".into()].into(),
            allow_workspace_write: original.limits.allow_workspace_write,
            allow_project_write: original.limits.allow_project_write,
        };

        let plan = prepare_project_setup(
            read_project_snapshot(dir.path(), &[]).await.unwrap(),
            roles,
            "scheduler-mcp",
            dir.path(),
            &[],
            ProjectAdapter::Codex,
        )
        .await
        .unwrap();
        assert_eq!(plan.record.config.project_id, original.project_id);
        assert_eq!(plan.record.config.limits, original.limits);
        assert!(!plan.config().integrations.mcp);
        assert!(!plan.config().integrations.skill);
        assert_eq!(plan.record.config.name, "Renamed project");
        assert_eq!(
            plan.record.config.allowed_worker_profile_ids,
            ["claude-cli".into()].into()
        );
        assert_eq!(
            tokio::fs::read(file).await.unwrap(),
            serde_json::to_vec(&original).unwrap()
        );
    }

    #[tokio::test]
    async fn desktop_preview_leaves_project_files_unchanged_and_omits_other_mcp_values() {
        let dir = tempfile::tempdir().unwrap();
        let mcp_path = dir.path().join(".codex/config.toml");
        let original = "developer_instructions = 'private-project-text'\n[mcp_servers.other]\ncommand = 'other-mcp'\n[mcp_servers.other.env]\nTOKEN = 'private-value'\n";
        write_new_file(&mcp_path, original.as_bytes())
            .await
            .unwrap();
        let roles = ProjectRoles {
            name: "Preview".into(),
            main_agent_profile_id: "codex-desktop".into(),
            worker_profile_ids: ["probe".into()].into(),
            allow_workspace_write: false,
            allow_project_write: false,
        };

        let setup = prepare_project_setup(
            read_project_snapshot(dir.path(), &[]).await.unwrap(),
            roles,
            "scheduler-mcp",
            dir.path(),
            &[],
            ProjectAdapter::Codex,
        )
        .await
        .unwrap();
        let preview = setup.preview();
        assert_eq!(preview.manifest_path, dir.path().join(PROJECT_FILE));
        assert!(preview.skill_content.contains("Probe"));
        assert!(!preview.skill_content.contains("private-project-text"));
        assert!(
            preview
                .mcp_content
                .contains("[mcp_servers.runspur_scheduler]")
        );
        assert!(preview.mcp_content.contains("--data-dir"));
        assert!(!preview.mcp_content.contains("private-value"));
        assert!(!preview.mcp_content.contains("other-mcp"));
        assert_eq!(tokio::fs::read_to_string(mcp_path).await.unwrap(), original);
        assert!(!preview.manifest_path.exists());
        assert!(!preview.skill_path.exists());
    }

    #[tokio::test]
    async fn desktop_confirmation_refuses_files_changed_since_preview() {
        let dir = tempfile::tempdir().unwrap();
        let roles = ProjectRoles {
            name: "Preview".into(),
            main_agent_profile_id: "codex-desktop".into(),
            worker_profile_ids: ["probe".into()].into(),
            allow_workspace_write: false,
            allow_project_write: false,
        };
        let setup = prepare_project_setup(
            read_project_snapshot(dir.path(), &[]).await.unwrap(),
            roles,
            "scheduler-mcp",
            dir.path(),
            &[],
            ProjectAdapter::Codex,
        )
        .await
        .unwrap();
        let preview = setup.preview();
        let mcp = "# user added this after preview\n";
        write_new_file(&preview.mcp_path, mcp.as_bytes())
            .await
            .unwrap();

        let error = setup.validate_snapshot().await.unwrap_err();
        assert!(error.to_string().contains("MCP configuration changed"));
        assert_eq!(
            tokio::fs::read_to_string(preview.mcp_path).await.unwrap(),
            mcp
        );
        assert!(!preview.manifest_path.exists());
        assert!(!preview.skill_path.exists());
    }
}
