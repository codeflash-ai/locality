//! Read-only whole-workspace coordinator for durable hosted profile attachments.

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::io;
use std::path::{Path, PathBuf};

use locality_core::model::MountId;
use locality_core::workspace_layout::{MountTarget, PortableMountId};
use locality_protocol::workspace_api_v2::{
    WorkspaceClientCapabilitiesV2, WorkspaceProfileSessionV2,
};
use locality_protocol::workspace_export_v2::WorkspaceExportControlMetadataV2;
use locality_protocol::{SandboxSessionState, SessionCapability, SessionErrorCode};
use locality_store::{
    CanonicalApiOrigin, CredentialError, CredentialStore, HostedWorkspaceCredentialRef,
    HostedWorkspaceIdentity, HostedWorkspaceMountMapping, HostedWorkspaceRepository,
    HostedWorkspaceTransitionKind, LegacyWorkspaceMount, SqliteStateStore, StoreError,
    WorkspaceHostBindingResolver, open_credential_store,
};
use localityd::workspace_materializer::{
    PublishedWorkspace, StagedWorkspaceMaterialization, WorkspaceMaterializationError,
    WorkspaceOwnershipCapability, WorkspacePublicationCheckpoint, WorkspacePublicationExpectation,
    WorkspacePublicationHooks, load_workspace_publication_receipt,
    publish_staged_workspace_with_hooks, recover_and_verify_workspace_publication_state,
    remove_owned_workspace_publication,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::sandbox::{
    FreshnessWaitAvailability, SandboxContentEncodingPreference, SandboxHttpClient,
    SandboxInitError, SandboxProfileKey, WorkspaceProfileNegotiation, export_attempt_request,
    replica_encoding_for_protocol, stage_workspace_export_response, workspace_limits_for_offer,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostedWorkspaceAttachOptions {
    pub api_url: String,
    pub root: PathBuf,
    pub credential_ref: HostedWorkspaceCredentialRef,
    pub content_encoding: SandboxContentEncodingPreference,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostedWorkspaceOperation {
    Attach,
    Refresh,
    Relocate,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostedWorkspaceAttachReport {
    pub ok: bool,
    pub api_origin: String,
    pub profile_id: String,
    pub profile_revision: u64,
    pub root: String,
    pub mount_count: usize,
    pub files: u64,
    pub directories: u64,
    pub materialized_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostedWorkspaceListReport {
    pub ok: bool,
    pub attachments: Vec<HostedWorkspaceListEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostedWorkspaceListEntry {
    pub api_origin: String,
    pub profile_id: String,
    pub profile_revision: u64,
    pub root: String,
    pub layout_version: u16,
    pub layout_digest: String,
    pub mounts: Vec<HostedWorkspaceMountListEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostedWorkspaceMountListEntry {
    pub portable_mount_id: String,
    pub local_mount_id: String,
    pub mount_target: String,
    pub active: bool,
}

#[derive(Debug)]
pub enum HostedWorkspaceAttachError {
    InvalidPlacement(String),
    Credential(CredentialError),
    Protocol(SandboxInitError),
    Store(StoreError),
    Lock(String),
    Materialization(WorkspaceMaterializationError),
    GenerationTwoRequired,
    Recovery(String),
}

impl HostedWorkspaceAttachError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidPlacement(_) => "hosted_workspace_invalid_placement",
            Self::Credential(error) => error.code(),
            Self::Protocol(error) => error.code(),
            Self::Store(_) => "hosted_workspace_state_error",
            Self::Lock(_) => "workspace_path_lock_unavailable",
            Self::Materialization(_) => "hosted_workspace_materialization_failed",
            Self::GenerationTwoRequired => "workspace_layout_update_required",
            Self::Recovery(_) => "hosted_workspace_recovery_required",
        }
    }
}

impl Display for HostedWorkspaceAttachError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPlacement(message) => formatter.write_str(message),
            Self::Credential(error) => Display::fmt(error, formatter),
            Self::Protocol(error) => Display::fmt(error, formatter),
            Self::Store(error) => Display::fmt(error, formatter),
            Self::Lock(message) => write!(formatter, "workspace path lock unavailable: {message}"),
            Self::Materialization(error) => Display::fmt(error, formatter),
            Self::GenerationTwoRequired => formatter
                .write_str("hosted workspace attachment requires a generation-2 layout-1 profile"),
            Self::Recovery(message) => write!(formatter, "hosted workspace recovery: {message}"),
        }
    }
}

impl std::error::Error for HostedWorkspaceAttachError {}

impl From<CredentialError> for HostedWorkspaceAttachError {
    fn from(value: CredentialError) -> Self {
        Self::Credential(value)
    }
}

impl From<SandboxInitError> for HostedWorkspaceAttachError {
    fn from(value: SandboxInitError) -> Self {
        Self::Protocol(value)
    }
}

impl From<StoreError> for HostedWorkspaceAttachError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl From<WorkspaceMaterializationError> for HostedWorkspaceAttachError {
    fn from(value: WorkspaceMaterializationError) -> Self {
        Self::Materialization(value)
    }
}

/// Attach an untracked profile or refresh the matching durable attachment.
/// Network requests and archive staging complete before the shared path lock is
/// acquired. Only the final overlap revalidation, publication, and SQLite
/// mount-set commit execute while that lock is held.
pub fn run_hosted_workspace_attach(
    options: HostedWorkspaceAttachOptions,
) -> Result<HostedWorkspaceAttachReport, HostedWorkspaceAttachError> {
    run_hosted_workspace_attach_at_state_root(options, &locality_platform::default_state_root())
}

/// State-root-selectable variant for embedding and isolated tests.
pub fn run_hosted_workspace_attach_at_state_root(
    options: HostedWorkspaceAttachOptions,
    state_root: &Path,
) -> Result<HostedWorkspaceAttachReport, HostedWorkspaceAttachError> {
    run_hosted_workspace_operation_at_state_root(options, state_root, None)
}

pub fn run_hosted_workspace_refresh_at_state_root(
    options: HostedWorkspaceAttachOptions,
    state_root: &Path,
) -> Result<HostedWorkspaceAttachReport, HostedWorkspaceAttachError> {
    run_hosted_workspace_operation_at_state_root(
        options,
        state_root,
        Some(HostedWorkspaceOperation::Refresh),
    )
}

pub fn run_hosted_workspace_relocate_at_state_root(
    options: HostedWorkspaceAttachOptions,
    state_root: &Path,
) -> Result<HostedWorkspaceAttachReport, HostedWorkspaceAttachError> {
    run_hosted_workspace_operation_at_state_root(
        options,
        state_root,
        Some(HostedWorkspaceOperation::Relocate),
    )
}

pub fn run_hosted_workspace_operation_at_state_root(
    options: HostedWorkspaceAttachOptions,
    state_root: &Path,
    operation: Option<HostedWorkspaceOperation>,
) -> Result<HostedWorkspaceAttachReport, HostedWorkspaceAttachError> {
    let mut root = absolute_normalized_root(&options.root)?;
    validate_destination_parent(&root)?;
    if operation == Some(HostedWorkspaceOperation::Relocate) && root.exists() {
        return Err(HostedWorkspaceAttachError::InvalidPlacement(format!(
            "hosted workspace relocation destination `{}` must be absent",
            root.display()
        )));
    }
    let api_origin = CanonicalApiOrigin::new(&options.api_url)
        .map_err(|error| HostedWorkspaceAttachError::InvalidPlacement(error.to_string()))?;
    let credentials = open_credential_store(state_root);
    let profile_key = SandboxProfileKey::new(credentials.get(options.credential_ref.as_str())?)?;
    let ownership = profile_key.ownership_capability();
    let client = SandboxHttpClient::new(&options.api_url)?;
    let (session, capabilities) =
        match client.create_workspace_profile_session_negotiated(&profile_key)? {
            WorkspaceProfileNegotiation::Generation2 {
                session,
                capabilities,
            } => (session, capabilities),
            WorkspaceProfileNegotiation::Generation1(_) => {
                return Err(HostedWorkspaceAttachError::GenerationTwoRequired);
            }
        };
    let identity = HostedWorkspaceIdentity::new(api_origin, session.profile_id().clone());
    recover_hosted_workspace_identity_at_state_root(state_root, &identity, &profile_key)?;

    let mut store = SqliteStateStore::open(state_root.to_path_buf())?;
    let existing = store.get_hosted_workspace_attachment(&identity)?;
    match (operation, existing.as_ref()) {
        (Some(HostedWorkspaceOperation::Attach), Some(_)) => {
            return Err(HostedWorkspaceAttachError::InvalidPlacement(
                "hosted workspace profile is already attached; use refresh or relocate".to_string(),
            ));
        }
        (Some(HostedWorkspaceOperation::Refresh), None) => {
            return Err(HostedWorkspaceAttachError::InvalidPlacement(
                "hosted workspace profile is not attached; use attach first".to_string(),
            ));
        }
        (Some(HostedWorkspaceOperation::Relocate), None) => {
            return Err(HostedWorkspaceAttachError::InvalidPlacement(
                "hosted workspace profile is not attached; use attach first".to_string(),
            ));
        }
        (Some(HostedWorkspaceOperation::Relocate), Some(existing))
            if locality_store::host_paths_equivalent(
                locality_store::WorkspaceHostPlatform::current(),
                existing.root(),
                &root,
            ) =>
        {
            return Err(HostedWorkspaceAttachError::InvalidPlacement(
                "hosted workspace relocation destination must differ from the active root"
                    .to_string(),
            ));
        }
        (Some(HostedWorkspaceOperation::Refresh), Some(existing)) | (None, Some(existing)) => {
            if !locality_store::host_paths_equivalent(
                locality_store::WorkspaceHostPlatform::current(),
                existing.root(),
                &root,
            ) {
                return Err(HostedWorkspaceAttachError::InvalidPlacement(
                    "hosted workspace root changed; use the explicit relocate workflow".to_string(),
                ));
            }
            root = existing.root().to_path_buf();
        }
        _ => {}
    }
    let mappings = mappings_for_session(&store, &identity, &session)?;
    let transition_id = random_local_id("hosted-transition")?;
    let _transition_liveness =
        locality_platform::HostedWorkspaceTransitionLock::try_acquire(state_root, &transition_id)
            .map_err(|error| HostedWorkspaceAttachError::Lock(error.to_string()))?;
    let prepared = locality_store::PreparedHostedWorkspaceTransition::new(
        transition_id.clone(),
        identity.clone(),
        options.credential_ref,
        root.clone(),
        session.profile_revision(),
        session.session_layout().layout_version(),
        session.session_layout().layout_digest().clone(),
        mappings,
        now_timestamp(),
    )?;
    let pending = {
        // The pending row is also a path reservation consumed by connector
        // mount preflight. Publish it while holding the same lock so connector
        // creation cannot validate an overlapping root and commit through this
        // preparation boundary.
        let _path_lock = locality_platform::DaemonRemountCoordinatorLock::try_acquire(state_root)
            .map_err(|error| HostedWorkspaceAttachError::Lock(error.to_string()))?;
        revalidate_hosted_workspace_placement(&store, &identity, &root)?;
        store.begin_hosted_workspace_transition(prepared)?
    };

    let staged = match download_and_stage(
        &client,
        &session,
        &capabilities,
        options.content_encoding,
        &root,
    ) {
        Ok(staged) => staged,
        Err(error) => {
            // No final path has been touched. Cancel only this newly prepared
            // transition so a retry may negotiate a fresh immutable export.
            let _ = store.cancel_hosted_workspace_transition(&transition_id);
            return Err(error);
        }
    };

    let published = commit_staged_hosted_workspace(
        &mut store, state_root, &identity, &pending, &root, staged, &ownership,
    )?;
    Ok(HostedWorkspaceAttachReport {
        ok: true,
        api_origin: identity.api_origin().as_str().to_string(),
        profile_id: identity.profile_id().as_str().to_string(),
        profile_revision: session.profile_revision(),
        root: root.display().to_string(),
        mount_count: unique_session_mounts(&session)?.len(),
        files: published.validated.files,
        directories: published.validated.directories,
        materialized_bytes: published.validated.content_bytes,
    })
}

pub fn list_hosted_workspace_attachments_at_state_root(
    state_root: &Path,
) -> Result<HostedWorkspaceListReport, HostedWorkspaceAttachError> {
    let store = SqliteStateStore::open(state_root.to_path_buf())?;
    let mut attachments = Vec::new();
    for attachment in store.list_hosted_workspace_attachments()? {
        let mounts = store
            .list_hosted_workspace_mount_mappings(attachment.identity())?
            .into_iter()
            .map(|mapping| HostedWorkspaceMountListEntry {
                portable_mount_id: mapping.portable_mount_id().as_str().to_string(),
                local_mount_id: mapping.local_mount_id().as_str().to_string(),
                mount_target: mapping.mount_target().as_str().to_string(),
                active: mapping.is_active(),
            })
            .collect();
        attachments.push(HostedWorkspaceListEntry {
            api_origin: attachment.identity().api_origin().as_str().to_string(),
            profile_id: attachment.identity().profile_id().as_str().to_string(),
            profile_revision: attachment.profile_revision(),
            root: attachment.root().display().to_string(),
            layout_version: attachment.layout_version(),
            layout_digest: attachment.layout_digest().as_str().to_string(),
            mounts,
        });
    }
    Ok(HostedWorkspaceListReport {
        ok: true,
        attachments,
    })
}

fn download_and_stage(
    client: &SandboxHttpClient,
    session: &WorkspaceProfileSessionV2,
    capabilities: &WorkspaceClientCapabilitiesV2,
    content_encoding: SandboxContentEncodingPreference,
    root: &Path,
) -> Result<StagedWorkspaceMaterialization, HostedWorkspaceAttachError> {
    let capability = SessionCapability {
        session_id: session.session_id().clone(),
        opaque_capability: session.opaque_capability().to_string(),
        expires_at: session.expires_at().to_string(),
    };
    let mut status = client.workspace_session_status(session, capabilities)?;
    if status.state() == SandboxSessionState::Bootstrapping
        && status.error().is_some_and(|error| {
            error.retriable
                && matches!(
                    error.code,
                    SessionErrorCode::Bootstrapping
                        | SessionErrorCode::Stale
                        | SessionErrorCode::Incomplete
                )
        })
        && status.freshness_requirement().on_stale
            == locality_protocol::StaleSessionBehavior::WaitThenFail
        && capabilities.supports_freshness_wait()
        && client.wait_for_workspace_freshness(
            session,
            capabilities,
            status.freshness_requirement(),
        )? == FreshnessWaitAvailability::Completed
    {
        status = client.workspace_session_status(session, capabilities)?;
    }
    if status.state() != SandboxSessionState::Ready {
        return Err(SandboxInitError::SessionNotReady {
            state: status.state(),
            code: status.error().map(|error| error.code),
        }
        .into());
    }
    if status.error().is_some() {
        return Err(SandboxInitError::InvalidReadySession("error is present").into());
    }
    let limits = status
        .export_attempt_limits()
        .ok_or(SandboxInitError::InvalidReadySession(
            "export-attempt limits are absent",
        ))?;
    let request = export_attempt_request(&capability, content_encoding, limits)?;
    let offer = client.create_workspace_export_attempt(session, &status, capabilities, &request)?;
    let encoding = replica_encoding_for_protocol(offer.offer().content_encoding);
    if let Some(required) = content_encoding.required_encoding()
        && required != encoding
    {
        return Err(SandboxInitError::UnsupportedExportEncoding(format!(
            "{} (requested {})",
            encoding_name(encoding),
            encoding_name(required)
        ))
        .into());
    }
    let response = client.open_workspace_export_attempt(session, &offer)?;
    let materialization_limits = workspace_limits_for_offer(&offer)?;
    stage_workspace_export_response(
        response,
        encoding,
        root,
        materialization_limits,
        session,
        &offer,
    )
    .map_err(Into::into)
}

fn commit_staged_hosted_workspace(
    store: &mut SqliteStateStore,
    state_root: &Path,
    identity: &HostedWorkspaceIdentity,
    pending: &locality_store::PendingHostedWorkspaceTransition,
    root: &Path,
    staged: StagedWorkspaceMaterialization,
    ownership: &WorkspaceOwnershipCapability,
) -> Result<PublishedWorkspace, HostedWorkspaceAttachError> {
    let _path_lock = locality_platform::DaemonRemountCoordinatorLock::try_acquire(state_root)
        .map_err(|error| HostedWorkspaceAttachError::Lock(error.to_string()))?;
    revalidate_hosted_workspace_placement(store, identity, root)?;
    revalidate_pending_transition(store, pending)?;
    if pending.kind() == HostedWorkspaceTransitionKind::Relocate && root.exists() {
        return Err(HostedWorkspaceAttachError::InvalidPlacement(format!(
            "hosted workspace relocation destination `{}` is no longer absent",
            root.display()
        )));
    }
    validate_receipt_metadata_against_pending(
        &staged.validated().terminal_control.metadata,
        pending,
    )?;
    let mut hooks = HostedWorkspacePublicationHooks {
        store,
        identity,
        pending,
        root,
        committed: false,
    };
    let published = publish_staged_workspace_with_hooks(staged, root, ownership, &mut hooks)?;
    if !hooks.committed {
        return Err(HostedWorkspaceAttachError::Recovery(
            "publication completed without committing attachment state".to_string(),
        ));
    }
    drop(hooks);
    complete_pending_cleanup(store, identity, ownership)?;
    Ok(published)
}

struct HostedWorkspacePublicationHooks<'a> {
    store: &'a mut SqliteStateStore,
    identity: &'a HostedWorkspaceIdentity,
    pending: &'a locality_store::PendingHostedWorkspaceTransition,
    root: &'a Path,
    committed: bool,
}

