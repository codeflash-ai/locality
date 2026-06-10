use std::cell::RefCell;
use std::rc::Rc;

use afs_core::journal::{JournalEntry, JournalStatus, JournalStore, PushId};
use afs_core::model::{MountId, RemoteId};
use afs_core::planner::{GuardrailDecision, PushOperation, PushPlan};
use afs_core::push::{
    PushApplier, PushApplyRequest, PushApplyResult, PushConcurrencyCheck, PushConcurrencyRequest,
    PushExecutionAction, PushExecutionRequest, PushPipelineAction, PushPipelineResult,
    PushReconcileRequest, PushReconcileResult, PushReconciler, PushStage, execute_journaled_push,
};
use afs_core::validation::ValidationReport;
use afs_core::{AfsError, AfsResult};

#[test]
fn executor_journals_checks_applies_and_reconciles_in_order() {
    let events = event_log();
    let mut journal = RecordingJournal::new(events.clone());
    let mut concurrency = FakeConcurrency::new(events.clone());
    let mut applier = FakeApplier::new(events.clone(), [RemoteId::new("page-1")]);
    let mut reconciler = FakeReconciler::new(events.clone());

    let result = execute_journaled_push(
        &mut journal,
        &mut concurrency,
        &mut applier,
        &mut reconciler,
        PushExecutionRequest::new(push_id(), mount_id(), approved_pipeline()),
    )
    .expect("push execution");

    assert_eq!(result.action, PushExecutionAction::Reconciled);
    assert_eq!(result.changed_remote_ids, vec![RemoteId::new("page-1")]);
    assert_eq!(result.reconciled_remote_ids, vec![RemoteId::new("page-1")]);
    assert_eq!(result.journal_status, Some(JournalStatus::Reconciled));
    assert_eq!(
        result.completed_stages,
        vec![
            PushStage::ParseAndValidate,
            PushStage::Diff,
            PushStage::PlanAndConfirm,
            PushStage::ConcurrencyCheckAndApply,
            PushStage::JournalAndReconcile,
        ]
    );
    assert_eq!(
        events.borrow().as_slice(),
        [
            "append:prepared",
            "update:applying",
            "check",
            "apply",
            "update:applied",
            "reconcile",
            "update:reconciled",
        ]
    );
    assert_eq!(
        journal.entry.expect("journal").status,
        JournalStatus::Reconciled
    );
    assert_eq!(concurrency.seen_push_id, Some(push_id()));
    assert_eq!(applier.seen_push_id, Some(push_id()));
    assert_eq!(reconciler.seen_push_id, Some(push_id()));
}

#[test]
fn executor_does_not_journal_or_apply_until_pipeline_is_approved() {
    let events = event_log();
    let mut journal = RecordingJournal::new(events.clone());
    let mut concurrency = FakeConcurrency::new(events.clone());
    let mut applier = FakeApplier::new(events.clone(), [RemoteId::new("page-1")]);
    let mut reconciler = FakeReconciler::new(events.clone());

    let result = execute_journaled_push(
        &mut journal,
        &mut concurrency,
        &mut applier,
        &mut reconciler,
        PushExecutionRequest::new(
            push_id(),
            mount_id(),
            pipeline_with_action(PushPipelineAction::ConfirmPlan),
        ),
    )
    .expect("not-ready result");

    assert_eq!(
        result.action,
        PushExecutionAction::NotReady {
            pipeline_action: PushPipelineAction::ConfirmPlan,
        }
    );
    assert_eq!(result.journal_status, None);
    assert!(journal.entry.is_none());
    assert!(events.borrow().is_empty());
}

#[test]
fn executor_marks_failed_when_concurrency_check_fails_before_apply() {
    let events = event_log();
    let mut journal = RecordingJournal::new(events.clone());
    let mut concurrency = FakeConcurrency::new(events.clone())
        .with_failure(AfsError::Guardrail("remote moved".into()));
    let mut applier = FakeApplier::new(events.clone(), [RemoteId::new("page-1")]);
    let mut reconciler = FakeReconciler::new(events.clone());

    let error = execute_journaled_push(
        &mut journal,
        &mut concurrency,
        &mut applier,
        &mut reconciler,
        PushExecutionRequest::new(push_id(), mount_id(), approved_pipeline()),
    )
    .expect_err("concurrency failure");

    assert_eq!(error, AfsError::Guardrail("remote moved".into()));
    assert_eq!(
        events.borrow().as_slice(),
        [
            "append:prepared",
            "update:applying",
            "check",
            "update:failed"
        ]
    );
    assert!(matches!(
        journal.entry.expect("journal").status,
        JournalStatus::Failed(_)
    ));
}

