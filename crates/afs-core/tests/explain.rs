use afs_core::explain::{
    RemoteChangeAction, RemoteChangeInput, RemoteChangeState, explain_remote_change,
};
use afs_core::model::{CanonicalDocument, RemoteId};
use afs_core::shadow::ShadowDocument;

#[test]
fn explain_reports_all_synced_when_local_and_remote_match_shadow() {
    let shadow = shadow("Base body.");
    let local = document("Base body.");
    let remote = document("Base body.");

    let explanation = explain_remote_change(
        &shadow,
        RemoteChangeInput::available(&local, 7),
        RemoteChangeInput::available(&remote, 7),
    );

    assert_eq!(explanation.state, RemoteChangeState::AllSynced);
    assert_eq!(explanation.action, RemoteChangeAction::None);
    assert!(!explanation.local.changed);
    assert!(!explanation.remote.changed);
    assert!(explanation.issues.is_empty());
}

#[test]
fn explain_reports_remote_changed_only_as_safe_to_fast_forward() {
    let shadow = shadow("Base body.");
    let local = document("Base body.");
    let remote = document("Remote body.");

    let explanation = explain_remote_change(
        &shadow,
        RemoteChangeInput::available(&local, 7),
        RemoteChangeInput::available(&remote, 7),
    );

    assert_eq!(explanation.state, RemoteChangeState::RemoteChangedOnly);
    assert_eq!(explanation.action, RemoteChangeAction::SafeToFastForward);
    assert!(!explanation.local.changed);
    assert!(explanation.remote.changed);
    assert_eq!(
        explanation
            .remote
            .plan
            .expect("remote plan")
            .summary
            .blocks_updated,
        1
    );
}

#[test]
fn explain_reports_local_changed_only_as_pushable() {
    let shadow = shadow("Base body.");
    let local = document("Local body.");
    let remote = document("Base body.");

    let explanation = explain_remote_change(
        &shadow,
        RemoteChangeInput::available(&local, 7),
        RemoteChangeInput::available(&remote, 7),
    );

    assert_eq!(explanation.state, RemoteChangeState::LocalChangedOnly);
    assert_eq!(explanation.action, RemoteChangeAction::PushLocalChanges);
    assert!(explanation.local.changed);
    assert!(!explanation.remote.changed);
}

#[test]
fn explain_reports_both_changed_as_review_before_push() {
    let shadow = shadow("Base body.");
    let local = document("Local body.");
    let remote = document("Remote body.");

    let explanation = explain_remote_change(
        &shadow,
        RemoteChangeInput::available(&local, 7),
        RemoteChangeInput::available(&remote, 7),
    );

    assert_eq!(explanation.state, RemoteChangeState::BothChanged);
    assert_eq!(explanation.action, RemoteChangeAction::ReviewBeforePush);
    assert!(explanation.local.changed);
    assert!(explanation.remote.changed);
}

#[test]
fn explain_reports_unavailable_side_as_needs_review() {
    let shadow = shadow("Base body.");
    let remote = document("Remote body.");

    let explanation = explain_remote_change(
        &shadow,
        RemoteChangeInput::unavailable("local_parse_failed", "local Markdown is invalid"),
        RemoteChangeInput::available(&remote, 7),
    );

    assert_eq!(explanation.state, RemoteChangeState::NeedsReview);
    assert_eq!(explanation.action, RemoteChangeAction::ReviewBeforePush);
    assert_eq!(explanation.issues[0].code, "local_parse_failed");
}

fn document(body: &str) -> CanonicalDocument {
    CanonicalDocument::new(frontmatter(), markdown_body(body))
}

fn shadow(body: &str) -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        markdown_body(body),
        7,
        [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
    )
    .expect("shadow")
    .with_frontmatter(frontmatter())
}

fn frontmatter() -> String {
    "afs:\n  id: page-1\n  type: page\ntitle: Roadmap\n".to_string()
}

fn markdown_body(body: &str) -> String {
    format!("# Roadmap\n\n{body}\n")
}