impl WorkspacePublicationHooks for HostedWorkspacePublicationHooks<'_> {
    fn before_publication(&mut self) -> io::Result<()> {
        revalidate_hosted_workspace_placement(self.store, self.identity, self.root)
            .map_err(io::Error::other)?;
        revalidate_pending_transition(self.store, self.pending).map_err(io::Error::other)?;
        if self.pending.kind() == HostedWorkspaceTransitionKind::Relocate && self.root.exists() {
            return Err(io::Error::other(
                "hosted workspace relocation destination is no longer absent",
            ));
        }
        Ok(())
    }

    fn checkpoint(&mut self, checkpoint: WorkspacePublicationCheckpoint) -> io::Result<()> {
        if checkpoint == WorkspacePublicationCheckpoint::ReceiptDurable {
            self.store
                .commit_hosted_workspace_transition(
                    self.pending.prepared().transition_id(),
                    &now_timestamp(),
                )
                .map_err(io::Error::other)?;
            self.committed = true;
        }
        Ok(())
    }
}

/// Recover publication journals and pending transitions without contacting the
/// hosted API. Revocation therefore cannot strand a locally owned generation.
pub fn recover_hosted_workspace_attachments_at_state_root(
    state_root: &Path,
) -> Result<(), HostedWorkspaceAttachError> {
    let credentials = open_credential_store(state_root);
    recover_hosted_workspace_attachments_with_credentials_at_state_root(
        state_root,
        credentials.as_ref(),
    )
}

