use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, OnceLock},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use regex::Regex;
use scheduler_core::*;
use scheduler_platform::{
    DataPaths, WorkerJob, WorkerRegistry, configure_suspended, identity, resume_suspended,
};
use tokio::{
    fs::{self, File},
    io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader},
    process::Command,
};
use tokio_util::sync::CancellationToken;

const MAX_LINE: usize = 1024 * 1024;
const MAX_LOG: usize = 8 * 1024 * 1024;

pub fn redact_text(text: &str) -> String {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r#"(?i)(bearer\s+|(?:api[_-]?key|access[_-]?token|messaging[_-]?token|password)\s*[=:]\s*[\"']?)[^\s\"',;]+"#).expect("Static redaction pattern"))
        .replace_all(text, "${1}<redacted>").into_owned()
}
fn redact_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                let key = key.to_ascii_lowercase();
                if key.contains("token")
                    || key.contains("secret")
                    || key.contains("password")
                    || key.contains("api_key")
                    || key.contains("apikey")
                    || key == "authorization"
                {
                    *value = serde_json::Value::String("<redacted>".into());
                } else {
                    redact_json(value);
                }
            }
        }
        serde_json::Value::Array(values) => values.iter_mut().for_each(redact_json),
        serde_json::Value::String(text) => *text = redact_text(text),
        _ => {}
    }
}

fn bounded_text(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.into();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[output truncated]", &value[..end])
}

#[derive(Default, Debug)]
pub struct ParsedWorker {
    pub terminal: bool,
    pub success: bool,
    pub summary: String,
    pub verification: Vec<Verification>,
    pub error_class: Option<String>,
    pub log_truncated: bool,
}
impl ParsedWorker {
    pub fn consume(&mut self, kind: &AgentKind, event: &serde_json::Value) -> Result<()> {
        match (kind, event.get("type").and_then(|v| v.as_str())) {
            (AgentKind::CodexExec, Some("item.completed")) => {
                let item = &event["item"];
                if item["type"] == "agent_message" {
                    self.summary = bounded_text(
                        item["text"]
                            .as_str()
                            .context("Codex agent message is missing text")?,
                        MAX_RESULT_BYTES,
                    );
                }
            }
            (AgentKind::CodexExec, Some("turn.completed")) => {
                self.terminal = true;
                self.success = true;
                self.error_class = None;
            }
            (AgentKind::CodexExec, Some("turn.failed")) => {
                self.terminal = true;
                self.success = false;
                self.summary = bounded_text(
                    event["error"]["message"]
                        .as_str()
                        .unwrap_or("Codex turn failed"),
                    MAX_RESULT_BYTES,
                );
                self.error_class = Some("agent_reported_error".into());
            }
            (AgentKind::ClaudePrint, Some("result")) => {
                let error = event["is_error"]
                    .as_bool()
                    .context("Claude result is missing is_error")?;
                let subtype = event["subtype"]
                    .as_str()
                    .context("Claude result is missing subtype")?;
                self.terminal = true;
                self.success = !error && subtype == "success";
                self.summary = bounded_text(
                    event["result"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| {
                            event
                                .get("errors")
                                .map(ToString::to_string)
                                .unwrap_or_else(|| subtype.into())
                        })
                        .as_str(),
                    MAX_RESULT_BYTES,
                );
                self.error_class = (!self.success).then(|| "agent_reported_error".into());
            }
            (AgentKind::Probe, Some("probe_result")) => {
                self.terminal = true;
                self.success = event["success"]
                    .as_bool()
                    .context("Probe result is missing success")?;
                self.summary = bounded_text(
                    event["summary"]
                        .as_str()
                        .context("Probe result is missing summary")?,
                    MAX_RESULT_BYTES,
                );
                if let Some(value) = event.get("verification") {
                    self.verification = serde_json::from_value(value.clone())?;
                }
                self.error_class = (!self.success).then(|| "probe_failed".into());
            }
            _ => {}
        }
        Ok(())
    }
}

