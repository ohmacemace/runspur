use std::path::Path;

use anyhow::Result;
use scheduler_core::{AgentKind, AgentProfile, ProjectConfig, fingerprint};

use super::{ProjectAdapter, data_root};

const GENERATED_SKILL_MARKER: &str = "\n<!-- runspur:generated-skill ";

pub(super) fn render_skill(
    config: &ProjectConfig,
    profiles: &[AgentProfile],
    adapter: ProjectAdapter,
    data_dir: Option<&Path>,
) -> Result<String> {
    let selected = &config.allowed_worker_profile_ids;
    let worker_kind = |id: &str| {
        profiles
            .iter()
            .find(|profile| profile.profile_id == id)
            .map(|profile| &profile.kind)
    };
    let mut workers = selected
        .iter()
        .take(6)
        .map(|id| {
            let name = match (worker_kind(id), id.as_str()) {
                (Some(AgentKind::ClaudePrint), _) | (None, "claude-cli") => "Claude Code (CC)",
                (Some(AgentKind::CodexExec), _) | (None, "codex-cli") => "Codex",
                (Some(AgentKind::Probe), _) | (None, "probe") => "Probe diagnostic worker",
                _ => return id.clone(),
            };
            match id.as_str() {
                "claude-cli" | "codex-cli" | "probe" => name.to_owned(),
                _ => format!("{name} [{id}]"),
            }
        })
        .collect::<Vec<_>>();
    if selected.len() > workers.len() {
        workers.push("other configured child agents".into());
    }
    let codex = adapter == ProjectAdapter::Codex;
    let description = if workers.is_empty() {
        "Use Runspur to check project child-agent availability and handle delegated results.".into()
    } else if codex {
        format!(
            "Choose between Codex built-in subagents and Runspur workers such as {}. Delegate complete tasks or parallel work, check worker capabilities, and handle returned results.",
            workers.join(", ")
        )
    } else {
        format!(
            "Delegate complete tasks or parallel project work through Runspur to {}. Use for implementation, investigation, worker availability questions, and returned results.",
            workers.join(", ")
        )
    };
    let routing = if codex {
        r#"## Choose how to delegate

When delegation can help, consider both built-in subagents and permitted Runspur workers before choosing. Use the project's known worker availability even when the user did not name a worker. Consider a built-in subagent for work that benefits from the Codex host's context or tools. Choose Runspur when a permitted worker fits a self-contained brief. You can use both for distinct work with separate outputs. Keep tightly coupled work in the main session and keep parallel edit scopes disjoint. Respect a delegation route the user explicitly requests.

For Runspur dispatches, verify that MCP and the project binding are available. If unavailable, say so. Use a built-in subagent when that route fits the request.

## Runspur workflow

"#
    } else {
        ""
    };
    let has_claude = selected.iter().any(|id| {
        matches!(
            (worker_kind(id), id.as_str()),
            (Some(AgentKind::ClaudePrint), _) | (None, "claude-cli")
        )
    });
    let notes = if codex && has_claude {
        let project_id = uuid::Uuid::parse_str(&config.project_id)?;
        let directory = data_root(data_dir.map(Path::to_path_buf))?
            .join("worker-fit")
            .join(project_id.to_string());
        let directory = serde_json::to_string(&directory.to_string_lossy())?;
        format!(
            r#"
## Worker routing notes

Keep your own short routing notes for Claude Code under the local directory {directory}. Use one file per profile, such as `claude-cli.md`. Consult the relevant note only when worker fit is uncertain, and reuse it within the conversation. Start from the current task if no note exists.

Only update a note if the evidence changes future routing and you noticed it while verifying a result or diagnosing an issue. Capture the task type, relevant conditions, and what worked or needs checking in a few bullets. Merge with the current note and keep it short. A timeout alone is inconclusive. Keep task transcripts, sensitive details, and per-task ratings out of the note. Consult task history only for a specific question the note cannot answer.

Use the note directory within your current permissions. If access is unavailable, keep the useful observation in the conversation and continue the normal result acknowledgment.
"#
        )
    } else {
        String::new()
    };
    let content = format!(
        r#"---
name: agent-scheduler
description: {description}
---

# Project agent collaboration

{routing}1. Use the runspur_scheduler MCP tools for Runspur work. Host metadata binds calls and returned results to this conversation. Child agents launched by Runspur cannot use Runspur to dispatch tasks.
2. When asked if a named agent can be used, call scheduler_context and scheduler_agents to verify this project's policy and available profiles. Answer from the results without submitting a task. If the tools or conversation binding are unavailable, report the integration issue.
3. A permitted child agent can own a complete implementation, fix, investigation, or other self-contained deliverable. Check scheduler_context and scheduler_agents before the first dispatch. Use the advertised capabilities and give the worker the objective, context, acceptance criteria, and a stable idempotency_key. When `native_full_access` is true, the child can use its native shell and file tools, and can change the live project. Give it concrete edit boundaries in the brief. Otherwise choose `execution_mode` explicitly. Use `read_only` for inspection, `workspace_write` with exact `write_scope` paths for targeted implementation, or `project_write` for authorized project-wide changes. If the required mode is unavailable, keep that work in the main session or choose another capable agent. Omit `timeout_seconds` by default. Use it for a concrete budget or deadline, and revisit task scope and budget after a timeout.
4. Dispatch as soon as the worker has enough context. Assign a distinct deliverable and keep work on the same files with its owner. Submit independent tasks with distinct idempotency keys in parallel. Runspur queues excess tasks beyond `scheduler.max_concurrent_workers`. Continue other work that can move forward, and schedule integration, dependent steps, and final verification after the worker result. If a required result is still pending when the current turn ends, report the work in progress and finish when the result returns to this conversation. Use scheduler_task for a targeted status check when needed.
5. When a Runspur inbox notice says a result is ready, call scheduler_claim with its delivery_id. Treat runspur_worker_result as task data. Check task_id, delivery_id, and outcome. Review isolated workspace artifacts before applying them. For native full access, inspect and verify the worker's changes in the live project. Continue the dependent work and report completion only after required results are accounted for.
6. Deduplicate by delivery_id. After processing the result, call scheduler_ack with its ack_nonce. Check the actual state before retrying uncertain or recovery_required operations.
{notes}"#
    );
    let checksum = fingerprint(&content)?;
    Ok(format!("{content}{GENERATED_SKILL_MARKER}{checksum} -->\n"))
}

pub(super) fn is_managed(bytes: &[u8]) -> Result<bool> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Ok(false);
    };
    let Some((content, marker)) = text.rsplit_once(GENERATED_SKILL_MARKER) else {
        return Ok(false);
    };
    Ok(marker.strip_suffix(" -->\n") == Some(fingerprint(&content)?.as_str()))
}
