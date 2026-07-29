//! Canonical portable workspace-layout-v1 profile and session contracts.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};

use locality_core::portable::SourceScopeId;
use locality_core::workspace_layout::{MountTarget, PortableMountId};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

pub const WORKSPACE_LAYOUT_VERSION: u16 = 1;
pub const MIN_PROFILE_MOUNTS: usize = 1;
pub const MAX_PROFILE_MOUNTS: usize = 256;
pub const MIN_PROFILE_SCOPE_BINDINGS: usize = 1;
pub const MAX_PROFILE_SCOPE_BINDINGS: usize = 4096;
pub const MAX_WORKSPACE_LAYOUT_PREIMAGE_BYTES: usize = 1024 * 1024;
pub const WORKSPACE_LAYOUT_V1_DOMAIN: &[u8] = b"locality.workspace-layout.v1\0";

pub const WORKSPACE_LAYOUT_V1_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/workspace-layout-v1.json");
pub const SESSION_LAYOUT_V1_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/session-layout-v1.json");
pub const WORKSPACE_LAYOUT_V1_PREIMAGE_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/workspace-layout-v1-preimage.json");

/// Canonical lowercase-hyphenated non-nil UUID for one workspace profile.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct WorkspaceProfileId(String);

impl WorkspaceProfileId {
    pub fn new(value: impl Into<String>) -> Result<Self, WorkspaceProfileIdError> {
        let value = value.into();
        if !is_canonical_lowercase_hyphenated_uuid(&value) {
            return Err(WorkspaceProfileIdError::NonCanonical);
        }
        if value.bytes().all(|byte| byte == b'0' || byte == b'-') {
            return Err(WorkspaceProfileIdError::Nil);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for WorkspaceProfileId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for WorkspaceProfileId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceProfileIdError {
    NonCanonical,
    Nil,
}

impl Display for WorkspaceProfileIdError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonCanonical => formatter
                .write_str("workspace profile ID must be a canonical lowercase-hyphenated UUID"),
            Self::Nil => formatter.write_str("workspace profile ID must not be the nil UUID"),
        }
    }
}

impl std::error::Error for WorkspaceProfileIdError {}

/// Canonical wire spelling of the SHA-256 workspace layout digest.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct LayoutDigest(String);

impl LayoutDigest {
    pub fn new(value: impl Into<String>) -> Result<Self, LayoutDigestError> {
        let value = value.into();
        if value.len() != 71
            || !value.starts_with("sha256:")
            || !value.as_bytes()[7..]
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        {
            return Err(LayoutDigestError::NonCanonical);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn sha256(preimage: &[u8]) -> Self {
        let digest = Sha256::digest(preimage);
        let mut value = String::with_capacity(71);
        value.push_str("sha256:");
        for byte in digest {
            use std::fmt::Write;
            write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
        }
        Self(value)
    }
}

impl Display for LayoutDigest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for LayoutDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutDigestError {
    NonCanonical,
}

impl Display for LayoutDigestError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("layout digest must be `sha256:` plus 64 lowercase hex digits")
    }
}

impl std::error::Error for LayoutDigestError {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileMount {
    mount_id: PortableMountId,
    target: MountTarget,
}

impl ProfileMount {
    pub fn new(mount_id: PortableMountId, target: MountTarget) -> Self {
        Self { mount_id, target }
    }

    pub fn mount_id(&self) -> &PortableMountId {
        &self.mount_id
    }

