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
use locality_protocol::{SandboxSessionState, SessionCapability, SessionErrorCode};
use locality_store::{
    CanonicalApiOrigin, CredentialError, HostedWorkspaceCredentialRef, HostedWorkspaceIdentity,
    HostedWorkspaceMountMapping, HostedWorkspaceRepository, LegacyWorkspaceMount, SqliteStateStore,
    StoreError, WorkspaceHostBindingResolver, open_credential_store,
};
use localityd::workspace_materializer::{
    PublishedWorkspace, StagedWorkspaceMaterialization, WorkspaceMaterializationError,
    WorkspaceOwnershipCapability, WorkspacePublicationCheckpoint, WorkspacePublicationHooks,
    load_workspace_publication_receipt, publish_staged_workspace_with_hooks,
    recover_and_verify_workspace_publication_state,
};

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostedWorkspaceAttachReport {
    pub api_origin: String,
    pub profile_id: String,
    pub profile_revision: u64,
    pub root: String,
    pub mount_count: usize,
    pub files: u64,
    pub directories: u64,
    pub materialized_bytes: u64,
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
    recover_hosted_workspace_attachments_at_state_root(state_root)?;
    let mut root = absolute_normalized_root(&options.root)?;
    validate_destination_parent(&root)?;
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
    if let Some(existing) = store.get_hosted_workspace_attachment(&identity)? {
        if !locality_store::host_paths_equivalent(
            locality_store::WorkspaceHostPlatform::current(),
            existing.root(),
            &root,
        ) {
            return Err(HostedWorkspaceAttachError::InvalidPlacement(
                "hosted workspace relocation is not enabled by this attach/refresh coordinator"
                    .to_string(),
            ));
        }
        root = existing.root().to_path_buf();
    }
    let mappings = mappings_for_session(&store, &identity, &session)?;
    let transition_id = random_local_id("hosted-transition")?;
    let pending = locality_store::PreparedHostedWorkspaceTransition::new(
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
    {
        // The pending row is also a path reservation consumed by connector
        // mount preflight. Publish it while holding the same lock so connector
        // creation cannot validate an overlapping root and commit through this
        // preparation boundary.
        let _path_lock = locality_platform::DaemonRemountCoordinatorLock::try_acquire(state_root)
            .map_err(|error| HostedWorkspaceAttachError::Lock(error.to_string()))?;
        revalidate_hosted_workspace_placement(&store, &identity, &root)?;
        store.begin_hosted_workspace_transition(pending)?;
    }

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
        &mut store,
        state_root,
        &identity,
        &transition_id,
        &root,
        staged,
        &ownership,
    )?;
    Ok(HostedWorkspaceAttachReport {
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
    transition_id: &str,
    root: &Path,
    staged: StagedWorkspaceMaterialization,
    ownership: &WorkspaceOwnershipCapability,
) -> Result<PublishedWorkspace, HostedWorkspaceAttachError> {
    let _path_lock = locality_platform::DaemonRemountCoordinatorLock::try_acquire(state_root)
        .map_err(|error| HostedWorkspaceAttachError::Lock(error.to_string()))?;
    revalidate_hosted_workspace_placement(store, identity, root)?;
    let mut hooks = HostedWorkspacePublicationHooks {
        store,
        identity,
        transition_id,
        root,
        committed: false,
    };
    let published = publish_staged_workspace_with_hooks(staged, root, ownership, &mut hooks)?;
    if !hooks.committed {
        return Err(HostedWorkspaceAttachError::Recovery(
            "publication completed without committing attachment state".to_string(),
        ));
    }
    Ok(published)
}

struct HostedWorkspacePublicationHooks<'a> {
    store: &'a mut SqliteStateStore,
    identity: &'a HostedWorkspaceIdentity,
    transition_id: &'a str,
    root: &'a Path,
    committed: bool,
}

impl WorkspacePublicationHooks for HostedWorkspacePublicationHooks<'_> {
    fn before_publication(&mut self) -> io::Result<()> {
        revalidate_hosted_workspace_placement(self.store, self.identity, self.root)
            .map_err(io::Error::other)
    }

    fn checkpoint(&mut self, checkpoint: WorkspacePublicationCheckpoint) -> io::Result<()> {
        if checkpoint == WorkspacePublicationCheckpoint::ReceiptDurable {
            self.store
                .commit_hosted_workspace_transition(self.transition_id, &now_timestamp())
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
    let mut store = SqliteStateStore::open(state_root.to_path_buf())?;
    let credentials = open_credential_store(state_root);
    let attachments = store.list_hosted_workspace_attachments()?;
    for attachment in &attachments {
        let key = SandboxProfileKey::new(credentials.get(attachment.credential_ref().as_str())?)?;
        recover_hosted_workspace_identity_locked(
            &mut store,
            state_root,
            attachment.identity(),
            &key,
        )?;
    }
    let pending = store.list_pending_hosted_workspace_transitions()?;
    for transition in pending {
        let key = SandboxProfileKey::new(
            credentials.get(transition.prepared().credential_ref().as_str())?,
        )?;
        recover_hosted_workspace_identity_locked(
            &mut store,
            state_root,
            transition.prepared().identity(),
            &key,
        )?;
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
    let Some(root) = pending
        .as_ref()
        .map(|pending| pending.prepared().target_root().to_path_buf())
        .or_else(|| {
            attachment
                .as_ref()
                .map(|attachment| attachment.root().to_path_buf())
        })
    else {
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
        return Ok(());
    };
    if !verified {
        if attachment.is_none() && !root.exists() {
            store.cancel_hosted_workspace_transition(pending.prepared().transition_id())?;
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
        store.commit_hosted_workspace_transition(
            pending.prepared().transition_id(),
            &now_timestamp(),
        )?;
        return Ok(());
    }
    let current_matches = attachment.as_ref().is_some_and(|current| {
        metadata.profile_id() == identity.profile_id()
            && metadata.profile_revision() == current.profile_revision()
            && metadata.layout_version() == current.layout_version()
            && metadata.layout_digest() == current.layout_digest()
    });
    if current_matches {
        store.cancel_hosted_workspace_transition(pending.prepared().transition_id())?;
        return Ok(());
    }
    Err(HostedWorkspaceAttachError::Recovery(
        "published receipt matches neither pending nor active attachment state".to_string(),
    ))
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
    WorkspaceHostBindingResolver::current()
        .resolve_ephemeral_publication_root_on_current_host(root, &active)
        .map_err(|error| HostedWorkspaceAttachError::InvalidPlacement(error.to_string()))?;
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
                .map(Ok)
                .unwrap_or_else(random_local_mount_id);
            HostedWorkspaceMountMapping::proposal(
                portable,
                local?,
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

fn random_local_mount_id() -> Result<MountId, HostedWorkspaceAttachError> {
    random_local_id("hosted-mount").map(MountId::new)
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
