use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{AgentProfile, OriginSession, ProjectRecord, SchedulerError, SubmitTask};

pub const WIRE_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CredentialRef {
    Admin,
    Project { project_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Challenge {
    pub version: u32,
    pub server_nonce: String,
    #[serde(default)]
    pub error: Option<ApiError>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientProof {
    pub version: u32,
    pub credential: CredentialRef,
    pub origin: Option<OriginSession>,
    pub client_nonce: String,
    pub proof: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerProof {
    pub proof: String,
    pub error: Option<ApiError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiRequest {
    pub version: u32,
    pub request_id: String,
    pub operation: Operation,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeHookCall {
    pub host_pid: u32,
    pub project_root: std::path::PathBuf,
    pub inbox: String,
    pub inbox_token: String,
    pub tool_use_id: String,
    pub operation_sha256: String,
}

impl std::fmt::Debug for ClaudeHookCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeHookCall")
            .field("host_pid", &self.host_pid)
            .field("tool_use_id", &self.tool_use_id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Operation {
    Health,
    Shutdown,
    PutProject(ProjectRecord),
    ListProjects,
    PutProfile(AgentProfile),
    ListProfiles,
    SessionStatus,
    Submit(SubmitTask),
    GetTask {
        task_id: String,
    },
    ListTasks {
        limit: u32,
    },
    ListSessions {
        project_id: String,
        before: Option<i64>,
        limit: u32,
    },
    TaskHistory {
        project_id: Option<String>,
        session_id: Option<String>,
        before: Option<i64>,
        limit: u32,
    },
    OriginTaskHistory {
        project_id: String,
        origin_id: String,
        before: Option<i64>,
        limit: u32,
    },
    TaskDetail {
        task_id: String,
        offset: usize,
        revision: Option<String>,
    },
    CancelTask {
        task_id: String,
    },
    Ack {
        delivery_id: String,
        nonce: String,
    },
    RegisterClaudeHook(ClaudeHookCall),
    ClaimClaudeDelivery {
        delivery_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiResponse {
    pub request_id: String,
    pub result: Option<serde_json::Value>,
    pub error: Option<ApiError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDetailChunk {
    pub text: String,
    pub next_offset: Option<usize>,
    pub revision: String,
}

impl TaskDetailChunk {
    pub fn from_json(
        json: &str,
        offset: usize,
        expected_revision: Option<&str>,
    ) -> crate::Result<Self> {
        let revision = hex::encode(Sha256::digest(json.as_bytes()));
        if expected_revision.is_some_and(|expected| expected != revision) {
            return Err(SchedulerError::new(
                "task_changed",
                "Task changed during the detail read",
            ));
        }
        if !json.is_char_boundary(offset) {
            return Err(SchedulerError::new(
                "invalid_task_offset",
                "Task detail offset is invalid",
            ));
        }
        if offset != 0 && expected_revision.is_none() {
            return Err(SchedulerError::new(
                "task_revision_required",
                "Continuation requires a task revision",
            ));
        }
        // A byte can expand to six bytes in the JSON envelope around this chunk.
        let end = json.floor_char_boundary(offset.saturating_add(MAX_FRAME_BYTES / 8));
        Ok(Self {
            text: json[offset..end].to_owned(),
            next_offset: (end < json.len()).then_some(end),
            revision,
        })
    }
}

impl ApiResponse {
    pub fn success(request_id: String, value: impl Serialize) -> Self {
        match serde_json::to_value(value) {
            Ok(value) => Self {
                request_id,
                result: Some(value),
                error: None,
            },
            Err(error) => Self::error(request_id, "serialization_error", error.to_string()),
        }
    }
    pub fn error(request_id: String, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            request_id,
            result: None,
            error: Some(ApiError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_detail_chunks_fit_the_wire_and_preserve_unicode() {
        let detail = serde_json::to_string(&"任務\n\u{0001}\"\\🙂".repeat(90_000)).unwrap();
        assert!(detail.len() > MAX_FRAME_BYTES);
        let mut offset = 0;
        let mut revision = None;
        let mut reconstructed = String::new();
        loop {
            let chunk = TaskDetailChunk::from_json(&detail, offset, revision.as_deref()).unwrap();
            let response = ApiResponse::success(crate::new_id(), &chunk);
            assert!(serde_json::to_vec(&response).unwrap().len() <= MAX_FRAME_BYTES);
            reconstructed.push_str(&chunk.text);
            let Some(next) = chunk.next_offset else { break };
            assert_eq!(next, reconstructed.len());
            offset = next;
            revision = Some(chunk.revision);
        }
        assert_eq!(reconstructed, detail);
        assert_eq!(
            TaskDetailChunk::from_json("new task state", offset, revision.as_deref())
                .unwrap_err()
                .code,
            "task_changed"
        );
        assert_eq!(
            TaskDetailChunk::from_json("🙂", 1, None).unwrap_err().code,
            "invalid_task_offset"
        );
        assert_eq!(
            TaskDetailChunk::from_json("🙂 text", 4, None)
                .unwrap_err()
                .code,
            "task_revision_required"
        );
    }
}
