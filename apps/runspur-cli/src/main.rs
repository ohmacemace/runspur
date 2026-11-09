use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use scheduler_app::{daemon, setup};

mod panel;

#[derive(Parser)]
#[command(version, about = "Runspur project collaboration")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the shared scheduler in the background.
    Start {
        #[arg(long, env = "RUNSPUR_DATA_DIR", hide = true)]
        data_dir: Option<PathBuf>,
    },
    /// Stop the shared scheduler.
    Stop {
        #[arg(long, env = "RUNSPUR_DATA_DIR", hide = true)]
        data_dir: Option<PathBuf>,
    },
    /// Open the Runspur project workspace in your browser.
    Tui {
        #[arg(long, env = "RUNSPUR_DATA_DIR", hide = true)]
        data_dir: Option<PathBuf>,
        #[arg(long, hide = true)]
        no_open: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let executable = std::env::current_exe().context("Cannot locate the Runspur executable")?;
    let native = executable
        .parent()
        .context("Cannot locate the Runspur components")?;
    match cli.command {
        Command::Start { data_dir } => {
            let scheduler = bundled_component(native, "schedulerd")?;
            daemon::connect_bundled(&setup::data_root(data_dir)?, &scheduler).await?;
            println!("Runspur scheduler is ready.");
            Ok(())
        }
        Command::Stop { data_dir } => {
            daemon::stop(&setup::data_root(data_dir)?).await?;
            println!("Runspur scheduler stopped.");
            Ok(())
        }
        Command::Tui { data_dir, no_open } => {
            panel::run(native, setup::data_root(data_dir)?, no_open).await
        }
    }
}

fn bundled_component(native: &Path, name: &str) -> Result<PathBuf> {
    let path = native.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    std::fs::canonicalize(&path)
        .with_context(|| format!("Cannot locate the Runspur component {}", path.display()))
}
