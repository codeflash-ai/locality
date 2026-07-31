//! Authenticated generation-aware local delivery.
//!
//! This module intentionally defines a transport trait instead of an HTTP
//! endpoint. A hosted adapter must authenticate and authorize every request;
//! tests use a deterministic fake. The local apply path is shared by future
//! `loc pull` and Live Mode integration.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt::{Display, Formatter};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

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
    create_dir_all_durable, remove_dir_all_durable, remove_path_durable, rename_noreplace_durable,
};
use crate::generation_mount::{SecureMount, SecureTarget};

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

    fn open_content(
        &mut self,
        delta_id: &str,
        identity: &GenerationFileIdentity,
    ) -> Result<Box<dyn Read + Send>, Self::Error>;
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
    apply_authorized_delivery_inner(
        store,
        mount_id,
        mount_root,
        delivery,
        transport,
        None,
        &mut ApplyHooks::default(),
    )
}

#[derive(Default)]
struct ApplyHooks<'a> {
    after_target_open: Option<&'a mut dyn FnMut()>,
    after_preimage_move: Option<&'a mut dyn FnMut()>,
}

fn apply_authorized_delivery_inner<T: GenerationDeliveryTransport>(
    store: &mut SqliteStateStore,
    mount_id: &MountId,
    mount_root: &Path,
    delivery: AuthorizedGenerationDelivery,
    transport: &mut T,
    interrupt_after_filesystem_mutations: Option<usize>,
    hooks: &mut ApplyHooks<'_>,
) -> Result<GenerationSyncSummary, GenerationSyncError> {
    delivery
        .terminal_receipt
        .validate_against(&delivery.delta)
        .map_err(|error| GenerationSyncError::Contract(error.to_string()))?;
    validate_mount_delta(&delivery.delta, mount_id)?;

    if let Some(existing) = store.get_generation_apply(&delivery.delta.delta_id)?
        && existing.status == GenerationApplyStatus::Completed
    {
        if existing.delta != delivery.delta || existing.receipt != delivery.terminal_receipt {
            return Err(GenerationSyncError::JournalMismatch);
        }
        return Ok(summary(&existing, true));
    }
    validate_local_base(store, mount_id, &delivery.delta)?;
    let _mount_guard = MountApplyGuard::acquire(mount_root)?;
    let secure_mount = SecureMount::open(mount_root).map_err(GenerationSyncError::MountAccess)?;

    let stage_relative = stage_relative_path(&delivery.delta)?;
    let stage_root = store.root.join(&stage_relative);
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
    stage_contents(&stage_root, &journal.delta, transport)?;
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
            &secure_mount,
            &stage_root,
            &journal.delta.delta_id,
            index,
            entry,
            hooks,
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

static ACTIVE_MOUNT_APPLIES: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();

struct MountApplyGuard {
    key: PathBuf,
}

impl MountApplyGuard {
    fn acquire(mount_root: &Path) -> Result<Self, GenerationSyncError> {
        let key = mount_root
            .canonicalize()
            .map_err(GenerationSyncError::MountAccess)?;
        let mut active = ACTIVE_MOUNT_APPLIES
            .get_or_init(|| Mutex::new(BTreeSet::new()))
            .lock()
            .map_err(|_| GenerationSyncError::MountCoordinatorPoisoned)?;
        if !active.insert(key.clone()) {
            return Err(GenerationSyncError::MountBusy);
        }
        Ok(Self { key })
    }
}

impl Drop for MountApplyGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = ACTIVE_MOUNT_APPLIES
            .get_or_init(|| Mutex::new(BTreeSet::new()))
            .lock()
        {
            active.remove(&self.key);
        }
    }
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

fn validate_mount_delta(
    delta: &GenerationDelta,
    mount_id: &MountId,
) -> Result<(), GenerationSyncError> {
    if delta.mount_id.as_str() != mount_id.as_str() {
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
            if verify_file(&payload, identity).is_ok() {
                continue;
            }
            remove_path_durable(&payload)?;
        }
        let partial = payload_root.join(format!(".{index}.partial"));
        if partial.exists() {
            if verify_file(&partial, identity).is_ok() {
                rename_noreplace_durable(&partial, &payload)?;
                continue;
            }
            remove_path_durable(&partial)?;
        }
        let mut content = transport
            .open_content(&delta.delta_id, identity)
            .map_err(|error| GenerationSyncError::Transport(error.to_string()))?;
        write_verified_stream(&partial, content.as_mut(), identity)?;
        rename_noreplace_durable(&partial, &payload)?;
    }
    Ok(())
}

