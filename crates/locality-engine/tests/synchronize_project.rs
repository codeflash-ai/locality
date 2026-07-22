use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, Connector,
    ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest, NativeEntity,
    PORTABLE_SCOPE_ROOT_RELATIONSHIP, ParsedEntity, PortableArtifactKey, PortableBootstrapRequest,
    PortableChangeBatch, PortableCheckpoint, PortableCompleteness, PortableContentArtifact,
    PortableFetchRequest, PortableFetchResult, PortableIncompleteReason,
    PortableProjectionArtifact, PortableRenderRequest, PortableRenderResult, PortableSourceChange,
    PortableSyncRequest,
};
use locality_core::LocalityResult;
use locality_core::model::{CanonicalDocument, EntityKind, RemoteId, TreeEntry};
use locality_core::portable::{
    LogicalPath, ProjectionFileKind, SourceAction, SourceConnectionId, SourceEdge, SourceObject,
};
use locality_engine::synchronize_project::{
    BootstrapAggregationLimits, bootstrap_and_project, bootstrap_and_project_to_completion,
    synchronize_and_project_portable,
};

#[derive(Clone)]
struct FixtureConnector {
    incomplete: bool,
    duplicate_projection_key: bool,
    duplicate_owning_root_edge: bool,
    omit_owning_root_edge: bool,
}

impl FixtureConnector {
    fn complete() -> Self {
        Self {
            incomplete: false,
            duplicate_projection_key: false,
            duplicate_owning_root_edge: false,
            omit_owning_root_edge: false,
        }
    }
}

