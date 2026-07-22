//! Deterministic synchronize/project workflow.
//!
//! The workflow executes a synchronous portable connector and returns only
//! unpersisted immutable candidates. Hosts own clocks, durable IDs,
//! transactions, checkpoint commits, and ready-revision publication.

use std::collections::{BTreeMap, BTreeSet};

use locality_connector::{
    Connector, PortableArtifactKey, PortableBootstrapRequest, PortableChangeBatch,
    PortableCompleteness, PortableContentArtifact, PortableEnumerateRequest,
    PortableEnumerateResult, PortableFetchReason, PortableFetchRequest, PortableIncompleteReason,
    PortableProjectionArtifact, PortableRenderRequest, PortableSourceChange, PortableSyncRequest,
    portable_scope_root_remote_id,
};
use locality_core::model::RemoteId;
use locality_core::portable::{
    LogicalPath, ProjectionFileKind, SourceAction, SourceConnectionId, SourceObject,
};
use locality_core::{LocalityError, LocalityResult};
use sha2::{Digest, Sha256};

/// Phase-zero compatibility workflow retained for existing hosts.
pub trait SynchronizeAndProjectWorkflow {
    fn synchronize_and_project(
        &self,
        request: PortableEnumerateRequest,
    ) -> LocalityResult<PortableEnumerateResult>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImmutableSourceVersionCandidate {
    pub source_object: SourceObject,
    pub provider_version: Option<String>,
    pub native_sha256: String,
    pub canonical_sha256: String,
    pub native_body: Vec<u8>,
    pub canonical_artifact_key: PortableArtifactKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImmutableContentCandidate {
    pub artifact_key: PortableArtifactKey,
    pub sha256: String,
    pub byte_length: u64,
    pub media_type: String,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImmutableProjectionCandidate {
    pub artifact_key: PortableArtifactKey,
    pub logical_path: LogicalPath,
    pub content_artifact_key: PortableArtifactKey,
    pub content_sha256: String,
    pub source_remote_ids: Vec<RemoteId>,
    pub file_kind: ProjectionFileKind,
    pub format_version: u32,
    pub supported_actions: BTreeSet<SourceAction>,
}

/// One checkpoint-sized, unpersisted workflow result.
///
/// `publication_eligible` is private and derived exclusively from connector
/// completeness. Hosts may persist partial candidates under an unready
/// revision, but must not publish until this method returns true for the final
/// aggregate checkpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnpersistedSynchronizationBatch {
    pub source_connection_id: SourceConnectionId,
    pub observed_changes: Vec<PortableSourceChange>,
    pub source_versions: Vec<ImmutableSourceVersionCandidate>,
    pub contents: Vec<ImmutableContentCandidate>,
    pub projections: Vec<ImmutableProjectionCandidate>,
    pub next_checkpoint: locality_connector::PortableCheckpoint,
    pub completeness: PortableCompleteness,
    publication_eligible: bool,
}

impl UnpersistedSynchronizationBatch {
    pub fn is_publication_eligible(&self) -> bool {
        self.publication_eligible
    }

    pub fn require_complete(&self) -> LocalityResult<()> {
        if self.publication_eligible {
            Ok(())
        } else {
            Err(LocalityError::InvalidState(
                "portable synchronization batch is incomplete and cannot be published".to_string(),
            ))
        }
    }
}

/// Hard bounds for aggregating a paginated portable bootstrap.
///
/// These limits apply to the aggregate rather than to an individual connector
/// request. `PortableBootstrapRequest::max_changes` remains the per-checkpoint
/// provider-work bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootstrapAggregationLimits {
    pub max_checkpoints: usize,
    pub max_total_changes: usize,
    pub max_total_content_bytes: u64,
}

impl BootstrapAggregationLimits {
    fn validate(self) -> LocalityResult<Self> {
        if self.max_checkpoints == 0
            || self.max_total_changes == 0
            || self.max_total_content_bytes == 0
        {
            return Err(aggregation_error(
                "portable bootstrap aggregation limits must be nonzero",
            ));
        }
        Ok(self)
    }
}

/// Run one bootstrap checkpoint through fetch and render.
pub fn bootstrap_and_project<C: Connector + ?Sized>(
    connector: &C,
    request: PortableBootstrapRequest,
    format_version: u32,
) -> LocalityResult<UnpersistedSynchronizationBatch> {
    let source_connection_id = request.source_connection_id.clone();
    let batch = connector.bootstrap_portable(request)?;
    project_batch(
        connector,
        source_connection_id,
        batch,
        PortableFetchReason::Bootstrap,
        format_version,
    )
}

/// Run every checkpoint of one bounded bootstrap and return one deterministic
/// unpersisted candidate.
///
/// `CheckpointContinuation` is pagination control flow, not a coverage gap.
/// Every other incomplete reason is retained and therefore continues to block
/// publication after the terminal checkpoint is reached.
pub fn bootstrap_and_project_to_completion<C: Connector + ?Sized>(
    connector: &C,
    request: PortableBootstrapRequest,
    format_version: u32,
    limits: BootstrapAggregationLimits,
) -> LocalityResult<UnpersistedSynchronizationBatch> {
    let limits = limits.validate()?;
    let expected_source_connection_id = request.source_connection_id.clone();
    let scope = request.scope;
    let max_changes = request.max_changes;
    let mut current_checkpoint = request.checkpoint;
    let mut seen_checkpoints = BTreeSet::new();
    if let Some(checkpoint) = &current_checkpoint {
        seen_checkpoints.insert(checkpoint_identity(checkpoint));
    }
    let mut aggregate = BootstrapAggregate::new(expected_source_connection_id.clone());
    let mut checkpoint_count = 0_usize;

    loop {
        if checkpoint_count >= limits.max_checkpoints {
            return Err(aggregation_error(
                "portable bootstrap aggregation exceeded its checkpoint limit",
            ));
        }
        checkpoint_count += 1;
        let page = bootstrap_and_project(
            connector,
            PortableBootstrapRequest {
                source_connection_id: expected_source_connection_id.clone(),
                scope: scope.clone(),
                checkpoint: current_checkpoint.clone(),
                max_changes,
            },
            format_version,
        )
        .map_err(|_| aggregation_error("portable bootstrap aggregation page failed"))?;
        if page.source_connection_id != expected_source_connection_id {
            return Err(aggregation_error(
                "portable bootstrap aggregation changed source connection",
            ));
        }

        let (continuation, preserved_completeness) =
            completeness_without_continuation(&page.completeness);
        if continuation {
            validate_continuation_checkpoint(
                current_checkpoint.as_ref(),
                &page.next_checkpoint,
                &mut seen_checkpoints,
            )?;
        }
        let next_checkpoint = page.next_checkpoint.clone();
        aggregate.push(page, preserved_completeness, limits)?;

        if !continuation {
            return Ok(aggregate.finish(next_checkpoint));
        }
        current_checkpoint = Some(next_checkpoint);
    }
}

struct BootstrapAggregate {
    source_connection_id: SourceConnectionId,
    observed_changes: BTreeMap<RemoteId, PortableSourceChange>,
    source_versions: BTreeMap<RemoteId, ImmutableSourceVersionCandidate>,
    contents: BTreeMap<PortableArtifactKey, ImmutableContentCandidate>,
    projections: BTreeMap<PortableArtifactKey, ImmutableProjectionCandidate>,
    projection_paths: BTreeSet<String>,
    completeness: PortableCompleteness,
    total_changes: usize,
    total_content_bytes: u64,
}

impl BootstrapAggregate {
    fn new(source_connection_id: SourceConnectionId) -> Self {
        Self {
            source_connection_id,
            observed_changes: BTreeMap::new(),
            source_versions: BTreeMap::new(),
            contents: BTreeMap::new(),
            projections: BTreeMap::new(),
            projection_paths: BTreeSet::new(),
            completeness: PortableCompleteness::complete(),
            total_changes: 0,
            total_content_bytes: 0,
        }
    }

