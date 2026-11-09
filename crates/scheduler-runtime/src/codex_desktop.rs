use std::{collections::HashSet, path::PathBuf, sync::Mutex, time::Duration};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use scheduler_core::*;
use scheduler_platform::{
    BoxStream, connect_pipe, current_principal, process_info, process_principal, read_frame,
    write_frame,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::codex_host::{CodexHost, classify_codex_host};
use crate::{DeliveryOutcome, ParentAdapter, ResolvedParent};

const PIPE: &str = r"\\.\pipe\codex-ipc";
const MAX_NATIVE_FRAME: usize = 16 * 1024 * 1024;
const VERIFIED_ASAR: &str = "fb47d6491dc3006b708cb1e14de479b882bf50f88b7daa81818f154c253266aa";

pub fn codex_mcp_origin(thread_id: Option<&Value>) -> scheduler_core::Result<OriginSession> {
    let id = thread_id
        .and_then(Value::as_str)
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
        .ok_or_else(|| {
            SchedulerError::new(
                "origin_session_unavailable",
                "Codex did not provide a valid thread ID on this tool call",
            )
        })?;
    Ok(OriginSession {
        provider_session_id: id.into(),
        source: OriginSource::RequestMetadata,
    })
}

#[derive(Default)]
pub struct CodexDesktopAdapter {
    verified_processes: Mutex<HashSet<ProcessIdentity>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NativeGeneration {
    server_process: ProcessIdentity,
    owner_client_id: String,
}

struct NativeConnection {
    stream: BoxStream,
    server_process: ProcessIdentity,
    client_id: String,
}

impl NativeConnection {
    async fn connect(adapter: &CodexDesktopAdapter) -> Result<Self> {
        let peer = connect_pipe(PIPE).await?;
        adapter.verify_broker(&peer.process).await?;
        let mut connection = Self {
            stream: peer.stream,
            server_process: peer.process,
            client_id: "initializing-client".into(),
        };
        let response = connection
            .request("initialize", json!({"clientType":"cli"}), 0, None)
            .await?;
        connection.client_id = response["result"]["clientId"]
            .as_str()
            .context("Desktop initialization did not return a client ID")?
            .into();
        Ok(connection)
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        version: u32,
        target: Option<&str>,
    ) -> Result<Value> {
        let id = new_id();
        write_frame(
            &mut self.stream,
            &json!({"type":"request","requestId":id,
            "sourceClientId":self.client_id,"version":version,"method":method,"params":params,
            "targetClientId":target,"timeoutMs":10000}),
            MAX_NATIVE_FRAME,
        )
        .await?;
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                let message = self.next_message().await?;
                if message["type"] == "response" && message["requestId"] == id {
                    if message["resultType"] == "error" {
                        bail!(
                            "Desktop rejected {method}: {}",
                            message["error"].as_str().unwrap_or("unknown error")
                        );
                    }
                    return Ok(message);
                }
            }
        })
        .await
        .context("Desktop request outcome is unknown after timeout")?
    }

    async fn next_message(&mut self) -> Result<Value> {
        let message: Value = read_frame(&mut self.stream, MAX_NATIVE_FRAME).await?;
        if message["type"] == "client-discovery-request" {
            write_frame(
                &mut self.stream,
                &json!({"type":"client-discovery-response",
                "requestId":message["requestId"],"response":{"canHandle":false}}),
                MAX_NATIVE_FRAME,
            )
            .await?;
        }
        Ok(message)
    }

    async fn owner(&mut self, session: &str) -> Result<String> {
        let response = self
            .request(
                "thread-owner-discovery",
                json!({"hostId":"local","conversationId":session}),
                1,
                None,
            )
            .await?;
        if response["result"]["supportsUntrustedAppInput"] != true {
            bail!("Desktop owner does not support untrusted external results");
        }
        Ok(response["handledByClientId"]
            .as_str()
            .context("Desktop did not identify the session owner")?
            .into())
    }

    async fn following(&mut self, session: &str, owner: &str, enabled: bool) -> Result<()> {
        write_frame(
            &mut self.stream,
            &json!({"type":"broadcast","method":"thread-stream-following-changed",
            "sourceClientId":self.client_id,"targetClientIds":[owner],"version":1,
            "params":{"conversationId":session,"hostId":"local","following":enabled}}),
            MAX_NATIVE_FRAME,
        )
        .await?;
        Ok(())
    }

    async fn snapshot(&mut self, session: &str, owner: &str) -> Result<Value> {
        self.following(session, owner, true).await?;
        let received = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let message = self.next_message().await?;
                if message["type"] == "broadcast"
                    && message["method"] == "thread-stream-state-changed"
                    && message["sourceClientId"] == owner
                    && message["params"]["conversationId"] == session
                    && message["params"]["change"]["type"] == "snapshot"
                {
                    return Ok::<_, anyhow::Error>(
                        message["params"]["change"]["conversationState"].clone(),
                    );
                }
            }
        })
        .await
        .context("Desktop did not provide a bounded current-state snapshot")
        .and_then(|r| r);
        let _ = self.following(session, owner, false).await;
        received
    }

    async fn deliver_result(
        &mut self,
        session: &str,
        owner: &str,
        snapshot: &Value,
        envelope: &DeliveryEnvelope,
    ) -> Result<DeliveryOutcome> {
        let status = &snapshot["threadRuntimeStatus"];
        if status["type"] == "active"
            && status["activeFlags"].as_array().is_some_and(|flags| {
                flags
                    .iter()
                    .any(|flag| flag == "waitingOnApproval" || flag == "waitingOnUserInput")
            })
        {
            return Ok(DeliveryOutcome::Held(
                "Parent is waiting for approval or user input".into(),
            ));
        }
        // Standalone tool output is queued on an active Codex turn.
        let request = native_result_request(session, envelope)?;
        match self
            .request(
                "thread-follower-start-turn",
                json!({"conversationId":session,
                "turnStart":{"request":request,"context":{"inheritThreadSettings":true}}}),
                2,
                Some(owner),
            )
            .await
        {
            Ok(response) => match response["result"]["result"]["turn"]["id"].as_str() {
                Some(id) => Ok(DeliveryOutcome::Accepted {
                    parent_turn_id: id.into(),
                }),
                None => Ok(DeliveryOutcome::Uncertain(
                    "Desktop accepted a response without a correlated turn ID".into(),
                )),
            },
            Err(error) => Ok(DeliveryOutcome::Uncertain(format!(
                "Result request may have reached the desktop: {error}"
            ))),
        }
    }
}