/// Credential-injected recovery entry point for embedded owners and isolated
/// tests. A missing credential scopes recovery out for only that identity.
pub fn recover_hosted_workspace_attachments_with_credentials_at_state_root(
    state_root: &Path,
    credentials: &dyn CredentialStore,
) -> Result<(), HostedWorkspaceAttachError> {
    let mut store = SqliteStateStore::open(state_root.to_path_buf())?;
    let mut identities = BTreeMap::new();
    for attachment in store.list_hosted_workspace_attachments()? {
        identities.insert(
            attachment.identity().clone(),
            attachment.credential_ref().clone(),
        );
    }
    for transition in store.list_pending_hosted_workspace_transitions()? {
        identities.insert(
            transition.prepared().identity().clone(),
            transition.prepared().credential_ref().clone(),
        );
    }
    for cleanup in store.list_pending_hosted_workspace_cleanups()? {
        identities.insert(cleanup.identity().clone(), cleanup.credential_ref().clone());
    }
    for (identity, credential_ref) in identities {
        let secret = match credentials.get(credential_ref.as_str()) {
            Ok(secret) => secret,
            Err(CredentialError::NotFound(_)) => continue,
            Err(error) => return Err(error.into()),
        };
        let key = SandboxProfileKey::new(secret)?;
        recover_hosted_workspace_identity_locked(&mut store, state_root, &identity, &key)?;
    }
    Ok(())
}

