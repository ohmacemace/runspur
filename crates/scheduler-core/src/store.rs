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

enum HistoryFilter {
    Origin(String),
    LegacySession(String),
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

fn scheduler_worker_limit(connection: &Connection) -> Result<Option<usize>> {
    Ok(connection
        .query_row(
            "SELECT max_concurrent_workers FROM scheduler_settings WHERE id=1",
            [],
            |row| row.get::<_, u16>(0).map(usize::from),
        )
        .optional()?)
}

fn scheduler_worker_access(connection: &Connection) -> Result<WorkerAccess> {
    let access: Option<String> = connection
        .query_row(
            "SELECT worker_access FROM scheduler_worker_access WHERE id=1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    access.map_or(Ok(WorkerAccess::default()), |access| {
        Ok(serde_json::from_value(serde_json::Value::String(access))?)
    })
}

fn read_scheduler_settings(
    connection: &Connection,
    default_limit: usize,
) -> Result<SchedulerSettings> {
    Ok(SchedulerSettings {
        max_concurrent_workers: scheduler_worker_limit(connection)?.unwrap_or(default_limit),
        worker_access: scheduler_worker_access(connection)?,
    })
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
            CREATE TABLE IF NOT EXISTS scheduler_settings (
              id INTEGER PRIMARY KEY CHECK(id=1),
              max_concurrent_workers INTEGER NOT NULL CHECK(max_concurrent_workers BETWEEN 1 AND 64));
            CREATE TABLE IF NOT EXISTS scheduler_worker_access (
              id INTEGER PRIMARY KEY CHECK(id=1),
              worker_access TEXT NOT NULL CHECK(worker_access IN ('project','full','follow_main')));
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
            CREATE INDEX IF NOT EXISTS tasks_project_history ON tasks(project_id, sequence);
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

    pub async fn scheduler_worker_limit(&self) -> Result<Option<usize>> {
        self.run(|connection| scheduler_worker_limit(connection))
            .await
    }

    pub async fn set_scheduler_worker_limit(&self, max_concurrent_workers: usize) -> Result<()> {
        self.set_scheduler_settings(Some(max_concurrent_workers), None, max_concurrent_workers)
            .await
            .map(|_| ())
    }

    pub async fn scheduler_settings(&self, default_limit: usize) -> Result<SchedulerSettings> {
        self.run(move |connection| {
            let tx = connection.transaction()?;
            read_scheduler_settings(&tx, default_limit)
        })
        .await
    }

    pub async fn set_scheduler_settings(
        &self,
        max_concurrent_workers: Option<usize>,
        worker_access: Option<WorkerAccess>,
        default_limit: usize,
    ) -> Result<SchedulerSettings> {
        if max_concurrent_workers.is_some_and(|limit| !(1..=MAX_SCHEDULER_WORKERS).contains(&limit))
        {
            return Err(SchedulerError::new(
                "invalid_scheduler_limit",
                format!("Global worker limit must be between 1 and {MAX_SCHEDULER_WORKERS}"),
            ));
        }
        self.run(move |connection| {
            let tx = connection.transaction()?;
            if let Some(limit) = max_concurrent_workers {
                tx.execute(
                    "INSERT INTO scheduler_settings(id, max_concurrent_workers) VALUES(1, ?1)
                     ON CONFLICT(id) DO UPDATE SET max_concurrent_workers=excluded.max_concurrent_workers",
                    [limit as i64],
                )?;
            }
            if let Some(access) = worker_access {
                tx.execute(
                    "INSERT INTO scheduler_worker_access(id, worker_access) VALUES(1, ?1)
                     ON CONFLICT(id) DO UPDATE SET worker_access=excluded.worker_access",
                    [state(access)?],
                )?;
            }
            let settings = read_scheduler_settings(&tx, default_limit)?;
            tx.commit()?;
            Ok(settings)
        })
        .await
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
        self.run(move |c| get_profile(c, &id)).await
    }

    pub async fn put_project(&self, mut project: ProjectRecord) -> Result<ProjectRecord> {
        validate_project(&project.config)?;
        let expected_revision = std::mem::take(&mut project.revision);
        project.revision = fingerprint(&project.config)?;
        self.run(move |c| {
            let tx = c.transaction()?;
            let root = project.root.to_str().ok_or_else(|| {
                SchedulerError::new(
                    "invalid_path",
                    "Project root must be representable in the JSON protocol",
                )
            })?;
            let old: Option<String> = tx
                .query_row(
                    "SELECT body FROM projects WHERE id=?1",
                    [&project.config.project_id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(old) = old {
                let old: ProjectRecord = decode(old)?;
                if old.root != project.root {
                    return Err(SchedulerError::new(
                        "project_root_mismatch",
                        "Moving a registered project requires explicit migration",
                    ));
                }
                if !expected_revision.is_empty() && old.revision != expected_revision {
                    return Err(SchedulerError::new(
                        "project_revision_changed",
                        "Project registration changed after preview. Preview it again.",
                    ));
                }
                if old.config.main_agent_profile_id != project.config.main_agent_profile_id {
                    let pending: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM tasks t
                         LEFT JOIN outbox o ON o.task_id=t.id
                         WHERE t.project_id=?1 AND
                         (t.execution_state NOT IN ('succeeded','failed','canceled','timed_out')
                          OR o.state NOT IN ('acknowledged_by_parent','refused')))",
                        [&project.config.project_id],
                        |r| r.get(0),
                    )?;
                    if pending {
                        return Err(SchedulerError::new(
                            "project_switch_busy",
                            "Finish the project's running tasks and pending returns before changing its main agent.",
                        ));
                    }
                }
            }
            tx.execute(
                "INSERT INTO projects(id,root,revision,body) VALUES(?1,?2,?3,?4)
                ON CONFLICT(id) DO UPDATE SET revision=excluded.revision, body=excluded.body",
                params![
                    project.config.project_id,
                    root,
                    project.revision,
                    encode(&project)?
                ],
            )?;
            tx.commit()?;
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
        self.submit_checked(
            auth,
            submission,
            None,
            |_, _, profile, request, native_full| {
                if matches!(profile.kind, AgentKind::CodexDesktop) {
                    return Err(SchedulerError::new(
                        "worker_not_supported",
                        "Desktop parent profile is not a worker executor",
                    ));
                }
                if native_full || !request.execution_mode.is_read_only() {
                    return Err(SchedulerError::new(
                        "worker_not_supported",
                        "Writing requires an authorized worker adapter",
                    ));
                }
                Ok(())
            },
        )
        .await
    }

    pub async fn submit_with_worker_policy(
        &self,
        auth: &AuthContext,
        submission: SubmitTask,
        main_permission: Option<MainPermission>,
        validate: impl FnOnce(
            &AgentProfile,
            &AgentProfile,
            &SubmitTask,
            &Registration,
            bool,
        ) -> Result<()>
        + Send
        + 'static,
    ) -> Result<TaskAccepted> {
        self.submit_checked(
            auth,
            submission,
            main_permission,
            move |tx, registration, profile, request, native_full| {
                let main = get_profile(tx, &registration.main_agent_profile_id)?;
                validate(&main, profile, request, registration, native_full)
            },
        )
        .await
    }

    async fn submit_checked(
        &self,
        auth: &AuthContext,
        submission: SubmitTask,
        main_permission: Option<MainPermission>,
        validate: impl FnOnce(
            &Transaction<'_>,
            &Registration,
            &AgentProfile,
            &SubmitTask,
            bool,
        ) -> Result<()>
        + Send
        + 'static,
    ) -> Result<TaskAccepted> {
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
            let worker_access = scheduler_worker_access(&tx)?;
            let allow_full = worker_access.allows_native_full(main_permission);
            let inherit_read_only = worker_access.inherits_read_only(main_permission);
            let timeout = validate_submission_with_access(&project.config, &submission, allow_full || inherit_read_only)?;
            if inherit_read_only && !submission.execution_mode.is_read_only() {
                return Err(SchedulerError::new("main_permission_read_only", "The main Agent is read-only under the current scheduler settings"));
            }
            let count: u32 = tx.query_row("SELECT count(*) FROM tasks WHERE root_task_id=?1", [&registration.root_task_id], |r| r.get(0))?;
            if count >= project.config.limits.max_dispatches_per_root {
                return Err(SchedulerError::new("dispatch_limit_reached", "Root dispatch limit reached"));
            }
            let profile = get_profile(&tx, &submission.worker_profile_id)?;
            let native_full = worker_access.permits_native_full(main_permission, &profile.kind);
            if allow_full && !native_full {
                validate_project_execution_mode(&project.config, submission.execution_mode)?;
            }
            validate(&tx, &registration, &profile, &submission, native_full)?;
            let view = TaskView { task_id: new_id(), project_id: project_id.clone(),
                parent_registration_id: registration_id.clone(), root_task_id: registration.root_task_id,
                workspace_root: registration.workspace_root.clone(),
                worker_profile_id: submission.worker_profile_id, execution_state: ExecutionState::Queued,
                delivery_state: DeliveryState::NotReady, created_at_ms: now_ms(), started_at_ms: None,
                finished_at_ms: None, result: None, delivery_id: new_id(), delivery_error: None,
                parent_turn_id: None, cancel_requested: false };
            let spec = QueuedTask { view: view.clone(), prompt: submission.prompt, timeout_seconds: timeout,
                project_root: registration.workspace_root, project_config: project.config, profile, attempt_id: new_id(),
                execution_mode: submission.execution_mode, write_scope: submission.write_scope,
                native_full, main_permission, access_revoked: false };
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

    pub async fn task_for_delivery(
        &self,
        auth: &AuthContext,
        delivery_id: &str,
    ) -> Result<TaskView> {
        auth.root()?;
        let auth = auth.clone();
        let delivery_id = delivery_id.to_owned();
        self.run(move |c| {
            let task_id: String = c
                .query_row(
                    "SELECT task_id FROM outbox WHERE delivery_id=?1",
                    [&delivery_id],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| {
                    SchedulerError::new("delivery_not_found", "Delivery was not found")
                })?;
            let task = get_task(c, &task_id)?;
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

    pub async fn task_history(
        &self,
        auth: &AuthContext,
        project_id: Option<&str>,
        session_id: Option<&str>,
        before: Option<i64>,
        limit: u32,
    ) -> Result<TaskPage> {
        if project_id.is_none() && session_id.is_some() {
            auth.require_admin()?;
            return Err(SchedulerError::new(
                "project_required",
                "A session filter requires a project",
            ));
        }
        self.read_task_history(
            auth,
            project_id.map(str::to_owned),
            session_id.map(|id| HistoryFilter::LegacySession(id.to_owned())),
            before,
            limit,
        )
        .await
    }

    pub async fn origin_task_history(
        &self,
        auth: &AuthContext,
        project_id: &str,
        origin_id: &str,
        before: Option<i64>,
        limit: u32,
    ) -> Result<TaskPage> {
        self.read_task_history(
            auth,
            Some(project_id.to_owned()),
            Some(HistoryFilter::Origin(origin_id.to_owned())),
            before,
            limit,
        )
        .await
    }

    async fn read_task_history(
        &self,
        auth: &AuthContext,
        project_id: Option<String>,
        filter: Option<HistoryFilter>,
        before: Option<i64>,
        limit: u32,
    ) -> Result<TaskPage> {
        auth.require_admin()?;
        let through = before.map_or(i64::MAX, |cursor| cursor.saturating_sub(1));
        let limit = limit.clamp(1, 50);
        self.run(move |c| {
            let origin_id = match (filter, &project_id) {
                (Some(HistoryFilter::Origin(id)), _) => Some(id),
                (Some(HistoryFilter::LegacySession(id)), Some(project)) => {
                    let Some(origin) = resolve_legacy_origin(c, project, &id)? else {
                        return Ok(TaskPage { items: Vec::new(), next_cursor: None });
                    };
                    Some(origin)
                }
                _ => None,
            };
            let sql = match (&project_id, &origin_id) {
                (Some(_), Some(_)) => "SELECT sequence,body FROM tasks
                    WHERE sequence<=?1 AND project_id=?3 AND registration_id=?4 ORDER BY sequence DESC LIMIT ?2",
                (Some(_), None) => "SELECT sequence,body FROM tasks WHERE sequence<=?1 AND project_id=?3 ORDER BY sequence DESC LIMIT ?2",
                (None, _) => "SELECT sequence,body FROM tasks WHERE sequence<=?1 ORDER BY sequence DESC LIMIT ?2",
            };
            let mut stmt = c.prepare(sql)?;
            let mut rows = match (project_id, origin_id) {
                (Some(project), Some(origin)) => stmt.query(params![through, limit + 1, project, origin])?,
                (Some(project), None) => stmt.query(params![through, limit + 1, project])?,
                (None, _) => stmt.query(params![through, limit + 1])?,
            };
            let mut items = Vec::new();
            let mut bytes = 0;
            let mut last = None;
            while let Some(row) = rows.next()? {
                if items.len() == limit as usize {
                    return Ok(TaskPage { items, next_cursor: last });
                }
                let mut task: TaskView = decode(row.get(1)?)?;
                task.result = None;
                task.delivery_error = None;
                bytes += serde_json::to_vec(&task)?.len();
                // Keep room for the response envelope, commas, and cursor.
                if bytes > MAX_FRAME_BYTES / 2 {
                    if last.is_none() {
                        return Err(SchedulerError::new("task_summary_too_large", "Task summary exceeds the page limit"));
                    }
                    return Ok(TaskPage { items, next_cursor: last });
                }
                last = Some(row.get(0)?);
                items.push(task);
            }
            Ok(TaskPage { items, next_cursor: None })
        }).await
    }

    pub async fn list_sessions(
        &self,
        auth: &AuthContext,
        project_id: &str,
        before: Option<i64>,
        limit: u32,
    ) -> Result<SessionPage> {
        auth.require_admin()?;
        let project_id = project_id.to_owned();
        let through = before.map_or(i64::MAX, |cursor| cursor.saturating_sub(1));
        let limit = limit.clamp(1, 50);
        self.run(move |c| {
            let mut stmt = c.prepare(
                "SELECT r.id, r.adapter_id, r.session_id, COUNT(*), MAX(t.created_at_ms), MAX(t.sequence) AS newest,
                   CASE WHEN json_extract(r.body, '$.route.parameters.transport') = 'native_inbox'
                        THEN 'native_inbox' END
                 FROM tasks t JOIN registrations r ON r.id=t.registration_id AND r.project_id=t.project_id
                 WHERE t.project_id=?1 GROUP BY r.id
                 HAVING newest<=?2 ORDER BY newest DESC LIMIT ?3",
            )?;
            let mut rows = stmt.query(params![project_id, through, limit + 1])?;
            let mut items = Vec::new();
            let mut last = None;
            while let Some(row) = rows.next()? {
                if items.len() == limit as usize {
                    return Ok(SessionPage { items, next_cursor: last });
                }
                items.push(SessionSummary {
                    origin_id: row.get(0)?,
                    adapter_id: row.get(1)?,
                    session_id: row.get(2)?,
                    task_count: row.get::<_, i64>(3)? as u64,
                    last_dispatched_at_ms: row.get(4)?,
                    transport: row.get(6)?,
                });
                last = Some(row.get(5)?);
            }
            Ok(SessionPage { items, next_cursor: None })
        })
        .await
    }

    pub async fn task_detail(&self, auth: &AuthContext, id: &str) -> Result<TaskDetail> {
        auth.require_admin()?;
        let id = id.to_owned();
        self.run(move |c| {
            let spec = get_spec(c, &id)?;
            let session_id = c
                .query_row(
                    "SELECT session_id FROM registrations WHERE id=?1",
                    [&spec.view.parent_registration_id],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(TaskDetail {
                origin_id: spec.view.parent_registration_id.clone(),
                task: spec.view,
                prompt: spec.prompt,
                timeout_seconds: spec.timeout_seconds,
                session_id,
            })
        })
        .await
    }

    pub async fn claim_next(&self, default_max_workers: usize) -> Result<Option<QueuedTask>> {
        self.run(move |c| {
            let tx = c.transaction()?;
            let id: Option<String> = tx.query_row(
                "SELECT id FROM tasks WHERE execution_state='queued' ORDER BY sequence LIMIT 1",
                [],
                |row| row.get(0),
            ).optional()?;
            let Some(id) = id else { return Ok(None); };
            let settings = read_scheduler_settings(&tx, default_max_workers)?;
            let active: i64 = tx.query_row(
                "SELECT count(*) FROM tasks WHERE execution_state IN ('starting','running','waiting_approval')",
                [],
                |row| row.get(0),
            )?;
            if active >= settings.max_concurrent_workers as i64 {
                return Ok(None);
            }
            let mut spec = get_spec(&tx, &id)?;
            let access = settings.worker_access;
            if (spec.native_full && !access.permits_native_full(spec.main_permission, &spec.profile.kind))
                || (access.inherits_read_only(spec.main_permission) && !spec.execution_mode.is_read_only())
            {
                spec.native_full = false;
                spec.access_revoked = true;
                tx.execute("UPDATE tasks SET spec=?1 WHERE id=?2", params![encode(&spec)?, id])?;
            }
            let started_at = now_ms();
            spec.view.execution_state = ExecutionState::Starting;
            spec.view.started_at_ms = Some(started_at);
            write_task(&tx, &spec.view)?;
            tx.execute("UPDATE attempts SET started_at_ms=?1,lease_until_ms=?2 WHERE id=?3",
                params![started_at, started_at + spec.timeout_seconds as i64 * 1000, spec.attempt_id])?;
            tx.commit()?;
            Ok(Some(spec))
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
            && (completion.result.exit_code.is_some_and(|code| code != 0)
                || completion.result.error_class.is_some())
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
fn get_profile(c: &Connection, id: &str) -> Result<AgentProfile> {
    let body = c
        .query_row("SELECT body FROM profiles WHERE id=?1", [id], |r| r.get(0))
        .optional()?
        .ok_or_else(|| SchedulerError::new("profile_not_found", "Agent profile was not found"))?;
    decode(body)
}
fn resolve_legacy_origin(c: &Connection, project: &str, session: &str) -> Result<Option<String>> {
    let mut stmt =
        c.prepare("SELECT id FROM registrations WHERE project_id=?1 AND session_id=?2 LIMIT 2")?;
    let mut rows = stmt.query(params![project, session])?;
    let origin = rows.next()?.map(|row| row.get(0)).transpose()?;
    if rows.next()?.is_some() {
        return Err(SchedulerError::new(
            "ambiguous_session_id",
            "Multiple origins have this host session ID; use the Runspur origin ID",
        ));
    }
    Ok(origin)
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
            execution_mode: ExecutionMode::ReadOnly,
            write_scope: Vec::new(),
        }
    }

    async fn allow_model_workers(store: &Store, mut project: ProjectRecord) {
        for (id, kind, program) in [
            ("codex-main", AgentKind::CodexDesktop, None),
            (
                "codex-worker",
                AgentKind::CodexExec,
                Some("codex.exe".into()),
            ),
            (
                "claude-worker",
                AgentKind::ClaudePrint,
                Some("claude.exe".into()),
            ),
        ] {
            if program.is_some() {
                project.config.allowed_worker_profile_ids.insert(id.into());
            }
            store
                .put_profile(AgentProfile {
                    profile_id: id.into(),
                    kind,
                    program,
                    model: None,
                    reasoning_effort: None,
                    environment_keys: Default::default(),
                })
                .await
                .unwrap();
        }
        store.put_project(project).await.unwrap();
    }

    fn model_request(key: &str, mode: ExecutionMode) -> SubmitTask {
        SubmitTask {
            worker_profile_id: "claude-worker".into(),
            execution_mode: mode,
            ..request(key)
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
    async fn legacy_project_worker_limit_does_not_block_another_task() {
        let (_dir, store, mut project, origin, _) = fixture().await;
        project.config.limits.max_concurrent_workers = 1;
        store.put_project(project).await.unwrap();
        let first = store.submit(&origin, request("first")).await.unwrap();
        let second = store.submit(&origin, request("second")).await.unwrap();

        assert_eq!(
            store.claim_next(2).await.unwrap().unwrap().view.task_id,
            first.task_id
        );
        assert_eq!(
            store.claim_next(2).await.unwrap().unwrap().view.task_id,
            second.task_id
        );
    }

    #[tokio::test]
    async fn global_worker_limit_is_shared_live_and_persistent() {
        let (dir, store, project, origin_a, _) = fixture().await;
        let mut config = project.config;
        config.project_id = new_id();
        let other = store
            .put_project(ProjectRecord {
                root: dir.path().join("second-project"),
                revision: String::new(),
                config,
            })
            .await
            .unwrap();
        let registration = store
            .ensure_registration(
                &other.config.project_id,
                &other.config.main_agent_profile_id,
                ParentRoute {
                    adapter_id: "test".into(),
                    session_id: "conversation-other".into(),
                    parameters: serde_json::json!({}),
                },
                "generation-one",
                other.root,
            )
            .await
            .unwrap();
        let origin_b = AuthContext::Root {
            project_id: registration.project_id,
            registration_id: registration.registration_id,
        };
        let first = store.submit(&origin_a, request("first")).await.unwrap();
        let second = store.submit(&origin_b, request("second")).await.unwrap();
        let third = store.submit(&origin_a, request("third")).await.unwrap();

        assert_eq!(store.scheduler_worker_limit().await.unwrap(), None);
        let first_spec = store.claim_next(1).await.unwrap().unwrap();
        assert_eq!(first_spec.view.task_id, first.task_id);
        assert!(store.claim_next(1).await.unwrap().is_none());

        store.set_scheduler_worker_limit(2).await.unwrap();
        let second_spec = store.claim_next(1).await.unwrap().unwrap();
        assert_eq!(second_spec.view.task_id, second.task_id);
        assert!(store.claim_next(1).await.unwrap().is_none());

        store.set_scheduler_worker_limit(1).await.unwrap();
        assert!(store.set_scheduler_worker_limit(0).await.is_err());
        assert!(store.set_scheduler_worker_limit(65).await.is_err());
        assert!(store.claim_next(2).await.unwrap().is_none());
        for spec in [&first_spec, &second_spec] {
            let view = store
                .task(&AuthContext::Admin, &spec.view.task_id)
                .await
                .unwrap();
            assert_eq!(view.execution_state, ExecutionState::Starting);
            assert!(!view.cancel_requested);
            store
                .commit_completion(completion(spec), "p".into(), "h".into(), nonce_hash("n"))
                .await
                .unwrap();
            if spec.view.task_id == first.task_id {
                assert!(store.claim_next(2).await.unwrap().is_none());
            }
        }
        assert_eq!(
            store.claim_next(2).await.unwrap().unwrap().view.task_id,
            third.task_id
        );

        drop(store);
        let reopened = Store::open(&dir.path().join("state.sqlite3")).unwrap();
        assert_eq!(reopened.scheduler_worker_limit().await.unwrap(), Some(1));
    }

    #[tokio::test]
    async fn access_settings_preserve_existing_or_unset_concurrency() {
        let dir = tempfile::tempdir().unwrap();
        let old_path = dir.path().join("legacy.sqlite3");
        Connection::open(&old_path).unwrap().execute_batch("\
            PRAGMA user_version=1;
            CREATE TABLE scheduler_settings(id INTEGER PRIMARY KEY, max_concurrent_workers INTEGER NOT NULL);
            INSERT INTO scheduler_settings VALUES(1,5);").unwrap();
        let old = Store::open(&old_path).unwrap();
        assert_eq!(
            old.scheduler_settings(2).await.unwrap(),
            SchedulerSettings {
                max_concurrent_workers: 5,
                worker_access: WorkerAccess::Project,
            }
        );
        assert_eq!(
            old.set_scheduler_settings(None, Some(WorkerAccess::Full), 2)
                .await
                .unwrap()
                .max_concurrent_workers,
            5
        );
        assert_eq!(
            old.set_scheduler_settings(Some(4), None, 2)
                .await
                .unwrap()
                .worker_access,
            WorkerAccess::Full
        );
        drop(old);
        let reopened = Store::open(&old_path).unwrap();
        assert_eq!(
            reopened.scheduler_settings(2).await.unwrap(),
            SchedulerSettings {
                max_concurrent_workers: 4,
                worker_access: WorkerAccess::Full,
            }
        );

        let fresh_path = dir.path().join("fresh.sqlite3");
        let fresh = Store::open(&fresh_path).unwrap();
        fresh
            .set_scheduler_settings(None, Some(WorkerAccess::FollowMain), 2)
            .await
            .unwrap();
        drop(fresh);
        let reopened = Store::open(&fresh_path).unwrap();
        assert_eq!(
            reopened.scheduler_settings(3).await.unwrap(),
            SchedulerSettings {
                max_concurrent_workers: 3,
                worker_access: WorkerAccess::FollowMain,
            }
        );
    }

    #[tokio::test]
    async fn enqueue_applies_access_precedence_without_bypassing_request_validation() {
        let (_dir, store, project, origin, _) = fixture().await;
        allow_model_workers(&store, project).await;
        let write = model_request("write", ExecutionMode::ProjectWrite);
        let no_adapter_limit =
            |_: &AgentProfile, _: &AgentProfile, _: &SubmitTask, _: &Registration, _: bool| Ok(());
        let project_error = store
            .submit_with_worker_policy(
                &origin,
                write.clone(),
                Some(MainPermission::Full),
                no_adapter_limit,
            )
            .await
            .unwrap_err();
        assert_eq!(project_error.code, "project_write_disabled");

        store
            .set_scheduler_settings(None, Some(WorkerAccess::Full), 2)
            .await
            .unwrap();
        for (id, native_full) in [
            ("claude-worker", true),
            ("codex-worker", true),
            ("probe", false),
        ] {
            let submission = SubmitTask {
                worker_profile_id: id.into(),
                ..request(id)
            };
            let accepted = store
                .submit_with_worker_policy(&origin, submission, None, move |_, _, _, _, allowed| {
                    assert_eq!(allowed, native_full);
                    Ok(())
                })
                .await
                .unwrap();
            assert_eq!(
                store
                    .specification(&accepted.task_id)
                    .await
                    .unwrap()
                    .native_full,
                native_full
            );
        }
        for (invalid, expected) in [
            (
                SubmitTask {
                    worker_profile_id: "not-allowed".into(),
                    ..write.clone()
                },
                "worker_not_allowed",
            ),
            (
                SubmitTask {
                    timeout_seconds: Some(0),
                    ..write.clone()
                },
                "invalid_timeout",
            ),
            (
                SubmitTask {
                    write_scope: vec!["../escape".into()],
                    execution_mode: ExecutionMode::WorkspaceWrite,
                    ..write.clone()
                },
                "invalid_write_scope",
            ),
        ] {
            assert_eq!(
                store
                    .submit_with_worker_policy(&origin, invalid, None, no_adapter_limit)
                    .await
                    .unwrap_err()
                    .code,
                expected
            );
        }

        store
            .set_scheduler_settings(None, Some(WorkerAccess::FollowMain), 2)
            .await
            .unwrap();
        for permission in [None, Some(MainPermission::Restricted)] {
            assert_eq!(
                store
                    .submit_with_worker_policy(&origin, write.clone(), permission, no_adapter_limit)
                    .await
                    .unwrap_err()
                    .code,
                "project_write_disabled"
            );
        }
        assert_eq!(
            store
                .submit_with_worker_policy(
                    &origin,
                    write.clone(),
                    Some(MainPermission::ReadOnly),
                    no_adapter_limit
                )
                .await
                .unwrap_err()
                .code,
            "main_permission_read_only"
        );
        let accepted = store
            .submit_with_worker_policy(&origin, write, Some(MainPermission::Full), no_adapter_limit)
            .await
            .unwrap();
        assert!(
            store
                .specification(&accepted.task_id)
                .await
                .unwrap()
                .native_full
        );
    }

    #[tokio::test]
    async fn claiming_never_upgrades_existing_tasks_and_persists_access_revocation() {
        let (_dir, store, mut project, origin, _) = fixture().await;
        project.config.limits.allow_project_write = true;
        allow_model_workers(&store, project).await;
        let no_adapter_limit =
            |_: &AgentProfile, _: &AgentProfile, _: &SubmitTask, _: &Registration, _: bool| Ok(());
        let legacy = store
            .submit_with_worker_policy(
                &origin,
                model_request("legacy", ExecutionMode::ReadOnly),
                None,
                no_adapter_limit,
            )
            .await
            .unwrap();
        let legacy_id = legacy.task_id.clone();
        store
            .run(move |connection| {
                let mut spec = serde_json::to_value(get_spec(connection, &legacy_id)?)?;
                let spec = spec.as_object_mut().unwrap();
                for field in ["native_full", "main_permission", "access_revoked"] {
                    spec.remove(field);
                }
                connection.execute(
                    "UPDATE tasks SET spec=?1 WHERE id=?2",
                    params![encode(spec)?, legacy_id],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let scoped = store
            .submit_with_worker_policy(
                &origin,
                model_request("scoped", ExecutionMode::ProjectWrite),
                Some(MainPermission::ReadOnly),
                no_adapter_limit,
            )
            .await
            .unwrap();
        store
            .set_scheduler_settings(None, Some(WorkerAccess::Full), 8)
            .await
            .unwrap();
        let legacy_claim = store.claim_next(8).await.unwrap().unwrap();
        assert_eq!(legacy_claim.view.task_id, legacy.task_id);
        assert!(!legacy_claim.native_full && !legacy_claim.access_revoked);

        let unverified = store
            .submit_with_worker_policy(
                &origin,
                model_request("unverified", ExecutionMode::ReadOnly),
                None,
                no_adapter_limit,
            )
            .await
            .unwrap();
        let verified = store
            .submit_with_worker_policy(
                &origin,
                model_request("verified", ExecutionMode::ReadOnly),
                Some(MainPermission::Full),
                no_adapter_limit,
            )
            .await
            .unwrap();
        store
            .set_scheduler_settings(None, Some(WorkerAccess::FollowMain), 8)
            .await
            .unwrap();
        for (expected_id, revoked, full) in [
            (scoped.task_id, true, false),
            (unverified.task_id, true, false),
            (verified.task_id, false, true),
        ] {
            let claimed = store.claim_next(8).await.unwrap().unwrap();
            assert_eq!(claimed.view.task_id, expected_id);
            assert_eq!(claimed.access_revoked, revoked);
            assert_eq!(claimed.native_full, full);
            let persisted = store.specification(&expected_id).await.unwrap();
            assert_eq!(persisted.access_revoked, revoked);
            assert_eq!(persisted.native_full, full);
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
    async fn dispatch_policy_validates_the_bound_origin_and_profiles_before_enqueue() {
        let (_dir, store, _project, origin, _) = fixture().await;
        store
            .put_profile(AgentProfile {
                profile_id: "codex-main".into(),
                kind: AgentKind::CodexDesktop,
                program: None,
                model: None,
                reasoning_effort: None,
                environment_keys: Default::default(),
            })
            .await
            .unwrap();
        let rejected = store
            .submit_with_worker_policy(
                &origin,
                request("policy"),
                None,
                |main, worker, request, binding, native_full| {
                    assert!(!native_full);
                    assert_eq!(main.profile_id, binding.main_agent_profile_id);
                    assert_eq!(binding.route.adapter_id, "test");
                    assert_eq!(worker.profile_id, request.worker_profile_id);
                    Err(SchedulerError::new(
                        "adapter_not_authorized",
                        "Test rejects this adapter",
                    ))
                },
            )
            .await;
        assert_eq!(rejected.unwrap_err().code, "adapter_not_authorized");
        assert!(store.claim_next(2).await.unwrap().is_none());

        let accepted = store
            .submit_with_worker_policy(&origin, request("policy"), None, |_, _, _, _, _| Ok(()))
            .await
            .unwrap();
        assert!(!accepted.deduplicated);
        assert_eq!(
            store.claim_next(2).await.unwrap().unwrap().view.task_id,
            accepted.task_id
        );
    }

    async fn same_host_id_from_second_adapter(
        store: &Store,
        project: &ProjectRecord,
    ) -> AuthContext {
        let registration = store
            .ensure_registration(
                &project.config.project_id,
                "codex-main",
                ParentRoute {
                    adapter_id: "other-adapter".into(),
                    session_id: "conversation-a".into(),
                    parameters: serde_json::Value::Null,
                },
                "generation-one",
                project.root.clone(),
            )
            .await
            .unwrap();
        AuthContext::Root {
            project_id: registration.project_id,
            registration_id: registration.registration_id,
        }
    }

    #[tokio::test]
    async fn matching_host_session_ids_are_listed_as_separate_origins() {
        let (dir, store, project, first_origin, _) = fixture().await;
        let second_origin = same_host_id_from_second_adapter(&store, &project).await;
        let (_, second_id) = second_origin.root().unwrap();
        let mut second_route = store.registration(second_id).await.unwrap().route;
        second_route.parameters =
            serde_json::json!({"transport":"native_inbox","private":"unpublished"});
        store
            .refresh_route(second_id, second_route, "new-generation".into())
            .await
            .unwrap();
        let first = store.submit(&first_origin, request("same")).await.unwrap();
        let second = store.submit(&second_origin, request("same")).await.unwrap();
        drop(store);
        let store = Store::open(&dir.path().join("state.sqlite3")).unwrap();

        let admin = AuthContext::Admin;
        let page = store
            .list_sessions(&admin, &project.config.project_id, None, 10)
            .await
            .unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].transport.as_deref(), Some("native_inbox"));
        assert!(
            !serde_json::to_value(&page.items[0])
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("private")
        );
        assert!(
            serde_json::to_value(&page.items[1])
                .unwrap()
                .get("transport")
                .is_none()
        );
        for (item, auth, accepted, adapter) in [
            (&page.items[0], &second_origin, &second, "other-adapter"),
            (&page.items[1], &first_origin, &first, "test"),
        ] {
            assert_eq!(item.session_id, "conversation-a");
            assert_eq!(item.task_count, 1);
            assert_eq!(item.adapter_id, adapter);
            assert_eq!(item.origin_id, auth.root().unwrap().1);
            let filtered = store
                .origin_task_history(
                    &admin,
                    &project.config.project_id,
                    &item.origin_id,
                    None,
                    10,
                )
                .await
                .unwrap();
            assert_eq!(filtered.items.len(), 1);
            assert_eq!(filtered.items[0].task_id, accepted.task_id);
            let detail = store.task_detail(&admin, &accepted.task_id).await.unwrap();
            assert_eq!(detail.origin_id, item.origin_id);
        }
        assert_eq!(
            store
                .origin_task_history(
                    &first_origin,
                    &project.config.project_id,
                    &page.items[0].origin_id,
                    None,
                    10,
                )
                .await
                .unwrap_err()
                .code,
            "permission_denied"
        );
    }

    #[tokio::test]
    async fn ambiguous_legacy_session_id_cannot_return_combined_history() {
        let (_dir, store, project, first_origin, _) = fixture().await;
        let second_origin = same_host_id_from_second_adapter(&store, &project).await;
        store.submit(&first_origin, request("same")).await.unwrap();
        store.submit(&second_origin, request("same")).await.unwrap();

        let result = store
            .task_history(
                &AuthContext::Admin,
                Some(&project.config.project_id),
                Some("conversation-a"),
                None,
                10,
            )
            .await;
        assert_eq!(result.unwrap_err().code, "ambiguous_session_id");
    }

    #[tokio::test]
    async fn operator_task_history_pages_across_projects_and_keeps_origin_scope() {
        let (dir, store, project, first_origin, second_origin) = fixture().await;
        let mut other_project = project.clone();
        other_project.config.project_id = new_id();
        other_project.root = dir.path().join("other");
        store.put_project(other_project.clone()).await.unwrap();
        let other_registration = store
            .ensure_registration(
                &other_project.config.project_id,
                "codex-main",
                ParentRoute {
                    adapter_id: "test".into(),
                    session_id: "conversation-a".into(),
                    parameters: serde_json::json!({}),
                },
                "generation-one",
                other_project.root,
            )
            .await
            .unwrap();
        let other_origin = AuthContext::Root {
            project_id: other_registration.project_id,
            registration_id: other_registration.registration_id,
        };
        let first = store.submit(&first_origin, request("first")).await.unwrap();
        let other = store.submit(&other_origin, request("other")).await.unwrap();
        let second = store
            .submit(&second_origin, request("second"))
            .await
            .unwrap();
        let third = store.submit(&first_origin, request("third")).await.unwrap();
        let project_id = Some(project.config.project_id.as_str());
        let admin = AuthContext::Admin;

        let sessions = store
            .list_sessions(&admin, &project.config.project_id, None, 1)
            .await
            .unwrap();
        assert_eq!(sessions.items[0].session_id, "conversation-a");
        assert_eq!(sessions.items[0].task_count, 2);
        let older_sessions = store
            .list_sessions(&admin, &project.config.project_id, sessions.next_cursor, 1)
            .await
            .unwrap();
        assert_eq!(older_sessions.items[0].session_id, "conversation-b");
        assert_eq!(older_sessions.items[0].task_count, 1);
        assert_eq!(older_sessions.next_cursor, None);
        assert_eq!(
            store
                .list_sessions(&first_origin, &project.config.project_id, None, 1)
                .await
                .unwrap_err()
                .code,
            "permission_denied"
        );

        let page = store
            .task_history(&admin, project_id, None, None, 2)
            .await
            .unwrap();
        assert_eq!(page.items[0].task_id, third.task_id);
        assert_eq!(page.items[1].task_id, second.task_id);
        let newer = store.submit(&first_origin, request("newer")).await.unwrap();
        let older = store
            .task_history(&admin, project_id, None, page.next_cursor, 2)
            .await
            .unwrap();
        assert_eq!(older.items.len(), 1);
        assert_eq!(older.items[0].task_id, first.task_id);
        assert_eq!(older.next_cursor, None);
        let all = store
            .task_history(&admin, None, None, None, 10)
            .await
            .unwrap();
        assert_eq!(all.items.len(), 5);
        assert_eq!(all.items[3].task_id, other.task_id);
        let session = store
            .task_history(&admin, project_id, Some("conversation-a"), None, 2)
            .await
            .unwrap();
        assert_eq!(session.items[0].task_id, newer.task_id);
        assert_eq!(session.items[1].task_id, third.task_id);
        let older_dispatches = store
            .task_history(
                &admin,
                project_id,
                Some("conversation-a"),
                session.next_cursor,
                2,
            )
            .await
            .unwrap();
        assert_eq!(older_dispatches.items.len(), 1);
        assert_eq!(older_dispatches.items[0].task_id, first.task_id);
        assert_eq!(older_dispatches.next_cursor, None);
        assert_eq!(
            store
                .task_history(&admin, None, Some("conversation-a"), None, 10)
                .await
                .unwrap_err()
                .code,
            "project_required"
        );
        assert_eq!(
            store
                .task_history(&first_origin, None, None, None, 10)
                .await
                .unwrap_err()
                .code,
            "permission_denied"
        );
        assert_eq!(store.list_tasks(&first_origin, 50).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn operator_task_detail_preserves_original_input_and_current_state() {
        let (_dir, store, _project, origin, _) = fixture().await;
        let mut submission = request("detail");
        submission.prompt = "逐行检查\nwith original whitespace  ".into();
        submission.timeout_seconds = Some(55);
        let accepted = store.submit(&origin, submission.clone()).await.unwrap();
        let spec = store.claim_next(2).await.unwrap().unwrap();
        let completion = completion(&spec);
        store
            .commit_completion(completion, "path".into(), "digest".into(), "hash".into())
            .await
            .unwrap();
        let detail = store
            .task_detail(&AuthContext::Admin, &accepted.task_id)
            .await
            .unwrap();
        assert_eq!(detail.task.task_id, accepted.task_id);
        assert_eq!(detail.task.execution_state, ExecutionState::Succeeded);
        assert_eq!(detail.task.result.unwrap().summary, "marker");
        assert_eq!(detail.prompt, submission.prompt);
        assert_eq!(detail.timeout_seconds, 55);
        assert_eq!(detail.session_id.as_deref(), Some("conversation-a"));
        assert_eq!(
            store
                .task_detail(&origin, &accepted.task_id)
                .await
                .unwrap_err()
                .code,
            "permission_denied"
        );
    }

    #[tokio::test]
    async fn switching_main_waits_for_original_session_and_keeps_history() {
        let (_dir, store, original, origin, _) = fixture().await;
        let submitted = store.submit(&origin, request("existing")).await.unwrap();
        let mut updated = original.clone();
        updated.config.main_agent_profile_id = "claude-main".into();

        let error = store.put_project(updated.clone()).await.unwrap_err();
        assert_eq!(error.code, "project_switch_busy");

        let spec = store.claim_next(2).await.unwrap().unwrap();
        let result = store
            .commit_completion(
                completion(&spec),
                "result.json".into(),
                "hash".into(),
                nonce_hash("nonce"),
            )
            .await
            .unwrap();
        let error = store.put_project(updated.clone()).await.unwrap_err();
        assert_eq!(error.code, "project_switch_busy");

        store
            .acknowledge(&origin, &result.delivery_id, "nonce")
            .await
            .unwrap();
        store.put_project(updated).await.unwrap();
        let historical = store
            .task(&AuthContext::Admin, &submitted.task_id)
            .await
            .unwrap();
        assert_eq!(historical.project_id, original.config.project_id);
        assert_eq!(
            historical.delivery_state,
            DeliveryState::AcknowledgedByParent
        );
        assert!(store.submit(&origin, request("new")).await.is_err());
    }

    #[tokio::test]
    async fn queue_claim_is_atomic_under_concurrency() {
        let (_dir, store, _project, a, _b) = fixture().await;
        store.submit(&a, request("one")).await.unwrap();
        let (one, two) = tokio::join!(store.claim_next(2), store.claim_next(2));
        assert_eq!(
            usize::from(one.unwrap().is_some()) + usize::from(two.unwrap().is_some()),
            1
        );
    }

    #[tokio::test]
    async fn completion_and_delivery_are_transactional_and_ack_is_scoped() {
        let (_dir, store, _project, a, b) = fixture().await;
        let task = store.submit(&a, request("one")).await.unwrap();
        let spec = store.claim_next(2).await.unwrap().unwrap();
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
        assert!(
            store
                .task_for_delivery(&AuthContext::Admin, &result.delivery_id)
                .await
                .is_err()
        );
        assert!(
            store
                .task_for_delivery(&b, &result.delivery_id)
                .await
                .is_err()
        );
        assert_eq!(
            store
                .task_for_delivery(&a, &result.delivery_id)
                .await
                .unwrap()
                .task_id,
            task.task_id
        );
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
        let spec = store.claim_next(2).await.unwrap().unwrap();
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
            store.claim_next(2).await.unwrap().unwrap().view.task_id,
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
    async fn managed_worker_can_succeed_without_a_process_exit_code() {
        let (_dir, store, _project, origin, _) = fixture().await;
        store.submit(&origin, request("managed")).await.unwrap();
        let spec = store.claim_next(2).await.unwrap().unwrap();
        let mut result = completion(&spec);
        result.result.exit_code = None;
        let stored = store
            .commit_completion(result, "p".into(), "h".into(), nonce_hash("n"))
            .await
            .unwrap();
        assert_eq!(stored.execution_state, ExecutionState::Succeeded);
        assert_eq!(stored.delivery_state, DeliveryState::Pending);
    }

    #[tokio::test]
    async fn invalid_attempt_rolls_back_and_fast_parent_ack_wins_over_transport_update() {
        let (_dir, store, _project, a, _b) = fixture().await;
        store.submit(&a, request("one")).await.unwrap();
        let spec = store.claim_next(2).await.unwrap().unwrap();
        for (exit_code, error_class) in [(Some(1), None), (Some(0), Some("reported_failure"))] {
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
