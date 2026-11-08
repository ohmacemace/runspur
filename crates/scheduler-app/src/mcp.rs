use std::{path::Path, sync::Arc};

use anyhow::{Result, bail};
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
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SubmitParams {
    /// A worker ID allowed by the current project.
    worker_profile_id: String,
    /// Independent read-only task and necessary context.
    prompt: String,
    /// Reuse this key when retrying the exact same logical submission.
    idempotency_key: String,
    timeout_seconds: Option<u32>,
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
struct EmptyParams {}

impl ProjectMcp {
    pub async fn new(project_root: &Path, paths: DataPaths) -> Result<Self> {
        let (_, project) = crate::setup::read_project(project_root).await?;
        if !project.integrations.mcp {
            bail!("MCP integration is disabled for this project");
        }
        let tools = vec![
            Tool::new("scheduler_context", "Get this project's effective policy and the automatically resolved origin session.", JsonObject::new()).with_input_schema::<EmptyParams>(),
            Tool::new("scheduler_agents", "List Agent profiles allowed for this project.", JsonObject::new()).with_input_schema::<EmptyParams>(),
            Tool::new("scheduler_submit", "Persist and enqueue an independent worker task; returns task_id without waiting for execution. Completion is pushed to this origin session.", JsonObject::new()).with_input_schema::<SubmitParams>(),
            Tool::new("scheduler_task", "Read one task owned by this origin session for an explicit status or diagnostic request.", JsonObject::new()).with_input_schema::<TaskParams>(),
            Tool::new("scheduler_tasks", "List this origin session's recent tasks.", JsonObject::new()).with_input_schema::<ListParams>(),
            Tool::new("scheduler_cancel", "Request cancellation of one task owned by this origin session.", JsonObject::new()).with_input_schema::<TaskParams>(),
            Tool::new("scheduler_ack", "Confirm consumption of the specified pushed result, using its delivery_id and ack_nonce.", JsonObject::new()).with_input_schema::<AckParams>(),
        ];
        Ok(Self {
            paths,
            project_id: project.project_id,
            tools: Arc::new(tools),
        })
    }

    async fn execute(
        &self,
        request: CallToolRequestParams,
        meta: &RequestMetaObject,
    ) -> Result<Value> {
        let origin = resolve_origin(meta)?;
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let operation = match request.name.as_ref() {
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
            _ => bail!("Unknown scheduler tool"),
        };
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

fn resolve_origin(meta: &RequestMetaObject) -> Result<OriginSession> {
    if let Some(value) = meta.get("threadId") {
        let id = value.as_str().filter(|v| !v.is_empty()).context_origin()?;
        uuid::Uuid::parse_str(id).map_err(|_| {
            SchedulerError::new("origin_session_unavailable", "Host threadId is not a UUID")
        })?;
        return Ok(OriginSession {
            provider_session_id: id.into(),
            source: OriginSource::RequestMetadata,
        });
    }
    // Environment fallback is invocation-scoped only; project configuration never supplies these IDs.
    for key in ["CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID"] {
        if let Ok(id) = std::env::var(key)
            && !id.is_empty()
        {
            return Ok(OriginSession {
                provider_session_id: id,
                source: OriginSource::HostEnvironment,
            });
        }
    }
    Err(SchedulerError::new("origin_session_unavailable", "The host supplied no reliable conversation context; this adapter needs host metadata or a session hook, not manual fixed-session configuration").into())
}
trait OriginOption<T> {
    fn context_origin(self) -> Result<T>;
}
impl<T> OriginOption<T> for Option<T> {
    fn context_origin(self) -> Result<T> {
        self.ok_or_else(|| {
            SchedulerError::new(
                "origin_session_unavailable",
                "Host threadId is missing or malformed",
            )
            .into()
        })
    }
}

impl ServerHandler for ProjectMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("runspur-scheduler", env!("CARGO_PKG_VERSION")))
            .with_instructions("Project-scoped asynchronous delegation. Host conversation metadata is resolved per call. Submit returns after persistence; results are actively pushed. Worker invocations cannot delegate.")
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