    pub fn target(&self) -> &MountTarget {
        &self.target
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileScopeBinding {
    scope_ordinal: u32,
    source_scope_id: SourceScopeId,
    mount_id: PortableMountId,
}

impl ProfileScopeBinding {
    pub fn new(
        scope_ordinal: u32,
        source_scope_id: SourceScopeId,
        mount_id: PortableMountId,
    ) -> Self {
        Self {
            scope_ordinal,
            source_scope_id,
            mount_id,
        }
    }

    pub fn scope_ordinal(&self) -> u32 {
        self.scope_ordinal
    }

    pub fn source_scope_id(&self) -> &SourceScopeId {
        &self.source_scope_id
    }

    pub fn mount_id(&self) -> &PortableMountId {
        &self.mount_id
    }
}

/// Complete canonical mount and source-scope mapping for one profile revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkspaceLayout {
    layout_version: u16,
    profile_id: WorkspaceProfileId,
    profile_revision: u64,
    mounts: Vec<ProfileMount>,
    scope_bindings: Vec<ProfileScopeBinding>,
    layout_digest: LayoutDigest,
}

impl WorkspaceLayout {
    pub fn new(
        profile_id: WorkspaceProfileId,
        profile_revision: u64,
        mounts: Vec<ProfileMount>,
        scope_bindings: Vec<ProfileScopeBinding>,
    ) -> Result<Self, WorkspaceLayoutError> {
        validate_profile_revision(profile_revision)?;
        let resolved_bindings = validate_profile_collections(&mounts, &scope_bindings)?;
        let preimage = encode_preimage(
            WORKSPACE_LAYOUT_VERSION,
            &profile_id,
            profile_revision,
            &mounts,
            &resolved_bindings,
        )?;
        Ok(Self {
            layout_version: WORKSPACE_LAYOUT_VERSION,
            profile_id,
            profile_revision,
            mounts,
            scope_bindings,
            layout_digest: LayoutDigest::sha256(&preimage),
        })
    }

    pub fn layout_version(&self) -> u16 {
        self.layout_version
    }

    pub fn profile_id(&self) -> &WorkspaceProfileId {
        &self.profile_id
    }

    pub fn profile_revision(&self) -> u64 {
        self.profile_revision
    }

    pub fn mounts(&self) -> &[ProfileMount] {
        &self.mounts
    }

    pub fn scope_bindings(&self) -> &[ProfileScopeBinding] {
        &self.scope_bindings
    }

    pub fn layout_digest(&self) -> &LayoutDigest {
        &self.layout_digest
    }

    pub fn canonical_preimage(&self) -> Result<Vec<u8>, WorkspaceLayoutError> {
        let resolved_bindings = validate_profile_collections(&self.mounts, &self.scope_bindings)?;
        encode_preimage(
            self.layout_version,
            &self.profile_id,
            self.profile_revision,
            &self.mounts,
            &resolved_bindings,
        )
    }

