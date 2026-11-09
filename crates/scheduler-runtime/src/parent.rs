use anyhow::Result;
use async_trait::async_trait;
use scheduler_core::{
    AgentKind, DeliveryEnvelope, OriginSession, OriginSource, ParentRoute, ProcessIdentity,
    ProjectRecord, Registration,
};

#[derive(Debug, Clone)]
pub struct ResolvedParent {
    pub route: ParentRoute,
    pub generation: String,
    pub workspace_root: std::path::PathBuf,
}

#[derive(Debug)]
pub enum DeliveryOutcome {
    Accepted {
        parent_turn_id: String,
    },
    /// The host confirmed delivery without a turn or comparable transport reference.
    AcceptedWithoutReference,
    Busy,
    Held(String),
    Refused(String),
    Unavailable(String),
    Uncertain(String),
}

#[async_trait]
pub trait ParentAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn profile_kind(&self) -> AgentKind;
    fn family(&self) -> &'static str;
    async fn resolve(
        &self,
        project: &ProjectRecord,
        origin: &OriginSession,
    ) -> Result<ResolvedParent>;
    async fn resolve_from_peer(
        &self,
        project: &ProjectRecord,
        origin: &OriginSession,
        _peer: &ProcessIdentity,
    ) -> Result<ResolvedParent> {
        self.resolve(project, origin).await
    }
    async fn refresh(
        &self,
        project: &ProjectRecord,
        registration: &Registration,
    ) -> Result<ResolvedParent> {
        self.resolve(
            project,
            &OriginSession {
                provider_session_id: registration.route.session_id.clone(),
                source: OriginSource::RequestMetadata,
            },
        )
        .await
    }
    async fn deliver(
        &self,
        registration: &Registration,
        envelope: &DeliveryEnvelope,
    ) -> DeliveryOutcome;
}