impl Connector for FixtureConnector {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind("fixture")
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities::read_only()
    }

    fn supported_push_operations(&self) -> BTreeSet<locality_core::planner::PushOperationKind> {
        BTreeSet::new()
    }

    fn enumerate(&self, _request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        Ok(Vec::new())
    }

    fn bootstrap_portable(
        &self,
        request: PortableBootstrapRequest,
    ) -> LocalityResult<PortableChangeBatch> {
        let mut changes = vec![
            change(&request.source_connection_id, "page-b", "B/page.md"),
            change(&request.source_connection_id, "page-a", "A/page.md"),
        ];
        if self.duplicate_owning_root_edge {
            changes[0].source_object.edges.push(SourceEdge {
                relationship: PORTABLE_SCOPE_ROOT_RELATIONSHIP.to_string(),
                target_remote_id: RemoteId::new("other-root"),
            });
        }
        if self.omit_owning_root_edge {
            changes[1].source_object.edges.clear();
        }
        Ok(PortableChangeBatch {
            changes,
            next_checkpoint: PortableCheckpoint {
                format_version: 1,
                opaque: "ready".to_string(),
            },
            completeness: if self.incomplete {
                PortableCompleteness::incomplete(PortableIncompleteReason::ConnectorLimitation {
                    code: "fixture_gap".to_string(),
                    remote_id: None,
                })
            } else {
                PortableCompleteness::complete()
            },
        })
    }

    fn sync_portable(&self, request: PortableSyncRequest) -> LocalityResult<PortableChangeBatch> {
        self.bootstrap_portable(PortableBootstrapRequest {
            source_connection_id: request.source_connection_id,
            scope: request.scope,
            checkpoint: Some(request.checkpoint),
            max_changes: request.max_changes,
        })
    }

    fn fetch_portable(&self, request: PortableFetchRequest) -> LocalityResult<PortableFetchResult> {
        Ok(PortableFetchResult {
            native: NativeEntity {
                remote_id: request.remote_id.clone(),
                kind: "fixture_page".to_string(),
                raw: format!("native:{}", request.remote_id.as_str()).into_bytes(),
            },
            provider_version: Some(format!("v:1:{}", request.remote_id.as_str())),
            completeness: PortableCompleteness::complete(),
        })
    }

    fn render_portable(
        &self,
        request: &PortableRenderRequest,
    ) -> LocalityResult<PortableRenderResult> {
        let remote_id = request.native.remote_id.as_str();
        let canonical = PortableContentArtifact {
            artifact_key: PortableArtifactKey::new(format!("fixture:{remote_id}:canonical:v1")),
            media_type: "text/markdown".to_string(),
            body: format!("canonical:{remote_id}\n").into_bytes(),
        };
        let projection_key = if self.duplicate_projection_key {
            "fixture:shared:projection:v1".to_string()
        } else {
            format!("fixture:{remote_id}:projection:v1")
        };
        Ok(PortableRenderResult {
            canonical: canonical.clone(),
            projections: vec![PortableProjectionArtifact {
                artifact: PortableContentArtifact {
                    artifact_key: PortableArtifactKey::new(projection_key),
                    media_type: "text/markdown".to_string(),
                    body: canonical.body,
                },
                logical_path: request.logical_path.clone(),
                file_kind: ProjectionFileKind::Markdown,
                format_version: request.format_version,
                supported_actions: BTreeSet::from([SourceAction::Read]),
            }],
            completeness: PortableCompleteness::complete(),
        })
    }

    fn fetch(&self, _request: FetchRequest) -> LocalityResult<NativeEntity> {
        unreachable!("portable engine uses fetch_portable")
    }

    fn render(&self, _entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        unreachable!("portable engine uses render_portable")
    }

    fn parse(&self, _document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        unreachable!("not used")
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        unreachable!("not used")
    }

    fn apply(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        unreachable!("not used")
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        unreachable!("not used")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PagedFault {
    None,
    NonContinuationIncomplete,
    EmptyCheckpoint,
    RepeatedCheckpoint,
    CheckpointCycle,
    ChangedConnection,
    DuplicateSourceVersion,
    DuplicateObservedSource,
    DuplicateContentArtifact,
    DuplicateProjectionArtifact,
    DuplicateLogicalPath,
}

struct PagedFixtureConnector {
    fault: PagedFault,
    calls: Mutex<Vec<Option<String>>>,
}

impl PagedFixtureConnector {
    fn new(fault: PagedFault) -> Self {
        Self {
            fault,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<Option<String>> {
        self.calls.lock().expect("calls lock").clone()
    }
}

impl Connector for PagedFixtureConnector {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind("paged-fixture")
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities::read_only()
    }

    fn supported_push_operations(&self) -> BTreeSet<locality_core::planner::PushOperationKind> {
        BTreeSet::new()
    }

    fn enumerate(&self, _request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        Ok(Vec::new())
    }

    fn bootstrap_portable(
        &self,
        request: PortableBootstrapRequest,
    ) -> LocalityResult<PortableChangeBatch> {
        let checkpoint = request
            .checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.opaque.clone());
        self.calls
            .lock()
            .expect("calls lock")
            .push(checkpoint.clone());

        let offset: usize = match checkpoint.as_deref() {
            None => 0,
            Some("cp1") => 1,
            Some("cp2") => 2,
            Some(_) => {
                return Err(locality_core::LocalityError::InvalidState(
                    "fixture checkpoint is invalid".to_string(),
                ));
            }
        };
        let requested = usize::try_from(request.max_changes).expect("u32 fits usize");
        if requested == 0 {
            return Err(locality_core::LocalityError::InvalidState(
                "fixture page size is zero".to_string(),
            ));
        }
        let end = offset.saturating_add(requested).min(3);
        let mut changes = (offset..end)
            .map(|index| paged_change(&request.source_connection_id, index, self.fault))
            .collect::<Vec<_>>();
        if self.fault == PagedFault::DuplicateObservedSource && offset == 0 {
            changes[0].requires_fetch = false;
            changes[0].source_object.deleted = true;
        }

        let has_more = end < 3;
        let mut next_opaque = match end {
            0 => "cp0",
            1 => "cp1",
            2 => "cp2",
            _ => "done",
        }
        .to_string();
        let mut completeness = if has_more {
            PortableCompleteness::incomplete(PortableIncompleteReason::CheckpointContinuation)
        } else {
            PortableCompleteness::complete()
        };
        if self.fault == PagedFault::NonContinuationIncomplete && offset == 0 {
            completeness.merge(PortableCompleteness::incomplete(
                PortableIncompleteReason::ConnectorLimitation {
                    code: "fixture_gap".to_string(),
                    remote_id: Some(RemoteId::new("provider-value")),
                },
            ));
        }
        match self.fault {
            PagedFault::EmptyCheckpoint if offset == 0 => next_opaque.clear(),
            PagedFault::RepeatedCheckpoint if offset == 1 => {
                next_opaque = "cp1".to_string();
                completeness = PortableCompleteness::incomplete(
                    PortableIncompleteReason::CheckpointContinuation,
                );
            }
            PagedFault::CheckpointCycle if offset == 2 => {
                next_opaque = "cp1".to_string();
                completeness = PortableCompleteness::incomplete(
                    PortableIncompleteReason::CheckpointContinuation,
                );
            }
            _ => {}
        }

        Ok(PortableChangeBatch {
            changes,
            next_checkpoint: PortableCheckpoint {
                format_version: 1,
                opaque: next_opaque,
            },
            completeness,
        })
    }

    fn sync_portable(&self, request: PortableSyncRequest) -> LocalityResult<PortableChangeBatch> {
        self.bootstrap_portable(PortableBootstrapRequest {
            source_connection_id: request.source_connection_id,
            scope: request.scope,
            checkpoint: Some(request.checkpoint),
            max_changes: request.max_changes,
        })
    }

    fn fetch_portable(&self, request: PortableFetchRequest) -> LocalityResult<PortableFetchResult> {
        Ok(PortableFetchResult {
            native: NativeEntity {
                remote_id: request.remote_id.clone(),
                kind: "fixture_page".to_string(),
                raw: format!("native:{}", request.remote_id.as_str()).into_bytes(),
            },
            provider_version: Some(format!("v1:{}", request.remote_id.as_str())),
            completeness: PortableCompleteness::complete(),
        })
    }

    fn render_portable(
        &self,
        request: &PortableRenderRequest,
    ) -> LocalityResult<PortableRenderResult> {
        let remote_id = request.native.remote_id.as_str();
        let canonical_key = if self.fault == PagedFault::DuplicateContentArtifact {
            "paged:shared:canonical:v1".to_string()
        } else {
            format!("paged:{remote_id}:canonical:v1")
        };
        let projection_key = if self.fault == PagedFault::DuplicateProjectionArtifact {
            "paged:shared:projection:v1".to_string()
        } else {
            format!("paged:{remote_id}:projection:v1")
        };
        let canonical = PortableContentArtifact {
            artifact_key: PortableArtifactKey::new(canonical_key),
            media_type: "text/markdown".to_string(),
            body: format!("canonical:{remote_id}\n").into_bytes(),
        };
        Ok(PortableRenderResult {
            canonical: canonical.clone(),
            projections: vec![PortableProjectionArtifact {
                artifact: PortableContentArtifact {
                    artifact_key: PortableArtifactKey::new(projection_key),
                    media_type: "text/markdown".to_string(),
                    body: canonical.body,
                },
                logical_path: request.logical_path.clone(),
                file_kind: ProjectionFileKind::Markdown,
                format_version: request.format_version,
                supported_actions: BTreeSet::from([SourceAction::Read]),
            }],
            completeness: PortableCompleteness::complete(),
        })
    }

    fn fetch(&self, _request: FetchRequest) -> LocalityResult<NativeEntity> {
        unreachable!("portable engine uses fetch_portable")
    }

    fn render(&self, _entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        unreachable!("portable engine uses render_portable")
    }

    fn parse(&self, _document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        unreachable!("not used")
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        unreachable!("not used")
    }

    fn apply(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        unreachable!("not used")
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        unreachable!("not used")
    }
}

#[test]
fn deterministic_retry_returns_identical_unpersisted_candidates_and_hashes() {
    let connector = FixtureConnector::complete();
    let first = bootstrap_and_project(&connector, request(), 1).expect("first batch");
    let retry = bootstrap_and_project(&connector, request(), 1).expect("retry batch");

    assert_eq!(first, retry);
    assert!(first.is_publication_eligible());
    first.require_complete().expect("complete batch");
    assert_eq!(
        first
            .source_versions
            .iter()
            .map(|candidate| candidate.source_object.remote_id.as_str())
            .collect::<Vec<_>>(),
        vec!["page-a", "page-b"]
    );
    assert_eq!(
        first.source_versions[0].native_sha256,
        "sha256:884605d515a578dab164b91d10c384cf3a66163c0de4361a58bb61068e21b05a"
    );
    assert_eq!(
        first.source_versions[0].canonical_sha256,
        "sha256:5ebbb50be5c7e33f3fa9ea6c8ae904fa32f8827011e15bbf4947897bdc53e333"
    );
    assert!(first.observed_changes.iter().all(|change| {
        change.source_object.edges
            == vec![SourceEdge {
                relationship: PORTABLE_SCOPE_ROOT_RELATIONSHIP.to_string(),
                target_remote_id: RemoteId::new("root"),
            }]
    }));
}

#[test]
fn incomplete_connector_batch_cannot_be_published() {
    let connector = FixtureConnector {
        incomplete: true,
        duplicate_projection_key: false,
        duplicate_owning_root_edge: false,
        omit_owning_root_edge: false,
    };
    let batch = bootstrap_and_project(&connector, request(), 1).expect("incomplete batch");

    assert!(!batch.is_publication_eligible());
    assert!(batch.require_complete().is_err());
}

#[test]
fn synchronization_uses_the_same_deterministic_candidate_pipeline() {
    let connector = FixtureConnector::complete();
    let bootstrap = bootstrap_and_project(&connector, request(), 1).expect("bootstrap batch");
    let synchronized = synchronize_and_project_portable(
        &connector,
        PortableSyncRequest {
            source_connection_id: SourceConnectionId::new("source-fixture"),
            scope: locality_connector::PortableSourceScope::explicit_roots([RemoteId::new("root")]),
            checkpoint: PortableCheckpoint {
                format_version: 1,
                opaque: "ready".to_string(),
            },
            hints: Vec::new(),
            max_changes: 100,
        },
        1,
    )
    .expect("synchronization batch");

    assert_eq!(bootstrap.source_versions, synchronized.source_versions);
    assert_eq!(bootstrap.contents, synchronized.contents);
    assert_eq!(bootstrap.projections, synchronized.projections);
}

#[test]
fn conflicting_artifact_keys_fail_the_whole_batch() {
    let connector = FixtureConnector {
        incomplete: false,
        duplicate_projection_key: true,
        duplicate_owning_root_edge: false,
        omit_owning_root_edge: false,
    };
    let error = bootstrap_and_project(&connector, request(), 1)
        .expect_err("duplicate artifact identity must fail closed");

    assert!(
        error
            .to_string()
            .contains("identified different immutable bytes")
    );
}

#[test]
fn multiple_owning_root_edges_fail_closed() {
    let connector = FixtureConnector {
        incomplete: false,
        duplicate_projection_key: false,
        duplicate_owning_root_edge: true,
        omit_owning_root_edge: false,
    };
    let error = bootstrap_and_project(&connector, request(), 1)
        .expect_err("multiple owning roots must fail closed");
    assert!(error.to_string().contains("multiple owning-root edges"));
}

#[test]
fn mixed_owning_root_provenance_fails_closed() {
    let connector = FixtureConnector {
        incomplete: false,
        duplicate_projection_key: false,
        duplicate_owning_root_edge: false,
        omit_owning_root_edge: true,
    };
    let error = bootstrap_and_project(&connector, request(), 1)
        .expect_err("mixed owning-root provenance must fail closed");
    assert!(error.to_string().contains("ambiguous owning-root"));
}

#[test]
fn paginated_bootstrap_matches_the_exact_one_shot_candidate() {
    let paginated_connector = PagedFixtureConnector::new(PagedFault::None);
    let paginated = bootstrap_and_project_to_completion(
        &paginated_connector,
        paged_request(1),
        1,
        generous_aggregation_limits(),
    )
    .expect("paginated aggregate");
    let one_shot_connector = PagedFixtureConnector::new(PagedFault::None);
    let one_shot =
        bootstrap_and_project(&one_shot_connector, paged_request(100), 1).expect("one shot");

    assert_eq!(paginated, one_shot);
    assert_eq!(
        paginated_connector.calls(),
        vec![None, Some("cp1".to_string()), Some("cp2".to_string())]
    );
    assert!(paginated.is_publication_eligible());
    paginated.require_complete().expect("complete aggregate");
}

#[test]
fn paginated_bootstrap_begins_with_the_callers_checkpoint() {
    let checkpoint = PortableCheckpoint {
        format_version: 1,
        opaque: "cp1".to_string(),
    };
    let mut aggregate_request = paged_request(1);
    aggregate_request.checkpoint = Some(checkpoint.clone());
    let connector = PagedFixtureConnector::new(PagedFault::None);
    let aggregate = bootstrap_and_project_to_completion(
        &connector,
        aggregate_request,
        1,
        generous_aggregation_limits(),
    )
    .expect("aggregate from caller checkpoint");
    let mut one_shot_request = paged_request(100);
    one_shot_request.checkpoint = Some(checkpoint);
    let one_shot = bootstrap_and_project(
        &PagedFixtureConnector::new(PagedFault::None),
        one_shot_request,
        1,
    )
    .expect("one shot from caller checkpoint");

    assert_eq!(aggregate, one_shot);
    assert_eq!(
        connector.calls(),
        vec![Some("cp1".to_string()), Some("cp2".to_string())]
    );
}

#[test]
fn pagination_removes_only_continuation_incompleteness() {
    let connector = PagedFixtureConnector::new(PagedFault::NonContinuationIncomplete);
    let aggregate = bootstrap_and_project_to_completion(
        &connector,
        paged_request(1),
        1,
        generous_aggregation_limits(),
    )
    .expect("incomplete aggregate");

    assert!(!aggregate.is_publication_eligible());
    assert_eq!(
        aggregate.completeness.incomplete_reasons(),
        [PortableIncompleteReason::ConnectorLimitation {
            code: "fixture_gap".to_string(),
            remote_id: Some(RemoteId::new("provider-value")),
        }]
    );
}

#[test]
fn continuation_checkpoints_must_be_nonempty_changed_and_acyclic() {
    for (fault, expected) in [
        (PagedFault::EmptyCheckpoint, "empty checkpoint"),
        (PagedFault::RepeatedCheckpoint, "repeated its checkpoint"),
        (PagedFault::CheckpointCycle, "checkpoint cycle"),
    ] {
        let connector = PagedFixtureConnector::new(fault);
        let error = bootstrap_and_project_to_completion(
            &connector,
            paged_request(1),
            1,
            generous_aggregation_limits(),
        )
        .expect_err("unsafe checkpoint must fail");
        assert!(error.to_string().contains(expected), "{fault:?}: {error}");
    }
}

#[test]
fn aggregation_maps_changed_connection_failures_to_a_static_error() {
    let connector = PagedFixtureConnector::new(PagedFault::ChangedConnection);
    let error = bootstrap_and_project_to_completion(
        &connector,
        paged_request(1),
        1,
        generous_aggregation_limits(),
    )
    .expect_err("changed connection must fail");

    assert_eq!(
        error.to_string(),
        "invalid state: portable bootstrap aggregation page failed"
    );
    assert!(!error.to_string().contains("provider-changed-connection"));
}

#[test]
fn aggregation_rejects_every_cross_page_identity_collision() {
    for (fault, expected) in [
        (
            PagedFault::DuplicateSourceVersion,
            "repeated a source version",
        ),
        (
            PagedFault::DuplicateObservedSource,
            "repeated an observed source",
        ),
        (
            PagedFault::DuplicateContentArtifact,
            "repeated a content artifact",
        ),
        (
            PagedFault::DuplicateProjectionArtifact,
            "repeated a projection artifact",
        ),
        (PagedFault::DuplicateLogicalPath, "repeated a logical path"),
    ] {
        let connector = PagedFixtureConnector::new(fault);
        let error = bootstrap_and_project_to_completion(
            &connector,
            paged_request(1),
            1,
            generous_aggregation_limits(),
        )
        .expect_err("cross-page collision must fail");
        assert!(error.to_string().contains(expected), "{fault:?}: {error}");
    }
}

#[test]
fn aggregation_limits_are_nonzero_and_enforced_before_growth() {
    for limits in [
        BootstrapAggregationLimits {
            max_checkpoints: 0,
            ..generous_aggregation_limits()
        },
        BootstrapAggregationLimits {
            max_total_changes: 0,
            ..generous_aggregation_limits()
        },
        BootstrapAggregationLimits {
            max_total_content_bytes: 0,
            ..generous_aggregation_limits()
        },
    ] {
        let connector = PagedFixtureConnector::new(PagedFault::None);
        let error = bootstrap_and_project_to_completion(&connector, paged_request(1), 1, limits)
            .expect_err("zero limit must fail");
        assert!(error.to_string().contains("limits must be nonzero"));
        assert!(connector.calls().is_empty());
    }

    let checkpoint_connector = PagedFixtureConnector::new(PagedFault::None);
    let checkpoint_error = bootstrap_and_project_to_completion(
        &checkpoint_connector,
        paged_request(1),
        1,
        BootstrapAggregationLimits {
            max_checkpoints: 2,
            ..generous_aggregation_limits()
        },
    )
    .expect_err("checkpoint bound");
    assert!(checkpoint_error.to_string().contains("checkpoint limit"));
    assert_eq!(checkpoint_connector.calls().len(), 2);

    let change_connector = PagedFixtureConnector::new(PagedFault::None);
    let change_error = bootstrap_and_project_to_completion(
        &change_connector,
        paged_request(1),
        1,
        BootstrapAggregationLimits {
            max_total_changes: 2,
            ..generous_aggregation_limits()
        },
    )
    .expect_err("change bound");
    assert!(change_error.to_string().contains("change limit"));

    let content_connector = PagedFixtureConnector::new(PagedFault::None);
    let content_error = bootstrap_and_project_to_completion(
        &content_connector,
        paged_request(1),
        1,
        BootstrapAggregationLimits {
            max_total_content_bytes: 1,
            ..generous_aggregation_limits()
        },
    )
    .expect_err("content bound");
    assert!(content_error.to_string().contains("content byte limit"));
}

#[test]
fn aggregation_preserves_direct_single_page_behavior() {
    let direct_connector = PagedFixtureConnector::new(PagedFault::None);
    let direct =
        bootstrap_and_project(&direct_connector, paged_request(100), 1).expect("direct batch");
    let aggregate_connector = PagedFixtureConnector::new(PagedFault::None);
    let aggregate = bootstrap_and_project_to_completion(
        &aggregate_connector,
        paged_request(100),
        1,
        generous_aggregation_limits(),
    )
    .expect("single-page aggregate");

    assert_eq!(aggregate, direct);
    assert_eq!(aggregate_connector.calls(), vec![None]);
}

fn request() -> PortableBootstrapRequest {
    PortableBootstrapRequest {
        source_connection_id: SourceConnectionId::new("source-fixture"),
        scope: locality_connector::PortableSourceScope::explicit_roots([RemoteId::new("root")]),
        checkpoint: None,
        max_changes: 100,
    }
}

fn paged_request(max_changes: u32) -> PortableBootstrapRequest {
    PortableBootstrapRequest {
        source_connection_id: SourceConnectionId::new("paged-source"),
        scope: locality_connector::PortableSourceScope::explicit_roots([RemoteId::new("root")]),
        checkpoint: None,
        max_changes,
    }
}

fn generous_aggregation_limits() -> BootstrapAggregationLimits {
    BootstrapAggregationLimits {
        max_checkpoints: 10,
        max_total_changes: 100,
        max_total_content_bytes: 1_000_000,
    }
}

fn paged_change(
    requested_connection: &SourceConnectionId,
    index: usize,
    fault: PagedFault,
) -> PortableSourceChange {
    let connection = if fault == PagedFault::ChangedConnection {
        SourceConnectionId::new("provider-changed-connection")
    } else {
        requested_connection.clone()
    };
    let remote_id = match (fault, index) {
        (PagedFault::DuplicateSourceVersion | PagedFault::DuplicateObservedSource, 1) => "page-a",
        (_, 0) => "page-a",
        (_, 1) => "page-b",
        _ => "page-c",
    };
    let path = if fault == PagedFault::DuplicateLogicalPath {
        "Shared/page.md".to_string()
    } else {
        format!("{remote_id}/page.md")
    };
    change(&connection, remote_id, &path)
}

fn change(
    source_connection_id: &SourceConnectionId,
    remote_id: &str,
    path: &str,
) -> PortableSourceChange {
    PortableSourceChange {
        source_object: SourceObject {
            source_connection_id: source_connection_id.clone(),
            remote_id: RemoteId::new(remote_id),
            kind: EntityKind::Page,
            edges: vec![SourceEdge {
                relationship: PORTABLE_SCOPE_ROOT_RELATIONSHIP.to_string(),
                target_remote_id: RemoteId::new("root"),
            }],
            opaque_version: Some("v1".to_string()),
            deleted: false,
            connector_metadata: BTreeMap::new(),
            acl_observations: Vec::new(),
            discovered_at: None,
            observed_at: None,
        },
        logical_path: Some(LogicalPath::new(path).expect("logical path")),
        requires_fetch: true,
    }
}
