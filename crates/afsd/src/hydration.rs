use std::collections::{BTreeMap, VecDeque};
use std::path::{Component, Path, PathBuf};

use afs_core::canonical::parse_canonical_markdown;
use afs_core::conflict::{
    has_unresolved_conflict_markers, render_inline_conflict_markdown_with_base,
};
use afs_core::hydration::{HydrationReason, HydrationRequest};
use afs_core::model::{CanonicalDocument, HydrationState, MountId, RemoteId};
use afs_core::shadow::ShadowDocument;
use afs_core::{AfsError, AfsResult};
use afs_store::{
    EntityRecord, EntityRepository, FreshnessStateRepository, MountConfig, MountRepository,
    ShadowRepository, StoreError,
};

use crate::media::{
    document_with_absolute_media_hrefs, render_document_with_absolute_media_hrefs,
    update_hydrated_media_manifest,
};
use crate::shadow_match::{parsed_matches_shadow, shadows_match};

pub trait HydrationEngine {
    fn queue(&mut self, request: HydrationRequest) -> AfsResult<()>;
    fn drain_ready(&mut self) -> AfsResult<usize>;
}

pub trait HydrationSource {
    fn fetch_render(&self, request: &HydrationRequest) -> AfsResult<HydratedEntity>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HydratedEntity {
    pub document: CanonicalDocument,
    pub shadow: ShadowDocument,
    pub remote_edited_at: Option<String>,
    pub assets: Vec<HydratedAsset>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HydratedAsset {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub media: Option<HydratedAssetMedia>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HydratedAssetMedia {
    pub block_id: String,
    pub kind: String,
    pub source_url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HydrationDrainReport {
    pub hydrated: usize,
    pub skipped_dirty: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HydrationOutcome {
    Hydrated,
    SkippedDirty,
}

pub struct HydrationExecutor<'a, S, Source: ?Sized> {
    store: &'a mut S,
    source: &'a Source,
    output_root: Option<PathBuf>,
}

impl<'a, S, Source> HydrationExecutor<'a, S, Source>
where
    S: MountRepository + EntityRepository + ShadowRepository + FreshnessStateRepository,
    Source: HydrationSource + ?Sized,
{
    pub fn new(store: &'a mut S, source: &'a Source) -> Self {
        Self {
            store,
            source,
            output_root: None,
        }
    }

    pub fn new_with_output_root(
        store: &'a mut S,
        source: &'a Source,
        output_root: PathBuf,
    ) -> Self {
        Self {
            store,
            source,
            output_root: Some(output_root),
        }
    }

    pub fn hydrate_request(&mut self, request: HydrationRequest) -> AfsResult<HydrationOutcome> {
        if request.target_state != HydrationState::Hydrated {
            return Err(AfsError::Unsupported(
                "daemon hydration currently supports hydrated targets only",
            ));
        }

        let mount = require_mount(self.store, &request.mount_id)?;
        let entity = require_entity(self.store, &request.mount_id, &request.remote_id)?;
        let output_root = self
            .output_root
            .as_deref()
            .unwrap_or(&mount.root)
            .to_path_buf();
        let path = request_path(&mount, &request.path);
        let can_replace = self.can_replace_file(&mount, &entity, &path)?;
        if !can_replace && request.reason == HydrationReason::RemoteFastForward {
            self.mark_dirty_if_allowed(entity)?;
            return Ok(HydrationOutcome::SkippedDirty);
        }

        let mut render_request = request.clone();
        render_request.path = entity.path.clone();
        let rendered = self.source.fetch_render(&render_request)?;
        if rendered.shadow.entity_id != request.remote_id {
            return Err(AfsError::InvalidState(format!(
                "hydration source returned shadow for `{}` while hydrating `{}`",
                rendered.shadow.entity_id.0, request.remote_id.0
            )));
        }

        if !can_replace {
            if file_has_unresolved_conflict_markers(&path)? {
                self.mark_conflicted_if_allowed(entity)?;
            } else if !self.remote_matches_shadow(&mount, &entity, &rendered.shadow)? {
                self.materialize_conflict(&mount, &output_root, entity, &path, rendered)?;
            } else {
                self.mark_dirty_if_allowed(entity)?;
            }
            return Ok(HydrationOutcome::SkippedDirty);
        }

        for asset in &rendered.assets {
            let path = mount_relative_path(&output_root, &asset.path)?;
            write_binary_atomic(&path, &asset.bytes)?;
        }
        update_hydrated_media_manifest(&output_root, &rendered.assets)?;
        write_atomic(
            &path,
            render_document_with_absolute_media_hrefs(
                &rendered.document,
                &entity.path,
                &output_root,
            ),
        )?;
        self.store
            .save_shadow(&mount.mount_id, rendered.shadow.clone())
            .map_err(AfsError::from)?;
        self.store
            .save_entity(hydrated_record(
                entity,
                &rendered.shadow,
                rendered.remote_edited_at,
            ))
            .map_err(AfsError::from)?;
        self.clear_remote_hint(&request.mount_id, &request.remote_id)?;

        Ok(HydrationOutcome::Hydrated)
    }

    pub fn drain_queue(&mut self, queue: &mut HydrationQueue) -> AfsResult<HydrationDrainReport> {
        let mut report = HydrationDrainReport::default();

        while let Some(request) = queue.pop_ready() {
            match self.hydrate_request(request.clone()) {
                Ok(HydrationOutcome::Hydrated) => report.hydrated += 1,
                Ok(HydrationOutcome::SkippedDirty) => report.skipped_dirty += 1,
                Err(error) => {
                    queue.queue_request(request);
                    return Err(error);
                }
            }
        }

        Ok(report)
    }

    fn can_replace_file(
        &mut self,
        mount: &MountConfig,
        entity: &EntityRecord,
        path: &Path,
    ) -> AfsResult<bool> {
        if !path.exists() {
            return Ok(true);
        }

        if is_stub_file(path)? {
            return Ok(true);
        }

        let contents = read_to_string(path)?;
        let Ok(parsed) = parse_canonical_markdown(&contents) else {
            return Ok(false);
        };
        let shadow = match self.store.load_shadow(&mount.mount_id, &entity.remote_id) {
            Ok(shadow) => shadow,
            Err(StoreError::ShadowMissing { .. }) => return Ok(false),
            Err(error) => return Err(AfsError::from(error)),
        };

        Ok(parsed_matches_shadow(&parsed, &shadow))
    }

    fn mark_dirty_if_allowed(&mut self, mut entity: EntityRecord) -> AfsResult<()> {
        if entity.hydration != HydrationState::Conflicted
            && entity.hydration.can_transition_to(&HydrationState::Dirty)
        {
            entity.hydration = HydrationState::Dirty;
            self.store.save_entity(entity).map_err(AfsError::from)?;
        }

        Ok(())
    }

    fn mark_conflicted_if_allowed(&mut self, mut entity: EntityRecord) -> AfsResult<()> {
        if entity.hydration.can_transition_to(&HydrationState::Dirty) {
            entity.hydration = HydrationState::Dirty;
        }
        if entity
            .hydration
            .can_transition_to(&HydrationState::Conflicted)
        {
            entity.hydration = HydrationState::Conflicted;
            self.store.save_entity(entity).map_err(AfsError::from)?;
        }

        Ok(())
    }

    fn materialize_conflict(
        &mut self,
        mount: &MountConfig,
        output_root: &Path,
        mut entity: EntityRecord,
        path: &Path,
        rendered: HydratedEntity,
    ) -> AfsResult<()> {
        for asset in &rendered.assets {
            let path = mount_relative_path(output_root, &asset.path)?;
            write_binary_atomic(&path, &asset.bytes)?;
        }
        update_hydrated_media_manifest(output_root, &rendered.assets)?;
        let local_contents = read_to_string(path)?;
        let base_shadow = match self.store.load_shadow(&mount.mount_id, &entity.remote_id) {
            Ok(shadow) => Some(shadow),
            Err(StoreError::ShadowMissing { .. }) => None,
            Err(error) => return Err(AfsError::from(error)),
        };
        let remote_document =
            document_with_absolute_media_hrefs(&rendered.document, &entity.path, output_root);
        let conflict_markdown = render_inline_conflict_markdown_with_base(
            &local_contents,
            base_shadow
                .as_ref()
                .map(|shadow| shadow.rendered_body.as_str()),
            &remote_document,
        );
        write_atomic(path, conflict_markdown)?;
        self.store
            .save_shadow(&mount.mount_id, rendered.shadow.clone())
            .map_err(AfsError::from)?;

        if entity.hydration.can_transition_to(&HydrationState::Dirty) {
            entity.hydration = HydrationState::Dirty;
        }
        if entity
            .hydration
            .can_transition_to(&HydrationState::Conflicted)
        {
            entity.hydration = HydrationState::Conflicted;
        }
        entity.content_hash = Some(rendered.shadow.body_hash.clone());
        if rendered.remote_edited_at.is_some() {
            entity.remote_edited_at = rendered.remote_edited_at;
        }
        self.store.save_entity(entity).map_err(AfsError::from)?;

        Ok(())
    }

    fn remote_matches_shadow(
        &mut self,
        mount: &MountConfig,
        entity: &EntityRecord,
        rendered: &ShadowDocument,
    ) -> AfsResult<bool> {
        let shadow = match self.store.load_shadow(&mount.mount_id, &entity.remote_id) {
            Ok(shadow) => shadow,
            Err(StoreError::ShadowMissing { .. }) => return Ok(false),
            Err(error) => return Err(AfsError::from(error)),
        };

        Ok(shadows_match(&shadow, rendered))
    }

    fn clear_remote_hint(&mut self, mount_id: &MountId, remote_id: &RemoteId) -> AfsResult<()> {
        let Some(mut freshness) = self
            .store
            .get_freshness_state(mount_id, remote_id)
            .map_err(AfsError::from)?
        else {
            return Ok(());
        };
        freshness.remote_hint_pending = false;
        self.store
            .save_freshness_state(freshness)
            .map_err(AfsError::from)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HydrationQueue {
    order: VecDeque<HydrationKey>,
    pending: BTreeMap<HydrationKey, HydrationRequest>,
}

impl HydrationQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn queue_request(&mut self, request: HydrationRequest) -> bool {
        let key = HydrationKey::from_request(&request);
        let inserted = !self.pending.contains_key(&key);

        if inserted {
            self.order.push_back(key.clone());
            self.pending.insert(key, request);
            return true;
        }

        if let Some(existing) = self.pending.get_mut(&key) {
            merge_request(existing, request);
        }

        false
    }

    pub fn peek_ready(&self) -> Option<&HydrationRequest> {
        let key = self.next_ready_key()?;
        self.pending.get(key)
    }

    pub fn pop_ready(&mut self) -> Option<HydrationRequest> {
        let index = self.next_ready_index()?;
        let key = self.order.remove(index)?;
        self.pending.remove(&key)
    }

    pub fn drain_ready_with(
        &mut self,
        mut hydrate: impl FnMut(HydrationRequest) -> AfsResult<()>,
    ) -> AfsResult<usize> {
        let mut drained = 0;

        while let Some(request) = self.pop_ready() {
            if let Err(error) = hydrate(request.clone()) {
                self.queue_request(request);
                return Err(error);
            }

            drained += 1;
        }

        Ok(drained)
    }

    fn next_ready_key(&self) -> Option<&HydrationKey> {
        self.next_ready_index()
            .and_then(|index| self.order.get(index))
    }

    fn next_ready_index(&self) -> Option<usize> {
        let mut best: Option<(usize, HydrationPriority)> = None;

        for (index, key) in self.order.iter().enumerate() {
            let Some(request) = self.pending.get(key) else {
                continue;
            };
            let priority = hydration_priority(&request.reason);

            if best
                .as_ref()
                .is_none_or(|(_, best_priority)| priority > *best_priority)
            {
                best = Some((index, priority));
            }
        }

        best.map(|(index, _)| index)
    }
}

impl HydrationEngine for HydrationQueue {
    fn queue(&mut self, request: HydrationRequest) -> AfsResult<()> {
        self.queue_request(request);
        Ok(())
    }

    fn drain_ready(&mut self) -> AfsResult<usize> {
        let count = self.pending.len();
        self.pending.clear();
        self.order.clear();
        Ok(count)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum HydrationPriority {
    Low,
    Normal,
    High,
}

pub fn hydration_priority(reason: &HydrationReason) -> HydrationPriority {
    match reason {
        HydrationReason::ExplicitPull | HydrationReason::FileOpen | HydrationReason::StubRead => {
            HydrationPriority::High
        }
        HydrationReason::Policy | HydrationReason::RemoteFastForward => HydrationPriority::Normal,
        HydrationReason::Prefetch => HydrationPriority::Low,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct HydrationKey {
    mount_id: MountId,
    remote_id: RemoteId,
}

impl HydrationKey {
    fn from_request(request: &HydrationRequest) -> Self {
        Self {
            mount_id: request.mount_id.clone(),
            remote_id: request.remote_id.clone(),
        }
    }
}

fn merge_request(existing: &mut HydrationRequest, incoming: HydrationRequest) {
    let existing_priority = hydration_priority(&existing.reason);
    let incoming_priority = hydration_priority(&incoming.reason);
    let target_state = strongest_target_state(&existing.target_state, &incoming.target_state);

    if incoming_priority > existing_priority {
        existing.path = incoming.path;
        existing.reason = incoming.reason;
    }

    existing.target_state = target_state;
}

fn strongest_target_state(current: &HydrationState, incoming: &HydrationState) -> HydrationState {
    if hydration_target_rank(incoming) > hydration_target_rank(current) {
        incoming.clone()
    } else {
        current.clone()
    }
}

fn hydration_target_rank(state: &HydrationState) -> u8 {
    match state {
        HydrationState::Virtual => 0,
        HydrationState::Stub => 1,
        HydrationState::Hydrated => 2,
        HydrationState::Dirty => 3,
        HydrationState::Conflicted => 4,
    }
}

fn require_mount<S>(store: &S, mount_id: &MountId) -> AfsResult<MountConfig>
where
    S: MountRepository,
{
    store
        .get_mount(mount_id)
        .map_err(AfsError::from)?
        .ok_or_else(|| StoreError::MountMissing(mount_id.clone()).into())
}

fn require_entity<S>(store: &S, mount_id: &MountId, remote_id: &RemoteId) -> AfsResult<EntityRecord>
where
    S: EntityRepository,
{
    store
        .get_entity(mount_id, remote_id)
        .map_err(AfsError::from)?
        .ok_or_else(|| {
            StoreError::EntityMissing {
                mount_id: mount_id.clone(),
                remote_id: remote_id.clone(),
            }
            .into()
        })
}

fn hydrated_record(
    mut entity: EntityRecord,
    shadow: &ShadowDocument,
    remote_edited_at: Option<String>,
) -> EntityRecord {
    entity.hydration = HydrationState::Hydrated;
    entity.content_hash = Some(shadow.body_hash.clone());
    if remote_edited_at.is_some() {
        entity.remote_edited_at = remote_edited_at;
    }
    entity
}

fn request_path(mount: &MountConfig, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        mount.root.join(path)
    }
}

fn is_stub_file(path: &Path) -> AfsResult<bool> {
    if !path.exists() {
        return Ok(false);
    }

    let contents = read_to_string(path)?;
    Ok(contents.contains(CanonicalDocument::STUB_MARKER))
}

fn file_has_unresolved_conflict_markers(path: &Path) -> AfsResult<bool> {
    let contents = read_to_string(path)?;
    Ok(has_unresolved_conflict_markers(&contents))
}

fn write_atomic(path: &Path, contents: String) -> AfsResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            AfsError::Io(format!(
                "failed to create `{}` for hydration write: {error}",
                parent.display()
            ))
        })?;
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("afs-hydrate");
    let temp_path = path.with_file_name(format!(".{file_name}.afs-tmp"));

    std::fs::write(&temp_path, contents).map_err(|error| {
        AfsError::Io(format!(
            "failed to write hydration temp file `{}`: {error}",
            temp_path.display()
        ))
    })?;
    std::fs::rename(&temp_path, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp_path);
        AfsError::Io(format!(
            "failed to replace hydrated file `{}`: {error}",
            path.display()
        ))
    })?;

    Ok(())
}

fn write_binary_atomic(path: &Path, contents: &[u8]) -> AfsResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            AfsError::Io(format!(
                "failed to create `{}` for hydration asset write: {error}",
                parent.display()
            ))
        })?;
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("afs-hydrate-asset");
    let temp_path = path.with_file_name(format!(".{file_name}.afs-tmp"));

    std::fs::write(&temp_path, contents).map_err(|error| {
        AfsError::Io(format!(
            "failed to write hydration asset temp file `{}`: {error}",
            temp_path.display()
        ))
    })?;
    std::fs::rename(&temp_path, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp_path);
        AfsError::Io(format!(
            "failed to replace hydrated asset `{}`: {error}",
            path.display()
        ))
    })?;

    Ok(())
}

fn mount_relative_path(mount_root: &Path, path: &Path) -> AfsResult<PathBuf> {
    if path.components().any(|component| {
        matches!(
            component,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir
        )
    }) {
        return Err(AfsError::InvalidState(format!(
            "hydrated asset path `{}` is not mount-relative",
            path.display()
        )));
    }

    Ok(mount_root.join(path))
}

fn read_to_string(path: &Path) -> AfsResult<String> {
    std::fs::read_to_string(path)
        .map_err(|error| AfsError::Io(format!("failed to read `{}`: {error}", path.display())))
}
