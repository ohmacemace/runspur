use clap::{Parser, Subcommand};
use scheduler_core::*;
use scheduler_platform::{DataPaths, LocalClient};
use serde_json::json;
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "Project-scoped Agent scheduler management")]
struct Cli {
    #[arg(long, global = true, env = "RUNSPUR_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Initialize or import the versioned project policy; never select a fixed parent conversation.
    Init {
        #[arg(long, default_value = ".")]
        project_root: PathBuf,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        main_agent: Option<String>,
        #[arg(long, value_delimiter = ',')]
        workers: Option<Vec<String>>,
    },
    /// Install this project's Skill and MCP namespace, preserving unrelated configuration.
    Integrate {
        #[arg(long, default_value = ".")]
        project_root: PathBuf,
        #[arg(long, default_value = "scheduler-mcp")]
        mcp_program: String,
    },
    /// Discover missing default profiles and list local executable configuration.
    Profiles,
    /// Import one trusted local AgentProfile JSON file.
    ProfilePut {
        #[arg(long)]
        file: PathBuf,
    },
    /// List registered projects.
    Projects,
    /// Show a task or recent tasks as the local operator.
    Status {
        task_id: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    Cancel {
        task_id: String,
    },
    /// Check the local service and project setup without invoking a model.
    Doctor {
        #[arg(long)]
        project_root: Option<PathBuf>,
    },
    #[command(hide = true)]
    ProbeWorker,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if matches!(cli.command, Command::ProbeWorker) {
        std::process::exit(scheduler_app::probe::run_probe().await?);
    }
    let root = scheduler_app::setup::data_root(cli.data_dir.clone())?;
    let paths = DataPaths::open(&root)?;
    let client = LocalClient {
        paths,
        credential: CredentialRef::Admin,
        origin: None,
    };
    let result = match cli.command {
        Command::Init {
            project_root,
            name,
            main_agent,
            workers,
        } => {
            scheduler_app::setup::seed_profiles(&client, std::env::current_exe()?).await?;
            json!(
                scheduler_app::setup::initialize_project(
                    &client,
                    &project_root,
                    name,
                    main_agent,
                    workers
                )
                .await?
            )
        }
        Command::Integrate {
            project_root,
            mcp_program,
        } => {
            let (root, config) = scheduler_app::setup::read_project(&project_root).await?;
            scheduler_app::setup::install_project(
                &root,
                &config,
                &mcp_program,
                cli.data_dir.as_deref(),
            )
            .await?
        }
        Command::Profiles => {
            json!(scheduler_app::setup::seed_profiles(&client, std::env::current_exe()?).await?)
        }
        Command::ProfilePut { file } => {
            let profile: AgentProfile = serde_json::from_slice(&tokio::fs::read(file).await?)?;
            client.call(Operation::PutProfile(profile)).await?
        }
        Command::Projects => client.call(Operation::ListProjects).await?,
        Command::Status {
            task_id: Some(task_id),
            ..
        } => client.call(Operation::GetTask { task_id }).await?,
        Command::Status { limit, .. } => client.call(Operation::ListTasks { limit }).await?,
        Command::Cancel { task_id } => client.call(Operation::CancelTask { task_id }).await?,
        Command::Doctor { project_root } => {
            let health = client.call(Operation::Health).await?;
            let project = if let Some(root) = project_root {
                Some(scheduler_app::setup::read_project(&root).await?.1)
            } else {
                None
            };
            json!({"service":health,"project":project,"origin_resolution":"host metadata is read on each MCP tools/call",
                "parent_support":"Codex desktop: experimental, fixed Windows distribution; Claude inbox: not enabled"})
        }
        Command::ProbeWorker => unreachable!(),
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