fn recover_hosted_workspace_identity_at_state_root(
    state_root: &Path,
    identity: &HostedWorkspaceIdentity,
    key: &SandboxProfileKey,
) -> Result<(), HostedWorkspaceAttachError> {
    let mut store = SqliteStateStore::open(state_root.to_path_buf())?;
    recover_hosted_workspace_identity_locked(&mut store, state_root, identity, key)
}

fn recover_hosted_workspace_identity_locked(
    store: &mut SqliteStateStore,
    state_root: &Path,
    identity: &HostedWorkspaceIdentity,
    key: &SandboxProfileKey,
) -> Result<(), HostedWorkspaceAttachError> {
    let attachment = store.get_hosted_workspace_attachment(identity)?;
    let pending = store.get_pending_hosted_workspace_transition(identity)?;
    let _transition_liveness = if let Some(pending) = &pending {
        match locality_platform::HostedWorkspaceTransitionLock::try_acquire(
            state_root,
            pending.prepared().transition_id(),
        ) {
            Ok(lock) => Some(lock),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(HostedWorkspaceAttachError::Lock(error.to_string())),
        }
    } else {
        None
    };
    let Some(root) = pending
        .as_ref()
        .map(|pending| pending.prepared().target_root().to_path_buf())
        .or_else(|| {
            attachment
                .as_ref()
                .map(|attachment| attachment.root().to_path_buf())
        })
    else {
        complete_pending_cleanup(store, identity, &key.ownership_capability())?;
        return Ok(());
    };
    let _path_lock = locality_platform::DaemonRemountCoordinatorLock::try_acquire(state_root)
        .map_err(|error| HostedWorkspaceAttachError::Lock(error.to_string()))?;
    revalidate_hosted_workspace_placement(store, identity, &root)?;
    let verified =
        recover_and_verify_workspace_publication_state(&root, &key.ownership_capability())?;
    let Some(pending) = pending else {
        if attachment.is_some() && !verified {
            return Err(HostedWorkspaceAttachError::Recovery(
                "attached workspace root is not bound to a durable publication receipt".to_string(),
            ));
        }
        complete_pending_cleanup(store, identity, &key.ownership_capability())?;
        return Ok(());
    };
    if !verified {
        if !root.exists() {
            store.cancel_hosted_workspace_transition(pending.prepared().transition_id())?;
            complete_pending_cleanup(store, identity, &key.ownership_capability())?;
            return Ok(());
        }
        return Err(HostedWorkspaceAttachError::Recovery(
            "pending workspace root has no verifiable publication receipt".to_string(),
        ));
    }
    let receipt = load_workspace_publication_receipt(&root)?.ok_or_else(|| {
        HostedWorkspaceAttachError::Recovery(
            "verified workspace publication has no receipt".to_string(),
        )
    })?;
    let metadata = &receipt.terminal_control.metadata;
    let proposed_matches = metadata.profile_id() == identity.profile_id()
        && metadata.profile_revision() == pending.prepared().profile_revision()
        && metadata.layout_version() == pending.prepared().layout_version()
        && metadata.layout_digest() == pending.prepared().layout_digest();
    if proposed_matches {
        validate_receipt_metadata_against_pending(metadata, &pending)?;
        store.commit_hosted_workspace_transition(
            pending.prepared().transition_id(),
            &now_timestamp(),
        )?;
        complete_pending_cleanup(store, identity, &key.ownership_capability())?;
        return Ok(());
    }
    let current_matches = attachment.as_ref().is_some_and(|current| {
        current.root() == root
            && metadata.profile_id() == identity.profile_id()
            && metadata.profile_revision() == current.profile_revision()
            && metadata.layout_version() == current.layout_version()
            && metadata.layout_digest() == current.layout_digest()
    });
    if current_matches {
        store.cancel_hosted_workspace_transition(pending.prepared().transition_id())?;
        complete_pending_cleanup(store, identity, &key.ownership_capability())?;
        return Ok(());
    }
    Err(HostedWorkspaceAttachError::Recovery(
        "published receipt matches neither pending nor active attachment state".to_string(),
    ))
}

