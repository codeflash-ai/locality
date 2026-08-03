//! Durable attachment state for read-only hosted workspace profiles.
//!
//! Hosted profile mounts deliberately do not use [`crate::MountConfig`]. That
//! table is the connector runtime inventory consumed by discovery, Live Mode,
//! push, and per-mount pull. Keeping hosted mappings in this separate boundary
//! makes their read-only, whole-workspace publication semantics structural.

use std::fmt::{Display, Formatter};
use std::path::{Component, Path, PathBuf};

use locality_core::model::MountId;
use locality_core::workspace_layout::{MountTarget, PortableMountId};
use locality_protocol::workspace_layout::{LayoutDigest, WorkspaceProfileId};
use url::Url;

use crate::{StoreError, StoreResult};

pub const HOSTED_WORKSPACE_ATTACHMENT_COMPONENT_VERSION: u32 = 1;
pub const HOSTED_WORKSPACE_LAYOUT_VERSION: u16 = 1;

/// Canonical tuple member used with a hosted profile ID as durable identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalApiOrigin(String);

impl CanonicalApiOrigin {
    pub fn new(value: impl AsRef<str>) -> Result<Self, CanonicalApiOriginError> {
        let value = value.as_ref();
        if value.trim() != value {
            return Err(CanonicalApiOriginError::Invalid);
        }
        let parsed = Url::parse(value).map_err(|_| CanonicalApiOriginError::Invalid)?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.cannot_be_a_base()
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !matches!(parsed.path(), "" | "/")
        {
            return Err(CanonicalApiOriginError::Invalid);
        }
        Ok(Self(parsed.origin().ascii_serialization()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for CanonicalApiOrigin {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CanonicalApiOriginError {
    Invalid,
}

impl Display for CanonicalApiOriginError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "API origin must be an HTTP(S) origin without credentials, path, query, or fragment",
        )
    }
}

impl std::error::Error for CanonicalApiOriginError {}

/// Opaque credential-store lookup key. Plain profile keys cannot satisfy this
/// syntax and therefore cannot accidentally cross the SQLite boundary.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostedWorkspaceCredentialRef(String);

impl HostedWorkspaceCredentialRef {
    pub const PREFIX: &'static str = "hosted-workspace:";
    pub const MAX_BYTES: usize = 256;

    pub fn new(value: impl Into<String>) -> Result<Self, HostedWorkspaceCredentialRefError> {
        let value = value.into();
        let suffix = value
            .strip_prefix(Self::PREFIX)
            .ok_or(HostedWorkspaceCredentialRefError::Invalid)?;
        if suffix.is_empty()
            || value.len() > Self::MAX_BYTES
            || !suffix.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
            })
        {
            return Err(HostedWorkspaceCredentialRefError::Invalid);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostedWorkspaceCredentialRefError {
    Invalid,
}

impl Display for HostedWorkspaceCredentialRefError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("hosted workspace credential reference is invalid")
    }
}

impl std::error::Error for HostedWorkspaceCredentialRefError {}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostedWorkspaceIdentity {
    api_origin: CanonicalApiOrigin,
    profile_id: WorkspaceProfileId,
}

impl HostedWorkspaceIdentity {
    pub fn new(api_origin: CanonicalApiOrigin, profile_id: WorkspaceProfileId) -> Self {
        Self {
            api_origin,
            profile_id,
        }
    }

    pub fn api_origin(&self) -> &CanonicalApiOrigin {
        &self.api_origin
    }

    pub fn profile_id(&self) -> &WorkspaceProfileId {
        &self.profile_id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostedWorkspaceTransitionKind {
    Attach,
    Refresh,
    Relocate,
}

impl HostedWorkspaceTransitionKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Attach => "attach",
            Self::Refresh => "refresh",
            Self::Relocate => "relocate",
        }
    }

