//! Authenticated generation-aware local delivery.
//!
//! This module intentionally defines a transport trait instead of an HTTP
//! endpoint. A hosted adapter must authenticate and authorize every request;
//! tests use a deterministic fake. The local apply path is shared by future
//! `loc pull` and Live Mode integration.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};

use locality_core::model::MountId;
use locality_core::portable::{SourceConnectionId, SourceGenerationId};
use locality_protocol::freshness_delivery::{
    GenerationDelta, GenerationDeltaEntry, GenerationDeltaTerminalReceipt, GenerationFileIdentity,
};
use locality_store::{
    GenerationApplyOutcome, GenerationApplyStatus, GenerationDeliveryRepository,
    GenerationPathState, PreparedGenerationApply, SqliteStateStore,
};
use sha2::{Digest, Sha256};

use crate::durable_fs::{
    create_dir_all_durable, remove_dir_all_durable, remove_path_durable, rename_replace_durable,
    write_new_file_durable,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationDeliveryRequest {
    pub mount_id: MountId,
    pub source_connection_id: SourceConnectionId,
    pub observed_generation_id: SourceGenerationId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedGenerationDelivery {
    pub delta: GenerationDelta,
    pub terminal_receipt: GenerationDeltaTerminalReceipt,
}

/// Authenticated backend transport boundary. Implementations may use HTTP,
/// IPC, or another authenticated channel, but the public local-sync code does
/// not assume or invent a route.
pub trait GenerationDeliveryTransport {
    type Error: std::error::Error + Send + Sync + 'static;

    fn next_delta(
        &mut self,
        request: &GenerationDeliveryRequest,
    ) -> Result<Option<AuthorizedGenerationDelivery>, Self::Error>;

    fn fetch_content(
        &mut self,
        delta_id: &str,
        identity: &GenerationFileIdentity,
    ) -> Result<Vec<u8>, Self::Error>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GenerationSyncSummary {
    pub delta_id: Option<String>,
    pub applied_paths: u64,
    pub deleted_paths: u64,
    pub conflicted_paths: u64,
    pub replayed: bool,
}

pub struct GenerationSyncClient<T> {
    transport: T,
}

impl<T> GenerationSyncClient<T>
where
    T: GenerationDeliveryTransport,
{
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn sync_mount(
        &mut self,
        store: &mut SqliteStateStore,
        mount_id: &MountId,
        mount_root: &Path,
    ) -> Result<GenerationSyncSummary, GenerationSyncError> {
        let observed = store
            .get_observed_generation(mount_id)?
            .ok_or_else(|| GenerationSyncError::MissingObservedGeneration(mount_id.clone()))?;
        let request = GenerationDeliveryRequest {
            mount_id: mount_id.clone(),
            source_connection_id: observed.source_connection_id,
            observed_generation_id: observed.generation_id,
        };
        let Some(delivery) = self
            .transport
            .next_delta(&request)
            .map_err(|error| GenerationSyncError::Transport(error.to_string()))?
        else {
            return Ok(GenerationSyncSummary::default());
        };
        apply_authorized_delivery(store, mount_id, mount_root, delivery, &mut self.transport)
    }
}

pub fn apply_authorized_delivery<T: GenerationDeliveryTransport>(
    store: &mut SqliteStateStore,
    mount_id: &MountId,
    mount_root: &Path,
    delivery: AuthorizedGenerationDelivery,
    transport: &mut T,
) -> Result<GenerationSyncSummary, GenerationSyncError> {
    apply_authorized_delivery_inner(store, mount_id, mount_root, delivery, transport, None)
}

fn apply_authorized_delivery_inner<T: GenerationDeliveryTransport>(
    store: &mut SqliteStateStore,
    mount_id: &MountId,
    mount_root: &Path,
    delivery: AuthorizedGenerationDelivery,
    transport: &mut T,
    interrupt_after_filesystem_mutations: Option<usize>,
) -> Result<GenerationSyncSummary, GenerationSyncError> {
    delivery
        .terminal_receipt
        .validate_against(&delivery.delta)
        .map_err(|error| GenerationSyncError::Contract(error.to_string()))?;
    validate_single_mount_delta(&delivery.delta, mount_id)?;

    if let Some(existing) = store.get_generation_apply(&delivery.delta.delta_id)?
        && existing.status == GenerationApplyStatus::Completed
    {
        if existing.delta != delivery.delta || existing.receipt != delivery.terminal_receipt {
            return Err(GenerationSyncError::JournalMismatch);
        }
        return Ok(summary(&existing, true));
    }
    validate_local_base(store, mount_id, &delivery.delta)?;

    let stage_relative = stage_relative_path(&delivery.delta)?;
    let stage_root = store.root.join(&stage_relative);
    stage_contents(&stage_root, &delivery.delta, transport)?;
    let receipt_sha256 = delivery
        .terminal_receipt
        .canonical_sha256()
        .map_err(|error| GenerationSyncError::Contract(error.to_string()))?;
    let created_at = delivery.terminal_receipt.completed_at.clone();
    let journal = store.reserve_generation_apply(PreparedGenerationApply {
        delta: delivery.delta,
        receipt: delivery.terminal_receipt,
        receipt_sha256,
        stage_root: path_to_portable_text(&stage_relative)?,
        created_at: created_at.clone(),
    })?;
    store.mark_generation_apply_started(&journal.delta.delta_id, &created_at)?;
    let already_recorded = journal
        .outcomes
        .iter()
        .map(|(index, _)| *index)
        .collect::<BTreeSet<_>>();
    let mut filesystem_mutations = 0_usize;

    for (index, entry) in journal.delta.entries.iter().enumerate() {
        let index = index as u64;
        if already_recorded.contains(&index) {
            continue;
        }
        let (outcome, mutated) = apply_entry(
            mount_root,
            &stage_root,
            &journal.delta.delta_id,
            index,
            entry,
        )?;
        if mutated {
            filesystem_mutations += 1;
            if interrupt_after_filesystem_mutations == Some(filesystem_mutations) {
                return Err(GenerationSyncError::InjectedInterruption);
            }
        }
        store.record_generation_apply_outcome(
            &journal.delta.delta_id,
            index,
            outcome,
            &created_at,
        )?;
    }

    let completed = store.complete_generation_apply(&journal.delta.delta_id, &created_at)?;
    let completed_summary = summary(&completed, false);
    if completed_summary.conflicted_paths == 0 && stage_root.exists() {
        remove_dir_all_durable(&stage_root)?;
    }
    Ok(completed_summary)
}

fn validate_local_base(
    store: &SqliteStateStore,
    mount_id: &MountId,
    delta: &GenerationDelta,
) -> Result<(), GenerationSyncError> {
    let observed = store
        .get_observed_generation(mount_id)?
        .ok_or_else(|| GenerationSyncError::MissingObservedGeneration(mount_id.clone()))?;
    if observed.source_connection_id != delta.source_connection_id
        || observed.generation_id != delta.base_generation_id
        || observed.workspace_layout_version != delta.workspace_layout_version
        || observed.workspace_layout_digest != delta.workspace_layout_digest.as_str()
    {
        return Err(GenerationSyncError::LocalBaseMismatch);
    }
    let paths = store
        .list_generation_paths(mount_id)?
        .into_iter()
        .map(|path| (path.projection_id.clone(), path))
        .collect::<BTreeMap<_, _>>();
    for entry in &delta.entries {
        let Some(old) = &entry.old else {
            continue;
        };
        let Some(path) = paths.get(&old.projection_id) else {
            return Err(GenerationSyncError::LocalBaseMismatch);
        };
        let expected = if path.state == GenerationPathState::Conflicted {
            path.incoming_identity.as_ref()
        } else {
            path.base_identity.as_ref()
        };
        if expected != Some(old) {
            return Err(GenerationSyncError::LocalBaseMismatch);
        }
    }
    Ok(())
}

fn validate_single_mount_delta(
    delta: &GenerationDelta,
    mount_id: &MountId,
) -> Result<(), GenerationSyncError> {
    if delta.entries.is_empty() {
        return Err(GenerationSyncError::EmptyDelta);
    }
    if delta
        .entries
        .iter()
        .any(|entry| entry.mount_id.as_str() != mount_id.as_str())
    {
        return Err(GenerationSyncError::UnexpectedMount);
    }
    Ok(())
}

fn stage_relative_path(delta: &GenerationDelta) -> Result<PathBuf, GenerationSyncError> {
    let digest = delta
        .canonical_sha256()
        .map_err(|error| GenerationSyncError::Contract(error.to_string()))?;
    Ok(PathBuf::from("generation-delivery").join(&digest[7..]))
}

fn stage_contents<T: GenerationDeliveryTransport>(
    stage_root: &Path,
    delta: &GenerationDelta,
    transport: &mut T,
) -> Result<(), GenerationSyncError> {
    let payload_root = stage_root.join("payloads");
    create_dir_all_durable(&payload_root)?;
    for (index, entry) in delta.entries.iter().enumerate() {
        let Some(identity) = &entry.new else {
            continue;
        };
        let payload = payload_root.join(index.to_string());
        if payload.exists() {
            verify_file(&payload, identity)?;
            continue;
        }
        let content = transport
            .fetch_content(&delta.delta_id, identity)
            .map_err(|error| GenerationSyncError::Transport(error.to_string()))?;
        verify_content(&content, identity)?;
        write_new_file_durable(&payload, &content)?;
    }
    Ok(())
}

fn apply_entry(
    mount_root: &Path,
    stage_root: &Path,
    delta_id: &str,
    index: u64,
    entry: &GenerationDeltaEntry,
) -> Result<(GenerationApplyOutcome, bool), GenerationSyncError> {
    if let (Some(old), Some(new)) = (&entry.old, &entry.new)
        && old.logical_path != new.logical_path
    {
        return conflict_outcome(mount_root, old, Some(new));
    }
    match (&entry.old, &entry.new) {
        (None, Some(new)) => {
            let destination = checked_mount_path(mount_root, new)?;
            match digest_if_regular_file(&destination)? {
                None => {
                    publish_payload(mount_root, stage_root, delta_id, index, new, &destination)?;
                    Ok((GenerationApplyOutcome::Applied, true))
                }
                Some(actual) if actual == new.content_sha256 => {
                    Ok((GenerationApplyOutcome::Applied, false))
                }
                Some(actual) => Ok((
                    GenerationApplyOutcome::Conflict {
                        local_sha256: Some(actual),
                        incoming_identity: Some(new.clone()),
                    },
                    false,
                )),
            }
        }
        (Some(old), Some(new)) => {
            let destination = checked_mount_path(mount_root, old)?;
            match digest_if_regular_file(&destination)? {
                Some(actual) if actual == new.content_sha256 => {
                    Ok((GenerationApplyOutcome::Applied, false))
                }
                Some(actual) if actual == old.content_sha256 => {
                    publish_payload(mount_root, stage_root, delta_id, index, new, &destination)?;
                    Ok((GenerationApplyOutcome::Applied, true))
                }
                actual => Ok((
                    GenerationApplyOutcome::Conflict {
                        local_sha256: actual,
                        incoming_identity: Some(new.clone()),
                    },
                    false,
                )),
            }
        }
        (Some(old), None) => {
            let destination = checked_mount_path(mount_root, old)?;
            match digest_if_regular_file(&destination)? {
                None => Ok((GenerationApplyOutcome::Deleted, false)),
                Some(actual) if actual == old.content_sha256 => {
                    remove_path_durable(&destination)?;
                    Ok((GenerationApplyOutcome::Deleted, true))
                }
                Some(actual) => Ok((
                    GenerationApplyOutcome::Conflict {
                        local_sha256: Some(actual),
                        incoming_identity: None,
                    },
                    false,
                )),
            }
        }
        (None, None) => Err(GenerationSyncError::Contract(
            "delta entry has no identity".to_string(),
        )),
    }
}

fn conflict_outcome(
    mount_root: &Path,
    old: &GenerationFileIdentity,
    incoming: Option<&GenerationFileIdentity>,
) -> Result<(GenerationApplyOutcome, bool), GenerationSyncError> {
    let path = checked_mount_path(mount_root, old)?;
    Ok((
        GenerationApplyOutcome::Conflict {
            local_sha256: digest_if_regular_file(&path)?,
            incoming_identity: incoming.cloned(),
        },
        false,
    ))
}

fn publish_payload(
    mount_root: &Path,
    stage_root: &Path,
    delta_id: &str,
    index: u64,
    identity: &GenerationFileIdentity,
    destination: &Path,
) -> Result<(), GenerationSyncError> {
    let parent = destination
        .parent()
        .ok_or(GenerationSyncError::InvalidMountPath)?;
    validate_no_symlink_ancestry(mount_root, parent)?;
    create_dir_all_durable(parent)?;
    validate_no_symlink_ancestry(mount_root, parent)?;
    let payload = stage_root.join("payloads").join(index.to_string());
    verify_file(&payload, identity)?;
    let temp = parent.join(format!(
        ".locality-generation-{}-{index}.tmp",
        short_safe_id(delta_id)
    ));
    if temp.exists() {
        verify_file(&temp, identity)?;
    } else {
        write_new_file_durable(&temp, &fs::read(&payload)?)?;
    }
    rename_replace_durable(&temp, destination)?;
    Ok(())
}

fn checked_mount_path(
    mount_root: &Path,
    identity: &GenerationFileIdentity,
) -> Result<PathBuf, GenerationSyncError> {
    let path = mount_root.join(identity.logical_path.to_relative_path_buf());
    if !path.starts_with(mount_root) {
        return Err(GenerationSyncError::InvalidMountPath);
    }
    validate_no_symlink_ancestry(
        mount_root,
        path.parent().ok_or(GenerationSyncError::InvalidMountPath)?,
    )?;
    Ok(path)
}

fn validate_no_symlink_ancestry(root: &Path, path: &Path) -> Result<(), GenerationSyncError> {
    if !path.starts_with(root) {
        return Err(GenerationSyncError::InvalidMountPath);
    }
    let mut cursor = Some(path);
    while let Some(current) = cursor {
        match fs::symlink_metadata(current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(GenerationSyncError::SymlinkPath(current.to_path_buf()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if current == root {
            return Ok(());
        }
        cursor = current.parent();
    }
    Err(GenerationSyncError::InvalidMountPath)
}

fn digest_if_regular_file(path: &Path) -> Result<Option<String>, GenerationSyncError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(GenerationSyncError::UnsafeLocalPath(path.to_path_buf()))
        }
        Ok(_) => Ok(Some(sha256_label(&fs::read(path)?))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn verify_file(path: &Path, identity: &GenerationFileIdentity) -> Result<(), GenerationSyncError> {
    verify_content(&fs::read(path)?, identity)
}

fn verify_content(
    content: &[u8],
    identity: &GenerationFileIdentity,
) -> Result<(), GenerationSyncError> {
    if content.len() as u64 != identity.byte_length
        || sha256_label(content) != identity.content_sha256
    {
        return Err(GenerationSyncError::ContentMismatch(
            identity.content_version_id.as_str().to_string(),
        ));
    }
    Ok(())
}

fn sha256_label(value: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(value))
}

fn short_safe_id(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))[..16].to_string()
}

fn path_to_portable_text(path: &Path) -> Result<String, GenerationSyncError> {
    path.to_str()
        .map(str::to_string)
        .ok_or(GenerationSyncError::InvalidStagePath)
}

fn summary(
    journal: &locality_store::GenerationApplyJournalRecord,
    replayed: bool,
) -> GenerationSyncSummary {
    let mut summary = GenerationSyncSummary {
        delta_id: Some(journal.delta.delta_id.clone()),
        replayed,
        ..GenerationSyncSummary::default()
    };
    for (_, outcome) in &journal.outcomes {
        match outcome {
            GenerationApplyOutcome::Applied => summary.applied_paths += 1,
            GenerationApplyOutcome::Deleted => summary.deleted_paths += 1,
            GenerationApplyOutcome::Conflict { .. } => summary.conflicted_paths += 1,
        }
    }
    summary
}

#[derive(Debug)]
pub enum GenerationSyncError {
    Store(locality_store::StoreError),
    Io(std::io::Error),
    Transport(String),
    Contract(String),
    MissingObservedGeneration(MountId),
    EmptyDelta,
    UnexpectedMount,
    InvalidMountPath,
    InvalidStagePath,
    UnsafeLocalPath(PathBuf),
    SymlinkPath(PathBuf),
    ContentMismatch(String),
    LocalBaseMismatch,
    JournalMismatch,
    InjectedInterruption,
}

impl Display for GenerationSyncError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => Display::fmt(error, formatter),
            Self::Io(error) => Display::fmt(error, formatter),
            Self::Transport(error) => write!(formatter, "generation transport failed: {error}"),
            Self::Contract(error) => write!(formatter, "generation contract failed: {error}"),
            Self::MissingObservedGeneration(mount_id) => write!(
                formatter,
                "mount `{}` has no observed backend generation",
                mount_id.0
            ),
            Self::EmptyDelta => formatter.write_str("generation delta is empty"),
            Self::UnexpectedMount => formatter.write_str("generation delta names another mount"),
            Self::InvalidMountPath => formatter.write_str("generation path escapes its mount"),
            Self::InvalidStagePath => formatter.write_str("generation stage path is not UTF-8"),
            Self::UnsafeLocalPath(path) => {
                write!(
                    formatter,
                    "generation path `{}` is not a regular file",
                    path.display()
                )
            }
            Self::SymlinkPath(path) => {
                write!(
                    formatter,
                    "generation path `{}` traverses a symlink",
                    path.display()
                )
            }
            Self::ContentMismatch(id) => {
                write!(
                    formatter,
                    "generation content `{id}` failed digest or length validation"
                )
            }
            Self::LocalBaseMismatch => formatter
                .write_str("local observed generation, layout, or path base does not match delta"),
            Self::JournalMismatch => formatter.write_str("generation journal replay mismatch"),
            Self::InjectedInterruption => {
                formatter.write_str("injected generation apply interruption")
            }
        }
    }
}