fn resolve_program(program: &Path) -> Result<PathBuf> {
    let resolved = if program.is_absolute() {
        fs_canonical(program)?
    } else if program.components().count() == 1 {
        let mut names = vec![program.to_path_buf()];
        #[cfg(windows)]
        if program.extension().is_none() {
            names.insert(0, program.with_extension("exe"));
        }
        let mut found = None;
        for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            if !dir.is_absolute() {
                continue;
            }
            for name in &names {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    found = Some(fs_canonical(&candidate)?);
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        found.context("Worker executable was not found on the absolute PATH entries")?
    } else {
        bail!("Worker executable must be absolute or a bare executable name");
    };
    #[cfg(windows)]
    if resolved
        .extension()
        .and_then(|v| v.to_str())
        .is_none_or(|v| !v.eq_ignore_ascii_case("exe"))
    {
        bail!("Windows workers require the actual .exe, not a .cmd/.ps1 shell wrapper");
    }
    Ok(resolved)
}
fn fs_canonical(path: &Path) -> Result<PathBuf> {
    if !path.is_file() {
        bail!("Worker executable is not a file: {}", path.display());
    }
    Ok(std::fs::canonicalize(path)?)
}

pub fn worker_command(task: &QueuedTask) -> Result<Command> {
    validate_profile(&task.profile)?;
    let program = resolve_program(
        task.profile
            .program
            .as_deref()
            .context("Worker has no executable")?,
    )?;
    let mut command = Command::new(program);
    command
        .current_dir(&task.project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.env_clear();
    let defaults: BTreeSet<&str> = [
        "PATH",
        "PATHEXT",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "HOME",
        "HOMEDRIVE",
        "HOMEPATH",
        "TEMP",
        "TMP",
        "SHELL",
        "LANG",
        "LC_ALL",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "SSL_CERT_FILE",
        "CODEX_CA_CERTIFICATE",
    ]
    .into();
    for (key, value) in std::env::vars_os() {
        let name = key.to_string_lossy().to_ascii_uppercase();
        if defaults.contains(name.as_str()) || task.profile.environment_keys.contains(name.as_str())
        {
            command.env(key, value);
        }
    }
    command.env("RUNSPUR_WORKER_TASK_ID", &task.view.task_id);
    match task.profile.kind {
        AgentKind::CodexExec => {
            command.args([
                "exec",
                "--json",
                "--color",
                "never",
                "--ephemeral",
                "--ignore-user-config",
                "--skip-git-repo-check",
                "--sandbox",
                "read-only",
                "--disable",
                "plugins",
                "--disable",
                "hooks",
                "--disable",
                "apps",
                "-c",
                "mcp_servers.runspur_scheduler.enabled=false",
            ]);
            if let Some(model) = &task.profile.model {
                command.arg(format!("--model={model}"));
            }
            if let Some(effort) = &task.profile.reasoning_effort {
                command.args(["-c", &format!("model_reasoning_effort=\"{effort}\"")]);
            }
            command.arg("-");
        }
        AgentKind::ClaudePrint => {
            command.args([
                "--safe-mode",
                "--disable-slash-commands",
                "--strict-mcp-config",
                "--mcp-config",
                "{\"mcpServers\":{}}",
                "--tools",
                "Read,Grep,Glob",
                "--permission-mode",
                "plan",
                "--no-chrome",
                "--no-session-persistence",
                "--print",
                "--output-format",
                "stream-json",
                "--verbose",
                "--input-format",
                "text",
            ]);
            if let Some(model) = &task.profile.model {
                command.arg(format!("--model={model}"));
            }
            if let Some(effort) = &task.profile.reasoning_effort {
                command.arg(format!("--effort={effort}"));
            }
        }
        AgentKind::Probe => {
            command.arg("probe-worker");
        }
        AgentKind::CodexDesktop => bail!("A desktop parent is not a CLI worker"),
    }
    configure_suspended(&mut command);
    Ok(command)
}

async fn line_bounded<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    line: &mut Vec<u8>,
) -> Result<usize> {
    line.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(line.len());
        }
        let newline = available.iter().position(|v| *v == b'\n');
        let count = newline.map_or(available.len(), |p| p + 1);
        if line.len() + count > MAX_LINE {
            bail!("Worker protocol line exceeds 1 MiB");
        }
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        if newline.is_some() {
            return Ok(line.len());
        }
    }
}

