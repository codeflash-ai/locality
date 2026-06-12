use std::path::PathBuf;

use afs_core::conflict::BlockChangeSet;
use afs_core::hydration::{HydrationPolicy, should_eager_hydrate};
use afs_core::model::{
    CanonicalBlock, CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, SourceSpan,
    TreeEntry,
};
use afs_core::planner::{GuardrailDecision, GuardrailPolicy, PushOperation, PushPlan};
use afs_core::push::evaluate_guardrails;
use afs_core::sync::{SyncDecision, ThreeTreeSnapshot, classify_changes, classify_colliding_edits};
use afs_core::validation::{
    DirectiveIntegrity, classify_directive_change, validate_directive_integrity,
};

#[test]
fn hydration_ladder_allows_expected_transitions() {
    assert!(HydrationState::Virtual.can_transition_to(&HydrationState::Stub));
    assert!(HydrationState::Stub.can_transition_to(&HydrationState::Hydrated));
    assert!(HydrationState::Hydrated.can_transition_to(&HydrationState::Dirty));
    assert!(HydrationState::Hydrated.can_transition_to(&HydrationState::Conflicted));
    assert!(HydrationState::Dirty.can_transition_to(&HydrationState::Conflicted));
    assert!(HydrationState::Dirty.can_transition_to(&HydrationState::Hydrated));
    assert!(HydrationState::Conflicted.can_transition_to(&HydrationState::Dirty));
    assert!(!HydrationState::Virtual.can_transition_to(&HydrationState::Hydrated));
    assert!(!HydrationState::Hydrated.can_transition_to(&HydrationState::Stub));
}

#[test]
fn boolean_change_classification_matches_push_pull_model() {
    let id = RemoteId("page-1".to_string());

    assert_eq!(
        classify_changes(false, false, id.clone()),
        SyncDecision::Noop
    );
    assert_eq!(
        classify_changes(true, false, id.clone()),
        SyncDecision::Pull {
            remote_id: id.clone()
        }
    );
    assert_eq!(
        classify_changes(false, true, id.clone()),
        SyncDecision::Push {
            remote_id: id.clone()
        }
    );
    assert_eq!(
        classify_changes(true, true, id.clone()),
        SyncDecision::Conflict { remote_id: id }
    );
}

#[test]
fn three_tree_snapshot_classifies_clean_remote_change_as_pull() {
    let synced = entry("page-1", "hash-a", "rev-a");
    let local = synced.clone();
    let remote = entry("page-1", "hash-b", "rev-b");

    let snapshot = ThreeTreeSnapshot {
        remote: Some(remote),
        local: Some(local),
        synced: Some(synced),
    };

    assert_eq!(
        snapshot.classify(),
        SyncDecision::Pull {
            remote_id: RemoteId::new("page-1")
        }
    );
}

#[test]
fn three_tree_snapshot_classifies_local_change_as_push() {
    let synced = entry("page-1", "hash-a", "rev-a");
    let remote = synced.clone();
    let local = entry("page-1", "hash-local", "rev-a");

    let snapshot = ThreeTreeSnapshot {
        remote: Some(remote),
        local: Some(local),
        synced: Some(synced),
    };

    assert_eq!(
        snapshot.classify(),
        SyncDecision::Push {
            remote_id: RemoteId::new("page-1")
        }
    );
}

#[test]
fn three_tree_snapshot_classifies_overlapping_local_and_remote_changes_as_conflict() {
    let synced = entry("page-1", "hash-a", "rev-a");
    let remote = entry("page-1", "hash-remote", "rev-b");
    let local = entry("page-1", "hash-local", "rev-a");

    let snapshot = ThreeTreeSnapshot {
        remote: Some(remote),
        local: Some(local),
        synced: Some(synced),
    };

    assert_eq!(
        snapshot.classify(),
        SyncDecision::Conflict {
            remote_id: RemoteId::new("page-1")
        }
    );
}

#[test]
fn remote_deletion_removes_clean_local_projection() {
    let synced = entry("page-1", "hash-a", "rev-a");
    let local = synced.clone();

    let snapshot = ThreeTreeSnapshot {
        remote: None,
        local: Some(local),
        synced: Some(synced),
    };

    assert_eq!(
        snapshot.classify(),
        SyncDecision::DeleteLocalProjection {
            remote_id: RemoteId::new("page-1")
        }
    );
}

#[test]
fn remote_deletion_conflicts_when_local_is_dirty() {
    let synced = entry("page-1", "hash-a", "rev-a");
    let local = entry("page-1", "local-change", "rev-a");

    let snapshot = ThreeTreeSnapshot {
        remote: None,
        local: Some(local),
        synced: Some(synced),
    };

    assert_eq!(
        snapshot.classify(),
        SyncDecision::Conflict {
            remote_id: RemoteId::new("page-1")
        }
    );
}

#[test]
fn local_deletion_pushes_archive_when_remote_is_clean() {
    let synced = entry("page-1", "hash-a", "rev-a");
    let remote = synced.clone();

    let snapshot = ThreeTreeSnapshot {
        remote: Some(remote),
        local: None,
        synced: Some(synced),
    };

    assert_eq!(
        snapshot.classify(),
        SyncDecision::Push {
            remote_id: RemoteId::new("page-1")
        }
    );
}

