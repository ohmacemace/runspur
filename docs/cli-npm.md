# Runspur CLI

Runspur lets Codex and Claude Code delegate work to each other and sends results back to the conversation that started the task. The Windows x64 npm package includes the Runspur CLI, scheduler, MCP bridge, and local browser panel.

## Install

With Node.js and npm installed, run the following in PowerShell.

```powershell
npm install --global runspur
runspur --version
```

If PowerShell blocks script shims, use `npm.cmd` and `runspur.cmd`.

## Open the panel

Run the following to open the panel in your browser. Runspur starts the local scheduler as needed.

```powershell
runspur tui
```

In **Projects**, add an existing folder and select the main Agent, child Agents, and file permissions. Review the configuration and confirm to connect the project. Open a new conversation in the configured host to load the project's Skill and MCP tools.

Automatic result delivery is verified on the supported Codex desktop distribution and on native Claude Code CLI sessions. Claude Code requires version 2.1.319 or later. Claude Desktop is untested.

For headless use, run `runspur start` to start the scheduler in the background and `runspur stop` to stop it. Each command returns when the scheduler reaches the requested state.

## Manage projects and dispatches

Use **Projects** to edit the Agents or permissions of a connected project. New projects start read-only. Use Claude Code **2.1.319** for isolated workspace edits. To refresh an existing project's Runspur Skill, preview and save **Edit configuration**, then open a new conversation.

Use **Dispatch history** to find tasks by project and conversation, then inspect their results and delivery status.

Use **Settings** to set the global concurrency limit and child Agent access. Project permissions are the default. **Full access** enables native shell and file tools in the live project. **Follow main Agent** uses verified full or read-only access and falls back to project permissions for other or unavailable modes.

Before uninstalling, finish active tasks and close connected Agent conversations. Stop the scheduler from **Overview** or run `runspur stop`. If the panel is open, press Ctrl+C in the `runspur tui` terminal to close it. Then run `npm uninstall --global runspur`. Project settings and the Runspur state directory are stored separately.

[Full usage and source](https://github.com/ohmacemace/runspur) · [简体中文](https://github.com/ohmacemace/runspur/blob/main/docs/README.zh-CN.md)