async fn read_output<R: AsyncRead + Unpin>(
    stream: R,
    path: PathBuf,
    kind: Option<AgentKind>,
) -> Result<ParsedWorker> {
    let mut file = File::create_new(path).await?;
    let mut reader = BufReader::new(stream);
    let mut bytes = Vec::new();
    let mut written = 0;
    let mut parsed = ParsedWorker::default();
    while line_bounded(&mut reader, &mut bytes).await? != 0 {
        let text = std::str::from_utf8(&bytes).context("Worker output is not UTF-8")?;
        let cleaned = if let Some(kind) = &kind {
            if text.trim().is_empty() {
                continue;
            }
            let mut event: serde_json::Value = serde_json::from_str(text)
                .context("Worker stdout is not the declared JSONL protocol")?;
            parsed.consume(kind, &event)?;
            redact_json(&mut event);
            format!("{}\n", serde_json::to_string(&event)?)
        } else {
            redact_text(text)
        };
        if written + cleaned.len() <= MAX_LOG {
            file.write_all(cleaned.as_bytes()).await?;
            written += cleaned.len();
        } else {
            parsed.log_truncated = true;
        }
    }
    file.sync_all().await?;
    parsed.summary = redact_text(&parsed.summary);
    Ok(parsed)
}

enum End {
    Exited(std::process::ExitStatus),
    Canceled,
    TimedOut,
    Error(String),
}

