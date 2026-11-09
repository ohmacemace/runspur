use crate::{
    AgentKind, AgentProfile, DispatchLimits, MAX_PROMPT_BYTES, OriginSession, ProjectConfig,
    SCHEMA_VERSION, SubmitTask,
};

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error("{code}: {message}")]
pub struct SchedulerError {
    pub code: String,
    pub message: String,
}

impl SchedulerError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}
pub type Result<T> = std::result::Result<T, SchedulerError>;

impl From<rusqlite::Error> for SchedulerError {
    fn from(error: rusqlite::Error) -> Self {
        Self::new("storage_error", error.to_string())
    }
}
impl From<serde_json::Error> for SchedulerError {
    fn from(error: serde_json::Error) -> Self {
        Self::new("serialization_error", error.to_string())
    }
}

pub fn valid_profile_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

pub fn validate_project(config: &ProjectConfig) -> Result<()> {
    if config.schema_version != SCHEMA_VERSION {
        return Err(SchedulerError::new(
            "unsupported_version",
            "Unsupported project schema",
        ));
    }
    if uuid::Uuid::parse_str(&config.project_id).is_err() {
        return Err(SchedulerError::new(
            "invalid_project",
            "project_id must be a UUID",
        ));
    }
    if config.name.trim().is_empty()
        || !valid_profile_id(&config.main_agent_profile_id)
        || config
            .allowed_worker_profile_ids
            .iter()
            .any(|id| !valid_profile_id(id))
    {
        return Err(SchedulerError::new(
            "invalid_project",
            "Invalid project name or profile ID",
        ));
    }
    validate_limits(&config.limits)
}

fn validate_limits(limits: &DispatchLimits) -> Result<()> {
    if limits.max_dispatches_per_root == 0
        || limits.default_timeout_seconds == 0
        || limits.max_timeout_seconds < limits.default_timeout_seconds
        || limits.max_timeout_seconds > 7 * 24 * 3600
    {
        return Err(SchedulerError::new(
            "invalid_limits",
            "Invalid dispatch count or timeout limits",
        ));
    }
    Ok(())
}

pub fn validate_profile(profile: &AgentProfile) -> Result<()> {
    if !valid_profile_id(&profile.profile_id) {
        return Err(SchedulerError::new("invalid_profile", "Invalid profile ID"));
    }
    if !matches!(profile.kind, AgentKind::CodexDesktop) && profile.program.is_none() {
        return Err(SchedulerError::new(
            "invalid_profile",
            "Worker profile requires an executable",
        ));
    }
    if let Some(model) = &profile.model
        && (model.is_empty()
            || model.len() > 128
            || model.starts_with('-')
            || model.chars().any(char::is_control))
    {
        return Err(SchedulerError::new(
            "invalid_profile",
            "Invalid model identifier",
        ));
    }
    if let Some(effort) = &profile.reasoning_effort
        && !["minimal", "low", "medium", "high", "xhigh", "max"].contains(&effort.as_str())
    {
        return Err(SchedulerError::new(
            "invalid_profile",
            "Unsupported reasoning effort",
        ));
    }
    if profile.environment_keys.iter().any(|key| {
        let key = key.to_ascii_uppercase();
        key.starts_with("RUNSPUR_")
            || key.starts_with("CLAUDE_CODE_MESSAGING_")
            || ["CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID"].contains(&key.as_str())
    }) {
        return Err(SchedulerError::new(
            "invalid_profile",
            "Parent routing environment cannot be delegated",
        ));
    }
    Ok(())
}

/// `known_worker` and `profile_id` come from the runtime/adapter, not model arguments.
pub fn authorize_origin(
    config: &ProjectConfig,
    profile_id: &str,
    origin: Option<&OriginSession>,
    known_worker: bool,
) -> Result<()> {
    if known_worker {
        return Err(SchedulerError::new(
            "worker_cannot_dispatch",
            "A worker invocation cannot become a project root",
        ));
    }
    if profile_id != config.main_agent_profile_id {
        return Err(SchedulerError::new(
            "main_profile_mismatch",
            "Invocation does not match the project's main Agent",
        ));
    }
    if !config.integrations.mcp {
        return Err(SchedulerError::new(
            "integration_disabled",
            "Project MCP integration is disabled",
        ));
    }
    let origin = origin.ok_or_else(|| {
        SchedulerError::new(
            "origin_session_unavailable",
            "The host did not provide a reliable origin session",
        )
    })?;
    if origin.provider_session_id.is_empty()
        || origin.provider_session_id.len() > 256
        || origin.provider_session_id.chars().any(char::is_control)
    {
        return Err(SchedulerError::new(
            "origin_session_unavailable",
            "Invalid origin session identity",
        ));
    }
    Ok(())
}

