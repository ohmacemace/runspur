use std::time::Duration;

use crate::*;
use anyhow::{Result, bail};
use scheduler_core::*;

#[derive(Clone)]
pub struct LocalClient {
    pub paths: DataPaths,
    pub credential: CredentialRef,
    pub origin: Option<OriginSession>,
}

impl LocalClient {
    pub async fn call(&self, operation: Operation) -> Result<serde_json::Value> {
        tokio::time::timeout(Duration::from_secs(30), self.call_inner(operation)).await
            .map_err(|_| anyhow::anyhow!("Local API request timed out; inspect idempotency/delivery state before retrying"))?
    }

    async fn call_inner(&self, operation: Operation) -> Result<serde_json::Value> {
        let mut peer = connect_pipe(&self.paths.endpoint()?).await?;
        let challenge: Challenge = read_frame(&mut peer.stream, MAX_FRAME_BYTES).await?;
        if let Some(error) = challenge.error {
            return Err(SchedulerError::new(error.code, error.message).into());
        }
        if challenge.version != WIRE_VERSION || challenge.server_nonce.len() != 64 {
            bail!("Invalid local server challenge");
        }
        let key = CredentialStore::new(self.paths.clone()).load(&self.credential)?;
        let mut proof = ClientProof {
            version: WIRE_VERSION,
            credential: self.credential.clone(),
            origin: self.origin.clone(),
            client_nonce: format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
            proof: String::new(),
        };
        let payload = proof_payload(&challenge.server_nonce, &proof)?;
        proof.proof = key.sign("client", &payload);
        write_frame(&mut peer.stream, &proof, MAX_FRAME_BYTES).await?;
        let confirmation: ServerProof = read_frame(&mut peer.stream, MAX_FRAME_BYTES).await?;
        if let Some(error) = confirmation.error {
            return Err(SchedulerError::new(error.code, error.message).into());
        }
        if !key.verify("server", &payload, &confirmation.proof) {
            bail!("Local server proof did not match the expected credential");
        }
        let request_id = new_id();
        write_frame(
            &mut peer.stream,
            &ApiRequest {
                version: WIRE_VERSION,
                request_id: request_id.clone(),
                operation,
            },
            MAX_FRAME_BYTES,
        )
        .await?;
        let response: ApiResponse = read_frame(&mut peer.stream, MAX_FRAME_BYTES).await?;
        if response.request_id != request_id {
            bail!("Local API response ID mismatch");
        }
        if let Some(error) = response.error {
            return Err(SchedulerError::new(error.code, error.message).into());
        }
        response
            .result
            .ok_or_else(|| anyhow::anyhow!("Local API omitted the result"))
    }
}
