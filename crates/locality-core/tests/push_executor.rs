use std::cell::RefCell;
use std::rc::Rc;

use locality_core::journal::{
    JournalApplyEffect, JournalEntry, JournalMetadata, JournalPreimage, JournalStatus,
    JournalStore, PushId,
};
use locality_core::model::{MountId, RemoteId};
use locality_core::planner::{GuardrailDecision, PushOperation, PushPlan};
use locality_core::push::{
    PushApplier, PushApplyRequest, PushApplyResult, PushConcurrencyCheck, PushConcurrencyRequest,
    PushExecutionAction, PushExecutionRequest, PushPipelineAction, PushPipelineResult,
    PushReconcileRequest, PushReconcileResult, PushReconciler, PushStage,
    execute_journaled_push_with_host,
};
use locality_core::readable_diff::ReadableDiffOutput;
use locality_core::shadow::ShadowDocument;
use locality_core::validation::ValidationReport;
use locality_core::{LocalityError, LocalityResult};

#[test]
fn executor_journals_checks_applies_and_reconciles_in_order() {
    let events = event_log();
    let mut host = RecordingHost::new(events.clone());

    let result = execute_journaled_push_with_host(
        &mut host,
        PushExecutionRequest::new(push_id(), mount_id(), approved_pipeline())
            .with_preimages(vec![JournalPreimage::from_shadow(shadow())]),
    )
    .expect("push execution");

    assert_eq!(result.action, PushExecutionAction::Reconciled);
    assert_eq!(result.changed_remote_ids, vec![RemoteId::new("page-1")]);
    assert_eq!(result.apply_effects.len(), 1);
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
            "effects:1",
            "update:applied",
            "reconcile",
            "update:reconciled",
        ]
    );
    let entry = host.journal.entry.expect("journal");
    assert_eq!(entry.status, JournalStatus::Reconciled);
    assert_eq!(entry.preimages.len(), 1);
    assert_eq!(entry.apply_effects.len(), 1);
    assert_eq!(host.concurrency.seen_push_id, Some(push_id()));
    assert_eq!(host.applier.seen_push_id, Some(push_id()));
    assert_eq!(host.reconciler.seen_push_id, Some(push_id()));
}

#[test]
fn executor_copies_request_metadata_and_readable_diff_to_journal() {
    let events = event_log();
    let mut host = RecordingHost::new(events);
    let metadata = JournalMetadata::anonymous(Some(PushId("push-0".to_string())), Some(12_345));
    let readable_diff = ReadableDiffOutput {
        files: Vec::new(),
        text: "diff --locality a/Roadmap/page.md b/Roadmap/page.md\n".to_string(),
    };

    execute_journaled_push_with_host(
        &mut host,
        PushExecutionRequest::new(push_id(), mount_id(), approved_pipeline())
            .with_metadata(metadata.clone())
            .with_readable_diff(Some(readable_diff.clone())),
    )
    .expect("push execution");

    let entry = host.journal.entry.expect("journal");
    assert_eq!(entry.metadata, metadata);
    assert_eq!(entry.readable_diff, Some(readable_diff));
}

