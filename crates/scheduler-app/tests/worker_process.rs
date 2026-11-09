#![cfg(windows)]

use std::{path::PathBuf, time::Duration};

use scheduler_core::*;
use scheduler_platform::{DataPaths, WorkerRegistry, is_running};
use scheduler_runtime::run_worker;
use tokio_util::sync::CancellationToken;

async fn fixture(
    prompt: serde_json::Value,
    timeout: u32,
) -> (tempfile::TempDir, Store, DataPaths, QueuedTask) {
    let dir = tempfile::tempdir().unwrap();
    let paths = DataPaths::prepare(&dir.path().join("state")).unwrap();
    let root = dir.path().join("项目 示例");
    std::fs::create_dir(&root).unwrap();
    let store = Store::open(&paths.database).unwrap();
    store
        .put_profile(AgentProfile {
            profile_id: "probe".into(),
            kind: AgentKind::Probe,
            program: Some(PathBuf::from(env!("CARGO_BIN_EXE_schedulerctl"))),
            model: None,
            reasoning_effort: None,
            environment_keys: Default::default(),
        })
        .await
        .unwrap();
    let config = ProjectConfig {
        schema_version: 1,
        project_id: new_id(),
        name: "worker test".into(),
        main_agent_profile_id: "test-parent".into(),
        allowed_worker_profile_ids: ["probe".into()].into(),
        integrations: IntegrationConfig::default(),
        limits: DispatchLimits {
            default_timeout_seconds: timeout,
            max_timeout_seconds: 60,
            ..Default::default()
        },
    };
    let project = store
        .put_project(ProjectRecord {
            root,
            config,
            revision: String::new(),
        })
        .await
        .unwrap();
    let reg = store
        .ensure_registration(
            &project.config.project_id,
            "test-parent",
            ParentRoute {
                adapter_id: "fixture".into(),
                session_id: new_id(),
                parameters: serde_json::json!({}),
            },
            "g1",
            project.root.clone(),
        )
        .await
        .unwrap();
    let auth = AuthContext::Root {
        project_id: reg.project_id,
        registration_id: reg.registration_id,
    };
    store
        .submit(
            &auth,
            SubmitTask {
                worker_profile_id: "probe".into(),
                prompt: prompt.to_string(),
                idempotency_key: new_id(),
                timeout_seconds: Some(timeout),
                execution_mode: ExecutionMode::ReadOnly,
                write_scope: vec![],
            },
        )
        .await
        .unwrap();
    let task = store.claim_next(2).await.unwrap().unwrap();
    (dir, store, paths, task)
}

#[tokio::test]
async fn real_subprocess_produces_a_valid_result_and_drains_stderr() {
    let (_dir, store, paths, task) = fixture(
        serde_json::json!({"summary":"真实子进程 标记","delay_ms":1,"stderr_bytes":1024*1024}),
        15,
    )
    .await;
    let registry = WorkerRegistry::default();
    let result = run_worker(&task, &paths, &registry, CancellationToken::new(), &store)
        .await
        .unwrap();
    assert_eq!(result.execution_state, ExecutionState::Succeeded);
    assert_eq!(result.result.summary, "真实子进程 标记");
    assert_eq!(result.result.exit_code, Some(0));
    assert!(
        paths
            .attempt_dir(&task.view.task_id, &task.attempt_id)
            .unwrap()
            .join("stdout.jsonl")
            .exists()
    );
    let (_, process) = store.unfinished().await.unwrap().remove(0);
    assert!(!is_running(&process.unwrap()));
}

#[tokio::test]
async fn exit_zero_without_the_declared_final_event_is_not_success() {
    let (_dir, store, paths, task) =
        fixture(serde_json::json!({"omit_final":true,"delay_ms":1}), 10).await;
    let result = run_worker(
        &task,
        &paths,
        &WorkerRegistry::default(),
        CancellationToken::new(),
        &store,
    )
    .await
    .unwrap();
    assert_eq!(result.execution_state, ExecutionState::Failed);
    assert_eq!(
        result.result.error_class.as_deref(),
        Some("protocol_incompatible")
    );
}

#[tokio::test]
async fn deadline_terminates_only_the_supervised_worker() {
    let (_dir, store, paths, task) = fixture(serde_json::json!({"delay_ms":10000}), 1).await;
    let result = run_worker(
        &task,
        &paths,
        &WorkerRegistry::default(),
        CancellationToken::new(),
        &store,
    )
    .await
    .unwrap();
    assert_eq!(result.execution_state, ExecutionState::TimedOut);
    let (_, process) = store.unfinished().await.unwrap().remove(0);
    assert!(!is_running(&process.unwrap()));
}

#[tokio::test]
async fn cancellation_is_independent_of_a_running_subprocess() {
    let (_dir, store, paths, task) = fixture(serde_json::json!({"delay_ms":10000}), 20).await;
    let token = CancellationToken::new();
    let stop = token.clone();
    let monitor = store.clone();
    let id = task.view.task_id.clone();
    let cancel = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if monitor
                    .task(&AuthContext::Admin, &id)
                    .await
                    .unwrap()
                    .execution_state
                    == ExecutionState::Running
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        stop.cancel();
    });
    let result = run_worker(&task, &paths, &WorkerRegistry::default(), token, &store)
        .await
        .unwrap();
    cancel.await.unwrap();
    assert_eq!(result.execution_state, ExecutionState::Canceled);
}