fn apply_entry(
    mount: &SecureMount,
    stage_root: &Path,
    delta_id: &str,
    index: u64,
    entry: &GenerationDeltaEntry,
    hooks: &mut ApplyHooks<'_>,
) -> Result<(GenerationApplyOutcome, bool), GenerationSyncError> {
    if let (Some(old), Some(new)) = (&entry.old, &entry.new)
        && old.logical_path != new.logical_path
    {
        let target = mount.target(&old.logical_path.to_relative_path_buf(), true)?;
        return conflict_outcome(&target, Some(new));
    }
    match (&entry.old, &entry.new) {
        (None, Some(new)) => {
            let target = mount.target(&new.logical_path.to_relative_path_buf(), true)?;
            call_hook(&mut hooks.after_target_open);
            match digest_target(&target)? {
                None => match publish_payload(&target, stage_root, delta_id, index, new)? {
                    PublishResult::Published => Ok((GenerationApplyOutcome::Applied, true)),
                    PublishResult::Occupied => conflict_outcome(&target, Some(new)),
                },
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
            let target = mount.target(&old.logical_path.to_relative_path_buf(), true)?;
            call_hook(&mut hooks.after_target_open);
            apply_fenced_update(&target, stage_root, delta_id, index, old, new, hooks, 0)
        }
        (Some(old), None) => {
            let target = mount.target(&old.logical_path.to_relative_path_buf(), true)?;
            call_hook(&mut hooks.after_target_open);
            apply_fenced_delete(&target, delta_id, index, old, hooks, 0)
        }
        (None, None) => Err(GenerationSyncError::Contract(
            "delta entry has no identity".to_string(),
        )),
    }
}

fn conflict_outcome(
    target: &SecureTarget,
    incoming: Option<&GenerationFileIdentity>,
) -> Result<(GenerationApplyOutcome, bool), GenerationSyncError> {
    Ok((
        GenerationApplyOutcome::Conflict {
            local_sha256: digest_target(target)?,
            incoming_identity: incoming.cloned(),
        },
        false,
    ))
}

fn apply_fenced_update(
    target: &SecureTarget,
    stage_root: &Path,
    delta_id: &str,
    index: u64,
    old: &GenerationFileIdentity,
    new: &GenerationFileIdentity,
    hooks: &mut ApplyHooks<'_>,
    attempt: u8,
) -> Result<(GenerationApplyOutcome, bool), GenerationSyncError> {
    if attempt >= 8 {
        return Err(GenerationSyncError::ConcurrentMutation);
    }
    let preimage = preimage_name(delta_id, index);
    if let Some(preimage_digest) = digest_named(target, &preimage)? {
        let current = digest_target(target)?;
        if preimage_digest != old.content_sha256 {
            if current.is_none() {
                target.restore_named(&preimage)?;
                return Ok((
                    GenerationApplyOutcome::Conflict {
                        local_sha256: Some(preimage_digest),
                        incoming_identity: Some(new.clone()),
                    },
                    true,
                ));
            }
            return Err(GenerationSyncError::ConcurrentMutation);
        }
        match current {
            Some(actual) if actual == new.content_sha256 => {
                target.remove_named(&preimage)?;
                return Ok((GenerationApplyOutcome::Applied, true));
            }
            Some(actual) => {
                target.remove_named(&preimage)?;
                return Ok((
                    GenerationApplyOutcome::Conflict {
                        local_sha256: Some(actual),
                        incoming_identity: Some(new.clone()),
                    },
                    true,
                ));
            }
            None => match publish_payload(target, stage_root, delta_id, index, new)? {
                PublishResult::Published => {
                    target.remove_named(&preimage)?;
                    return Ok((GenerationApplyOutcome::Applied, true));
                }
                PublishResult::Occupied => {
                    return apply_fenced_update(
                        target,
                        stage_root,
                        delta_id,
                        index,
                        old,
                        new,
                        hooks,
                        attempt + 1,
                    );
                }
            },
        }
    }

    match digest_target(target)? {
        Some(actual) if actual == new.content_sha256 => {
            Ok((GenerationApplyOutcome::Applied, false))
        }
        Some(actual) if actual == old.content_sha256 => match target.move_current_to(&preimage) {
            Ok(()) => {
                call_hook(&mut hooks.after_preimage_move);
                apply_fenced_update(
                    target,
                    stage_root,
                    delta_id,
                    index,
                    old,
                    new,
                    hooks,
                    attempt + 1,
                )
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => apply_fenced_update(
                target,
                stage_root,
                delta_id,
                index,
                old,
                new,
                hooks,
                attempt + 1,
            ),
            Err(error) => Err(error.into()),
        },
        actual => Ok((
            GenerationApplyOutcome::Conflict {
                local_sha256: actual,
                incoming_identity: Some(new.clone()),
            },
            false,
        )),
    }
}

fn apply_fenced_delete(
    target: &SecureTarget,
    delta_id: &str,
    index: u64,
    old: &GenerationFileIdentity,
    hooks: &mut ApplyHooks<'_>,
    attempt: u8,
) -> Result<(GenerationApplyOutcome, bool), GenerationSyncError> {
    if attempt >= 8 {
        return Err(GenerationSyncError::ConcurrentMutation);
    }
    let preimage = preimage_name(delta_id, index);
    if let Some(preimage_digest) = digest_named(target, &preimage)? {
        let current = digest_target(target)?;
        if preimage_digest != old.content_sha256 {
            if current.is_none() {
                target.restore_named(&preimage)?;
                return Ok((
                    GenerationApplyOutcome::Conflict {
                        local_sha256: Some(preimage_digest),
                        incoming_identity: None,
                    },
                    true,
                ));
            }
            return Err(GenerationSyncError::ConcurrentMutation);
        }
        target.remove_named(&preimage)?;
        return match current {
            None => Ok((GenerationApplyOutcome::Deleted, true)),
            Some(actual) => Ok((
                GenerationApplyOutcome::Conflict {
                    local_sha256: Some(actual),
                    incoming_identity: None,
                },
                true,
            )),
        };
    }

    match digest_target(target)? {
        None => Ok((GenerationApplyOutcome::Deleted, false)),
        Some(actual) if actual == old.content_sha256 => match target.move_current_to(&preimage) {
            Ok(()) => {
                call_hook(&mut hooks.after_preimage_move);
                apply_fenced_delete(target, delta_id, index, old, hooks, attempt + 1)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                apply_fenced_delete(target, delta_id, index, old, hooks, attempt + 1)
            }
            Err(error) => Err(error.into()),
        },
        Some(actual) => Ok((
            GenerationApplyOutcome::Conflict {
                local_sha256: Some(actual),
                incoming_identity: None,
            },
            false,
        )),
    }
}

enum PublishResult {
    Published,
    Occupied,
}

fn publish_payload(
    target: &SecureTarget,
    stage_root: &Path,
    delta_id: &str,
    index: u64,
    identity: &GenerationFileIdentity,
) -> Result<PublishResult, GenerationSyncError> {
    let payload = stage_root.join("payloads").join(index.to_string());
    verify_file(&payload, identity)?;
    let temporary = OsString::from(format!(
        ".locality-generation-{}-{index}.tmp",
        short_safe_id(delta_id)
    ));
    if let Some(file) = target.open_named(&temporary)?
        && verify_open_file(file, identity).is_err()
    {
        target.remove_named(&temporary)?;
    }
    if target.open_named(&temporary)?.is_none() {
        let mut source = File::open(&payload)?;
        let file = target.create_named(&temporary)?;
        if let Err(error) = write_verified_open_file(file, &mut source, identity) {
            let _ = target.remove_named(&temporary);
            return Err(error);
        }
    }
    match target.publish_named(&temporary) {
        Ok(()) => Ok(PublishResult::Published),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = target.remove_named(&temporary);
            Ok(PublishResult::Occupied)
        }
        Err(error) => Err(error.into()),
    }
}

fn call_hook(hook: &mut Option<&mut dyn FnMut()>) {
    if let Some(hook) = hook.take() {
        hook();
    }
}

fn preimage_name(delta_id: &str, index: u64) -> OsString {
    OsString::from(format!(
        ".locality-generation-{}-{index}.preimage",
        short_safe_id(delta_id)
    ))
}

fn digest_target(target: &SecureTarget) -> Result<Option<String>, GenerationSyncError> {
    target.open_current()?.map(digest_open_file).transpose()
}

fn digest_named(
    target: &SecureTarget,
    name: &OsStr,
) -> Result<Option<String>, GenerationSyncError> {
    target.open_named(name)?.map(digest_open_file).transpose()
}

fn digest_open_file(mut file: File) -> Result<String, GenerationSyncError> {
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn verify_file(path: &Path, identity: &GenerationFileIdentity) -> Result<(), GenerationSyncError> {
    verify_open_file(File::open(path)?, identity)
}

fn verify_open_file(
    file: File,
    identity: &GenerationFileIdentity,
) -> Result<(), GenerationSyncError> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != identity.byte_length {
        return Err(GenerationSyncError::ContentMismatch(
            identity.content_version_id.as_str().to_string(),
        ));
    }
    let actual = digest_open_file(file)?;
    if actual == identity.content_sha256 {
        Ok(())
    } else {
        Err(GenerationSyncError::ContentMismatch(
            identity.content_version_id.as_str().to_string(),
        ))
    }
}

fn write_verified_stream(
    path: &Path,
    reader: &mut dyn Read,
    identity: &GenerationFileIdentity,
) -> Result<(), GenerationSyncError> {
    write_verified_stream_with_sync(path, reader, identity, |file| file.sync_all())
}

fn write_verified_stream_with_sync(
    path: &Path,
    reader: &mut dyn Read,
    identity: &GenerationFileIdentity,
    sync: impl FnOnce(&File) -> std::io::Result<()>,
) -> Result<(), GenerationSyncError> {
    let file = OpenOptions::new().create_new(true).write(true).open(path)?;
    let result = write_verified_open_file_with_sync(file, reader, identity, sync);
    if result.is_err() {
        let _ = remove_path_durable(path);
    }
    result
}

fn write_verified_open_file(
    file: File,
    reader: &mut dyn Read,
    identity: &GenerationFileIdentity,
) -> Result<(), GenerationSyncError> {
    write_verified_open_file_with_sync(file, reader, identity, |file| file.sync_all())
}

fn write_verified_open_file_with_sync(
    mut file: File,
    reader: &mut dyn Read,
    identity: &GenerationFileIdentity,
    sync: impl FnOnce(&File) -> std::io::Result<()>,
) -> Result<(), GenerationSyncError> {
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| content_mismatch(identity))?;
        if total > identity.byte_length {
            return Err(content_mismatch(identity));
        }
        digest.update(&buffer[..read]);
        file.write_all(&buffer[..read])?;
    }
    if total != identity.byte_length
        || format!("sha256:{:x}", digest.finalize()) != identity.content_sha256
    {
        return Err(content_mismatch(identity));
    }
    sync(&file)?;
    Ok(())
}