#[test]
fn executor_does_not_journal_or_apply_until_pipeline_is_approved() {
    let events = event_log();
    let mut host = RecordingHost::new(events.clone());

    let result = execute_journaled_push_with_host(
        &mut host,
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
    assert!(host.journal.entry.is_none());
    assert!(events.borrow().is_empty());
}

#[test]
fn executor_reverts_journal_when_concurrency_check_fails_before_apply() {
    let events = event_log();
    let mut host = RecordingHost::new(events.clone());
    host.concurrency = host
        .concurrency
        .with_failure(LocalityError::Guardrail("remote moved".into()));

    let error = execute_journaled_push_with_host(
        &mut host,
        PushExecutionRequest::new(push_id(), mount_id(), approved_pipeline()),
    )
    .expect_err("concurrency failure");

    assert_eq!(error, LocalityError::Guardrail("remote moved".into()));
    assert_eq!(
        events.borrow().as_slice(),
        [
            "append:prepared",
            "update:applying",
            "check",
            "update:reverted"
        ]
    );
    assert_eq!(
        host.journal.entry.expect("journal").status,
        JournalStatus::Reverted
    );
}

#[test]
fn executor_marks_failed_when_apply_fails() {
    let events = event_log();
    let mut host = RecordingHost::new(events.clone());
    host.applier = host
        .applier
        .with_failure(LocalityError::NotImplemented("fake apply"));

    let error = execute_journaled_push_with_host(
        &mut host,
        PushExecutionRequest::new(push_id(), mount_id(), approved_pipeline()),
    )
    .expect_err("apply failure");

    assert_eq!(error, LocalityError::NotImplemented("fake apply"));
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
        host.journal.entry.expect("journal").status,
        JournalStatus::Failed(_)
    ));
}

#[test]
fn executor_marks_failed_when_reconcile_fails_after_apply() {
    let events = event_log();
    let mut host = RecordingHost::new(events.clone());
    host.reconciler = host
        .reconciler
        .with_failure(LocalityError::Io("read-back mismatch".into()));

    let error = execute_journaled_push_with_host(
        &mut host,
        PushExecutionRequest::new(push_id(), mount_id(), approved_pipeline()),
    )
    .expect_err("reconcile failure");

    assert_eq!(error, LocalityError::Io("read-back mismatch".into()));
    assert_eq!(
        events.borrow().as_slice(),
        [
            "append:prepared",
            "update:applying",
            "check",
            "apply",
            "effects:1",
            "update:applied",
            "reconcile",
            "update:failed",
        ]
    );
    assert!(matches!(
        host.journal.entry.expect("journal").status,
        JournalStatus::Failed(_)
    ));
}

#[test]
fn executor_rejects_approved_pipeline_without_plan_before_journaling() {
    let events = event_log();
    let mut host = RecordingHost::new(events.clone());
    let mut pipeline = approved_pipeline();
    pipeline.plan = None;

    let error = execute_journaled_push_with_host(
        &mut host,
        PushExecutionRequest::new(push_id(), mount_id(), pipeline),
    )
    .expect_err("invalid pipeline");

    assert_eq!(
        error,
        LocalityError::InvalidState("push pipeline approved apply without a plan".to_string())
    );
    assert!(host.journal.entry.is_none());
    assert!(events.borrow().is_empty());
}

type EventLog = Rc<RefCell<Vec<&'static str>>>;

fn event_log() -> EventLog {
    Rc::new(RefCell::new(Vec::new()))
}

#[derive(Debug)]
struct RecordingHost {
    journal: RecordingJournal,
    concurrency: FakeConcurrency,
    applier: FakeApplier,
    reconciler: FakeReconciler,
}

impl RecordingHost {
    fn new(events: EventLog) -> Self {
        Self {
            journal: RecordingJournal::new(events.clone()),
            concurrency: FakeConcurrency::new(events.clone()),
            applier: FakeApplier::new(events.clone(), [RemoteId::new("page-1")]),
            reconciler: FakeReconciler::new(events),
        }
    }
}

impl JournalStore for RecordingHost {
    fn append(&mut self, entry: JournalEntry) -> LocalityResult<()> {
        self.journal.append(entry)
    }

    fn update_status(&mut self, push_id: &PushId, status: JournalStatus) -> LocalityResult<()> {
        self.journal.update_status(push_id, status)
    }

    fn record_apply_effects(
        &mut self,
        push_id: &PushId,
        effects: Vec<JournalApplyEffect>,
    ) -> LocalityResult<()> {
        self.journal.record_apply_effects(push_id, effects)
    }
}

impl PushConcurrencyCheck for RecordingHost {
    fn check(&mut self, request: PushConcurrencyRequest<'_>) -> LocalityResult<()> {
        self.concurrency.check(request)
    }
}