#[test]
fn disjoint_block_changes_auto_merge_and_overlapping_blocks_conflict() {
    let page_id = RemoteId::new("page-1");
    let remote = BlockChangeSet::from_blocks([RemoteId::new("block-a")]);
    let local = BlockChangeSet::from_blocks([RemoteId::new("block-b")]);

    assert_eq!(
        classify_colliding_edits(page_id.clone(), &remote, &local),
        SyncDecision::AutoMerge {
            remote_id: page_id.clone()
        }
    );

    let overlapping_local = BlockChangeSet::from_blocks([RemoteId::new("block-a")]);

    assert_eq!(
        classify_colliding_edits(page_id.clone(), &remote, &overlapping_local),
        SyncDecision::Conflict { remote_id: page_id }
    );
}

#[test]
fn structural_block_changes_do_not_auto_merge() {
    let page_id = RemoteId::new("page-1");
    let remote = BlockChangeSet::structural();
    let local = BlockChangeSet::from_blocks([RemoteId::new("block-b")]);

    assert_eq!(
        classify_colliding_edits(page_id.clone(), &remote, &local),
        SyncDecision::Conflict { remote_id: page_id }
    );
}

#[test]
fn hydration_eager_threshold_is_configurable() {
    let default_policy = HydrationPolicy::default();
    assert!(!should_eager_hydrate(10, &default_policy));

    let tuned_policy = HydrationPolicy {
        eager_under_page_count: Some(10_000),
        ..HydrationPolicy::default()
    };

    assert!(should_eager_hydrate(10, &tuned_policy));
    assert!(!should_eager_hydrate(10_001, &tuned_policy));
}

#[test]
fn directive_validation_allows_move_and_delete_but_rejects_mangle_and_invention() {
    let directive = CanonicalBlock::directive(
        RemoteId::new("block-1"),
        "synced_block",
        "::afs{id=block-1 type=synced_block}",
    );
    let shadow = CanonicalDocument::new("---\n---", "").with_blocks(vec![
        CanonicalBlock::native(Some(RemoteId::new("block-intro")), Some("hash".to_string())),
        directive.clone(),
    ]);
    let moved = CanonicalDocument::new("---\n---", "").with_blocks(vec![
        directive.clone(),
        CanonicalBlock::native(Some(RemoteId::new("block-intro")), Some("hash".to_string())),
    ]);

    assert!(validate_directive_integrity(&shadow, &moved, "page.md").is_clean());

    let deleted = CanonicalDocument::new("---\n---", "").with_blocks(vec![CanonicalBlock::native(
        Some(RemoteId::new("block-intro")),
        Some("hash".to_string()),
    )]);
    assert!(validate_directive_integrity(&shadow, &deleted, "page.md").is_clean());
    assert_eq!(
        classify_directive_change(&directive, None),
        DirectiveIntegrity::Deleted
    );

    let mut directive_at_line_10 = directive.clone();
    directive_at_line_10.source_span = Some(SourceSpan {
        start_line: 10,
        end_line: 10,
    });
    let mut directive_at_line_20 = directive.clone();
    directive_at_line_20.source_span = Some(SourceSpan {
        start_line: 20,
        end_line: 20,
    });
    assert_eq!(
        classify_directive_change(&directive_at_line_10, Some(&directive_at_line_20)),
        DirectiveIntegrity::Moved
    );

    let mangled =
        CanonicalDocument::new("---\n---", "").with_blocks(vec![CanonicalBlock::directive(
            RemoteId::new("block-1"),
            "synced_block",
            "::afs{id=block-1 type=synced_block title=\"edited\"}",
        )]);
    assert_eq!(
        validate_directive_integrity(&shadow, &mangled, "page.md").issues[0].code,
        "directive_mangled"
    );

    let invented =
        CanonicalDocument::new("---\n---", "").with_blocks(vec![CanonicalBlock::directive(
            RemoteId::new("invented"),
            "synced_block",
            "::afs{id=invented type=synced_block}",
        )]);
    assert_eq!(
        validate_directive_integrity(&shadow, &invented, "page.md").issues[0].code,
        "directive_unknown"
    );
}

#[test]
fn guardrails_require_confirm_for_large_archives_or_mount_touch() {
    let policy = GuardrailPolicy::default();
    let archive_ops = (0..11)
        .map(|index| PushOperation::ArchiveBlock {
            block_id: RemoteId::new(format!("block-{index}")),
        })
        .collect();
    let archive_plan = PushPlan::new(vec![RemoteId::new("page-1")], archive_ops);

    assert!(matches!(
        evaluate_guardrails(&archive_plan, &policy, Some(100)),
        GuardrailDecision::ConfirmRequired { .. }
    ));

    let broad_plan = PushPlan::new(
        (0..6)
            .map(|index| RemoteId::new(format!("page-{index}")))
            .collect(),
        vec![PushOperation::UpdateBlock {
            block_id: RemoteId::new("block-1"),
            content: "updated".to_string(),
        }],
    );

    assert!(matches!(
        evaluate_guardrails(&broad_plan, &policy, Some(100)),
        GuardrailDecision::ConfirmRequired { .. }
    ));
}

fn entry(remote_id: &str, content_hash: &str, remote_edited_at: &str) -> TreeEntry {
    TreeEntry {
        mount_id: MountId::new("mount-1"),
        remote_id: RemoteId::new(remote_id),
        kind: EntityKind::Page,
        title: "Roadmap".to_string(),
        path: PathBuf::from("Roadmap ~page.md"),
        hydration: HydrationState::Hydrated,
        content_hash: Some(content_hash.to_string()),
        remote_edited_at: Some(remote_edited_at.to_string()),
        stub_frontmatter: None,
    }
}
