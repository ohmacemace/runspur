# Runspur CLI

Runspur lets Codex and Claude Code delegate work to each other and sends results back to the conversation that started the task. The Windows x64 npm package includes the Runspur CLI, scheduler, and MCP bridge.

## Install

With Node.js and npm installed, run the following in PowerShell.

```powershell
npm install --global runspur
runspur --version
```

If PowerShell blocks script shims, use `npm.cmd` and `runspur.cmd`.

## Connect a project

Run the setup wizard. Runspur starts the local scheduler in the background as needed.

```powershell
runspur new
```

Choose the project folder, main Agent, available child Agents, and file permissions. Review the settings and confirm to connect the project. Open a new conversation in the configured host to load the project's Skill and MCP tools.

Automatic result delivery is verified on the supported Codex desktop distribution and on native Claude Code CLI sessions. Claude Code requires version 2.1.319 or later. Claude Desktop is untested.

## Manage projects and dispatches

Run `runspur edit` and choose a connected project to change its Agents or permissions. Leave a selection blank to keep its current setting, then review and confirm the changes. Worker access defaults to read-only for new projects. A supported Claude Code worker can be authorized to edit task-specific files or text files across the isolated project workspace.

For scripts, pass the project folder explicitly. Omit `--apply` to preview. The write modes are `--permission workspace-write` and `--permission project-write`.

```powershell
runspur configure --project-root 'C:\path\your-project' --main-agent codex --workers claude-cli
```

Use `runspur projects`, `runspur sessions --project <project-id>`, and `runspur tasks --project <project-id>` to locate a dispatch. Use `runspur task <task-id>` to inspect its result and delivery status.

Before updating or removing the package, finish active tasks, close connected Agent conversations, and run `runspur stop`. Then run `npm update --global runspur` or `npm uninstall --global runspur`. Project settings and the Runspur state directory are stored separately.

[Full usage and source](https://github.com/ohmacemace/runspur) · [简体中文](https://github.com/ohmacemace/runspur/blob/main/docs/README.zh-CN.md)
