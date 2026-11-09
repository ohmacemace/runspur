#![cfg(windows)]

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Result, bail};
use async_trait::async_trait;
use scheduler_core::*;
use scheduler_platform::{DataPaths, LocalClient, LocalListener};
use scheduler_runtime::{
    DeliveryOutcome, ParentAdapter, ResolvedParent, Scheduler, WorkerAdapter, WorkerContext, serve,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

struct SimulatedAgent {
    id: &'static str,
    kind: AgentKind,
    completions: mpsc::UnboundedSender<DeliveryEnvelope>,
}

#[async_trait]
impl ParentAdapter for SimulatedAgent {
    fn id(&self) -> &'static str {
        self.id
    }
    fn profile_kind(&self) -> AgentKind {
        self.kind.clone()
    }
    fn family(&self) -> &'static str {
        self.id
    }
    async fn resolve(
        &self,
        project: &ProjectRecord,
        origin: &OriginSession,
    ) -> Result<ResolvedParent> {
        Ok(ResolvedParent {
            route: ParentRoute {
                adapter_id: self.id.into(),
                session_id: origin.provider_session_id.clone(),
                parameters: json!({"private_route":"fixture"}),
            },
            generation: format!("{}-owner", self.id),
            workspace_root: project.root.clone(),
            main_permission: None,
        })
    }
    async fn deliver(
        &self,
        _registration: &Registration,
        envelope: &DeliveryEnvelope,
    ) -> DeliveryOutcome {
        self.completions.send(envelope.clone()).unwrap();
        DeliveryOutcome::AcceptedWithoutReference
    }
}

#[async_trait]
impl WorkerAdapter for SimulatedAgent {
    fn profile_kind(&self) -> AgentKind {
        self.kind.clone()
    }
    fn family(&self) -> &'static str {
        self.id
    }
    async fn run(
        &self,
        task: &QueuedTask,
        _context: WorkerContext<'_>,
        _cancel: CancellationToken,
    ) -> Result<Completion> {
        Ok(Completion {
            schema_version: SCHEMA_VERSION,
            task_id: task.view.task_id.clone(),
            attempt_id: task.attempt_id.clone(),
            execution_state: ExecutionState::Succeeded,
            finished_at_ms: now_ms(),
            result: TaskResult {
                summary: format!("{} executed {}", self.id, task.prompt),
                exit_code: None,
                error_class: None,
                verification: vec![],
                artifact_refs: vec![],
            },
        })
    }
}

