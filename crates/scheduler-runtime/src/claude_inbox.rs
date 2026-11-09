#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_contains_only_an_opaque_delivery_id_and_targets_the_native_id() {
        let session = Uuid::new_v4();
        let delivery = Uuid::new_v4();
        let frame = wake_frame(session, delivery);
        assert_eq!(frame["session_id"], session.to_string());
        assert_eq!(frame["type"], "user");
        let text = frame["message"]["content"].as_str().unwrap();
        assert!(text.contains(&delivery.to_string()));
        assert!(text.contains("scheduler_claim"));
        assert!(!text.contains("ack_nonce"));
    }

    #[test]
    fn accepts_native_windows_local_inbox_scope_only() {
        let actual_shape = r"\\.\pipe\LOCAL\cc-msg-0123456789abcdef";
        assert_eq!(inbox_endpoint(actual_shape).unwrap(), actual_shape);
        assert_eq!(
            inbox_endpoint(r"uds:\\.\pipe\cc-msg-legacy").unwrap(),
            r"\\.\pipe\cc-msg-legacy"
        );
        assert!(inbox_endpoint(r"\\remote\pipe\LOCAL\cc-msg-x").is_err());
        assert!(inbox_endpoint(r"\\.\pipe\OTHER\cc-msg-x").is_err());
        assert!(inbox_endpoint(r"\\.\pipe\LOCAL\extra\cc-msg-x").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn alternate_bundle_requires_the_projects_exact_claude_bridge_entry() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let state = temp.path().join("state");
        let bridge = temp.path().join("scheduler-mcp.exe");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&state).unwrap();
        std::fs::write(&bridge, []).unwrap();
        let project = project.canonicalize().unwrap();
        let state = state.canonicalize().unwrap();
        let entry = |name: &str, host: &str, root: &str| {
            json!({"mcpServers":{
                (name): {"type":"stdio","command":bridge,"args":["--project-root",root,"--host",host,"--data-dir",state]}
            }})
        };
        let path = project.join(".mcp.json");
        std::fs::write(
            &path,
            entry("runspur_scheduler", "claude-code", ".").to_string(),
        )
        .unwrap();
        assert_eq!(
            configured_project_bridge(&project, &state).unwrap(),
            bridge.canonicalize().unwrap()
        );
        for invalid in [
            entry("other", "claude-code", "."),
            entry("runspur_scheduler", "codex", "."),
            entry("runspur_scheduler", "claude-code", ".."),
        ] {
            std::fs::write(&path, invalid.to_string()).unwrap();
            assert!(configured_project_bridge(&project, &state).is_err());
        }
    }
}
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use scheduler_core::{
    ClaudeHookCall, MAX_FRAME_BYTES, ParentRoute, ProcessIdentity, ProjectRecord, Registration,
};
use scheduler_platform::{connect_pipe, is_running, process_info};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::{DeliveryOutcome, ResolvedParent, workers::resolve_program};

const CALL_TTL: Duration = Duration::from_secs(90);
const MAX_CALLS: usize = 1024;
const MAX_ROUTES: usize = 256;
const INBOX_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Default)]
pub(crate) struct ClaudeInboxRegistry(Mutex<Registry>);

#[derive(Default)]
struct Registry {
    routes: HashMap<(String, Uuid), InboxRoute>,
    calls: HashMap<(String, String), HookCall>,
}

#[derive(Clone)]
struct InboxRoute {
    workspace: PathBuf,
    owner: ProcessIdentity,
    bridge: PathBuf,
    endpoint: String,
    token: String,
    generation: String,
}

struct HookCall {
    session: Uuid,
    generation: String,
    operation_sha256: String,
    created: Instant,
}

