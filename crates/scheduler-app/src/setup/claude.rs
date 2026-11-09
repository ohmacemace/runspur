use std::path::Path;

use anyhow::{Context, Result, bail};
use scheduler_core::{AgentProfile, ProjectConfig};
use serde_json::{Value, json};

use super::{
    CleanupEdit, ConfigEdit, HookEdit, ProjectIntegration, bridge::Bridge, mcp_arguments,
    read_optional_file, skill,
};

pub(super) const SKILL_PATH: &str = ".claude/skills/agent-scheduler/SKILL.md";
pub(super) const MCP_PATH: &str = ".mcp.json";
pub(super) const HOOK_PATH: &str = ".claude/settings.local.json";
const ORIGIN_HOOK_FLAG: &str = "--claude-origin-hook";
const HOST_ARGS: &[&str] = &["--host", "claude-code"];

fn entry(program: &str, data_dir: Option<&Path>) -> Value {
    let mut args = mcp_arguments(data_dir);
    args.extend(HOST_ARGS.iter().map(|arg| (*arg).to_owned()));
    json!({ "type": "stdio", "command": program, "args": args })
}

fn installed_bridge<'a>(current: &'a Value, desired: Bridge<'_>) -> Option<Bridge<'a>> {
    let object = current.as_object()?;
    if object.len() != 3 || current["type"] != "stdio" {
        return None;
    }
    let args: Vec<_> = current["args"]
        .as_array()?
        .iter()
        .map(Value::as_str)
        .collect::<Option<_>>()?;
    Bridge::recognize(current["command"].as_str()?, &args, desired, HOST_ARGS)
}

fn document(bytes: Option<&[u8]>) -> Result<Value> {
    match bytes {
        Some(bytes) => {
            serde_json::from_slice(bytes).context("Existing .mcp.json could not be parsed")
        }
        None => Ok(json!({})),
    }
}

async fn prepare_hook(
    root: &Path,
    program: &str,
    data_dir: Option<&Path>,
    enable: bool,
) -> Result<Option<HookEdit>> {
    let path = root.join(HOOK_PATH);
    let before = read_optional_file(&path).await?;
    if !enable && before.is_none() {
        return Ok(None);
    }
    let mut settings =
        document(before.as_deref()).context("Cannot read Claude project-local settings")?;
    let root = settings
        .as_object_mut()
        .context("Claude project-local settings must be an object")?;
    if !enable && !root.contains_key("hooks") {
        return Ok(None);
    }
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("Claude hooks must be an object")?;
    if !enable && !hooks.contains_key("PreToolUse") {
        return Ok(None);
    }
    let rules = hooks
        .entry("PreToolUse")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("Claude PreToolUse hooks must be an array")?;
    let mut args = mcp_arguments(data_dir);
    args[1] = "${CLAUDE_PROJECT_DIR}".into();
    args.extend([
        "--host".into(),
        "claude-code".into(),
        ORIGIN_HOOK_FLAG.into(),
    ]);
    let rule = json!({
        "matcher": "^mcp__runspur_scheduler__scheduler_.*$",
        "hooks": [{"type":"command", "command": program, "args": args, "timeout": 10}]
    });
    let mut installed = false;
    for candidate in rules.iter() {
        if candidate == &rule {
            installed = true;
        } else if candidate
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|handlers| {
                handlers.iter().any(|handler| {
                    handler
                        .get("args")
                        .and_then(Value::as_array)
                        .is_some_and(|args| {
                            args.iter()
                                .any(|arg| arg.as_str() == Some(ORIGIN_HOOK_FLAG))
                        })
                })
            })
        {
            bail!(
                "Existing Claude Code Runspur hook has user changes: {}",
                path.display()
            );
        }
    }
    let content = enable
        .then(|| serde_json::to_string_pretty(&rule))
        .transpose()?;
    let after = match (enable, installed) {
        (true, false) => {
            rules.push(rule);
            Some(format!("{}\n", serde_json::to_string_pretty(&settings)?).into_bytes())
        }
        (false, true) => {
            rules.retain(|candidate| candidate != &rule);
            Some(format!("{}\n", serde_json::to_string_pretty(&settings)?).into_bytes())
        }
        (true, true) => before.clone(),
        (false, false) => return Ok(None),
    };
    Ok(Some(HookEdit {
        file: ConfigEdit {
            path,
            before,
            after,
        },
        content,
    }))
}

