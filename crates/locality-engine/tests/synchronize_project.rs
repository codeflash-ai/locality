use std::collections::{BTreeMap, BTreeSet};

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
    bootstrap_and_project, synchronize_and_project_portable,
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

fn request() -> PortableBootstrapRequest {
    PortableBootstrapRequest {
        source_connection_id: SourceConnectionId::new("source-fixture"),
        scope: locality_connector::PortableSourceScope::explicit_roots([RemoteId::new("root")]),
        checkpoint: None,
        max_changes: 100,
    }
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