    fn push(
        &mut self,
        page: UnpersistedSynchronizationBatch,
        preserved_completeness: PortableCompleteness,
        limits: BootstrapAggregationLimits,
    ) -> LocalityResult<()> {
        for source in &page.source_versions {
            if self
                .source_versions
                .contains_key(&source.source_object.remote_id)
            {
                return Err(aggregation_error(
                    "portable bootstrap aggregation repeated a source version",
                ));
            }
        }
        for change in &page.observed_changes {
            if self
                .observed_changes
                .contains_key(&change.source_object.remote_id)
            {
                return Err(aggregation_error(
                    "portable bootstrap aggregation repeated an observed source",
                ));
            }
        }
        for projection in &page.projections {
            if self.projections.contains_key(&projection.artifact_key) {
                return Err(aggregation_error(
                    "portable bootstrap aggregation repeated a projection artifact",
                ));
            }
            if self
                .projection_paths
                .contains(projection.logical_path.as_str())
            {
                return Err(aggregation_error(
                    "portable bootstrap aggregation repeated a logical path",
                ));
            }
        }
        for content in &page.contents {
            if self.contents.contains_key(&content.artifact_key) {
                return Err(aggregation_error(
                    "portable bootstrap aggregation repeated a content artifact",
                ));
            }
        }

        let total_changes = self
            .total_changes
            .checked_add(page.observed_changes.len())
            .ok_or_else(|| {
                aggregation_error("portable bootstrap aggregation change count overflowed")
            })?;
        if total_changes > limits.max_total_changes {
            return Err(aggregation_error(
                "portable bootstrap aggregation exceeded its change limit",
            ));
        }
        let page_content_bytes = page.contents.iter().try_fold(0_u64, |total, content| {
            total.checked_add(content.byte_length).ok_or_else(|| {
                aggregation_error("portable bootstrap aggregation content bytes overflowed")
            })
        })?;
        let total_content_bytes = self
            .total_content_bytes
            .checked_add(page_content_bytes)
            .ok_or_else(|| {
                aggregation_error("portable bootstrap aggregation content bytes overflowed")
            })?;
        if total_content_bytes > limits.max_total_content_bytes {
            return Err(aggregation_error(
                "portable bootstrap aggregation exceeded its content byte limit",
            ));
        }

        self.total_changes = total_changes;
        self.total_content_bytes = total_content_bytes;
        self.completeness.merge(preserved_completeness);
        self.source_versions.extend(
            page.source_versions
                .into_iter()
                .map(|source| (source.source_object.remote_id.clone(), source)),
        );
        self.observed_changes.extend(
            page.observed_changes
                .into_iter()
                .map(|change| (change.source_object.remote_id.clone(), change)),
        );
        self.contents.extend(
            page.contents
                .into_iter()
                .map(|content| (content.artifact_key.clone(), content)),
        );
        for projection in page.projections {
            self.projection_paths
                .insert(projection.logical_path.as_str().to_string());
            self.projections
                .insert(projection.artifact_key.clone(), projection);
        }
        Ok(())
    }