    pub(crate) fn parse(value: &str) -> StoreResult<Self> {
        match value {
            "attach" => Ok(Self::Attach),
            "refresh" => Ok(Self::Refresh),
            "relocate" => Ok(Self::Relocate),
            _ => Err(StoreError::InvalidState(
                "hosted workspace transition has an unknown kind".to_string(),
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostedWorkspaceMountMapping {
    portable_mount_id: PortableMountId,
    local_mount_id: MountId,
    mount_target: MountTarget,
    active: bool,
    first_seen_revision: u64,
    last_seen_revision: u64,
}

impl HostedWorkspaceMountMapping {
    pub fn proposal(
        portable_mount_id: PortableMountId,
        local_mount_id: MountId,
        mount_target: MountTarget,
        profile_revision: u64,
    ) -> StoreResult<Self> {
        validate_local_mount_id(&local_mount_id)?;
        validate_profile_revision(profile_revision)?;
        Ok(Self {
            portable_mount_id,
            local_mount_id,
            mount_target,
            active: true,
            first_seen_revision: profile_revision,
            last_seen_revision: profile_revision,
        })
    }

    pub(crate) fn persisted(
        portable_mount_id: PortableMountId,
        local_mount_id: MountId,
        mount_target: MountTarget,
        active: bool,
        first_seen_revision: u64,
        last_seen_revision: u64,
    ) -> StoreResult<Self> {
        validate_local_mount_id(&local_mount_id)?;
        validate_profile_revision(first_seen_revision)?;
        validate_profile_revision(last_seen_revision)?;
        if first_seen_revision > last_seen_revision {
            return Err(StoreError::InvalidState(
                "hosted workspace mount first revision exceeds its last revision".to_string(),
            ));
        }
        Ok(Self {
            portable_mount_id,
            local_mount_id,
            mount_target,
            active,
            first_seen_revision,
            last_seen_revision,
        })
    }

    pub fn portable_mount_id(&self) -> &PortableMountId {
        &self.portable_mount_id
    }

    pub fn local_mount_id(&self) -> &MountId {
        &self.local_mount_id
    }

    pub fn mount_target(&self) -> &MountTarget {
        &self.mount_target
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn first_seen_revision(&self) -> u64 {
        self.first_seen_revision
    }

    pub fn last_seen_revision(&self) -> u64 {
        self.last_seen_revision
    }

    pub(crate) fn with_history(mut self, first_seen_revision: u64) -> Self {
        self.first_seen_revision = first_seen_revision;
        self
    }

    pub(crate) fn inactive(mut self) -> Self {
        self.active = false;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostedWorkspaceAttachment {
    identity: HostedWorkspaceIdentity,
    credential_ref: HostedWorkspaceCredentialRef,
    root: PathBuf,
    profile_revision: u64,
    layout_version: u16,
    layout_digest: LayoutDigest,
    updated_at: String,
}

impl HostedWorkspaceAttachment {
    pub(crate) fn new(
        identity: HostedWorkspaceIdentity,
        credential_ref: HostedWorkspaceCredentialRef,
        root: PathBuf,
        profile_revision: u64,
        layout_version: u16,
        layout_digest: LayoutDigest,
        updated_at: String,
    ) -> StoreResult<Self> {
        validate_host_root(&root)?;
        validate_profile_revision(profile_revision)?;
        validate_layout_version(layout_version)?;
        validate_timestamp(&updated_at)?;
        Ok(Self {
            identity,
            credential_ref,
            root,
            profile_revision,
            layout_version,
            layout_digest,
            updated_at,
        })
    }

    pub fn identity(&self) -> &HostedWorkspaceIdentity {
        &self.identity
    }

    pub fn credential_ref(&self) -> &HostedWorkspaceCredentialRef {
        &self.credential_ref
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn profile_revision(&self) -> u64 {
        self.profile_revision
    }

    pub fn layout_version(&self) -> u16 {
        self.layout_version
    }

    pub fn layout_digest(&self) -> &LayoutDigest {
        &self.layout_digest
    }

    pub fn updated_at(&self) -> &str {
        &self.updated_at
    }
}

/// Full proposed profile state reserved before network download or staging.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedHostedWorkspaceTransition {
    transition_id: String,
    identity: HostedWorkspaceIdentity,
    credential_ref: HostedWorkspaceCredentialRef,
    target_root: PathBuf,
    profile_revision: u64,
    layout_version: u16,
    layout_digest: LayoutDigest,
    mounts: Vec<HostedWorkspaceMountMapping>,
    created_at: String,
}

impl PreparedHostedWorkspaceTransition {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        transition_id: impl Into<String>,
        identity: HostedWorkspaceIdentity,
        credential_ref: HostedWorkspaceCredentialRef,
        target_root: impl Into<PathBuf>,
        profile_revision: u64,
        layout_version: u16,
        layout_digest: LayoutDigest,
        mounts: Vec<HostedWorkspaceMountMapping>,
        created_at: impl Into<String>,
    ) -> StoreResult<Self> {
        let transition = Self {
            transition_id: transition_id.into(),
            identity,
            credential_ref,
            target_root: target_root.into(),
            profile_revision,
            layout_version,
            layout_digest,
            mounts,
            created_at: created_at.into(),
        };
        transition.validate()?;
        Ok(transition)
    }

    pub fn validate(&self) -> StoreResult<()> {
        validate_transition_id(&self.transition_id)?;
        validate_host_root(&self.target_root)?;
        validate_profile_revision(self.profile_revision)?;
        validate_layout_version(self.layout_version)?;
        validate_timestamp(&self.created_at)?;
        if self.mounts.is_empty() || self.mounts.len() > 256 {
            return Err(StoreError::InvalidState(
                "hosted workspace transition must contain 1 through 256 mounts".to_string(),
            ));
        }
        let mut portable = std::collections::BTreeSet::new();
        let mut local = std::collections::BTreeSet::new();
        let mut targets = std::collections::BTreeSet::new();
        let mut previous_portable: Option<&PortableMountId> = None;
        for mount in &self.mounts {
            if !mount.active || mount.last_seen_revision != self.profile_revision {
                return Err(StoreError::InvalidState(
                    "hosted workspace transition mounts must be active at the proposed revision"
                        .to_string(),
                ));
            }
            if !portable.insert(mount.portable_mount_id.clone())
                || !local.insert(mount.local_mount_id.clone())
                || !targets.insert(mount.mount_target.collision_key())
            {
                return Err(StoreError::InvalidState(
                    "hosted workspace transition mount IDs and targets must be distinct"
                        .to_string(),
                ));
            }
            if previous_portable.is_some_and(|previous| previous >= mount.portable_mount_id()) {
                return Err(StoreError::InvalidState(
                    "hosted workspace transition mounts must use canonical portable-ID order"
                        .to_string(),
                ));
            }
            previous_portable = Some(mount.portable_mount_id());
        }
        Ok(())
    }

    pub fn transition_id(&self) -> &str {
        &self.transition_id
    }

    pub fn identity(&self) -> &HostedWorkspaceIdentity {
        &self.identity
    }

    pub fn credential_ref(&self) -> &HostedWorkspaceCredentialRef {
        &self.credential_ref
    }

    pub fn target_root(&self) -> &Path {
        &self.target_root
    }

    pub fn profile_revision(&self) -> u64 {
        self.profile_revision
    }

    pub fn layout_version(&self) -> u16 {
        self.layout_version
    }

    pub fn layout_digest(&self) -> &LayoutDigest {
        &self.layout_digest
    }

    pub fn mounts(&self) -> &[HostedWorkspaceMountMapping] {
        &self.mounts
    }

    pub fn created_at(&self) -> &str {
        &self.created_at
    }
}

fn validate_transition_id(value: &str) -> StoreResult<()> {
    if value.is_empty()
        || value.len() > 200
        || value.bytes().any(|byte| {
            !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
        })
    {
        return Err(StoreError::InvalidState(
            "hosted workspace transition ID is invalid".to_string(),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingHostedWorkspaceTransition {
    prepared: PreparedHostedWorkspaceTransition,
    kind: HostedWorkspaceTransitionKind,
    base_profile_revision: Option<u64>,
    base_layout_digest: Option<LayoutDigest>,
    base_root: Option<PathBuf>,
}

impl PendingHostedWorkspaceTransition {
    pub(crate) fn new(
        prepared: PreparedHostedWorkspaceTransition,
        kind: HostedWorkspaceTransitionKind,
        base_profile_revision: Option<u64>,
        base_layout_digest: Option<LayoutDigest>,
        base_root: Option<PathBuf>,
    ) -> StoreResult<Self> {
        if base_profile_revision.is_some() != base_layout_digest.is_some()
            || base_profile_revision.is_some() != base_root.is_some()
            || (kind == HostedWorkspaceTransitionKind::Attach && base_profile_revision.is_some())
            || (kind != HostedWorkspaceTransitionKind::Attach && base_profile_revision.is_none())
        {
            return Err(StoreError::InvalidState(
                "hosted workspace transition has inconsistent base state".to_string(),
            ));
        }
        Ok(Self {
            prepared,
            kind,
            base_profile_revision,
            base_layout_digest,
            base_root,
        })
    }

    pub fn prepared(&self) -> &PreparedHostedWorkspaceTransition {
        &self.prepared
    }

    pub fn kind(&self) -> HostedWorkspaceTransitionKind {
        self.kind
    }

    pub fn base_profile_revision(&self) -> Option<u64> {
        self.base_profile_revision
    }

    pub fn base_layout_digest(&self) -> Option<&LayoutDigest> {
        self.base_layout_digest.as_ref()
    }

    pub fn base_root(&self) -> Option<&Path> {
        self.base_root.as_deref()
    }
}

pub(crate) fn prepare_pending_transition(
    attachment: Option<&HostedWorkspaceAttachment>,
    existing_mappings: &[HostedWorkspaceMountMapping],
    reserved_local_mount_ids: &std::collections::BTreeSet<MountId>,
    mut prepared: PreparedHostedWorkspaceTransition,
) -> StoreResult<PendingHostedWorkspaceTransition> {
    prepared.validate()?;
    if attachment.is_some_and(|attachment| attachment.identity() != prepared.identity()) {
        return Err(StoreError::InvalidState(
            "hosted workspace attachment identity changed during transition".to_string(),
        ));
    }

    let existing_by_portable = existing_mappings
        .iter()
        .map(|mapping| (mapping.portable_mount_id().clone(), mapping))
        .collect::<std::collections::BTreeMap<_, _>>();
    let existing_by_local = existing_mappings
        .iter()
        .map(|mapping| {
            (
                mapping.local_mount_id().clone(),
                mapping.portable_mount_id(),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    for mapping in &mut prepared.mounts {
        if let Some(existing) = existing_by_portable.get(mapping.portable_mount_id()) {
            if mapping.local_mount_id() != existing.local_mount_id() {
                return Err(StoreError::InvalidState(format!(
                    "portable mount `{}` is already mapped to local mount `{}`",
                    mapping.portable_mount_id(),
                    existing.local_mount_id().as_str()
                )));
            }
            mapping.first_seen_revision = existing.first_seen_revision();
        } else if let Some(existing_portable) = existing_by_local.get(mapping.local_mount_id()) {
            return Err(StoreError::InvalidState(format!(
                "local mount `{}` is already mapped to portable mount `{existing_portable}`",
                mapping.local_mount_id().as_str()
            )));
        } else if reserved_local_mount_ids.contains(mapping.local_mount_id()) {
            return Err(StoreError::InvalidState(format!(
                "local mount `{}` is already reserved outside this hosted profile",
                mapping.local_mount_id().as_str()
            )));
        }
    }

    let (kind, base_profile_revision, base_layout_digest, base_root) = match attachment {
        None => (HostedWorkspaceTransitionKind::Attach, None, None, None),
        Some(current) => {
            if prepared.profile_revision() < current.profile_revision() {
                return Err(StoreError::InvalidState(
                    "hosted workspace profile revision cannot move backward".to_string(),
                ));
            }
            if prepared.profile_revision() == current.profile_revision() {
                let current_mounts = existing_mappings
                    .iter()
                    .filter(|mapping| mapping.is_active())
                    .map(|mapping| {
                        (
                            mapping.portable_mount_id(),
                            mapping.local_mount_id(),
                            mapping.mount_target(),
                        )
                    })
                    .collect::<Vec<_>>();
                let proposed_mounts = prepared
                    .mounts()
                    .iter()
                    .map(|mapping| {
                        (
                            mapping.portable_mount_id(),
                            mapping.local_mount_id(),
                            mapping.mount_target(),
                        )
                    })
                    .collect::<Vec<_>>();
                if prepared.layout_version() != current.layout_version()
                    || prepared.layout_digest() != current.layout_digest()
                    || prepared.target_root() != current.root()
                    || proposed_mounts != current_mounts
                {
                    return Err(StoreError::InvalidState(
                        "a hosted workspace profile revision cannot be reinterpreted".to_string(),
                    ));
                }
            }
            let kind = if prepared.target_root() == current.root() {
                HostedWorkspaceTransitionKind::Refresh
            } else {
                HostedWorkspaceTransitionKind::Relocate
            };
            (
                kind,
                Some(current.profile_revision()),
                Some(current.layout_digest().clone()),
                Some(current.root().to_path_buf()),
            )
        }
    };
    PendingHostedWorkspaceTransition::new(
        prepared,
        kind,
        base_profile_revision,
        base_layout_digest,
        base_root,
    )
}

pub(crate) fn committed_attachment(
    pending: &PendingHostedWorkspaceTransition,
    current: Option<&HostedWorkspaceAttachment>,
    committed_at: impl Into<String>,
) -> StoreResult<HostedWorkspaceAttachment> {
    let matches_base = match (pending.base_profile_revision(), current) {
        (None, None) => true,
        (Some(base_revision), Some(current)) => {
            current.profile_revision() == base_revision
                && Some(current.layout_digest()) == pending.base_layout_digest()
                && Some(current.root()) == pending.base_root()
        }
        _ => false,
    };
    if !matches_base {
        return Err(StoreError::InvalidState(
            "hosted workspace attachment changed after its transition was prepared".to_string(),
        ));
    }
    let prepared = pending.prepared();
    HostedWorkspaceAttachment::new(
        prepared.identity().clone(),
        prepared.credential_ref().clone(),
        prepared.target_root().to_path_buf(),
        prepared.profile_revision(),
        prepared.layout_version(),
        prepared.layout_digest().clone(),
        committed_at.into(),
    )
}

fn validate_identifier(label: &str, value: &str, max: usize) -> StoreResult<()> {
    if value.is_empty()
        || value.len() > max
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(StoreError::InvalidState(format!(
            "hosted workspace {label} is invalid"
        )));
    }
    Ok(())
}

fn validate_local_mount_id(mount_id: &MountId) -> StoreResult<()> {
    validate_identifier("local mount ID", mount_id.as_str(), 200)
}

fn validate_profile_revision(profile_revision: u64) -> StoreResult<()> {
    if profile_revision == 0 || profile_revision > i64::MAX as u64 {
        return Err(StoreError::InvalidState(
            "hosted workspace profile revision is invalid".to_string(),
        ));
    }
    Ok(())
}

fn validate_layout_version(layout_version: u16) -> StoreResult<()> {
    if layout_version != HOSTED_WORKSPACE_LAYOUT_VERSION {
        return Err(StoreError::InvalidState(format!(
            "hosted workspace layout version {layout_version} is unsupported"
        )));
    }
    Ok(())
}

fn validate_host_root(root: &Path) -> StoreResult<()> {
    if !root.is_absolute()
        || root.as_os_str().is_empty()
        || root
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(StoreError::InvalidState(
            "hosted workspace root must be an absolute normalized local path".to_string(),
        ));
    }
    Ok(())
}

fn validate_timestamp(value: &str) -> StoreResult<()> {
    validate_identifier("timestamp", value, 100)
}