impl CodexDesktopAdapter {
    async fn verify_broker(&self, process: &ProcessIdentity) -> Result<()> {
        if self
            .verified_processes
            .lock()
            .map_err(|_| anyhow::anyhow!("Broker cache poisoned"))?
            .contains(process)
        {
            return Ok(());
        }
        if process_principal(process.pid)? != current_principal()? {
            bail!("Desktop pipe belongs to another user");
        }
        let info = process_info(process.pid)?;
        let name = info
            .executable
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or_default();
        if !name.eq_ignore_ascii_case("ChatGPT.exe") && !name.eq_ignore_ascii_case("Codex.exe") {
            bail!("Desktop pipe is not held by the expected desktop program");
        }
        let asar = info
            .executable
            .parent()
            .context("Desktop program directory is unavailable")?
            .join("resources/app.asar");
        let hash = tokio::task::spawn_blocking(move || -> Result<String> {
            use std::io::Read;
            let mut file = std::fs::File::open(asar)?;
            let mut hash = Sha256::new();
            let mut buffer = [0u8; 65536];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                hash.update(&buffer[..count]);
            }
            Ok(hex::encode(hash.finalize()))
        })
        .await??;
        if hash != VERIFIED_ASAR {
            bail!("Desktop distribution is not in this adapter's verified compatibility record");
        }
        self.verified_processes
            .lock()
            .map_err(|_| anyhow::anyhow!("Broker cache poisoned"))?
            .insert(process.clone());
        Ok(())
    }

    async fn resolved(
        &self,
        project: &ProjectRecord,
        origin: &OriginSession,
    ) -> Result<ResolvedParent> {
        uuid::Uuid::parse_str(&origin.provider_session_id)
            .context("Codex origin must be an actual thread UUID")?;
        let mut connection = NativeConnection::connect(self).await?;
        let owner = connection.owner(&origin.provider_session_id).await?;
        let snapshot = connection
            .snapshot(&origin.provider_session_id, &owner)
            .await?;
        let cwd = snapshot["cwd"]
            .as_str()
            .context("Desktop snapshot did not provide a working directory")?;
        let actual = tokio::fs::canonicalize(cwd)
            .await
            .context("Cannot validate desktop working directory")?;
        let expected = tokio::fs::canonicalize(&project.root).await?;
        let declared = nearest_project(&actual).await?;
        if let Some(config) = declared {
            if config.project_id != project.config.project_id {
                bail!("Desktop origin belongs to another project");
            }
        } else if !actual.starts_with(&expected) {
            bail!("Desktop origin belongs to an unregistered project workspace");
        }
        let source = snapshot["threadSource"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if source.contains("subagent") || source.contains("sub_agent") {
            bail!("A provider subagent thread cannot become a scheduler root");
        }
        let generation = NativeGeneration {
            server_process: connection.server_process,
            owner_client_id: owner,
        };
        Ok(ResolvedParent {
            generation: scheduler_core::fingerprint(&generation)?,
            workspace_root: actual,
            main_permission: active_main_permission(&snapshot),
            route: ParentRoute {
                adapter_id: self.id().into(),
                session_id: origin.provider_session_id.clone(),
                parameters: serde_json::to_value(generation)?,
            },
        })
    }

    pub fn known_core_path(&self) -> Result<Option<PathBuf>> {
        let list = self
            .verified_processes
            .lock()
            .map_err(|_| anyhow::anyhow!("Broker cache poisoned"))?;
        for process in list.iter() {
            if let Ok(info) = process_info(process.pid)
                && let Some(parent) = info.executable.parent()
            {
                let path = parent.join("resources/codex.exe");
                if path.is_file() {
                    return Ok(Some(path));
                }
            }
        }
        Ok(None)
    }
}

