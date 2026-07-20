//! Remote-truth authority boundary for the local host.
//!
//! Direct mode exposes the already-configured synchronous connector. Backend
//! mode will use a separate replica-service implementation; callers select one
//! authority and never fall back between them.

use locality_connector::Connector;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteTruthAuthority {
    DirectSource,
    BackendReplica,
}

/// One configured authority for a mount's remote truth.
///
/// Execution policy belongs to the source before it is placed behind this
/// boundary. The provider deliberately does not implement [`Connector`]: a
/// borrowed adapter cannot faithfully implement `Connector::with_execution_policy`,
/// which returns a newly configured owned connector.
pub trait RemoteTruthProvider {
    type Source: Connector + ?Sized;

    fn authority(&self) -> RemoteTruthAuthority;
    fn source(&self) -> &Self::Source;
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
}

impl<Source> RemoteTruthProvider for DirectSourceReplica<'_, Source>
where
    Source: Connector + ?Sized,
{
    type Source = Source;

    fn authority(&self) -> RemoteTruthAuthority {
        RemoteTruthAuthority::DirectSource
    }

    fn source(&self) -> &Self::Source {
        self.source
    }
}