pub(super) async fn prepare(
    root: &Path,
    config: &ProjectConfig,
    program: &str,
    data_dir: Option<&Path>,
    profiles: &[AgentProfile],
) -> Result<ProjectIntegration> {
    let native = if let Some(claude) = profiles
        .iter()
        .find(|profile| profile.profile_id == config.main_agent_profile_id)
        .and_then(|profile| profile.program.as_deref())
        .filter(|_| config.integrations.mcp)
    {
        scheduler_runtime::claude_supports_native_inbox(claude).await?
    } else {
        false
    };
    let skill_path = root.join(SKILL_PATH);
    let config_path = root.join(MCP_PATH);
    let skill_old = read_optional_file(&skill_path).await?;
    if let Some(bytes) = &skill_old
        && !skill::is_managed(bytes)?
    {
        bail!(
            "Existing project Skill differs; refusing to overwrite {}",
            skill_path.display()
        );
    }
    let previous = read_optional_file(&config_path).await?;
    let mut document = document(previous.as_deref())?;
    let object = document
        .as_object_mut()
        .context("Existing .mcp.json must be an object")?;
    let servers = object
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("Existing .mcp.json mcpServers must be an object")?;
    let desired = Bridge { program, data_dir };
    let current = servers.get("runspur_scheduler");
    let bridge = if let Some(current) = current {
        installed_bridge(current, desired).context(
            "Existing runspur_scheduler MCP entry differs; refusing to overwrite user configuration",
        )?
    } else {
        desired
    };
    let bundled;
    let selected_program = if native && bridge.program == "scheduler-mcp" {
        desired.program
    } else {
        bridge.program
    };
    let program = if native && selected_program == "scheduler-mcp" {
        bundled = std::env::current_exe()?
            .with_file_name(format!("scheduler-mcp{}", std::env::consts::EXE_SUFFIX));
        if !bundled.is_file() {
            bail!(
                "Set --mcp-program to the native scheduler-mcp executable for the Claude Code hook"
            );
        }
        bundled
            .to_str()
            .context("Runspur MCP path is not valid Unicode")?
    } else {
        selected_program
    };
    let hook = prepare_hook(root, program, bridge.data_dir, native).await?;
    let entry = entry(program, bridge.data_dir);
    let changed = if config.integrations.mcp {
        current != Some(&entry)
    } else {
        current.is_some()
    };
    if config.integrations.mcp {
        servers.insert("runspur_scheduler".into(), entry.clone());
    } else {
        servers.remove("runspur_scheduler");
    }
    let updated = match &previous {
        Some(bytes) if !changed => std::str::from_utf8(bytes)?.to_owned(),
        _ => format!("{}\n", serde_json::to_string_pretty(&document)?),
    };
    Ok(ProjectIntegration {
        skill_path,
        skill_content: skill::render_skill(config, profiles)?,
        config_path,
        skill_old,
        previous,
        document: updated,
        runspur_entry: serde_json::to_string_pretty(
            &json!({"mcpServers": {"runspur_scheduler": entry}}),
        )?,
        hook,
    })
}