#[async_trait]
impl ParentAdapter for CodexDesktopAdapter {
    fn id(&self) -> &'static str {
        "codex-desktop"
    }
    fn profile_kind(&self) -> scheduler_core::AgentKind {
        scheduler_core::AgentKind::CodexDesktop
    }
    fn family(&self) -> &'static str {
        "codex"
    }
    async fn resolve_from_peer(
        &self,
        project: &ProjectRecord,
        origin: &OriginSession,
        peer: &ProcessIdentity,
    ) -> Result<ResolvedParent> {
        match classify_codex_host(peer) {
            CodexHost::DesktopProcess => self.resolve(project, origin).await,
            CodexHost::LocalCli => Err(SchedulerError::new(
                "codex_cli_return_unavailable",
                "Detected Codex in a terminal process. Active return to this terminal session is not available.",
            ).into()),
            CodexHost::Unknown => Err(SchedulerError::new(
                "origin_session_unavailable",
                "Runspur could not verify the local host of this Codex tool call.",
            ).into()),
        }
    }
    async fn resolve(
        &self,
        project: &ProjectRecord,
        origin: &OriginSession,
    ) -> Result<ResolvedParent> {
        #[cfg(windows)]
        {
            self.resolved(project, origin).await
        }
        #[cfg(not(windows))]
        {
            let _ = (project, origin);
            bail!("Codex desktop return has only been verified on Windows")
        }
    }
    async fn deliver(
        &self,
        registration: &Registration,
        envelope: &DeliveryEnvelope,
    ) -> DeliveryOutcome {
        match self.send(registration, envelope).await {
            Ok(outcome) => outcome,
            Err(error) => DeliveryOutcome::Unavailable(error.to_string()),
        }
    }
}

impl CodexDesktopAdapter {
    async fn send(
        &self,
        registration: &Registration,
        envelope: &DeliveryEnvelope,
    ) -> Result<DeliveryOutcome> {
        let expected: NativeGeneration =
            serde_json::from_value(registration.route.parameters.clone())?;
        let mut connection = NativeConnection::connect(self).await?;
        let owner = connection.owner(&registration.route.session_id).await?;
        if connection.server_process != expected.server_process || owner != expected.owner_client_id
        {
            return Ok(DeliveryOutcome::Unavailable(
                "Origin generation requires automatic revalidation".into(),
            ));
        }
        let snapshot = connection
            .snapshot(&registration.route.session_id, &owner)
            .await?;
        connection
            .deliver_result(&registration.route.session_id, &owner, &snapshot, envelope)
            .await
    }
}

#[derive(Serialize)]
struct NativeTextInput {
    r#type: &'static str,
    text: String,
    text_elements: Vec<Value>,
}