impl ClaudeInboxRegistry {
    pub(crate) async fn register(
        &self,
        project: &ProjectRecord,
        session: Uuid,
        hook: ClaudeHookCall,
        hook_peer: &ProcessIdentity,
        claude_program: &Path,
        state_root: &Path,
    ) -> Result<()> {
        if hook.project_root != project.root
            || !valid_tool_use_id(&hook.tool_use_id)
            || hook.operation_sha256.len() != 64
            || !hook.operation_sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || hook.inbox_token.is_empty()
            || hook.inbox_token.len() > 1024
        {
            bail!("Invalid Claude Code tool hook proof");
        }
        let endpoint = inbox_endpoint(&hook.inbox)?;
        let process = process_info(hook.host_pid)?;
        let bridge = runspur_bridge_image(hook_peer, &process.identity, false)
            .context("Claude Code hook did not come from a direct scheduler-mcp child process")?;
        let bundled = std::env::current_exe()?
            .with_file_name("scheduler-mcp.exe")
            .canonicalize()
            .ok();
        if !is_running(&process.identity)
            || std::fs::canonicalize(&process.executable)? != resolve_program(claude_program)?
            || (bundled.as_ref() != Some(&bridge)
                && configured_project_bridge(&project.root, state_root)
                    .ok()
                    .as_ref()
                    != Some(&bridge))
        {
            bail!("Claude Code hook did not come from the registered Claude Code executable");
        }
        let native_peer = tokio::time::timeout(INBOX_TIMEOUT, connect_pipe(endpoint))
            .await
            .context("Claude Code inbox did not respond")?
            .context("Claude Code inbox is unavailable")?;
        if native_peer.process != process.identity {
            bail!("Claude Code inbox is not owned by this native session's host");
        }
        drop(native_peer);

        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("Claude Code inbox registry is unavailable"))?;
        let project_id = &project.config.project_id;
        let route_key = (project_id.clone(), session);
        let call_key = (project_id.clone(), hook.tool_use_id);
        state
            .calls
            .retain(|_, call| call.created.elapsed() < CALL_TTL);
        if state.calls.len() >= MAX_CALLS || state.calls.contains_key(&call_key) {
            bail!("Claude Code tool hook proof is already registered or the registry is busy");
        }
        if state
            .routes
            .get(&route_key)
            .is_some_and(|route| route.owner != process.identity && is_running(&route.owner))
        {
            bail!("This Claude Code session already has another live owner");
        }
        // A switch can retain the process. Never keep another conversation's route for that owner.
        state
            .routes
            .retain(|key, route| *key == route_key || route.owner != process.identity);
        if state.routes.len() >= MAX_ROUTES {
            state.routes.retain(|_, route| is_running(&route.owner));
            if state.routes.len() >= MAX_ROUTES && !state.routes.contains_key(&route_key) {
                bail!("Claude Code inbox registry is full");
            }
        }
        let generation = state
            .routes
            .get(&route_key)
            .filter(|route| {
                route.owner == process.identity
                    && route.bridge == bridge
                    && route.endpoint == endpoint
                    && route.token == hook.inbox_token
            })
            .map_or_else(
                || Uuid::new_v4().to_string(),
                |route| route.generation.clone(),
            );
        state.routes.insert(
            route_key,
            InboxRoute {
                workspace: project.root.clone(),
                owner: process.identity,
                bridge,
                endpoint: endpoint.into(),
                token: hook.inbox_token,
                generation: generation.clone(),
            },
        );
        state.calls.insert(
            call_key,
            HookCall {
                session,
                generation,
                operation_sha256: hook.operation_sha256,
                created: Instant::now(),
            },
        );
        Ok(())
    }

    pub(crate) fn consume(
        &self,
        project: &ProjectRecord,
        tool_use_id: &str,
        bridge: &ProcessIdentity,
    ) -> Result<(ResolvedParent, String)> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("Claude Code inbox registry is unavailable"))?;
        let project_id = &project.config.project_id;
        let call_key = (project_id.clone(), tool_use_id.to_owned());
        let call = state.calls.get(&call_key).filter(|call| call.created.elapsed() < CALL_TTL)
            .context("Claude Code did not attest this tool call. Enable the project's Runspur hook and native inbox.")?;
        let route = state
            .routes
            .get(&(project_id.clone(), call.session))
            .filter(|route| {
                route.workspace == project.root
                    && route.generation == call.generation
                    && is_running(&route.owner)
            })
            .context("Claude Code changed sessions before this tool call could run")?;
        if runspur_bridge_image(bridge, &route.owner, true).as_ref() != Some(&route.bridge) {
            bail!("Runspur MCP bridge does not belong to the attested Claude Code process");
        }
        let resolved = resolved(call.session, route);
        let call = state
            .calls
            .remove(&call_key)
            .context("Claude Code tool proof is unavailable")?;
        Ok((resolved, call.operation_sha256))
    }

    pub(crate) fn refresh(&self, project: &ProjectRecord, session: Uuid) -> Result<ResolvedParent> {
        let route = self.lookup(&project.config.project_id, session)?;
        if route.workspace != project.root {
            bail!("Claude Code inbox belongs to a different workspace");
        }
        Ok(resolved(session, &route))
    }

    fn lookup(&self, project_id: &str, session: Uuid) -> Result<InboxRoute> {
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("Claude Code inbox registry is unavailable"))?
            .routes
            .get(&(project_id.to_owned(), session))
            .filter(|route| is_running(&route.owner))
            .cloned()
            .context("The original Claude Code inbox is unavailable")
    }

    pub(crate) async fn wake(
        &self,
        registration: &Registration,
        delivery_id: &str,
    ) -> DeliveryOutcome {
        let (Ok(session), Ok(delivery)) = (
            Uuid::parse_str(&registration.route.session_id),
            Uuid::parse_str(delivery_id),
        ) else {
            return DeliveryOutcome::Refused("Invalid Claude Code delivery identity".into());
        };
        let Ok(route) = self.lookup(&registration.project_id, session) else {
            return DeliveryOutcome::Unavailable(
                "The original Claude Code inbox is unavailable".into(),
            );
        };
        if route.workspace != registration.workspace_root
            || route.generation != registration.generation
        {
            return DeliveryOutcome::Unavailable("The original Claude Code inbox changed".into());
        }
        let mut write_started = false;
        let sent = tokio::time::timeout(INBOX_TIMEOUT, async {
            let mut peer = connect_pipe(&route.endpoint).await?;
            if peer.process != route.owner {
                bail!("The original Claude Code process no longer owns its inbox");
            }
            let mut frames = serde_json::to_vec(&json!({"type":"auth","token":route.token}))?;
            frames.push(b'\n');
            serde_json::to_writer(&mut frames, &wake_frame(session, delivery))?;
            frames.push(b'\n');
            write_started = true;
            peer.stream.write_all(&frames).await?;
            peer.stream.flush().await?;
            Ok::<_, anyhow::Error>(())
        })
        .await;
        match sent {
            Ok(Ok(())) => DeliveryOutcome::Uncertain("Claude Code was notified; waiting for the original conversation to claim the result".into()),
            _ if !write_started => DeliveryOutcome::Unavailable("The original Claude Code inbox is unavailable".into()),
            _ => DeliveryOutcome::Uncertain("Claude Code did not confirm receipt of the result notification".into()),
        }
    }
}

