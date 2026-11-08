use anyhow::Result;
use async_trait::async_trait;
use scheduler_core::{DeliveryEnvelope, OriginSession, ParentRoute, ProjectRecord, Registration};

#[derive(Debug, Clone)]
pub struct ResolvedParent {
    pub route: ParentRoute,
    pub generation: String,
    pub workspace_root: std::path::PathBuf,
}

#[derive(Debug)]
pub enum DeliveryOutcome {
    Accepted { parent_turn_id: String },
    Busy,
    Held(String),
    Refused(String),
    Unavailable(String),
    Uncertain(String),
}

#[async_trait]
pub trait ParentAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    async fn resolve(
        &self,
        project: &ProjectRecord,
        origin: &OriginSession,
    ) -> Result<ResolvedParent>;
    async fn deliver(
        &self,
        registration: &Registration,
        envelope: &DeliveryEnvelope,
    ) -> DeliveryOutcome;
}
