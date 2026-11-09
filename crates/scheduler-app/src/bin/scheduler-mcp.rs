use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use rmcp::ServiceExt;
use scheduler_app::mcp::{McpHost, ProjectMcp};
use scheduler_platform::DataPaths;
use tokio::io::AsyncReadExt;

mod scoped_file_mcp;

#[derive(Parser)]
#[command(version, about = "Project-scoped scheduler MCP stdio bridge")]
struct Cli {
    #[arg(long, default_value = ".")]
    project_root: PathBuf,
    #[arg(long, env = "RUNSPUR_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "codex")]
    host: Host,
    #[arg(long, hide = true)]
    worker_file_broker: Option<PathBuf>,
    #[arg(long, hide = true)]
    claude_origin_hook: bool,
}

#[derive(Clone, ValueEnum)]
enum Host {
    Codex,
    ClaudeCode,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    if let Some(ticket) = cli.worker_file_broker {
        return scoped_file_mcp::serve(&ticket).await;
    }
    let paths = DataPaths::open(&scheduler_app::setup::data_root(cli.data_dir)?)?;
    if cli.claude_origin_hook {
        anyhow::ensure!(
            matches!(cli.host, Host::ClaudeCode),
            "This hook requires the Claude Code host"
        );
        let mut input = Vec::new();
        tokio::io::stdin()
            .take((scheduler_core::MAX_FRAME_BYTES + 1) as u64)
            .read_to_end(&mut input)
            .await?;
        anyhow::ensure!(
            input.len() <= scheduler_core::MAX_FRAME_BYTES,
            "Claude Code hook input is too large"
        );
        return scheduler_app::mcp::register_native_claude_hook(&cli.project_root, paths, &input)
            .await;
    }
    let host = match cli.host {
        Host::Codex => McpHost::Codex,
        Host::ClaudeCode => McpHost::claude()?,
    };
    let server = ProjectMcp::for_host(&cli.project_root, paths, host).await?;
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
