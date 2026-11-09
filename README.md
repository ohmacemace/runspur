# Runspur

**English** · [简体中文](docs/README.zh-CN.md)

[![CI](https://github.com/ohmacemace/runspur/actions/workflows/ci.yml/badge.svg)](https://github.com/ohmacemace/runspur/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Runspur coordinates work between Codex and Claude Code. The main Agent can delegate a task and keep working while a child Agent runs in the background. Runspur delivers the result to the conversation that started the task.

- Choose a main Agent, available workers, and their file permissions per project.
- Inspect each dispatch, its result, and its delivery status by project or conversation.
- Use the CLI or the Windows app to connect projects and manage their configuration.

Runspur supports **Windows x64**. Claude Code **2.1.319 or later** is required for either Agent role. Automatic delivery has been verified with a specific Codex desktop distribution and native Claude Code CLI sessions. Claude Desktop is untested. [Compatibility details](#compatibility)

## Quick start

### CLI via npm

The CLI is distributed through npm as `runspur`. Version 0.1.2 supports Windows x64. With Node.js and npm installed, run the following in PowerShell. [Runspur on npm](https://www.npmjs.com/package/runspur)

```powershell
npm install --global runspur
runspur new
```

The setup wizard asks for the project folder, name, main Agent, child Agents, and file permissions. It starts the shared scheduler in the background when needed. Review the project integration and confirm to save. Install the Agent CLIs you plan to use before running the wizard.

Run `runspur new` again to connect another project. Use `runspur edit` to change an existing project's configuration. Run `runspur --help` for commands. If PowerShell blocks the npm-generated script shims, use `npm.cmd` and `runspur.cmd`.

```powershell
runspur projects
runspur edit
```

Use `runspur stop` before updating with `npm update --global runspur` or uninstalling with `npm uninstall --global runspur`. Project settings and task history are stored separately from the npm installation.

#### Delegate a task

For a Codex main agent, open the project in the verified Codex desktop host. For a Claude Code main agent on 2.1.319 or later, save the project integration and run `claude` in the project folder. Approve the project's Runspur MCP server if Claude Code asks. Runspur uses a project-local hook and the originating session's local inbox. Your Claude Code inbound messaging settings still apply.

The main Agent uses the Runspur Skill and MCP tools to select an available worker and delegate tasks. For example, Codex can send a test failure investigation to Claude Code while it continues an independent change. The findings return to the original Codex conversation. To resume a native Claude Code conversation, run `claude --resume <session-id>` in the project folder.

Both clients preview the previous adapter's Runspur entries before switching main Agents. Saving preserves project identity and history. Finish pending tasks and acknowledgments before switching.

### Windows app

The Windows desktop app uses a separate NSIS installer. Build it from source and open Runspur. It starts the scheduler automatically. Go to **Projects → Add project**, choose a folder, select the main Agent, workers, and permissions, then review and confirm the changes. Open a new conversation in the selected Agent from that folder. [Windows installer build instructions](#build-the-windows-desktop-app)

Use **Settings → Language** to follow the system language or select English or Simplified Chinese.

## Project configuration

The project stores its Agent choices and permission policy in `.agent-scheduler/project.json`. Runspur previews project integration changes and preserves unrelated host settings. Select child Agents from a different provider than the main Agent. Child Agents run through their provider's CLI.

Use `runspur edit` to select and change a project interactively. For automation, `runspur configure --project-root <folder>` previews changes to the specified project and preserves any fields you omit. Add `--apply` to save. `runspur configure --help` lists the available flags.

### Choose worker permissions

New projects start with read-only workers. The two write modes use a verified Claude Code 2.1.319 worker and a separate task workspace.

| Mode | CLI project setting | What the worker can do |
| --- | --- | --- |
| `read_only` | `--permission read-only` (default) | Inspect the project |
| `workspace_write` | `--permission workspace-write` | Create or edit files explicitly named in the task's `write_scope` |
| `project_write` | `--permission project-write` | Create or edit UTF-8 text files across the project using the file tools. The task omits `write_scope` |

Project-wide authorization also allows tasks with a narrower `write_scope`. The main Agent chooses the execution mode for each task from the modes exposed by `scheduler_agents`.

Both write modes save changes to the isolated workspace and record them in `changes.json`. The file tools enforce the project boundary, and the main Agent reviews and applies authorized changes to the original project.

## Tools and commands

| MCP tool | Purpose |
| --- | --- |
| `scheduler_context` | Effective project policy and automatically resolved origin |
| `scheduler_agents` | Profiles available to the project |
| `scheduler_submit` | Persist and enqueue a task |
| `scheduler_task` / `scheduler_tasks` | Inspect this conversation's tasks |
| `scheduler_cancel` | Request cancellation |
| `scheduler_claim` | Retrieve the result named in a Claude Code native inbox notification, using `delivery_id` in the originating conversation |
| `scheduler_ack` | Confirm consumption using `delivery_id` and `ack_nonce` |

Local operator commands:

```powershell
runspur projects
runspur sessions --project <project-id>
runspur tasks --project <project-id> --origin <origin-id>
runspur task <task-id>
runspur status
runspur status <task-id>
runspur cancel <task-id>
```

`sessions` lists conversations with dispatches in the project. Pass its `origin_id` to `tasks --origin` to filter dispatches by their original conversation. `--session` accepts legacy host session IDs when they identify a single origin in the project. Pass `next_cursor` to `--before` to read the next page with the same filters. `task` returns the prompt, original conversation, result or failure, and delivery details. The CLI and Windows app use the same operator queries.

Operator access permits inspection and cancellation across conversations, but cannot impersonate a parent's consumption acknowledgment. `scheduler-mcp` uses stdio; its stdout is reserved for MCP messages.

The defaults allow two concurrent workers globally and per project, with a one-hour task timeout. The project manifest controls project limits. To set a different global concurrency limit for a foreground scheduler, use `runspur start --max-workers`.

## Reliability model

Execution and delivery have separate states:

- `succeeded`, `failed`, `canceled`, and `timed_out` describe execution.
- `pending`, `retry_wait`, `held`, and `orphaned` retain results while delivery waits or retries.
- `accepted_by_transport` means the original channel accepted the result. The record can include a host receipt such as a turn ID.
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
| Codex exec / Claude print workers | Both support read-only tasks. Both write modes use Claude Code 2.1.319 |
| Codex Windows desktop delivery | Experimental; verified distribution only |
| Claude Code CLI as main agent | Project-local native inbox integration on 2.1.319 or later |
| Claude Desktop | Untested |
| Parent-model consumption and acknowledgment | Verified with Codex desktop and native Claude Code |
| macOS / Linux platform backends | Not implemented or validated |

The verified desktop baseline is `OpenAI.Codex 26.1103.4239.0`, with core `0.167.0-alpha.3.2`. The adapter checks the user's desktop process, project directory, exact owner, and a pinned `app.asar` hash. Unknown distributions are rejected. This is an internal desktop IPC integration, not a stable public desktop API.

## Build and test

Source builds require Rust stable, the Windows MSVC target, and Visual Studio C++ Build Tools.

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --locked
```

Rust tests cover transactions, conversation isolation, permissions, credentials, subprocess supervision, MCP stdio, dispatch history, delivery, and crash recovery. Windows CI also validates the npm tarball through an offline installation and runs the desktop UI build and desktop Rust tests.

The opt-in desktop origin check requires Node.js and the verified running Codex desktop. It creates a temporary project and conversation, then checks that an independent MCP client cannot impersonate that conversation using its thread ID.

```powershell
node .\scripts\windows-desktop-smoke.cjs --codex-core "C:\path\to\desktop-core\codex.exe" --artifact-parent "C:\existing\temporary-directory"
```

Use the executable bundled with Codex desktop for `--codex-core` and an existing directory for `--artifact-parent`. The test writes a local report. To check live delegation and acknowledgment, run a task from the verified Codex conversation itself.

### Package the CLI for npm

Build on Windows x64, then use the version shown in the generated filename to test the tarball.

```powershell
cargo build --locked --release -p scheduler-app --bins
.\scripts\package-npm.ps1
.\scripts\test-npm-distribution.ps1 -PackagePath .\dist\runspur-<version>.tgz
```

The package version comes from Cargo. A maintainer publishes the verified tarball with `npm.cmd publish` and completes npm account verification. The same tarball supports offline installation with `npm.cmd install --global`.

### Build the Windows desktop app

Runspur Desktop ships with its scheduler. The installed app uses the same default scheduler data directory as the CLI, `%LOCALAPPDATA%\Runspur\Runspur\data`, and connects to the running scheduler when available. Build the x64 Windows installer with Rust, MSVC Build Tools, Node.js, npm, and WebView2.

```powershell
cargo build --locked --release -p scheduler-app --bins
Set-Location apps/runspur-desktop
npm ci
npm run package:windows
```

The installer is generated under `src-tauri/target/release/bundle/nsis/`. Use `npm run dev` to run a development window with its own scheduler state in `%LOCALAPPDATA%\Runspur\state\desktop-dev`.

## Contributing

Bug reports, code fixes, and documentation improvements are welcome. The guide covers issue reports, pull requests, and local verification. [Contributing guide](docs/CONTRIBUTING.md)

## License

[MIT](LICENSE). Copyright (c) 2026 mace.

Copies or substantial portions of the software must retain the original copyright notice and MIT permission notice. Binary packages also include third-party license notices.
