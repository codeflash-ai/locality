//! Remote-truth authority boundary for the local host.
//!
//! Direct mode exposes the already-configured synchronous connector. Backend
//! mode exposes a connector-neutral replica service; callers select one
//! authority and never fall back between them.

use locality_connector::Connector;
use locality_protocol::{
    ChangesetEnvelope, ChangesetReceipt, ChangesetStatus, ChangesetStatusRequest,
    ReplicaExportFrame, ReplicaExportRequest, SessionGrant, SessionRequest,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteTruthAuthority {
    DirectSource,
    BackendReplica,
}

/// One configured authority for a mount's remote truth.
///
/// This base boundary intentionally has no connector-associated type. Direct
/// and backend replicas expose different, explicit ports, so selecting backend
/// authority can never silently invoke a locally configured connector.
pub trait RemoteTruthProvider {
    fn authority(&self) -> RemoteTruthAuthority;
}

/// Connector-neutral client port for a hosted replica service.
///
/// The associated iterator lets concrete transports stream export frames
/// without making this host boundary depend on an async runtime or HTTP stack.
pub trait ReplicaService {
    type Error;
    type Export: Iterator<Item = Result<ReplicaExportFrame, Self::Error>>;

    fn create_session(&self, request: SessionRequest) -> Result<SessionGrant, Self::Error>;
    fn open_export(&self, request: ReplicaExportRequest) -> Result<Self::Export, Self::Error>;
    fn submit_changeset(
        &self,
        changeset: ChangesetEnvelope,
    ) -> Result<ChangesetReceipt, Self::Error>;
    fn changeset_status(
        &self,
        request: ChangesetStatusRequest,
    ) -> Result<ChangesetStatus, Self::Error>;
}

/// Direct-mode provider that preserves the existing connector path.
#[derive(Debug)]
pub struct DirectSourceReplica<'a, Source: ?Sized> {
    source: &'a Source,
}

impl<Source: ?Sized> Copy for DirectSourceReplica<'_, Source> {}

impl<Source: ?Sized> Clone for DirectSourceReplica<'_, Source> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, Source: ?Sized> DirectSourceReplica<'a, Source> {
    pub fn new(source: &'a Source) -> Self {
        Self { source }
    }

    pub fn source(&self) -> &Source {
        self.source
    }
}

impl<Source> RemoteTruthProvider for DirectSourceReplica<'_, Source>
where
    Source: Connector + ?Sized,
{
    fn authority(&self) -> RemoteTruthAuthority {
        RemoteTruthAuthority::DirectSource
    }
}

/// Backend-mode provider backed only by the hosted replica-service port.
#[derive(Debug)]
pub struct BackendReplica<'a, Service: ?Sized> {
    service: &'a Service,
}

impl<Service: ?Sized> Copy for BackendReplica<'_, Service> {}

impl<Service: ?Sized> Clone for BackendReplica<'_, Service> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, Service: ?Sized> BackendReplica<'a, Service> {
    pub fn new(service: &'a Service) -> Self {
        Self { service }
    }

    pub fn service(&self) -> &Service {
        self.service
    }
}

impl<Service> RemoteTruthProvider for BackendReplica<'_, Service>
where
    Service: ReplicaService + ?Sized,
{
    fn authority(&self) -> RemoteTruthAuthority {
        RemoteTruthAuthority::BackendReplica
    }
}
