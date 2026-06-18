//! Remote change explanation.
//!
//! Freshness metadata can tell AFS that the Remote Tree moved, but it cannot
//! explain what changed. This module compares the three Nucleus views: the
//! Synced Tree shadow, the current Local Tree document, and a freshly rendered
//! Remote Tree document. The result is connector-neutral and suitable for CLI,
//! desktop, and daemon review flows.

use serde::{Deserialize, Serialize};

use crate::diff::{BlockDiffEngine, DiffEngine};
use crate::model::CanonicalDocument;
use crate::planner::PushPlan;
use crate::shadow::{ShadowDocument, rendered_bodies_equivalent};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteChangeDocument<'a> {
    pub document: &'a CanonicalDocument,
    pub body_start_line: usize,
}

impl<'a> RemoteChangeDocument<'a> {
    pub fn new(document: &'a CanonicalDocument, body_start_line: usize) -> Self {
        Self {
            document,
            body_start_line,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteChangeInput<'a> {
    Available(RemoteChangeDocument<'a>),
    Unavailable(RemoteChangeIssue),
}

impl<'a> RemoteChangeInput<'a> {
    pub fn available(document: &'a CanonicalDocument, body_start_line: usize) -> Self {
        Self::Available(RemoteChangeDocument::new(document, body_start_line))
    }

    pub fn unavailable(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Unavailable(RemoteChangeIssue::new(code, message))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteChangeExplanation {
    pub state: RemoteChangeState,
    pub action: RemoteChangeAction,
    pub local: RemoteChangeSide,
    pub remote: RemoteChangeSide,
    pub issues: Vec<RemoteChangeIssue>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteChangeState {
    AllSynced,
    LocalChangedOnly,
    RemoteChangedOnly,
    BothChanged,
    NeedsReview,
}

impl RemoteChangeState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AllSynced => "all_synced",
            Self::LocalChangedOnly => "local_changed_only",
            Self::RemoteChangedOnly => "remote_changed_only",
            Self::BothChanged => "both_changed",
            Self::NeedsReview => "needs_review",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteChangeAction {
    None,
    PushLocalChanges,
    SafeToFastForward,
    ReviewBeforePush,
}

impl RemoteChangeAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::PushLocalChanges => "push_local_changes",
            Self::SafeToFastForward => "safe_to_fast_forward",
            Self::ReviewBeforePush => "review_before_push",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteChangeSide {
    pub changed: bool,
    pub plan: Option<PushPlan>,
    pub issue: Option<RemoteChangeIssue>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteChangeIssue {
    pub code: String,
    pub message: String,
}

impl RemoteChangeIssue {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

pub fn explain_remote_change(
    shadow: &ShadowDocument,
    local: RemoteChangeInput<'_>,
    remote: RemoteChangeInput<'_>,
) -> RemoteChangeExplanation {
    let local = explain_side(shadow, local);
    let remote = explain_side(shadow, remote);
    let issues = [&local, &remote]
        .into_iter()
        .filter_map(|side| side.issue.clone())
        .collect::<Vec<_>>();
    let state = if issues.is_empty() {
        match (local.changed, remote.changed) {
            (false, false) => RemoteChangeState::AllSynced,
            (true, false) => RemoteChangeState::LocalChangedOnly,
            (false, true) => RemoteChangeState::RemoteChangedOnly,
            (true, true) => RemoteChangeState::BothChanged,
        }
    } else {
        RemoteChangeState::NeedsReview
    };
    let action = action_for_state(&state);

    RemoteChangeExplanation {
        state,
        action,
        local,
        remote,
        issues,
    }
}

fn explain_side(shadow: &ShadowDocument, input: RemoteChangeInput<'_>) -> RemoteChangeSide {
    let document = match input {
        RemoteChangeInput::Available(document) => document,
        RemoteChangeInput::Unavailable(issue) => {
            return RemoteChangeSide {
                changed: false,
                plan: None,
                issue: Some(issue),
            };
        }
    };

    let changed = document_changed_from_shadow(shadow, document.document);
    if !changed {
        return RemoteChangeSide {
            changed,
            plan: Some(PushPlan::default()),
            issue: None,
        };
    }

    let engine = BlockDiffEngine::new().with_edited_body_start_line(document.body_start_line);
    match engine.plan_push(shadow, document.document) {
        Ok(plan) => RemoteChangeSide {
            changed,
            plan: Some(plan),
            issue: None,
        },
        Err(error) => RemoteChangeSide {
            changed,
            plan: None,
            issue: Some(RemoteChangeIssue::new(
                "change_plan_failed",
                error.to_string(),
            )),
        },
    }
}

fn document_changed_from_shadow(shadow: &ShadowDocument, document: &CanonicalDocument) -> bool {
    shadow.frontmatter != document.frontmatter
        || !rendered_bodies_equivalent(&shadow.rendered_body, &document.body)
}

fn action_for_state(state: &RemoteChangeState) -> RemoteChangeAction {
    match state {
        RemoteChangeState::AllSynced => RemoteChangeAction::None,
        RemoteChangeState::LocalChangedOnly => RemoteChangeAction::PushLocalChanges,
        RemoteChangeState::RemoteChangedOnly => RemoteChangeAction::SafeToFastForward,
        RemoteChangeState::BothChanged | RemoteChangeState::NeedsReview => {
            RemoteChangeAction::ReviewBeforePush
        }
    }
}