#[tokio::test]
async fn adapters_swap_roles_and_same_host_session_id_stays_isolated() {
    let directory = tempfile::tempdir().unwrap();
    let paths = DataPaths::prepare(&directory.path().join("state")).unwrap();
    let root = directory.path().join("project");
    tokio::fs::create_dir(&root).await.unwrap();
    let (alpha_results, mut alpha) = mpsc::unbounded_channel();
    let (beta_results, mut beta) = mpsc::unbounded_channel();
    let alpha_adapter = Arc::new(SimulatedAgent {
        id: "alpha",
        kind: AgentKind::CodexExec,
        completions: alpha_results,
    });
    let beta_adapter = Arc::new(SimulatedAgent {
        id: "beta",
        kind: AgentKind::ClaudePrint,
        completions: beta_results,
    });
    let app = Scheduler::with_adapters(
        paths,
        2,
        vec![alpha_adapter.clone(), beta_adapter.clone()],
        vec![alpha_adapter, beta_adapter],
    )
    .unwrap();
    for (id, kind) in [
        ("alpha", AgentKind::CodexExec),
        ("beta", AgentKind::ClaudePrint),
    ] {
        app.store
            .put_profile(AgentProfile {
                profile_id: id.into(),
                kind,
                program: Some("unused-by-simulated-adapter.exe".into()),
                model: None,
                reasoning_effort: None,
                environment_keys: Default::default(),
            })
            .await
            .unwrap();
    }
    let mut project = ProjectRecord {
        root,
        revision: String::new(),
        config: ProjectConfig {
            schema_version: SCHEMA_VERSION,
            project_id: new_id(),
            name: "swappable".into(),
            main_agent_profile_id: "alpha".into(),
            allowed_worker_profile_ids: ["beta".into()].into(),
            integrations: IntegrationConfig::default(),
            limits: DispatchLimits::default(),
        },
    };
    let host_session = OriginSession {
        provider_session_id: "same-id-in-both-hosts".into(),
        source: OriginSource::RequestMetadata,
    };
    let loops = app.start();
    let mut submissions = Vec::new();
    for (main, worker) in [("alpha", "beta"), ("beta", "alpha")] {
        project.config.main_agent_profile_id = main.into();
        project.config.allowed_worker_profile_ids = [worker.into()].into();
        app.handle(&AuthContext::Admin, Operation::PutProject(project.clone()))
            .await
            .unwrap();
        let auth = app
            .resolve_origin(&project.config.project_id, Some(&host_session))
            .await
            .unwrap();
        let context = app.handle(&auth, Operation::SessionStatus).await.unwrap();
        assert_eq!(context["origin"]["origin_id"], auth.root().unwrap().1);
        assert!(context["origin"]["route"].get("parameters").is_none());
        let task: TaskAccepted = serde_json::from_value(
            app.handle(
                &auth,
                Operation::Submit(SubmitTask {
                    worker_profile_id: worker.into(),
                    prompt: format!("for {main}"),
                    idempotency_key: "same-key".into(),
                    timeout_seconds: Some(15),
                    execution_mode: ExecutionMode::ReadOnly,
                    write_scope: vec![],
                }),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        let result = event(if main == "alpha" {
            &mut alpha
        } else {
            &mut beta
        })
        .await;
        assert_eq!(result.task_id, task.task_id);
        assert_eq!(
            result.result.summary,
            format!("{worker} executed for {main}")
        );
        let acknowledgement = Operation::Ack {
            delivery_id: result.delivery_id,
            nonce: result.ack_nonce,
        };
        app.handle(&auth, acknowledgement.clone()).await.unwrap();
        wait_delivery(&app, &task.task_id, DeliveryState::AcknowledgedByParent).await;
        submissions.push((auth, task.task_id, acknowledgement));
    }
    for (index, (_, task_id, acknowledgement)) in submissions.iter().enumerate() {
        let (other, _, _) = &submissions[1 - index];
        assert!(app.store.task(other, task_id).await.is_err());
        assert!(app.handle(other, acknowledgement.clone()).await.is_err());
    }
    app.shutdown().await;
    for task in loops {
        task.await.unwrap();
    }
}

#[derive(Clone)]
struct Origin {
    project: String,
    root: PathBuf,
    sender: mpsc::UnboundedSender<DeliveryEnvelope>,
    mode: Arc<Mutex<u8>>,
}
#[derive(Default)]
struct FixtureParent {
    origins: Mutex<HashMap<String, Origin>>,
}
impl FixtureParent {
    fn add(
        &self,
        project: &ProjectRecord,
    ) -> (
        String,
        mpsc::UnboundedReceiver<DeliveryEnvelope>,
        Arc<Mutex<u8>>,
    ) {
        let id = new_id();
        let (sender, receiver) = mpsc::unbounded_channel();
        let mode = Arc::new(Mutex::new(0));
        self.origins.lock().unwrap().insert(
            id.clone(),
            Origin {
                project: project.config.project_id.clone(),
                root: project.root.clone(),
                sender,
                mode: mode.clone(),
            },
        );
        (id, receiver, mode)
    }
}
#[async_trait]
impl ParentAdapter for FixtureParent {
    fn id(&self) -> &'static str {
        "codex-desktop"
    }
    fn profile_kind(&self) -> AgentKind {
        AgentKind::CodexDesktop
    }
    fn family(&self) -> &'static str {
        "codex"
    }
    async fn resolve(
        &self,
        project: &ProjectRecord,
        origin: &OriginSession,
    ) -> Result<ResolvedParent> {
        let origins = self.origins.lock().unwrap();
        let known = origins
            .get(&origin.provider_session_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown fixture origin"))?;
        if known.project != project.config.project_id {
            bail!("Origin belongs to another project");
        }
        Ok(ResolvedParent {
            route: ParentRoute {
                adapter_id: self.id().into(),
                session_id: origin.provider_session_id.clone(),
                parameters: json!({}),
            },
            generation: "fixture-generation".into(),
            workspace_root: known.root.clone(),
            main_permission: None,
        })
    }
    async fn deliver(
        &self,
        registration: &Registration,
        envelope: &DeliveryEnvelope,
    ) -> DeliveryOutcome {
        let origin = self.origins.lock().unwrap()[&registration.route.session_id].clone();
        let mode = *origin.mode.lock().unwrap();
        if mode == 1 {
            return DeliveryOutcome::Busy;
        }
        if mode == 2 {
            return DeliveryOutcome::Unavailable("Fixture offline".into());
        }
        origin.sender.send(envelope.clone()).unwrap();
        if mode == 3 {
            DeliveryOutcome::Uncertain("Fixture sent then lost receipt".into())
        } else {
            DeliveryOutcome::Accepted {
                parent_turn_id: new_id(),
            }
        }
    }
}

struct Harness {
    app: Arc<Scheduler>,
    service: Option<tokio::task::JoinHandle<Result<()>>>,
    background: Vec<tokio::task::JoinHandle<()>>,
    parent: Arc<FixtureParent>,
    _dir: tempfile::TempDir,
}
impl Harness {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = DataPaths::prepare(&dir.path().join("state")).unwrap();
        let listener = LocalListener::bind(&paths.endpoint().unwrap()).unwrap();
        let parent = Arc::new(FixtureParent::default());
        let app = Scheduler::with_parents(paths, 2, vec![parent.clone()]).unwrap();
        app.store
            .put_profile(AgentProfile {
                profile_id: "main".into(),
                kind: AgentKind::CodexDesktop,
                program: None,
                model: None,
                reasoning_effort: None,
                environment_keys: Default::default(),
            })
            .await
            .unwrap();
        app.store
            .put_profile(AgentProfile {
                profile_id: "probe".into(),
                kind: AgentKind::Probe,
                program: Some(env!("CARGO_BIN_EXE_schedulerctl").into()),
                model: None,
                reasoning_effort: None,
                environment_keys: Default::default(),
            })
            .await
            .unwrap();
        app.recover().await.unwrap();
        let background = app.start();
        let server = app.clone();
        let service = tokio::spawn(async move { serve(server, listener).await });
        Self {
            app,
            service: Some(service),
            background,
            parent,
            _dir: dir,
        }
    }
    fn admin(&self) -> LocalClient {
        LocalClient {
            paths: self.app.paths.clone(),
            credential: CredentialRef::Admin,
            origin: None,
        }
    }
    fn root(&self, project: &ProjectRecord, session: &str) -> LocalClient {
        LocalClient {
            paths: self.app.paths.clone(),
            credential: CredentialRef::Project {
                project_id: project.config.project_id.clone(),
            },
            origin: Some(OriginSession {
                provider_session_id: session.into(),
                source: OriginSource::RequestMetadata,
            }),
        }
    }
    async fn project(&self, label: &str) -> ProjectRecord {
        let root = self._dir.path().join(label);
        tokio::fs::create_dir(&root).await.unwrap();
        let config = ProjectConfig {
            schema_version: 1,
            project_id: new_id(),
            name: label.into(),
            main_agent_profile_id: "main".into(),
            allowed_worker_profile_ids: ["probe".into()].into(),
            integrations: IntegrationConfig::default(),
            limits: DispatchLimits {
                default_timeout_seconds: 15,
                max_timeout_seconds: 60,
                ..Default::default()
            },
        };
        tokio::fs::create_dir(root.join(".agent-scheduler"))
            .await
            .unwrap();
        tokio::fs::write(
            root.join(PROJECT_FILE),
            serde_json::to_vec_pretty(&config).unwrap(),
        )
        .await
        .unwrap();
        serde_json::from_value(
            self.admin()
                .call(Operation::PutProject(ProjectRecord {
                    root,
                    config,
                    revision: String::new(),
                }))
                .await
                .unwrap(),
        )
        .unwrap()
    }
    async fn shutdown(mut self) {
        self.app.shutdown().await;
        for task in self.background.drain(..) {
            task.await.unwrap();
        }
        self.service.take().unwrap().await.unwrap().unwrap();
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.app.request_shutdown();
        if let Some(service) = &self.service {
            service.abort();
        }
        for background in &self.background {
            background.abort();
        }
    }
}

