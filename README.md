# Runspur

[![CI](https://github.com/ohmacemace/runspur/actions/workflows/ci.yml/badge.svg)](https://github.com/ohmacemace/runspur/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

A project-scoped, asynchronous Agent scheduler written in Rust.

Runspur accepts tasks through project-level Skills and MCP tools, runs workers in the background, persists their results, and actively delivers each result back to the conversation that submitted it. The parent Agent can keep working while independent tasks run.

**The first release is a Windows x64 backend preview with a CLI and MCP bridge.** Codex desktop delivery is experimental and currently restricted to a verified desktop distribution.

## Install

Run in PowerShell on Windows x64:

```powershell
irm https://raw.githubusercontent.com/ohmacemace/runspur/main/install.ps1 | iex
```

The installer downloads the latest GitHub Release, verifies its SHA-256 checksum and program versions, installs into `%LOCALAPPDATA%\Runspur\bin`, and adds that directory to your user PATH. No Rust, Node.js, or administrator access is required.

Open a new terminal and check:

```powershell
schedulerd --version
schedulerctl --help
scheduler-mcp --help
```

Run the installer again to update. Close running Runspur programs before updating. Tasks, credentials, and project settings are stored separately from the installation directory.

You can also download the ZIP and `SHA256SUMS.txt` from [GitHub Releases](https://github.com/ohmacemace/runspur/releases).

### Install a specific version or location

Download the script, inspect it, and invoke it with parameters:

```powershell
Invoke-WebRequest https://raw.githubusercontent.com/ohmacemace/runspur/main/install.ps1 -OutFile install.ps1
.\install.ps1 -Version v0.1.0
.\install.ps1 -Version v0.1.0 -InstallDir "C:\tools\runspur" -NoPathUpdate
```

Offline installation uses the release ZIP and checksum file:

```powershell
.\install.ps1 -Version v0.1.0 -ArchivePath ".\runspur-v0.1.0-windows-x86_64.zip" -ChecksumPath ".\SHA256SUMS.txt"
```

Requires Windows PowerShell 5.1 or later. To uninstall, stop Runspur, remove its installation directory, and remove that directory from your user PATH. Keep or delete the application data separately as needed.

## Quick start

### 1. Start the daemon

Keep this terminal running. Press Ctrl+C to stop it gracefully.

```powershell
schedulerd
```

The daemon runs as the current user. It stores its database, protected credentials, and artifacts in the user's application data directory.

### 2. Initialize a project

In another terminal, replace the project path with an existing directory:

```powershell
$project = "D:\path\your-project"
$mcp = (Get-Command scheduler-mcp.exe).Source
schedulerctl init --project-root $project
schedulerctl integrate --project-root $project --mcp-program $mcp
schedulerctl doctor --project-root $project
```

This creates:

```text
<project>/
  .agent-scheduler/project.json
  .agents/skills/agent-scheduler/SKILL.md
  .codex/config.toml                     # mcp_servers.runspur_scheduler
```

The default parent profile is `codex-desktop`; the default worker pool contains only `probe`, a deterministic diagnostic worker. The installer preserves unrelated MCP settings and refuses to overwrite conflicting integration files.

`saved_requires_host_reload` means the files were saved. Load or reload the project integration using your host's supported mechanism, and confirm that the Skill and MCP tools are available.

For a custom state directory, pass the same absolute `--data-dir` to the daemon and management commands. Running `schedulerctl integrate` with that option also records the directory in the MCP entry. `RUNSPUR_DATA_DIR` is supported as well.

### 3. Submit a task

Use the project's `agent-scheduler` Skill in a normal Codex desktop conversation. Start with `scheduler_context` and `scheduler_agents`, then call `scheduler_submit` with an allowed worker.

Example diagnostic task:

```json
{
  "worker_profile_id": "probe",
  "prompt": "RUNSPUR_PROBE_OK",
  "idempotency_key": "first-probe",
  "timeout_seconds": 30
}
```

Submission returns a `task_id`, `execution_state: queued`, and `result_pending: true` after persistence. It does not wait for the worker. Repeating the same key and input in the same conversation returns the original task; different input returns `idempotency_conflict`.

Continue independent work after submission. Results are pushed by the outbox; continuous polling is unnecessary. After the parent has actually consumed a result, it confirms consumption with `scheduler_ack`.

Conversation identity is resolved on every call from host metadata such as `_meta.threadId`. Project settings never contain a fixed conversation ID: tasks from A return to A, and tasks from B return to B. Missing reliable identity returns `origin_session_unavailable`.

## Agent profiles

```powershell
schedulerctl profiles
```

This discovers missing defaults and lists trusted local profiles, preserving existing profiles with the same ID. On Windows, discovery checks desktop Codex core installations and `%USERPROFILE%\.local\bin\claude.exe`.

Edit `allowed_worker_profile_ids` in `.agent-scheduler/project.json` to include existing profiles such as `codex-cli` or `claude-cli`, retain the project UUID, then import it again:

```powershell
schedulerctl init --project-root $project
```

A new project can use `init --workers probe,claude-cli,codex-cli` if those profiles are available.

Import a custom profile with `schedulerctl profile-put --file <profile.json>`:

```json
{
  "profile_id": "my-claude",
  "kind": "claude_print",
  "program": "C:\\path\\claude.exe",
  "model": null,
  "reasoning_effort": null,
  "environment_keys": ["ANTHROPIC_API_KEY"]
}
```

Windows workers require the actual `.exe`, rather than a `.cmd` or `.ps1` wrapper. Adapters construct their own read-only arguments. Profiles contain allowed environment-variable names, not their values. Model submissions cannot specify executable paths, raw flags, roles, projects, or parent routes.

## Tools and commands

| MCP tool | Purpose |
| --- | --- |
| `scheduler_context` | Effective project policy and automatically resolved origin |
| `scheduler_agents` | Profiles available to the project |
| `scheduler_submit` | Persist and enqueue a task |
| `scheduler_task` / `scheduler_tasks` | Inspect this conversation's tasks |
| `scheduler_cancel` | Request cancellation |
| `scheduler_ack` | Confirm consumption using `delivery_id` and `ack_nonce` |

Local operator commands:

```powershell
schedulerctl projects
schedulerctl status
schedulerctl status <task-id>
schedulerctl cancel <task-id>
```

Operator access permits inspection and cancellation across conversations, but cannot impersonate a parent's consumption acknowledgment. `scheduler-mcp` uses stdio; its stdout is reserved for MCP messages.

Default limits: two global workers, two workers per project, 20 new tasks per internal root registration, a 3,600-second default timeout, and an 86,400-second maximum timeout. Configure project limits in the manifest and the global limit with `schedulerd --max-workers`. Renewing a registration does not reset its dispatch count.

## Reliability model

Execution and delivery have separate states:

- `succeeded`, `failed`, `canceled`, and `timed_out` describe execution.
- `pending`, `retry_wait`, `held`, and `orphaned` retain results while delivery waits or retries.
- `accepted_by_transport` means the original channel accepted the result and returned a correlated turn ID.
- `acknowledged_by_parent` means the original parent confirmed consumption with the correct nonce.
- `uncertain` stops automatic retransmission when the result may already have arrived.

Success requires a supported final event, exit code zero, no reported Agent error, and durable results. Worker processes are supervised with Windows Job Objects; cancellation and deadlines clean up that task's process tree. Workers cannot dispatch further tasks.

SQLite WAL, completion files, and a transactional outbox support recovery. A durable completion written before a database commit is recovered without re-execution. Interrupted execution without a reliable completion is not blindly rerun. An interrupted `sending` becomes `uncertain`; an accepted delivery keeps its receipt while waiting for acknowledgment.

Artifacts are stored as `artifacts/<task-id>/<attempt-id>/{stdout.jsonl,stderr.log,completion.json}` in the application data directory. Output is bounded and stdout/stderr are drained concurrently. Automatic history cleanup is not implemented yet.

## Compatibility

| Capability | Current status |
| --- | --- |
| Windows x64 daemon, CLI, and MCP stdio | Implemented and tested |
| Project integration, automatic registration, isolation, and idempotency | Covered by real-process integration tests |
| Probe worker | Executed in the live desktop smoke; diagnostic only |
| Codex exec / Claude print workers | Adapters and parsers implemented; successful real-model tasks still need validation |
| Codex Windows desktop delivery | Experimental; verified distribution only |
| Real parent-model consumption and acknowledgment | Not yet validated; previous model probes hit `usageLimitExceeded` |
| Claude parent inbox, other parent entrypoints, and dsh | Not enabled |
| macOS / Linux platform backends | Not implemented or validated |

The verified desktop baseline is `OpenAI.Codex 26.1103.4239.0`, with core `0.167.0-alpha.3.2`. The adapter checks the user's desktop process, project directory, exact owner, and a pinned `app.asar` hash. Unknown distributions are rejected. This is an internal desktop IPC integration, not a stable public desktop API.

Workers are read-only in this release. GUI, code-writing/worktree/merge workflows, startup registration, and interactive worker approval bridging are not implemented.

## Build and test

Source builds require Rust stable, the Windows MSVC target, and Visual Studio C++ Build Tools.

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --locked
```

There are 32 Rust tests covering transactions, automatic conversation isolation, permissions, credentials, named pipes, Job Objects, subprocess output, cancellation, timeouts, MCP stdio, uncertain delivery, and crash recovery. CI also runs eight PowerShell installation tests, including update rollback, archive validation, and a real installed-service round trip.

The opt-in desktop smoke requires Node.js and the verified running Windows desktop. It creates a fresh project and thread, executes Probe through the real daemon, checks persisted tool-output identity, and restarts its own daemon to verify the delivery receipt:

```powershell
node .\scripts\windows-desktop-smoke.cjs --codex-core "C:\path\to\desktop-core\codex.exe" --artifact-parent "C:\existing\temporary-directory"
```

The diagnostic client supplies the new thread's metadata. It does not simulate a real parent-model acknowledgment. Reports and temporary projects stay local.

To build a release package and test installation:

```powershell
cargo build --workspace --release --locked
.\scripts\package-windows.ps1 -Tag v0.1.0
.\scripts\test-distribution.ps1 -Tag v0.1.0
```

Main pushes and pull requests run Windows CI. Version tags trigger the same checks, followed by release builds, installation tests, checksums, and GitHub Release publication.

## License

[MIT](LICENSE). Copyright (c) 2026 mace.

Copies or substantial portions of the software must retain the original copyright notice and MIT permission notice. Binary packages also include third-party license notices.
