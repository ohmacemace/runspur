use std::{
    collections::BTreeSet,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const SCHEMA_VERSION: u32 = 1;
pub const PROJECT_FILE: &str = ".agent-scheduler/project.json";
pub const MAX_PROMPT_BYTES: usize = 256 * 1024;
pub const MAX_RESULT_BYTES: usize = 128 * 1024;
pub const MAX_SCHEDULER_WORKERS: usize = 64;

pub fn new_id() -> String {
    Uuid::new_v4().to_string()
}
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntegrationConfig {
    pub skill: bool,
    pub mcp: bool,
}
impl Default for IntegrationConfig {
    fn default() -> Self {
        Self {
            skill: true,
            mcp: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DispatchLimits {
    // Retained for manifest compatibility and existing project revision fingerprints.
    pub max_concurrent_workers: u32,
    pub max_dispatches_per_root: u32,
    pub default_timeout_seconds: u32,
    pub max_timeout_seconds: u32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_workspace_write: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_project_write: bool,
}
impl Default for DispatchLimits {
    fn default() -> Self {
        Self {
            max_concurrent_workers: 2,
            max_dispatches_per_root: 20,
            default_timeout_seconds: 3600,
            max_timeout_seconds: 86400,
            allow_workspace_write: false,
            allow_project_write: false,
        }
    }
}

impl DispatchLimits {
    pub fn permits_execution(&self, mode: ExecutionMode) -> bool {
        match mode {
            ExecutionMode::ReadOnly => true,
            ExecutionMode::WorkspaceWrite => self.allow_workspace_write || self.allow_project_write,
            ExecutionMode::ProjectWrite => self.allow_project_write,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    pub schema_version: u32,
    pub project_id: String,
    pub name: String,
    pub main_agent_profile_id: String,
    pub allowed_worker_profile_ids: BTreeSet<String>,
    #[serde(default)]
    pub integrations: IntegrationConfig,
    #[serde(default)]
    pub limits: DispatchLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    CodexDesktop,
    CodexExec,
    ClaudePrint,
    Probe,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerAccess {
    #[default]
    Project,
    Full,
    FollowMain,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MainPermission {
    Full,
    ReadOnly,
    Restricted,
}

impl WorkerAccess {
    pub fn permits_native_full(self, main: Option<MainPermission>, kind: &AgentKind) -> bool {
        self.allows_native_full(main)
            && matches!(kind, AgentKind::ClaudePrint | AgentKind::CodexExec)
    }

    pub(crate) fn allows_native_full(self, main: Option<MainPermission>) -> bool {
        self == Self::Full || (self == Self::FollowMain && main == Some(MainPermission::Full))
    }

    pub(crate) fn inherits_read_only(self, main: Option<MainPermission>) -> bool {
        self == Self::FollowMain && main == Some(MainPermission::ReadOnly)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchedulerSettings {
    pub max_concurrent_workers: usize,
    #[serde(default)]
    pub worker_access: WorkerAccess,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentProfile {
    pub profile_id: String,
    pub kind: AgentKind,
    pub program: Option<PathBuf>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub environment_keys: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ParentRoute {
    pub adapter_id: String,
    pub session_id: String,
    #[serde(default)]
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OriginSource {
    HostEnvironment,
    RequestMetadata,
    SessionHook,
    /// The adapter supplied a native Claude tool-use ID. The hook registration resolves it.
    ClaudeTool,
}

/// Supplied by the integration adapter, never by SubmitTask's model arguments.
/// The source label is diagnostic; it does not grant permission by itself.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OriginSession {
    pub provider_session_id: String,
    pub source: OriginSource,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Registration {
    pub registration_id: String,
    pub project_id: String,
    pub main_agent_profile_id: String,
    pub root_task_id: String,
    pub generation: String,
    pub route: ParentRoute,
    pub workspace_root: PathBuf,
    pub project_revision: String,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    Queued,
    Starting,
    Running,
    WaitingApproval,
    Succeeded,
    Failed,
    Canceled,
    TimedOut,
}
impl ExecutionState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Canceled | Self::TimedOut
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    NotReady,
    Pending,
    Sending,
    AcceptedByTransport,
    AcknowledgedByParent,
    Held,
    Refused,
    RetryWait,
    Orphaned,
    Uncertain,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    #[default]
    ReadOnly,
    WorkspaceWrite,
    ProjectWrite,
}

impl ExecutionMode {
    pub fn is_read_only(&self) -> bool {
        *self == Self::ReadOnly
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitTask {
    pub worker_profile_id: String,
    pub prompt: String,
    pub idempotency_key: String,
    pub timeout_seconds: Option<u32>,
    #[serde(default, skip_serializing_if = "ExecutionMode::is_read_only")]
    pub execution_mode: ExecutionMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_scope: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskAccepted {
    pub task_id: String,
    pub execution_state: ExecutionState,
    pub result_pending: bool,
    pub deduplicated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Verification {
    pub description: String,
    pub outcome: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub summary: String,
    pub exit_code: Option<i32>,
    pub error_class: Option<String>,
    pub verification: Vec<Verification>,
    pub artifact_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskView {
    pub task_id: String,
    pub project_id: String,
    pub parent_registration_id: String,
    pub root_task_id: String,
    pub workspace_root: PathBuf,
    pub worker_profile_id: String,
    pub execution_state: ExecutionState,
    pub delivery_state: DeliveryState,
    pub created_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub result: Option<TaskResult>,
    pub delivery_id: String,
    pub delivery_error: Option<String>,
    pub parent_turn_id: Option<String>,
    #[serde(default)]
    pub cancel_requested: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPage {
    pub items: Vec<TaskView>,
    pub next_cursor: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub origin_id: String,
    pub adapter_id: String,
    pub session_id: String,
    pub task_count: u64,
    pub last_dispatched_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPage {
    pub items: Vec<SessionSummary>,
    pub next_cursor: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDetail {
    pub task: TaskView,
    pub prompt: String,
    pub timeout_seconds: u32,
    #[serde(default)]
    pub origin_id: String,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedTask {
    pub view: TaskView,
    pub prompt: String,
    pub timeout_seconds: u32,
    pub project_root: PathBuf,
    pub project_config: ProjectConfig,
    pub profile: AgentProfile,
    pub attempt_id: String,
    #[serde(default)]
    pub execution_mode: ExecutionMode,
    #[serde(default)]
    pub write_scope: Vec<String>,
    #[serde(default)]
    pub native_full: bool,
    #[serde(default)]
    pub main_permission: Option<MainPermission>,
    #[serde(default)]
    pub access_revoked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Completion {
    pub schema_version: u32,
    pub task_id: String,
    pub attempt_id: String,
    pub execution_state: ExecutionState,
    pub finished_at_ms: i64,
    pub result: TaskResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryEnvelope {
    pub schema_version: u32,
    pub kind: String,
    pub task_id: String,
    pub delivery_id: String,
    pub project_id: String,
    pub workspace_root: PathBuf,
    pub execution_state: ExecutionState,
    pub result: TaskResult,
    pub ack_nonce: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingDelivery {
    pub task: TaskView,
    pub registration: Registration,
    pub attempts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub started_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRecord {
    pub root: PathBuf,
    pub revision: String,
    pub config: ProjectConfig,
}