    pub fn recompute_digest(&self) -> Result<LayoutDigest, WorkspaceLayoutError> {
        Ok(LayoutDigest::sha256(&self.canonical_preimage()?))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceLayoutWire {
    layout_version: u16,
    profile_id: WorkspaceProfileId,
    profile_revision: u64,
    mounts: Vec<ProfileMount>,
    scope_bindings: Vec<ProfileScopeBinding>,
    layout_digest: LayoutDigest,
}

impl<'de> Deserialize<'de> for WorkspaceLayout {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WorkspaceLayoutWire::deserialize(deserializer)?;
        if wire.layout_version != WORKSPACE_LAYOUT_VERSION {
            return Err(serde::de::Error::custom(
                WorkspaceLayoutError::UnsupportedLayoutVersion {
                    actual: wire.layout_version,
                },
            ));
        }
        let layout = Self::new(
            wire.profile_id,
            wire.profile_revision,
            wire.mounts,
            wire.scope_bindings,
        )
        .map_err(serde::de::Error::custom)?;
        if layout.layout_digest != wire.layout_digest {
            return Err(serde::de::Error::custom(
                WorkspaceLayoutError::LayoutDigestMismatch,
            ));
        }
        Ok(layout)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLayoutEntry {
    scope_ordinal: u32,
    mount_id: PortableMountId,
    target: MountTarget,
}

impl SessionLayoutEntry {
    pub fn new(scope_ordinal: u32, mount_id: PortableMountId, target: MountTarget) -> Self {
        Self {
            scope_ordinal,
            mount_id,
            target,
        }
    }

    pub fn scope_ordinal(&self) -> u32 {
        self.scope_ordinal
    }

    pub fn mount_id(&self) -> &PortableMountId {
        &self.mount_id
    }

    pub fn target(&self) -> &MountTarget {
        &self.target
    }
}

/// Session-carried layout syntax. Profile-context verification is explicit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionLayout {
    layout_version: u16,
    layout_digest: LayoutDigest,
    entries: Vec<SessionLayoutEntry>,
}

impl SessionLayout {
    pub fn new(
        layout_digest: LayoutDigest,
        entries: Vec<SessionLayoutEntry>,
    ) -> Result<Self, WorkspaceLayoutError> {
        let mounts = validate_session_entries(&entries)?;
        // Profile IDs have a fixed 36-byte canonical spelling and revisions
        // have a fixed-width encoding, so this enforces the preimage ceiling
        // without requiring unsealed profile authority.
        let placeholder_profile_id =
            WorkspaceProfileId::new("00000000-0000-0000-0000-000000000001")
                .expect("fixed non-nil profile ID");
        encode_preimage(
            WORKSPACE_LAYOUT_VERSION,
            &placeholder_profile_id,
            1,
            &mounts,
            &session_binding_views(&entries),
        )?;
        Ok(Self {
            layout_version: WORKSPACE_LAYOUT_VERSION,
            layout_digest,
            entries,
        })
    }

    pub fn from_workspace(workspace: &WorkspaceLayout) -> Result<Self, WorkspaceLayoutError> {
        let target_by_mount = workspace
            .mounts
            .iter()
            .map(|mount| (mount.mount_id(), mount.target()))
            .collect::<BTreeMap<_, _>>();
        let entries = workspace
            .scope_bindings
            .iter()
            .map(|binding| {
                let target = target_by_mount
                    .get(binding.mount_id())
                    .expect("validated workspace mount reference");
                SessionLayoutEntry::new(
                    binding.scope_ordinal(),
                    binding.mount_id().clone(),
                    (*target).clone(),
                )
            })
            .collect();
        Self::new(workspace.layout_digest.clone(), entries)
    }

    pub fn layout_version(&self) -> u16 {
        self.layout_version
    }

    pub fn layout_digest(&self) -> &LayoutDigest {
        &self.layout_digest
    }

    pub fn entries(&self) -> &[SessionLayoutEntry] {
        &self.entries
    }

    pub fn profile_context_preimage(
        &self,
        profile_id: &WorkspaceProfileId,
        profile_revision: u64,
    ) -> Result<Vec<u8>, WorkspaceLayoutError> {
        validate_profile_revision(profile_revision)?;
        let mounts = validate_session_entries(&self.entries)?;
        encode_preimage(
            self.layout_version,
            profile_id,
            profile_revision,
            &mounts,
            &session_binding_views(&self.entries),
        )
    }

    pub fn verify_profile_context(
        &self,
        profile_id: &WorkspaceProfileId,
        profile_revision: u64,
    ) -> Result<(), WorkspaceLayoutError> {
        let digest =
            LayoutDigest::sha256(&self.profile_context_preimage(profile_id, profile_revision)?);
        if digest != self.layout_digest {
            return Err(WorkspaceLayoutError::LayoutDigestMismatch);
        }
        Ok(())
    }

    pub fn validate_against_workspace(
        &self,
        workspace: &WorkspaceLayout,
    ) -> Result<(), WorkspaceLayoutError> {
        self.verify_profile_context(workspace.profile_id(), workspace.profile_revision())?;
        if self.layout_version != workspace.layout_version
            || self.layout_digest != workspace.layout_digest
        {
            return Err(WorkspaceLayoutError::LayoutDigestMismatch);
        }
        let expected = Self::from_workspace(workspace)?;
        if self.entries != expected.entries {
            return Err(WorkspaceLayoutError::WorkspaceEntriesMismatch);
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionLayoutWire {
    layout_version: u16,
    layout_digest: LayoutDigest,
    entries: Vec<SessionLayoutEntry>,
}

impl<'de> Deserialize<'de> for SessionLayout {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SessionLayoutWire::deserialize(deserializer)?;
        if wire.layout_version != WORKSPACE_LAYOUT_VERSION {
            return Err(serde::de::Error::custom(
                WorkspaceLayoutError::UnsupportedLayoutVersion {
                    actual: wire.layout_version,
                },
            ));
        }
        Self::new(wire.layout_digest, wire.entries).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceLayoutError {
    UnsupportedLayoutVersion { actual: u16 },
    ZeroProfileRevision,
    MountCount { actual: usize },
    ScopeBindingCount { actual: usize },
    NonCanonicalMountOrder { index: usize },
    DuplicateMountId { index: usize },
    TargetCollision { index: usize },
    NonCanonicalScopeOrdinal { index: usize, actual: u32 },
    DuplicateSourceScopeId { index: usize },
    UnknownMountReference { scope_ordinal: u32 },
    UnusedMount { index: usize },
    InconsistentSessionMountTarget { scope_ordinal: u32 },
    LayoutDigestMismatch,
    WorkspaceEntriesMismatch,
    PreimageLengthOverflow,
    PreimageTooLarge { actual: usize },
}

impl Display for WorkspaceLayoutError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedLayoutVersion { actual } => {
                write!(
                    formatter,
                    "workspace layout version {actual} is unsupported"
                )
            }
            Self::ZeroProfileRevision => formatter.write_str("profile revision must be positive"),
            Self::MountCount { actual } => write!(
                formatter,
                "workspace layout has {actual} mounts; expected {MIN_PROFILE_MOUNTS} through {MAX_PROFILE_MOUNTS}"
            ),
            Self::ScopeBindingCount { actual } => write!(
                formatter,
                "workspace layout has {actual} scope bindings; expected {MIN_PROFILE_SCOPE_BINDINGS} through {MAX_PROFILE_SCOPE_BINDINGS}"
            ),
            Self::NonCanonicalMountOrder { index } => {
                write!(
                    formatter,
                    "mount at index {index} is not in exact mount-ID byte order"
                )
            }
            Self::DuplicateMountId { index } => {
                write!(formatter, "mount at index {index} duplicates a mount ID")
            }
            Self::TargetCollision { index } => {
                write!(
                    formatter,
                    "mount target at index {index} has a Unicode collision"
                )
            }
            Self::NonCanonicalScopeOrdinal { index, actual } => write!(
                formatter,
                "scope binding at index {index} has ordinal {actual}; expected {index}"
            ),
            Self::DuplicateSourceScopeId { index } => {
                write!(
                    formatter,
                    "scope binding at index {index} duplicates a source-scope ID"
                )
            }
            Self::UnknownMountReference { scope_ordinal } => write!(
                formatter,
                "scope ordinal {scope_ordinal} references an unknown mount"
            ),
            Self::UnusedMount { index } => {
                write!(formatter, "mount at index {index} has no scope binding")
            }
            Self::InconsistentSessionMountTarget { scope_ordinal } => write!(
                formatter,
                "scope ordinal {scope_ordinal} changes the target for an existing mount ID"
            ),
            Self::LayoutDigestMismatch => formatter.write_str("workspace layout digest mismatch"),
            Self::WorkspaceEntriesMismatch => {
                formatter.write_str("session layout entries do not exactly match the workspace")
            }
            Self::PreimageLengthOverflow => {
                formatter.write_str("workspace layout preimage length overflow")
            }
            Self::PreimageTooLarge { actual } => write!(
                formatter,
                "workspace layout preimage is {actual} bytes; maximum is {MAX_WORKSPACE_LAYOUT_PREIMAGE_BYTES}"
            ),
        }
    }
}

impl std::error::Error for WorkspaceLayoutError {}

struct BindingView<'a> {
    scope_ordinal: u32,
    mount_id: &'a PortableMountId,
    target: &'a MountTarget,
}

fn validate_profile_revision(profile_revision: u64) -> Result<(), WorkspaceLayoutError> {
    if profile_revision == 0 {
        return Err(WorkspaceLayoutError::ZeroProfileRevision);
    }
    Ok(())
}

fn validate_profile_collections<'a>(
    mounts: &'a [ProfileMount],
    scope_bindings: &'a [ProfileScopeBinding],
) -> Result<Vec<BindingView<'a>>, WorkspaceLayoutError> {
    validate_mount_count(mounts.len())?;
    validate_scope_binding_count(scope_bindings.len())?;
    validate_mount_order_and_targets(mounts)?;

