use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::*;

#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
}

fn encode<T: Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}
fn decode<T: DeserializeOwned>(value: String) -> Result<T> {
    Ok(serde_json::from_str(&value)?)
}
fn state<T: Serialize>(value: T) -> Result<String> {
    serde_json::to_value(value)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| SchedulerError::new("invalid_state", "State did not serialize as a string"))
}
pub fn fingerprint(value: &impl Serialize) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(value)?)))
}
pub fn nonce_hash(nonce: &str) -> String {
    hex::encode(Sha256::digest(nonce.as_bytes()))
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version != 0 && version != 1 {
            return Err(SchedulerError::new(
                "unsupported_store_version",
                "Database belongs to an unsupported schema version",
            ));
        }
        let tx = connection.transaction()?;
        tx.execute_batch("\
            CREATE TABLE IF NOT EXISTS projects (
              id TEXT PRIMARY KEY, root TEXT NOT NULL UNIQUE, revision TEXT NOT NULL, body TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS profiles (id TEXT PRIMARY KEY, body TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS registrations (
              id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id),
              adapter_id TEXT NOT NULL, session_id TEXT NOT NULL, body TEXT NOT NULL,
              UNIQUE(project_id, adapter_id, session_id));
            CREATE TABLE IF NOT EXISTS tasks (
              sequence INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE, project_id TEXT NOT NULL REFERENCES projects(id),
              registration_id TEXT NOT NULL REFERENCES registrations(id), root_task_id TEXT NOT NULL,
              idempotency_key TEXT NOT NULL, fingerprint TEXT NOT NULL, spec TEXT NOT NULL,
              body TEXT NOT NULL, execution_state TEXT NOT NULL, created_at_ms INTEGER NOT NULL,
              cancel_requested INTEGER NOT NULL DEFAULT 0,
              UNIQUE(registration_id, idempotency_key));
            CREATE INDEX IF NOT EXISTS tasks_queue ON tasks(execution_state, created_at_ms, sequence);
            CREATE TABLE IF NOT EXISTS attempts (
              id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id),
              pid INTEGER, process_start INTEGER, started_at_ms INTEGER, finished_at_ms INTEGER,
              lease_until_ms INTEGER);
            CREATE TABLE IF NOT EXISTS results (
              task_id TEXT PRIMARY KEY REFERENCES tasks(id), attempt_id TEXT NOT NULL,
              sha256 TEXT NOT NULL, completion_path TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS outbox (
              delivery_id TEXT PRIMARY KEY, task_id TEXT NOT NULL UNIQUE REFERENCES tasks(id),
              registration_id TEXT NOT NULL REFERENCES registrations(id), target_generation TEXT NOT NULL,
              state TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, next_retry_at_ms INTEGER NOT NULL,
              nonce_hash TEXT NOT NULL, last_error TEXT, delivery_attempt_id TEXT);
            CREATE INDEX IF NOT EXISTS outbox_due ON outbox(state, next_retry_at_ms);
            CREATE TABLE IF NOT EXISTS acknowledgments (
              delivery_id TEXT PRIMARY KEY REFERENCES outbox(delivery_id),
              registration_id TEXT NOT NULL, received_at_ms INTEGER NOT NULL);")?;
        tx.pragma_update(None, "user_version", 1)?;
        tx.commit()?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    async fn run<T, F>(&self, action: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = connection.lock().map_err(|_| {
                SchedulerError::new("storage_poisoned", "Database mutex is poisoned")
            })?;
            action(&mut connection)
        })
        .await
        .map_err(|e| SchedulerError::new("storage_join_error", e.to_string()))?
    }

    pub async fn put_profile(&self, profile: AgentProfile) -> Result<()> {
        validate_profile(&profile)?;
        self.run(move |c| {
            c.execute("INSERT INTO profiles(id,body) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body",
                params![profile.profile_id, encode(&profile)?])?;
            Ok(())
        }).await
    }

    pub async fn profiles(&self) -> Result<Vec<AgentProfile>> {
        self.run(|c| {
            let mut s = c.prepare("SELECT body FROM profiles ORDER BY id")?;
            s.query_map([], |r| r.get::<_, String>(0))?
                .map(|r| decode(r?))
                .collect()
        })
        .await
    }

    pub async fn profile(&self, id: &str) -> Result<AgentProfile> {
        let id = id.to_owned();
        self.run(move |c| {
            let body = c
                .query_row("SELECT body FROM profiles WHERE id=?1", [id], |r| r.get(0))
                .optional()?
                .ok_or_else(|| {
                    SchedulerError::new("profile_not_found", "Agent profile was not found")
                })?;
            decode(body)
        })
        .await
    }

    pub async fn put_project(&self, mut project: ProjectRecord) -> Result<ProjectRecord> {
        validate_project(&project.config)?;
        project.revision = fingerprint(&project.config)?;
        self.run(move |c| {
            let root = project.root.to_str().ok_or_else(|| {
                SchedulerError::new(
                    "invalid_path",
                    "Project root must be representable in the JSON protocol",
                )
            })?;
            let old: Option<String> = c
                .query_row(
                    "SELECT root FROM projects WHERE id=?1",
                    [&project.config.project_id],
                    |r| r.get(0),
                )
                .optional()?;
            if old.as_deref().is_some_and(|p| p != root) {
                return Err(SchedulerError::new(
                    "project_root_mismatch",
                    "Moving a registered project requires explicit migration",
                ));
            }
            c.execute(
                "INSERT INTO projects(id,root,revision,body) VALUES(?1,?2,?3,?4)
                ON CONFLICT(id) DO UPDATE SET revision=excluded.revision, body=excluded.body",
                params![
                    project.config.project_id,
                    root,
                    project.revision,
                    encode(&project)?
                ],
            )?;
            Ok(project)
        })
        .await
    }

    pub async fn project(&self, id: &str) -> Result<ProjectRecord> {
        let id = id.to_owned();
        self.run(move |c| get_project(c, &id)).await
    }

    pub async fn projects(&self) -> Result<Vec<ProjectRecord>> {
        self.run(|c| {
            let mut s = c.prepare("SELECT body FROM projects ORDER BY root")?;
            s.query_map([], |r| r.get::<_, String>(0))?
                .map(|r| decode(r?))
                .collect()
        })
        .await
    }

    /// The caller has already validated provider identity, project root and generation.
    pub async fn ensure_registration(
        &self,
        project_id: &str,
        profile_id: &str,
        route: ParentRoute,
        generation: &str,
        workspace_root: std::path::PathBuf,
    ) -> Result<Registration> {
        let project_id = project_id.to_owned();
        let profile_id = profile_id.to_owned();
        let generation = generation.to_owned();
        self.run(move |c| {
            let tx = c.transaction()?;
            let project = get_project(&tx, &project_id)?;
            let previous: Option<String> = tx.query_row(
                "SELECT body FROM registrations WHERE project_id=?1 AND adapter_id=?2 AND session_id=?3",
                params![project_id, route.adapter_id, route.session_id], |r| r.get(0)).optional()?;
            let mut registration = if let Some(body) = previous { decode::<Registration>(body)? }
                else { Registration { registration_id: new_id(), project_id: project_id.clone(),
                    main_agent_profile_id: profile_id.clone(), root_task_id: new_id(), generation: generation.clone(),
                    route: route.clone(), workspace_root: workspace_root.clone(), project_revision: project.revision.clone(),
                    created_at_ms: now_ms(), expires_at_ms: 0 } };
            let renewed_generation = registration.generation != generation;
            registration.main_agent_profile_id = profile_id;
            registration.project_revision = project.revision;
            registration.route = route;
            registration.workspace_root = workspace_root;
            registration.generation = generation;
            registration.expires_at_ms = now_ms() + 7 * 24 * 3600 * 1000;
            tx.execute("INSERT INTO registrations(id,project_id,adapter_id,session_id,body) VALUES(?1,?2,?3,?4,?5)
                ON CONFLICT(id) DO UPDATE SET body=excluded.body",
                params![registration.registration_id, project_id, registration.route.adapter_id,
                    registration.route.session_id, encode(&registration)?])?;
            if renewed_generation {
                tx.execute("UPDATE outbox SET target_generation=?1, next_retry_at_ms=?2
                    WHERE registration_id=?3 AND state IN ('pending','retry_wait','held','orphaned')",
                    params![registration.generation, now_ms(), registration.registration_id])?;
            }
            tx.commit()?;
            Ok(registration)
        }).await
    }

    pub async fn registration(&self, id: &str) -> Result<Registration> {
        let id = id.to_owned();
        self.run(move |c| get_registration(c, &id)).await
    }

    /// Revalidate only the same logical parent; never migrate a pending result to another session.
    pub async fn refresh_route(
        &self,
        registration_id: &str,
        route: ParentRoute,
        generation: String,
    ) -> Result<Registration> {
        let id = registration_id.to_owned();
        self.run(move |c| {
            let tx = c.transaction()?;
            let mut registration = get_registration(&tx, &id)?;
            if registration.route.adapter_id != route.adapter_id || registration.route.session_id != route.session_id {
                return Err(SchedulerError::new("origin_mismatch", "Route refresh cannot change the origin conversation"));
            }
            registration.route = route;
            registration.generation = generation;
            registration.expires_at_ms = now_ms() + 7 * 24 * 3600 * 1000;
            tx.execute("UPDATE registrations SET body=?1 WHERE id=?2", params![encode(&registration)?, id])?;
            tx.execute("UPDATE outbox SET target_generation=?1 WHERE registration_id=?2 AND state IN ('pending','retry_wait','held','orphaned')",
                params![registration.generation, id])?;
            tx.commit()?;
            Ok(registration)
        }).await
    }

    pub async fn specification(&self, task_id: &str) -> Result<QueuedTask> {
        let id = task_id.to_owned();
        self.run(move |c| get_spec(c, &id)).await
    }

    pub async fn submit(&self, auth: &AuthContext, submission: SubmitTask) -> Result<TaskAccepted> {
        let (project_id, registration_id) = auth.root()?;
        let project_id = project_id.to_owned();
        let registration_id = registration_id.to_owned();
        let request_hash = fingerprint(&submission)?;
        self.run(move |c| {
            let tx = c.transaction()?;
            let previous: Option<(String, String)> = tx.query_row(
                "SELECT body,fingerprint FROM tasks WHERE registration_id=?1 AND idempotency_key=?2",
                params![registration_id, submission.idempotency_key], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            if let Some((body, original_hash)) = previous {
                if original_hash != request_hash {
                    return Err(SchedulerError::new("idempotency_conflict", "Key was already used with different input"));
                }
                let view: TaskView = decode(body)?;
                return Ok(TaskAccepted { task_id: view.task_id, execution_state: view.execution_state,
                    result_pending: !view.execution_state.is_terminal(), deduplicated: true });
            }
            let project = get_project(&tx, &project_id)?;
            let registration = get_registration(&tx, &registration_id)?;
            if registration.project_id != project_id || registration.expires_at_ms <= now_ms() {
                return Err(SchedulerError::new("origin_session_expired", "Origin registration is no longer valid"));
            }
            if registration.main_agent_profile_id != project.config.main_agent_profile_id {
                return Err(SchedulerError::new("main_profile_mismatch", "Origin no longer has dispatch rights under current project policy"));
            }
            let timeout = validate_submission(&project.config, &submission)?;
            let count: u32 = tx.query_row("SELECT count(*) FROM tasks WHERE root_task_id=?1", [&registration.root_task_id], |r| r.get(0))?;
            if count >= project.config.limits.max_dispatches_per_root {
                return Err(SchedulerError::new("dispatch_limit_reached", "Root dispatch limit reached"));
            }
            let profile: AgentProfile = decode(tx.query_row("SELECT body FROM profiles WHERE id=?1",
                [&submission.worker_profile_id], |r| r.get(0)).optional()?
                .ok_or_else(|| SchedulerError::new("profile_not_found", "Worker profile is not configured"))?)?;
            if matches!(profile.kind, AgentKind::CodexDesktop) {
                return Err(SchedulerError::new("worker_not_supported", "Desktop parent profile is not a worker executor"));
            }
            let view = TaskView { task_id: new_id(), project_id: project_id.clone(),
                parent_registration_id: registration_id.clone(), root_task_id: registration.root_task_id,
                workspace_root: registration.workspace_root.clone(),
                worker_profile_id: submission.worker_profile_id, execution_state: ExecutionState::Queued,
                delivery_state: DeliveryState::NotReady, created_at_ms: now_ms(), started_at_ms: None,
                finished_at_ms: None, result: None, delivery_id: new_id(), delivery_error: None,
                parent_turn_id: None, cancel_requested: false };
            let spec = QueuedTask { view: view.clone(), prompt: submission.prompt, timeout_seconds: timeout,
                project_root: registration.workspace_root, project_config: project.config, profile, attempt_id: new_id() };
            tx.execute("INSERT INTO tasks(id,project_id,registration_id,root_task_id,idempotency_key,fingerprint,spec,body,execution_state,created_at_ms)
                VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'queued',?9)",
                params![view.task_id, project_id, registration_id, view.root_task_id, submission.idempotency_key,
                    request_hash, encode(&spec)?, encode(&view)?, view.created_at_ms])?;
            tx.execute("INSERT INTO attempts(id,task_id) VALUES(?1,?2)", params![spec.attempt_id, view.task_id])?;
            tx.commit()?;
            Ok(TaskAccepted { task_id: view.task_id, execution_state: ExecutionState::Queued,
                result_pending: true, deduplicated: false })
        }).await
    }

    pub async fn task(&self, auth: &AuthContext, id: &str) -> Result<TaskView> {
        let id = id.to_owned();
        let auth = auth.clone();
        self.run(move |c| {
            let task = get_task(c, &id)?;
            auth.can_access(&task.project_id, &task.parent_registration_id)?;
            Ok(task)
        })
        .await
    }

    pub async fn list_tasks(&self, auth: &AuthContext, limit: u32) -> Result<Vec<TaskView>> {
        let auth = auth.clone();
        let limit = limit.clamp(1, 200);
        self.run(move |c| {
            let (sql, root): (&str, Option<&str>) = match &auth {
                AuthContext::Admin => ("SELECT body FROM tasks ORDER BY sequence DESC LIMIT ?1", None),
                AuthContext::Root { registration_id, .. } => ("SELECT body FROM tasks WHERE registration_id=?2 ORDER BY sequence DESC LIMIT ?1", Some(registration_id)),
            };
            let mut s = c.prepare(sql)?;
            let rows: Vec<String> = if let Some(root) = root {
                s.query_map(params![limit, root], |r| r.get::<_, String>(0))?.collect::<std::result::Result<_, _>>()?
            } else {
                s.query_map(params![limit], |r| r.get::<_, String>(0))?.collect::<std::result::Result<_, _>>()?
            };
            rows.into_iter().map(decode).collect()
        }).await
    }

    pub async fn claim_next(&self) -> Result<Option<QueuedTask>> {
        self.run(|c| {
            let tx = c.transaction()?;
            let candidates: Vec<String> = {
                let mut s = tx.prepare("SELECT id FROM tasks WHERE execution_state='queued' ORDER BY sequence LIMIT 128")?;
                s.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
            };
            for id in candidates {
                let mut spec = get_spec(&tx, &id)?;
                let active: u32 = tx.query_row("SELECT count(*) FROM tasks WHERE project_id=?1 AND execution_state IN ('starting','running','waiting_approval')",
                    [&spec.view.project_id], |r| r.get(0))?;
                if active >= spec.project_config.limits.max_concurrent_workers { continue; }
                spec.view.execution_state = ExecutionState::Starting;
                spec.view.started_at_ms = Some(now_ms());
                write_task(&tx, &spec.view)?;
                tx.execute("UPDATE attempts SET started_at_ms=?1,lease_until_ms=?2 WHERE id=?3",
                    params![now_ms(), now_ms() + spec.timeout_seconds as i64 * 1000, spec.attempt_id])?;
                tx.commit()?;
                return Ok(Some(spec));
            }
            tx.commit()?;
            Ok(None)
        }).await
    }

    pub async fn record_spawn(&self, task_id: &str, identity: ProcessIdentity) -> Result<()> {
        let id = task_id.to_owned();
        self.run(move |c| {
            let tx = c.transaction()?;
            let mut spec = get_spec(&tx, &id)?;
            spec.view.execution_state = ExecutionState::Running;
            write_task(&tx, &spec.view)?;
            tx.execute(
                "UPDATE attempts SET pid=?1,process_start=?2 WHERE id=?3",
                params![identity.pid, identity.started_at as i64, spec.attempt_id],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn request_cancel(&self, auth: &AuthContext, task_id: &str) -> Result<TaskView> {
        let auth = auth.clone();
        let id = task_id.to_owned();
        self.run(move |c| {
            let tx = c.transaction()?;
            let mut task = get_task(&tx, &id)?;
            auth.can_access(&task.project_id, &task.parent_registration_id)?;
            if !task.execution_state.is_terminal() {
                task.cancel_requested = true;
                write_task(&tx, &task)?;
            }
            tx.commit()?;
            Ok(task)
        })
        .await
    }

    pub async fn is_cancel_requested(&self, id: &str) -> Result<bool> {
        let id = id.to_owned();
        self.run(move |c| Ok(get_task(c, &id)?.cancel_requested))
            .await
    }

    pub async fn commit_completion(
        &self,
        completion: Completion,
        path: String,
        sha256: String,
        ack_nonce_hash: String,
    ) -> Result<TaskView> {
        if !completion.execution_state.is_terminal() || completion.schema_version != SCHEMA_VERSION
        {
            return Err(SchedulerError::new(
                "invalid_completion",
                "Completion must describe a terminal outcome",
            ));
        }
        if completion.execution_state == ExecutionState::Succeeded
            && (completion.result.exit_code != Some(0) || completion.result.error_class.is_some())
        {
            return Err(SchedulerError::new(
                "invalid_completion",
                "Successful completion must agree with the exit and error semantics",
            ));
        }
        self.run(move |c| {
            let tx = c.transaction()?;
            let spec = get_spec(&tx, &completion.task_id)?;
            if spec.attempt_id != completion.attempt_id {
                return Err(SchedulerError::new("attempt_mismatch", "Completion does not belong to this execution attempt"));
            }
            if spec.view.execution_state.is_terminal() { return Ok(spec.view); }
            let registration = get_registration(&tx, &spec.view.parent_registration_id)?;
            let mut task = spec.view;
            task.execution_state = completion.execution_state;
            task.delivery_state = DeliveryState::Pending;
            task.finished_at_ms = Some(completion.finished_at_ms);
            task.result = Some(completion.result);
            write_task(&tx, &task)?;
            tx.execute("INSERT INTO results(task_id,attempt_id,sha256,completion_path) VALUES(?1,?2,?3,?4)",
                params![task.task_id, completion.attempt_id, sha256, path])?;
            tx.execute("UPDATE attempts SET finished_at_ms=?1 WHERE id=?2", params![task.finished_at_ms, completion.attempt_id])?;
            tx.execute("INSERT INTO outbox(delivery_id,task_id,registration_id,target_generation,state,next_retry_at_ms,nonce_hash)
                VALUES(?1,?2,?3,?4,'pending',?5,?6)", params![task.delivery_id, task.task_id,
                    task.parent_registration_id, registration.generation, now_ms(), ack_nonce_hash])?;
            tx.commit()?;
            Ok(task)
        }).await
    }

    pub async fn due_deliveries(&self, limit: u32) -> Result<Vec<PendingDelivery>> {
        self.run(move |c| {
            let mut s = c.prepare(
                "SELECT task_id,attempts,target_generation FROM outbox
                WHERE state IN ('pending','retry_wait','held','orphaned') AND next_retry_at_ms<=?1
                ORDER BY next_retry_at_ms LIMIT ?2",
            )?;
            let rows: Vec<(String, u32, String)> = s
                .query_map(params![now_ms(), limit.clamp(1, 100)], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?
                .collect::<std::result::Result<_, _>>()?;
            rows.into_iter()
                .map(|(id, attempts, generation)| {
                    let task = get_task(c, &id)?;
                    let registration = get_registration(c, &task.parent_registration_id)?;
                    if generation != registration.generation {
                        return Err(SchedulerError::new(
                            "generation_mismatch",
                            "Outbox target requires validated origin renewal",
                        ));
                    }
                    Ok(PendingDelivery {
                        task,
                        registration,
                        attempts,
                    })
                })
                .collect()
        })
        .await
    }

    pub async fn begin_delivery(&self, task_id: &str) -> Result<bool> {
        let id = task_id.to_owned();
        self.run(move |c| {
            let tx = c.transaction()?;
            let mut task = get_task(&tx, &id)?;
            if !matches!(task.delivery_state, DeliveryState::Pending | DeliveryState::RetryWait | DeliveryState::Held | DeliveryState::Orphaned) {
                return Ok(false);
            }
            task.delivery_state = DeliveryState::Sending;
            write_task(&tx, &task)?;
            tx.execute("UPDATE outbox SET state='sending',attempts=attempts+1,delivery_attempt_id=?1 WHERE task_id=?2",
                params![new_id(), id])?;
            tx.commit()?;
            Ok(true)
        }).await
    }

    pub async fn set_delivery(
        &self,
        task_id: &str,
        next: DeliveryState,
        error: Option<String>,
        parent_turn_id: Option<String>,
        retry_at_ms: i64,
    ) -> Result<()> {
        let id = task_id.to_owned();
        self.run(move |c| {
            let tx = c.transaction()?;
            let mut task = get_task(&tx, &id)?;
            if task.delivery_state == DeliveryState::AcknowledgedByParent {
                return Ok(());
            }
            task.delivery_state = next;
            task.delivery_error = error.clone();
            if parent_turn_id.is_some() {
                task.parent_turn_id = parent_turn_id;
            }
            write_task(&tx, &task)?;
            tx.execute(
                "UPDATE outbox SET state=?1,last_error=?2,next_retry_at_ms=?3 WHERE task_id=?4",
                params![state(next)?, error, retry_at_ms, id],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn acknowledge(
        &self,
        auth: &AuthContext,
        delivery_id: &str,
        nonce: &str,
    ) -> Result<TaskView> {
        auth.root()?;
        let auth = auth.clone();
        let delivery_id = delivery_id.to_owned();
        let hash = nonce_hash(nonce);
        self.run(move |c| {
            let tx = c.transaction()?;
            let (task_id, expected): (String, String) = tx.query_row(
                "SELECT task_id,nonce_hash FROM outbox WHERE delivery_id=?1", [&delivery_id],
                |r| Ok((r.get(0)?, r.get(1)?))).optional()?
                .ok_or_else(|| SchedulerError::new("delivery_not_found", "Delivery was not found"))?;
            let mut task = get_task(&tx, &task_id)?;
            auth.can_access(&task.project_id, &task.parent_registration_id)?;
            if hash.as_bytes().ct_eq(expected.as_bytes()).unwrap_u8() != 1 {
                return Err(SchedulerError::new("invalid_ack", "Acknowledgment nonce does not match"));
            }
            tx.execute("INSERT OR IGNORE INTO acknowledgments(delivery_id,registration_id,received_at_ms) VALUES(?1,?2,?3)",
                params![delivery_id, task.parent_registration_id, now_ms()])?;
            task.delivery_state = DeliveryState::AcknowledgedByParent;
            task.delivery_error = None;
            write_task(&tx, &task)?;
            tx.execute("UPDATE outbox SET state='acknowledged_by_parent',last_error=NULL WHERE delivery_id=?1", [&delivery_id])?;
            tx.commit()?;
            Ok(task)
        }).await
    }

    pub async fn unfinished(&self) -> Result<Vec<(QueuedTask, Option<ProcessIdentity>)>> {
        self.run(|c| {
            let rows: Vec<String> = {
                let mut s = c.prepare("SELECT id FROM tasks WHERE execution_state IN ('starting','running','waiting_approval')")?;
                s.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
            };
            rows.into_iter().map(|id| {
                let spec = get_spec(c, &id)?;
                let process = c.query_row("SELECT pid,process_start FROM attempts WHERE id=?1", [&spec.attempt_id], |r| {
                    let pid: Option<u32> = r.get(0)?;
                    let start: Option<i64> = r.get(1)?;
                    Ok(pid.zip(start).map(|(pid, start)| ProcessIdentity { pid, started_at: start as u64 }))
                })?;
                Ok((spec, process))
            }).collect()
        }).await
    }

    pub async fn mark_interrupted_deliveries_uncertain(&self) -> Result<()> {
        self.run(|c| {
            let tx = c.transaction()?;
            let ids: Vec<String> = {
                let mut s = tx.prepare("SELECT task_id FROM outbox WHERE state='sending'")?;
                s.query_map([], |r| r.get(0))?
                    .collect::<std::result::Result<_, _>>()?
            };
            for id in ids {
                let mut task = get_task(&tx, &id)?;
                task.delivery_state = DeliveryState::Uncertain;
                task.delivery_error =
                    Some("Daemon stopped before delivery acceptance was recorded".into());
                write_task(&tx, &task)?;
                tx.execute(
                    "UPDATE outbox SET state='uncertain',last_error=?1 WHERE task_id=?2",
                    params![task.delivery_error, id],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }
}

fn get_project(c: &Connection, id: &str) -> Result<ProjectRecord> {
    decode(
        c.query_row("SELECT body FROM projects WHERE id=?1", [id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| SchedulerError::new("project_not_found", "Project is not registered"))?,
    )
}
fn get_registration(c: &Connection, id: &str) -> Result<Registration> {
    decode(
        c.query_row("SELECT body FROM registrations WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()?
        .ok_or_else(|| {
            SchedulerError::new(
                "origin_session_unavailable",
                "Origin session is not registered",
            )
        })?,
    )
}
fn get_task(c: &Connection, id: &str) -> Result<TaskView> {
    decode(
        c.query_row("SELECT body FROM tasks WHERE id=?1", [id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| SchedulerError::new("task_not_found", "Task was not found"))?,
    )
}
fn get_spec(c: &Connection, id: &str) -> Result<QueuedTask> {
    let mut spec: QueuedTask = decode(
        c.query_row("SELECT spec FROM tasks WHERE id=?1", [id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| SchedulerError::new("task_not_found", "Task was not found"))?,
    )?;
    spec.view = get_task(c, id)?;
    Ok(spec)
}
fn write_task(tx: &Transaction<'_>, task: &TaskView) -> Result<()> {
    tx.execute(
        "UPDATE tasks SET body=?1,execution_state=?2,cancel_requested=?3 WHERE id=?4",
        params![
            encode(task)?,
            state(task.execution_state)?,
            task.cancel_requested,
            task.task_id
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (
        tempfile::TempDir,
        Store,
        ProjectRecord,
        AuthContext,
        AuthContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.sqlite3")).unwrap();
        store
            .put_profile(AgentProfile {
                profile_id: "probe".into(),
                kind: AgentKind::Probe,
                program: Some("probe.exe".into()),
                model: None,
                reasoning_effort: None,
                environment_keys: Default::default(),
            })
            .await
            .unwrap();
        let config = ProjectConfig {
            schema_version: 1,
            project_id: new_id(),
            name: "示例项目".into(),
            main_agent_profile_id: "codex-main".into(),
            allowed_worker_profile_ids: ["probe".into()].into(),
            integrations: IntegrationConfig::default(),
            limits: DispatchLimits::default(),
        };
        let project = store
            .put_project(ProjectRecord {
                root: dir.path().into(),
                revision: String::new(),
                config,
            })
            .await
            .unwrap();
        let mut roots = Vec::new();
        for id in ["conversation-a", "conversation-b"] {
            let reg = store
                .ensure_registration(
                    &project.config.project_id,
                    "codex-main",
                    ParentRoute {
                        adapter_id: "test".into(),
                        session_id: id.into(),
                        parameters: serde_json::json!({}),
                    },
                    "generation-one",
                    project.root.clone(),
                )
                .await
                .unwrap();
            roots.push(AuthContext::Root {
                project_id: reg.project_id,
                registration_id: reg.registration_id,
            });
        }
        (dir, store, project, roots.remove(0), roots.remove(0))
    }
    fn request(key: &str) -> SubmitTask {
        SubmitTask {
            worker_profile_id: "probe".into(),
            prompt: "fixed marker".into(),
            idempotency_key: key.into(),
            timeout_seconds: None,
        }
    }
    fn completion(spec: &QueuedTask) -> Completion {
        Completion {
            schema_version: 1,
            task_id: spec.view.task_id.clone(),
            attempt_id: spec.attempt_id.clone(),
            execution_state: ExecutionState::Succeeded,
            finished_at_ms: now_ms(),
            result: TaskResult {
                summary: "marker".into(),
                exit_code: Some(0),
                error_class: None,
                verification: vec![],
                artifact_refs: vec![],
            },
        }
    }

    #[tokio::test]
    async fn two_automatic_origins_are_independent_and_idempotent() {
        let (_dir, store, _project, a, b) = fixture().await;
        let one = store.submit(&a, request("same-key")).await.unwrap();
        let duplicate = store.submit(&a, request("same-key")).await.unwrap();
        let two = store.submit(&b, request("same-key")).await.unwrap();
        assert_eq!(one.task_id, duplicate.task_id);
        assert!(duplicate.deduplicated);
        assert_ne!(one.task_id, two.task_id);
        assert!(store.task(&b, &one.task_id).await.is_err());
        let mut conflict = request("same-key");
        conflict.prompt = "different".into();
        assert_eq!(
            store.submit(&a, conflict).await.unwrap_err().code,
            "idempotency_conflict"
        );
    }

    #[tokio::test]
    async fn queue_claim_is_atomic_under_concurrency() {
        let (_dir, store, _project, a, _b) = fixture().await;
        store.submit(&a, request("one")).await.unwrap();
        let (one, two) = tokio::join!(store.claim_next(), store.claim_next());
        assert_eq!(
            usize::from(one.unwrap().is_some()) + usize::from(two.unwrap().is_some()),
            1
        );
    }

    #[tokio::test]
    async fn completion_and_delivery_are_transactional_and_ack_is_scoped() {
        let (_dir, store, _project, a, b) = fixture().await;
        let task = store.submit(&a, request("one")).await.unwrap();
        let spec = store.claim_next().await.unwrap().unwrap();
        let done = completion(&spec);
        let result = store
            .commit_completion(
                done.clone(),
                "result.json".into(),
                "hash".into(),
                nonce_hash("nonce"),
            )
            .await
            .unwrap();
        assert_eq!(result.execution_state, ExecutionState::Succeeded);
        assert_eq!(result.delivery_state, DeliveryState::Pending);
        assert_eq!(store.due_deliveries(10).await.unwrap().len(), 1);
        store
            .commit_completion(
                done,
                "result.json".into(),
                "hash".into(),
                nonce_hash("nonce"),
            )
            .await
            .unwrap();
        assert_eq!(store.due_deliveries(10).await.unwrap().len(), 1);
        assert!(
            store
                .acknowledge(&AuthContext::Admin, &result.delivery_id, "nonce")
                .await
                .is_err()
        );
        assert!(
            store
                .acknowledge(&b, &result.delivery_id, "nonce")
                .await
                .is_err()
        );
        assert!(
            store
                .acknowledge(&a, &result.delivery_id, "bad-nonce")
                .await
                .is_err()
        );
        let acknowledged = store
            .acknowledge(&a, &result.delivery_id, "nonce")
            .await
            .unwrap();
        assert_eq!(
            acknowledged.delivery_state,
            DeliveryState::AcknowledgedByParent
        );
        store
            .acknowledge(&a, &result.delivery_id, "nonce")
            .await
            .unwrap();
        assert!(store.due_deliveries(10).await.unwrap().is_empty());
        assert_eq!(
            store.task(&a, &task.task_id).await.unwrap().execution_state,
            ExecutionState::Succeeded
        );
    }

    #[tokio::test]
    async fn restart_preserves_queued_and_marks_unknown_delivery_without_resending() {
        let (dir, store, _project, a, _b) = fixture().await;
        let first = store.submit(&a, request("one")).await.unwrap();
        let queued = store.submit(&a, request("two")).await.unwrap();
        let spec = store.claim_next().await.unwrap().unwrap();
        store
            .commit_completion(
                completion(&spec),
                "result.json".into(),
                "hash".into(),
                nonce_hash("nonce"),
            )
            .await
            .unwrap();
        assert!(store.begin_delivery(&first.task_id).await.unwrap());
        drop(store);
        let store = Store::open(&dir.path().join("state.sqlite3")).unwrap();
        store.mark_interrupted_deliveries_uncertain().await.unwrap();
        assert_eq!(
            store.task(&a, &first.task_id).await.unwrap().delivery_state,
            DeliveryState::Uncertain
        );
        assert!(store.due_deliveries(10).await.unwrap().is_empty());
        assert_eq!(
            store.claim_next().await.unwrap().unwrap().view.task_id,
            queued.task_id
        );
    }

    #[tokio::test]
    async fn registration_renewal_is_automatic_and_does_not_move_results_to_other_origins() {
        let (_dir, store, project, a, _b) = fixture().await;
        let (_, id) = a.root().unwrap();
        let before = store.registration(id).await.unwrap();
        let renewed = store
            .ensure_registration(
                &project.config.project_id,
                "codex-main",
                before.route.clone(),
                "generation-two",
                project.root.clone(),
            )
            .await
            .unwrap();
        assert_eq!(before.registration_id, renewed.registration_id);
        assert_eq!(before.root_task_id, renewed.root_task_id);
        let mut wrong = before.route;
        wrong.session_id = "another-conversation".into();
        assert_eq!(
            store
                .refresh_route(id, wrong, "new".into())
                .await
                .unwrap_err()
                .code,
            "origin_mismatch"
        );
    }

    #[tokio::test]
    async fn invalid_attempt_rolls_back_and_fast_parent_ack_wins_over_transport_update() {
        let (_dir, store, _project, a, _b) = fixture().await;
        store.submit(&a, request("one")).await.unwrap();
        let spec = store.claim_next().await.unwrap().unwrap();
        for (exit_code, error_class) in [
            (None, None),
            (Some(1), None),
            (Some(0), Some("reported_failure")),
        ] {
            let mut invalid = completion(&spec);
            invalid.result.exit_code = exit_code;
            invalid.result.error_class = error_class.map(str::to_owned);
            assert_eq!(
                store
                    .commit_completion(invalid, "p".into(), "h".into(), nonce_hash("n"))
                    .await
                    .unwrap_err()
                    .code,
                "invalid_completion"
            );
        }
        let mut bad = completion(&spec);
        bad.attempt_id = new_id();
        assert!(
            store
                .commit_completion(bad, "p".into(), "h".into(), nonce_hash("n"))
                .await
                .is_err()
        );
        assert!(store.due_deliveries(10).await.unwrap().is_empty());
        assert_eq!(
            store
                .task(&a, &spec.view.task_id)
                .await
                .unwrap()
                .execution_state,
            ExecutionState::Starting
        );
        let task = store
            .commit_completion(completion(&spec), "p".into(), "h".into(), nonce_hash("n"))
            .await
            .unwrap();
        store.begin_delivery(&task.task_id).await.unwrap();
        store.acknowledge(&a, &task.delivery_id, "n").await.unwrap();
        store
            .set_delivery(
                &task.task_id,
                DeliveryState::AcceptedByTransport,
                None,
                Some("turn".into()),
                0,
            )
            .await
            .unwrap();
        assert_eq!(
            store.task(&a, &task.task_id).await.unwrap().delivery_state,
            DeliveryState::AcknowledgedByParent
        );
    }
}
