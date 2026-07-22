use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use locality_core::canonical::parse_canonical_markdown;
use locality_core::conflict::{
    has_unresolved_conflict_markers, local_version_from_conflict_markers,
    render_inline_conflict_markdown_with_base,
};
use locality_core::hydration::{HydrationReason, HydrationRequest};
use locality_core::model::{CanonicalDocument, HydrationState, MountId, RemoteId};
use locality_core::path_projection::page_listing_parent_path;
use locality_core::shadow::ShadowDocument;
use locality_core::{LocalityError, LocalityResult};
use locality_store::{
    EntityRecord, EntityRepository, FreshnessStateRepository, MountConfig, MountRepository,
    RemoteObservationRecord, RemoteObservationRepository, ShadowRepository, StoreError,
};

use crate::freshness::{
    LIVE_MODE_POST_PUSH_SAME_VERSION_PROBE_WINDOW_MS, freshness_unix_ms, parse_freshness_timestamp,
};
use crate::media::{
    document_with_absolute_media_hrefs, render_document_with_absolute_media_hrefs,
    replace_hydrated_media_manifest, update_hydrated_media_manifest,
};
use crate::shadow_match::{
    contents_changes_retain_current_shadow_blocks, parsed_matches_shadow, shadows_match,
};

pub trait HydrationEngine {
    fn queue(&mut self, request: HydrationRequest) -> LocalityResult<()>;
    fn drain_ready(&mut self) -> LocalityResult<usize>;
}

pub trait HydrationSource {
    fn fetch_render(&self, request: &HydrationRequest) -> LocalityResult<HydratedEntity>;

    fn fetch_render_with_repository(
        &self,
        request: &HydrationRequest,
        _repository: &dyn HydrationRepository,
    ) -> LocalityResult<HydratedEntity> {
        self.fetch_render(request)
    }

    fn fetch_database_schema_yaml(
        &self,
        _database_id: &RemoteId,
    ) -> LocalityResult<Option<String>> {
        Ok(None)
    }
}

