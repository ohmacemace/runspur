use serde::{Deserialize, Serialize};

use crate::{AgentProfile, OriginSession, ProjectRecord, SubmitTask};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Operation {
    Health,
    PutProject(ProjectRecord),
    ListProjects,
    PutProfile(AgentProfile),
    ListProfiles,
    SessionStatus,
    Submit(SubmitTask),
    GetTask { task_id: String },
    ListTasks { limit: u32 },
    CancelTask { task_id: String },
    Ack { delivery_id: String, nonce: String },
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
