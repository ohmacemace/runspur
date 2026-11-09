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

#[path = "scoped_files.rs"]
pub mod scoped_files;

const MAX_LINE: usize = 1024 * 1024;
const MAX_LOG: usize = 8 * 1024 * 1024;
const SCOPED_CLAUDE_VERSIONS: &[&str] = &["2.1.220", "2.1.319"];
const SCOPED_CLAUDE_AUTH_KEYS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "API_TIMEOUT_MS",
];

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

pub(crate) fn resolve_program(program: &Path) -> Result<PathBuf> {
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

fn worker_command(task: &QueuedTask, attempt_dir: &Path) -> Result<Command> {
    validate_profile(&task.profile)?;
    let program = resolve_program(
        task.profile
            .program
            .as_deref()
            .context("Worker has no executable")?,
    )?;
    let mut command = Command::new(program);
    let scoped = !task.execution_mode.is_read_only();
    let cwd = if scoped {
        scoped_files::ScopedWorkspace::workspace(attempt_dir)
    } else {
        task.project_root.clone()
    };
    command
        .current_dir(cwd)
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
        if defaults.contains(name.as_str())
            || (task.profile.environment_keys.contains(name.as_str())
                && (!scoped || SCOPED_CLAUDE_AUTH_KEYS.contains(&name.as_str())))
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
                // Override the whole table. A disabled entry without a transport is invalid.
                "mcp_servers={}",
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
            if scoped {
                configure_scoped_claude(&mut command, attempt_dir, task.execution_mode)?;
            } else {
                command.args([
                    "--safe-mode",
                    "--strict-mcp-config",
                    "--mcp-config",
                    "{\"mcpServers\":{}}",
                    "--tools",
                    "Read,Grep,Glob",
                    "--permission-mode",
                    "plan",
                ]);
            }
            command.args([
                "--disable-slash-commands",
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

fn configure_scoped_claude(
    command: &mut Command,
    attempt_dir: &Path,
    mode: ExecutionMode,
) -> Result<()> {
    let broker = std::env::current_exe()?
        .with_file_name(format!("scheduler-mcp{}", std::env::consts::EXE_SUFFIX));
    if !broker.is_file() {
        bail!("The scheduler's scoped file MCP executable is missing");
    }
    let mcp = serde_json::json!({
        "mcpServers": {
            scoped_files::BROKER_SERVER: {
                "command": broker,
                "args": ["--worker-file-broker", scoped_files::ScopedWorkspace::ticket(attempt_dir).to_str().context("The attempt path is not UTF-8")?],
            },
        },
    });
    // Preserve explicit provider authentication and model keys without loading plugins or hooks.
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    if let Some(home) = home {
        let path = PathBuf::from(home).join(".claude/settings.json");
        if let Ok(file) = std::fs::File::open(path)
            && let Ok(settings) = serde_json::from_reader(std::io::Read::take(file, 1024 * 1024))
        {
            for (key, value) in claude_auth_environment(&settings) {
                if !command.as_std().get_envs().any(|(name, _)| name == key) {
                    command.env(key, value);
                }
            }
        }
    }
    let write_instructions = match mode {
        ExecutionMode::ProjectWrite => {
            "Use the Runspur file tools for text changes anywhere within the project. Use project-relative paths. Changes are isolated in the task workspace. Return a short summary of your work."
        }
        ExecutionMode::WorkspaceWrite => {
            "Use the Runspur file tools for workspace changes. Paths are project-relative, and only the task's authorized paths can be written. Return a short summary of your work."
        }
        ExecutionMode::ReadOnly => bail!("Read-only workers cannot launch the write broker"),
    };
    command
        .env("ENABLE_CLAUDEAI_MCP_SERVERS", "false")
        .env("ENABLE_TOOL_SEARCH", "false")
        .args([
            "--setting-sources",
            "",
            "--settings",
            "{\"disableAllHooks\":true,\"disableClaudeAiConnectors\":true,\"autoMemoryEnabled\":false}",
            "--strict-mcp-config",
            "--mcp-config",
            &mcp.to_string(),
            "--tools",
            "",
            "--allowed-tools",
            &scoped_files::BROKER_TOOLS.join(","),
            "--permission-mode",
            "dontAsk",
            "--append-system-prompt",
            write_instructions,
        ]);
    Ok(())
}

fn claude_auth_environment(settings: &serde_json::Value) -> Vec<(&'static str, &str)> {
    SCOPED_CLAUDE_AUTH_KEYS
        .iter()
        .filter_map(|&key| settings["env"][key].as_str().map(|value| (key, value)))
        .collect()
}

async fn scoped_workspace(
    task: &QueuedTask,
    attempt_dir: &Path,
) -> Result<Option<scoped_files::ScopedWorkspace>> {
    let scope = match task.execution_mode {
        ExecutionMode::ReadOnly => return Ok(None),
        ExecutionMode::WorkspaceWrite if !task.write_scope.is_empty() => {
            Some(task.write_scope.as_slice())
        }
        ExecutionMode::ProjectWrite if task.write_scope.is_empty() => None,
        ExecutionMode::WorkspaceWrite | ExecutionMode::ProjectWrite => {
            bail!("Worker write scope does not match its execution mode")
        }
    };
    if !matches!(task.profile.kind, AgentKind::ClaudePrint) {
        bail!("This worker does not support scoped writes");
    }
    let program = resolve_program(
        task.profile
            .program
            .as_deref()
            .context("Worker has no executable")?,
    )?;
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new(program)
            .arg("--version")
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let version = stdout.trim().strip_suffix(" (Claude Code)");
    if !output.status.success()
        || !version.is_some_and(|value| SCOPED_CLAUDE_VERSIONS.contains(&value))
    {
        bail!(
            "Scoped writes require a verified Claude Code version ({})",
            SCOPED_CLAUDE_VERSIONS.join(" or ")
        );
    }
    Ok(Some(scoped_files::ScopedWorkspace::prepare(
        &task.project_root,
        attempt_dir,
        scope,
    )?))
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
    mut scoped_start_pending: bool,
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
            if scoped_start_pending {
                if event["type"] == "system" && event["subtype"] == "init" {
                    verify_scoped_claude_startup(&event)?;
                    scoped_start_pending = false;
                } else if event["type"] != "system" {
                    bail!("Claude did not report the verified scoped tool configuration");
                }
            }
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
    if scoped_start_pending {
        bail!("Claude did not report the verified scoped tool configuration");
    }
    file.sync_all().await?;
    parsed.summary = redact_text(&parsed.summary);
    Ok(parsed)
}

fn verify_scoped_claude_startup(event: &serde_json::Value) -> Result<()> {
    let tools = event["tools"]
        .as_array()
        .context("Claude startup has no tool list")?;
    let servers = event["mcp_servers"]
        .as_array()
        .context("Claude startup has no MCP server list")?;
    if !event["claude_code_version"]
        .as_str()
        .is_some_and(|version| SCOPED_CLAUDE_VERSIONS.contains(&version))
        || event["permissionMode"] != "dontAsk"
        || tools.len() != scoped_files::BROKER_TOOLS.len()
        || !scoped_files::BROKER_TOOLS
            .iter()
            .all(|name| tools.iter().any(|tool| tool == name))
        || servers.len() != 1
        || servers[0]["name"] != scoped_files::BROKER_SERVER
        || servers[0]["status"] != "connected"
    {
        bail!("Claude did not start with only the verified Runspur file tools");
    }
    Ok(())
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
    let workspace = match scoped_workspace(task, &directory).await {
        Ok(workspace) => workspace,
        Err(error) => {
            return Ok(failed(
                task,
                "workspace_write_unavailable",
                redact_text(&error.to_string()),
            ));
        }
    };
    if cancel.is_cancelled() {
        return Ok(stopped(task, ExecutionState::Canceled, "canceled"));
    }
    let mut command = worker_command(task, &directory)?;
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
    let prompt = task.prompt.clone();
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
        workspace.is_some(),
    ));
    let mut errors = tokio::spawn(read_output(
        stderr,
        directory.join("stderr.log"),
        None,
        false,
    ));
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
    if let Some(workspace) = workspace {
        let prefix = format!("{}/{}", task.view.task_id, task.attempt_id);
        match workspace.capture_changes(&directory, &prefix) {
            Ok(artifact) => {
                let outcome = if let Some(artifact) = artifact {
                    result.result.artifact_refs.push(artifact);
                    "captured"
                } else {
                    "no changes"
                };
                result.result.verification.push(Verification {
                    description: "scoped_workspace_changes".into(),
                    outcome: outcome.into(),
                });
            }
            Err(error) if result.execution_state == ExecutionState::Succeeded => {
                result.execution_state = ExecutionState::Failed;
                result.result.error_class = Some("workspace_artifact_failed".into());
                result.result.summary = redact_text(&error.to_string());
            }
            Err(_) => {}
        }
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
    fn scoped_claude_requires_a_verified_build_and_only_the_broker_tools() {
        let mut startup = serde_json::json!({
            "claude_code_version": "2.1.220",
            "permissionMode": "dontAsk",
            "tools": scoped_files::BROKER_TOOLS,
            "mcp_servers": [{"name": scoped_files::BROKER_SERVER, "status": "connected"}],
        });
        assert!(verify_scoped_claude_startup(&startup).is_ok());
        startup["claude_code_version"] = serde_json::json!("2.1.319");
        assert!(verify_scoped_claude_startup(&startup).is_ok());
        startup["claude_code_version"] = serde_json::json!("2.1.320");
        assert!(verify_scoped_claude_startup(&startup).is_err());
        startup["claude_code_version"] = serde_json::json!("2.1.319");
        startup["tools"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!("Bash"));
        assert!(verify_scoped_claude_startup(&startup).is_err());
    }
    #[test]
    fn scoped_claude_imports_auth_without_importing_host_configuration() {
        let settings = serde_json::json!({"env": {
            "ANTHROPIC_BASE_URL": "https://gateway.invalid",
            "ANTHROPIC_MODEL": "test-model",
            "ENABLE_TOOL_SEARCH": "true",
            "NODE_OPTIONS": "--require unrelated.js"
        }});
        let environment = claude_auth_environment(&settings);
        assert!(environment.contains(&("ANTHROPIC_BASE_URL", "https://gateway.invalid")));
        assert!(environment.contains(&("ANTHROPIC_MODEL", "test-model")));
        assert_eq!(environment.len(), 2);
    }
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
