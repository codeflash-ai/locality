use std::cell::RefCell;

use afs_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, Connector,
    ConnectorCapabilities, ConnectorKind, ConnectorPushApplier, ConnectorPushConcurrencyCheck,
    ConnectorUndoApplier, EnumerateRequest, FetchRequest, NativeEntity, ParsedEntity,
};
use afs_core::journal::{JournalApplyEffect, PushId, PushOperationId};
use afs_core::model::{
    CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry,
};
use afs_core::planner::{PushOperation, PushPlan};
use afs_core::push::{PushApplier, PushApplyRequest, PushConcurrencyCheck, PushConcurrencyRequest};
use afs_core::undo::{UndoApplier, UndoApplyRequest, UndoOperation, UndoPlan, UndoPlanStatus};
use afs_core::{AfsError, AfsResult};

#[test]
fn connector_adapters_forward_push_identity_and_plan() {
    let connector = FakeConnector::default();
    let push_id = PushId("push-1".to_string());
    let mount_id = MountId::new("notion-main");
    let plan = push_plan();
    let remote_ids = vec![RemoteId::new("page-1")];
    let operation_ids = vec![PushOperationId::for_operation(
        &push_id,
        0,
        &plan.operations[0],
    )];

    let mut concurrency = ConnectorPushConcurrencyCheck::new(&connector);
    concurrency
        .check(PushConcurrencyRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_ids: &remote_ids,
        })
        .expect("concurrency check");

    let mut applier = ConnectorPushApplier::new(&connector);
    let result = applier
        .apply(PushApplyRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &operation_ids,
            remote_ids: &remote_ids,
        })
        .expect("apply");

    assert_eq!(result.changed_remote_ids, vec![RemoteId::new("page-1")]);
    assert_eq!(result.effects.len(), 1);
    assert_eq!(
        connector.concurrency_push_ids.borrow().as_slice(),
        std::slice::from_ref(&push_id)
    );
    assert_eq!(
        connector.apply_push_ids.borrow().as_slice(),
        std::slice::from_ref(&push_id)
    );
    assert_eq!(
        connector.apply_operation_counts.borrow().as_slice(),
        [plan.operations.len()]
    );
}

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

#[derive(Debug, Default)]
struct FakeConnector {
    concurrency_push_ids: RefCell<Vec<PushId>>,
    apply_push_ids: RefCell<Vec<PushId>>,
    apply_operation_counts: RefCell<Vec<usize>>,
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
        }
    }

    fn enumerate(&self, request: EnumerateRequest) -> AfsResult<Vec<TreeEntry>> {
        Ok(vec![TreeEntry {
            mount_id: request.mount_id,
            remote_id: RemoteId::new("page-1"),
            kind: EntityKind::Page,
            title: "Roadmap".to_string(),
            path: "Roadmap.md".into(),
            hydration: HydrationState::Hydrated,
            content_hash: None,
            remote_edited_at: None,
        }])
    }

    fn fetch(&self, request: FetchRequest) -> AfsResult<NativeEntity> {
        Ok(NativeEntity {
            remote_id: request.remote_id,
            kind: "page".to_string(),
            raw: Vec::new(),
        })
    }

    fn render(&self, entity: &NativeEntity) -> AfsResult<CanonicalDocument> {
        Ok(CanonicalDocument::new(
            format!("afs:\n  id: {}\n", entity.remote_id.0),
            "",
        ))
    }

    fn parse(&self, document: &CanonicalDocument) -> AfsResult<ParsedEntity> {
        Ok(ParsedEntity {
            remote_id: RemoteId::new("page-1"),
            native: NativeEntity {
                remote_id: RemoteId::new("page-1"),
                kind: "page".to_string(),
                raw: document.body.as_bytes().to_vec(),
            },
        })
    }

    fn check_concurrency(&self, request: ApplyPlanRequest<'_>) -> AfsResult<()> {
        self.concurrency_push_ids
            .borrow_mut()
            .push(request.push_id.clone());
        Ok(())
    }

    fn apply(&self, request: ApplyPlanRequest<'_>) -> AfsResult<ApplyPlanResult> {
        if request.plan.is_empty() {
            return Err(AfsError::InvalidState(
                "fake connector expected a non-empty plan".to_string(),
            ));
        }

        self.apply_push_ids
            .borrow_mut()
            .push(request.push_id.clone());
        self.apply_operation_counts
            .borrow_mut()
            .push(request.plan.operations.len());
        Ok(ApplyPlanResult {
            changed_remote_ids: request.plan.affected_entities.clone(),
            effects: vec![JournalApplyEffect::UpdatedBlock {
                operation_id: request.operation_ids[0].clone(),
                operation_index: 0,
                block_id: RemoteId::new("block-1"),
            }],
        })
    }

    fn apply_undo(&self, request: ApplyUndoRequest<'_>) -> AfsResult<ApplyUndoResult> {
        self.undo_push_ids
            .borrow_mut()
            .push(request.target_push_id.clone());
        self.undo_operation_counts
            .borrow_mut()
            .push(request.plan.operations.len());
        Ok(ApplyUndoResult {
            changed_remote_ids: request.plan.affected_entities.clone(),
        })
    }
}

fn push_plan() -> PushPlan {
    PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![PushOperation::UpdateBlock {
            block_id: RemoteId::new("block-1"),
            content: "Changed".to_string(),
        }],
    )
}