fn native_result_request(session: &str, envelope: &DeliveryEnvelope) -> Result<Value> {
    // Keep this typed even for the empty input list: future text inputs must materialize text_elements.
    let input: Vec<NativeTextInput> = Vec::new();
    Ok(
        json!({"threadId":session,"input":input,"toolOutput":{"name":"runspur_worker_result",
        "namespace":null,"output":serde_json::to_string(envelope)?,"retain":true}}),
    )
}

async fn nearest_project(cwd: &std::path::Path) -> Result<Option<ProjectConfig>> {
    for directory in cwd.ancestors() {
        let file = directory.join(PROJECT_FILE);
        if file.is_file() {
            let bytes = tokio::fs::read(file).await?;
            if bytes.len() > MAX_FRAME_BYTES {
                bail!("Project declaration exceeds the configuration bound");
            }
            let config: ProjectConfig = serde_json::from_slice(&bytes)?;
            validate_project(&config)?;
            return Ok(Some(config));
        }
    }
    Ok(None)
}

fn active_main_permission(snapshot: &Value) -> Option<MainPermission> {
    let turn = if snapshot["turnHistory"]["kind"] == "canonical" {
        let history = &snapshot["turnHistory"]["history"];
        let island = history["islands"].as_array()?.last()?;
        if island["newerBoundary"]["status"] != "exhausted" {
            return None;
        }
        let key = island["entries"].as_array()?.last()?["value"].as_str()?;
        history["entitiesByKey"].get(key)?
    } else {
        snapshot["turns"].as_array()?.last()?
    };
    if turn["status"] != "inProgress"
        || turn["turnId"].as_str().is_none_or(str::is_empty)
        || turn["pendingUserMessageAdmission"] == true
    {
        return None;
    }
    let turn_permissions = &turn["params"];
    let current_permissions = &snapshot["currentPermissions"];
    let turn_mode = permission_mode(turn_permissions, turn_permissions["permissions"].as_str());
    let current_mode = permission_mode(
        current_permissions,
        current_permissions["activePermissionProfile"]["id"].as_str(),
    );
    // A reconstructed turn can reuse earlier parameters. Full access requires the
    // admitted turn and the owner's current permissions to agree.
    match (turn_mode, current_mode) {
        (Some(MainPermission::ReadOnly), _) | (_, Some(MainPermission::ReadOnly)) => {
            Some(MainPermission::ReadOnly)
        }
        (Some(MainPermission::Full), Some(MainPermission::Full)) => Some(MainPermission::Full),
        (None, None) => None,
        _ => Some(MainPermission::Restricted),
    }
}