fn submission(key: &str, prompt: Value) -> Operation {
    Operation::Submit(SubmitTask {
        worker_profile_id: "probe".into(),
        prompt: prompt.to_string(),
        idempotency_key: key.into(),
        timeout_seconds: Some(15),
        execution_mode: ExecutionMode::ReadOnly,
        write_scope: vec![],
    })
}
async fn event(receiver: &mut mpsc::UnboundedReceiver<DeliveryEnvelope>) -> DeliveryEnvelope {
    tokio::time::timeout(Duration::from_secs(10), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}
async fn wait_delivery(app: &Scheduler, task_id: &str, expected: DeliveryState) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if app
                .store
                .task(&AuthContext::Admin, task_id)
                .await
                .unwrap()
                .delivery_state
                == expected
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

async fn query(harness: &Harness, arguments: &[&str]) -> Value {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_schedulerctl"));
    command
        .arg("--data-dir")
        .arg(&harness.app.paths.root)
        .args(arguments)
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "schedulerctl {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn cli_queries_project_sessions_and_dispatch_details() {
    let harness = Harness::new().await;
    let project = harness.project("primary project").await;
    let other = harness.project("other project").await;
    let (session, mut results, _) = harness.parent.add(&project);
    let (second_session, _second_results, _) = harness.parent.add(&project);
    let (other_session, _other_results, _) = harness.parent.add(&other);
    let origin = harness.root(&project, &session);
    let project_id = &project.config.project_id;

    // Leading JSON whitespace makes the detail span several IPC chunks while
    // leaving the probe's result small.
    let prompt = format!(
        "{}{}",
        "\t".repeat(220_000),
        json!({"summary":"子 Agent result 🚀","delay_ms":1})
    );
    let first: TaskAccepted = serde_json::from_value(
        origin
            .call(Operation::Submit(SubmitTask {
                worker_profile_id: "probe".into(),
                prompt: prompt.clone(),
                idempotency_key: "first".into(),
                timeout_seconds: Some(15),
                execution_mode: ExecutionMode::ReadOnly,
                write_scope: vec![],
            }))
            .await
            .unwrap(),
    )
    .unwrap();
    let delivery = event(&mut results).await;
    assert_eq!(delivery.task_id, first.task_id);
    wait_delivery(
        &harness.app,
        &first.task_id,
        DeliveryState::AcceptedByTransport,
    )
    .await;

    let detail = query(&harness, &["task", &first.task_id]).await;
    assert_eq!(detail["session_id"], session);
    assert!(
        detail["prompt"].as_str() == Some(prompt.as_str()),
        "large task prompt did not round-trip"
    );
    assert_eq!(detail["task"]["worker_profile_id"], "probe");
    assert_eq!(detail["task"]["execution_state"], "succeeded");
    assert_eq!(detail["task"]["result"]["summary"], "子 Agent result 🚀");
    assert_eq!(detail["task"]["delivery_state"], "accepted_by_transport");

    origin
        .call(Operation::Ack {
            delivery_id: delivery.delivery_id,
            nonce: delivery.ack_nonce,
        })
        .await
        .unwrap();
    let detail = query(&harness, &["task", &first.task_id]).await;
    assert_eq!(detail["task"]["delivery_state"], "acknowledged_by_parent");

    harness
        .root(&project, &second_session)
        .call(submission("independent", json!({"delay_ms":1})))
        .await
        .unwrap();
    harness
        .root(&other, &other_session)
        .call(submission("outside", json!({"delay_ms":1})))
        .await
        .unwrap();
    let latest = origin
        .call(submission("latest", json!({"delay_ms":1})))
        .await
        .unwrap();

    let sessions = query(
        &harness,
        &["sessions", "--project", project_id, "--limit", "1"],
    )
    .await;
    assert_eq!(sessions["items"].as_array().unwrap().len(), 1);
    assert_eq!(sessions["items"][0]["session_id"], session);
    assert_eq!(sessions["items"][0]["task_count"], 2);
    let cursor = sessions["next_cursor"].as_i64().unwrap().to_string();
    let sessions = query(
        &harness,
        &["sessions", "--project", project_id, "--before", &cursor],
    )
    .await;
    assert_eq!(sessions["items"].as_array().unwrap().len(), 1);
    assert_eq!(sessions["items"][0]["session_id"], second_session);
    assert_eq!(sessions["items"][0]["task_count"], 1);
    assert!(sessions["next_cursor"].is_null());

    let mut filters = vec![
        "tasks",
        "--project",
        project_id,
        "--session",
        &session,
        "--limit",
        "1",
    ];
    let tasks = query(&harness, &filters).await;
    assert_eq!(tasks["items"].as_array().unwrap().len(), 1);
    assert_eq!(tasks["items"][0]["task_id"], latest["task_id"]);
    assert_eq!(tasks["items"][0]["worker_profile_id"], "probe");
    let cursor = tasks["next_cursor"].as_i64().unwrap().to_string();
    filters.extend(["--before", &cursor]);
    let tasks = query(&harness, &filters).await;
    assert_eq!(tasks["items"].as_array().unwrap().len(), 1);
    assert_eq!(tasks["items"][0]["task_id"], first.task_id);
    assert_eq!(
        tasks["items"][0]["delivery_state"],
        "acknowledged_by_parent"
    );
    assert!(tasks["next_cursor"].is_null());

    for operation in [
        Operation::ListSessions {
            project_id: project_id.clone(),
            before: None,
            limit: 1,
        },
        Operation::TaskHistory {
            project_id: Some(project_id.clone()),
            session_id: Some(session.clone()),
            before: None,
            limit: 1,
        },
        Operation::TaskDetail {
            task_id: first.task_id,
            offset: 0,
            revision: None,
        },
    ] {
        let error = origin.call(operation).await.unwrap_err();
        assert_eq!(
            error.downcast_ref::<SchedulerError>().unwrap().code,
            "permission_denied"
        );
    }
    harness.shutdown().await;
}

#[tokio::test]
async fn real_ipc_submits_without_waiting_and_pushes_to_each_automatic_origin() {
    let harness = Harness::new().await;
    let project = harness.project("项目 A").await;
    let (a, mut a_results, _) = harness.parent.add(&project);
    let (b, mut b_results, _) = harness.parent.add(&project);
    let gate = project.root.join("release-marker");
    let task_a: TaskAccepted = serde_json::from_value(
        harness
            .root(&project, &a)
            .call(submission(
                "same-key",
                json!({"summary":"A-result","delay_ms":1,"wait_for_file":gate}),
            ))
            .await
            .unwrap(),
    )
    .unwrap();
    // The worker cannot finish until we create the gate, yet submit and another root call already return.
    assert!(a_results.try_recv().is_err());
    let context = harness
        .root(&project, &a)
        .call(Operation::SessionStatus)
        .await
        .unwrap();
    assert_eq!(context["origin"]["route"]["session_id"], a);
    let task_b: TaskAccepted = serde_json::from_value(
        harness
            .root(&project, &b)
            .call(submission(
                "same-key",
                json!({"summary":"B-result","delay_ms":1}),
            ))
            .await
            .unwrap(),
    )
    .unwrap();
    let b_result = event(&mut b_results).await;
    assert_eq!(b_result.task_id, task_b.task_id);
    assert_eq!(b_result.result.summary, "B-result");
    assert!(a_results.try_recv().is_err());
    assert!(
        harness
            .root(&project, &b)
            .call(Operation::GetTask {
                task_id: task_a.task_id.clone()
            })
            .await
            .is_err()
    );
    tokio::fs::write(gate, "release").await.unwrap();
    let a_result = event(&mut a_results).await;
    assert_eq!(a_result.task_id, task_a.task_id);
    assert_eq!(a_result.result.summary, "A-result");
    assert_ne!(task_a.task_id, task_b.task_id);
    harness
        .root(&project, &a)
        .call(Operation::Ack {
            delivery_id: a_result.delivery_id.clone(),
            nonce: a_result.ack_nonce.clone(),
        })
        .await
        .unwrap();
    harness
        .root(&project, &a)
        .call(Operation::Ack {
            delivery_id: a_result.delivery_id,
            nonce: a_result.ack_nonce,
        })
        .await
        .unwrap();
    wait_delivery(
        &harness.app,
        &task_a.task_id,
        DeliveryState::AcknowledgedByParent,
    )
    .await;
    harness.shutdown().await;
}

#[tokio::test]
async fn worker_cannot_use_even_the_operators_credential_and_cross_project_origins_fail() {
    let harness = Harness::new().await;
    let project = harness.project("project-a").await;
    let other = harness.project("project-b").await;
    let (a, mut results, _) = harness.parent.add(&project);
    let (b, _other_results, _) = harness.parent.add(&other);
    assert!(
        harness
            .root(&project, &b)
            .call(Operation::SessionStatus)
            .await
            .is_err()
    );
    let task: TaskAccepted = serde_json::from_value(harness.root(&project, &a).call(submission("role-test",
        json!({"summary":"role check","delay_ms":1,"attempt_control":harness.app.paths.root}))).await.unwrap()).unwrap();
    let result = event(&mut results).await;
    assert_eq!(result.task_id, task.task_id);
    assert_eq!(result.execution_state, ExecutionState::Succeeded);
    assert!(
        result
            .result
            .verification
            .iter()
            .any(|v| v.description == "worker_control_plane_access" && v.outcome == "denied")
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn busy_parent_is_retried_but_an_uncertain_send_is_not_blindly_repeated() {
    let harness = Harness::new().await;
    let project = harness.project("project").await;
    let (a, mut results, mode) = harness.parent.add(&project);
    *mode.lock().unwrap() = 1;
    let first: TaskAccepted = serde_json::from_value(
        harness
            .root(&project, &a)
            .call(submission("busy", json!({"delay_ms":1})))
            .await
            .unwrap(),
    )
    .unwrap();
    wait_delivery(&harness.app, &first.task_id, DeliveryState::RetryWait).await;
    assert!(results.try_recv().is_err());
    *mode.lock().unwrap() = 3;
    let delivered = event(&mut results).await;
    assert_eq!(delivered.task_id, first.task_id);
    wait_delivery(&harness.app, &first.task_id, DeliveryState::Uncertain).await;
    *mode.lock().unwrap() = 0;
    let second: TaskAccepted = serde_json::from_value(
        harness
            .root(&project, &a)
            .call(submission("next", json!({"delay_ms":1})))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(event(&mut results).await.task_id, second.task_id);
    assert!(results.try_recv().is_err());
    harness.shutdown().await;
}

struct McpChild {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: u64,
}
impl McpChild {
    async fn start(project: &ProjectRecord, data: &Path, stale_session: &str) -> Self {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_scheduler-mcp"))
            .args([
                "--project-root",
                project.root.to_str().unwrap(),
                "--data-dir",
                data.to_str().unwrap(),
            ])
            .env("CODEX_THREAD_ID", stale_session)
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut client = Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
            next_id: 1,
        };
        let init = client
            .request(
                "initialize",
                json!({"protocolVersion":"2025-06-18","capabilities":{},
            "clientInfo":{"name":"runspur-integration-test","version":"1"}}),
            )
            .await;
        assert!(init.get("error").is_none());
        client
            .input
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
            .unwrap();
        client
    }
    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.input
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        self.input.flush().await.unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let mut line = String::new();
                assert_ne!(self.output.read_line(&mut line).await.unwrap(), 0);
                let response: Value =
                    serde_json::from_str(&line).expect("MCP stdout must contain only JSON-RPC");
                if response["id"] == id {
                    break response;
                }
            }
        })
        .await
        .unwrap()
    }
    async fn tool(&mut self, name: &str, args: Value, origin: &str) -> Value {
        self.request(
            "tools/call",
            json!({"name":name,"arguments":args,"_meta":{"threadId":origin}}),
        )
        .await
    }
    async fn close(self) {
        let Self {
            mut child, input, ..
        } = self;
        drop(input);
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn actual_mcp_stdio_process_uses_per_call_metadata_for_two_roots() {
    let harness = Harness::new().await;
    let project = harness.project("MCP 项目").await;
    let (a, mut a_results, _) = harness.parent.add(&project);
    let (b, mut b_results, _) = harness.parent.add(&project);
    let mut mcp = McpChild::start(&project, &harness.app.paths.root, &a).await;
    let listing = mcp.request("tools/list", json!({})).await;
    assert!(
        listing["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == "scheduler_submit")
    );
    let missing = mcp
        .request(
            "tools/call",
            json!({"name":"scheduler_context","arguments":{}}),
        )
        .await;
    assert_eq!(missing["result"]["isError"], true);
    assert_eq!(
        missing["result"]["structuredContent"]["code"],
        "origin_session_unavailable"
    );
    let task_a = mcp
        .tool(
            "scheduler_submit",
            json!({"worker_profile_id":"probe","prompt":"MCP A",
        "idempotency_key":"one"}),
            &a,
        )
        .await;
    assert_ne!(task_a["result"]["isError"], true);
    let accepted_a: TaskAccepted =
        serde_json::from_value(task_a["result"]["structuredContent"].clone()).unwrap();
    let task_b = mcp
        .tool(
            "scheduler_submit",
            json!({"worker_profile_id":"probe","prompt":"MCP B",
        "idempotency_key":"one"}),
            &b,
        )
        .await;
    let accepted_b: TaskAccepted =
        serde_json::from_value(task_b["result"]["structuredContent"].clone()).unwrap();
    assert_ne!(accepted_a.task_id, accepted_b.task_id);
    let a_result = event(&mut a_results).await;
    let b_result = event(&mut b_results).await;
    assert_eq!(a_result.task_id, accepted_a.task_id);
    assert_eq!(b_result.task_id, accepted_b.task_id);
    let denied = mcp
        .tool("scheduler_task", json!({"task_id":accepted_a.task_id}), &b)
        .await;
    assert_eq!(denied["result"]["isError"], true);
    let ack = mcp
        .tool(
            "scheduler_ack",
            json!({"delivery_id":a_result.delivery_id,"ack_nonce":a_result.ack_nonce}),
            &a,
        )
        .await;
    assert_ne!(ack["result"]["isError"], true);
    let forged = mcp
        .tool(
            "scheduler_submit",
            json!({"worker_profile_id":"probe","prompt":"x","idempotency_key":"bad","role":"main"}),
            &a,
        )
        .await;
    assert_eq!(forged["result"]["isError"], true);
    mcp.close().await;
    harness.shutdown().await;
}

#[tokio::test]
async fn crash_between_completion_file_and_database_commit_is_recovered_without_execution() {
    let mut harness = Harness::new().await;
    let project = harness.project("recovery").await;
    let (session, _results, _) = harness.parent.add(&project);
    // Stop the execution loop so we can inject this precise crash boundary.
    harness.app.stop.cancel();
    for handle in harness.background.drain(..) {
        handle.abort();
        if let Err(error) = handle.await {
            assert!(error.is_cancelled());
        }
    }
    let scope = harness
        .app
        .resolve_origin(
            &project.config.project_id,
            Some(&OriginSession {
                provider_session_id: session,
                source: OriginSource::RequestMetadata,
            }),
        )
        .await
        .unwrap();
    let accepted = harness
        .app
        .store
        .submit(
            &scope,
            SubmitTask {
                worker_profile_id: "probe".into(),
                prompt: "never run again".into(),
                idempotency_key: "crash-boundary".into(),
                timeout_seconds: None,
                execution_mode: ExecutionMode::ReadOnly,
                write_scope: vec![],
            },
        )
        .await
        .unwrap();
    let spec = harness.app.store.claim_next(2).await.unwrap().unwrap();
    let completion = Completion {
        schema_version: 1,
        task_id: accepted.task_id.clone(),
        attempt_id: spec.attempt_id.clone(),
        execution_state: ExecutionState::Succeeded,
        finished_at_ms: now_ms(),
        result: TaskResult {
            summary: "already completed".into(),
            exit_code: Some(0),
            error_class: None,
            verification: vec![],
            artifact_refs: vec![],
        },
    };
    let path = harness
        .app
        .paths
        .attempt_dir(&accepted.task_id, &spec.attempt_id)
        .unwrap()
        .join("completion.json");
    scheduler_runtime::durable_file(&path, &serde_json::to_vec(&completion).unwrap())
        .await
        .unwrap();
    harness.app.recover().await.unwrap();
    let recovered = harness
        .app
        .store
        .task(&scope, &accepted.task_id)
        .await
        .unwrap();
    assert_eq!(recovered.execution_state, ExecutionState::Succeeded);
    assert_eq!(recovered.result.unwrap().summary, "already completed");
    assert_eq!(recovered.delivery_state, DeliveryState::Pending);
    assert!(harness.app.store.claim_next(2).await.unwrap().is_none());
    assert_eq!(harness.app.store.due_deliveries(10).await.unwrap().len(), 1);
    harness.app.recover().await.unwrap();
    assert_eq!(harness.app.store.due_deliveries(10).await.unwrap().len(), 1);
    harness.shutdown().await;
}
