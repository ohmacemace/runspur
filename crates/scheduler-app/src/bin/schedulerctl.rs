use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use scheduler_app::setup::{ProjectRoles, prepare_project_configuration};
use scheduler_core::*;
use serde_json::json;
use std::path::PathBuf;

#[path = "schedulerctl/wizard.rs"]
mod wizard;

#[derive(Parser)]
#[command(
    bin_name = "runspur",
    version,
    about = "Project-scoped Agent scheduler management"
)]
struct Cli {
    #[arg(long, global = true, env = "RUNSPUR_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Set up a project interactively.
    New {
        #[arg(long)]
        project_root: Option<PathBuf>,
    },
    /// Change an existing project's Agent and permission settings interactively.
    Edit {
        #[arg(long)]
        project_root: Option<PathBuf>,
    },
    /// Start the Runspur scheduler in the foreground.
    Start {
        #[arg(long, default_value_t = 2)]
        max_workers: usize,
    },
    /// Stop the shared scheduler before updating or uninstalling Runspur.
    Stop,
    /// Preview or save project settings and update the selected main Agent's integration.
    Configure {
        #[arg(long)]
        project_root: PathBuf,
        #[arg(long)]
        name: Option<String>,
        /// Main Agent product (codex or claude-code), or a configured profile ID.
        #[arg(long)]
        main_agent: Option<String>,
        /// Comma-separated worker profile IDs. Discover profiles with the profiles command.
        #[arg(long, value_delimiter = ',')]
        workers: Option<Vec<String>>,
        #[arg(long, value_enum)]
        permission: Option<Permission>,
        /// Native MCP executable. Defaults to the bridge bundled alongside this CLI.
        #[arg(long)]
        mcp_program: Option<PathBuf>,
        /// Save the configuration and its integration files. Omit to preview.
        #[arg(long)]
        apply: bool,
    },
    /// Initialize or import the versioned project policy; never select a fixed parent conversation.
    #[command(hide = true)]
    Init {
        #[arg(long, default_value = ".")]
        project_root: PathBuf,
        #[arg(long)]
        name: Option<String>,
        /// Main Agent product, or a configured profile ID.
        #[arg(long)]
        main_agent: Option<String>,
        #[arg(long, value_delimiter = ',')]
        workers: Option<Vec<String>>,
        /// Enable child Agents to write explicitly scoped files in isolated task workspaces.
        #[arg(long)]
        allow_workspace_write: bool,
        /// Allow child Agents to edit text files throughout the isolated project workspace.
        #[arg(long, conflicts_with = "allow_workspace_write")]
        allow_project_write: bool,
    },
    /// Install this project's Skill and MCP namespace, preserving unrelated configuration.
    #[command(hide = true)]
    Integrate {
        #[arg(long, default_value = ".")]
        project_root: PathBuf,
        #[arg(long, default_value = "scheduler-mcp")]
        mcp_program: String,
    },
    /// Open a Runspur-managed conversation with the project's Claude Code main agent.
    #[command(hide = true)]
    Claude {
        #[arg(long, default_value = ".")]
        project_root: PathBuf,
        /// Resume an original Runspur-managed native session for this project.
        #[arg(long)]
        resume: Option<uuid::Uuid>,
    },
    #[command(hide = true)]
    ProfilePut {
        #[arg(long)]
        file: PathBuf,
    },
    /// List registered projects.
    Projects,
    /// List conversations with dispatches in a project.
    Sessions {
        #[arg(long)]
        project: String,
        #[arg(long)]
        before: Option<i64>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// List dispatches, optionally filtered by project and conversation.
    Tasks {
        #[arg(long)]
        project: Option<String>,
        /// Runspur origin ID from the sessions command.
        #[arg(long, requires = "project", conflicts_with = "session")]
        origin: Option<String>,
        /// Legacy host session ID. Use --origin if the host ID is ambiguous.
        #[arg(long, requires = "project")]
        session: Option<String>,
        #[arg(long)]
        before: Option<i64>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Show a dispatch, including its original conversation, result, and delivery.
    Task {
        task_id: String,
    },
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

#[derive(Clone, Copy, ValueEnum)]
enum Permission {
    ReadOnly,
    WorkspaceWrite,
    ProjectWrite,
}

async fn bridge_program(override_path: Option<PathBuf>) -> Result<PathBuf> {
    let path = match override_path {
        Some(path) => path,
        None => std::env::current_exe()?.with_file_name(if cfg!(windows) {
            "scheduler-mcp.exe"
        } else {
            "scheduler-mcp"
        }),
    };
    ensure!(
        path.is_file(),
        "MCP executable not found: {}",
        path.display()
    );
    Ok(tokio::fs::canonicalize(path).await?)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Start { max_workers } => {
            return scheduler_app::daemon::run(cli.data_dir, max_workers).await;
        }
        Command::ProbeWorker => std::process::exit(scheduler_app::probe::run_probe().await?),
        _ => {}
    }
    let root = scheduler_app::setup::data_root(cli.data_dir.clone())?;
    if matches!(cli.command, Command::Stop) {
        scheduler_app::daemon::stop(&root).await?;
        println!("Runspur scheduler stopped.");
        return Ok(());
    }
    let client = scheduler_app::daemon::connect(&root).await?;
    let wizard_mode = if matches!(cli.command, Command::Edit { .. }) {
        wizard::Mode::Edit
    } else {
        wizard::Mode::New
    };
    let result = match cli.command {
        Command::New { project_root } | Command::Edit { project_root } => {
            let bridge = bridge_program(None).await?;
            let bridge = bridge
                .to_str()
                .context("MCP executable path is not valid Unicode")?;
            return wizard::run(&client, bridge, project_root, wizard_mode).await;
        }
        Command::Configure {
            project_root,
            name,
            main_agent,
            workers,
            permission,
            mcp_program,
            apply,
        } => {
            let bridge = bridge_program(mcp_program).await?;
            let bridge = bridge
                .to_str()
                .context("MCP executable path is not valid Unicode")?;
            scheduler_app::setup::seed_profiles(&client).await?;
            let root = tokio::fs::canonicalize(project_root)
                .await
                .context("Cannot open project folder")?;
            let plan = prepare_project_configuration(
                &client,
                &root,
                |current| {
                    let new_project = current.main_agent_profile_id.is_empty();
                    let (allow_workspace_write, allow_project_write) = match permission {
                        None => (
                            current.limits.allow_workspace_write,
                            current.limits.allow_project_write,
                        ),
                        Some(Permission::ReadOnly) => (false, false),
                        Some(Permission::WorkspaceWrite) => (true, false),
                        Some(Permission::ProjectWrite) => (false, true),
                    };
                    ProjectRoles {
                        name: name.unwrap_or_else(|| {
                            if new_project {
                                root.file_name()
                                    .and_then(|name| name.to_str())
                                    .unwrap_or("Runspur project")
                                    .into()
                            } else {
                                current.name.clone()
                            }
                        }),
                        main_agent_profile_id: match main_agent.as_deref() {
                            Some("codex") => "codex-desktop".into(),
                            Some("claude" | "claude-code") => "claude-cli".into(),
                            Some(profile) => profile.into(),
                            None if new_project => "codex-desktop".into(),
                            None => current.main_agent_profile_id.clone(),
                        },
                        worker_profile_ids: workers.map_or_else(
                            || current.allowed_worker_profile_ids.clone(),
                            |ids| ids.into_iter().collect(),
                        ),
                        allow_workspace_write,
                        allow_project_write,
                    }
                },
                bridge,
                &client.paths.root,
            )
            .await?;
            if apply {
                json!({"state": "saved_requires_host_reload", "project": plan.apply(&client).await?})
            } else {
                json!({"state": "preview", "configuration": plan.config(), "integration": plan.preview()})
            }
        }
        Command::Init {
            project_root,
            name,
            main_agent,
            workers,
            allow_workspace_write,
            allow_project_write,
        } => {
            scheduler_app::setup::seed_profiles(&client).await?;
            let permission = if allow_project_write {
                ExecutionMode::ProjectWrite
            } else if allow_workspace_write {
                ExecutionMode::WorkspaceWrite
            } else {
                ExecutionMode::ReadOnly
            };
            json!(
                scheduler_app::setup::initialize_project(
                    &client,
                    &project_root,
                    name,
                    main_agent,
                    workers,
                    permission,
                )
                .await?
            )
        }
        Command::Integrate {
            project_root,
            mcp_program,
        } => {
            let (root, config) = scheduler_app::setup::read_project(&project_root).await?;
            let profiles: Vec<AgentProfile> = match client.call(Operation::ListProfiles).await {
                Ok(value) => serde_json::from_value(value)?,
                Err(error)
                    if config.main_agent_profile_id == "codex-desktop"
                        && error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|source| {
                                source.kind() == std::io::ErrorKind::NotFound
                            }) =>
                {
                    Vec::new()
                }
                Err(error) => return Err(error),
            };
            scheduler_app::setup::install_project(
                &root,
                &config,
                &mcp_program,
                cli.data_dir.as_ref().map(|_| client.paths.root.as_path()),
                &profiles,
            )
            .await?
        }
        Command::Claude {
            project_root,
            resume,
        } => {
            let (root, config) = scheduler_app::setup::read_project(&project_root).await?;
            let projects: Vec<ProjectRecord> =
                serde_json::from_value(client.call(Operation::ListProjects).await?)?;
            let revision = fingerprint(&config)?;
            anyhow::ensure!(
                projects.iter().any(|project| project.root == root
                    && project.config.project_id == config.project_id
                    && project.revision == revision),
                "The project manifest must match the active Runspur registration."
            );
            let profiles: Vec<AgentProfile> =
                serde_json::from_value(client.call(Operation::ListProfiles).await?)?;
            let profile = profiles.into_iter().find(|profile| {
                profile.profile_id == config.main_agent_profile_id
                    && profile.kind == AgentKind::ClaudePrint
                    && scheduler_app::setup::desktop_parent_profile(profile)
            });
            let program = profile.and_then(|profile| profile.program).ok_or_else(|| {
                anyhow::anyhow!("Select an installed Claude Code main agent in Runspur.")
            })?;
            scheduler_runtime::run_managed_claude(
                client.paths.clone(),
                root,
                config.project_id,
                &program,
                resume,
            )
            .await?;
            return Ok(());
        }
        Command::ProfilePut { file } => {
            let profile: AgentProfile = serde_json::from_slice(&tokio::fs::read(file).await?)?;
            ensure!(
                profile.kind != AgentKind::Probe,
                "Choose a Codex or Claude Code profile."
            );
            client.call(Operation::PutProfile(profile)).await?
        }
        Command::Projects => client.call(Operation::ListProjects).await?,
        Command::Sessions {
            project,
            before,
            limit,
        } => {
            client
                .call(Operation::ListSessions {
                    project_id: project,
                    before,
                    limit,
                })
                .await?
        }
        Command::Tasks {
            project,
            origin,
            session,
            before,
            limit,
        } => {
            let operation = if let Some(origin_id) = origin {
                Operation::OriginTaskHistory {
                    project_id: project
                        .ok_or_else(|| anyhow::anyhow!("--origin requires --project"))?,
                    origin_id,
                    before,
                    limit,
                }
            } else {
                Operation::TaskHistory {
                    project_id: project,
                    session_id: session,
                    before,
                    limit,
                }
            };
            client.call(operation).await?
        }
        Command::Task { task_id } => serde_json::to_value(client.task_detail(&task_id).await?)?,
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
                "parent_support":"Codex desktop: experimental, verified Windows distribution; Claude Code CLI: native inbox on 2.1.319 or later"})
        }
        Command::Start { .. } | Command::Stop | Command::ProbeWorker => unreachable!(),
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
