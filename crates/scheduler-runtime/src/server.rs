use std::{sync::Arc, time::Duration};

use anyhow::{Result, bail};
use scheduler_core::*;
use scheduler_platform::{LocalListener, PeerConnection, proof_payload, read_frame, write_frame};

use crate::Scheduler;

enum ConnectionAuth {
    Scheduler(AuthContext, Option<String>),
    ClaudeHook {
        project_id: String,
        origin: OriginSession,
    },
}

pub async fn serve(scheduler: Arc<Scheduler>, mut listener: LocalListener) -> Result<()> {
    serve_on(scheduler, &mut listener).await
}

pub async fn serve_on(scheduler: Arc<Scheduler>, listener: &mut LocalListener) -> Result<()> {
    let mut connections = tokio::task::JoinSet::new();
    let limit = Arc::new(tokio::sync::Semaphore::new(64));
    loop {
        tokio::select! {
            _ = scheduler.stop.cancelled() => break,
            result = listener.accept() => {
                let peer = result?;
                let Ok(permit) = limit.clone().try_acquire_owned() else { continue };
                let app = scheduler.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = tokio::time::timeout(Duration::from_secs(30), serve_connection(app, peer)).await {
                        tracing::warn!(%error, "Local client connection timed out");
                    }
                });
            },
            _ = connections.join_next(), if !connections.is_empty() => {},
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}

pub async fn serve_connection(scheduler: Arc<Scheduler>, mut peer: PeerConnection) -> Result<()> {
    if scheduler.workers.contains(peer.process.pid)? {
        write_frame(
            &mut peer.stream,
            &Challenge {
                version: WIRE_VERSION,
                server_nonce: String::new(),
                error: Some(ApiError {
                    code: "worker_cannot_dispatch".into(),
                    message: "Worker process trees cannot use scheduler control-plane credentials"
                        .into(),
                }),
            },
            MAX_FRAME_BYTES,
        )
        .await?;
        return Ok(());
    }
    let server_nonce = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    write_frame(
        &mut peer.stream,
        &Challenge {
            version: WIRE_VERSION,
            server_nonce: server_nonce.clone(),
            error: None,
        },
        MAX_FRAME_BYTES,
    )
    .await?;
    let proof: ClientProof = read_frame(&mut peer.stream, MAX_FRAME_BYTES).await?;
    let authenticated = async {
        if proof.version != WIRE_VERSION || proof.client_nonce.len() != 64 {
            bail!("Invalid client authentication envelope");
        }
        let key = scheduler.credentials().load(&proof.credential)?;
        let payload = proof_payload(&server_nonce, &proof)?;
        if !key.verify("client", &payload, &proof.proof) {
            bail!("Client authentication proof failed");
        }
        let auth = match &proof.credential {
            CredentialRef::Admin => ConnectionAuth::Scheduler(AuthContext::Admin, None),
            CredentialRef::Project { project_id } => match proof.origin.as_ref() {
                Some(origin) if origin.source == OriginSource::SessionHook => {
                    ConnectionAuth::ClaudeHook {
                        project_id: project_id.clone(),
                        origin: origin.clone(),
                    }
                }
                Some(origin) if origin.source == OriginSource::ClaudeTool => {
                    let (auth, operation_hash) = scheduler
                        .resolve_claude_call(project_id, origin, &peer.process)
                        .await?;
                    ConnectionAuth::Scheduler(auth, Some(operation_hash))
                }
                _ => ConnectionAuth::Scheduler(
                    scheduler
                        .resolve_origin_with_peer(
                            project_id,
                            proof.origin.as_ref(),
                            Some(&peer.process),
                        )
                        .await?,
                    None,
                ),
            },
        };
        Ok::<_, anyhow::Error>((auth, key.sign("server", &payload)))
    }
    .await;
    let (auth, confirmation) = match authenticated {
        Ok(context) => context,
        Err(error) => {
            let api = api_error(&error);
            write_frame(
                &mut peer.stream,
                &ServerProof {
                    proof: String::new(),
                    error: Some(api),
                },
                MAX_FRAME_BYTES,
            )
            .await?;
            return Ok(());
        }
    };
    write_frame(
        &mut peer.stream,
        &ServerProof {
            proof: confirmation,
            error: None,
        },
        MAX_FRAME_BYTES,
    )
    .await?;
    let request: ApiRequest = read_frame(&mut peer.stream, MAX_FRAME_BYTES).await?;
    let shutdown = matches!(request.operation, Operation::Shutdown);
    let response = if request.version != WIRE_VERSION {
        ApiResponse::error(
            request.request_id,
            "unsupported_version",
            "Unsupported local API version",
        )
    } else {
        let result = match auth {
            ConnectionAuth::Scheduler(auth, expected) => {
                if expected.as_ref().is_some_and(|expected| {
                    fingerprint(&request.operation).as_ref() != Ok(expected)
                }) {
                    Err(SchedulerError::new(
                        "invalid_claude_call_proof",
                        "Claude Code tool input changed after its native hook attestation",
                    )
                    .into())
                } else {
                    scheduler.handle(&auth, request.operation).await
                }
            }
            ConnectionAuth::ClaudeHook { project_id, origin } => match request.operation {
                Operation::RegisterClaudeHook(hook) => {
                    scheduler
                        .register_claude_hook(&project_id, &origin, hook, &peer.process)
                        .await
                }
                _ => Err(SchedulerError::new(
                    "invalid_claude_hook_request",
                    "Claude Code hook connections can only attest native calls",
                )
                .into()),
            },
        };
        match result {
            Ok(value) => ApiResponse::success(request.request_id, value),
            Err(error) => {
                let api = api_error(&error);
                ApiResponse::error(request.request_id, api.code, api.message)
            }
        }
    };
    write_frame(&mut peer.stream, &response, MAX_FRAME_BYTES).await?;
    if shutdown && response.error.is_none() {
        scheduler.request_shutdown();
    }
    Ok(())
}

pub fn api_error(error: &anyhow::Error) -> ApiError {
    if let Some(error) = error.downcast_ref::<SchedulerError>() {
        ApiError {
            code: error.code.clone(),
            message: error.message.clone(),
        }
    } else {
        ApiError {
            code: "operation_failed".into(),
            message: crate::redact_text(&error.to_string()),
        }
    }
}