fn complete_pending_cleanup(
    store: &mut SqliteStateStore,
    identity: &HostedWorkspaceIdentity,
    ownership: &WorkspaceOwnershipCapability,
) -> Result<(), HostedWorkspaceAttachError> {
    let Some(cleanup) = store.get_pending_hosted_workspace_cleanup(identity)? else {
        return Ok(());
    };
    remove_owned_workspace_publication(
        cleanup.root(),
        &WorkspacePublicationExpectation {
            profile_id: identity.profile_id().clone(),
            profile_revision: cleanup.profile_revision(),
            layout_version: cleanup.layout_version(),
            layout_digest: cleanup.layout_digest().clone(),
        },
        ownership,
    )?;
    store.complete_hosted_workspace_cleanup(cleanup.cleanup_id())?;
    Ok(())
}

fn revalidate_hosted_workspace_placement(
    store: &SqliteStateStore,
    identity: &HostedWorkspaceIdentity,
    root: &Path,
) -> Result<(), HostedWorkspaceAttachError> {
    let mut active = SqliteStateStore::inspect_mount_roots_read_only(&store.root)?;
    for (index, attachment) in store
        .list_hosted_workspace_attachments()?
        .into_iter()
        .enumerate()
    {
        if attachment.identity() != identity || attachment.root() != root {
            active.push(LegacyWorkspaceMount::new(
                MountId::new(format!("hosted-workspace-{index}")),
                attachment.root(),
            ));
        }
    }
    for (index, pending) in store
        .list_pending_hosted_workspace_transitions()?
        .into_iter()
        .enumerate()
    {
        if pending.prepared().identity() != identity {
            active.push(LegacyWorkspaceMount::new(
                MountId::new(format!("pending-hosted-workspace-{index}")),
                pending.prepared().target_root(),
            ));
        }
    }
    for (index, cleanup) in store
        .list_pending_hosted_workspace_cleanups()?
        .into_iter()
        .enumerate()
    {
        if cleanup.identity() != identity || cleanup.root() != root {
            active.push(LegacyWorkspaceMount::new(
                MountId::new(format!("cleanup-hosted-workspace-{index}")),
                cleanup.root(),
            ));
        }
    }
    WorkspaceHostBindingResolver::current()
        .resolve_ephemeral_publication_root_on_current_host(root, &active)
        .map_err(|error| HostedWorkspaceAttachError::InvalidPlacement(error.to_string()))?;
    Ok(())
}

