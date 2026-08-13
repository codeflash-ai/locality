use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, Connector,
    ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest, NativeEntity,
    PORTABLE_SCOPE_ROOT_RELATIONSHIP, PORTABLE_SYNC_V2_MAX_CHECKPOINT_BYTES, ParsedEntity,
    PortableArtifactKey, PortableBatchAuthority, PortableBootstrapRequest, PortableChangeBatch,
    PortableChangeBatchV2, PortableCheckpoint, PortableCompleteness, PortableContentArtifact,
    PortableFetchRequest, PortableFetchResult, PortableIncompleteReason,
    PortableProjectionArtifact, PortableRenderRequest, PortableRenderResult, PortableSourceChange,
    PortableSyncHintV2, PortableSyncMode, PortableSyncRequest, PortableSyncRequestV2,
};
use locality_core::LocalityResult;
use locality_core::model::{CanonicalDocument, EntityKind, RemoteId, TreeEntry};
use locality_core::portable::{
    LogicalPath, ProjectionFileKind, SourceAction, SourceConnectionId, SourceEdge, SourceObject,
};
use locality_engine::synchronize_project::{
    BootstrapAggregationLimits, SynchronizationAggregationLimits, bootstrap_and_project,
    bootstrap_and_project_to_completion, synchronize_and_project_portable,
    synchronize_and_project_portable_v2, synchronize_and_project_portable_v2_to_completion,
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

struct V2FixtureConnector {
    authority: PortableBatchAuthority,
    complete: bool,
    provenance: V2FixtureProvenance,
    coverage: V2FixtureCoverage,
    change_count: usize,
    checkpoint_bytes: usize,
    dispatches: AtomicUsize,
    fetches: AtomicUsize,
}

#[derive(Clone, Copy)]
enum V2FixtureProvenance {
    InScope,
    Missing,
    MissingTombstone,
    Empty,
    Foreign,
    Mixed,
}

#[derive(Clone, Copy)]
enum V2FixtureCoverage {
    Exact,
    None,
    FirstOnly,
    Foreign,
    Duplicate,
}

impl V2FixtureConnector {
    fn new(authority: PortableBatchAuthority, complete: bool) -> Self {
        Self {
            authority,
            complete,
            provenance: V2FixtureProvenance::InScope,
            coverage: V2FixtureCoverage::Exact,
            change_count: 1,
            checkpoint_bytes: "v2-ready".len(),
            dispatches: AtomicUsize::new(0),
            fetches: AtomicUsize::new(0),
        }
    }

    fn with_provenance(mut self, provenance: V2FixtureProvenance) -> Self {
        self.provenance = provenance;
        self
    }

    fn with_coverage(mut self, coverage: V2FixtureCoverage) -> Self {
        self.coverage = coverage;
        self
    }

    fn with_change_count(mut self, change_count: usize) -> Self {
        self.change_count = change_count;
        self
    }

    fn with_checkpoint_bytes(mut self, checkpoint_bytes: usize) -> Self {
        self.checkpoint_bytes = checkpoint_bytes;
        self
    }
}

impl Connector for V2FixtureConnector {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind("v2-fixture")
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

    fn sync_portable_v2_impl(
        &self,
        request: PortableSyncRequestV2,
    ) -> LocalityResult<PortableChangeBatchV2> {
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        let mut in_scope = change(&request.source_connection_id, "page-a", "A/page.md");
        let mut foreign = change(&request.source_connection_id, "page-b", "B/page.md");
        foreign.source_object.edges[0].target_remote_id = RemoteId::new("foreign-root");
        let changes = match self.provenance {
            V2FixtureProvenance::InScope => (0..self.change_count)
                .map(|index| {
                    change(
                        &request.source_connection_id,
                        &format!("page-{index}"),
                        &format!("Page-{index}/page.md"),
                    )
                })
                .collect(),
            V2FixtureProvenance::Missing => {
                in_scope.source_object.edges.clear();
                vec![in_scope]
            }
            V2FixtureProvenance::MissingTombstone => {
                in_scope.source_object.edges.clear();
                in_scope.source_object.deleted = true;
                in_scope.requires_fetch = false;
                vec![in_scope]
            }
            V2FixtureProvenance::Empty => Vec::new(),
            V2FixtureProvenance::Foreign => vec![foreign],
            V2FixtureProvenance::Mixed => vec![in_scope, foreign],
        };
        let covered_root_remote_ids = match self.coverage {
            V2FixtureCoverage::Exact => request.scope.root_remote_ids.clone(),
            V2FixtureCoverage::None => Vec::new(),
            V2FixtureCoverage::FirstOnly => request
                .scope
                .root_remote_ids
                .first()
                .cloned()
                .into_iter()
                .collect(),
            V2FixtureCoverage::Foreign => vec![RemoteId::new("foreign-root")],
            V2FixtureCoverage::Duplicate => {
                vec![RemoteId::new("root"), RemoteId::new("root")]
            }
        };
        Ok(PortableChangeBatchV2 {
            changes,
            next_checkpoint: PortableCheckpoint {
                format_version: 1,
                opaque: "c".repeat(self.checkpoint_bytes),
            },
            completeness: if self.complete {
                PortableCompleteness::complete()
            } else {
                PortableCompleteness::incomplete(PortableIncompleteReason::CheckpointContinuation)
            },
            covered_root_remote_ids,
            authority: self.authority,
        })
    }

    fn fetch_portable(&self, request: PortableFetchRequest) -> LocalityResult<PortableFetchResult> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        FixtureConnector::complete().fetch_portable(request)
    }

    fn render_portable(
        &self,
        request: &PortableRenderRequest,
    ) -> LocalityResult<PortableRenderResult> {
        FixtureConnector::complete().render_portable(request)
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
enum V2PagedFault {
    None,
    TerminalIncremental,
    SubsetCoverage,
    DuplicateCoverage,
    ForeignCoverage,
    DuplicateSource,
    DuplicateArtifact,
    RepeatedCheckpoint,
    CheckpointCycle,
    TerminalRepeatedCheckpoint,
    TerminalEmptyCheckpoint,
    ResponseOverflow,
    IncompleteFetch,
    IncompleteRender,
}

struct V2PagedConnector {
    fault: V2PagedFault,
    requests: Mutex<Vec<PortableSyncRequestV2>>,
    fetches: AtomicUsize,
}

impl V2PagedConnector {
    fn new(fault: V2PagedFault) -> Self {
        Self {
            fault,
            requests: Mutex::new(Vec::new()),
            fetches: AtomicUsize::new(0),
        }
    }

    fn requests(&self) -> Vec<PortableSyncRequestV2> {
        self.requests.lock().expect("requests lock").clone()
    }
}

impl Connector for V2PagedConnector {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind("v2-paged-fixture")
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

    fn sync_portable_v2_impl(
        &self,
        request: PortableSyncRequestV2,
    ) -> LocalityResult<PortableChangeBatchV2> {
        self.requests
            .lock()
            .expect("requests lock")
            .push(request.clone());
        let page = match request.checkpoint.opaque.as_str() {
            "start" => 0,
            "cp-a" => 1,
            "cp-b" if self.fault == V2PagedFault::CheckpointCycle => 2,
            _ => {
                return Err(locality_core::LocalityError::InvalidState(
                    "fixture checkpoint is invalid".to_string(),
                ));
            }
        };

        let (remote_id, path, owning_root) = match page {
            0 => ("page-a", "A/page.md", "root-a"),
            1 if self.fault == V2PagedFault::DuplicateSource => {
                ("page-a", "A-again/page.md", "root-b")
            }
            1 => ("page-b", "B/page.md", "root-b"),
            _ => ("page-c", "C/page.md", "root-b"),
        };
        let mut changes = vec![v2_paged_change(
            &request.source_connection_id,
            remote_id,
            path,
            owning_root,
        )];
        if self.fault == V2PagedFault::ResponseOverflow && page == 0 {
            changes.push(v2_paged_change(
                &request.source_connection_id,
                "page-extra",
                "Extra/page.md",
                "root-a",
            ));
        }

        let (next_checkpoint, completeness) = match (self.fault, page) {
            (V2PagedFault::RepeatedCheckpoint, 0) => (
                "start",
                PortableCompleteness::incomplete(PortableIncompleteReason::CheckpointContinuation),
            ),
            (V2PagedFault::CheckpointCycle, 0) => (
                "cp-a",
                PortableCompleteness::incomplete(PortableIncompleteReason::CheckpointContinuation),
            ),
            (V2PagedFault::CheckpointCycle, 1) => (
                "cp-b",
                PortableCompleteness::incomplete(PortableIncompleteReason::CheckpointContinuation),
            ),
            (V2PagedFault::CheckpointCycle, _) => (
                "cp-a",
                PortableCompleteness::incomplete(PortableIncompleteReason::CheckpointContinuation),
            ),
            (V2PagedFault::TerminalRepeatedCheckpoint, 1) => {
                ("cp-a", PortableCompleteness::complete())
            }
            (V2PagedFault::TerminalEmptyCheckpoint, 1) => ("", PortableCompleteness::complete()),
            (_, 0) => (
                "cp-a",
                PortableCompleteness::incomplete(PortableIncompleteReason::CheckpointContinuation),
            ),
            (_, _) => ("done", PortableCompleteness::complete()),
        };

        let covered_root_remote_ids = match (self.fault, page) {
            (V2PagedFault::SubsetCoverage, 1) => vec![RemoteId::new("root-b")],
            (V2PagedFault::DuplicateCoverage, 0) => {
                vec![RemoteId::new("root-a"), RemoteId::new("root-a")]
            }
            (V2PagedFault::ForeignCoverage, 0) => vec![RemoteId::new("foreign-root")],
            (_, 0) => vec![RemoteId::new("root-a")],
            (_, 1) => vec![RemoteId::new("root-a"), RemoteId::new("root-b")],
            _ => Vec::new(),
        };
        let authority = if page == 0 || self.fault != V2PagedFault::TerminalIncremental {
            PortableBatchAuthority::CompleteScopeSnapshot
        } else {
            PortableBatchAuthority::Incremental
        };

        Ok(PortableChangeBatchV2 {
            changes,
            next_checkpoint: PortableCheckpoint {
                format_version: 1,
                opaque: next_checkpoint.to_string(),
            },
            completeness,
            covered_root_remote_ids,
            authority,
        })
    }

    fn fetch_portable(&self, request: PortableFetchRequest) -> LocalityResult<PortableFetchResult> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        let mut result = FixtureConnector::complete().fetch_portable(request)?;
        if self.fault == V2PagedFault::IncompleteFetch {
            result.completeness =
                PortableCompleteness::incomplete(PortableIncompleteReason::ConnectorLimitation {
                    code: "fetch_gap".to_string(),
                    remote_id: None,
                });
        }
        Ok(result)
    }

    fn render_portable(
        &self,
        request: &PortableRenderRequest,
    ) -> LocalityResult<PortableRenderResult> {
        let mut result = FixtureConnector::complete().render_portable(request)?;
        if self.fault == V2PagedFault::DuplicateArtifact {
            result.canonical.artifact_key = PortableArtifactKey::new("v2:shared:canonical:v1");
        }
        if self.fault == V2PagedFault::IncompleteRender {
            result.completeness =
                PortableCompleteness::incomplete(PortableIncompleteReason::ConnectorLimitation {
                    code: "render_gap".to_string(),
                    remote_id: None,
                });
        }
        Ok(result)
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

fn v2_request(mode: PortableSyncMode) -> PortableSyncRequestV2 {
    PortableSyncRequestV2 {
        source_connection_id: SourceConnectionId::new("source-fixture"),
        scope: locality_connector::PortableSourceScope::explicit_roots([RemoteId::new("root")]),
        checkpoint: PortableCheckpoint {
            format_version: 1,
            opaque: "ready".to_string(),
        },
        mode,
        hints: Vec::new(),
        max_changes: 100,
    }
}

#[test]
fn default_v2_adapter_preserves_mode_but_never_authorizes_omission() {
    let synchronized = synchronize_and_project_portable_v2(
        &FixtureConnector::complete(),
        v2_request(PortableSyncMode::ReconcileScope),
        1,
    )
    .expect("legacy connector through v2 adapter");

    assert_eq!(synchronized.mode(), PortableSyncMode::ReconcileScope);
    assert_eq!(
        synchronized.authority(),
        PortableBatchAuthority::Incremental
    );
    assert_eq!(
        synchronized.scope().root_remote_ids,
        vec![RemoteId::new("root")]
    );
    assert!(synchronized.batch().completeness.is_complete());
    assert!(!synchronized.authorizes_omission());
}

#[test]
fn v2_omission_authority_requires_all_three_conditions() {
    let mut authorized_cases = 0;
    let mut unauthorized_cases = 0;
    for mode in [
        PortableSyncMode::HintsOnly,
        PortableSyncMode::ReconcileScope,
    ] {
        for complete in [false, true] {
            for authority in [
                PortableBatchAuthority::Incremental,
                PortableBatchAuthority::CompleteScopeSnapshot,
            ] {
                let connector = V2FixtureConnector::new(authority, complete);
                let synchronized =
                    synchronize_and_project_portable_v2(&connector, v2_request(mode), 1)
                        .expect("v2 synchronization");
                let expected = mode == PortableSyncMode::ReconcileScope
                    && complete
                    && authority == PortableBatchAuthority::CompleteScopeSnapshot;
                assert_eq!(synchronized.mode(), mode);
                assert_eq!(synchronized.authority(), authority);
                assert_eq!(synchronized.authorizes_omission(), expected);
                assert_eq!(connector.dispatches.load(Ordering::SeqCst), 1);
                if expected {
                    authorized_cases += 1;
                } else {
                    unauthorized_cases += 1;
                }
            }
        }
    }
    assert_eq!(authorized_cases, 1);
    assert_eq!(unauthorized_cases, 7);
}

#[test]
fn v2_scope_provenance_is_required_and_foreign_roots_fail_closed() {
    for provenance in [
        V2FixtureProvenance::Missing,
        V2FixtureProvenance::MissingTombstone,
    ] {
        let connector =
            V2FixtureConnector::new(PortableBatchAuthority::CompleteScopeSnapshot, true)
                .with_provenance(provenance);
        let error = synchronize_and_project_portable_v2(
            &connector,
            v2_request(PortableSyncMode::ReconcileScope),
            1,
        )
        .expect_err("unbound changes and tombstones must fail the workflow");
        assert_eq!(
            error,
            locality_core::LocalityError::InvalidState(
                "portable v2 connector returned source without owning-root provenance".to_string()
            )
        );
    }

    let connector = V2FixtureConnector::new(PortableBatchAuthority::CompleteScopeSnapshot, true)
        .with_provenance(V2FixtureProvenance::Empty)
        .with_coverage(V2FixtureCoverage::None);
    let synchronized = synchronize_and_project_portable_v2(
        &connector,
        v2_request(PortableSyncMode::ReconcileScope),
        1,
    )
    .expect("uncovered empty result remains inspectable");
    assert!(!synchronized.authorizes_omission());

    let connector = V2FixtureConnector::new(PortableBatchAuthority::CompleteScopeSnapshot, true)
        .with_provenance(V2FixtureProvenance::Empty);
    let synchronized = synchronize_and_project_portable_v2(
        &connector,
        v2_request(PortableSyncMode::ReconcileScope),
        1,
    )
    .expect("explicitly covered empty result");
    assert!(synchronized.authorizes_omission());

    for provenance in [V2FixtureProvenance::Foreign, V2FixtureProvenance::Mixed] {
        let connector =
            V2FixtureConnector::new(PortableBatchAuthority::CompleteScopeSnapshot, true)
                .with_provenance(provenance);
        let error = synchronize_and_project_portable_v2(
            &connector,
            v2_request(PortableSyncMode::ReconcileScope),
            1,
        )
        .expect_err("foreign root provenance must fail the workflow");
        assert_eq!(
            error,
            locality_core::LocalityError::InvalidState(
                "portable v2 connector returned source outside the requested scope".to_string()
            )
        );
    }
}

#[test]
fn v2_coverage_must_exactly_equal_the_requested_scope() {
    let connector = V2FixtureConnector::new(PortableBatchAuthority::CompleteScopeSnapshot, true)
        .with_coverage(V2FixtureCoverage::FirstOnly);
    let mut request = v2_request(PortableSyncMode::ReconcileScope);
    request.scope.root_remote_ids.push(RemoteId::new("root-b"));
    let synchronized = synchronize_and_project_portable_v2(&connector, request, 1)
        .expect("strict coverage subset remains non-authoritative");
    assert_eq!(
        synchronized.scope().root_remote_ids,
        vec![RemoteId::new("root"), RemoteId::new("root-b")]
    );
    assert!(!synchronized.authorizes_omission());

    for (coverage, expected) in [
        (
            V2FixtureCoverage::Foreign,
            "portable sync v2 batch covers a root outside the requested scope",
        ),
        (
            V2FixtureCoverage::Duplicate,
            "portable sync v2 batch contains duplicate covered root remote IDs",
        ),
    ] {
        let connector =
            V2FixtureConnector::new(PortableBatchAuthority::CompleteScopeSnapshot, true)
                .with_coverage(coverage);
        let error = synchronize_and_project_portable_v2(
            &connector,
            v2_request(PortableSyncMode::ReconcileScope),
            1,
        )
        .expect_err("invalid response coverage must fail closed");
        assert_eq!(
            error,
            locality_core::LocalityError::InvalidState(expected.to_string())
        );
    }
}

#[test]
fn v2_response_bounds_fail_before_projection() {
    let connector = V2FixtureConnector::new(PortableBatchAuthority::Incremental, true)
        .with_change_count(1)
        .with_checkpoint_bytes(PORTABLE_SYNC_V2_MAX_CHECKPOINT_BYTES);
    let mut request = v2_request(PortableSyncMode::HintsOnly);
    request.max_changes = 1;
    synchronize_and_project_portable_v2(&connector, request, 1)
        .expect("response exactly at change and checkpoint limits");
    assert_eq!(connector.fetches.load(Ordering::SeqCst), 1);

    let connector =
        V2FixtureConnector::new(PortableBatchAuthority::Incremental, true).with_change_count(2);
    let mut request = v2_request(PortableSyncMode::HintsOnly);
    request.max_changes = 1;
    let error = synchronize_and_project_portable_v2(&connector, request, 1)
        .expect_err("response above request change limit");
    assert_eq!(
        error,
        locality_core::LocalityError::InvalidState(
            "portable sync v2 batch has 2 changes; request maximum is 1".to_string()
        )
    );
    assert_eq!(connector.fetches.load(Ordering::SeqCst), 0);

    let connector = V2FixtureConnector::new(PortableBatchAuthority::Incremental, true)
        .with_checkpoint_bytes(PORTABLE_SYNC_V2_MAX_CHECKPOINT_BYTES + 1);
    let error =
        synchronize_and_project_portable_v2(&connector, v2_request(PortableSyncMode::HintsOnly), 1)
            .expect_err("response above checkpoint limit");
    assert_eq!(
        error,
        locality_core::LocalityError::InvalidState(format!(
            "portable sync v2 response checkpoint is {} UTF-8 bytes; maximum is {}",
            PORTABLE_SYNC_V2_MAX_CHECKPOINT_BYTES + 1,
            PORTABLE_SYNC_V2_MAX_CHECKPOINT_BYTES
        ))
    );
    assert_eq!(connector.fetches.load(Ordering::SeqCst), 0);
}

#[test]
fn v2_validation_rejects_invalid_hints_before_connector_dispatch() {
    let connector = V2FixtureConnector::new(PortableBatchAuthority::Incremental, true);
    let mut request = v2_request(PortableSyncMode::HintsOnly);
    request.hints = vec![
        PortableSyncHintV2 {
            remote_id: RemoteId::new("duplicate"),
            provider_version: None,
            logical_path: None,
            source_kind: None,
            owning_root_remote_id: Some(RemoteId::new("root")),
        },
        PortableSyncHintV2 {
            remote_id: RemoteId::new("duplicate"),
            provider_version: None,
            logical_path: None,
            source_kind: None,
            owning_root_remote_id: Some(RemoteId::new("root")),
        },
    ];

    let error = synchronize_and_project_portable_v2(&connector, request, 1)
        .expect_err("duplicate hints must fail before connector dispatch");
    assert_eq!(
        error,
        locality_core::LocalityError::InvalidState(
            "portable sync v2 contains duplicate hint remote IDs".to_string()
        )
    );
    assert_eq!(connector.dispatches.load(Ordering::SeqCst), 0);
}

#[test]
fn paginated_v2_allows_repeated_coverage_with_an_exact_terminal_snapshot() {
    let connector = V2PagedConnector::new(V2PagedFault::None);
    let request = v2_paged_request(1);
    let aggregate = synchronize_and_project_portable_v2_to_completion(
        &connector,
        request.clone(),
        1,
        generous_sync_v2_aggregation_limits(),
    )
    .expect("multi-root v2 aggregate");

    assert_eq!(aggregate.scope(), &request.scope);
    assert_eq!(aggregate.mode(), request.mode);
    assert_eq!(
        aggregate.authority(),
        PortableBatchAuthority::CompleteScopeSnapshot
    );
    assert!(aggregate.authorizes_omission());
    assert!(aggregate.batch().is_publication_eligible());
    assert_eq!(aggregate.batch().next_checkpoint.opaque, "done");
    assert_eq!(
        aggregate
            .batch()
            .observed_changes
            .iter()
            .map(|change| change.source_object.remote_id.as_str())
            .collect::<Vec<_>>(),
        vec!["page-a", "page-b"]
    );

    let requests = connector.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], request);
    let mut second_request = request.clone();
    second_request.checkpoint.opaque = "cp-a".to_string();
    assert_eq!(requests[1], second_request);
}

#[test]
fn paginated_v2_uses_only_terminal_authority_and_complete_aggregate_coverage() {
    let terminal_incremental = V2PagedConnector::new(V2PagedFault::TerminalIncremental);
    let aggregate = synchronize_and_project_portable_v2_to_completion(
        &terminal_incremental,
        v2_paged_request(1),
        1,
        generous_sync_v2_aggregation_limits(),
    )
    .expect("terminal incremental aggregate");
    assert_eq!(aggregate.authority(), PortableBatchAuthority::Incremental);
    assert!(aggregate.batch().is_publication_eligible());
    assert!(!aggregate.authorizes_omission());

    let subset = V2PagedConnector::new(V2PagedFault::SubsetCoverage);
    let aggregate = synchronize_and_project_portable_v2_to_completion(
        &subset,
        v2_paged_request(1),
        1,
        generous_sync_v2_aggregation_limits(),
    )
    .expect("subset coverage remains inspectable");
    assert_eq!(
        aggregate.authority(),
        PortableBatchAuthority::CompleteScopeSnapshot
    );
    assert!(aggregate.batch().is_publication_eligible());
    assert!(!aggregate.authorizes_omission());
}

#[test]
fn paginated_v2_rejects_duplicate_and_foreign_coverage_before_fetch() {
    for fault in [
        V2PagedFault::DuplicateCoverage,
        V2PagedFault::ForeignCoverage,
    ] {
        let connector = V2PagedConnector::new(fault);
        synchronize_and_project_portable_v2_to_completion(
            &connector,
            v2_paged_request(1),
            1,
            generous_sync_v2_aggregation_limits(),
        )
        .expect_err("invalid coverage must fail");
        assert_eq!(connector.requests().len(), 1, "{fault:?}");
        assert_eq!(connector.fetches.load(Ordering::SeqCst), 0, "{fault:?}");
    }
}

#[test]
fn paginated_v2_rejects_checkpoint_replay_and_cycles() {
    for (fault, expected) in [
        (V2PagedFault::RepeatedCheckpoint, "did not advance"),
        (V2PagedFault::CheckpointCycle, "checkpoint cycle"),
    ] {
        let connector = V2PagedConnector::new(fault);
        let error = synchronize_and_project_portable_v2_to_completion(
            &connector,
            v2_paged_request(1),
            1,
            generous_sync_v2_aggregation_limits(),
        )
        .expect_err("unsafe checkpoint progression must fail");
        assert!(error.to_string().contains(expected), "{fault:?}: {error}");
    }
}

#[test]
fn paginated_v2_accepts_stateless_terminal_checkpoints() {
    for (fault, expected_checkpoint) in [
        (V2PagedFault::TerminalRepeatedCheckpoint, "cp-a"),
        (V2PagedFault::TerminalEmptyCheckpoint, ""),
    ] {
        let connector = V2PagedConnector::new(fault);
        let aggregate = synchronize_and_project_portable_v2_to_completion(
            &connector,
            v2_paged_request(1),
            1,
            generous_sync_v2_aggregation_limits(),
        )
        .expect("terminal checkpoint need not advance");
        assert_eq!(
            aggregate.batch().next_checkpoint.opaque,
            expected_checkpoint,
            "{fault:?}"
        );
        assert!(aggregate.authorizes_omission(), "{fault:?}");
    }
}

#[test]
fn paginated_v2_rejects_cross_page_source_and_artifact_collisions() {
    for (fault, expected) in [
        (V2PagedFault::DuplicateSource, "repeated a source version"),
        (
            V2PagedFault::DuplicateArtifact,
            "repeated a content artifact",
        ),
    ] {
        let connector = V2PagedConnector::new(fault);
        let error = synchronize_and_project_portable_v2_to_completion(
            &connector,
            v2_paged_request(1),
            1,
            generous_sync_v2_aggregation_limits(),
        )
        .expect_err("cross-page identity collision must fail");
        assert!(error.to_string().contains(expected), "{fault:?}: {error}");
    }
}

#[test]
fn paginated_v2_aggregate_limits_are_nonzero_and_enforced() {
    for limits in [
        SynchronizationAggregationLimits {
            max_checkpoints: 0,
            ..generous_sync_v2_aggregation_limits()
        },
        SynchronizationAggregationLimits {
            max_total_changes: 0,
            ..generous_sync_v2_aggregation_limits()
        },
        SynchronizationAggregationLimits {
            max_total_content_bytes: 0,
            ..generous_sync_v2_aggregation_limits()
        },
    ] {
        let connector = V2PagedConnector::new(V2PagedFault::None);
        let error = synchronize_and_project_portable_v2_to_completion(
            &connector,
            v2_paged_request(1),
            1,
            limits,
        )
        .expect_err("zero aggregate bound must fail");
        assert!(error.to_string().contains("limits must be nonzero"));
        assert!(connector.requests().is_empty());
    }

    for (limits, expected) in [
        (
            SynchronizationAggregationLimits {
                max_checkpoints: 1,
                ..generous_sync_v2_aggregation_limits()
            },
            "checkpoint limit",
        ),
        (
            SynchronizationAggregationLimits {
                max_total_changes: 1,
                ..generous_sync_v2_aggregation_limits()
            },
            "change limit",
        ),
        (
            SynchronizationAggregationLimits {
                max_total_content_bytes: 1,
                ..generous_sync_v2_aggregation_limits()
            },
            "content byte limit",
        ),
    ] {
        let connector = V2PagedConnector::new(V2PagedFault::None);
        let error = synchronize_and_project_portable_v2_to_completion(
            &connector,
            v2_paged_request(1),
            1,
            limits,
        )
        .expect_err("aggregate bound must fail");
        assert!(error.to_string().contains(expected), "{error}");
        if expected == "change limit" {
            assert_eq!(connector.requests().len(), 2);
            assert_eq!(connector.fetches.load(Ordering::SeqCst), 1);
        }
    }
}

#[test]
fn paginated_v2_merges_fetch_and_render_incompleteness() {
    for (fault, expected_code) in [
        (V2PagedFault::IncompleteFetch, "fetch_gap"),
        (V2PagedFault::IncompleteRender, "render_gap"),
    ] {
        let connector = V2PagedConnector::new(fault);
        let aggregate = synchronize_and_project_portable_v2_to_completion(
            &connector,
            v2_paged_request(1),
            1,
            generous_sync_v2_aggregation_limits(),
        )
        .expect("incomplete aggregate remains inspectable");
        assert!(!aggregate.batch().is_publication_eligible(), "{fault:?}");
        assert!(!aggregate.authorizes_omission(), "{fault:?}");
        assert_eq!(
            aggregate.batch().completeness.incomplete_reasons(),
            [PortableIncompleteReason::ConnectorLimitation {
                code: expected_code.to_string(),
                remote_id: None,
            }]
        );
    }
}

#[test]
fn paginated_v2_rejects_zero_work_and_oversized_responses_before_fetch() {
    let zero_work = V2PagedConnector::new(V2PagedFault::None);
    let error = synchronize_and_project_portable_v2_to_completion(
        &zero_work,
        v2_paged_request(0),
        1,
        generous_sync_v2_aggregation_limits(),
    )
    .expect_err("zero max_changes must fail validation");
    assert!(error.to_string().contains("max_changes must be in 1..="));
    assert!(zero_work.requests().is_empty());
    assert_eq!(zero_work.fetches.load(Ordering::SeqCst), 0);

    let overflow = V2PagedConnector::new(V2PagedFault::ResponseOverflow);
    synchronize_and_project_portable_v2_to_completion(
        &overflow,
        v2_paged_request(1),
        1,
        generous_sync_v2_aggregation_limits(),
    )
    .expect_err("response over per-request bound must fail");
    assert_eq!(overflow.requests().len(), 1);
    assert_eq!(overflow.fetches.load(Ordering::SeqCst), 0);
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

fn v2_paged_request(max_changes: u32) -> PortableSyncRequestV2 {
    PortableSyncRequestV2 {
        source_connection_id: SourceConnectionId::new("v2-paged-source"),
        scope: locality_connector::PortableSourceScope::explicit_roots([
            RemoteId::new("root-a"),
            RemoteId::new("root-b"),
        ]),
        checkpoint: PortableCheckpoint {
            format_version: 1,
            opaque: "start".to_string(),
        },
        mode: PortableSyncMode::ReconcileScope,
        hints: vec![PortableSyncHintV2 {
            remote_id: RemoteId::new("prior-a"),
            provider_version: Some("v0".to_string()),
            logical_path: Some(LogicalPath::new("Prior/page.md").expect("logical path")),
            source_kind: Some(EntityKind::Page),
            owning_root_remote_id: Some(RemoteId::new("root-a")),
        }],
        max_changes,
    }
}

fn generous_sync_v2_aggregation_limits() -> SynchronizationAggregationLimits {
    SynchronizationAggregationLimits {
        max_checkpoints: 10,
        max_total_changes: 100,
        max_total_content_bytes: 1_000_000,
    }
}

fn v2_paged_change(
    source_connection_id: &SourceConnectionId,
    remote_id: &str,
    path: &str,
    owning_root: &str,
) -> PortableSourceChange {
    let mut change = change(source_connection_id, remote_id, path);
    change.source_object.edges[0].target_remote_id = RemoteId::new(owning_root);
    change
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