pub fn valid_scoped_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 512
        && path.split('/').all(|part| {
            if part.is_empty()
                || part.ends_with([' ', '.'])
                || part.eq_ignore_ascii_case(".git")
                || part
                    .chars()
                    .any(|c| c.is_control() || "\\:*?\"<>|~".contains(c))
            {
                return false;
            }
            let device = part
                .split('.')
                .next()
                .unwrap_or_default()
                .trim_end()
                .to_ascii_uppercase();
            if matches!(
                device.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            ) {
                return false;
            }
            !["COM", "LPT"].into_iter().any(|prefix| {
                device.strip_prefix(prefix).is_some_and(|suffix| {
                    matches!(
                        suffix,
                        "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                    )
                })
            })
        })
}

pub fn validate_submission(config: &ProjectConfig, submission: &SubmitTask) -> Result<u32> {
    validate_submission_with_access(config, submission, false)
}

pub(crate) fn validate_submission_with_access(
    config: &ProjectConfig,
    submission: &SubmitTask,
    bypass_project_write_limit: bool,
) -> Result<u32> {
    validate_project(config)?;
    if !config
        .allowed_worker_profile_ids
        .contains(&submission.worker_profile_id)
    {
        return Err(SchedulerError::new(
            "worker_not_allowed",
            "Worker is not allowed in this project",
        ));
    }
    if submission.prompt.trim().is_empty() || submission.prompt.len() > MAX_PROMPT_BYTES {
        return Err(SchedulerError::new(
            "invalid_prompt",
            "Prompt is empty or exceeds the input limit",
        ));
    }
    if submission.idempotency_key.is_empty() || submission.idempotency_key.len() > 128 {
        return Err(SchedulerError::new(
            "invalid_idempotency_key",
            "A bounded idempotency_key is required",
        ));
    }
    let mode = submission.execution_mode;
    if !bypass_project_write_limit {
        validate_project_execution_mode(config, mode)?;
    }
    if mode == crate::ExecutionMode::WorkspaceWrite {
        let unique_paths: std::collections::BTreeSet<_> = submission
            .write_scope
            .iter()
            .map(|path| path.to_lowercase())
            .collect();
        if !(1..=64).contains(&submission.write_scope.len())
            || unique_paths.len() != submission.write_scope.len()
            || !submission
                .write_scope
                .iter()
                .all(|path| valid_scoped_path(path))
        {
            return Err(SchedulerError::new(
                "invalid_write_scope",
                "Provide 1 to 64 distinct exact project-relative file paths",
            ));
        }
    } else if !submission.write_scope.is_empty() {
        return Err(SchedulerError::new(
            "invalid_write_scope",
            "Only workspace_write accepts a list of exact writable files",
        ));
    }
    let timeout = submission
        .timeout_seconds
        .unwrap_or(config.limits.default_timeout_seconds);
    if timeout == 0 || timeout > config.limits.max_timeout_seconds {
        return Err(SchedulerError::new(
            "invalid_timeout",
            "Timeout is outside the project policy",
        ));
    }
    Ok(timeout)
}

