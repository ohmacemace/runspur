use std::{path::PathBuf, time::Duration};

use anyhow::{Result, bail};
use scheduler_core::{CredentialRef, MAX_PROMPT_BYTES, Operation, Verification};
use scheduler_platform::{DataPaths, LocalClient};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProbeInput {
    summary: String,
    delay_ms: u64,
    success: bool,
    exit_code: i32,
    omit_final: bool,
    stderr_bytes: usize,
    attempt_control: Option<PathBuf>,
    wait_for_file: Option<PathBuf>,
}
impl Default for ProbeInput {
    fn default() -> Self {
        Self {
            summary: "RUNSPUR_PROBE_OK".into(),
            delay_ms: 250,
            success: true,
            exit_code: 0,
            omit_final: false,
            stderr_bytes: 0,
            attempt_control: None,
            wait_for_file: None,
        }
    }
}

/// A deterministic real subprocess for exercising the scheduler without a model.
pub async fn run_probe() -> Result<i32> {
    let mut input = Vec::new();
    tokio::io::stdin()
        .take((MAX_PROMPT_BYTES + 1) as u64)
        .read_to_end(&mut input)
        .await?;
    if input.len() > MAX_PROMPT_BYTES {
        bail!("Probe input is too large");
    }
    let spec: ProbeInput = if input.iter().copied().find(|b| !b.is_ascii_whitespace()) == Some(b'{')
    {
        serde_json::from_slice(&input)?
    } else {
        ProbeInput {
            summary: String::from_utf8(input)?,
            ..Default::default()
        }
    };
    if spec.delay_ms > 86_400_000 || spec.stderr_bytes > 16 * 1024 * 1024 {
        bail!("Probe request exceeds diagnostic limits");
    }
    let mut verification = Vec::new();
    let mut success = spec.success;
    if let Some(directory) = spec.attempt_control {
        let client = LocalClient {
            paths: DataPaths::open(&directory)?,
            credential: CredentialRef::Admin,
            origin: None,
        };
        let denied = match client.call(Operation::ListProjects).await {
            Ok(_) => false,
            Err(error) => error.to_string().contains("worker_cannot_dispatch"),
        };
        verification.push(Verification {
            description: "worker_control_plane_access".into(),
            outcome: if denied {
                "denied"
            } else {
                "unexpected_response"
            }
            .into(),
        });
        success &= denied;
    }
    if spec.stderr_bytes > 0 {
        let mut stderr = tokio::io::stderr();
        let line = format!("{}\n", "x".repeat(8191));
        let mut remaining = spec.stderr_bytes;
        while remaining > 0 {
            let count = remaining.min(line.len());
            stderr.write_all(&line.as_bytes()[..count]).await?;
            remaining -= count;
        }
        stderr.flush().await?;
    }
    tokio::time::sleep(Duration::from_millis(spec.delay_ms)).await;
    if let Some(path) = spec.wait_for_file {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    if !spec.omit_final {
        let value = serde_json::json!({"type":"probe_result","success":success,
            "summary":spec.summary,"verification":verification});
        let mut stdout = tokio::io::stdout();
        stdout.write_all(format!("{value}\n").as_bytes()).await?;
        stdout.flush().await?;
    }
    Ok(spec.exit_code)
}