    let target_by_mount = mounts
        .iter()
        .map(|mount| (mount.mount_id(), mount.target()))
        .collect::<BTreeMap<_, _>>();
    let mut source_scope_ids = BTreeSet::new();
    let mut used_mount_ids = BTreeSet::new();
    let mut resolved = Vec::with_capacity(scope_bindings.len());
    for (index, binding) in scope_bindings.iter().enumerate() {
        let expected =
            u32::try_from(index).map_err(|_| WorkspaceLayoutError::PreimageLengthOverflow)?;
        if binding.scope_ordinal != expected {
            return Err(WorkspaceLayoutError::NonCanonicalScopeOrdinal {
                index,
                actual: binding.scope_ordinal,
            });
        }
        if !source_scope_ids.insert(binding.source_scope_id()) {
            return Err(WorkspaceLayoutError::DuplicateSourceScopeId { index });
        }
        let Some(target) = target_by_mount.get(binding.mount_id()) else {
            return Err(WorkspaceLayoutError::UnknownMountReference {
                scope_ordinal: binding.scope_ordinal,
            });
        };
        used_mount_ids.insert(binding.mount_id());
        resolved.push(BindingView {
            scope_ordinal: binding.scope_ordinal,
            mount_id: binding.mount_id(),
            target,
        });
    }
    for (index, mount) in mounts.iter().enumerate() {
        if !used_mount_ids.contains(mount.mount_id()) {
            return Err(WorkspaceLayoutError::UnusedMount { index });
        }
    }
    Ok(resolved)
}

fn validate_session_entries(
    entries: &[SessionLayoutEntry],
) -> Result<Vec<ProfileMount>, WorkspaceLayoutError> {
    validate_scope_binding_count(entries.len())?;
    let mut target_by_mount = BTreeMap::<PortableMountId, MountTarget>::new();
    for (index, entry) in entries.iter().enumerate() {
        let expected =
            u32::try_from(index).map_err(|_| WorkspaceLayoutError::PreimageLengthOverflow)?;
        if entry.scope_ordinal != expected {
            return Err(WorkspaceLayoutError::NonCanonicalScopeOrdinal {
                index,
                actual: entry.scope_ordinal,
            });
        }
        if let Some(existing_target) = target_by_mount.get(entry.mount_id()) {
            if existing_target != entry.target() {
                return Err(WorkspaceLayoutError::InconsistentSessionMountTarget {
                    scope_ordinal: entry.scope_ordinal,
                });
            }
        } else {
            target_by_mount.insert(entry.mount_id().clone(), entry.target().clone());
        }
    }
    validate_mount_count(target_by_mount.len())?;
    let mounts = target_by_mount
        .into_iter()
        .map(|(mount_id, target)| ProfileMount::new(mount_id, target))
        .collect::<Vec<_>>();
    validate_mount_order_and_targets(&mounts)?;
    Ok(mounts)
}

fn session_binding_views(entries: &[SessionLayoutEntry]) -> Vec<BindingView<'_>> {
    entries
        .iter()
        .map(|entry| BindingView {
            scope_ordinal: entry.scope_ordinal(),
            mount_id: entry.mount_id(),
            target: entry.target(),
        })
        .collect()
}

fn validate_mount_count(actual: usize) -> Result<(), WorkspaceLayoutError> {
    if !(MIN_PROFILE_MOUNTS..=MAX_PROFILE_MOUNTS).contains(&actual) {
        return Err(WorkspaceLayoutError::MountCount { actual });
    }
    Ok(())
}

fn validate_scope_binding_count(actual: usize) -> Result<(), WorkspaceLayoutError> {
    if !(MIN_PROFILE_SCOPE_BINDINGS..=MAX_PROFILE_SCOPE_BINDINGS).contains(&actual) {
        return Err(WorkspaceLayoutError::ScopeBindingCount { actual });
    }
    Ok(())
}

fn validate_mount_order_and_targets(mounts: &[ProfileMount]) -> Result<(), WorkspaceLayoutError> {
    for (index, pair) in mounts.windows(2).enumerate() {
        match pair[0]
            .mount_id()
            .as_str()
            .as_bytes()
            .cmp(pair[1].mount_id().as_str().as_bytes())
        {
            std::cmp::Ordering::Less => {}
            std::cmp::Ordering::Equal => {
                return Err(WorkspaceLayoutError::DuplicateMountId { index: index + 1 });
            }
            std::cmp::Ordering::Greater => {
                return Err(WorkspaceLayoutError::NonCanonicalMountOrder { index: index + 1 });
            }
        }
    }
    let mut collision_keys = BTreeSet::new();
    for (index, mount) in mounts.iter().enumerate() {
        if !collision_keys.insert(mount.target().collision_key()) {
            return Err(WorkspaceLayoutError::TargetCollision { index });
        }
    }
    Ok(())
}

fn encode_preimage(
    layout_version: u16,
    profile_id: &WorkspaceProfileId,
    profile_revision: u64,
    mounts: &[ProfileMount],
    bindings: &[BindingView<'_>],
) -> Result<Vec<u8>, WorkspaceLayoutError> {
    if layout_version != WORKSPACE_LAYOUT_VERSION {
        return Err(WorkspaceLayoutError::UnsupportedLayoutVersion {
            actual: layout_version,
        });
    }
    let mut preimage = Vec::with_capacity(1024);
    preimage.extend_from_slice(WORKSPACE_LAYOUT_V1_DOMAIN);
    push_frame(&mut preimage, &layout_version.to_be_bytes())?;
    push_frame(&mut preimage, profile_id.as_str().as_bytes())?;
    push_frame(&mut preimage, &profile_revision.to_be_bytes())?;
    let mount_count =
        u64::try_from(mounts.len()).map_err(|_| WorkspaceLayoutError::PreimageLengthOverflow)?;
    push_frame(&mut preimage, &mount_count.to_be_bytes())?;
    for mount in mounts {
        push_frame(&mut preimage, mount.mount_id().as_str().as_bytes())?;
        push_frame(&mut preimage, mount.target().as_str().as_bytes())?;
    }
    let binding_count =
        u64::try_from(bindings.len()).map_err(|_| WorkspaceLayoutError::PreimageLengthOverflow)?;
    push_frame(&mut preimage, &binding_count.to_be_bytes())?;
    for binding in bindings {
        push_frame(&mut preimage, &binding.scope_ordinal.to_be_bytes())?;
        push_frame(&mut preimage, binding.mount_id.as_str().as_bytes())?;
        push_frame(&mut preimage, binding.target.as_str().as_bytes())?;
    }
    Ok(preimage)
}

fn push_frame(preimage: &mut Vec<u8>, value: &[u8]) -> Result<(), WorkspaceLayoutError> {
    let value_length =
        u64::try_from(value.len()).map_err(|_| WorkspaceLayoutError::PreimageLengthOverflow)?;
    let framed_length = 8_usize
        .checked_add(value.len())
        .ok_or(WorkspaceLayoutError::PreimageLengthOverflow)?;
    let new_length = preimage
        .len()
        .checked_add(framed_length)
        .ok_or(WorkspaceLayoutError::PreimageLengthOverflow)?;
    if new_length > MAX_WORKSPACE_LAYOUT_PREIMAGE_BYTES {
        return Err(WorkspaceLayoutError::PreimageTooLarge { actual: new_length });
    }
    preimage.extend_from_slice(&value_length.to_be_bytes());
    preimage.extend_from_slice(value);
    Ok(())
}

fn is_canonical_lowercase_hyphenated_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)
            }
        })
}