fn revalidate_pending_transition(
    store: &SqliteStateStore,
    expected: &locality_store::PendingHostedWorkspaceTransition,
) -> Result<(), HostedWorkspaceAttachError> {
    let actual = store.get_pending_hosted_workspace_transition(expected.prepared().identity())?;
    if actual.as_ref() != Some(expected) {
        return Err(HostedWorkspaceAttachError::Recovery(
            "hosted workspace pending transition changed before publication".to_string(),
        ));
    }
    Ok(())
}

fn validate_receipt_metadata_against_pending(
    metadata: &WorkspaceExportControlMetadataV2,
    pending: &locality_store::PendingHostedWorkspaceTransition,
) -> Result<(), HostedWorkspaceAttachError> {
    let prepared = pending.prepared();
    if metadata.profile_id() != prepared.identity().profile_id()
        || metadata.profile_revision() != prepared.profile_revision()
        || metadata.layout_version() != prepared.layout_version()
        || metadata.layout_digest() != prepared.layout_digest()
    {
        return Err(HostedWorkspaceAttachError::Recovery(
            "workspace publication receipt does not match the pending attachment".to_string(),
        ));
    }
    Ok(())
}

/// Revalidate a connector mount candidate against durable hosted roots. The
/// caller must hold [`locality_platform::DaemonRemountCoordinatorLock`], the
/// same shared path-mutation exclusion used by hosted publication.
pub fn revalidate_connector_mount_placement_at_state_root(
    state_root: &Path,
    mount_id: &MountId,
    root: &Path,
) -> Result<(), HostedWorkspaceAttachError> {
    let root = absolute_normalized_root(root)?;
    let store = SqliteStateStore::open(state_root.to_path_buf())?;
    let mut active = SqliteStateStore::inspect_mount_roots_read_only(state_root)?
        .into_iter()
        .filter(|mount| &mount.mount_id != mount_id)
        .collect::<Vec<_>>();
    for (index, attachment) in store
        .list_hosted_workspace_attachments()?
        .into_iter()
        .enumerate()
    {
        active.push(LegacyWorkspaceMount::new(
            MountId::new(format!("hosted-workspace-{index}")),
            attachment.root(),
        ));
    }
    for (index, pending) in store
        .list_pending_hosted_workspace_transitions()?
        .into_iter()
        .enumerate()
    {
        active.push(LegacyWorkspaceMount::new(
            MountId::new(format!("pending-hosted-workspace-{index}")),
            pending.prepared().target_root(),
        ));
    }
    for (index, cleanup) in store
        .list_pending_hosted_workspace_cleanups()?
        .into_iter()
        .enumerate()
    {
        active.push(LegacyWorkspaceMount::new(
            MountId::new(format!("cleanup-hosted-workspace-{index}")),
            cleanup.root(),
        ));
    }
    WorkspaceHostBindingResolver::current()
        .resolve_ephemeral_publication_root_on_current_host(&root, &active)
        .map_err(|error| HostedWorkspaceAttachError::InvalidPlacement(error.to_string()))?;
    Ok(())
}