#[test]
fn executor_marks_failed_when_apply_fails() {
    let events = event_log();
    let mut journal = RecordingJournal::new(events.clone());
    let mut concurrency = FakeConcurrency::new(events.clone());
    let mut applier = FakeApplier::new(events.clone(), [RemoteId::new("page-1")])
        .with_failure(AfsError::NotImplemented("fake apply"));
    let mut reconciler = FakeReconciler::new(events.clone());

    let error = execute_journaled_push(
        &mut journal,
        &mut concurrency,
        &mut applier,
        &mut reconciler,
        PushExecutionRequest::new(push_id(), mount_id(), approved_pipeline()),
    )
    .expect_err("apply failure");

    assert_eq!(error, AfsError::NotImplemented("fake apply"));
    assert_eq!(
        events.borrow().as_slice(),
        [
            "append:prepared",
            "update:applying",
            "check",
            "apply",
            "update:failed",
        ]
    );
    assert!(matches!(
        journal.entry.expect("journal").status,
        JournalStatus::Failed(_)
    ));
}

#[test]
fn executor_marks_failed_when_reconcile_fails_after_apply() {
    let events = event_log();
    let mut journal = RecordingJournal::new(events.clone());
    let mut concurrency = FakeConcurrency::new(events.clone());
    let mut applier = FakeApplier::new(events.clone(), [RemoteId::new("page-1")]);
    let mut reconciler =
        FakeReconciler::new(events.clone()).with_failure(AfsError::Io("read-back mismatch".into()));

    let error = execute_journaled_push(
        &mut journal,
        &mut concurrency,
        &mut applier,
        &mut reconciler,
        PushExecutionRequest::new(push_id(), mount_id(), approved_pipeline()),
    )
    .expect_err("reconcile failure");

    assert_eq!(error, AfsError::Io("read-back mismatch".into()));
    assert_eq!(
        events.borrow().as_slice(),
        [
            "append:prepared",
            "update:applying",
            "check",
            "apply",
            "update:applied",
            "reconcile",
            "update:failed",
        ]
    );
    assert!(matches!(
        journal.entry.expect("journal").status,
        JournalStatus::Failed(_)
    ));
}

#[test]
fn executor_rejects_approved_pipeline_without_plan_before_journaling() {
    let events = event_log();
    let mut journal = RecordingJournal::new(events.clone());
    let mut concurrency = FakeConcurrency::new(events.clone());
    let mut applier = FakeApplier::new(events.clone(), [RemoteId::new("page-1")]);
    let mut reconciler = FakeReconciler::new(events.clone());
    let mut pipeline = approved_pipeline();
    pipeline.plan = None;

    let error = execute_journaled_push(
        &mut journal,
        &mut concurrency,
        &mut applier,
        &mut reconciler,
        PushExecutionRequest::new(push_id(), mount_id(), pipeline),
    )
    .expect_err("invalid pipeline");

    assert_eq!(
        error,
        AfsError::InvalidState("push pipeline approved apply without a plan".to_string())
    );
    assert!(journal.entry.is_none());
    assert!(events.borrow().is_empty());
}

type EventLog = Rc<RefCell<Vec<&'static str>>>;

fn event_log() -> EventLog {
    Rc::new(RefCell::new(Vec::new()))
}

#[derive(Debug)]
struct RecordingJournal {
    entry: Option<JournalEntry>,
    events: EventLog,
}

impl RecordingJournal {
    fn new(events: EventLog) -> Self {
        Self {
            entry: None,
            events,
        }
    }
}

impl JournalStore for RecordingJournal {
    fn append(&mut self, entry: JournalEntry) -> AfsResult<()> {
        self.events
            .borrow_mut()
            .push(status_event("append", &entry.status));
        self.entry = Some(entry);
        Ok(())
    }

