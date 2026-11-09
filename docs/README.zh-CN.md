# Runspur

[English](../README.md) · **简体中文**

[![CI](https://github.com/ohmacemace/runspur/actions/workflows/ci.yml/badge.svg)](https://github.com/ohmacemace/runspur/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](../LICENSE)

Runspur 协调 Codex 和 Claude Code 之间的工作。主 Agent 派发任务后可以继续手头的工作，子 Agent 在后台执行，结果由 Runspur 回传到发起任务的会话。

- 按项目选择主 Agent、可用的子 Agent 和文件权限。
- 按项目或会话查看每次派发的内容、结果和回传状态。
- 通过 CLI 或 Windows 应用接入项目并管理配置。

Runspur 支持 **Windows x64**。Claude Code 担任主 Agent 或子 Agent 时，要求 **2.1.319 或更新版本**。Claude Code 子 Agent 需要写入权限时，请使用 **2.1.319**。自动回传已在指定的 Codex 桌面版和 Claude Code CLI 原生会话中验证。Claude Desktop 尚未测试。[兼容性详情](#兼容性)

## 快速开始

### 通过 npm 使用 CLI

CLI 通过 npm 以 `runspur` 包发行，0.1.2 版支持 Windows x64。安装 Node.js 和 npm 后，在 PowerShell 中执行以下命令。[npm 包页面](https://www.npmjs.com/package/runspur)

```powershell
npm install --global runspur
runspur tui
```

`runspur tui` 会在默认浏览器中打开本机 Runspur 面板，与 Windows 应用共用界面。先安装计划使用的 Agent CLI，再到**项目 → 添加项目**，选择目录、主 Agent、子 Agent 和权限。预览接入配置后确认保存。

修改已有项目时，在**项目**列表找到它，再点**编辑配置**。多个项目共用同一个调度器，需要时自动启动。如果 PowerShell 阻止执行 npm 生成的脚本，可以使用 `npm.cmd` 和 `runspur.cmd tui`。

只需要后台调度时，运行 `runspur start`。调度器就绪后，命令会返回终端。用 `runspur stop` 停止同一个调度器。

卸载前，等正在运行的任务结束，关闭已接入的 Agent 会话，再在**概览**或用 `runspur stop` 停止调度器。在面板所在终端按 Ctrl+C 关闭本机面板，之后执行 `npm uninstall --global runspur`。项目配置和任务历史保存在独立目录。

#### 派发任务

Codex 当主 Agent 时，在已验证的 Codex 桌面宿主里打开项目。Claude Code 当主 Agent 且版本为 2.1.319 或更新时，保存 Runspur 接入配置，再到项目目录直接运行 `claude`。如果 Claude Code 询问是否启用项目的 Runspur MCP，请在宿主中确认。回传经过 Claude Code 原会话的本地收件箱，仍受 Claude Code 接收消息的设置约束。

主 Agent 通过 Runspur 的 Skill 和 MCP 工具选择可用的子 Agent 并派发任务。例如，Codex 可以让 Claude Code 在后台调查一项测试失败，同时继续处理独立的改动。调查结果会回到发起任务的 Codex 会话。要继续 Claude Code 原生会话，在项目目录运行 `claude --resume <会话 ID>`。

两个客户端切换主 Agent 时，都会预览旧适配器要清理的 Runspur 条目。切换保留项目 ID 和调度历史。请先等旧任务完成并确认回传。

### Windows 应用

Windows 桌面应用使用独立的 NSIS 安装包。从源码构建安装包并打开 Runspur，调度器会自动启动。在**项目 → 添加项目**选择目录、主 Agent、子 Agent 和权限，预览后保存。再在选中的主 Agent 中打开该项目的新会话。[Windows 安装包构建步骤](#构建-windows-桌面应用)

在**设置 → 语言**中可以跟随系统语言，也可以选择英文或简体中文。

## 项目配置

每个项目的主 Agent、子 Agent 和权限保存在 `.agent-scheduler/project.json`。Runspur 会预览接入变更并保留宿主原有的其他设置。子 Agent 通过对应厂商的 CLI 执行，需与主 Agent 来自不同厂商。

在**项目**列表选择项目并编辑配置。保存前可以预览新的 Skill、MCP 配置和 Runspur 将清理的接入条目。

### 选择子 Agent 权限

项目默认只读。两种写入模式都使用已验证的 Claude Code 2.1.319 子 Agent，并在独立任务工作区保存改动。

| 模式 | 项目设置 | 子 Agent 可以做什么 |
| --- | --- | --- |
| `read_only` | 只读（默认） | 查看项目 |
| `workspace_write` | 指定文件写入 | 新建或修改任务 `write_scope` 列出的文件 |
| `project_write` | 项目内写入 | 通过文件工具在项目范围内新建或修改 UTF-8 文本文件，任务省略 `write_scope` |

项目范围的授权也允许主 Agent 派发指定文件的写入任务。主 Agent 根据每项任务需要，从 `scheduler_agents` 提供的模式中选择。

两种写入模式都生成改动文件和 `changes.json`。文件工具限制访问边界，主 Agent 审阅后在授权范围内将改动应用到原项目。

默认全局和每个项目各允许两个子 Agent 并发执行，任务超时为一小时。项目限制可在清单中调整。

## Agent 工具和派发历史

| MCP 工具 | 作用 |
| --- | --- |
| `scheduler_context` | 当前项目策略与自动解析的发起上下文 |
| `scheduler_agents` | 本项目可用的 Agent 配置 |
| `scheduler_submit` | 异步提交 |
| `scheduler_task` / `scheduler_tasks` | 本会话的任务详情／列表，用于明确查询或诊断 |
| `scheduler_cancel` | 请求取消本会话的任务 |
| `scheduler_claim` | Claude Code 原会话收到本地通知后，用 `delivery_id` 读取结果 |
| `scheduler_ack` | 用 `delivery_id` 和 `ack_nonce` 确认已消费的结果 |

在**调度历史**中选择项目和会话，查看原始指令、执行的子 Agent、结果或失败原因及回传状态。浏览器面板和 Windows 应用查询同一个本机调度器。

## 状态、恢复和产物

执行和投递是两个状态轴：

- 执行终态：`succeeded`、`failed`、`canceled`、`timed_out`。成功要求有效最终事件、退出 0、无 Agent 错误及结果落盘。
- `pending` / `retry_wait` / `held` / `orphaned`：结果保留，按通道状态等待或退避。
- `accepted_by_transport`：原通道已接受结果。记录可包含宿主返回的 turn ID 等收据。
- `acknowledged_by_parent`：原会话以正确 nonce 调用 ack；重复确认幂等。
- `uncertain`：可能已经送达，自动重发停止；先核对目标会话和实际结果。

worker 在 Windows Job Object 中监督，取消和超时清理该任务的进程树；worker 对本机控制面的再派发请求会被拒绝。`waiting_approval` 是协议预留执行状态，首版 CLI worker 不提供交互审批桥接。

后台使用 SQLite WAL、完成文件与事务 outbox。启动时恢复已落盘但未提交的完成结果；对无法确定已执行情况的任务记录失败／`recovery_required`，不盲目重跑。中断的 `sending` 转为 `uncertain`，已 `accepted_by_transport` 的结果保持原投递记录。

状态目录包含：

```text
scheduler.sqlite3
credentials/                          # Windows DPAPI 保护的凭据
artifacts/<task-id>/<attempt-id>/
    stdout.jsonl
    stderr.log
    completion.json
```

任务详情中的 `artifact_refs` 相对 `artifacts/`。stdout/stderr 并行读取，单行上限 1 MiB、每路日志上限 8 MiB，摘要有大小限制。当前尚无自动历史清理命令。

## 兼容性

| 能力 | 状态 |
| --- | --- |
| Windows x64 应用、CLI 和 MCP | 支持 |
| Codex Windows 桌面主动回传 | 实验性，已验证指定的分发版本 |
| Claude Code CLI 主会话回传 | 2.1.319 起可使用项目 Hook 和原生收件箱 |
| Claude Desktop | 尚未测试 |
| Codex 和 Claude Code 子 Agent | 均支持只读，两种写入模式已在 Claude Code 2.1.319 验证 |
| macOS / Linux | 尚未支持 |

Codex 桌面回传已验证 `OpenAI.Codex 26.1103.4239.0` 和核心 `0.167.0-alpha.3.2`。适配器使用桌面内部 IPC，核验桌面进程用户、工作目录、owner 和 `app.asar` 哈希。

## 开发和复核

源码构建需要 Rust stable、Windows MSVC 目标和 Visual Studio C++ Build Tools。共享界面使用 Node.js 24 和 npm 构建，与 Windows CI 一致。

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --locked
```

Rust 测试覆盖存储事务、会话隔离、权限、凭据、进程监管、MCP stdio、派发查询、结果回传和崩溃恢复。Windows CI 还会验证 npm 包的离线安装、桌面界面构建和桌面 Rust 测试。

桌面会话身份检查需要 Node.js 和正在运行的已验证 Codex 桌面版。脚本创建临时项目和会话，再验证独立的 MCP 客户端即使提交真实 thread ID，也无法冒充该会话。

```powershell
node .\scripts\windows-desktop-smoke.cjs --codex-core "C:\path\to\desktop-core\codex.exe" --artifact-parent "C:\existing\temporary-directory"
```

`--codex-core` 指向桌面版自带的核心，`--artifact-parent` 指向已存在的目录。脚本将报告保存在本地。验证实际派发和消费确认时，从已验证的 Codex 会话发起任务。

### 工程结构

```text
crates/scheduler-core/       模型、权限、协议、SQLite store
crates/scheduler-platform/   Windows IPC、用户身份、凭据、进程树与目录
crates/scheduler-runtime/    worker、父回传适配器、调度、outbox、恢复
crates/scheduler-app/        调度器、MCP、项目接入和进程测试
apps/runspur-cli/           npm 命令与本机网页服务
scripts/                    显式桌面冒烟
apps/runspur-desktop/       Windows 桌面应用
```

### 打包 CLI

在 Windows x64 上构建并打包 CLI。下面的命令使用当前发布版本。打包后续版本时，将脚本打印的压缩包路径传给测试脚本。

```powershell
cargo build --locked --release -p scheduler-app -p runspur-cli --bins
npm --prefix apps/runspur-desktop ci
npm --prefix apps/runspur-desktop run ui:build
.\scripts\package-npm.ps1
.\scripts\test-npm-distribution.ps1 -PackagePath .\dist\runspur-0.1.2.tgz
```

维护者用 `npm.cmd publish` 发布验证过的压缩包，并完成 npm 账号验证。同一份包也可以通过 `npm.cmd install --global` 离线安装。

### 构建 Windows 桌面应用

需要 Rust、MSVC Build Tools、Node.js 24、npm 和 WebView2。

```powershell
cargo build --locked --release -p scheduler-app --bins
Set-Location apps/runspur-desktop
npm ci
npm run package:windows
```

安装包生成在 `src-tauri/target/release/bundle/nsis/`。安装版与 CLI 共用默认数据目录 `%LOCALAPPDATA%\Runspur\Runspur\data`。同时使用两端时，请保持版本一致以复用运行中的调度器。用 `npm run dev` 启动的开发版使用独立的 `%LOCALAPPDATA%\Runspur\state\desktop-dev`。

## 参与贡献

欢迎报告问题、提交代码修复和改进文档。贡献指南包含 Issue、Pull Request 和本地验证流程。[贡献指南](CONTRIBUTING.md#参与贡献)

## 许可证与署名

[MIT](../LICENSE)，Copyright (c) 2026 mace。

允许商用、修改和再分发；复制或分发本项目的全部或实质性部分时，必须保留原版权声明及 MIT 许可证文本。二进制分发同时附带依赖的第三方许可声明。