fn mappings_for_session(
    store: &SqliteStateStore,
    identity: &HostedWorkspaceIdentity,
    session: &WorkspaceProfileSessionV2,
) -> Result<Vec<HostedWorkspaceMountMapping>, HostedWorkspaceAttachError> {
    let existing = store
        .list_hosted_workspace_mount_mappings(identity)?
        .into_iter()
        .map(|mapping| (mapping.portable_mount_id().clone(), mapping))
        .collect::<BTreeMap<_, _>>();
    unique_session_mounts(session)?
        .into_iter()
        .map(|(portable, target)| {
            let local = existing
                .get(&portable)
                .map(|mapping| mapping.local_mount_id().clone())
                .unwrap_or_else(|| deterministic_local_mount_id(identity, &portable));
            HostedWorkspaceMountMapping::proposal(
                portable,
                local,
                target,
                session.profile_revision(),
            )
            .map_err(Into::into)
        })
        .collect()
}

fn unique_session_mounts(
    session: &WorkspaceProfileSessionV2,
) -> Result<BTreeMap<PortableMountId, MountTarget>, HostedWorkspaceAttachError> {
    let mut mounts = BTreeMap::new();
    for entry in session.session_layout().entries() {
        if let Some(existing) = mounts.insert(entry.mount_id().clone(), entry.target().clone())
            && existing != *entry.target()
        {
            return Err(HostedWorkspaceAttachError::Recovery(
                "session maps one portable mount ID to several targets".to_string(),
            ));
        }
    }
    Ok(mounts)
}

pub fn deterministic_local_mount_id(
    identity: &HostedWorkspaceIdentity,
    portable_mount_id: &PortableMountId,
) -> MountId {
    let mut digest = Sha256::new();
    digest.update(b"locality.hosted-workspace.mount-id.v1\0");
    digest.update(identity.api_origin().as_str().as_bytes());
    digest.update(b"\0");
    digest.update(identity.profile_id().as_str().as_bytes());
    digest.update(b"\0");
    digest.update(portable_mount_id.as_str().as_bytes());
    let digest = digest.finalize();
    let mut value = String::with_capacity("hosted-mount-".len() + digest.len() * 2);
    value.push_str("hosted-mount-");
    for byte in digest {
        use std::fmt::Write;
        write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
    }
    MountId::new(value)
}

fn random_local_id(prefix: &str) -> Result<String, HostedWorkspaceAttachError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| HostedWorkspaceAttachError::Recovery(error.to_string()))?;
    let mut value = String::with_capacity(prefix.len() + 1 + bytes.len() * 2);
    value.push_str(prefix);
    value.push('-');
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(value)
}

fn absolute_normalized_root(root: &Path) -> Result<PathBuf, HostedWorkspaceAttachError> {
    let root = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| HostedWorkspaceAttachError::InvalidPlacement(error.to_string()))?
            .join(root)
    };
    if root.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        return Err(HostedWorkspaceAttachError::InvalidPlacement(
            "hosted workspace root must not contain `.` or `..` components".to_string(),
        ));
    }
    Ok(root)
}

