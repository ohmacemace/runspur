use std::path::PathBuf;

use clap::Parser;
use scheduler_core::{AgentKind, AgentProfile};
use scheduler_platform::{DataPaths, LocalListener};
use scheduler_runtime::{Scheduler, serve};

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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "scheduler_runtime=info,schedulerd=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let paths = DataPaths::prepare(&scheduler_app::setup::data_root(cli.data_dir)?)?;
    // Acquire the OS endpoint before any database recovery or worker launch.
    let listener = LocalListener::bind(&paths.endpoint()?)?;
    let scheduler = Scheduler::new(paths.clone(), cli.max_workers)?;
    let root_profile = AgentProfile {
        profile_id: "codex-desktop".into(),
        kind: AgentKind::CodexDesktop,
        program: None,
        model: None,
        reasoning_effort: None,
        environment_keys: Default::default(),
    };
    if scheduler
        .store
        .profile(&root_profile.profile_id)
        .await
        .is_err()
    {
        scheduler.store.put_profile(root_profile).await?;
    }
    scheduler.recover().await?;
    let background = scheduler.start();
    tracing::info!(data_dir=%paths.root.display(), "Scheduler is accepting project-scoped requests");
    let service = serve(scheduler.clone(), listener);
    tokio::pin!(service);
    let result = tokio::select! {
        result = &mut service => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
    };
    scheduler.shutdown().await;
    for job in background {
        let _ = job.await;
    }
    result
}