    fn finish(
        self,
        next_checkpoint: locality_connector::PortableCheckpoint,
    ) -> UnpersistedSynchronizationBatch {
        let publication_eligible = self.completeness.is_complete();
        UnpersistedSynchronizationBatch {
            source_connection_id: self.source_connection_id,
            observed_changes: self.observed_changes.into_values().collect(),
            source_versions: self.source_versions.into_values().collect(),
            contents: self.contents.into_values().collect(),
            projections: self.projections.into_values().collect(),
            next_checkpoint,
            completeness: self.completeness,
            publication_eligible,
        }
    }
}

fn completeness_without_continuation(
    completeness: &PortableCompleteness,
) -> (bool, PortableCompleteness) {
    let reasons = completeness.incomplete_reasons();
    let continuation = reasons.contains(&PortableIncompleteReason::CheckpointContinuation);
    let mut preserved = if reasons.is_empty() && !completeness.is_complete() {
        PortableCompleteness::default()
    } else {
        PortableCompleteness::complete()
    };
    for reason in reasons {
        if reason != &PortableIncompleteReason::CheckpointContinuation {
            preserved.merge(PortableCompleteness::incomplete(reason.clone()));
        }
    }
    (continuation, preserved)
}

fn validate_continuation_checkpoint(
    current: Option<&locality_connector::PortableCheckpoint>,
    next: &locality_connector::PortableCheckpoint,
    seen: &mut BTreeSet<(u16, String)>,
) -> LocalityResult<()> {
    if next.opaque.is_empty() {
        return Err(aggregation_error(
            "portable bootstrap continuation returned an empty checkpoint",
        ));
    }
    if current == Some(next) {
        return Err(aggregation_error(
            "portable bootstrap continuation repeated its checkpoint",
        ));
    }
    if !seen.insert(checkpoint_identity(next)) {
        return Err(aggregation_error(
            "portable bootstrap continuation formed a checkpoint cycle",
        ));
    }
    Ok(())
}

fn checkpoint_identity(checkpoint: &locality_connector::PortableCheckpoint) -> (u16, String) {
    (checkpoint.format_version, checkpoint.opaque.clone())
}

fn aggregation_error(message: &'static str) -> LocalityError {
    LocalityError::InvalidState(message.to_string())
}

/// Run one incremental synchronization checkpoint through fetch and render.
pub fn synchronize_and_project_portable<C: Connector + ?Sized>(
    connector: &C,
    request: PortableSyncRequest,
    format_version: u32,
) -> LocalityResult<UnpersistedSynchronizationBatch> {
    let source_connection_id = request.source_connection_id.clone();
    let batch = connector.sync_portable(request)?;
    project_batch(
        connector,
        source_connection_id,
        batch,
        PortableFetchReason::Synchronization,
        format_version,
    )
}

fn project_batch<C: Connector + ?Sized>(
    connector: &C,
    source_connection_id: SourceConnectionId,
    mut batch: PortableChangeBatch,
    reason: PortableFetchReason,
    format_version: u32,
) -> LocalityResult<UnpersistedSynchronizationBatch> {
    batch.changes.sort_by(|left, right| {
        left.source_object
            .remote_id
            .cmp(&right.source_object.remote_id)
            .then_with(|| {
                left.logical_path
                    .as_ref()
                    .map(LogicalPath::as_str)
                    .cmp(&right.logical_path.as_ref().map(LogicalPath::as_str))
            })
    });
    validate_changes(&source_connection_id, &batch.changes)?;

    let mut completeness = batch.completeness;
    let mut source_versions = Vec::new();
    let mut contents = BTreeMap::<PortableArtifactKey, ImmutableContentCandidate>::new();
    let mut projections = BTreeMap::<PortableArtifactKey, ImmutableProjectionCandidate>::new();
    let mut canonical_artifact_keys = BTreeSet::new();
    let mut projection_paths = BTreeSet::new();

    for change in &batch.changes {
        if change.source_object.deleted || !change.requires_fetch {
            continue;
        }
        let logical_path = change.logical_path.clone().ok_or_else(|| {
            LocalityError::InvalidState(format!(
                "portable source `{}` requires fetch but has no logical path",
                change.source_object.remote_id.as_str()
            ))
        })?;
        let fetched = connector.fetch_portable(PortableFetchRequest {
            source_connection_id: source_connection_id.clone(),
            remote_id: change.source_object.remote_id.clone(),
            reason,
        })?;
        completeness.merge(fetched.completeness);
        if fetched.native.remote_id != change.source_object.remote_id {
            return Err(LocalityError::InvalidState(format!(
                "portable fetch for `{}` returned `{}`",
                change.source_object.remote_id.as_str(),
                fetched.native.remote_id.as_str()
            )));
        }

        let native_sha256 = sha256(&fetched.native.raw);
        let rendered = connector.render_portable(&PortableRenderRequest {
            source_connection_id: source_connection_id.clone(),
            logical_path,
            native: fetched.native.clone(),
            format_version,
        })?;
        completeness.merge(rendered.completeness);
        validate_artifact(&rendered.canonical)?;
        if rendered.projections.is_empty() {
            return Err(LocalityError::InvalidState(format!(
                "portable render for `{}` returned no projection artifacts",
                change.source_object.remote_id.as_str()
            )));
        }
        if !canonical_artifact_keys.insert(rendered.canonical.artifact_key.clone()) {
            return Err(LocalityError::InvalidState(
                "portable render returned a duplicate canonical artifact key".to_string(),
            ));
        }
        let canonical_sha256 = sha256(&rendered.canonical.body);
        insert_content(&mut contents, rendered.canonical.clone())?;

        let canonical_artifact_key = rendered.canonical.artifact_key.clone();
        for projection in rendered.projections {
            validate_projection(&change.source_object.remote_id, format_version, &projection)?;
            let content_sha256 = sha256(&projection.artifact.body);
            insert_content(&mut contents, projection.artifact.clone())?;
            let candidate = ImmutableProjectionCandidate {
                artifact_key: projection.artifact.artifact_key.clone(),
                logical_path: projection.logical_path,
                content_artifact_key: projection.artifact.artifact_key.clone(),
                content_sha256,
                source_remote_ids: vec![change.source_object.remote_id.clone()],
                file_kind: projection.file_kind,
                format_version: projection.format_version,
                supported_actions: projection.supported_actions,
            };
            if !projection_paths.insert(candidate.logical_path.as_str().to_string()) {
                return Err(LocalityError::InvalidState(format!(
                    "portable render returned duplicate logical path `{}`",
                    candidate.logical_path.as_str()
                )));
            }
            if projections
                .insert(candidate.artifact_key.clone(), candidate)
                .is_some()
            {
                return Err(LocalityError::InvalidState(
                    "portable render returned a duplicate projection artifact key".to_string(),
                ));
            }
        }

        source_versions.push(ImmutableSourceVersionCandidate {
            source_object: change.source_object.clone(),
            provider_version: fetched.provider_version,
            native_sha256,
            canonical_sha256,
            native_body: fetched.native.raw,
            canonical_artifact_key,
        });
    }

    source_versions.sort_by(|left, right| {
        left.source_object
            .remote_id
            .cmp(&right.source_object.remote_id)
    });
    let publication_eligible = completeness.is_complete();

    Ok(UnpersistedSynchronizationBatch {
        source_connection_id,
        observed_changes: batch.changes,
        source_versions,
        contents: contents.into_values().collect(),
        projections: projections.into_values().collect(),
        next_checkpoint: batch.next_checkpoint,
        completeness,
        publication_eligible,
    })
}

fn validate_changes(
    source_connection_id: &SourceConnectionId,
    changes: &[PortableSourceChange],
) -> LocalityResult<()> {
    let mut remote_ids = BTreeSet::new();
    let mut provenance_mode = None;
    for change in changes {
        if &change.source_object.source_connection_id != source_connection_id {
            return Err(LocalityError::InvalidState(
                "portable connector returned a source object for another connection".to_string(),
            ));
        }
        if !remote_ids.insert(change.source_object.remote_id.clone()) {
            return Err(LocalityError::InvalidState(format!(
                "portable connector returned duplicate source object `{}`",
                change.source_object.remote_id.as_str()
            )));
        }
        let has_root_provenance = portable_scope_root_remote_id(&change.source_object)?.is_some();
        if provenance_mode.is_some_and(|expected| expected != has_root_provenance) {
            return Err(LocalityError::InvalidState(
                "portable connector returned a batch with ambiguous owning-root provenance"
                    .to_string(),
            ));
        }
        provenance_mode.get_or_insert(has_root_provenance);
    }
    Ok(())
}

fn validate_artifact(artifact: &PortableContentArtifact) -> LocalityResult<()> {
    if !artifact.artifact_key.is_valid() || artifact.media_type.trim().is_empty() {
        return Err(LocalityError::InvalidState(
            "portable render returned invalid artifact metadata".to_string(),
        ));
    }
    Ok(())
}

fn validate_projection(
    source_remote_id: &RemoteId,
    requested_format_version: u32,
    projection: &PortableProjectionArtifact,
) -> LocalityResult<()> {
    validate_artifact(&projection.artifact)?;
    if projection.format_version == 0 || projection.format_version != requested_format_version {
        return Err(LocalityError::InvalidState(format!(
            "portable projection for `{}` returned format version {} when {} was requested",
            source_remote_id.as_str(),
            projection.format_version,
            requested_format_version
        )));
    }
    Ok(())
}

fn insert_content(
    contents: &mut BTreeMap<PortableArtifactKey, ImmutableContentCandidate>,
    artifact: PortableContentArtifact,
) -> LocalityResult<()> {
    validate_artifact(&artifact)?;
    let candidate = ImmutableContentCandidate {
        artifact_key: artifact.artifact_key.clone(),
        sha256: sha256(&artifact.body),
        byte_length: artifact.body.len() as u64,
        media_type: artifact.media_type,
        body: artifact.body,
    };
    if let Some(existing) = contents.get(&candidate.artifact_key) {
        if existing != &candidate {
            return Err(LocalityError::InvalidState(format!(
                "portable artifact key `{}` identified different immutable bytes",
                candidate.artifact_key.as_str()
            )));
        }
        return Ok(());
    }
    contents.insert(candidate.artifact_key.clone(), candidate);
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