pub async fn run_worker(
    task: &QueuedTask,
    paths: &DataPaths,
    registry: &WorkerRegistry,
    cancel: CancellationToken,
    store: &Store,
) -> Result<Completion> {
    let directory = paths.attempt_dir(&task.view.task_id, &task.attempt_id)?;
    fs::create_dir_all(&directory).await?;
    if task.view.cancel_requested || cancel.is_cancelled() {
        return Ok(stopped(task, ExecutionState::Canceled, "canceled"));
    }
    let mut command = worker_command(task)?;
    let job = Arc::new(WorkerJob::new()?);
    let mut child = command.spawn().context("Cannot start worker executable")?;
    let pid = child.id().context("Worker PID unavailable")?;
    let preparation = (|| -> Result<()> {
        job.attach(pid)?;
        registry.insert(task.view.task_id.clone(), job.clone())?;
        Ok(())
    })();
    if let Err(error) = preparation {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(error);
    }
    let process = identity(pid)?;
    store.record_spawn(&task.view.task_id, process).await?;
    let stdin = child.stdin.take().context("Worker stdin unavailable")?;
    let stdout = child.stdout.take().context("Worker stdout unavailable")?;
    let stderr = child.stderr.take().context("Worker stderr unavailable")?;
    let prompt = if task.profile.kind == AgentKind::Probe {
        task.prompt.clone()
    } else {
        format!(
            "独立只读子任务，task_id={}。请只处理所给任务并给出最终摘要；调度器负责结果交付。\n\n{}",
            task.view.task_id, task.prompt
        )
    };
    let mut input = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(10), async move {
            let mut stdin = stdin;
            stdin.write_all(prompt.as_bytes()).await?;
            stdin.shutdown().await
        })
        .await
        .context("Worker did not accept its input")?
        .map_err(anyhow::Error::from)
    });
    let mut output = tokio::spawn(read_output(
        stdout,
        directory.join("stdout.jsonl"),
        Some(task.profile.kind.clone()),
    ));
    let mut errors = tokio::spawn(read_output(stderr, directory.join("stderr.log"), None));
    if let Err(error) = resume_suspended(pid) {
        let _ = job.terminate();
        input.abort();
        output.abort();
        errors.abort();
        return Err(error).context("Cannot resume supervised worker");
    }
    let deadline = tokio::time::sleep(Duration::from_secs(task.timeout_seconds as u64));
    tokio::pin!(deadline);
    let mut input_done = false;
    let mut output_done = None;
    let mut errors_done = None;
    let mut output_finished = false;
    let mut errors_finished = false;
    let mut reader_error = None;
    let end = loop {
        tokio::select! {
            result = child.wait() => break match result { Ok(status) => End::Exited(status), Err(e) => End::Error(e.to_string()) },
            _ = cancel.cancelled() => break End::Canceled,
            _ = &mut deadline => break End::TimedOut,
            result = &mut input, if !input_done => {
                input_done = true;
                if let Err(error) = result.context("Worker input task failed").and_then(|r| r)
                    && child.try_wait()?.is_none() {
                    break End::Error(error.to_string());
                }
            },
            result = &mut output, if !output_finished => {
                output_finished = true;
                match result.context("Worker stdout reader failed").and_then(|r| r) {
                    Ok(parsed) => output_done = Some(parsed), Err(e) => break End::Error(e.to_string()),
                }
            },
            result = &mut errors, if !errors_finished => {
                errors_finished = true;
                match result.context("Worker stderr reader failed").and_then(|r| r) {
                    Ok(parsed) => errors_done = Some(parsed), Err(e) => break End::Error(e.to_string()),
                }
            },
        }
    };
    let _ = job.terminate();
    let exit = match &end {
        End::Exited(status) => Some(*status),
        _ => tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .ok()
            .and_then(std::result::Result::ok),
    };
    if !input_done {
        input.abort();
    }
    if !output_finished {
        match tokio::time::timeout(Duration::from_secs(3), &mut output).await {
            Ok(Ok(Ok(parsed))) => output_done = Some(parsed),
            Ok(Ok(Err(e))) => reader_error = Some(e.to_string()),
            _ => {
                output.abort();
                reader_error = Some("stdout reader did not finish".into());
            }
        }
    }
    if !errors_finished {
        match tokio::time::timeout(Duration::from_secs(3), &mut errors).await {
            Ok(Ok(Ok(parsed))) => errors_done = Some(parsed),
            Ok(Ok(Err(e))) => reader_error = Some(e.to_string()),
            _ => {
                errors.abort();
                reader_error = Some("stderr reader did not finish".into());
            }
        }
    }
    // Keep a non-empty retiring job registered, so remaining descendants never gain root rights.
    let _ = registry.retire(&task.view.task_id);
    let output_truncated = output_done.as_ref().is_some_and(|p| p.log_truncated);
    let mut result = match end {
        End::Canceled => stopped(task, ExecutionState::Canceled, "canceled"),
        End::TimedOut => stopped(task, ExecutionState::TimedOut, "timed_out"),
        End::Error(error) => failed(task, "protocol_incompatible", redact_text(&error)),
        End::Exited(status) => {
            let parsed = output_done.unwrap_or_default();
            if !parsed.terminal {
                failed(
                    task,
                    "protocol_incompatible",
                    "Worker exited without a valid final event".into(),
                )
            } else {
                let succeeded = status.success() && parsed.success;
                Completion {
                    schema_version: SCHEMA_VERSION,
                    task_id: task.view.task_id.clone(),
                    attempt_id: task.attempt_id.clone(),
                    execution_state: if succeeded {
                        ExecutionState::Succeeded
                    } else {
                        ExecutionState::Failed
                    },
                    finished_at_ms: now_ms(),
                    result: TaskResult {
                        summary: if parsed.summary.is_empty() {
                            "Worker finished without response text".into()
                        } else {
                            parsed.summary
                        },
                        exit_code: status.code(),
                        error_class: if succeeded {
                            None
                        } else {
                            Some(
                                parsed
                                    .error_class
                                    .unwrap_or_else(|| "exit_event_mismatch".into()),
                            )
                        },
                        verification: parsed.verification,
                        artifact_refs: vec![],
                    },
                }
            }
        }
    };
    if let Some(error) = reader_error
        && result.execution_state == ExecutionState::Succeeded
    {
        result = failed(task, "output_persistence_failed", redact_text(&error));
    }
    result.result.exit_code = exit.and_then(|v| v.code());
    result.result.artifact_refs = vec![
        format!("{}/{}/stdout.jsonl", task.view.task_id, task.attempt_id),
        format!("{}/{}/stderr.log", task.view.task_id, task.attempt_id),
    ];
    result.result.verification.push(Verification {
        description: "worker_process_exit".into(),
        outcome: result
            .result
            .exit_code
            .map_or_else(|| "no exit code".into(), |v| v.to_string()),
    });
    if errors_done.is_some_and(|p| p.log_truncated) {
        result.result.verification.push(Verification {
            description: "stderr_log".into(),
            outcome: "truncated at 8 MiB".into(),
        });
    }
    if output_truncated {
        result.result.verification.push(Verification {
            description: "stdout_log".into(),
            outcome: "truncated at 8 MiB".into(),
        });
    }
    Ok(result)
}

