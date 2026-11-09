use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use scheduler_core::*;
use scheduler_platform::{
    CredentialStore, DataPaths, SecretKey, WorkerRegistry, identity, is_running,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::claude_inbox::{ClaudeInboxRegistry, is_native_route};
use crate::claude_session::ClaudeSessionAdapter;
use crate::{
    CodexDesktopAdapter, DeliveryOutcome, ParentAdapter, ProcessWorkerAdapter, WorkerAdapter,
    WorkerContext, failed,
};

pub struct Scheduler {
    pub store: Store,
    pub paths: DataPaths,
    pub workers: WorkerRegistry,
    pub stop: CancellationToken,
    credentials: CredentialStore,
    master_key: SecretKey,
    parents: HashMap<String, Arc<dyn ParentAdapter>>,
    parent_kinds: Arc<HashMap<AgentKind, Arc<dyn ParentAdapter>>>,
    claude_inbox: Option<Arc<ClaudeInboxRegistry>>,
    worker_adapters: Arc<HashMap<AgentKind, Arc<dyn WorkerAdapter>>>,
    active: Mutex<HashMap<String, CancellationToken>>,
    wake_tasks: Notify,
    wake_delivery: Notify,
    default_max_workers: usize,
    slots: Arc<Semaphore>,
}

impl Scheduler {
    pub fn new(paths: DataPaths, max_workers: usize) -> Result<Arc<Self>> {
        let inbox = Arc::new(ClaudeInboxRegistry::default());
        let claude = ClaudeSessionAdapter(paths.clone(), inbox.clone());
        Self::build(
            paths,
            max_workers,
            vec![Arc::new(CodexDesktopAdapter::default()), Arc::new(claude)],
            process_workers(),
            Some(inbox),
        )
    }
    pub fn with_parents(
        paths: DataPaths,
        max_workers: usize,
        parents: Vec<Arc<dyn ParentAdapter>>,
    ) -> Result<Arc<Self>> {
        Self::with_adapters(paths, max_workers, parents, process_workers())
    }

    pub fn with_adapters(
        paths: DataPaths,
        max_workers: usize,
        parents: Vec<Arc<dyn ParentAdapter>>,
        workers: Vec<Arc<dyn WorkerAdapter>>,
    ) -> Result<Arc<Self>> {
        Self::build(paths, max_workers, parents, workers, None)
    }

    fn build(
        paths: DataPaths,
        max_workers: usize,
        parents: Vec<Arc<dyn ParentAdapter>>,
        workers: Vec<Arc<dyn WorkerAdapter>>,
        claude_inbox: Option<Arc<ClaudeInboxRegistry>>,
    ) -> Result<Arc<Self>> {
        if !(1..=MAX_SCHEDULER_WORKERS).contains(&max_workers) {
            bail!("Global worker limit must be between 1 and {MAX_SCHEDULER_WORKERS}");
        }
        let mut parent_ids = HashMap::new();
        let mut parent_kinds = HashMap::new();
        for parent in parents {
            if parent_ids
                .insert(parent.id().into(), parent.clone())
                .is_some()
                || parent_kinds.insert(parent.profile_kind(), parent).is_some()
            {
                bail!("Duplicate parent adapter or profile kind");
            }
        }
        let mut worker_adapters = HashMap::new();
        for worker in workers {
            if worker_adapters
                .insert(worker.profile_kind(), worker)
                .is_some()
            {
                bail!("Duplicate worker adapter profile kind");
            }
        }
        let credentials = CredentialStore::new(paths.clone());
        let master_key = credentials.ensure(&CredentialRef::Admin)?;
        let store = Store::open(&paths.database)?;
        Ok(Arc::new(Self {
            store,
            paths,
            credentials,
            master_key,
            workers: WorkerRegistry::default(),
            stop: CancellationToken::new(),
            parents: parent_ids,
            parent_kinds: Arc::new(parent_kinds),
            claude_inbox,
            worker_adapters: Arc::new(worker_adapters),
            active: Mutex::new(HashMap::new()),
            wake_tasks: Notify::new(),
            wake_delivery: Notify::new(),
            default_max_workers: max_workers,
            slots: Arc::new(Semaphore::new(MAX_SCHEDULER_WORKERS)),
        }))
    }

    pub fn credentials(&self) -> &CredentialStore {
        &self.credentials
    }

    pub async fn resolve_origin(
        &self,
        project_id: &str,
        origin: Option<&OriginSession>,
    ) -> Result<AuthContext> {
        self.resolve_origin_with_peer(project_id, origin, None)
            .await
            .map(|(auth, _)| auth)
    }

    pub(crate) async fn resolve_origin_with_peer(
        &self,
        project_id: &str,
        origin: Option<&OriginSession>,
        peer: Option<&ProcessIdentity>,
    ) -> Result<(AuthContext, Option<MainPermission>)> {
        let project = self.store.project(project_id).await?;
        let profile = self
            .store
            .profile(&project.config.main_agent_profile_id)
            .await?;
        authorize_origin(&project.config, &profile.profile_id, origin, false)?;
        let origin = origin.context("Origin was not available")?;
        let adapter = self.adapter_for_profile(&profile)?;
        let resolved = match peer {
            Some(peer) => adapter.resolve_from_peer(&project, origin, peer).await?,
            None => adapter.resolve(&project, origin).await?,
        };
        if resolved.route.adapter_id != adapter.id() {
            bail!("Parent adapter returned an invalid route");
        }
        let main_permission = resolved.main_permission;
        let auth = self.register_origin(project_id, &profile, resolved).await?;
        Ok((auth, main_permission))
    }

    pub(crate) async fn register_claude_hook(
        &self,
        project_id: &str,
        origin: &OriginSession,
        hook: ClaudeHookCall,
        peer: &ProcessIdentity,
    ) -> Result<Value> {
        let inbox = self
            .claude_inbox
            .as_ref()
            .context("Native Claude Code inbox support is unavailable")?;
        let project = self.store.project(project_id).await?;
        let profile = self
            .store
            .profile(&project.config.main_agent_profile_id)
            .await?;
        authorize_origin(&project.config, &profile.profile_id, Some(origin), false)?;
        if profile.kind != AgentKind::ClaudePrint {
            bail!("Claude Code is not this project's main Agent");
        }
        let program = profile
            .program
            .as_deref()
            .context("This Claude Code profile has no executable")?;
        let session = uuid::Uuid::parse_str(&origin.provider_session_id)
            .context("Invalid Claude Code session ID")?;
        if self
            .adapter_for_profile(&profile)?
            .resolve(&project, origin)
            .await
            .is_ok()
        {
            bail!("This Claude Code session already has a managed owner");
        }
        inbox
            .register(&project, session, hook, peer, program, &self.paths.root)
            .await?;
        Ok(json!({"registered":true}))
    }

    pub(crate) async fn resolve_claude_call(
        &self,
        project_id: &str,
        origin: &OriginSession,
        peer: &ProcessIdentity,
    ) -> Result<(AuthContext, String, Option<MainPermission>)> {
        let inbox = self
            .claude_inbox
            .as_ref()
            .context("Native Claude Code inbox support is unavailable")?;
        let project = self.store.project(project_id).await?;
        let profile = self
            .store
            .profile(&project.config.main_agent_profile_id)
            .await?;
        authorize_origin(&project.config, &profile.profile_id, Some(origin), false)?;
        if profile.kind != AgentKind::ClaudePrint {
            bail!("Claude Code is not this project's main Agent");
        }
        let (resolved, operation_sha256) =
            inbox.consume(&project, &origin.provider_session_id, peer)?;
        let main_permission = resolved.main_permission;
        let auth = self.register_origin(project_id, &profile, resolved).await?;
        Ok((auth, operation_sha256, main_permission))
    }

    async fn register_origin(
        &self,
        project_id: &str,
        profile: &AgentProfile,
        resolved: crate::ResolvedParent,
    ) -> Result<AuthContext> {
        let registration = self
            .store
            .ensure_registration(
                project_id,
                &profile.profile_id,
                resolved.route,
                &resolved.generation,
                resolved.workspace_root,
            )
            .await?;
        Ok(AuthContext::Root {
            project_id: project_id.into(),
            registration_id: registration.registration_id,
        })
    }

    fn adapter_for_profile(&self, profile: &AgentProfile) -> Result<Arc<dyn ParentAdapter>> {
        Ok(parent_adapter(&self.parent_kinds, profile)?.clone())
    }

    async fn scheduler_settings(&self) -> Result<SchedulerSettings> {
        Ok(self
            .store
            .scheduler_settings(self.default_max_workers)
            .await?)
    }

    pub async fn handle(&self, auth: &AuthContext, operation: Operation) -> Result<Value> {
        self.handle_with_main_permission(auth, operation, None)
            .await
    }

    pub(crate) async fn handle_with_main_permission(
        &self,
        auth: &AuthContext,
        operation: Operation,
        main_permission: Option<MainPermission>,
    ) -> Result<Value> {
        let value = match operation {
            Operation::Health => {
                json!({"version":env!("CARGO_PKG_VERSION"),"protocol_version":WIRE_VERSION,
                "platform":std::env::consts::OS,"active_workers":self.active.lock().map_err(|_| anyhow::anyhow!("Active registry poisoned"))?.len()})
            }
            Operation::Shutdown => {
                auth.require_admin()?;
                serde_json::to_value(identity(std::process::id())?)?
            }
            Operation::GetSchedulerSettings => {
                auth.require_admin()?;
                json!(self.scheduler_settings().await?)
            }
            Operation::SetSchedulerSettings {
                max_concurrent_workers,
                worker_access,
            } => {
                auth.require_admin()?;
                let settings = self
                    .store
                    .set_scheduler_settings(
                        max_concurrent_workers,
                        worker_access,
                        self.default_max_workers,
                    )
                    .await?;
                self.wake_tasks.notify_one();
                json!(settings)
            }
            Operation::PutProfile(profile) => {
                auth.require_admin()?;
                self.store.put_profile(profile).await?;
                json!({"saved":true})
            }
            Operation::ListProfiles => {
                let profiles = self.store.profiles().await?;
                if let AuthContext::Root { project_id, .. } = auth {
                    let project = self.store.project(project_id).await?;
                    let access = self.scheduler_settings().await?.worker_access;
                    json!(
                        profiles
                            .into_iter()
                            .filter(|p| p.profile_id == project.config.main_agent_profile_id
                                || project
                                    .config
                                    .allowed_worker_profile_ids
                                    .contains(&p.profile_id))
                            .map(|p| {
                                let adapter = self.worker_adapters.get(&p.kind).filter(|_| {
                                    project
                                        .config
                                        .allowed_worker_profile_ids
                                        .contains(&p.profile_id)
                                });
                                let native_full = adapter.is_some()
                                    && access.permits_native_full(main_permission, &p.kind);
                                let read_only = access == WorkerAccess::FollowMain
                                    && main_permission == Some(MainPermission::ReadOnly);
                                let modes = [
                                    ExecutionMode::ReadOnly,
                                    ExecutionMode::WorkspaceWrite,
                                    ExecutionMode::ProjectWrite,
                                ]
                                .into_iter()
                                .filter(|mode| {
                                    native_full
                                        || adapter.is_some_and(|worker| {
                                            (!read_only || mode.is_read_only())
                                                && project.config.limits.permits_execution(*mode)
                                                && worker.supports(*mode)
                                        })
                                })
                                .collect::<Vec<_>>();
                                json!({"profile_id":p.profile_id,"kind":p.kind,
                                    "worker": adapter.is_some(),
                                    "native_full_access": native_full,
                                    "execution_modes": modes})
                            })
                            .collect::<Vec<_>>()
                    )
                } else {
                    json!(profiles)
                }
            }
            Operation::PutProject(mut project) => {
                auth.require_admin()?;
                project.root = tokio::fs::canonicalize(&project.root)
                    .await
                    .context("Project root is not an existing directory")?;
                if !project.root.is_dir() {
                    bail!("Project root is not a directory");
                }
                validate_project(&project.config)?;
                let profile = self
                    .store
                    .profile(&project.config.main_agent_profile_id)
                    .await?;
                let parent = self.adapter_for_profile(&profile)?;
                for worker in &project.config.allowed_worker_profile_ids {
                    let worker_profile = self.store.profile(worker).await?;
                    let worker = worker_adapter(&self.worker_adapters, &worker_profile)?;
                    validate_pair(parent.as_ref(), worker.as_ref())?;
                }
                self.credentials.ensure(&CredentialRef::Project {
                    project_id: project.config.project_id.clone(),
                })?;
                let saved = self.store.put_project(project).await?;
                json!(saved)
            }
            Operation::ListProjects => {
                auth.require_admin()?;
                json!(self.store.projects().await?)
            }
            Operation::SessionStatus => {
                let (project, registration) = auth.root()?;
                let origin = self.store.registration(registration).await?;
                json!({"project":self.store.project(project).await?, "scheduler":self.scheduler_settings().await?, "origin":{
                    "origin_id": origin.registration_id,
                    "workspace_root": origin.workspace_root,
                    "route": {"adapter_id": origin.route.adapter_id, "session_id": origin.route.session_id}
                }})
            }
            Operation::Submit(submission) => {
                let parents = self.parent_kinds.clone();
                let workers = self.worker_adapters.clone();
                let task = self
                    .store
                    .submit_with_worker_policy(
                        auth,
                        submission,
                        main_permission,
                        move |main, worker, task, origin, native_full| {
                            let parent = parent_adapter(&parents, main)?;
                            if origin.route.adapter_id != parent.id() {
                                return Err(SchedulerError::new(
                                    "main_profile_mismatch",
                                    "Origin adapter no longer matches the main profile",
                                ));
                            }
                            let worker = worker_adapter(&workers, worker)?;
                            validate_pair(parent.as_ref(), worker.as_ref())?;
                            if !native_full && !worker.supports(task.execution_mode) {
                                return Err(SchedulerError::new(
                                    "execution_mode_unsupported",
                                    "Selected child Agent does not support this execution mode",
                                ));
                            }
                            Ok(())
                        },
                    )
                    .await?;
                self.wake_tasks.notify_one();
                json!(task)
            }
            Operation::GetTask { task_id } => json!(self.store.task(auth, &task_id).await?),
            Operation::ClaimClaudeDelivery { delivery_id } => {
                let (_, origin_id) = auth.root()?;
                let registration = self.store.registration(origin_id).await?;
                if !is_native_route(&registration.route) {
                    bail!("Result claims are available to the native Claude Code origin only");
                }
                let task = self.store.task_for_delivery(auth, &delivery_id).await?;
                json!(DeliveryEnvelope {
                    schema_version: SCHEMA_VERSION,
                    kind: "task_completed".into(),
                    task_id: task.task_id,
                    delivery_id: task.delivery_id,
                    project_id: task.project_id,
                    workspace_root: task.workspace_root,
                    execution_state: task.execution_state,
                    result: task.result.context("Terminal task has no result")?,
                    ack_nonce: self.master_key.ack_nonce(&delivery_id),
                })
            }
            Operation::RegisterClaudeHook(_) => {
                bail!("This request requires a verified Claude Code hook connection")
            }
            Operation::ListTasks { limit } => {
                let mut tasks = self.store.list_tasks(auth, limit.min(50)).await?;
                for task in &mut tasks {
                    if let Some(result) = &mut task.result {
                        result.summary = preview(&result.summary, 2000);
                        result.verification.truncate(8);
                    }
                }
                json!(tasks)
            }
            Operation::ListSessions {
                project_id,
                before,
                limit,
            } => {
                json!(
                    self.store
                        .list_sessions(auth, &project_id, before, limit)
                        .await?
                )
            }
            Operation::TaskHistory {
                project_id,
                session_id,
                before,
                limit,
            } => {
                json!(
                    self.store
                        .task_history(
                            auth,
                            project_id.as_deref(),
                            session_id.as_deref(),
                            before,
                            limit
                        )
                        .await?
                )
            }
            Operation::OriginTaskHistory {
                project_id,
                origin_id,
                before,
                limit,
            } => {
                json!(
                    self.store
                        .origin_task_history(auth, &project_id, &origin_id, before, limit)
                        .await?
                )
            }
            Operation::TaskDetail {
                task_id,
                offset,
                revision,
            } => {
                let detail = self.store.task_detail(auth, &task_id).await?;
                let detail = serde_json::to_string(&detail)?;
                json!(TaskDetailChunk::from_json(
                    &detail,
                    offset,
                    revision.as_deref()
                )?)
            }
            Operation::CancelTask { task_id } => {
                let task = self.store.request_cancel(auth, &task_id).await?;
                if let Some(token) = self
                    .active
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Active registry poisoned"))?
                    .get(&task_id)
                {
                    token.cancel();
                }
                self.wake_tasks.notify_one();
                json!(task)
            }
            Operation::Ack { delivery_id, nonce } => {
                json!(self.store.acknowledge(auth, &delivery_id, &nonce).await?)
            }
        };
        Ok(value)
    }

    pub async fn recover(&self) -> Result<()> {
        self.store.mark_interrupted_deliveries_uncertain().await?;
        for (task, process) in self.store.unfinished().await? {
            let path = self
                .paths
                .attempt_dir(&task.view.task_id, &task.attempt_id)?
                .join("completion.json");
            let completion = if path.exists() {
                let bytes = tokio::fs::read(&path).await?;
                if bytes.len() > MAX_FRAME_BYTES {
                    bail!("Staged completion exceeds the contract bound");
                }
                let completion: Completion = serde_json::from_slice(&bytes)?;
                if completion.task_id != task.view.task_id
                    || completion.attempt_id != task.attempt_id
                {
                    bail!("Staged completion does not match its recorded task and attempt");
                }
                completion
            } else {
                let alive = process.as_ref().is_some_and(is_running);
                if let Some(process) = process.filter(|_| alive) {
                    self.workers.block_orphan(process)?;
                }
                failed(&task, if alive { "recovery_required" } else { "daemon_restarted" },
                    if alive { "Daemon restarted. The recorded worker may still be alive; it was not rerun." }
                    else { "Daemon restarted before a durable completion was available. The task was not rerun." }.into())
            };
            self.persist_completion(&completion).await?;
        }
        Ok(())
    }

    async fn persist_completion(&self, completion: &Completion) -> Result<()> {
        let path = self
            .paths
            .attempt_dir(&completion.task_id, &completion.attempt_id)?
            .join("completion.json");
        let bytes = serde_json::to_vec(completion)?;
        if bytes.len() > MAX_FRAME_BYTES {
            bail!("Completion exceeds the contract bound");
        }
        let digest = hex::encode(Sha256::digest(&bytes));
        durable_file(&path, &bytes).await?;
        let task = self
            .store
            .task(&AuthContext::Admin, &completion.task_id)
            .await?;
        let nonce = self.master_key.ack_nonce(&task.delivery_id);
        self.store
            .commit_completion(
                completion.clone(),
                path.to_string_lossy().into(),
                digest,
                nonce_hash(&nonce),
            )
            .await?;
        self.wake_delivery.notify_one();
        Ok(())
    }

    pub fn start(self: &Arc<Self>) -> Vec<tokio::task::JoinHandle<()>> {
        let tasks = self.clone();
        let delivery = self.clone();
        vec![
            tokio::spawn(async move {
                tasks.task_loop().await;
            }),
            tokio::spawn(async move {
                delivery.delivery_loop().await;
            }),
        ]
    }

    async fn task_loop(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = self.stop.cancelled() => break,
                _ = self.wake_tasks.notified() => {},
                _ = tokio::time::sleep(Duration::from_millis(200)) => {},
            }
            if let Err(error) = self.workers.reap() {
                tracing::warn!(%error, "Worker cleanup inspection failed");
            }
            loop {
                if self.stop.is_cancelled() {
                    break;
                }
                let Ok(permit) = self.slots.clone().try_acquire_owned() else {
                    break;
                };
                let task = match self.store.claim_next(self.default_max_workers).await {
                    Ok(Some(task)) => task,
                    Ok(None) => break,
                    Err(error) => {
                        tracing::error!(%error, "Cannot claim queued task");
                        break;
                    }
                };
                let token = self.stop.child_token();
                if let Ok(mut active) = self.active.lock() {
                    active.insert(task.view.task_id.clone(), token.clone());
                }
                let scheduler = self.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    scheduler.run_one(task, token).await;
                });
            }
        }
    }

    async fn run_one(self: Arc<Self>, task: QueuedTask, token: CancellationToken) {
        if self
            .store
            .is_cancel_requested(&task.view.task_id)
            .await
            .unwrap_or(true)
        {
            token.cancel();
        }
        let mut completion = match worker_adapter(&self.worker_adapters, &task.profile) {
            Ok(adapter) if adapter.supports(task.execution_mode) => match adapter
                .run(
                    &task,
                    WorkerContext {
                        paths: &self.paths,
                        processes: &self.workers,
                        store: &self.store,
                    },
                    token,
                )
                .await
            {
                Ok(completion) => completion,
                Err(error) => failed(
                    &task,
                    "worker_start_failed",
                    crate::redact_text(&error.to_string()),
                ),
            },
            _ => failed(
                &task,
                "execution_mode_unsupported",
                "No installed worker supports this task".into(),
            ),
        };
        completion.result.summary = preview(&completion.result.summary, MAX_RESULT_BYTES);
        completion.result.verification.truncate(64);
        for verification in &mut completion.result.verification {
            verification.description = preview(&verification.description, 512);
            verification.outcome = preview(&verification.outcome, 512);
        }
        loop {
            match self.persist_completion(&completion).await {
                Ok(()) => break,
                Err(error) => {
                    tracing::error!(task_id=%task.view.task_id, %error, "Completion persistence pending; retaining the result and concurrency slot");
                    tokio::select! { _ = self.stop.cancelled() => break, _ = tokio::time::sleep(Duration::from_secs(1)) => {} }
                }
            }
        }
        if let Ok(mut active) = self.active.lock() {
            active.remove(&task.view.task_id);
        }
        self.wake_tasks.notify_one();
    }

    async fn delivery_loop(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = self.stop.cancelled() => break,
                _ = self.wake_delivery.notified() => {},
                _ = tokio::time::sleep(Duration::from_millis(500)) => {},
            }
            match self.store.due_deliveries(20).await {
                Ok(deliveries) => {
                    for delivery in deliveries {
                        if self.stop.is_cancelled() {
                            break;
                        }
                        if let Err(error) = self.deliver_one(delivery).await {
                            tracing::error!(%error, "Outbox processing failed");
                        }
                    }
                }
                Err(error) => tracing::error!(%error, "Cannot read outbox"),
            }
        }
    }

    async fn deliver_one(&self, mut delivery: PendingDelivery) -> Result<()> {
        let task_id = delivery.task.task_id.clone();
        let backoff = (500_i64 * (1_i64 << delivery.attempts.min(6))).min(30_000);
        let Some(adapter) = self.parents.get(&delivery.registration.route.adapter_id) else {
            self.store
                .set_delivery(
                    &task_id,
                    DeliveryState::Refused,
                    Some("Parent adapter is not installed".into()),
                    None,
                    0,
                )
                .await?;
            return Ok(());
        };
        let spec = self.store.specification(&task_id).await?;
        let project = ProjectRecord {
            root: spec.project_root,
            config: spec.project_config,
            revision: String::new(),
        };
        match adapter.refresh(&project, &delivery.registration).await {
            Ok(resolved) => {
                delivery.registration = self
                    .store
                    .refresh_route(
                        &delivery.registration.registration_id,
                        resolved.route,
                        resolved.generation,
                    )
                    .await?;
            }
            Err(error) => {
                self.store
                    .set_delivery(
                        &task_id,
                        DeliveryState::Orphaned,
                        Some(crate::redact_text(&error.to_string())),
                        None,
                        now_ms() + 30_000,
                    )
                    .await?;
                return Ok(());
            }
        }
        if !self.store.begin_delivery(&task_id).await? {
            return Ok(());
        }
        let envelope = DeliveryEnvelope {
            schema_version: SCHEMA_VERSION,
            kind: "task_completed".into(),
            task_id: task_id.clone(),
            delivery_id: delivery.task.delivery_id.clone(),
            project_id: delivery.task.project_id.clone(),
            workspace_root: delivery.task.workspace_root.clone(),
            execution_state: delivery.task.execution_state,
            result: delivery
                .task
                .result
                .context("Terminal task has no result")?,
            ack_nonce: self.master_key.ack_nonce(&delivery.task.delivery_id),
        };
        let (state, error, turn, retry) = match adapter
            .deliver(&delivery.registration, &envelope)
            .await
        {
            DeliveryOutcome::Accepted { parent_turn_id } => (
                DeliveryState::AcceptedByTransport,
                None,
                Some(parent_turn_id),
                0,
            ),
            DeliveryOutcome::AcceptedWithoutReference => {
                (DeliveryState::AcceptedByTransport, None, None, 0)
            }
            DeliveryOutcome::Busy => (
                DeliveryState::RetryWait,
                Some("Parent is working; result remains queued".into()),
                None,
                now_ms() + backoff,
            ),
            DeliveryOutcome::Held(message) => {
                (DeliveryState::Held, Some(message), None, now_ms() + backoff)
            }
            DeliveryOutcome::Refused(message) => (DeliveryState::Refused, Some(message), None, 0),
            DeliveryOutcome::Unavailable(message) => (
                DeliveryState::Orphaned,
                Some(message),
                None,
                now_ms() + 30_000,
            ),
            DeliveryOutcome::Uncertain(message) => {
                (DeliveryState::Uncertain, Some(message), None, 0)
            }
        };
        self.store
            .set_delivery(
                &task_id,
                state,
                error.map(|e| crate::redact_text(&e)),
                turn,
                retry,
            )
            .await?;
        Ok(())
    }

    pub async fn shutdown(&self) {
        self.request_shutdown();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if self.active.lock().map(|v| v.is_empty()).unwrap_or(false)
                || tokio::time::Instant::now() >= deadline
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub fn request_shutdown(&self) {
        self.stop.cancel();
    }
}

