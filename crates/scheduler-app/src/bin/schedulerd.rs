use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(version, about = "Runspur per-user asynchronous Agent scheduler")]
struct Cli {
    #[arg(long, env = "RUNSPUR_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 2)]
    max_workers: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    scheduler_app::daemon::run(cli.data_dir, cli.max_workers).await
}