fn resolved(session: Uuid, route: &InboxRoute) -> ResolvedParent {
    ResolvedParent {
        route: ParentRoute {
            adapter_id: crate::claude_session::ADAPTER_ID.into(),
            session_id: session.to_string(),
            parameters: json!({"transport":"native_inbox"}),
        },
        generation: route.generation.clone(),
        workspace_root: route.workspace.clone(),
    }
}

pub(crate) fn is_native_route(route: &ParentRoute) -> bool {
    route.parameters["transport"] == "native_inbox"
}

fn valid_tool_use_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn inbox_endpoint(endpoint: &str) -> Result<&str> {
    let endpoint = endpoint.strip_prefix("uds:").unwrap_or(endpoint);
    let name = endpoint
        .strip_prefix(r"\\.\pipe\")
        .context("Claude Code did not provide a local Windows inbox")?;
    let name = name.strip_prefix(r"LOCAL\").unwrap_or(name);
    if name.is_empty()
        || name.len() > 200
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        bail!("Claude Code provided an invalid local inbox name");
    }
    Ok(endpoint)
}

fn runspur_bridge_image(
    peer: &ProcessIdentity,
    owner: &ProcessIdentity,
    allow_cmd_intermediary: bool,
) -> Option<PathBuf> {
    let process = process_info(peer.pid).ok()?;
    if process.identity != *peer || peer.started_at < owner.started_at {
        return None;
    }
    let direct = process.parent_pid == Some(owner.pid);
    let via_cmd = allow_cmd_intermediary
        && process
            .parent_pid
            .and_then(|pid| process_info(pid).ok())
            .is_some_and(|parent| {
                parent.parent_pid == Some(owner.pid)
                    && parent.identity.started_at >= owner.started_at
                    && parent.identity.started_at <= peer.started_at
                    && parent
                        .executable
                        .file_name()
                        .is_some_and(|name| name.eq_ignore_ascii_case("cmd.exe"))
            });
    if direct || via_cmd {
        return process.executable.canonicalize().ok().filter(|path| {
            path.file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("scheduler-mcp.exe"))
        });
    }
    None
}

