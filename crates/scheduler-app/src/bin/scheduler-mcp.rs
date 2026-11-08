use std::path::PathBuf;

use clap::Parser;
use rmcp::ServiceExt;
use scheduler_platform::DataPaths;

#[derive(Parser)]
#[command(version, about = "Project-scoped scheduler MCP stdio bridge")]
struct Cli {
    #[arg(long, default_value = ".")]
    project_root: PathBuf,
    #[arg(long, env = "RUNSPUR_DATA_DIR")]
    data_dir: Option<PathBuf>,
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
    let paths = DataPaths::open(&scheduler_app::setup::data_root(cli.data_dir)?)?;
    let server = scheduler_app::mcp::ProjectMcp::new(&cli.project_root, paths).await?;
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