fn parent_adapter<'a>(
    adapters: &'a HashMap<AgentKind, Arc<dyn ParentAdapter>>,
    profile: &AgentProfile,
) -> scheduler_core::Result<&'a Arc<dyn ParentAdapter>> {
    adapters.get(&profile.kind).ok_or_else(|| {
        SchedulerError::new(
            "main_entrypoint_unsupported",
            "This profile has no verified parent adapter",
        )
    })
}

fn process_workers() -> Vec<Arc<dyn WorkerAdapter>> {
    vec![
        Arc::new(ProcessWorkerAdapter(AgentKind::CodexExec, "codex")),
        Arc::new(ProcessWorkerAdapter(AgentKind::ClaudePrint, "claude")),
        Arc::new(ProcessWorkerAdapter(AgentKind::Probe, "diagnostic")),
    ]
}

fn worker_adapter<'a>(
    adapters: &'a HashMap<AgentKind, Arc<dyn WorkerAdapter>>,
    profile: &AgentProfile,
) -> scheduler_core::Result<&'a Arc<dyn WorkerAdapter>> {
    adapters.get(&profile.kind).ok_or_else(|| {
        SchedulerError::new(
            "worker_not_supported",
            "This profile has no installed worker adapter",
        )
    })
}

fn validate_pair(
    parent: &dyn ParentAdapter,
    worker: &dyn WorkerAdapter,
) -> scheduler_core::Result<()> {
    if parent.family() == worker.family() {
        return Err(SchedulerError::new(
            "same_agent_family",
            "Select a child Agent from a different family than the main Agent",
        ));
    }
    Ok(())
}

pub async fn durable_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    if path.exists() {
        if tokio::fs::read(path).await? != bytes {
            bail!("Immutable completion file conflicts with existing content");
        }
        return Ok(());
    }
    let parent = path
        .parent()
        .context("Completion has no parent directory")?;
    tokio::fs::create_dir_all(parent).await?;
    let staging = parent.join(format!(".{}.tmp", new_id()));
    let result = async {
        let mut file = tokio::fs::File::create_new(&staging).await?;
        file.write_all(bytes).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&staging, path).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&staging).await;
    }
    result
}

pub fn preview(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.into();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated]", &text[..end])
}