fn configured_project_bridge(project: &Path, state_root: &Path) -> Result<PathBuf> {
    let mut file = std::fs::File::open(project.join(".mcp.json"))?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(&mut file, (MAX_FRAME_BYTES + 1) as u64),
        &mut bytes,
    )?;
    if bytes.len() > MAX_FRAME_BYTES {
        bail!("Project MCP configuration is too large");
    }
    let document: Value = serde_json::from_slice(&bytes)?;
    let entry = &document["mcpServers"]["runspur_scheduler"];
    if entry["type"] != "stdio" || entry.as_object().is_none_or(|fields| fields.len() != 3) {
        bail!("Project has no exact Runspur Claude Code MCP entry");
    }
    let program = Path::new(
        entry["command"]
            .as_str()
            .context("Runspur MCP executable is missing")?,
    );
    if !program.is_absolute() {
        bail!("Alternate Runspur MCP bridge must use an absolute executable path");
    }
    let args = entry["args"]
        .as_array()
        .context("Runspur MCP arguments are missing")?;
    let mut options = HashMap::new();
    for pair in args.chunks(2) {
        let [flag, argument] = pair else {
            bail!("Incomplete Runspur MCP argument");
        };
        let flag = flag.as_str().context("Invalid Runspur MCP argument")?;
        let argument = argument.as_str().context("Invalid Runspur MCP argument")?;
        if !matches!(flag, "--host" | "--project-root" | "--data-dir")
            || options.insert(flag, argument).is_some()
        {
            bail!("Unexpected Runspur MCP option");
        }
    }
    let project_arg = options
        .get("--project-root")
        .context("Runspur MCP workspace is missing")?;
    if options.get("--host") != Some(&"claude-code")
        || project.join(project_arg).canonicalize()? != project
        || options
            .get("--data-dir")
            .is_some_and(|path| Path::new(path).canonicalize().ok().as_deref() != Some(state_root))
    {
        bail!("Project MCP bridge is configured for a different host, workspace or state");
    }
    resolve_program(program)
}

fn wake_frame(session: Uuid, delivery: Uuid) -> Value {
    let message_id = Uuid::new_v4();
    json!({"msgV":1,"msg_id":message_id,"uuid":message_id,"type":"user","session_id":session,
    "message":{"role":"user","content":format!(
            "Runspur has a child Agent result for its originating conversation. Use scheduler_claim with delivery_id {delivery}. If the claim succeeds, acknowledge the returned result using scheduler_ack, then continue the original assignment."
    )}})
}