pub(super) async fn cleanup(
    root: &Path,
    program: &str,
    data_dir: Option<&Path>,
) -> Result<Vec<CleanupEdit>> {
    let mut edits = Vec::new();
    let skill_path = root.join(SKILL_PATH);
    if let Some(bytes) = read_optional_file(&skill_path).await? {
        if !skill::is_managed(&bytes)? {
            bail!(
                "Existing Claude Code Skill has user changes: {}",
                skill_path.display()
            );
        }
        edits.push(CleanupEdit {
            file: ConfigEdit {
                path: skill_path,
                before: Some(bytes),
                after: None,
            },
            entries: vec!["skill"],
        });
    }
    let config_path = root.join(MCP_PATH);
    let before = read_optional_file(&config_path).await?;
    let mut document = document(before.as_deref())?;
    let servers = document
        .get_mut("mcpServers")
        .map(|servers| {
            servers
                .as_object_mut()
                .context("Existing .mcp.json mcpServers must be an object")
        })
        .transpose()?;
    let desired = Bridge { program, data_dir };
    let bridge = if let Some(current) = servers
        .as_ref()
        .and_then(|servers| servers.get("runspur_scheduler"))
    {
        installed_bridge(current, desired)
            .context("Existing Claude Code Runspur MCP entry has user changes")?
    } else {
        desired
    };
    let hook = prepare_hook(root, bridge.program, bridge.data_dir, false).await?;
    if let Some(servers) = servers
        && servers.remove("runspur_scheduler").is_some()
    {
        let updated = format!("{}\n", serde_json::to_string_pretty(&document)?);
        edits.push(CleanupEdit {
            file: ConfigEdit {
                path: config_path,
                before,
                after: Some(updated.into_bytes()),
            },
            entries: vec!["runspur_scheduler"],
        });
    }
    if let Some(hook) = hook {
        edits.push(CleanupEdit {
            file: hook.file,
            entries: vec!["claude_origin_hook"],
        });
    }
    Ok(edits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_hook_preserves_existing_project_settings_and_cleans_up_its_own_rule() {
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("CLI package");
        let bundle = package.join("native");
        let suffix = std::env::consts::EXE_SUFFIX;
        tokio::fs::create_dir_all(&bundle).await.unwrap();
        for binary in ["schedulerctl", "scheduler-mcp"] {
            tokio::fs::write(bundle.join(format!("{binary}{suffix}")), b"fixture")
                .await
                .unwrap();
        }
        let program = bundle.join(format!("scheduler-mcp{suffix}"));
        let program = program.to_str().unwrap();
        let metadata =
            json!({"name":"runspur", "bin":{"runspur":format!("native/schedulerctl{suffix}")}});
        tokio::fs::write(
            package.join("package.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .await
        .unwrap();
        let settings = dir.path().join(HOOK_PATH);
        let original = json!({
            "hooks": {"PreToolUse": [{"matcher": "Write", "hooks": [{"type": "command", "command": "local-check"}]}]},
            "permissions": {"deny": ["SendMessage"]}
        });
        crate::setup::write_new_file(&settings, &serde_json::to_vec(&original).unwrap())
            .await
            .unwrap();

        let added = prepare_hook(dir.path(), program, None, true)
            .await
            .unwrap()
            .unwrap();
        assert!(
            added
                .content
                .as_deref()
                .unwrap()
                .contains("--claude-origin-hook")
        );
        assert_eq!(document(added.file.before.as_deref()).unwrap(), original);
        crate::setup::apply_files(&[added.file]).await.unwrap();
        let installed = document(read_optional_file(&settings).await.unwrap().as_deref()).unwrap();
        assert_eq!(
            installed["hooks"]["PreToolUse"][0],
            original["hooks"]["PreToolUse"][0]
        );
        assert_eq!(installed["permissions"], original["permissions"]);

        let config_path = dir.path().join(MCP_PATH);
        let previous = json!({"mcpServers":{
            "runspur_scheduler":entry(program, None),
            "custom":{"command":"other"}
        }});
        tokio::fs::write(&config_path, serde_json::to_vec(&previous).unwrap())
            .await
            .unwrap();
        let default = scheduler_platform::DataPaths::default_root().unwrap();
        let cleanup = cleanup(dir.path(), "scheduler-mcp", Some(&default))
            .await
            .unwrap();
        assert_eq!(cleanup.len(), 2);
        assert_eq!(cleanup[1].entries, ["claude_origin_hook"]);
        let files: Vec<_> = cleanup.into_iter().map(|edit| edit.file).collect();
        crate::setup::apply_files(&files).await.unwrap();
        let saved = document(read_optional_file(&config_path).await.unwrap().as_deref()).unwrap();
        assert_eq!(saved, json!({"mcpServers":{"custom":{"command":"other"}}}));
        assert_eq!(
            document(read_optional_file(&settings).await.unwrap().as_deref()).unwrap(),
            original
        );
    }
}
