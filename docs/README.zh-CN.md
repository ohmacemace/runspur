# Runspur

[English](../README.md) · **简体中文**

[![CI](https://github.com/ohmacemace/runspur/actions/workflows/ci.yml/badge.svg)](https://github.com/ohmacemace/runspur/actions/workflows/ci.yml)
[![npm](https://img.shields.io/npm/v/runspur)](https://www.npmjs.com/package/runspur)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](../LICENSE)

Runspur 让 Codex 和 Claude Code 在同一个项目里协作。主 Agent 可以把一项工作交给子 Agent 在后台执行，自己继续处理其他工作，结果会回到发起任务的会话。

## 安装

桌面应用和 npm CLI 均支持 **Windows x64**，共用本机调度器、项目配置和调度历史。

### Windows 应用

下载并运行 Windows x64 MSI 安装包，然后打开 **Runspur**。应用会自动启动调度器。[下载 Windows 安装包](https://github.com/ohmacemace/runspur/releases/latest)

### CLI

安装 Node.js 和 npm 后，在终端运行以下命令。

```powershell
npm install --global runspur
runspur tui
```

`runspur tui` 在默认浏览器中打开工作台。可以在终端用 `runspur start` 和 `runspur stop` 启停调度器。

## 接入项目

1. 安装准备使用的 Agent 客户端。子 Agent 通过对应的 CLI 执行。
2. 在 Runspur 中打开**项目 → 添加项目**并选择目录。选择主 Agent 和另一厂商的子 Agent，设置子 Agent 权限，再预览并确认接入配置。
3. 在项目目录中用主 Agent 开一个新会话，交给它可以整项派发或并行处理的工作。如果 Claude Code 提示启用项目的 Runspur MCP 服务，在 Claude Code 中确认即可。

在**调度历史**里可以查看每项任务由谁执行、返回了什么、是否已回传到原会话。要调整 Agent 或权限，在**项目**中选择对应项目，再点**编辑配置**。

在**设置**中调整所有项目共用的任务并发上限和子 Agent 权限模式。超出并发上限的任务会排队。

- **按项目权限**是默认模式。项目默认只读，可允许子 Agent 在独立工作区写入指定文件或编辑项目内的文本文件。
- **完整权限**启用子 Agent 的原生 Shell 和文件工具，可以直接修改项目，也可以按当前系统账号的权限访问项目外文件。
- **跟随主 Agent**在确认主 Agent 为完整权限或只读时，使用对应模式。其他模式或无法读取当前权限时，使用项目权限。

已接入的项目可以在**编辑配置**中预览并保存，再开一个主 Agent 会话加载新版 Skill。

## Agent 兼容性

| Agent | 主 Agent | 子 Agent |
| --- | --- | --- |
| Codex | Windows 桌面版，实验性。已验证 `OpenAI.Codex 26.1103.4239.0`（核心 `0.167.0-alpha.3.2`）。 | Codex CLI，支持只读和完整权限 |
| Claude Code | CLI 2.1.319 或更新版本 | CLI 2.1.319 或更新版本。独立工作区写入使用 2.1.319。 |

Claude Desktop 尚未测试。

## 参与贡献

欢迎报告问题、提交修复和改进文档。[贡献与源码构建指南](CONTRIBUTING.md#参与贡献) · [问题反馈](https://github.com/ohmacemace/runspur/issues)

## 许可证

[MIT](../LICENSE)