    fn update_status(&mut self, _push_id: &PushId, status: JournalStatus) -> AfsResult<()> {
        self.events
            .borrow_mut()
            .push(status_event("update", &status));
        let Some(entry) = self.entry.as_mut() else {
            return Err(AfsError::InvalidState(
                "journal status update before append".to_string(),
            ));
        };

        entry.status = status;
        Ok(())
    }
}

#[derive(Debug)]
struct FakeConcurrency {
    events: EventLog,
    failure: Option<AfsError>,
    seen_push_id: Option<PushId>,
}

impl FakeConcurrency {
    fn new(events: EventLog) -> Self {
        Self {
            events,
            failure: None,
            seen_push_id: None,
        }
    }

    fn with_failure(mut self, failure: AfsError) -> Self {
        self.failure = Some(failure);
        self
    }
}

impl PushConcurrencyCheck for FakeConcurrency {
    fn check(&mut self, request: PushConcurrencyRequest<'_>) -> AfsResult<()> {
        self.events.borrow_mut().push("check");
        self.seen_push_id = Some(request.push_id.clone());

        match &self.failure {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
}

#[derive(Debug)]
struct FakeApplier {
    events: EventLog,
    failure: Option<AfsError>,
    changed_remote_ids: Vec<RemoteId>,
    seen_push_id: Option<PushId>,
}

impl FakeApplier {
    fn new<const N: usize>(events: EventLog, changed_remote_ids: [RemoteId; N]) -> Self {
        Self {
            events,
            failure: None,
            changed_remote_ids: changed_remote_ids.into(),
            seen_push_id: None,
        }
    }

    fn with_failure(mut self, failure: AfsError) -> Self {
        self.failure = Some(failure);
        self
    }
}

impl PushApplier for FakeApplier {
    fn apply(&mut self, request: PushApplyRequest<'_>) -> AfsResult<PushApplyResult> {
        self.events.borrow_mut().push("apply");
        self.seen_push_id = Some(request.push_id.clone());

        match &self.failure {
            Some(error) => Err(error.clone()),
            None => Ok(PushApplyResult {
                changed_remote_ids: self.changed_remote_ids.clone(),
            }),
        }
    }
}

#[derive(Debug)]
struct FakeReconciler {
    events: EventLog,
    failure: Option<AfsError>,
    seen_push_id: Option<PushId>,
}

impl FakeReconciler {
    fn new(events: EventLog) -> Self {
        Self {
            events,
            failure: None,
            seen_push_id: None,
        }
    }

    fn with_failure(mut self, failure: AfsError) -> Self {
        self.failure = Some(failure);
        self
    }
}

impl PushReconciler for FakeReconciler {
    fn reconcile(&mut self, request: PushReconcileRequest<'_>) -> AfsResult<PushReconcileResult> {
        self.events.borrow_mut().push("reconcile");
        self.seen_push_id = Some(request.push_id.clone());

        match &self.failure {
            Some(error) => Err(error.clone()),
            None => Ok(PushReconcileResult {
                reconciled_remote_ids: request.changed_remote_ids.to_vec(),
            }),
        }
    }
}

fn approved_pipeline() -> PushPipelineResult {
    pipeline_with_action(PushPipelineAction::ProceedToApply)
}

fn pipeline_with_action(action: PushPipelineAction) -> PushPipelineResult {
    PushPipelineResult {
        validation: ValidationReport::clean(),
        plan: Some(push_plan()),
        guardrail: GuardrailDecision::Proceed,
        action,
        completed_stages: vec![
            PushStage::ParseAndValidate,
            PushStage::Diff,
            PushStage::PlanAndConfirm,
        ],
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

fn push_id() -> PushId {
    PushId("push-1".to_string())
}

fn mount_id() -> MountId {
    MountId::new("notion-main")
}

fn status_event(prefix: &'static str, status: &JournalStatus) -> &'static str {
    match (prefix, status) {
        ("append", JournalStatus::Prepared) => "append:prepared",
        ("update", JournalStatus::Applying) => "update:applying",
        ("update", JournalStatus::Applied) => "update:applied",
        ("update", JournalStatus::Reconciled) => "update:reconciled",
        ("update", JournalStatus::Failed(_)) => "update:failed",
        _ => "status:other",
    }
}