pub trait HydrationRepository {
    fn entity_record(
        &self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> LocalityResult<Option<EntityRecord>>;

    fn entity_record_by_path(
        &self,
        mount_id: &MountId,
        path: &Path,
    ) -> LocalityResult<Option<EntityRecord>>;

    fn entity_records(&self, mount_id: &MountId) -> LocalityResult<Vec<EntityRecord>>;
}

impl<T> HydrationRepository for T
where
    T: EntityRepository,
{
    fn entity_record(
        &self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> LocalityResult<Option<EntityRecord>> {
        EntityRepository::get_entity(self, mount_id, remote_id).map_err(LocalityError::from)
    }

    fn entity_record_by_path(
        &self,
        mount_id: &MountId,
        path: &Path,
    ) -> LocalityResult<Option<EntityRecord>> {
        EntityRepository::find_entity_by_path(self, mount_id, path).map_err(LocalityError::from)
    }

    fn entity_records(&self, mount_id: &MountId) -> LocalityResult<Vec<EntityRecord>> {
        EntityRepository::list_entities(self, mount_id).map_err(LocalityError::from)
    }
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
    pub remote_deleted: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HydrationOutcome {
    Hydrated,
    SkippedDirty,
    RemoteDeleted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirtyRemoteDriftOutcome {
    Merged,
    Conflicted,
}

pub struct HydrationExecutor<'a, S, Source: ?Sized> {
    store: &'a mut S,
    source: &'a Source,
    output_root: Option<PathBuf>,
}

impl<'a, S, Source> HydrationExecutor<'a, S, Source>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + FreshnessStateRepository
        + RemoteObservationRepository,
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

    pub fn hydrate_request(
        &mut self,
        request: HydrationRequest,
    ) -> LocalityResult<HydrationOutcome> {
        if request.target_state != HydrationState::Hydrated {
            return Err(LocalityError::Unsupported(
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
        if !can_replace && request.reason.is_remote_fast_forward() {
            self.mark_dirty_if_allowed(entity)?;
            return Ok(HydrationOutcome::SkippedDirty);
        }

        let mut render_request = request.clone();
        render_request.path = entity.path.clone();
        let rendered = match self
            .source
            .fetch_render_with_repository(&render_request, &*self.store)
        {
            Ok(rendered) => rendered,
            Err(error) if is_remote_not_found(&error) => {
                return self.reconcile_remote_not_found(&mount, entity, &path, can_replace);
            }
            Err(error) => return Err(error),
        };
        if rendered.shadow.entity_id != request.remote_id {
            return Err(LocalityError::InvalidState(format!(
                "hydration source returned shadow for `{}` while hydrating `{}`",
                rendered.shadow.entity_id.0, request.remote_id.0
            )));
        }
        let previous_shadow = match self.store.load_shadow(&mount.mount_id, &entity.remote_id) {
            Ok(shadow) => Some(shadow),
            Err(StoreError::ShadowMissing { .. }) => None,
            Err(error) => return Err(LocalityError::from(error)),
        };

        write_parent_database_schema_cache(self.store, self.source, &mount, &entity, &output_root)?;

        if !can_replace {
            if file_has_unresolved_conflict_markers(&path)? {
                if self.same_version_shadow_drifted(&mount, &entity, &rendered)? {
                    self.refresh_existing_conflict(
                        &mount,
                        &output_root,
                        entity,
                        &path,
                        rendered,
                        true,
                    )?;
                } else if same_remote_version(&entity, &rendered) {
                    self.refresh_existing_conflict(
                        &mount,
                        &output_root,
                        entity,
                        &path,
                        rendered,
                        false,
                    )?;
                } else {
                    self.mark_conflicted_if_allowed(entity)?;
                }
            } else if !self.remote_matches_shadow(&mount, &entity, &rendered.shadow)? {
                self.materialize_conflict(&mount, &output_root, entity, &path, rendered)?;
            } else {
                self.mark_dirty_if_allowed(entity)?;
            }
            return Ok(HydrationOutcome::SkippedDirty);
        }

        if request.reason == HydrationReason::LiveModeRemoteFastForward
            && live_mode_remote_fast_forward_render_looks_stale(
                &entity,
                previous_shadow.as_ref(),
                &rendered,
            )
        {
            self.keep_remote_hint_pending(&request.mount_id, &request.remote_id)?;
            return Ok(HydrationOutcome::Hydrated);
        }
        if request.reason == HydrationReason::LiveModeRemoteFastForward
            && self.same_version_live_mode_probe_should_continue(
                &request.mount_id,
                &request.remote_id,
                &entity,
                previous_shadow.as_ref(),
                &rendered,
            )?
        {
            self.keep_remote_hint_pending(&request.mount_id, &request.remote_id)?;
            return Ok(HydrationOutcome::Hydrated);
        }

        write_hydrated_asset_files(&output_root, &rendered.assets)?;
        replace_hydrated_media_manifest(&output_root, &rendered.assets)?;
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
            .map_err(LocalityError::from)?;
        self.store
            .save_entity(hydrated_record(
                entity,
                &rendered.shadow,
                rendered.remote_edited_at,
            ))
            .map_err(LocalityError::from)?;
        self.clear_remote_hint(&request.mount_id, &request.remote_id)?;

        Ok(HydrationOutcome::Hydrated)
    }

    pub fn drain_queue(
        &mut self,
        queue: &mut HydrationQueue,
    ) -> LocalityResult<HydrationDrainReport> {
        let mut report = HydrationDrainReport::default();

        while let Some(request) = queue.pop_ready() {
            match self.hydrate_request(request.clone()) {
                Ok(HydrationOutcome::Hydrated) => report.hydrated += 1,
                Ok(HydrationOutcome::SkippedDirty) => report.skipped_dirty += 1,
                Ok(HydrationOutcome::RemoteDeleted) => report.remote_deleted += 1,
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
    ) -> LocalityResult<bool> {
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
            Err(error) => return Err(LocalityError::from(error)),
        };

        Ok(parsed_matches_shadow(&parsed, &shadow))
    }

    fn mark_dirty_if_allowed(&mut self, mut entity: EntityRecord) -> LocalityResult<()> {
        if entity.hydration != HydrationState::Conflicted
            && entity.hydration.can_transition_to(&HydrationState::Dirty)
        {
            entity.hydration = HydrationState::Dirty;
            self.store
                .save_entity(entity)
                .map_err(LocalityError::from)?;
        }

        Ok(())
    }

    fn mark_conflicted_if_allowed(&mut self, mut entity: EntityRecord) -> LocalityResult<()> {
        if entity.hydration.can_transition_to(&HydrationState::Dirty) {
            entity.hydration = HydrationState::Dirty;
        }
        if entity
            .hydration
            .can_transition_to(&HydrationState::Conflicted)
        {
            entity.hydration = HydrationState::Conflicted;
            self.store
                .save_entity(entity)
                .map_err(LocalityError::from)?;
        }

        Ok(())
    }

    fn same_version_shadow_drifted(
        &mut self,
        mount: &MountConfig,
        entity: &EntityRecord,
        rendered: &HydratedEntity,
    ) -> LocalityResult<bool> {
        if !same_remote_version(entity, rendered) {
            return Ok(false);
        }

        Ok(!self.remote_matches_shadow(mount, entity, &rendered.shadow)?)
    }

    fn refresh_existing_conflict(
        &mut self,
        mount: &MountConfig,
        output_root: &Path,
        entity: EntityRecord,
        path: &Path,
        rendered: HydratedEntity,
        use_base_shadow: bool,
    ) -> LocalityResult<DirtyRemoteDriftOutcome> {
        let contents = read_to_string(path)?;
        let Some(local_contents) = local_version_from_conflict_markers(&contents) else {
            self.mark_conflicted_if_allowed(entity)?;
            return Ok(DirtyRemoteDriftOutcome::Conflicted);
        };
        self.materialize_conflict_from_contents(
            mount,
            output_root,
            entity,
            path,
            rendered,
            local_contents,
            use_base_shadow,
        )
    }

    fn materialize_conflict(
        &mut self,
        mount: &MountConfig,
        output_root: &Path,
        entity: EntityRecord,
        path: &Path,
        rendered: HydratedEntity,
    ) -> LocalityResult<DirtyRemoteDriftOutcome> {
        let local_contents = read_to_string(path)?;
        self.materialize_conflict_from_contents(
            mount,
            output_root,
            entity,
            path,
            rendered,
            local_contents,
            true,
        )
    }

    fn materialize_conflict_from_contents(
        &mut self,
        mount: &MountConfig,
        output_root: &Path,
        mut entity: EntityRecord,
        path: &Path,
        rendered: HydratedEntity,
        local_contents: String,
        use_base_shadow: bool,
    ) -> LocalityResult<DirtyRemoteDriftOutcome> {
        write_hydrated_asset_files(output_root, &rendered.assets)?;
        update_hydrated_media_manifest(output_root, &rendered.assets)?;
        let base_shadow = if use_base_shadow {
            match self.store.load_shadow(&mount.mount_id, &entity.remote_id) {
                Ok(shadow) => Some(shadow),
                Err(StoreError::ShadowMissing { .. }) => None,
                Err(error) => return Err(LocalityError::from(error)),
            }
        } else {
            None
        };
        let remote_document =
            document_with_absolute_media_hrefs(&rendered.document, &entity.path, output_root);
        let conflict_markdown = if !use_base_shadow
            && contents_changes_retain_current_shadow_blocks(&local_contents, &rendered.shadow)
        {
            local_contents
        } else {
            render_inline_conflict_markdown_with_base(
                &local_contents,
                base_shadow
                    .as_ref()
                    .map(|shadow| shadow.rendered_body.as_str()),
                &remote_document,
            )
        };
        let has_conflict_markers = has_unresolved_conflict_markers(&conflict_markdown);
        write_atomic(path, conflict_markdown)?;
        self.store
            .save_shadow(&mount.mount_id, rendered.shadow.clone())
            .map_err(LocalityError::from)?;

        if entity.hydration.can_transition_to(&HydrationState::Dirty) {
            entity.hydration = HydrationState::Dirty;
        }
        if has_conflict_markers
            && entity
                .hydration
                .can_transition_to(&HydrationState::Conflicted)
        {
            entity.hydration = HydrationState::Conflicted;
        }
        entity.content_hash = Some(rendered.shadow.body_hash.clone());
        if rendered.remote_edited_at.is_some() {
            entity.remote_edited_at = rendered.remote_edited_at;
        }
        self.store
            .save_entity(entity)
            .map_err(LocalityError::from)?;
        if !has_conflict_markers {
            self.clear_remote_hint(&mount.mount_id, &rendered.shadow.entity_id)?;
        }

        Ok(if has_conflict_markers {
            DirtyRemoteDriftOutcome::Conflicted
        } else {
            DirtyRemoteDriftOutcome::Merged
        })
    }

    fn remote_matches_shadow(
        &mut self,
        mount: &MountConfig,
        entity: &EntityRecord,
        rendered: &ShadowDocument,
    ) -> LocalityResult<bool> {
        let shadow = match self.store.load_shadow(&mount.mount_id, &entity.remote_id) {
            Ok(shadow) => shadow,
            Err(StoreError::ShadowMissing { .. }) => return Ok(false),
            Err(error) => return Err(LocalityError::from(error)),
        };

        Ok(shadows_match(&shadow, rendered))
    }

    fn clear_remote_hint(
        &mut self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> LocalityResult<()> {
        let Some(mut freshness) = self
            .store
            .get_freshness_state(mount_id, remote_id)
            .map_err(LocalityError::from)?
        else {
            return Ok(());
        };
        freshness.remote_hint_pending = false;
        self.store
            .save_freshness_state(freshness)
            .map_err(LocalityError::from)
    }

    fn keep_remote_hint_pending(
        &mut self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> LocalityResult<()> {
        let Some(mut freshness) = self
            .store
            .get_freshness_state(mount_id, remote_id)
            .map_err(LocalityError::from)?
        else {
            return Ok(());
        };
        freshness.remote_hint_pending = true;
        self.store
            .save_freshness_state(freshness)
            .map_err(LocalityError::from)
    }

    fn same_version_live_mode_probe_should_continue(
        &mut self,
        mount_id: &MountId,
        remote_id: &RemoteId,
        entity: &EntityRecord,
        previous_shadow: Option<&ShadowDocument>,
        rendered: &HydratedEntity,
    ) -> LocalityResult<bool> {
        if !same_remote_version(entity, rendered)
            || !render_matches_previous_shadow(previous_shadow, rendered)
        {
            return Ok(false);
        }
        let Some(freshness) = self
            .store
            .get_freshness_state(mount_id, remote_id)
            .map_err(LocalityError::from)?
        else {
            return Ok(false);
        };
        let Some(last_local_change_at) = freshness.last_local_change_at.as_deref() else {
            return Ok(false);
        };
        let Some(last_local_change_ms) = parse_freshness_timestamp(last_local_change_at) else {
            return Ok(false);
        };
        let now_ms = freshness_unix_ms();
        Ok(last_local_change_ms <= now_ms
            && now_ms.saturating_sub(last_local_change_ms)
                <= LIVE_MODE_POST_PUSH_SAME_VERSION_PROBE_WINDOW_MS)
    }

    fn reconcile_remote_not_found(
        &mut self,
        mount: &MountConfig,
        entity: EntityRecord,
        path: &Path,
        can_replace: bool,
    ) -> LocalityResult<HydrationOutcome> {
        self.record_deleted_remote_observation(mount, &entity)?;
        if !can_replace {
            self.mark_dirty_if_allowed(entity)?;
            return Ok(HydrationOutcome::SkippedDirty);
        }

        remove_clean_projection(path)?;
        self.store
            .delete_entity(&mount.mount_id, &entity.remote_id)
            .map_err(LocalityError::from)?;
        Ok(HydrationOutcome::RemoteDeleted)
    }

    fn record_deleted_remote_observation(
        &mut self,
        mount: &MountConfig,
        entity: &EntityRecord,
    ) -> LocalityResult<()> {
        let observed_at = crate::freshness::freshness_timestamp();
        let observation = RemoteObservationRecord::new(
            mount.mount_id.clone(),
            entity.remote_id.clone(),
            entity.kind.clone(),
            entity.title.clone(),
            entity.path.clone(),
            observed_at.clone(),
        )
        .deleted(true);
        self.store
            .save_remote_observation(observation)
            .map_err(LocalityError::from)?;

        if let Some(mut freshness) = self
            .store
            .get_freshness_state(&mount.mount_id, &entity.remote_id)
            .map_err(LocalityError::from)?
        {
            freshness.remote_hint_pending = true;
            freshness.last_checked_at = Some(observed_at);
            self.store
                .save_freshness_state(freshness)
                .map_err(LocalityError::from)?;
        }

        Ok(())
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

    pub fn contains_target(&self, mount_id: &MountId, remote_id: &RemoteId) -> bool {
        self.pending
            .contains_key(&HydrationKey::new(mount_id.clone(), remote_id.clone()))
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

    pub fn debug_requests(&self, limit: usize) -> Vec<HydrationRequest> {
        let mut requests = self
            .order
            .iter()
            .enumerate()
            .filter_map(|(index, key)| {
                self.pending
                    .get(key)
                    .cloned()
                    .map(|request| (index, request))
            })
            .collect::<Vec<_>>();
        requests.sort_by(|(left_index, left), (right_index, right)| {
            hydration_priority(&right.reason)
                .cmp(&hydration_priority(&left.reason))
                .then_with(|| left_index.cmp(right_index))
        });
        requests
            .into_iter()
            .take(limit)
            .map(|(_, request)| request)
            .collect()
    }

    pub fn drain_ready_with(
        &mut self,
        mut hydrate: impl FnMut(HydrationRequest) -> LocalityResult<()>,
    ) -> LocalityResult<usize> {
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
    fn queue(&mut self, request: HydrationRequest) -> LocalityResult<()> {
        self.queue_request(request);
        Ok(())
    }

    fn drain_ready(&mut self) -> LocalityResult<usize> {
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
        HydrationReason::ExplicitPull
        | HydrationReason::FileOpen
        | HydrationReason::LiveModeRemoteFastForward
        | HydrationReason::StubRead => HydrationPriority::High,
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
    fn new(mount_id: MountId, remote_id: RemoteId) -> Self {
        Self {
            mount_id,
            remote_id,
        }
    }

    fn from_request(request: &HydrationRequest) -> Self {
        Self::new(request.mount_id.clone(), request.remote_id.clone())
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

pub(crate) fn write_hydrated_asset_files(
    output_root: &Path,
    assets: &[HydratedAsset],
) -> LocalityResult<()> {
    for asset in assets {
        let path = mount_relative_path(output_root, &asset.path)?;
        write_binary_atomic(&path, &asset.bytes)?;
    }
    prune_stale_attachment_assets(output_root, assets, ".loc/gmail/attachments", "Gmail")?;
    prune_stale_attachment_assets(output_root, assets, ".loc/linear/attachments", "Linear")
}

fn prune_stale_attachment_assets(
    output_root: &Path,
    assets: &[HydratedAsset],
    root: &str,
    label: &str,
) -> LocalityResult<()> {
    let mut keep_by_parent: BTreeMap<PathBuf, BTreeSet<OsString>> = BTreeMap::new();
    for asset in assets {
        let Some((parent, filename)) =
            attachment_asset_parent_and_filename(&asset.path, Path::new(root))
        else {
            continue;
        };
        keep_by_parent.entry(parent).or_default().insert(filename);
    }

    for (parent, keep_names) in keep_by_parent {
        let absolute_parent = mount_relative_path(output_root, &parent)?;
        let entries = match std::fs::read_dir(&absolute_parent) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(LocalityError::Io(format!(
                    "failed to read {label} attachment cache directory `{}`: {error}",
                    absolute_parent.display()
                )));
            }
        };

        for entry in entries {
            let entry = entry.map_err(|error| {
                LocalityError::Io(format!(
                    "failed to read {label} attachment cache entry in `{}`: {error}",
                    absolute_parent.display()
                ))
            })?;
            let name = entry.file_name();
            if keep_names.contains(&name) || is_hydration_asset_temp_name(&name) {
                continue;
            }
            let file_type = entry.file_type().map_err(|error| {
                LocalityError::Io(format!(
                    "failed to inspect {label} attachment cache entry `{}`: {error}",
                    entry.path().display()
                ))
            })?;
            if file_type.is_file() {
                std::fs::remove_file(entry.path()).map_err(|error| {
                    LocalityError::Io(format!(
                        "failed to remove stale {label} attachment cache file `{}`: {error}",
                        entry.path().display()
                    ))
                })?;
            }
        }
    }

    Ok(())
}

fn attachment_asset_parent_and_filename(path: &Path, root: &Path) -> Option<(PathBuf, OsString)> {
    if !path.starts_with(root) {
        return None;
    }
    let parent = path.parent()?.to_path_buf();
    if parent == root {
        return None;
    }
    let filename = path.file_name()?.to_os_string();
    Some((parent, filename))
}

fn is_hydration_asset_temp_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.starts_with('.') && name.ends_with(".loc-tmp"))
}

fn require_mount<S>(store: &S, mount_id: &MountId) -> LocalityResult<MountConfig>
where
    S: MountRepository,
{
    store
        .get_mount(mount_id)
        .map_err(LocalityError::from)?
        .ok_or_else(|| StoreError::MountMissing(mount_id.clone()).into())
}

fn require_entity<S>(
    store: &S,
    mount_id: &MountId,
    remote_id: &RemoteId,
) -> LocalityResult<EntityRecord>
where
    S: EntityRepository,
{
    store
        .get_entity(mount_id, remote_id)
        .map_err(LocalityError::from)?
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

fn remove_clean_projection(path: &Path) -> LocalityResult<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(LocalityError::Io(format!(
                "failed to remove deleted remote projection `{}`: {error}",
                path.display()
            )));
        }
    }

    if path.file_name().is_some_and(|name| name == "page.md")
        && let Some(directory) = path.parent()
    {
        let _ = std::fs::remove_dir(directory);
    }
    Ok(())
}

fn is_remote_not_found(error: &LocalityError) -> bool {
    match error {
        LocalityError::RemoteNotFound(_) => true,
        LocalityError::Io(message) => {
            message.contains("HTTP 404") && message.contains("object_not_found")
        }
        _ => false,
    }
}

fn is_stub_file(path: &Path) -> LocalityResult<bool> {
    if !path.exists() {
        return Ok(false);
    }

    let contents = read_to_string(path)?;
    Ok(contents.contains(CanonicalDocument::STUB_MARKER))
}

pub fn write_parent_database_schema_cache<S, Source>(
    store: &S,
    source: &Source,
    mount: &MountConfig,
    entity: &EntityRecord,
    output_root: &Path,
) -> LocalityResult<bool>
where
    S: EntityRepository,
    Source: HydrationSource + ?Sized,
{
    let Some(database) = parent_database_entity(store, &mount.mount_id, entity)? else {
        return Ok(false);
    };
    let Some(schema) = source.fetch_database_schema_yaml(&database.remote_id)? else {
        return Ok(false);
    };
    write_atomic(
        &output_root.join(&database.path).join("_schema.yaml"),
        schema,
    )?;
    Ok(true)
}

fn parent_database_entity<S>(
    store: &S,
    mount_id: &MountId,
    entity: &EntityRecord,
) -> LocalityResult<Option<EntityRecord>>
where
    S: EntityRepository,
{
    if entity.kind != locality_core::model::EntityKind::Page {
        return Ok(None);
    }
    let parent_path = page_listing_parent_path(&entity.path);
    if parent_path.as_os_str().is_empty() {
        return Ok(None);
    }
    Ok(store
        .find_entity_by_path(mount_id, &parent_path)
        .map_err(LocalityError::from)?
        .filter(|entity| entity.kind == locality_core::model::EntityKind::Database))
}

fn file_has_unresolved_conflict_markers(path: &Path) -> LocalityResult<bool> {
    let contents = read_to_string(path)?;
    Ok(has_unresolved_conflict_markers(&contents))
}

fn same_remote_version(entity: &EntityRecord, rendered: &HydratedEntity) -> bool {
    rendered.remote_edited_at.is_some()
        && rendered.remote_edited_at.as_deref() == entity.remote_edited_at.as_deref()
}

fn live_mode_remote_fast_forward_render_looks_stale(
    entity: &EntityRecord,
    previous_shadow: Option<&ShadowDocument>,
    rendered: &HydratedEntity,
) -> bool {
    if rendered.remote_edited_at.as_deref() == entity.remote_edited_at.as_deref() {
        return false;
    }
    render_matches_previous_shadow(previous_shadow, rendered)
}

fn render_matches_previous_shadow(
    previous_shadow: Option<&ShadowDocument>,
    rendered: &HydratedEntity,
) -> bool {
    let Some(previous_shadow) = previous_shadow else {
        return false;
    };
    if rendered.shadow.rendered_body != previous_shadow.rendered_body
        || rendered.shadow.blocks != previous_shadow.blocks
    {
        return false;
    }

    frontmatter_without_sync_versions(&rendered.document.frontmatter)
        == frontmatter_without_sync_versions(&previous_shadow.frontmatter)
}

fn frontmatter_without_sync_versions(frontmatter: &str) -> String {
    frontmatter
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with("synced_at:") && !trimmed.starts_with("remote_edited_at:")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn write_atomic(path: &Path, contents: String) -> LocalityResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            LocalityError::Io(format!(
                "failed to create `{}` for hydration write: {error}",
                parent.display()
            ))
        })?;
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("loc-hydrate");
    let temp_path = path.with_file_name(format!(".{file_name}.loc-tmp"));

    std::fs::write(&temp_path, contents).map_err(|error| {
        LocalityError::Io(format!(
            "failed to write hydration temp file `{}`: {error}",
            temp_path.display()
        ))
    })?;
    std::fs::rename(&temp_path, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp_path);
        LocalityError::Io(format!(
            "failed to replace hydrated file `{}`: {error}",
            path.display()
        ))
    })?;

    Ok(())
}

fn write_binary_atomic(path: &Path, contents: &[u8]) -> LocalityResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            LocalityError::Io(format!(
                "failed to create `{}` for hydration asset write: {error}",
                parent.display()
            ))
        })?;
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("loc-hydrate-asset");
    let temp_path = path.with_file_name(format!(".{file_name}.loc-tmp"));

    std::fs::write(&temp_path, contents).map_err(|error| {
        LocalityError::Io(format!(
            "failed to write hydration asset temp file `{}`: {error}",
            temp_path.display()
        ))
    })?;
    std::fs::rename(&temp_path, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp_path);
        LocalityError::Io(format!(
            "failed to replace hydrated asset `{}`: {error}",
            path.display()
        ))
    })?;

    Ok(())
}

fn mount_relative_path(mount_root: &Path, path: &Path) -> LocalityResult<PathBuf> {
    if path.components().any(|component| {
        matches!(
            component,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir
        )
    }) {
        return Err(LocalityError::InvalidState(format!(
            "hydrated asset path `{}` is not mount-relative",
            path.display()
        )));
    }

    Ok(mount_root.join(path))
}

fn read_to_string(path: &Path) -> LocalityResult<String> {
    std::fs::read_to_string(path)
        .map_err(|error| LocalityError::Io(format!("failed to read `{}`: {error}", path.display())))
}