impl PushApplier for RecordingHost {
    fn apply(&mut self, request: PushApplyRequest<'_>) -> LocalityResult<PushApplyResult> {
        self.applier.apply(request)
    }
}

impl PushReconciler for RecordingHost {
    fn reconcile(
        &mut self,
        request: PushReconcileRequest<'_>,
    ) -> LocalityResult<PushReconcileResult> {
        self.reconciler.reconcile(request)
    }
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
    fn append(&mut self, entry: JournalEntry) -> LocalityResult<()> {
        self.events
            .borrow_mut()
            .push(status_event("append", &entry.status));
        self.entry = Some(entry);
        Ok(())
    }

    fn update_status(&mut self, _push_id: &PushId, status: JournalStatus) -> LocalityResult<()> {
        self.events
            .borrow_mut()
            .push(status_event("update", &status));
        let Some(entry) = self.entry.as_mut() else {
            return Err(LocalityError::InvalidState(
                "journal status update before append".to_string(),
            ));
        };

        entry.status = status;
        Ok(())
    }

    fn record_apply_effects(
        &mut self,
        _push_id: &PushId,
        effects: Vec<JournalApplyEffect>,
    ) -> LocalityResult<()> {
        self.events.borrow_mut().push(effect_event(effects.len()));
        let Some(entry) = self.entry.as_mut() else {
            return Err(LocalityError::InvalidState(
                "journal effects update before append".to_string(),
            ));
        };

        entry.apply_effects = effects;
        Ok(())
    }
}

#[derive(Debug)]
struct FakeConcurrency {
    events: EventLog,
    failure: Option<LocalityError>,
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

    fn with_failure(mut self, failure: LocalityError) -> Self {
        self.failure = Some(failure);
        self
    }
}

impl PushConcurrencyCheck for FakeConcurrency {
    fn check(&mut self, request: PushConcurrencyRequest<'_>) -> LocalityResult<()> {
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
    failure: Option<LocalityError>,
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

    fn with_failure(mut self, failure: LocalityError) -> Self {
        self.failure = Some(failure);
        self
    }
}

impl PushApplier for FakeApplier {
    fn apply(&mut self, request: PushApplyRequest<'_>) -> LocalityResult<PushApplyResult> {
        self.events.borrow_mut().push("apply");
        self.seen_push_id = Some(request.push_id.clone());

        match &self.failure {
            Some(error) => Err(error.clone()),
            None => Ok(PushApplyResult {
                changed_remote_ids: self.changed_remote_ids.clone(),
                effects: vec![JournalApplyEffect::UpdatedBlock {
                    operation_id: request.operation_ids[0].clone(),
                    operation_index: 0,
                    block_id: RemoteId::new("block-1"),
                }],
            }),
        }
    }
}

#[derive(Debug)]
struct FakeReconciler {
    events: EventLog,
    failure: Option<LocalityError>,
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

    fn with_failure(mut self, failure: LocalityError) -> Self {
        self.failure = Some(failure);
        self
    }
}

impl PushReconciler for FakeReconciler {
    fn reconcile(
        &mut self,
        request: PushReconcileRequest<'_>,
    ) -> LocalityResult<PushReconcileResult> {
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

fn shadow() -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        "# Roadmap\n\nOriginal paragraph.",
        9,
        [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
    )
    .expect("shadow")
}

fn status_event(prefix: &'static str, status: &JournalStatus) -> &'static str {
    match (prefix, status) {
        ("append", JournalStatus::Prepared) => "append:prepared",
        ("update", JournalStatus::Applying) => "update:applying",
        ("update", JournalStatus::Applied) => "update:applied",
        ("update", JournalStatus::Reconciled) => "update:reconciled",
        ("update", JournalStatus::Reverted) => "update:reverted",
        ("update", JournalStatus::Failed(_)) => "update:failed",
        _ => "status:other",
    }
}

fn effect_event(effect_count: usize) -> &'static str {
    match effect_count {
        1 => "effects:1",
        _ => "effects:other",
    }
}
