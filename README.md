# Runspur

**English** · [简体中文](docs/README.zh-CN.md)

[![CI](https://github.com/ohmacemace/runspur/actions/workflows/ci.yml/badge.svg)](https://github.com/ohmacemace/runspur/actions/workflows/ci.yml)
[![npm](https://img.shields.io/npm/v/runspur)](https://www.npmjs.com/package/runspur)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Runspur lets Codex and Claude Code work together on a project. Your main Agent can delegate a task in the background, keep working, and receive the result in the conversation that started it.

## Install

Runspur is available for **Windows x64**. Choose either client. Both manage projects and dispatch history through the same local scheduler.

### Windows app

Download and run the Windows x64 setup executable, then open **Runspur**. The app starts the scheduler automatically. [Download the Windows installer](https://github.com/ohmacemace/runspur/releases/latest)

### CLI

Install Node.js and npm, then run these commands.

```powershell
npm install --global runspur
runspur tui
```

`runspur tui` opens the workspace in your default browser. Use `runspur start` and `runspur stop` to control the scheduler from a terminal.

## Connect a project

1. Install the Agent clients you plan to use. Workers use their provider's CLI.
2. In Runspur, choose **Projects → Add project** and select a folder. Choose a main Agent and a worker from the other provider, set the worker's permissions, then preview and confirm the integration.
3. Open a new conversation in your main Agent from the project directory. Give it a task with work it can delegate. If Claude Code asks to enable the project's Runspur MCP server, approve it in Claude Code.

Open **Dispatch history** to see who worked on a task, what they returned, and whether the result reached the original conversation. To change Agents or permissions, select the project and choose **Edit configuration**.

Workers start with read-only access. You can allow writes to files named in the task or text edits across the project. Write tasks produce changes in a separate workspace for the main Agent to review and apply.

## Agent compatibility

| Agent | Main Agent | Worker |
| --- | --- | --- |
| Codex | Windows desktop, experimental. Verified with `OpenAI.Codex 26.1103.4239.0` (core `0.167.0-alpha.3.2`). | Codex CLI, read-only |
| Claude Code | CLI 2.1.319 or later | CLI 2.1.319 or later for read-only. Use 2.1.319 for writes. |

Claude Desktop is untested.

## Contributing

Bug reports, fixes, and documentation improvements are welcome. [Contribution and source build guide](docs/CONTRIBUTING.md) · [Report an issue](https://github.com/ohmacemace/runspur/issues)

## License

[MIT](LICENSE)
