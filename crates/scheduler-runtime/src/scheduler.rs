use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use scheduler_core::*;
use scheduler_platform::{CredentialStore, DataPaths, SecretKey, WorkerRegistry, is_running};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::{CodexDesktopAdapter, DeliveryOutcome, ParentAdapter, failed, run_worker};

pub struct Scheduler {
    pub store: Store,
    pub paths: DataPaths,
    pub workers: WorkerRegistry,
    pub stop: CancellationToken,
    credentials: CredentialStore,
    master_key: SecretKey,
    parents: HashMap<String, Arc<dyn ParentAdapter>>,
    active: Mutex<HashMap<String, CancellationToken>>,
    wake_tasks: Notify,
    wake_delivery: Notify,
    slots: Arc<Semaphore>,
}

impl Scheduler {
    pub fn new(paths: DataPaths, max_workers: usize) -> Result<Arc<Self>> {
        Self::with_parents(
            paths,
            max_workers,
            vec![Arc::new(CodexDesktopAdapter::default())],
        )
    }
    pub fn with_parents(
        paths: DataPaths,
        max_workers: usize,
        parents: Vec<Arc<dyn ParentAdapter>>,
    ) -> Result<Arc<Self>> {
        if !(1..=64).contains(&max_workers) {
            bail!("Global worker limit must be between 1 and 64");
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
            parents: parents
                .into_iter()
                .map(|p| (p.id().to_owned(), p))
                .collect(),
            active: Mutex::new(HashMap::new()),
            wake_tasks: Notify::new(),
            wake_delivery: Notify::new(),
            slots: Arc::new(Semaphore::new(max_workers)),
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
        let project = self.store.project(project_id).await?;
        let profile = self
            .store
            .profile(&project.config.main_agent_profile_id)
            .await?;
        authorize_origin(&project.config, &profile.profile_id, origin, false)?;
        let origin = origin.context("Origin was not available")?;
        let adapter = self.adapter_for_profile(&profile)?;
        let resolved = adapter.resolve(&project, origin).await?;
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
        let id = match profile.kind {
            AgentKind::CodexDesktop => "codex-desktop",
            _ => {
                return Err(SchedulerError::new(
                    "main_entrypoint_unsupported",
                    "This profile has no verified automatic parent-return adapter",
                )
                .into());
            }
        };
        self.parents
            .get(id)
            .cloned()
            .context("Parent adapter is unavailable on this build")
    }

    pub async fn handle(&self, auth: &AuthContext, operation: Operation) -> Result<Value> {
        let value = match operation {
            Operation::Health => {
                json!({"version":env!("CARGO_PKG_VERSION"),"protocol_version":WIRE_VERSION,
                "platform":std::env::consts::OS,"active_workers":self.active.lock().map_err(|_| anyhow::anyhow!("Active registry poisoned"))?.len()})
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
                    json!(
                        profiles
                            .into_iter()
                            .filter(|p| p.profile_id == project.config.main_agent_profile_id
                                || project
                                    .config
                                    .allowed_worker_profile_ids
                                    .contains(&p.profile_id))
                            .map(|p| json!({"profile_id":p.profile_id,
                            "kind":p.kind,"worker":p.kind != AgentKind::CodexDesktop}))
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
                self.adapter_for_profile(&profile)?;
                for worker in &project.config.allowed_worker_profile_ids {
                    if self.store.profile(worker).await?.kind == AgentKind::CodexDesktop {
                        return Err(SchedulerError::new(
                            "worker_not_supported",
                            "Desktop parent cannot be in the worker pool",
                        )
                        .into());
                    }
                }
                let saved = self.store.put_project(project).await?;
                self.credentials.ensure(&CredentialRef::Project {
                    project_id: saved.config.project_id.clone(),
                })?;
                json!(saved)
            }
            Operation::ListProjects => {
                auth.require_admin()?;
                json!(self.store.projects().await?)
            }
            Operation::SessionStatus => {
                let (project, registration) = auth.root()?;
                json!({"project":self.store.project(project).await?, "origin":self.store.registration(registration).await?})
            }
            Operation::Submit(submission) => {
                let task = self.store.submit(auth, submission).await?;
                self.wake_tasks.notify_one();
                json!(task)
            }
            Operation::GetTask { task_id } => json!(self.store.task(auth, &task_id).await?),
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
                let task = match self.store.claim_next().await {
                    Ok(Some(task)) => task,
                    Ok(None) => break,
                    Err(error) => {
                        tracing::error!(%error, "Cannot claim queued task");
                        break;
                    }
                };
                let token = CancellationToken::new();
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
        let mut completion =
            match run_worker(&task, &self.paths, &self.workers, token, &self.store).await {
                Ok(completion) => completion,
                Err(error) => failed(
                    &task,
                    "worker_start_failed",
                    crate::redact_text(&error.to_string()),
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
        let origin = OriginSession {
            provider_session_id: delivery.registration.route.session_id.clone(),
            source: OriginSource::RequestMetadata,
        };
        match adapter.resolve(&project, &origin).await {
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
        if let Ok(active) = self.active.lock() {
            for token in active.values() {
                token.cancel();
            }
        }
    }
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
