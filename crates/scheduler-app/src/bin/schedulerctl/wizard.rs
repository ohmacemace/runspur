use std::{
    collections::BTreeSet,
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use scheduler_app::setup::{
    ProjectRoles, desktop_parent_profile, desktop_worker_profile, prepare_project_configuration,
    read_project_at_root, seed_profiles,
};
use scheduler_core::{
    AgentKind, AgentProfile, ExecutionMode, Operation, ProjectConfig, ProjectRecord,
};
use scheduler_platform::LocalClient;

pub(super) enum Mode {
    New,
    Edit,
}

pub(super) async fn run(
    client: &LocalClient,
    bridge: &str,
    project_root: Option<PathBuf>,
    mode: Mode,
) -> Result<()> {
    let root = match project_root {
        Some(root) => root,
        None => {
            let current = std::env::current_dir()?;
            let folder = answer(&format!("Project folder [{}]", current.display()), None)?;
            if folder.is_empty() {
                current
            } else {
                PathBuf::from(folder.trim_matches('"'))
            }
        }
    };
    let root = tokio::fs::canonicalize(root)
        .await
        .context("Cannot open project folder")?;
    let registered: Vec<ProjectRecord> =
        serde_json::from_value(client.call(Operation::ListProjects).await?)?;
    let config = read_project_at_root(&root).await?.or_else(|| {
        registered
            .into_iter()
            .find(|record| record.root == root)
            .map(|record| record.config)
    });
    match mode {
        Mode::New => ensure!(
            config.is_none(),
            "A Runspur project already exists here. Use `runspur edit --project-root <folder>` to edit it."
        ),
        Mode::Edit => ensure!(
            config.is_some(),
            "No Runspur project exists here. Use `runspur new --project-root <folder>` to connect it."
        ),
    }

    let profiles = seed_profiles(client).await?;
    let roles = project_roles(&root, config.as_ref(), &profiles)?;
    let mut config_changed = false;
    let plan = prepare_project_configuration(
        client,
        &root,
        |current| {
            let current = (!current.main_agent_profile_id.is_empty()).then_some(current);
            config_changed = config.as_ref() != current;
            roles
        },
        bridge,
        &client.paths.root,
    )
    .await;
    ensure!(
        !config_changed,
        "Project configuration changed while you were editing. Run the wizard again."
    );
    let plan = plan?;
    let preview = plan.preview();
    println!(
        "\nProject configuration at {}",
        preview.manifest_path.display()
    );
    println!("{}", serde_json::to_string_pretty(plan.config())?);
    println!(
        "\nSkill at {}\n{}",
        preview.skill_path.display(),
        preview.skill_content
    );
    println!(
        "\nMCP entry at {}\n{}",
        preview.mcp_path.display(),
        preview.mcp_content
    );
    if let (Some(path), Some(content)) = (preview.hook_path, preview.hook_content) {
        println!("\nHook at {}\n{content}", path.display());
    }
    if !preview.cleanup.is_empty() {
        println!(
            "\nPrevious Runspur integration entries to remove\n{}",
            serde_json::to_string_pretty(&preview.cleanup)?
        );
    }

    let (confirmation, success) = match mode {
        Mode::New => ("Connect this project?", "Project connected."),
        Mode::Edit => ("Save project changes?", "Project updated."),
    };
    let confirmed = prompt(&format!("\n{confirmation} [y/N] "))?;
    if confirmed
        .is_some_and(|input| input.eq_ignore_ascii_case("y") || input.eq_ignore_ascii_case("yes"))
    {
        plan.apply(client).await?;
        println!("{success} Open a new conversation in the selected main Agent.");
    } else {
        println!("Project setup canceled.");
    }
    Ok(())
}

fn project_roles(
    root: &Path,
    config: Option<&ProjectConfig>,
    profiles: &[AgentProfile],
) -> Result<ProjectRoles> {
    let default_name = config.map_or_else(
        || {
            root.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("Runspur project")
        },
        |config| config.name.as_str(),
    );
    let name = answer("Project name", Some(default_name))?;
    let mut parents: Vec<_> = profiles
        .iter()
        .filter(|profile| desktop_parent_profile(profile))
        .collect();
    parents.sort_by_key(|profile| profile.profile_id != "codex-desktop");
    ensure!(!parents.is_empty(), "No supported main Agent was found.");
    print_profiles("Main Agent", &parents);
    let default_parent = config.map_or(Some(0), |config| {
        parents
            .iter()
            .position(|profile| profile.profile_id == config.main_agent_profile_id)
    });
    let default_parent = default_parent.map(|index| (index + 1).to_string());
    let selected = answer("Main Agent", default_parent.as_deref())?;
    let parent = *numbered_choice(&parents, &selected)?;

    let workers: Vec<_> = profiles
        .iter()
        .filter(|worker| {
            desktop_worker_profile(worker)
                && !matches!(
                    (&parent.kind, &worker.kind),
                    (AgentKind::CodexDesktop, AgentKind::CodexExec)
                        | (AgentKind::ClaudePrint, AgentKind::ClaudePrint)
                )
        })
        .collect();
    ensure!(
        !workers.is_empty(),
        "No eligible child Agent was found. Install {} or register its native executable with `runspur profile-put`.",
        if parent.kind == AgentKind::CodexDesktop {
            "Claude Code"
        } else {
            "the Codex CLI"
        }
    );
    print_profiles("Child Agents", &workers);
    let default_workers = if workers.len() == 1 {
        Some("1".into())
    } else {
        config.and_then(|config| {
            let saved: Vec<_> = workers
                .iter()
                .enumerate()
                .filter(|(_, worker)| {
                    config
                        .allowed_worker_profile_ids
                        .contains(&worker.profile_id)
                })
                .map(|(index, _)| (index + 1).to_string())
                .collect();
            (!saved.is_empty() && saved.len() == config.allowed_worker_profile_ids.len())
                .then(|| saved.join(","))
        })
    };
    let selected = answer(
        "Child Agents (comma-separated numbers)",
        default_workers.as_deref(),
    )?;
    let worker_profile_ids: BTreeSet<_> = selected
        .split(',')
        .map(|number| Ok(numbered_choice(&workers, number.trim())?.profile_id.clone()))
        .collect::<Result<_>>()?;

    let permissions = [
        ExecutionMode::ReadOnly,
        ExecutionMode::WorkspaceWrite,
        ExecutionMode::ProjectWrite,
    ];
    println!("\nWorker permissions");
    println!("  1. Read only");
    let permits_writes = workers.iter().any(|worker| {
        worker.kind == AgentKind::ClaudePrint && worker_profile_ids.contains(&worker.profile_id)
    });
    let permissions = if permits_writes {
        println!("  2. Write to files named in each task");
        println!("  3. Write to text files across the project");
        &permissions[..]
    } else {
        println!("The selected Codex workers support read-only tasks.");
        &permissions[..1]
    };
    let default_permission = match config
        .filter(|_| permits_writes)
        .map(|config| &config.limits)
    {
        Some(limits) if limits.allow_project_write => "3",
        Some(limits) if limits.allow_workspace_write => "2",
        _ => "1",
    };
    let selected = answer("Permission", Some(default_permission))?;
    let permission = *numbered_choice(permissions, &selected)?;

    Ok(ProjectRoles {
        name,
        main_agent_profile_id: parent.profile_id.clone(),
        worker_profile_ids,
        allow_workspace_write: permission == ExecutionMode::WorkspaceWrite,
        allow_project_write: permission == ExecutionMode::ProjectWrite,
    })
}

fn print_profiles(title: &str, profiles: &[&AgentProfile]) {
    println!("\n{title}");
    for (index, profile) in profiles.iter().enumerate() {
        let (provider, default_id) = match profile.kind {
            AgentKind::CodexDesktop => ("Codex", "codex-desktop"),
            AgentKind::CodexExec => ("Codex", "codex-cli"),
            AgentKind::ClaudePrint => ("Claude Code", "claude-cli"),
            AgentKind::Probe => continue,
        };
        if profile.profile_id == default_id {
            println!("  {}. {provider}", index + 1);
        } else {
            println!("  {}. {provider} ({})", index + 1, profile.profile_id);
        }
    }
}

fn numbered_choice<'a, T>(choices: &'a [T], number: &str) -> Result<&'a T> {
    number
        .parse::<usize>()
        .ok()
        .and_then(|number| number.checked_sub(1))
        .and_then(|index| choices.get(index))
        .with_context(|| format!("Choose a number from 1 to {}.", choices.len()))
}

fn answer(question: &str, default: Option<&str>) -> Result<String> {
    let text = default.map_or_else(
        || format!("{question} "),
        |value| format!("{question} [{value}] "),
    );
    let input = prompt(&text)?.context("Project setup canceled because input closed.")?;
    Ok(if input.is_empty() {
        default.unwrap_or_default().into()
    } else {
        input
    })
}

fn prompt(question: &str) -> Result<Option<String>> {
    print!("{question}");
    io::stdout().flush()?;
    let mut input = String::new();
    let bytes = io::stdin().read_line(&mut input)?;
    Ok((bytes != 0).then(|| input.trim().to_owned()))
}
