use std::cell::RefCell;

use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, BatchObservationChange,
    BatchObservationCompleteness, BatchObserveRequest, BatchObserveResult, Connector,
    ConnectorCapabilities, ConnectorCheckpoint, ConnectorKind, ConnectorUndoApplier,
    EnumerateRequest, FetchRequest, NativeEntity, ParsedEntity,
};
use locality_core::LocalityResult;
use locality_core::journal::PushId;
use locality_core::model::{
    CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry,
};
use locality_core::planner::PushOperationKind;
use locality_core::undo::{UndoApplier, UndoApplyRequest, UndoOperation, UndoPlan, UndoPlanStatus};

#[test]
fn connector_adapter_forwards_undo_plan() {
    let connector = FakeConnector::default();
    let target_push_id = PushId("push-1".to_string());
    let mount_id = MountId::new("notion-main");
    let plan = UndoPlan {
        target_push_id: target_push_id.clone(),
        mount_id: mount_id.clone(),
        affected_entities: vec![RemoteId::new("page-1")],
        operations: vec![UndoOperation::RestoreBlockContent {
            block_id: RemoteId::new("block-1"),
            content: "Previous".to_string(),
        }],
        unsupported: Vec::new(),
        status: UndoPlanStatus::Complete,
    };

    let mut applier = ConnectorUndoApplier::new(&connector);
    let result = applier
        .apply_undo(UndoApplyRequest {
            target_push_id: &target_push_id,
            mount_id: &mount_id,
            plan: &plan,
        })
        .expect("undo apply");

    assert_eq!(result.changed_remote_ids, vec![RemoteId::new("page-1")]);
    assert_eq!(
        connector.undo_push_ids.borrow().as_slice(),
        std::slice::from_ref(&target_push_id)
    );
    assert_eq!(
        connector.undo_operation_counts.borrow().as_slice(),
        [plan.operations.len()]
    );
}

#[test]
fn connector_capabilities_are_serializable_stage10_flags() {
    let capabilities = ConnectorCapabilities {
        supports_remote_observation: true,
        supports_lazy_child_enumeration: true,
        ..ConnectorCapabilities::default()
    };

    assert!(capabilities.supports_local_only_stage10());
    let json = serde_json::to_string(&capabilities).expect("serialize capabilities");

    assert!(json.contains("supports_remote_observation"));
    assert!(json.contains("supports_lazy_child_enumeration"));
}

#[test]
fn connector_capabilities_default_entity_body_updates_for_old_json() {
    let old_json = r#"{
        "supports_block_updates": true,
        "supports_databases": false,
        "supports_oauth": false,
        "supports_remote_observation": true,
        "supports_lazy_child_enumeration": true,
        "supports_media_download": false,
        "supports_undo": true,
        "supports_batch_observation": false
    }"#;

    let old: ConnectorCapabilities = serde_json::from_str(old_json).expect("old capabilities");
    assert!(!old.supports_entity_body_updates);

    let current = ConnectorCapabilities {
        supports_entity_body_updates: true,
        ..old
    };
    let decoded: ConnectorCapabilities = serde_json::from_str(
        &serde_json::to_string(&current).expect("serialize current capabilities"),
    )
    .expect("current capabilities");
    assert!(decoded.supports_entity_body_updates);
}

#[test]
fn connector_capabilities_default_batch_observation_for_old_json() {
    let old_json = r#"{
        "supports_block_updates": true,
        "supports_entity_body_updates": false,
        "supports_databases": false,
        "supports_oauth": false,
        "supports_remote_observation": false,
        "supports_lazy_child_enumeration": false,
        "supports_media_download": false,
        "supports_undo": false
    }"#;

    let capabilities: ConnectorCapabilities =
        serde_json::from_str(old_json).expect("old capabilities");

    assert!(!capabilities.supports_batch_observation);
}

#[test]
fn batch_observation_alone_supports_local_only_stage10() {
    let capabilities = ConnectorCapabilities {
        supports_batch_observation: true,
        ..ConnectorCapabilities::default()
    };

    assert!(capabilities.supports_local_only_stage10());
}

#[test]
fn batch_observation_completeness_defaults_to_incremental() {
    assert_eq!(
        BatchObservationCompleteness::default(),
        BatchObservationCompleteness::Incremental
    );
}