pub(crate) fn validate_project_execution_mode(
    config: &ProjectConfig,
    mode: crate::ExecutionMode,
) -> Result<()> {
    if config.limits.permits_execution(mode) {
        return Ok(());
    }
    let (code, message) = if mode == crate::ExecutionMode::ProjectWrite {
        (
            "project_write_disabled",
            "Project policy has not enabled project-wide writes",
        )
    } else {
        (
            "workspace_write_disabled",
            "Project policy has not enabled scoped writes",
        )
    };
    Err(SchedulerError::new(code, message))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthContext {
    Admin,
    Root {
        project_id: String,
        registration_id: String,
    },
}
impl AuthContext {
    pub fn require_admin(&self) -> Result<()> {
        if matches!(self, Self::Admin) {
            Ok(())
        } else {
            Err(SchedulerError::new(
                "permission_denied",
                "Management credentials required",
            ))
        }
    }
    pub fn root(&self) -> Result<(&str, &str)> {
        match self {
            Self::Root {
                project_id,
                registration_id,
            } => Ok((project_id, registration_id)),
            Self::Admin => Err(SchedulerError::new(
                "origin_session_required",
                "An automatically resolved origin session is required",
            )),
        }
    }
    pub fn can_access(&self, project: &str, registration: &str) -> Result<()> {
        match self {
            Self::Admin => Ok(()),
            Self::Root {
                project_id,
                registration_id,
            } if project_id == project && registration_id == registration => Ok(()),
            _ => Err(SchedulerError::new(
                "permission_denied",
                "Task belongs to another root binding",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_write_requires_its_own_opt_in() {
        let mut project = project();
        project.limits.allow_workspace_write = true;
        let mut submission = SubmitTask {
            worker_profile_id: "claude-worker".into(),
            prompt: "Change the project files".into(),
            idempotency_key: "project-write".into(),
            timeout_seconds: None,
            execution_mode: crate::ExecutionMode::ProjectWrite,
            write_scope: vec![],
        };
        assert_eq!(
            validate_submission(&project, &submission).unwrap_err().code,
            "project_write_disabled"
        );
        project.limits.allow_project_write = true;
        assert!(validate_submission(&project, &submission).is_ok());
        submission.write_scope = vec!["src/main.rs".into()];
        assert_eq!(
            validate_submission(&project, &submission).unwrap_err().code,
            "invalid_write_scope"
        );
    }

    #[test]
    fn workspace_write_requires_opt_in_and_exact_portable_paths() {
        let mut project = project();
        let mut submission = SubmitTask {
            worker_profile_id: "claude-worker".into(),
            prompt: "Create the file".into(),
            idempotency_key: "write".into(),
            timeout_seconds: None,
            execution_mode: crate::ExecutionMode::WorkspaceWrite,
            write_scope: vec!["src/hello.py".into()],
        };
        assert_eq!(
            validate_submission(&project, &submission).unwrap_err().code,
            "workspace_write_disabled"
        );
        project.limits.allow_workspace_write = true;
        assert!(validate_submission(&project, &submission).is_ok());
        for invalid in [
            "../hello.py",
            "C:/escape",
            "file:stream",
            ".git/config",
            "NUL.txt",
            "a\\b",
            "x.",
        ] {
            submission.write_scope = vec![invalid.into()];
            assert_eq!(
                validate_submission(&project, &submission).unwrap_err().code,
                "invalid_write_scope"
            );
        }
    }
    use crate::{IntegrationConfig, new_id};

    fn project() -> ProjectConfig {
        ProjectConfig {
            schema_version: 1,
            project_id: new_id(),
            name: "示例项目".into(),
            main_agent_profile_id: "codex-main".into(),
            allowed_worker_profile_ids: ["claude-worker".into()].into(),
            integrations: IntegrationConfig::default(),
            limits: DispatchLimits::default(),
        }
    }
    #[test]
    fn roles_and_projects_do_not_come_from_submission() {
        let root = AuthContext::Root {
            project_id: "p1".into(),
            registration_id: "r1".into(),
        };
        assert!(root.require_admin().is_err());
        assert!(root.can_access("p2", "r1").is_err());
        assert!(root.can_access("p1", "r2").is_err());
        assert!(root.can_access("p1", "r1").is_ok());
        let forged = r#"{"worker_profile_id":"claude-worker","prompt":"x","idempotency_key":"k","timeout_seconds":null,"role":"main","parent_registration_id":"other"}"#;
        assert!(serde_json::from_str::<SubmitTask>(forged).is_err());
    }
    #[test]
    fn policy_rejects_unknown_workers_and_unbounded_inputs() {
        let p = project();
        let mut s = SubmitTask {
            worker_profile_id: "other".into(),
            prompt: "read".into(),
            idempotency_key: "key".into(),
            timeout_seconds: None,
            execution_mode: crate::ExecutionMode::ReadOnly,
            write_scope: Vec::new(),
        };
        assert_eq!(
            validate_submission(&p, &s).unwrap_err().code,
            "worker_not_allowed"
        );
        s.worker_profile_id = "claude-worker".into();
        assert_eq!(validate_submission(&p, &s).unwrap(), 3600);
        s.timeout_seconds = Some(0);
        assert_eq!(
            validate_submission(&p, &s).unwrap_err().code,
            "invalid_timeout"
        );
    }
    #[test]
    fn profile_does_not_accept_raw_execution_flags_or_parent_secrets() {
        let raw = r#"{"profile_id":"worker","kind":"codex_exec","program":"codex.exe","extra_args":["--dangerously-bypass-approvals-and-sandbox"]}"#;
        assert!(serde_json::from_str::<AgentProfile>(raw).is_err());
        let mut profile = AgentProfile {
            profile_id: "worker".into(),
            kind: AgentKind::CodexExec,
            program: Some("codex.exe".into()),
            model: None,
            reasoning_effort: None,
            environment_keys: ["CLAUDE_CODE_MESSAGING_TOKEN".into()].into(),
        };
        assert!(validate_profile(&profile).is_err());
        profile.environment_keys.clear();
        assert!(validate_profile(&profile).is_ok());
    }
    #[test]
    fn normal_project_sessions_are_eligible_but_same_kind_worker_is_not() {
        let project = project();
        let a = crate::OriginSession {
            provider_session_id: "conversation-a".into(),
            source: crate::OriginSource::HostEnvironment,
        };
        let b = crate::OriginSession {
            provider_session_id: "conversation-b".into(),
            source: crate::OriginSource::RequestMetadata,
        };
        assert!(authorize_origin(&project, "codex-main", Some(&a), false).is_ok());
        assert!(authorize_origin(&project, "codex-main", Some(&b), false).is_ok());
        assert_eq!(
            authorize_origin(&project, "codex-main", Some(&a), true)
                .unwrap_err()
                .code,
            "worker_cannot_dispatch"
        );
        assert_eq!(
            authorize_origin(&project, "codex-main", None, false)
                .unwrap_err()
                .code,
            "origin_session_unavailable"
        );
        let a_scope = AuthContext::Root {
            project_id: project.project_id.clone(),
            registration_id: "a".into(),
        };
        let b_scope = AuthContext::Root {
            project_id: project.project_id.clone(),
            registration_id: "b".into(),
        };
        assert!(a_scope.can_access(&project.project_id, "a").is_ok());
        assert!(b_scope.can_access(&project.project_id, "b").is_ok());
        assert!(a_scope.can_access(&project.project_id, "b").is_err());
    }
}