pub fn failed(task: &QueuedTask, error_class: &str, summary: String) -> Completion {
    Completion {
        schema_version: SCHEMA_VERSION,
        task_id: task.view.task_id.clone(),
        attempt_id: task.attempt_id.clone(),
        execution_state: ExecutionState::Failed,
        finished_at_ms: now_ms(),
        result: TaskResult {
            summary,
            exit_code: None,
            error_class: Some(error_class.into()),
            verification: vec![],
            artifact_refs: vec![],
        },
    }
}
fn stopped(task: &QueuedTask, state: ExecutionState, class: &str) -> Completion {
    let mut completion = failed(task, class, format!("Task {class}"));
    completion.execution_state = state;
    completion
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn structured_results_are_required_and_provider_specific() {
        let mut codex = ParsedWorker::default();
        codex.consume(&AgentKind::CodexExec, &serde_json::json!({"type":"item.completed","item":{"type":"agent_message","text":"final"}})).unwrap();
        assert!(!codex.terminal);
        codex
            .consume(
                &AgentKind::CodexExec,
                &serde_json::json!({"type":"turn.completed"}),
            )
            .unwrap();
        assert!(codex.terminal && codex.success);
        assert_eq!(codex.summary, "final");
        let mut claude = ParsedWorker::default();
        claude
            .consume(
                &AgentKind::ClaudePrint,
                &serde_json::json!({"type":"assistant","message":"not final"}),
            )
            .unwrap();
        assert!(!claude.terminal);
        claude.consume(&AgentKind::ClaudePrint, &serde_json::json!({"type":"result","is_error":true,"subtype":"error_during_execution","errors":["quota"]})).unwrap();
        assert!(claude.terminal && !claude.success);
        assert!(
            ParsedWorker::default()
                .consume(
                    &AgentKind::ClaudePrint,
                    &serde_json::json!({"type":"result","result":"untyped"})
                )
                .is_err()
        );
    }
    #[test]
    fn logs_redact_credentials_and_truncate_on_utf8_boundaries() {
        let text = redact_text("Authorization: Bearer secret-value api_key=private-value");
        assert!(!text.contains("secret-value"));
        assert!(!text.contains("private-value"));
        let mut value = serde_json::json!({"messaging_token":"private", "result":"ok"});
        redact_json(&mut value);
        assert_eq!(value["messaging_token"], "<redacted>");
        assert!(bounded_text("中文内容", 4).starts_with("中"));
    }
}