#[test]
fn batch_observation_constructors_preserve_changes_and_checkpoint() {
    let entry = TreeEntry {
        mount_id: MountId::new("linear-main"),
        remote_id: RemoteId::new("issue-1"),
        kind: EntityKind::Page,
        title: "Fix login".to_string(),
        path: "teams/Engineering (ENG)/ENG-1-fix-login/page.md".into(),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some("2026-07-15T12:00:00Z".to_string()),
        stub_frontmatter: None,
    };
    let changes = vec![
        BatchObservationChange::Upsert(entry.clone()),
        BatchObservationChange::Tombstone {
            remote_id: RemoteId::new("issue-2"),
        },
    ];
    let checkpoint = ConnectorCheckpoint {
        state_version: 1,
        min_reader_version: 1,
        state_json: r#"{"watermark":"2026-07-15T12:00:00Z"}"#.to_string(),
    };

    let complete = BatchObserveResult::complete(changes.clone(), checkpoint.clone());
    assert_eq!(complete.changes, changes);
    assert_eq!(
        complete.completeness,
        BatchObservationCompleteness::Complete
    );
    assert_eq!(complete.next_checkpoint, checkpoint);
    assert!(complete.is_complete());

    let incremental_changes = vec![BatchObservationChange::Upsert(entry)];
    let incremental_checkpoint = ConnectorCheckpoint {
        state_version: 2,
        min_reader_version: 1,
        state_json: "{}".to_string(),
    };
    let incremental = BatchObserveResult::incremental(
        incremental_changes.clone(),
        incremental_checkpoint.clone(),
    );
    assert_eq!(incremental.changes, incremental_changes);
    assert_eq!(
        incremental.completeness,
        BatchObservationCompleteness::Incremental
    );
    assert!(!incremental.is_complete());
    assert_eq!(incremental.next_checkpoint, incremental_checkpoint);
}

#[test]
fn connector_default_batch_observation_is_exact_unsupported_error() {
    let error = FakeConnector::default()
        .observe_batch(BatchObserveRequest {
            mount_id: MountId::new("linear-main"),
            checkpoint: None,
        })
        .expect_err("default batch observation must be unsupported");

    assert_eq!(
        error,
        locality_core::LocalityError::Unsupported("connector does not support batch observation")
    );
}

#[test]
fn default_supported_operations_exclude_unadvertised_entity_body_updates() {
    let connector = FakeConnector::default();

    assert!(
        !connector
            .supported_push_operations()
            .contains(&PushOperationKind::UpdateEntityBody)
    );
}

#[derive(Debug, Default)]
struct FakeConnector {
    undo_push_ids: RefCell<Vec<PushId>>,
    undo_operation_counts: RefCell<Vec<usize>>,
}

impl Connector for FakeConnector {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind("fake")
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_block_updates: true,
            supports_databases: false,
            supports_oauth: false,
            supports_undo: true,
            ..ConnectorCapabilities::default()
        }
    }

    fn enumerate(&self, request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        Ok(vec![TreeEntry {
            mount_id: request.mount_id,
            remote_id: RemoteId::new("page-1"),
            kind: EntityKind::Page,
            title: "Roadmap".to_string(),
            path: "Roadmap.md".into(),
            hydration: HydrationState::Hydrated,
            content_hash: None,
            remote_edited_at: None,
            stub_frontmatter: None,
        }])
    }

    fn fetch(&self, request: FetchRequest) -> LocalityResult<NativeEntity> {
        Ok(NativeEntity {
            remote_id: request.remote_id,
            kind: "page".to_string(),
            raw: Vec::new(),
        })
    }

    fn render(&self, entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        Ok(CanonicalDocument::new(
            format!("loc:\n  id: {}\n", entity.remote_id.0),
            "",
        ))
    }

    fn parse(&self, document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        Ok(ParsedEntity {
            remote_id: RemoteId::new("page-1"),
            native: NativeEntity {
                remote_id: RemoteId::new("page-1"),
                kind: "page".to_string(),
                raw: document.body.as_bytes().to_vec(),
            },
        })
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        Ok(())
    }

    fn apply(&self, request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        Ok(ApplyPlanResult {
            changed_remote_ids: request.plan.affected_entities.clone(),
            effects: Vec::new(),
        })
    }

    fn apply_undo(&self, request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        self.undo_push_ids
            .borrow_mut()
            .push(request.target_push_id.clone());
        self.undo_operation_counts
            .borrow_mut()
            .push(request.plan.operations.len());
        Ok(ApplyUndoResult {
            changed_remote_ids: request.plan.affected_entities.clone(),
            observations: Vec::new(),
        })
    }
}
