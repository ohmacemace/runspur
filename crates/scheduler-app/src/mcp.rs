use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, JsonObject,
        ListToolsResult, PaginatedRequestParams, RequestMetaObject, ServerCapabilities,
        ServerConfig, Tool,
    },
    service::RequestContext,
};
use scheduler_core::*;
use scheduler_platform::{DataPaths, LocalClient};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone)]
pub struct ProjectMcp {
    paths: DataPaths,
    project_id: String,
    tools: Arc<Vec<Tool>>,
    host: McpHost,
}

#[derive(Clone)]
pub enum McpHost {
    Codex,
    ManagedClaude(OriginSession),
    NativeClaude,
}

impl McpHost {
    pub fn claude() -> Result<Self> {
        if std::env::var_os("RUNSPUR_CLAUDE_SESSION").is_none() {
            return Ok(Self::NativeClaude);
        }
        Ok(Self::ManagedClaude(claude_origin(
            std::env::var("RUNSPUR_CLAUDE_SESSION").ok().as_deref(),
            std::env::var("CLAUDE_CODE_SESSION_ID").ok().as_deref(),
        )?))
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SubmitParams {
    /// The profile ID of a child Agent allowed by the current project.
    worker_profile_id: String,
    /// The main Agent's task brief for the selected child Agent, forwarded as written.
    prompt: String,
    /// Reuse this key when retrying the exact same logical submission.
    idempotency_key: String,
    timeout_seconds: Option<u32>,
    /// Choose the advertised mode for the deliverable. Omission defaults to read-only in project access mode. Native full access uses the worker's own shell and file tools in the live project.
    execution_mode: Option<McpExecutionMode>,
    /// Exact project-relative file paths writable in the isolated task workspace.
    #[serde(default)]
    write_scope: Vec<String>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum McpExecutionMode {
    ReadOnly,
    WorkspaceWrite,
    ProjectWrite,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TaskParams {
    task_id: String,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListParams {
    limit: Option<u32>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AckParams {
    delivery_id: String,
    ack_nonce: String,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ClaimParams {
    delivery_id: String,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyParams {}

impl ProjectMcp {
    pub async fn new(project_root: &Path, paths: DataPaths) -> Result<Self> {
        Self::for_host(project_root, paths, McpHost::Codex).await
    }

    pub async fn for_host(project_root: &Path, paths: DataPaths, host: McpHost) -> Result<Self> {
        let (_, project) = crate::setup::read_project(project_root).await?;
        if !project.integrations.mcp {
            bail!("MCP integration is disabled for this project");
        }
        let mut tools = vec![
            Tool::new("scheduler_context", "Get this project's policy, the origin session, and global worker settings.", JsonObject::new()).with_input_schema::<EmptyParams>(),
            Tool::new("scheduler_agents", "List the project's child Agents, their available execution modes, and whether native full access is active for this session.", JsonObject::new()).with_input_schema::<EmptyParams>(),
            Tool::new("scheduler_submit", "Give an allowed child Agent ownership of a complete task or an independent deliverable. Choose an advertised execution mode that fits the work. In native full access the worker can use its shell and edit the live project. Project-scoped writes return isolated artifacts. The call queues the task and returns an ID immediately. Account for required results before reporting completion.", JsonObject::new()).with_input_schema::<SubmitParams>(),
            Tool::new("scheduler_task", "Check one task's progress or result in the originating session when needed for dependent work.", JsonObject::new()).with_input_schema::<TaskParams>(),
            Tool::new("scheduler_tasks", "List this origin session's recent tasks.", JsonObject::new()).with_input_schema::<ListParams>(),
            Tool::new("scheduler_cancel", "Request cancellation of one task owned by this origin session.", JsonObject::new()).with_input_schema::<TaskParams>(),
            Tool::new("scheduler_ack", "Confirm consumption of the returned result, using its delivery_id and ack_nonce.", JsonObject::new()).with_input_schema::<AckParams>(),
        ];
        if matches!(&host, McpHost::NativeClaude) {
            tools.push(Tool::new(
                "scheduler_claim",
                "Claim the result named in a Runspur completion notification. Only the originating conversation can claim it.",
                JsonObject::new(),
            ).with_input_schema::<ClaimParams>());
        }
        Ok(Self {
            paths,
            project_id: project.project_id,
            tools: Arc::new(tools),
            host,
        })
    }

    async fn execute(
        &self,
        request: CallToolRequestParams,
        meta: &RequestMetaObject,
    ) -> Result<Value> {
        let origin = match &self.host {
            McpHost::Codex => resolve_origin(meta)?,
            McpHost::ManagedClaude(origin) => origin.clone(),
            McpHost::NativeClaude => native_origin(meta)?,
        };
        let operation = tool_operation(
            &request.name,
            Value::Object(request.arguments.unwrap_or_default()),
        )?;
        LocalClient {
            paths: self.paths.clone(),
            credential: CredentialRef::Project {
                project_id: self.project_id.clone(),
            },
            origin: Some(origin),
        }
        .call(operation)
        .await
    }
}

fn tool_operation(name: &str, arguments: Value) -> Result<Operation> {
    Ok(match name {
        "scheduler_context" => {
            let _: EmptyParams = serde_json::from_value(arguments)?;
            Operation::SessionStatus
        }
        "scheduler_agents" => {
            let _: EmptyParams = serde_json::from_value(arguments)?;
            Operation::ListProfiles
        }
        "scheduler_submit" => {
            let params: SubmitParams = serde_json::from_value(arguments)?;
            Operation::Submit(SubmitTask {
                worker_profile_id: params.worker_profile_id,
                prompt: params.prompt,
                idempotency_key: params.idempotency_key,
                timeout_seconds: params.timeout_seconds,
                execution_mode: match params.execution_mode {
                    Some(McpExecutionMode::WorkspaceWrite) => ExecutionMode::WorkspaceWrite,
                    Some(McpExecutionMode::ProjectWrite) => ExecutionMode::ProjectWrite,
                    Some(McpExecutionMode::ReadOnly) | None => ExecutionMode::ReadOnly,
                },
                write_scope: params.write_scope,
            })
        }
        "scheduler_task" => {
            let params: TaskParams = serde_json::from_value(arguments)?;
            Operation::GetTask {
                task_id: params.task_id,
            }
        }
        "scheduler_tasks" => {
            let params: ListParams = serde_json::from_value(arguments)?;
            Operation::ListTasks {
                limit: params.limit.unwrap_or(20).min(50),
            }
        }
        "scheduler_cancel" => {
            let params: TaskParams = serde_json::from_value(arguments)?;
            Operation::CancelTask {
                task_id: params.task_id,
            }
        }
        "scheduler_ack" => {
            let params: AckParams = serde_json::from_value(arguments)?;
            Operation::Ack {
                delivery_id: params.delivery_id,
                nonce: params.ack_nonce,
            }
        }
        "scheduler_claim" => {
            let params: ClaimParams = serde_json::from_value(arguments)?;
            Operation::ClaimClaudeDelivery {
                delivery_id: params.delivery_id,
            }
        }
        _ => bail!("Unknown scheduler tool"),
    })
}

fn resolve_origin(meta: &RequestMetaObject) -> Result<OriginSession> {
    Ok(scheduler_runtime::codex_mcp_origin(meta.get("threadId"))?)
}

fn native_origin(meta: &RequestMetaObject) -> Result<OriginSession> {
    let tool_use_id = meta
        .get("claudecode/toolUseId")
        .and_then(Value::as_str)
        .context("Claude Code did not provide a native tool-call identity")?;
    Ok(OriginSession {
        provider_session_id: tool_use_id.into(),
        source: OriginSource::ClaudeTool,
    })
}

#[derive(Deserialize)]
struct NativeClaudeHook {
    session_id: String,
    cwd: PathBuf,
    hook_event_name: String,
    tool_name: String,
    tool_input: Value,
    tool_use_id: String,
    agent_id: Option<String>,
    permission_mode: Option<String>,
    mcp_server: ClaudeMcpServer,
}

#[derive(Deserialize)]
struct ClaudeMcpServer {
    name: String,
}

/// Register one native tool call using only host hook input and environment.
pub async fn register_native_claude_hook(
    project_root: &Path,
    paths: DataPaths,
    input: &[u8],
) -> Result<()> {
    if std::env::var_os("RUNSPUR_CLAUDE_SESSION").is_some() {
        return Ok(());
    }
    let hook: NativeClaudeHook = serde_json::from_slice(input)?;
    if hook.hook_event_name != "PreToolUse"
        || hook.mcp_server.name != "runspur_scheduler"
        || hook.agent_id.is_some()
    {
        bail!("Only the main Claude Code conversation can attest a Runspur tool call");
    }
    let (root, project) = crate::setup::read_project(project_root).await?;
    if !hook.cwd.canonicalize()?.starts_with(&root) {
        bail!("The Claude Code tool call is outside the configured Runspur workspace");
    }
    let session = uuid::Uuid::parse_str(&hook.session_id)?;
    if std::env::var("CLAUDE_CODE_SESSION_ID")
        .ok()
        .and_then(|id| uuid::Uuid::parse_str(&id).ok())
        != Some(session)
    {
        bail!("Claude Code's hook did not attest the current session ID");
    }
    let name = hook
        .tool_name
        .strip_prefix("mcp__runspur_scheduler__")
        .context("Unrecognized Runspur tool hook")?;
    let operation = tool_operation(name, hook.tool_input)?;
    let call = ClaudeHookCall {
        host_pid: std::env::var("CLAUDE_PID")
            .context("Claude Code hook host PID is unavailable")?
            .parse()?,
        project_root: root,
        inbox: std::env::var("CLAUDE_CODE_MESSAGING_SOCKET")
            .context("Claude Code native inbox is unavailable")?,
        inbox_token: std::env::var("CLAUDE_CODE_MESSAGING_TOKEN")
            .context("Claude Code native inbox token is unavailable")?,
        tool_use_id: hook.tool_use_id,
        operation_sha256: fingerprint(&operation)?,
        permission_mode: hook.permission_mode,
    };
    LocalClient {
        paths,
        credential: CredentialRef::Project {
            project_id: project.project_id,
        },
        origin: Some(OriginSession {
            provider_session_id: session.to_string(),
            source: OriginSource::SessionHook,
        }),
    }
    .call(Operation::RegisterClaudeHook(call))
    .await?;
    Ok(())
}

fn claude_origin(expected: Option<&str>, native: Option<&str>) -> Result<OriginSession> {
    let parse = |value: Option<&str>| value.and_then(|id| uuid::Uuid::parse_str(id).ok());
    let Some(expected) = parse(expected).filter(|id| Some(*id) == parse(native)) else {
        bail!("Start this Claude Code conversation with Runspur to use its scheduler tools");
    };
    Ok(OriginSession {
        provider_session_id: expected.to_string(),
        source: OriginSource::HostEnvironment,
    })
}

impl ServerHandler for ProjectMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("runspur-scheduler", env!("CARGO_PKG_VERSION")))
            .with_instructions("Runspur child Agents can complete whole assignments, including implementation when their advertised access permits it. Check scheduler_context and scheduler_agents, then dispatch early with scheduler_submit. Keep each deliverable with its assigned Agent. Results return asynchronously to the originating conversation. As the main Agent, you are accountable for the final deliverable. Review and verify child Agent results before adopting them or reporting completion. Child Agents cannot delegate through Runspur.")
    }
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.tools.as_ref().clone()))
    }
    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|t| t.name == name).cloned()
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        match self.execute(request, &context.meta).await {
            Ok(value) => Ok(CallToolResult::structured(if value.is_object() {
                value
            } else {
                json!({"items":value})
            })
            .into()),
            Err(error) => {
                let api = scheduler_runtime::api_error(&error);
                Ok(
                    CallToolResult::structured_error(
                        json!({"code":api.code,"message":api.message}),
                    )
                    .into(),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_origin_uses_only_the_host_tool_use_identity() {
        let meta: RequestMetaObject = serde_json::from_value(json!({
            "claudecode/toolUseId":"toolu_native", "threadId":"unused-session"
        }))
        .unwrap();
        let origin = native_origin(&meta).unwrap();
        assert_eq!(origin.source, OriginSource::ClaudeTool);
        assert_eq!(origin.provider_session_id, "toolu_native");
        let only_session: RequestMetaObject =
            serde_json::from_value(json!({"threadId":"unused-session"})).unwrap();
        assert!(native_origin(&only_session).is_err());
        assert!(
            tool_operation(
                "scheduler_claim",
                json!({"delivery_id":"id","session_id":"forged"})
            )
            .is_err()
        );
    }
    #[test]
    fn managed_claude_requires_matching_native_and_launcher_session_ids() {
        let a = uuid::Uuid::new_v4().to_string();
        let b = uuid::Uuid::new_v4().to_string();
        let origin = claude_origin(Some(&a), Some(&a)).unwrap();
        assert_eq!(origin.provider_session_id, a);
        assert_eq!(origin.source, OriginSource::HostEnvironment);
        assert!(claude_origin(Some(&a), Some(&b)).is_err());
        assert!(claude_origin(Some(&a), None).is_err());
        assert!(claude_origin(None, Some(&a)).is_err());
    }
    #[test]
    fn each_call_resolves_its_own_origin_and_does_not_accept_forged_tool_arguments() {
        let a = new_id();
        let b = new_id();
        let first: RequestMetaObject = serde_json::from_value(json!({"threadId":a})).unwrap();
        let second: RequestMetaObject = serde_json::from_value(json!({"threadId":b})).unwrap();
        assert_eq!(resolve_origin(&first).unwrap().provider_session_id, a);
        assert_eq!(resolve_origin(&second).unwrap().provider_session_id, b);
        let forged = json!({"worker_profile_id":"probe","prompt":"x","idempotency_key":"key","session_id":a,"role":"main"});
        assert!(serde_json::from_value::<SubmitParams>(forged).is_err());
        let bad: RequestMetaObject = serde_json::from_value(json!({"threadId":null})).unwrap();
        assert!(resolve_origin(&bad).is_err());
    }
}