fn validate_destination_parent(root: &Path) -> Result<(), HostedWorkspaceAttachError> {
    let parent = root.parent().ok_or_else(|| {
        HostedWorkspaceAttachError::InvalidPlacement(
            "hosted workspace root must have an existing parent".to_string(),
        )
    })?;
    let metadata = std::fs::symlink_metadata(parent).map_err(|error| {
        HostedWorkspaceAttachError::InvalidPlacement(format!(
            "hosted workspace parent `{}` is unavailable: {error}",
            parent.display()
        ))
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(HostedWorkspaceAttachError::InvalidPlacement(format!(
            "hosted workspace parent `{}` must be a real directory",
            parent.display()
        )));
    }
    Ok(())
}

fn now_timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn encoding_name(encoding: localityd::remote_truth::ReplicaArchiveEncoding) -> &'static str {
    match encoding {
        localityd::remote_truth::ReplicaArchiveEncoding::Identity => "identity",
        localityd::remote_truth::ReplicaArchiveEncoding::Zstd => "zstd",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use locality_protocol::workspace_export_v2::WorkspaceExportTerminalControlV2;
    use locality_store::{InMemoryStateStore, PreparedHostedWorkspaceTransition};

    fn prepared(
        transition_id: &str,
        revision: u64,
        digest: &str,
        root: &Path,
    ) -> PreparedHostedWorkspaceTransition {
        let identity = HostedWorkspaceIdentity::new(
            CanonicalApiOrigin::new("https://api.example.com").unwrap(),
            locality_protocol::workspace_layout::WorkspaceProfileId::new(
                "018f4f6e-9f2c-7b1a-8c3d-4e5f60718293",
            )
            .unwrap(),
        );
        PreparedHostedWorkspaceTransition::new(
            transition_id,
            identity,
            HostedWorkspaceCredentialRef::new("hosted-workspace:test").unwrap(),
            root,
            revision,
            1,
            locality_protocol::workspace_layout::LayoutDigest::new(digest).unwrap(),
            vec![
                HostedWorkspaceMountMapping::proposal(
                    PortableMountId::new("portable").unwrap(),
                    MountId::new("local"),
                    MountTarget::new("docs").unwrap(),
                    revision,
                )
                .unwrap(),
            ],
            "2026-08-03T00:00:00Z",
        )
        .unwrap()
    }

    #[test]
    fn receipt_metadata_must_match_the_exact_pending_attachment() {
        let control: WorkspaceExportTerminalControlV2 = serde_json::from_str(include_str!(
            "../../locality-protocol/fixtures/workspace-export-terminal-control-v2.json"
        ))
        .unwrap();
        let root = std::env::temp_dir().join("locality-receipt-binding-test");
        let mut store = InMemoryStateStore::new();
        let matching = store
            .begin_hosted_workspace_transition(prepared(
                "matching",
                7,
                "sha256:6d739ad2748910520e4df1d680e9ea78a94230d0751e6346d3f5d3c57b9259b5",
                &root,
            ))
            .unwrap();
        validate_receipt_metadata_against_pending(&control.metadata, &matching).unwrap();

        store
            .cancel_hosted_workspace_transition("matching")
            .unwrap();
        let mismatched = store
            .begin_hosted_workspace_transition(prepared(
                "mismatched",
                8,
                "sha256:6d739ad2748910520e4df1d680e9ea78a94230d0751e6346d3f5d3c57b9259b5",
                &root,
            ))
            .unwrap();
        let error =
            validate_receipt_metadata_against_pending(&control.metadata, &mismatched).unwrap_err();
        assert_eq!(error.code(), "hosted_workspace_recovery_required");
    }

    #[test]
    fn prepublication_revalidation_rejects_replaced_pending_payload() {
        let root = std::env::temp_dir().join(random_local_id("pending-revalidation").unwrap());
        let state_root = root.join("state");
        let workspace_root = root.join("Workspace");
        let mut store = SqliteStateStore::open(state_root.clone()).unwrap();
        let expected = store
            .begin_hosted_workspace_transition(prepared(
                "expected",
                7,
                "sha256:6d739ad2748910520e4df1d680e9ea78a94230d0751e6346d3f5d3c57b9259b5",
                &workspace_root,
            ))
            .unwrap();
        store
            .cancel_hosted_workspace_transition("expected")
            .unwrap();
        store
            .begin_hosted_workspace_transition(prepared(
                "replacement",
                8,
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                &workspace_root,
            ))
            .unwrap();

        let error = revalidate_pending_transition(&store, &expected).unwrap_err();
        assert_eq!(error.code(), "hosted_workspace_recovery_required");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn relocation_prepublication_revalidation_rejects_raced_destination_creation() {
        let root = std::env::temp_dir().join(random_local_id("relocation-race").unwrap());
        let state_root = root.join("state");
        let old_root = root.join("OldWorkspace");
        let new_root = root.join("NewWorkspace");
        std::fs::create_dir_all(&root).unwrap();
        let mut store = SqliteStateStore::open(state_root.clone()).unwrap();
        let initial = store
            .begin_hosted_workspace_transition(prepared(
                "initial",
                7,
                "sha256:6d739ad2748910520e4df1d680e9ea78a94230d0751e6346d3f5d3c57b9259b5",
                &old_root,
            ))
            .unwrap();
        let identity = initial.prepared().identity().clone();
        store
            .commit_hosted_workspace_transition("initial", "2026-08-03T00:00:01Z")
            .unwrap();
        let pending = store
            .begin_hosted_workspace_transition(prepared(
                "relocate",
                8,
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                &new_root,
            ))
            .unwrap();
        assert_eq!(pending.kind(), HostedWorkspaceTransitionKind::Relocate);
        std::fs::create_dir(&new_root).unwrap();
        let mut hooks = HostedWorkspacePublicationHooks {
            store: &mut store,
            identity: &identity,
            pending: &pending,
            root: &new_root,
            committed: false,
        };
        let error = hooks
            .before_publication()
            .expect_err("raced destination must block relocation publication");
        assert!(error.to_string().contains("no longer absent"));
        drop(hooks);
        assert_eq!(
            store
                .get_hosted_workspace_attachment(&identity)
                .unwrap()
                .unwrap()
                .root(),
            old_root
        );
        assert!(
            store
                .get_pending_hosted_workspace_transition(&identity)
                .unwrap()
                .is_some()
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