impl std::error::Error for GenerationSyncError {}

impl From<locality_store::StoreError> for GenerationSyncError {
    fn from(value: locality_store::StoreError) -> Self {
        Self::Store(value)
    }
}

impl From<std::io::Error> for GenerationSyncError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use locality_core::portable::{ContentVersionId, LogicalPath, ProjectionId};
    use locality_core::workspace_layout::PortableMountId;
    use locality_protocol::FreshnessEpoch;
    use locality_protocol::freshness_delivery::{
        FRESHNESS_DELIVERY_READER_VERSION, GENERATION_DELTA_FORMAT_VERSION,
    };
    use locality_protocol::workspace_layout::LayoutDigest;
    use locality_store::{
        GenerationPathRecord, GenerationPathState, MountConfig, MountRepository,
        ObservedGenerationRecord,
    };

    use super::*;

    #[derive(Debug)]
    struct FakeTransportError(String);

    impl Display for FakeTransportError {
        fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(&self.0)
        }
    }

    impl std::error::Error for FakeTransportError {}

    #[derive(Default)]
    struct FakeTransport {
        deliveries: VecDeque<AuthorizedGenerationDelivery>,
        contents: BTreeMap<String, Vec<u8>>,
        requests: Vec<GenerationDeliveryRequest>,
        content_fetches: usize,
    }

    impl GenerationDeliveryTransport for FakeTransport {
        type Error = FakeTransportError;

        fn next_delta(
            &mut self,
            request: &GenerationDeliveryRequest,
        ) -> Result<Option<AuthorizedGenerationDelivery>, Self::Error> {
            self.requests.push(request.clone());
            Ok(self.deliveries.pop_front())
        }

        fn fetch_content(
            &mut self,
            _delta_id: &str,
            identity: &GenerationFileIdentity,
        ) -> Result<Vec<u8>, Self::Error> {
            self.content_fetches += 1;
            self.contents
                .get(identity.content_version_id.as_str())
                .cloned()
                .ok_or_else(|| FakeTransportError("missing fake content".to_string()))
        }
    }

    fn identity(
        projection: &str,
        path: &str,
        version: &str,
        content: &[u8],
    ) -> GenerationFileIdentity {
        GenerationFileIdentity {
            projection_id: ProjectionId::new(projection),
            logical_path: LogicalPath::new(path).unwrap(),
            content_version_id: ContentVersionId::new(version),
            content_sha256: sha256_label(content),
            byte_length: content.len() as u64,
        }
    }

    fn delivery(id: &str, entries: Vec<GenerationDeltaEntry>) -> AuthorizedGenerationDelivery {
        let delta = GenerationDelta {
            format_version: GENERATION_DELTA_FORMAT_VERSION,
            minimum_reader_version: FRESHNESS_DELIVERY_READER_VERSION,
            delta_id: id.to_string(),
            source_connection_id: SourceConnectionId::new("source-main"),
            base_generation_id: SourceGenerationId::new("generation-1").unwrap(),
            target_generation_id: SourceGenerationId::new("generation-2").unwrap(),
            target_complete: true,
            target_inventory_sha256: sha256_label(b"target-inventory"),
            workspace_layout_version: 1,
            workspace_layout_digest: LayoutDigest::new(sha256_label(b"layout")).unwrap(),
            entries,
        };
        let terminal_receipt = GenerationDeltaTerminalReceipt {
            format_version: delta.format_version,
            minimum_reader_version: delta.minimum_reader_version,
            delta_id: delta.delta_id.clone(),
            source_connection_id: delta.source_connection_id.clone(),
            base_generation_id: delta.base_generation_id.clone(),
            target_generation_id: delta.target_generation_id.clone(),
            target_inventory_sha256: delta.target_inventory_sha256.clone(),
            workspace_layout_version: delta.workspace_layout_version,
            workspace_layout_digest: delta.workspace_layout_digest.clone(),
            delta_sha256: delta.canonical_sha256().unwrap(),
            entry_count: delta.entries.len() as u64,
            changed_content_bytes: delta.changed_content_bytes().unwrap(),
            authorization_epoch: FreshnessEpoch::new(3).unwrap(),
            completed_at: "2026-07-31T12:00:00Z".to_string(),
        };
        AuthorizedGenerationDelivery {
            delta,
            terminal_receipt,
        }
    }

    fn seed(fixture: &Fixture, paths: Vec<GenerationFileIdentity>) -> SqliteStateStore {
        let mut store = SqliteStateStore::open(fixture.state_root.clone()).unwrap();
        store
            .save_mount(MountConfig::new(
                fixture.mount_id.clone(),
                "backend",
                &fixture.mount_root,
            ))
            .unwrap();
        store
            .seed_observed_generation(
                ObservedGenerationRecord {
                    mount_id: fixture.mount_id.clone(),
                    source_connection_id: SourceConnectionId::new("source-main"),
                    generation_id: SourceGenerationId::new("generation-1").unwrap(),
                    inventory_sha256: sha256_label(b"base-inventory"),
                    workspace_layout_version: 1,
                    workspace_layout_digest: sha256_label(b"layout"),
                    last_receipt_sha256: None,
                    updated_at: "2026-07-31T11:00:00Z".to_string(),
                },
                paths
                    .into_iter()
                    .map(|identity| GenerationPathRecord {
                        mount_id: fixture.mount_id.clone(),
                        projection_id: identity.projection_id.clone(),
                        logical_path: identity.logical_path.as_str().to_string(),
                        base_generation_id: SourceGenerationId::new("generation-1").unwrap(),
                        base_identity: Some(identity),
                        state: GenerationPathState::Clean,
                        incoming_identity: None,
                        updated_at: "2026-07-31T11:00:00Z".to_string(),
                    })
                    .collect(),
            )
            .unwrap();
        store
    }

    #[test]
    fn crash_after_file_replace_recovers_and_exact_replay_is_a_noop() {
        let fixture = Fixture::new("crash-recovery");
        let old = identity("projection-a", "Roadmap.md", "content-old", b"old");
        let new = identity("projection-a", "Roadmap.md", "content-new", b"new!");
        fs::write(fixture.mount_root.join("Roadmap.md"), b"old").unwrap();
        let mut store = seed(&fixture, vec![old.clone()]);
        let delivery = delivery(
            "delta-crash",
            vec![GenerationDeltaEntry {
                mount_id: PortableMountId::new("mount-main").unwrap(),
                old: Some(old),
                new: Some(new.clone()),
            }],
        );
        let mut transport = FakeTransport::default();
        transport
            .contents
            .insert("content-new".to_string(), b"new!".to_vec());

        let error = apply_authorized_delivery_inner(
            &mut store,
            &fixture.mount_id,
            &fixture.mount_root,
            delivery.clone(),
            &mut transport,
            Some(1),
        )
        .expect_err("injected crash");
        assert!(matches!(error, GenerationSyncError::InjectedInterruption));
        assert_eq!(
            fs::read(fixture.mount_root.join("Roadmap.md")).unwrap(),
            b"new!"
        );
        assert_eq!(
            store
                .get_observed_generation(&fixture.mount_id)
                .unwrap()
                .unwrap()
                .generation_id
                .as_str(),
            "generation-1"
        );
        assert!(
            store
                .get_generation_apply("delta-crash")
                .unwrap()
                .unwrap()
                .outcomes
                .is_empty()
        );

        let recovered = apply_authorized_delivery_inner(
            &mut store,
            &fixture.mount_id,
            &fixture.mount_root,
            delivery.clone(),
            &mut transport,
            None,
        )
        .unwrap();
        assert_eq!(recovered.applied_paths, 1);
        assert!(!recovered.replayed);
        assert_eq!(transport.content_fetches, 1, "recovery reused staged bytes");
        let completed_journal = store.get_generation_apply("delta-crash").unwrap().unwrap();
        assert!(!store.root.join(completed_journal.stage_root).exists());

        let replay = apply_authorized_delivery(
            &mut store,
            &fixture.mount_id,
            &fixture.mount_root,
            delivery,
            &mut FakeTransport::default(),
        )
        .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.applied_paths, 1);
        assert_eq!(
            store
                .get_observed_generation(&fixture.mount_id)
                .unwrap()
                .unwrap()
                .generation_id
                .as_str(),
            "generation-2"
        );
    }

    #[test]
    fn clean_deletion_applies_while_dirty_update_is_preserved_as_conflict() {
        let fixture = Fixture::new("delete-dirty");
        let delete_old = identity("projection-a", "Delete.md", "delete-old", b"delete me");
        let dirty_old = identity("projection-b", "Dirty.md", "dirty-old", b"base");
        let dirty_new = identity("projection-b", "Dirty.md", "dirty-new", b"remote");
        fs::write(fixture.mount_root.join("Delete.md"), b"delete me").unwrap();
        fs::write(fixture.mount_root.join("Dirty.md"), b"local edit").unwrap();
        let mut store = seed(&fixture, vec![delete_old.clone(), dirty_old.clone()]);
        let delivery = delivery(
            "delta-delete-dirty",
            vec![
                GenerationDeltaEntry {
                    mount_id: PortableMountId::new("mount-main").unwrap(),
                    old: Some(delete_old),
                    new: None,
                },
                GenerationDeltaEntry {
                    mount_id: PortableMountId::new("mount-main").unwrap(),
                    old: Some(dirty_old),
                    new: Some(dirty_new.clone()),
                },
            ],
        );
        let mut transport = FakeTransport::default();
        transport
            .contents
            .insert("dirty-new".to_string(), b"remote".to_vec());

        let summary = apply_authorized_delivery(
            &mut store,
            &fixture.mount_id,
            &fixture.mount_root,
            delivery,
            &mut transport,
        )
        .unwrap();

        assert_eq!(summary.deleted_paths, 1);
        assert_eq!(summary.conflicted_paths, 1);
        assert!(!fixture.mount_root.join("Delete.md").exists());
        assert_eq!(
            fs::read(fixture.mount_root.join("Dirty.md")).unwrap(),
            b"local edit"
        );
        let paths = store.list_generation_paths(&fixture.mount_id).unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].state, GenerationPathState::Conflicted);
        assert_eq!(paths[0].incoming_identity, Some(dirty_new));
        assert_eq!(paths[0].base_generation_id.as_str(), "generation-1");
        let journal = store
            .get_generation_apply("delta-delete-dirty")
            .unwrap()
            .unwrap();
        assert!(
            store.root.join(journal.stage_root).exists(),
            "conflict retains staged incoming bytes"
        );
        assert_eq!(
            store
                .get_observed_generation(&fixture.mount_id)
                .unwrap()
                .unwrap()
                .generation_id
                .as_str(),
            "generation-2"
        );
    }

    #[test]
    fn fake_transport_receives_observed_head_and_bad_content_never_reserves_journal() {
        let fixture = Fixture::new("transport-contract");
        let old = identity("projection-a", "Roadmap.md", "content-old", b"old");
        let new = identity("projection-a", "Roadmap.md", "content-new", b"good");
        fs::write(fixture.mount_root.join("Roadmap.md"), b"old").unwrap();
        let mut store = seed(&fixture, vec![old.clone()]);
        let delivery = delivery(
            "delta-bad-content",
            vec![GenerationDeltaEntry {
                mount_id: PortableMountId::new("mount-main").unwrap(),
                old: Some(old),
                new: Some(new),
            }],
        );
        let mut transport = FakeTransport::default();
        transport.deliveries.push_back(delivery);
        transport
            .contents
            .insert("content-new".to_string(), b"evil".to_vec());
        let mut client = GenerationSyncClient::new(transport);

        let error = client
            .sync_mount(&mut store, &fixture.mount_id, &fixture.mount_root)
            .expect_err("digest mismatch");
        assert!(matches!(error, GenerationSyncError::ContentMismatch(_)));
        assert_eq!(client.transport().requests.len(), 1);
        assert_eq!(
            client.transport().requests[0]
                .observed_generation_id
                .as_str(),
            "generation-1"
        );
        assert!(store.list_active_generation_applies().unwrap().is_empty());
        assert_eq!(
            fs::read(fixture.mount_root.join("Roadmap.md")).unwrap(),
            b"old"
        );
    }

    struct Fixture {
        root: PathBuf,
        state_root: PathBuf,
        mount_root: PathBuf,
        mount_id: MountId,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "locality-generation-sync-{label}-{}-{stamp}-{nonce}",
                std::process::id()
            ));
            let state_root = root.join("state");
            let mount_root = root.join("mount");
            fs::create_dir_all(&mount_root).unwrap();
            Self {
                root,
                state_root,
                mount_root,
                mount_id: MountId::new("mount-main"),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}
