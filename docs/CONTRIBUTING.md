# Contributing to Runspur

**English** · [简体中文](#参与贡献)

## Report an issue

Search for an existing report first. For a bug, include the Runspur, Windows, and Agent versions, the smallest set of steps that reproduces it, and the expected and actual behavior. Remove credentials and private project content from logs before sharing them. [Issues](https://github.com/ohmacemace/runspur/issues)

## Open a pull request

1. For a new Agent integration or a protocol change, open an issue to discuss the proposal first. Small fixes and documentation improvements can go directly to a pull request.
2. Fork the repository and work on a branch in your fork. Keep the pull request focused on one change.
3. Describe the problem, link the related issue when there is one, and include the checks you ran. [Pull requests](https://github.com/ohmacemace/runspur/pulls)

## Build from source and run checks

Use Windows x64 with Rust stable, the MSVC target, Visual Studio C++ Build Tools, Node.js 24, and WebView2. Run these checks from the repository root.

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

To run the Windows app from source, build the scheduler binaries and start the development app.

```powershell
cargo build --locked --release -p scheduler-app --bins
Set-Location apps/runspur-desktop
npm ci
npm run dev
```

The development app uses separate local scheduler data. Verify CLI and Windows app changes separately. For UI changes, check both English and Simplified Chinese.

## 参与贡献

[English](#contributing-to-runspur) · **简体中文**

### 报告问题

请先搜索已有报告。提交 bug 时，附上 Runspur、Windows 和 Agent 的版本、最小复现步骤，以及预期和实际行为。分享日志前请移除凭据和私有项目内容。[问题反馈](https://github.com/ohmacemace/runspur/issues)

### 提交 Pull Request

1. 新增 Agent 接入或修改协议时，请先开 Issue 讨论方案。小修复和文档改进可以直接提交 PR。
2. Fork 仓库，在自己的分支上开发。每个 PR 聚焦一项改动。
3. 说明要解决的问题，附上相关 Issue 和已运行的检查。[Pull Request](https://github.com/ohmacemace/runspur/pulls)

### 从源码运行与检查

在 Windows x64 上安装 Rust stable、MSVC 目标、Visual Studio C++ Build Tools、Node.js 24 和 WebView2。在仓库根目录执行以下检查。

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

要从源码运行 Windows 应用，先构建调度器，再启动开发版。

```powershell
cargo build --locked --release -p scheduler-app --bins
Set-Location apps/runspur-desktop
npm ci
npm run dev
```

开发版使用独立的本机调度数据。分别验证 CLI 和 Windows 应用的改动。改动界面时，检查英文和简体中文。