fn permission_mode(permissions: &Value, profile: Option<&str>) -> Option<MainPermission> {
    match (permissions["sandboxPolicy"]["type"].as_str(), profile) {
        (Some("readOnly"), _) | (_, Some(":read-only")) => Some(MainPermission::ReadOnly),
        (Some("dangerFullAccess"), None | Some(":danger-full-access"))
        | (None, Some(":danger-full-access")) => Some(MainPermission::Full),
        (None, None) => None,
        _ => Some(MainPermission::Restricted),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_access_requires_agreement_with_the_admitted_active_turn() {
        let mut turn = json!({
            "turnId": "current-turn",
            "status": "inProgress",
            "params": {"permissions": ":danger-full-access"}
        });
        let permission = json!({
            "sandboxPolicy": {"type": "dangerFullAccess"},
            "activePermissionProfile": {"id": ":danger-full-access"}
        });
        let legacy = |turn: &Value| {
            json!({
                "turns": [turn],
                "currentPermissions": permission
            })
        };
        let canonical = json!({
            "turns": [],
            "currentPermissions": permission,
            "turnHistory": {"kind": "canonical", "history": {
                "islands": [{"newerBoundary": {"status": "exhausted"}, "entries": [{"value": "current"}]}],
                "entitiesByKey": {"current": turn}
            }}
        });
        assert_eq!(
            active_main_permission(&legacy(&turn)),
            Some(MainPermission::Full)
        );
        assert_eq!(
            active_main_permission(&canonical),
            Some(MainPermission::Full)
        );
        let mut history_with_gap = canonical;
        history_with_gap["turnHistory"]["history"]["islands"][0]["newerBoundary"]["status"] =
            json!("available");
        assert_eq!(active_main_permission(&history_with_gap), None);

        turn["params"] = json!({"sandboxPolicy": {"type": "readOnly"}});
        assert_eq!(
            active_main_permission(&legacy(&turn)),
            Some(MainPermission::ReadOnly)
        );
        turn["params"] = json!({"permissions": ":workspace"});
        assert_eq!(
            active_main_permission(&legacy(&turn)),
            Some(MainPermission::Restricted)
        );
        turn["params"] = json!({"sandboxPolicy": {"type": "dangerFullAccess"}});
        let mut different = legacy(&turn);
        different["currentPermissions"] = json!({"sandboxPolicy": {"type": "workspaceWrite"}});
        assert_eq!(
            active_main_permission(&different),
            Some(MainPermission::Restricted)
        );
        different["currentPermissions"] = json!({"sandboxPolicy": {"type": "readOnly"}});
        assert_eq!(
            active_main_permission(&different),
            Some(MainPermission::ReadOnly)
        );

        turn["pendingUserMessageAdmission"] = json!(true);
        assert_eq!(active_main_permission(&legacy(&turn)), None);
        turn["pendingUserMessageAdmission"] = json!(false);
        turn["status"] = json!("completed");
        assert_eq!(active_main_permission(&legacy(&turn)), None);
        assert_eq!(
            active_main_permission(&json!({"currentPermissions": permission})),
            None
        );
    }

    #[test]
    fn native_data_shapes_preserve_result_role_and_required_text_arrays() {
        let text = NativeTextInput {
            r#type: "text",
            text: "marker".into(),
            text_elements: vec![],
        };
        let value = serde_json::to_value(text).unwrap();
        assert_eq!(value["text_elements"], json!([]));
        let envelope = test_envelope();
        let request = native_result_request("origin", &envelope).unwrap();
        assert_eq!(request["input"], json!([]));
        assert_eq!(request["toolOutput"]["name"], "runspur_worker_result");
        assert_eq!(request["toolOutput"]["retain"], true);
        assert!(!request.to_string().contains("role\":\"user"));
    }

    fn test_envelope() -> DeliveryEnvelope {
        DeliveryEnvelope {
            schema_version: 1,
            kind: "task_completed".into(),
            task_id: new_id(),
            delivery_id: new_id(),
            project_id: new_id(),
            workspace_root: ".".into(),
            execution_state: ExecutionState::Failed,
            result: TaskResult {
                summary: "data, not authorization".into(),
                exit_code: Some(1),
                error_class: Some("worker_failed".into()),
                verification: vec![],
                artifact_refs: vec![],
            },
            ack_nonce: "nonce".into(),
        }
    }

    #[tokio::test]
    async fn active_origin_accepts_result_as_tool_output_and_holds_for_user_decisions() {
        let (client, mut peer) = tokio::io::duplex(4096);
        let mut connection = NativeConnection {
            stream: Box::pin(client),
            server_process: ProcessIdentity {
                pid: 1,
                started_at: 1,
            },
            client_id: "bridge".into(),
        };
        let envelope = test_envelope();
        for flag in ["waitingOnApproval", "waitingOnUserInput"] {
            let held = connection
                .deliver_result(
                    "origin",
                    "owner",
                    &json!({
                        "threadRuntimeStatus": {"type": "active", "activeFlags": [flag]}
                    }),
                    &envelope,
                )
                .await
                .unwrap();
            assert!(matches!(held, DeliveryOutcome::Held(_)));
        }

        let server = tokio::spawn(async move {
            let request: Value = read_frame(&mut peer, MAX_NATIVE_FRAME).await.unwrap();
            assert_eq!(request["method"], "thread-follower-start-turn");
            assert_eq!(request["targetClientId"], "owner");
            assert_eq!(request["params"]["conversationId"], "origin");
            let body = &request["params"]["turnStart"]["request"];
            assert_eq!(body["input"], json!([]));
            assert_eq!(body["toolOutput"]["name"], "runspur_worker_result");
            let reply = json!({"type":"response", "requestId":request["requestId"],
                "result":{"result":{"turn":{"id":"active-turn"}}}});
            write_frame(&mut peer, &reply, MAX_NATIVE_FRAME)
                .await
                .unwrap();
        });
        let active = json!({"threadRuntimeStatus":{"type":"active","activeFlags":[]}});
        let outcome = connection
            .deliver_result("origin", "owner", &active, &envelope)
            .await
            .unwrap();
        assert!(
            matches!(outcome, DeliveryOutcome::Accepted { parent_turn_id } if parent_turn_id == "active-turn")
        );
        server.await.unwrap();
    }
}