fn content_mismatch(identity: &GenerationFileIdentity) -> GenerationSyncError {
    GenerationSyncError::ContentMismatch(identity.content_version_id.as_str().to_string())
}

#[cfg(test)]
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
    MountAccess(std::io::Error),
    MountBusy,
    MountCoordinatorPoisoned,
    Transport(String),
    Contract(String),
    MissingObservedGeneration(MountId),
    UnexpectedMount,
    InvalidStagePath,
    ContentMismatch(String),
    LocalBaseMismatch,
    JournalMismatch,
    ConcurrentMutation,
    InjectedInterruption,
}

impl Display for GenerationSyncError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => Display::fmt(error, formatter),
            Self::Io(error) => Display::fmt(error, formatter),
            Self::MountAccess(error) => {
                write!(formatter, "generation mount is busy or unsafe: {error}")
            }
            Self::MountBusy => formatter.write_str("generation mount already has an active apply"),
            Self::MountCoordinatorPoisoned => {
                formatter.write_str("generation mount coordinator is unavailable")
            }
            Self::Transport(error) => write!(formatter, "generation transport failed: {error}"),
            Self::Contract(error) => write!(formatter, "generation contract failed: {error}"),
            Self::MissingObservedGeneration(mount_id) => write!(
                formatter,
                "mount `{}` has no observed backend generation",
                mount_id.0
            ),
            Self::UnexpectedMount => formatter.write_str("generation delta names another mount"),
            Self::InvalidStagePath => formatter.write_str("generation stage path is not UTF-8"),
            Self::ContentMismatch(id) => {
                write!(
                    formatter,
                    "generation content `{id}` failed digest or length validation"
                )
            }
            Self::LocalBaseMismatch => formatter
                .write_str("local observed generation, layout, or path base does not match delta"),
            Self::JournalMismatch => formatter.write_str("generation journal replay mismatch"),
            Self::ConcurrentMutation => formatter.write_str(
                "local file changed concurrently while generation apply held its preimage",
            ),
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
    use std::io::{Seek, SeekFrom};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
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
        content_readers: VecDeque<Box<dyn Read + Send>>,
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

        fn open_content(
            &mut self,
            _delta_id: &str,
            identity: &GenerationFileIdentity,
        ) -> Result<Box<dyn Read + Send>, Self::Error> {
            self.content_fetches += 1;
            if let Some(reader) = self.content_readers.pop_front() {
                return Ok(reader);
            }
            self.contents
                .get(identity.content_version_id.as_str())
                .cloned()
                .map(|content| Box::new(std::io::Cursor::new(content)) as Box<dyn Read + Send>)
                .ok_or_else(|| FakeTransportError("missing fake content".to_string()))
        }
    }

    struct FailAfterFirstChunk {
        bytes: std::io::Cursor<Vec<u8>>,
        emitted: bool,
    }

    impl Read for FailAfterFirstChunk {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.emitted {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "injected stream failure",
                ));
            }
            self.emitted = true;
            let chunk = buffer.len().min(2);
            self.bytes.read(&mut buffer[..chunk])
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
            mount_id: PortableMountId::new("mount-main").unwrap(),
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
            mount_id: delta.mount_id.clone(),
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
            &mut ApplyHooks::default(),
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
            &mut ApplyHooks::default(),
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
                    old: Some(delete_old),
                    new: None,
                },
                GenerationDeltaEntry {
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
    fn fake_transport_receives_observed_head_and_bad_content_is_journaled_before_download() {
        let fixture = Fixture::new("transport-contract");
        let old = identity("projection-a", "Roadmap.md", "content-old", b"old");
        let new = identity("projection-a", "Roadmap.md", "content-new", b"good");
        fs::write(fixture.mount_root.join("Roadmap.md"), b"old").unwrap();
        let mut store = seed(&fixture, vec![old.clone()]);
        let delivery = delivery(
            "delta-bad-content",
            vec![GenerationDeltaEntry {
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
        let active = store.list_active_generation_applies().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].delta.delta_id, "delta-bad-content");
        assert!(active[0].outcomes.is_empty());
        assert_eq!(
            fs::read(fixture.mount_root.join("Roadmap.md")).unwrap(),
            b"old"
        );
    }

    #[test]
    fn empty_delta_advances_the_mount_generation_without_fetching_content() {
        let fixture = Fixture::new("empty-delta");
        let mut store = seed(&fixture, Vec::new());
        let delivery = delivery("delta-empty", Vec::new());
        let mut transport = FakeTransport::default();

        let summary = apply_authorized_delivery(
            &mut store,
            &fixture.mount_id,
            &fixture.mount_root,
            delivery,
            &mut transport,
        )
        .unwrap();

        assert_eq!(summary.applied_paths, 0);
        assert_eq!(transport.content_fetches, 0);
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
    fn failed_partial_download_is_removed_and_retry_streams_successfully() {
        let fixture = Fixture::new("partial-download");
        let old = identity("projection-a", "Roadmap.md", "content-old", b"old");
        let new = identity("projection-a", "Roadmap.md", "content-new", b"remote");
        fs::write(fixture.mount_root.join("Roadmap.md"), b"old").unwrap();
        let mut store = seed(&fixture, vec![old.clone()]);
        let delivery = delivery(
            "delta-partial",
            vec![GenerationDeltaEntry {
                old: Some(old),
                new: Some(new),
            }],
        );
        let mut failing = FakeTransport::default();
        failing
            .content_readers
            .push_back(Box::new(FailAfterFirstChunk {
                bytes: std::io::Cursor::new(b"remote".to_vec()),
                emitted: false,
            }));

        let error = apply_authorized_delivery(
            &mut store,
            &fixture.mount_id,
            &fixture.mount_root,
            delivery.clone(),
            &mut failing,
        )
        .expect_err("stream fails after a partial write");
        assert!(matches!(error, GenerationSyncError::Io(_)));
        let journal = store
            .get_generation_apply("delta-partial")
            .unwrap()
            .unwrap();
        let payload_root = store.root.join(&journal.stage_root).join("payloads");
        assert!(!payload_root.join(".0.partial").exists());
        assert!(!payload_root.join("0").exists());
        assert_eq!(
            fs::read(fixture.mount_root.join("Roadmap.md")).unwrap(),
            b"old"
        );
        fs::write(payload_root.join(".0.partial"), b"poison").unwrap();
        fs::write(
            fixture.mount_root.join(format!(
                ".locality-generation-{}-0.tmp",
                short_safe_id("delta-partial")
            )),
            b"poison",
        )
        .unwrap();

        let mut retry = FakeTransport::default();
        retry
            .contents
            .insert("content-new".to_string(), b"remote".to_vec());
        let summary = apply_authorized_delivery(
            &mut store,
            &fixture.mount_id,
            &fixture.mount_root,
            delivery,
            &mut retry,
        )
        .unwrap();
        assert_eq!(summary.applied_paths, 1);
        assert_eq!(
            fs::read(fixture.mount_root.join("Roadmap.md")).unwrap(),
            b"remote"
        );
    }

    #[test]
    fn failed_stage_fsync_does_not_poison_authenticated_retry() {
        let fixture = Fixture::new("partial-fsync");
        let identity = identity("projection-a", "Roadmap.md", "content-new", b"remote");
        let partial = fixture.state_root.join("payload.partial");
        fs::create_dir_all(&fixture.state_root).unwrap();
        let error = write_verified_stream_with_sync(
            &partial,
            &mut std::io::Cursor::new(b"remote"),
            &identity,
            |_| Err(std::io::Error::other("injected fsync failure")),
        )
        .expect_err("fsync failure");
        assert!(matches!(error, GenerationSyncError::Io(_)));
        assert!(!partial.exists());

        write_verified_stream(&partial, &mut std::io::Cursor::new(b"remote"), &identity).unwrap();
        verify_file(&partial, &identity).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_old_handle_write_is_restored_as_update_conflict() {
        let fixture = Fixture::new("concurrent-update");
        let old = identity("projection-a", "Roadmap.md", "content-old", b"old");
        let new = identity("projection-a", "Roadmap.md", "content-new", b"remote");
        let path = fixture.mount_root.join("Roadmap.md");
        fs::write(&path, b"old").unwrap();
        let writer = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut store = seed(&fixture, vec![old.clone()]);
        let delivery = delivery(
            "delta-concurrent-update",
            vec![GenerationDeltaEntry {
                old: Some(old),
                new: Some(new),
            }],
        );
        let mut transport = FakeTransport::default();
        transport
            .contents
            .insert("content-new".to_string(), b"remote".to_vec());
        let start = Arc::new(Barrier::new(2));
        let finished = Arc::new(Barrier::new(2));
        let writer_start = Arc::clone(&start);
        let writer_finished = Arc::clone(&finished);
        let writer_thread = thread::spawn(move || {
            let mut writer = writer;
            writer_start.wait();
            writer.seek(SeekFrom::Start(0)).unwrap();
            writer.set_len(0).unwrap();
            writer.write_all(b"local edit").unwrap();
            writer.sync_all().unwrap();
            writer_finished.wait();
        });
        let mut interleave = || {
            start.wait();
            finished.wait();
        };
        let mut hooks = ApplyHooks {
            after_preimage_move: Some(&mut interleave),
            ..ApplyHooks::default()
        };

        let summary = apply_authorized_delivery_inner(
            &mut store,
            &fixture.mount_id,
            &fixture.mount_root,
            delivery,
            &mut transport,
            None,
            &mut hooks,
        )
        .unwrap();
        writer_thread.join().unwrap();

        assert_eq!(summary.conflicted_paths, 1);
        assert_eq!(fs::read(path).unwrap(), b"local edit");
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_old_handle_write_is_restored_instead_of_deleted() {
        let fixture = Fixture::new("concurrent-delete");
        let old = identity("projection-a", "Delete.md", "content-old", b"old");
        let path = fixture.mount_root.join("Delete.md");
        fs::write(&path, b"old").unwrap();
        let writer = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut store = seed(&fixture, vec![old.clone()]);
        let delivery = delivery(
            "delta-concurrent-delete",
            vec![GenerationDeltaEntry {
                old: Some(old),
                new: None,
            }],
        );
        let start = Arc::new(Barrier::new(2));
        let finished = Arc::new(Barrier::new(2));
        let writer_start = Arc::clone(&start);
        let writer_finished = Arc::clone(&finished);
        let writer_thread = thread::spawn(move || {
            let mut writer = writer;
            writer_start.wait();
            writer.seek(SeekFrom::Start(0)).unwrap();
            writer.set_len(0).unwrap();
            writer.write_all(b"keep me").unwrap();
            writer.sync_all().unwrap();
            writer_finished.wait();
        });
        let mut interleave = || {
            start.wait();
            finished.wait();
        };
        let mut hooks = ApplyHooks {
            after_preimage_move: Some(&mut interleave),
            ..ApplyHooks::default()
        };

        let summary = apply_authorized_delivery_inner(
            &mut store,
            &fixture.mount_id,
            &fixture.mount_root,
            delivery,
            &mut FakeTransport::default(),
            None,
            &mut hooks,
        )
        .unwrap();
        writer_thread.join().unwrap();

        assert_eq!(summary.conflicted_paths, 1);
        assert_eq!(fs::read(path).unwrap(), b"keep me");
    }

    #[cfg(unix)]
    #[test]
    fn parent_replacement_race_cannot_write_outside_the_open_mount_tree() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("parent-race");
        let nested = fixture.mount_root.join("nested");
        let displaced = fixture.mount_root.join("displaced");
        let outside = fixture.root.join("outside");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(nested.join("Roadmap.md"), b"old").unwrap();
        fs::write(outside.join("Roadmap.md"), b"outside").unwrap();
        let old = identity("projection-a", "nested/Roadmap.md", "content-old", b"old");
        let new = identity(
            "projection-a",
            "nested/Roadmap.md",
            "content-new",
            b"remote",
        );
        let mut store = seed(&fixture, vec![old.clone()]);
        let delivery = delivery(
            "delta-parent-race",
            vec![GenerationDeltaEntry {
                old: Some(old),
                new: Some(new),
            }],
        );
        let mut transport = FakeTransport::default();
        transport
            .contents
            .insert("content-new".to_string(), b"remote".to_vec());
        let mut replace_parent = || {
            fs::rename(&nested, &displaced).unwrap();
            symlink(&outside, &nested).unwrap();
        };
        let mut hooks = ApplyHooks {
            after_target_open: Some(&mut replace_parent),
            ..ApplyHooks::default()
        };

        let summary = apply_authorized_delivery_inner(
            &mut store,
            &fixture.mount_id,
            &fixture.mount_root,
            delivery,
            &mut transport,
            None,
            &mut hooks,
        )
        .unwrap();

        assert_eq!(summary.applied_paths, 1);
        assert_eq!(fs::read(displaced.join("Roadmap.md")).unwrap(), b"remote");
        assert_eq!(fs::read(outside.join("Roadmap.md")).unwrap(), b"outside");
    }

    #[test]
    fn mount_lock_rejects_a_second_apply_coordinator() {
        let fixture = Fixture::new("mount-lock");
        let mut store = seed(&fixture, Vec::new());
        let _held = SecureMount::open(&fixture.mount_root).unwrap();

        let error = apply_authorized_delivery(
            &mut store,
            &fixture.mount_id,
            &fixture.mount_root,
            delivery("delta-locked", Vec::new()),
            &mut FakeTransport::default(),
        )
        .expect_err("second mount coordinator must fail closed");
        assert!(matches!(error, GenerationSyncError::MountAccess(_)));
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
