use anyhow::Result;
use async_trait::async_trait;
use scheduler_core::{AgentKind, Completion, ExecutionMode, QueuedTask, Store};
use scheduler_platform::{DataPaths, WorkerRegistry};
use tokio_util::sync::CancellationToken;

pub struct WorkerContext<'a> {
    pub paths: &'a DataPaths,
    pub processes: &'a WorkerRegistry,
    pub store: &'a Store,
}

#[async_trait]
pub trait WorkerAdapter: Send + Sync {
    fn profile_kind(&self) -> AgentKind;
    fn family(&self) -> &'static str;
    fn supports(&self, mode: ExecutionMode) -> bool {
        mode.is_read_only()
    }
    async fn run(
        &self,
        task: &QueuedTask,
        context: WorkerContext<'_>,
        cancel: CancellationToken,
    ) -> Result<Completion>;
}

pub(crate) struct ProcessWorkerAdapter(pub AgentKind, pub &'static str);

#[async_trait]
impl WorkerAdapter for ProcessWorkerAdapter {
    fn profile_kind(&self) -> AgentKind {
        self.0.clone()
    }
    fn family(&self) -> &'static str {
        self.1
    }
    fn supports(&self, mode: ExecutionMode) -> bool {
        mode.is_read_only() || self.0 == AgentKind::ClaudePrint
    }
    async fn run(
        &self,
        task: &QueuedTask,
        context: WorkerContext<'_>,
        cancel: CancellationToken,
    ) -> Result<Completion> {
        crate::run_worker(
            task,
            context.paths,
            context.processes,
            cancel,
            context.store,
        )
        .await
    }
}
