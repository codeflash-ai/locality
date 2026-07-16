use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use loc_cli::history::{
    HistoryError, LogOptions, run_log, run_undo, run_undo_with_applier,
    run_undo_with_applier_at_state_root, undo_report_exit_code,
};
use locality_core::canonical::render_canonical_markdown;
use locality_core::freshness::RemoteObservation;
use locality_core::journal::{
    JournalApplyEffect, JournalEntry, JournalPreimage, JournalStatus, PushId, PushOperationId,
};
use locality_core::model::{CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId};
use locality_core::planner::{PushOperation, PushPlan};
use locality_core::shadow::ShadowDocument;
use locality_core::undo::{UndoApplier, UndoApplyRequest, UndoApplyResult};
use locality_core::{LocalityError, LocalityResult};
use locality_store::{
    EntityRecord, EntityRepository, InMemoryStateStore, JournalRepository, MountConfig,
    MountRepository, ProjectionMode, ShadowRepository, SqliteStateStore, VirtualMutationKind,
    VirtualMutationRecord, VirtualMutationRepository,
};
use localityd::file_provider::{
    WindowsCloudFilesProjectionEvent, WindowsCloudFilesProjectionRecoveryStatus,
    consume_windows_cloud_files_projection_acknowledgement,
    consume_windows_cloud_files_quarantine_acknowledgement,
    list_windows_cloud_files_projection_recoveries,
};
use localityd::virtual_fs::virtual_fs_content_path;

#[test]
fn log_lists_journal_entries_newest_first() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    store
        .append_journal(journal_entry("push-1", "page-1", JournalStatus::Prepared))
        .expect("append first journal");
    store
        .append_journal(journal_entry("push-2", "page-1", JournalStatus::Reconciled))
        .expect("append second journal");

    let report = run_log(&store, LogOptions::default()).expect("log report");

    assert!(report.ok);
    assert_eq!(report.entries.len(), 2);
    assert_eq!(report.entries[0].push_id, "push-2");
    assert_eq!(report.entries[0].status, "reconciled");
    assert_eq!(report.entries[0].operation_count, 1);
    assert_eq!(report.entries[0].preimage_count, 1);
    assert_eq!(report.entries[0].apply_effect_count, 0);
    assert_eq!(report.entries[0].plan_summary.blocks_updated, 1);
    assert_eq!(report.entries[1].push_id, "push-1");
}

#[test]
fn log_filters_journal_entries_by_projected_path() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    store
        .save_entity(entity_record(&fixture, "page-2", "Notes.md"))
        .expect("save second entity");
    store
        .append_journal(journal_entry("push-1", "page-1", JournalStatus::Prepared))
        .expect("append first journal");
    store
        .append_journal(journal_entry("push-2", "page-2", JournalStatus::Prepared))
        .expect("append second journal");

    let report = run_log(
        &store,
        LogOptions {
            path: Some(fixture.root.join("Roadmap.md")),
            ..LogOptions::default()
        },
    )
    .expect("filtered log");

    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].push_id, "push-1");
    assert_eq!(report.entries[0].remote_ids, vec!["page-1"]);
}

#[test]
fn log_page_directory_targets_page_document() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    fs::create_dir_all(fixture.root.join("Roadmap")).expect("create page dir");
    fs::write(fixture.root.join("Roadmap/page.md"), "").expect("write page document");
    store
        .save_entity(entity_record(&fixture, "page-2", "Roadmap/page.md"))
        .expect("save page-directory entity");
    store
        .append_journal(journal_entry("push-1", "page-2", JournalStatus::Prepared))
        .expect("append page-directory journal");

    let report = run_log(
        &store,
        LogOptions {
            path: Some(fixture.root.join("Roadmap")),
            ..LogOptions::default()
        },
    )
    .expect("page-directory log");

    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].push_id, "push-1");
    assert_eq!(report.entries[0].remote_ids, vec!["page-2"]);
}

#[test]
fn log_filters_created_entity_journal_by_created_path() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    fs::create_dir_all(fixture.root.join("New child")).expect("create child dir");
    fs::write(fixture.root.join("New child/page.md"), "").expect("write child page");
    store
        .save_entity(entity_record(
            &fixture,
            "created-page-1",
            "New child/page.md",
        ))
        .expect("save created entity");
    let operation = PushOperation::CreateEntity {
        parent_id: RemoteId::new("page-1"),
        parent_kind: Some(EntityKind::Page),
        parent_workspace: false,
        title: "New child".to_string(),
        properties: Default::default(),
        body: "Created child.".to_string(),
        source_path: "New child/page.md".into(),
    };
    let push_id = PushId("push-create".to_string());
    let operation_id = PushOperationId::for_operation(&push_id, 0, &operation);
    let entry = JournalEntry::new(
        push_id.clone(),
        fixture.mount_id.clone(),
        vec![RemoteId::new("page-1")],
        PushPlan::new(vec![RemoteId::new("page-1")], vec![operation]),
        JournalStatus::Reconciled,
    )
    .with_apply_effects(vec![JournalApplyEffect::CreatedEntity {
        operation_id,
        operation_index: 0,
        parent_id: RemoteId::new("page-1"),
        entity_id: RemoteId::new("created-page-1"),
    }]);
    store.append_journal(entry).expect("append create journal");

    let report = run_log(
        &store,
        LogOptions {
            path: Some(fixture.root.join("New child/page.md")),
            ..LogOptions::default()
        },
    )
    .expect("filtered created entity log");

    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].push_id, push_id.0);
    assert_eq!(report.entries[0].remote_ids, vec!["page-1"]);
    assert_eq!(report.entries[0].apply_effect_count, 1);
}

#[test]
fn log_reports_structured_error_for_unknown_path() {
    let fixture = HistoryFixture::new();
    let store = fixture.store();

    let error = run_log(
        &store,
        LogOptions {
            path: Some(fixture.root.join("Missing.md")),
            ..LogOptions::default()
        },
    )
    .expect_err("missing path");

    assert_eq!(error.code(), "entity_path_missing");
}

#[test]
fn undo_prepared_journal_entry_marks_it_reverted() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    store
        .append_journal(journal_entry("push-1", "page-1", JournalStatus::Prepared))
        .expect("append journal");

    let report = run_undo(&mut store, "push-1").expect("undo report");

    assert!(report.ok);
    assert_eq!(report.action, "reverted_local_journal");
    assert_eq!(report.status, "reverted");
    assert_eq!(undo_report_exit_code(&report), 0);
    assert_eq!(
        store
            .get_journal(&PushId("push-1".to_string()))
            .expect("get journal")
            .expect("journal")
            .status,
        JournalStatus::Reverted
    );
}

#[test]
fn undo_failed_journal_without_apply_effects_marks_it_reverted() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    store
        .append_journal(journal_entry(
            "push-1",
            "page-1",
            JournalStatus::Failed("remote changed before apply".to_string()),
        ))
        .expect("append journal");

    let report = run_undo(&mut store, "push-1").expect("undo report");

    assert!(report.ok);
    assert_eq!(report.action, "reverted_empty_failed_journal");
    assert_eq!(report.status, "reverted");
    assert_eq!(undo_report_exit_code(&report), 0);
    assert_eq!(
        store
            .get_journal(&PushId("push-1".to_string()))
            .expect("get journal")
            .expect("journal")
            .status,
        JournalStatus::Reverted
    );
}

#[test]
fn undo_reconciled_journal_entry_derives_reverse_plan_and_stops_before_apply() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    store
        .append_journal(journal_entry("push-1", "page-1", JournalStatus::Reconciled))
        .expect("append journal");

    let report = run_undo(&mut store, "push-1").expect("undo report");

    assert!(!report.ok);
    assert_eq!(report.action, "reverse_apply_not_implemented");
    assert_eq!(report.status, "reconciled");
    assert_eq!(
        report.undo_plan.as_ref().expect("undo plan").status,
        "complete"
    );
    assert_eq!(
        report
            .undo_plan
            .as_ref()
            .expect("undo plan")
            .operations
            .len(),
        1
    );
    assert_eq!(undo_report_exit_code(&report), 5);
    assert_eq!(
        store
            .get_journal(&PushId("push-1".to_string()))
            .expect("get journal")
            .expect("journal")
            .status,
        JournalStatus::Reconciled
    );
}

#[test]
fn undo_reports_blocked_plan_for_append_without_created_id() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    store
        .append_journal(journal_entry_with_operations(
            "push-1",
            "page-1",
            JournalStatus::Reconciled,
            vec![PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("page-1-paragraph-1")),
                content: "New paragraph.".to_string(),
            }],
        ))
        .expect("append journal");

    let report = run_undo(&mut store, "push-1").expect("undo report");

    assert!(!report.ok);
    assert_eq!(report.action, "undo_plan_blocked");
    let undo_plan = report.undo_plan.expect("undo plan");
    assert_eq!(undo_plan.status, "blocked");
    assert_eq!(
        undo_plan.unsupported[0].code,
        "append_block_missing_created_id"
    );
}

#[test]
fn undo_with_applier_reverses_complete_plan_and_marks_journal_reverted() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    store
        .append_journal(journal_entry("push-1", "page-1", JournalStatus::Reconciled))
        .expect("append journal");
    let mut applier = FakeUndoApplier::default();

    let report = run_undo_with_applier(&mut store, "push-1", &mut applier).expect("undo report");

    assert!(report.ok);
    assert_eq!(report.action, "reverse_applied");
    assert_eq!(report.changed_remote_ids, vec!["page-1"]);
    assert_eq!(applier.applied_push_ids, vec![PushId("push-1".to_string())]);
    assert_eq!(
        store
            .get_journal(&PushId("push-1".to_string()))
            .expect("get journal")
            .expect("journal")
            .status,
        JournalStatus::Reverted
    );
}

#[test]
fn undo_with_applier_rejects_target_that_is_not_latest_for_every_touched_entity() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    store
        .append_journal(journal_entry("push-1", "page-1", JournalStatus::Reconciled))
        .expect("append target journal");
    store
        .append_journal(journal_entry("push-2", "page-1", JournalStatus::Reconciled))
        .expect("append later journal");
    let mut applier = FakeUndoApplier::default();

    let error = run_undo_with_applier(&mut store, "push-1", &mut applier)
        .expect_err("later journal must block undo");

    assert_eq!(error.code(), "undo_not_latest");
    assert!(error.message().contains("push-2"), "{}", error.message());
    assert!(applier.applied_push_ids.is_empty());
    assert_eq!(
        store
            .get_journal(&PushId("push-1".to_string()))
            .expect("get journal")
            .expect("journal")
            .status,
        JournalStatus::Reconciled
    );
}

#[test]
fn undo_latest_check_covers_disjoint_affected_preimage_and_entity_operation_ids() {
    for later_id in ["affected-only", "preimage-only", "created-only"] {
        let fixture = HistoryFixture::new();
        let mut store = fixture.store();
        let operation = PushOperation::CreateEntity {
            parent_id: RemoteId::new("affected-only"),
            parent_kind: Some(EntityKind::Page),
            parent_workspace: false,
            title: "Created".to_string(),
            properties: Default::default(),
            body: String::new(),
            source_path: PathBuf::from("Created/page.md"),
        };
        let push_id = PushId("push-disjoint".to_string());
        let operation_id = PushOperationId::for_operation(&push_id, 0, &operation);
        store
            .append_journal(
                JournalEntry::new(
                    push_id.clone(),
                    fixture.mount_id.clone(),
                    vec![RemoteId::new("affected-only")],
                    PushPlan::new(vec![RemoteId::new("affected-only")], vec![operation]),
                    JournalStatus::Reconciled,
                )
                .with_preimages(vec![JournalPreimage::from_shadow(shadow("preimage-only"))])
                .with_apply_effects(vec![JournalApplyEffect::CreatedEntity {
                    operation_id,
                    operation_index: 0,
                    parent_id: RemoteId::new("affected-only"),
                    entity_id: RemoteId::new("created-only"),
                }]),
            )
            .expect("append target journal");
        store
            .append_journal(journal_entry(
                "push-z-later",
                later_id,
                JournalStatus::Reconciled,
            ))
            .expect("append later journal");
        let mut applier = FakeUndoApplier::default();

        let error = run_undo_with_applier(&mut store, push_id.0, &mut applier)
            .expect_err("later touched id must block undo");

        assert_eq!(error.code(), "undo_not_latest", "{later_id}");
        assert!(applier.applied_push_ids.is_empty(), "{later_id}");
    }
}

#[test]
fn undo_with_applier_ignores_later_reverted_journal_for_latest_check() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    store
        .append_journal(journal_entry("push-1", "page-1", JournalStatus::Reconciled))
        .expect("append target journal");
    store
        .append_journal(journal_entry("push-2", "page-1", JournalStatus::Reverted))
        .expect("append reverted journal");
    let mut applier = FakeUndoApplier::default();

    let report = run_undo_with_applier(&mut store, "push-1", &mut applier)
        .expect("reverted later journal does not block");

    assert!(report.ok);
    assert_eq!(applier.applied_push_ids, vec![PushId("push-1".to_string())]);
}

#[test]
fn undo_with_applier_requires_changed_id_for_block_preimage() {
    assert_incomplete_apply_result_is_rejected(journal_entry(
        "push-1",
        "page-1",
        JournalStatus::Reconciled,
    ));
}

#[test]
fn undo_with_applier_requires_changed_id_for_body_preimage() {
    assert_incomplete_apply_result_is_rejected(journal_entry_with_operations(
        "push-1",
        "page-1",
        JournalStatus::Reconciled,
        vec![PushOperation::UpdateEntityBody {
            entity_id: RemoteId::new("page-1"),
            body: "Updated body.".to_string(),
        }],
    ));
}

#[test]
fn undo_with_applier_requires_changed_id_for_property_preimage() {
    let mut entry = journal_entry_with_operations(
        "push-1",
        "page-1",
        JournalStatus::Reconciled,
        vec![PushOperation::UpdateProperties {
            entity_id: RemoteId::new("page-1"),
            properties: std::collections::BTreeMap::from([(
                "Status".to_string(),
                locality_core::planner::PropertyValue::String("Done".to_string()),
            )]),
        }],
    );
    entry.preimages = vec![JournalPreimage::from_shadow(
        shadow("page-1").with_frontmatter(
            "loc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\nStatus: Todo\n",
        ),
    )];

    assert_incomplete_apply_result_is_rejected(entry);
}

#[test]
fn undo_with_applier_requires_changed_id_for_entity_operation() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    seed_created_entity_undo(&fixture, &mut store);
    let mut applier = FakeUndoApplier::default().with_changed_remote_ids(Vec::new());

    let error = run_undo_with_applier(&mut store, "push-create", &mut applier)
        .expect_err("missing created entity id must fail closed");

    assert_eq!(error.code(), "incomplete_undo_apply_result");
    assert!(error.message().contains("created-page-1"));
    assert_eq!(
        store
            .get_journal(&PushId("push-create".to_string()))
            .expect("get journal")
            .expect("journal")
            .status,
        JournalStatus::Reconciled
    );
}

#[test]
fn undo_with_applier_restores_local_projection_from_preimage() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    let pushed_body = "# Roadmap\n\nUpdated paragraph.";
    fs::write(
        fixture.root.join("Roadmap.md"),
        canonical_markdown("page-1", pushed_body),
    )
    .expect("write pushed projection");
    store
        .save_shadow(
            &fixture.mount_id,
            shadow_with_frontmatter("page-1", pushed_body, "Roadmap", None),
        )
        .expect("save pushed shadow");
    store
        .save_entity(
            entity_record(&fixture, "page-1", "Roadmap.md").with_content_hash(
                shadow_with_frontmatter("page-1", pushed_body, "Roadmap", None).body_hash,
            ),
        )
        .expect("save pushed entity");
    store
        .append_journal(journal_entry("push-1", "page-1", JournalStatus::Reconciled))
        .expect("append journal");
    let mut applier = FakeUndoApplier::default();

    let report = run_undo_with_applier(&mut store, "push-1", &mut applier).expect("undo report");

    assert!(report.ok);
    assert_eq!(report.action, "reverse_applied");
    let restored = fs::read_to_string(fixture.root.join("Roadmap.md")).expect("restored file");
    assert_eq!(
        restored,
        render_canonical_markdown(&CanonicalDocument::new(
            "loc:\n  id: page-1\n  type: page\ntitle: Roadmap\n",
            "# Roadmap\n\nOriginal paragraph.",
        ))
    );
    let shadow = store
        .load_shadow(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("restored shadow");
    assert_eq!(shadow.rendered_body, "# Roadmap\n\nOriginal paragraph.");
}

#[test]
fn undo_with_applier_reconciles_restored_entity_location_from_observation() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    let old_parent = EntityRecord::new(
        fixture.mount_id.clone(),
        RemoteId::new("team-old"),
        EntityKind::Directory,
        "Old team",
        "teams/old",
    );
    store.save_entity(old_parent).expect("save old parent");
    let current_path = PathBuf::from("teams/new/ENG-42 New title.md");
    fs::create_dir_all(fixture.root.join("teams/new")).expect("create current parent");
    fs::write(
        fixture.root.join(&current_path),
        "---\nloc:\n  id: page-1\n  type: page\n  parent: team-new\n  synced_at: now\n  remote_edited_at: now\ntitle: New title\n---\nNew body",
    )
    .expect("write current projection");
    let current_shadow = single_block_shadow("page-1", "New body").with_frontmatter(
        "loc:\n  id: page-1\n  type: page\n  parent: team-new\n  synced_at: now\n  remote_edited_at: now\ntitle: New title\n",
    );
    store
        .save_shadow(&fixture.mount_id, current_shadow.clone())
        .expect("save moved shadow");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "New title",
                current_path.clone(),
            )
            .with_hydration(HydrationState::Hydrated)
            .with_content_hash(current_shadow.body_hash),
        )
        .expect("save moved entity");
    let preimage = shadow_with_body("page-1", "# Roadmap\n\nOriginal body").with_frontmatter(
        "loc:\n  id: page-1\n  type: page\n  parent: team-old\n  synced_at: now\n  remote_edited_at: now\ntitle: Old title\n",
    );
    let entry = JournalEntry::new(
        PushId("push-move".to_string()),
        fixture.mount_id.clone(),
        vec![RemoteId::new("page-1")],
        PushPlan::new(
            vec![RemoteId::new("page-1")],
            vec![PushOperation::MoveEntity {
                entity_id: RemoteId::new("page-1"),
                new_parent_id: RemoteId::new("team-new"),
                new_parent_kind: EntityKind::Directory,
                new_title: "New title".to_string(),
                projected_path: current_path.clone(),
            }],
        ),
        JournalStatus::Reconciled,
    )
    .with_preimages(vec![JournalPreimage::from_shadow(preimage)]);
    store.append_journal(entry).expect("append move journal");
    let observation = RemoteObservation::new(
        fixture.mount_id.clone(),
        RemoteId::new("page-1"),
        EntityKind::Page,
        "Old title",
        "ENG-42 Old title.md",
    )
    .with_parent(RemoteId::new("team-old"));
    let mut applier = FakeUndoApplier::default().with_observations(vec![observation]);

    let report = run_undo_with_applier(&mut store, "push-move", &mut applier).expect("undo move");

    assert!(report.ok);
    let restored_path = PathBuf::from("teams/old/ENG-42 Old title.md");
    assert!(!fixture.root.join(current_path).exists());
    assert!(fixture.root.join(&restored_path).exists());
    let restored = fs::read_to_string(fixture.root.join(&restored_path)).expect("restored file");
    assert_eq!(
        restored,
        render_canonical_markdown(&CanonicalDocument::new(
            "loc:\n  id: page-1\n  type: page\n  parent: team-old\n  synced_at: now\n  remote_edited_at: now\ntitle: Old title\n",
            "# Roadmap\n\nOriginal body",
        ))
    );
    let entity = store
        .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("read entity")
        .expect("entity");
    assert_eq!(entity.title, "Old title");
    assert_eq!(entity.path, restored_path);
}

#[test]
fn undo_with_applier_accepts_notion_page_container_observation_path() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    let current_path = seed_move_undo(&fixture, &mut store);
    let observation = RemoteObservation::new(
        fixture.mount_id.clone(),
        RemoteId::new("page-1"),
        EntityKind::Page,
        "Old title",
        "Old title/page.md",
    )
    .with_parent(RemoteId::new("team-old"));
    let mut applier = FakeUndoApplier::default().with_observations(vec![observation]);

    let report = run_undo_with_applier(&mut store, "push-move", &mut applier)
        .expect("undo Notion-shaped move");

    assert!(report.ok);
    let restored_path = PathBuf::from("teams/old/Old title/page.md");
    assert!(!fixture.root.join(current_path).exists());
    assert!(fixture.root.join(&restored_path).exists());
    assert_eq!(
        store
            .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
            .expect("read entity")
            .expect("entity")
            .path,
        restored_path
    );
}

#[test]
fn virtual_undo_move_relocates_cache_without_recording_local_mutation() {
    let fixture = HistoryFixture::new();
    let mut store = InMemoryStateStore::new();
    let (visible_root, state_root, current_path) =
        seed_virtual_move_undo(&fixture, &mut store, ProjectionMode::LinuxFuse);
    let restored_path = PathBuf::from("teams/old/ENG-42 Old title.md");
    let observation = RemoteObservation::new(
        fixture.mount_id.clone(),
        RemoteId::new("page-1"),
        EntityKind::Page,
        "Old title",
        "ENG-42 Old title.md",
    )
    .with_parent(RemoteId::new("team-old"));
    let mut applier = FakeUndoApplier::default().with_observations(vec![observation]);

    let report = run_undo_with_applier_at_state_root(
        &mut store,
        "push-move",
        &mut applier,
        Some(&state_root),
    )
    .expect("undo virtual move");

    assert!(report.ok);
    let old_cache = virtual_fs_content_path(&state_root, &fixture.mount_id, &current_path)
        .expect("old cache path");
    let restored_cache = virtual_fs_content_path(&state_root, &fixture.mount_id, &restored_path)
        .expect("restored cache path");
    assert!(!old_cache.exists());
    let cached = fs::read_to_string(restored_cache).expect("restored cache");
    assert_eq!(
        cached,
        render_canonical_markdown(&CanonicalDocument::new(
            "loc:\n  id: page-1\n  type: page\n  parent: team-old\n  synced_at: now\n  remote_edited_at: now\ntitle: Old title\n",
            "# Roadmap\n\nOriginal body",
        ))
    );
    assert!(
        store
            .list_virtual_mutations(&fixture.mount_id)
            .expect("list virtual mutations")
            .is_empty(),
        "undo reconciliation must not replay as a local move"
    );
    assert_eq!(
        store
            .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
            .expect("read entity")
            .expect("entity")
            .path,
        restored_path
    );
    assert!(!visible_root.join(&current_path).exists());
}

#[test]
fn windows_cloud_files_undo_move_dematerializes_when_destination_parent_is_missing() {
    let fixture = HistoryFixture::new();
    let mut store = InMemoryStateStore::new();
    let (visible_root, state_root, current_path) =
        seed_virtual_move_undo(&fixture, &mut store, ProjectionMode::WindowsCloudFiles);
    let restored_path = PathBuf::from("teams/old/ENG-42 Old title.md");
    let expected = render_canonical_markdown(&CanonicalDocument::new(
        "loc:\n  id: page-1\n  type: page\n  parent: team-old\n  synced_at: now\n  remote_edited_at: now\ntitle: Old title\n",
        "# Roadmap\n\nOriginal body",
    ));
    let observation = RemoteObservation::new(
        fixture.mount_id.clone(),
        RemoteId::new("page-1"),
        EntityKind::Page,
        "Old title",
        "ENG-42 Old title.md",
    )
    .with_parent(RemoteId::new("team-old"));
    let mut applier = FakeUndoApplier::default().with_observations(vec![observation]);

    let report = run_undo_with_applier_at_state_root(
        &mut store,
        "push-move",
        &mut applier,
        Some(&state_root),
    )
    .expect("undo Windows Cloud Files move");

    assert!(report.ok);
    assert!(!visible_root.join(&current_path).exists());
    assert!(!visible_root.join(&restored_path).exists());
    assert!(!visible_root.join("teams/old").exists());
    let old_cache = virtual_fs_content_path(&state_root, &fixture.mount_id, &current_path)
        .expect("old cache path");
    let restored_cache = virtual_fs_content_path(&state_root, &fixture.mount_id, &restored_path)
        .expect("restored cache path");
    assert!(!old_cache.exists());
    assert_eq!(
        fs::read_to_string(restored_cache).expect("read restored cache"),
        expected
    );
    let recoveries = list_windows_cloud_files_projection_recoveries(&state_root)
        .expect("list projection recoveries");
    assert_eq!(recoveries.len(), 1);
    assert_eq!(
        recoveries[0].status,
        WindowsCloudFilesProjectionRecoveryStatus::QuarantinedClean
    );
    assert!(recoveries[0].quarantine_path.is_file());
    assert!(
        store
            .list_virtual_mutations(&fixture.mount_id)
            .expect("list virtual mutations")
            .is_empty(),
        "provider reconciliation must not replay as a local move"
    );
    let entity = store
        .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("read restored entity")
        .expect("restored entity");
    let wrapped_identifier =
        localityd::virtual_projection::wrap_identifier(&fixture.mount_id, "page-1");
    assert!(!consume_windows_cloud_files_projection_acknowledgement(
        &state_root,
        &visible_root,
        &fixture.mount_id,
        &RemoteId::new("page-1"),
        &wrapped_identifier,
        &restored_path,
        WindowsCloudFilesProjectionEvent::CloudFilesRenameTarget,
        Some(&entity),
    ));
    for (event, observed_quarantine_path) in [
        (
            WindowsCloudFilesProjectionEvent::CloudFilesQuarantineMoveSource,
            Some(recoveries[0].quarantine_path.as_path()),
        ),
        (
            WindowsCloudFilesProjectionEvent::WatcherQuarantineMoveSource,
            None,
        ),
    ] {
        assert!(consume_windows_cloud_files_quarantine_acknowledgement(
            &state_root,
            &visible_root,
            &fixture.mount_id,
            &RemoteId::new("page-1"),
            &wrapped_identifier,
            &current_path,
            event,
            Some(&entity),
            observed_quarantine_path,
        ));
    }
}

#[test]
fn windows_cloud_files_undo_move_dematerializes_when_destination_parent_exists() {
    let fixture = HistoryFixture::new();
    let mut store = InMemoryStateStore::new();
    let (visible_root, state_root, current_path) =
        seed_virtual_move_undo(&fixture, &mut store, ProjectionMode::WindowsCloudFiles);
    let restored_path = PathBuf::from("teams/old/ENG-42 Old title.md");
    fs::create_dir_all(visible_root.join("teams/old")).expect("create existing destination parent");
    let expected = render_canonical_markdown(&CanonicalDocument::new(
        "loc:\n  id: page-1\n  type: page\n  parent: team-old\n  synced_at: now\n  remote_edited_at: now\ntitle: Old title\n",
        "# Roadmap\n\nOriginal body",
    ));
    let observation = RemoteObservation::new(
        fixture.mount_id.clone(),
        RemoteId::new("page-1"),
        EntityKind::Page,
        "Old title",
        "ENG-42 Old title.md",
    )
    .with_parent(RemoteId::new("team-old"));
    let mut applier = FakeUndoApplier::default().with_observations(vec![observation]);

    let report = run_undo_with_applier_at_state_root(
        &mut store,
        "push-move",
        &mut applier,
        Some(&state_root),
    )
    .expect("undo Windows Cloud Files move");

    assert!(report.ok);
    assert!(!visible_root.join(&current_path).exists());
    assert!(!visible_root.join(&restored_path).exists());
    assert!(visible_root.join("teams/old").is_dir());
    let restored_cache = virtual_fs_content_path(&state_root, &fixture.mount_id, &restored_path)
        .expect("restored cache path");
    assert_eq!(
        fs::read_to_string(restored_cache).expect("read restored cache"),
        expected
    );
    assert!(
        store
            .list_virtual_mutations(&fixture.mount_id)
            .expect("list virtual mutations")
            .is_empty(),
        "provider reconciliation must not replay as a local move"
    );
    let entity = store
        .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("read restored entity")
        .expect("restored entity");
    let wrapped_identifier =
        localityd::virtual_projection::wrap_identifier(&fixture.mount_id, "page-1");
    assert!(!consume_windows_cloud_files_projection_acknowledgement(
        &state_root,
        &visible_root,
        &fixture.mount_id,
        &RemoteId::new("page-1"),
        &wrapped_identifier,
        &restored_path,
        WindowsCloudFilesProjectionEvent::CloudFilesRenameTarget,
        Some(&entity),
    ));
    let recoveries = list_windows_cloud_files_projection_recoveries(&state_root)
        .expect("list projection recoveries");
    assert_eq!(recoveries.len(), 1);
    assert!(recoveries[0].quarantine_path.is_file());
    for (event, observed_quarantine_path) in [
        (
            WindowsCloudFilesProjectionEvent::CloudFilesQuarantineMoveSource,
            Some(recoveries[0].quarantine_path.as_path()),
        ),
        (
            WindowsCloudFilesProjectionEvent::WatcherQuarantineMoveSource,
            None,
        ),
    ] {
        assert!(consume_windows_cloud_files_quarantine_acknowledgement(
            &state_root,
            &visible_root,
            &fixture.mount_id,
            &RemoteId::new("page-1"),
            &wrapped_identifier,
            &current_path,
            event,
            Some(&entity),
            observed_quarantine_path,
        ));
    }
}

#[test]
fn windows_cloud_files_undo_move_preserves_nonempty_page_container() {
    let fixture = HistoryFixture::new();
    let mut store = InMemoryStateStore::new();
    let current_path = PathBuf::from("teams/new/New title/page.md");
    let (visible_root, state_root, current_path) = seed_virtual_move_undo_at(
        &fixture,
        &mut store,
        ProjectionMode::WindowsCloudFiles,
        current_path,
    );
    let current_container = visible_root.join(current_path.parent().expect("current container"));
    let untracked_path = current_container.join("local-notes.md");
    fs::write(&untracked_path, "keep this local content").expect("write untracked local content");
    let restored_path = PathBuf::from("teams/old/Old title/page.md");
    let observation = RemoteObservation::new(
        fixture.mount_id.clone(),
        RemoteId::new("page-1"),
        EntityKind::Page,
        "Old title",
        "Old title/page.md",
    )
    .with_parent(RemoteId::new("team-old"));
    let mut applier = FakeUndoApplier::default().with_observations(vec![observation]);

    run_undo_with_applier_at_state_root(&mut store, "push-move", &mut applier, Some(&state_root))
        .expect_err("nonempty page container must not be recursively dematerialized");

    assert!(current_container.exists());
    assert!(visible_root.join(&current_path).exists());
    assert_eq!(
        fs::read_to_string(untracked_path).expect("read preserved local content"),
        "keep this local content"
    );
    assert!(!visible_root.join(&restored_path).exists());
    assert!(!visible_root.join("teams/old").exists());
    assert!(
        store
            .list_virtual_mutations(&fixture.mount_id)
            .expect("list virtual mutations")
            .is_empty()
    );
    let recoveries = list_windows_cloud_files_projection_recoveries(&state_root)
        .expect("list projection recoveries");
    assert!(recoveries.is_empty());
    let entity = store
        .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("read restored entity")
        .expect("restored entity");
    let wrapped_file_identifier =
        localityd::virtual_projection::wrap_identifier(&fixture.mount_id, "page-1");
    let wrapped_container_identifier =
        localityd::virtual_projection::wrap_identifier(&fixture.mount_id, "children:page-1");
    for (identifier, path) in [
        (wrapped_file_identifier.as_str(), current_path.as_path()),
        (
            wrapped_container_identifier.as_str(),
            current_path.parent().expect("current relative container"),
        ),
    ] {
        for (event, observed_quarantine_path) in [
            (
                WindowsCloudFilesProjectionEvent::CloudFilesQuarantineMoveSource,
                Some(state_root.join("missing-quarantine")),
            ),
            (
                WindowsCloudFilesProjectionEvent::WatcherQuarantineMoveSource,
                None,
            ),
        ] {
            assert!(!consume_windows_cloud_files_quarantine_acknowledgement(
                &state_root,
                &visible_root,
                &fixture.mount_id,
                &RemoteId::new("page-1"),
                identifier,
                path,
                event,
                Some(&entity),
                observed_quarantine_path.as_deref(),
            ));
        }
    }
}

#[test]
fn windows_cloud_files_undo_move_dematerializes_empty_page_container() {
    let fixture = HistoryFixture::new();
    let mut store = InMemoryStateStore::new();
    let current_path = PathBuf::from("teams/new/New title/page.md");
    let (visible_root, state_root, current_path) = seed_virtual_move_undo_at(
        &fixture,
        &mut store,
        ProjectionMode::WindowsCloudFiles,
        current_path,
    );
    let current_container = visible_root.join(current_path.parent().expect("current container"));
    let restored_path = PathBuf::from("teams/old/Old title/page.md");
    let observation = RemoteObservation::new(
        fixture.mount_id.clone(),
        RemoteId::new("page-1"),
        EntityKind::Page,
        "Old title",
        "Old title/page.md",
    )
    .with_parent(RemoteId::new("team-old"));
    let mut applier = FakeUndoApplier::default().with_observations(vec![observation]);

    run_undo_with_applier_at_state_root(&mut store, "push-move", &mut applier, Some(&state_root))
        .expect("dematerialize empty page container");

    assert!(!current_container.exists());
    assert!(!visible_root.join(&restored_path).exists());
    assert!(!visible_root.join("teams/old").exists());
    assert!(
        store
            .list_virtual_mutations(&fixture.mount_id)
            .expect("list virtual mutations")
            .is_empty()
    );
    let recoveries = list_windows_cloud_files_projection_recoveries(&state_root)
        .expect("list projection recoveries");
    assert_eq!(recoveries.len(), 1);
    assert_eq!(
        recoveries[0].status,
        WindowsCloudFilesProjectionRecoveryStatus::QuarantinedClean
    );
    assert!(recoveries[0].quarantine_path.is_dir());
    assert!(recoveries[0].quarantine_path.join("page.md").is_file());
    let entity = store
        .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("read restored entity")
        .expect("restored entity");
    let wrapped_file_identifier =
        localityd::virtual_projection::wrap_identifier(&fixture.mount_id, "page-1");
    let wrapped_container_identifier =
        localityd::virtual_projection::wrap_identifier(&fixture.mount_id, "children:page-1");
    for (identifier, path, quarantine_path) in [
        (
            wrapped_file_identifier.as_str(),
            current_path.as_path(),
            recoveries[0].quarantine_path.join("page.md"),
        ),
        (
            wrapped_container_identifier.as_str(),
            current_path.parent().expect("current relative container"),
            recoveries[0].quarantine_path.clone(),
        ),
    ] {
        for (event, observed_quarantine_path) in [
            (
                WindowsCloudFilesProjectionEvent::CloudFilesQuarantineMoveSource,
                Some(quarantine_path.as_path()),
            ),
            (
                WindowsCloudFilesProjectionEvent::WatcherQuarantineMoveSource,
                None,
            ),
        ] {
            assert!(consume_windows_cloud_files_quarantine_acknowledgement(
                &state_root,
                &visible_root,
                &fixture.mount_id,
                &RemoteId::new("page-1"),
                identifier,
                path,
                event,
                Some(&entity),
                observed_quarantine_path,
            ));
        }
    }
}

#[test]
fn virtual_undo_move_preflights_backing_and_visible_destination_collisions() {
    for collision in ["backing", "visible"] {
        let fixture = HistoryFixture::new();
        let mut store = InMemoryStateStore::new();
        let (visible_root, state_root, current_path) =
            seed_virtual_move_undo(&fixture, &mut store, ProjectionMode::WindowsCloudFiles);
        let restored_path = PathBuf::from("teams/old/ENG-42 Old title.md");
        let collision_path = if collision == "backing" {
            virtual_fs_content_path(&state_root, &fixture.mount_id, &restored_path)
                .expect("collision cache path")
        } else {
            visible_root.join(&restored_path)
        };
        fs::create_dir_all(collision_path.parent().expect("collision parent"))
            .expect("create collision parent");
        fs::write(&collision_path, format!("{collision} collision")).expect("write collision");
        let observation = RemoteObservation::new(
            fixture.mount_id.clone(),
            RemoteId::new("page-1"),
            EntityKind::Page,
            "Old title",
            "ENG-42 Old title.md",
        )
        .with_parent(RemoteId::new("team-old"));
        let mut applier = FakeUndoApplier::default().with_observations(vec![observation]);

        let error = run_undo_with_applier_at_state_root(
            &mut store,
            "push-move",
            &mut applier,
            Some(&state_root),
        )
        .expect_err("destination collision must fail closed");

        assert_eq!(error.code(), "invalid_undo_observation", "{collision}");
        assert_eq!(
            fs::read_to_string(&collision_path).expect("read collision"),
            format!("{collision} collision")
        );
        assert!(visible_root.join(&current_path).exists(), "{collision}");
        assert_eq!(
            store
                .get_journal(&PushId("push-move".to_string()))
                .expect("get journal")
                .expect("journal")
                .status,
            JournalStatus::Reconciled,
            "{collision}"
        );
    }
}

#[test]
fn undo_with_applier_cleans_only_empty_previous_page_containers() {
    for has_child in [false, true] {
        let fixture = HistoryFixture::new();
        let mut store = fixture.store();
        let current_path = seed_move_undo_at(
            &fixture,
            &mut store,
            PathBuf::from("teams/new/New title/page.md"),
        );
        let previous_container = fixture
            .root
            .join(current_path.parent().expect("page container"));
        if has_child {
            fs::write(previous_container.join("child.md"), "child").expect("write child");
        }
        let observation = RemoteObservation::new(
            fixture.mount_id.clone(),
            RemoteId::new("page-1"),
            EntityKind::Page,
            "Old title",
            "Old title/page.md",
        )
        .with_parent(RemoteId::new("team-old"));
        let mut applier = FakeUndoApplier::default().with_observations(vec![observation]);

        run_undo_with_applier(&mut store, "push-move", &mut applier).expect("undo page move");

        assert_eq!(previous_container.exists(), has_child);
        if has_child {
            assert!(previous_container.join("child.md").exists());
        }
    }
}

#[test]
fn undo_with_applier_rejects_missing_or_mismatched_move_observations() {
    for case in [
        "missing",
        "wrong_mount",
        "wrong_entity",
        "wrong_parent",
        "wrong_title",
        "wrong_kind",
        "deleted",
        "wrong_path",
        "deep_path",
    ] {
        let fixture = HistoryFixture::new();
        let mut store = fixture.store();
        let current_path = seed_move_undo(&fixture, &mut store);
        let mut observation = RemoteObservation::new(
            fixture.mount_id.clone(),
            RemoteId::new("page-1"),
            EntityKind::Page,
            "Old title",
            "ENG-42 Old title.md",
        )
        .with_parent(RemoteId::new("team-old"));
        let observations = match case {
            "missing" => vec![],
            "wrong_mount" => {
                observation.mount_id = MountId::new("other-mount");
                vec![observation]
            }
            "wrong_entity" => {
                observation.remote_id = RemoteId::new("other-page");
                vec![observation]
            }
            "wrong_parent" => {
                observation.parent_remote_id = Some(RemoteId::new("team-new"));
                vec![observation]
            }
            "wrong_title" => {
                observation.title = "New title".to_string();
                vec![observation]
            }
            "wrong_kind" => {
                observation.kind = EntityKind::Directory;
                vec![observation]
            }
            "deleted" => vec![observation.deleted(true)],
            "wrong_path" => {
                observation.projected_path = "teams/other/ENG-42 Old title.md".into();
                vec![observation]
            }
            "deep_path" => {
                observation.projected_path = "teams/old/nested/deeper/page.md".into();
                vec![observation]
            }
            _ => unreachable!(),
        };
        let mut applier = FakeUndoApplier::default().with_observations(observations);

        let error = run_undo_with_applier(&mut store, "push-move", &mut applier)
            .expect_err("invalid move observation must fail closed");

        assert_eq!(error.code(), "invalid_undo_observation", "case {case}");
        assert!(fixture.root.join(&current_path).exists(), "case {case}");
        assert_eq!(
            store
                .get_journal(&PushId("push-move".to_string()))
                .expect("get journal")
                .expect("journal")
                .status,
            JournalStatus::Reconciled,
            "case {case}"
        );
    }
}

#[test]
fn undo_with_applier_rejects_observation_path_owned_by_another_entity() {
    for indexed in [false, true] {
        let fixture = HistoryFixture::new();
        let mut store = fixture.store();
        let current_path = seed_move_undo(&fixture, &mut store);
        let collision_path = PathBuf::from("teams/old/ENG-42 Old title.md");
        fs::create_dir_all(
            fixture
                .root
                .join(collision_path.parent().expect("collision parent")),
        )
        .expect("create collision parent");
        fs::write(fixture.root.join(&collision_path), "unrelated contents")
            .expect("write collision");
        if indexed {
            store
                .save_entity(EntityRecord::new(
                    fixture.mount_id.clone(),
                    RemoteId::new("other-page"),
                    EntityKind::Page,
                    "Old title",
                    collision_path.clone(),
                ))
                .expect("save collision entity");
        }
        let observation = RemoteObservation::new(
            fixture.mount_id.clone(),
            RemoteId::new("page-1"),
            EntityKind::Page,
            "Old title",
            "ENG-42 Old title.md",
        )
        .with_parent(RemoteId::new("team-old"));
        let mut applier = FakeUndoApplier::default().with_observations(vec![observation]);

        let error = run_undo_with_applier(&mut store, "push-move", &mut applier)
            .expect_err("path collision must fail closed");

        assert_eq!(error.code(), "invalid_undo_observation");
        assert_eq!(
            fs::read_to_string(fixture.root.join(collision_path)).expect("read collision"),
            "unrelated contents"
        );
        assert!(fixture.root.join(current_path).exists());
    }
}

#[test]
fn undo_with_applier_recreates_archived_entity_from_observation_and_preimage() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    seed_archived_parent(&fixture, &mut store);
    let entry = archived_entity_journal(&fixture);
    store
        .delete_entity(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("delete archived entity record");
    fs::remove_file(fixture.root.join("Roadmap.md")).expect("remove archived projection");
    store.append_journal(entry).expect("append archive journal");
    let observation = RemoteObservation::new(
        fixture.mount_id.clone(),
        RemoteId::new("page-1"),
        EntityKind::Page,
        "Roadmap",
        "Roadmap.md",
    )
    .with_parent(RemoteId::new("archive-root"));
    let mut applier = FakeUndoApplier::default().with_observations(vec![observation]);

    let report = run_undo_with_applier(&mut store, "push-archive", &mut applier)
        .expect("undo archived entity");

    assert!(report.ok);
    let entity = store
        .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("read entity")
        .expect("restored entity");
    assert_eq!(entity.path, PathBuf::from("Roadmap.md"));
    assert_eq!(entity.hydration, HydrationState::Hydrated);
    let contents =
        fs::read_to_string(fixture.root.join("Roadmap.md")).expect("restored archived projection");
    assert_eq!(
        contents,
        render_canonical_markdown(&CanonicalDocument::new(
            "loc:\n  id: page-1\n  type: page\n  parent: archive-root\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n",
            "Original archived body.",
        ))
    );
}

#[test]
fn undo_with_applier_requires_observation_to_restore_archived_entity() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    seed_archived_parent(&fixture, &mut store);
    let entry = archived_entity_journal(&fixture);
    store
        .delete_entity(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("delete archived entity record");
    fs::remove_file(fixture.root.join("Roadmap.md")).expect("remove archived projection");
    store.append_journal(entry).expect("append archive journal");
    let mut applier = FakeUndoApplier::default();

    let error = run_undo_with_applier(&mut store, "push-archive", &mut applier)
        .expect_err("missing restore observation must fail closed");

    assert_eq!(error.code(), "invalid_undo_observation");
    assert!(
        store
            .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
            .expect("read entity")
            .is_none()
    );
    assert_eq!(
        store
            .get_journal(&PushId("push-archive".to_string()))
            .expect("get journal")
            .expect("journal")
            .status,
        JournalStatus::Reconciled
    );
}

#[test]
fn undo_restore_archived_entity_blocks_pending_target_mutation_before_remote_apply() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    seed_archived_parent(&fixture, &mut store);
    let entry = archived_entity_journal(&fixture);
    store
        .delete_entity(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("delete archived entity record");
    fs::remove_file(fixture.root.join("Roadmap.md")).expect("remove archived projection");
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: fixture.mount_id.clone(),
            local_id: "move:page-1".to_string(),
            mutation_kind: VirtualMutationKind::Move,
            target_remote_id: Some(RemoteId::new("page-1")),
            parent_remote_id: Some(RemoteId::new("archive-root")),
            original_path: Some(PathBuf::from("Old Roadmap.md")),
            projected_path: PathBuf::from("Roadmap.md"),
            title: "Roadmap".to_string(),
            content_path: None,
            created_at: "now".to_string(),
            updated_at: "now".to_string(),
        })
        .expect("save pending mutation");
    store.append_journal(entry).expect("append archive journal");
    let mut applier = FakeUndoApplier::default();

    let error = run_undo_with_applier(&mut store, "push-archive", &mut applier)
        .expect_err("pending archived-entity mutation must block undo");

    assert_eq!(error.code(), "unsafe_undo_local_state");
    assert!(applier.applied_push_ids.is_empty());
}

#[test]
fn undo_with_applier_archives_clean_created_entity_and_removes_projection() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    let created_path = seed_created_entity_undo(&fixture, &mut store);
    let observation = created_entity_observation(&fixture).deleted(true);
    let mut applier = FakeUndoApplier::default()
        .with_changed_remote_ids(vec![RemoteId::new("created-page-1")])
        .with_observations(vec![observation]);

    let report = run_undo_with_applier(&mut store, "push-create", &mut applier)
        .expect("undo created entity");

    assert!(report.ok);
    assert_eq!(report.changed_remote_ids, vec!["created-page-1"]);
    assert!(!fixture.root.join(&created_path).exists());
    assert!(
        store
            .get_entity(&fixture.mount_id, &RemoteId::new("created-page-1"))
            .expect("read entity")
            .is_none()
    );
}

#[test]
fn windows_cloud_files_undo_archive_removes_clean_visible_projection_without_local_mutation() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    let created_path = seed_created_entity_undo(&fixture, &mut store);
    let visible_root = fixture.root.join("provider/notion-main");
    let visible_path = visible_root.join(&created_path);
    fs::create_dir_all(visible_path.parent().expect("visible parent"))
        .expect("create visible parent");
    fs::rename(fixture.root.join(&created_path), &visible_path)
        .expect("move created projection into provider root");
    let mut mount = store
        .get_mount(&fixture.mount_id)
        .expect("get mount")
        .expect("mount");
    mount.root = visible_root.clone();
    mount.projection = ProjectionMode::WindowsCloudFiles;
    store.save_mount(mount).expect("save virtual mount");
    let state_root = fixture.root.join("state");
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, &created_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("create cache parent");
    fs::copy(&visible_path, &cache_path).expect("seed cache");
    let observation = created_entity_observation(&fixture).deleted(true);
    let mut applier = FakeUndoApplier::default()
        .with_changed_remote_ids(vec![RemoteId::new("created-page-1")])
        .with_observations(vec![observation]);

    let report = run_undo_with_applier_at_state_root(
        &mut store,
        "push-create",
        &mut applier,
        Some(&state_root),
    )
    .expect("undo virtual created entity");

    assert!(report.ok);
    assert!(!cache_path.exists());
    assert!(!visible_path.exists());
    assert!(
        store
            .list_virtual_mutations(&fixture.mount_id)
            .expect("list virtual mutations")
            .is_empty()
    );
    let recoveries = list_windows_cloud_files_projection_recoveries(&state_root)
        .expect("list projection recoveries");
    assert_eq!(recoveries.len(), 1);
    assert_eq!(
        recoveries[0].status,
        WindowsCloudFilesProjectionRecoveryStatus::QuarantinedClean
    );
    assert!(recoveries[0].quarantine_path.join("page.md").is_file());
    let wrapped_file_identifier =
        localityd::virtual_projection::wrap_identifier(&fixture.mount_id, "created-page-1");
    let wrapped_container_identifier = localityd::virtual_projection::wrap_identifier(
        &fixture.mount_id,
        "children:created-page-1",
    );
    for (identifier, path, event, observed_quarantine_path) in [
        (
            wrapped_file_identifier.as_str(),
            created_path.as_path(),
            WindowsCloudFilesProjectionEvent::CloudFilesQuarantineArchiveSource,
            Some(recoveries[0].quarantine_path.join("page.md")),
        ),
        (
            wrapped_file_identifier.as_str(),
            created_path.as_path(),
            WindowsCloudFilesProjectionEvent::WatcherQuarantineArchiveSource,
            None,
        ),
        (
            wrapped_container_identifier.as_str(),
            created_path.parent().expect("created page container"),
            WindowsCloudFilesProjectionEvent::CloudFilesQuarantineArchiveSource,
            Some(recoveries[0].quarantine_path.clone()),
        ),
        (
            wrapped_container_identifier.as_str(),
            created_path.parent().expect("created page container"),
            WindowsCloudFilesProjectionEvent::WatcherQuarantineArchiveSource,
            None,
        ),
    ] {
        assert!(consume_windows_cloud_files_quarantine_acknowledgement(
            &state_root,
            &visible_root,
            &fixture.mount_id,
            &RemoteId::new("created-page-1"),
            identifier,
            path,
            event,
            None,
            observed_quarantine_path.as_deref(),
        ));
    }
}

#[test]
fn windows_cloud_files_undo_archive_preserves_nonempty_page_container() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    let created_path = seed_created_entity_undo(&fixture, &mut store);
    let visible_root = fixture.root.join("provider/notion-main");
    let visible_path = visible_root.join(&created_path);
    fs::create_dir_all(visible_path.parent().expect("visible parent"))
        .expect("create visible parent");
    fs::rename(fixture.root.join(&created_path), &visible_path)
        .expect("move created projection into provider root");
    let untracked_path = visible_path
        .parent()
        .expect("page container")
        .join("local-notes.md");
    fs::write(&untracked_path, "keep archive notes").expect("write untracked notes");
    let mut mount = store
        .get_mount(&fixture.mount_id)
        .expect("get mount")
        .expect("mount");
    mount.root = visible_root;
    mount.projection = ProjectionMode::WindowsCloudFiles;
    store.save_mount(mount).expect("save virtual mount");
    let state_root = fixture.root.join("state");
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, &created_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("create cache parent");
    fs::copy(&visible_path, &cache_path).expect("seed cache");
    let observation = created_entity_observation(&fixture).deleted(true);
    let mut applier = FakeUndoApplier::default()
        .with_changed_remote_ids(vec![RemoteId::new("created-page-1")])
        .with_observations(vec![observation]);

    run_undo_with_applier_at_state_root(&mut store, "push-create", &mut applier, Some(&state_root))
        .expect_err("nonempty archived page container must fail closed");

    assert!(visible_path.is_file());
    assert_eq!(
        fs::read_to_string(untracked_path).expect("read untracked notes"),
        "keep archive notes"
    );
    assert!(
        list_windows_cloud_files_projection_recoveries(&state_root)
            .expect("list projection recoveries")
            .is_empty()
    );
}

#[test]
fn undo_with_applier_requires_absent_parent_for_workspace_created_entity() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    seed_workspace_created_entity_undo(&fixture, &mut store);
    let observation = RemoteObservation::new(
        fixture.mount_id.clone(),
        RemoteId::new("workspace-page-1"),
        EntityKind::Page,
        "Workspace page",
        "Workspace page/page.md",
    )
    .with_parent(RemoteId::new("unexpected-parent"))
    .deleted(true);
    let mut applier = FakeUndoApplier::default()
        .with_changed_remote_ids(vec![RemoteId::new("workspace-page-1")])
        .with_observations(vec![observation]);

    let error = run_undo_with_applier(&mut store, "push-workspace-create", &mut applier)
        .expect_err("workspace entity observation must have no parent");

    assert_eq!(error.code(), "invalid_undo_observation");
    assert!(error.message().contains("expected parent"));
}

#[test]
fn undo_with_applier_preflights_created_entity_local_work_before_remote_archive() {
    for case in ["dirty", "conflicted", "diverged"] {
        let fixture = HistoryFixture::new();
        let mut store = fixture.store();
        let created_path = seed_created_entity_undo(&fixture, &mut store);
        if case == "diverged" {
            fs::write(
                fixture.root.join(&created_path),
                created_entity_markdown("Locally changed body."),
            )
            .expect("write local edit");
        } else {
            let mut entity = store
                .get_entity(&fixture.mount_id, &RemoteId::new("created-page-1"))
                .expect("read created entity")
                .expect("created entity");
            entity.hydration = if case == "dirty" {
                HydrationState::Dirty
            } else {
                HydrationState::Conflicted
            };
            store.save_entity(entity).expect("save unsafe entity state");
        }
        let mut applier = FakeUndoApplier::default()
            .with_changed_remote_ids(vec![RemoteId::new("created-page-1")])
            .with_observations(vec![created_entity_observation(&fixture).deleted(true)]);

        let error = run_undo_with_applier(&mut store, "push-create", &mut applier)
            .expect_err("local work must block remote create undo");

        assert_eq!(error.code(), "unsafe_undo_local_state", "case {case}");
        assert!(applier.applied_push_ids.is_empty(), "case {case}");
        assert!(fixture.root.join(created_path).exists(), "case {case}");
        assert_eq!(
            store
                .get_journal(&PushId("push-create".to_string()))
                .expect("get journal")
                .expect("journal")
                .status,
            JournalStatus::Reconciled,
            "case {case}"
        );
    }
}

#[test]
fn undo_with_applier_preflights_local_work_before_restoring_any_preimage() {
    for case in ["dirty", "hash_mismatch", "diverged"] {
        let fixture = HistoryFixture::new();
        let mut store = fixture.store();
        let pushed_body = "# Roadmap\n\nUpdated paragraph.";
        let pushed_shadow = shadow_with_body("page-1", pushed_body);
        fs::write(
            fixture.root.join("Roadmap.md"),
            canonical_markdown("page-1", pushed_body),
        )
        .expect("write pushed projection");
        store
            .save_shadow(&fixture.mount_id, pushed_shadow.clone())
            .expect("save pushed shadow");
        let mut entity = entity_record(&fixture, "page-1", "Roadmap.md")
            .with_content_hash(pushed_shadow.body_hash.clone());
        match case {
            "dirty" => entity.hydration = HydrationState::Dirty,
            "hash_mismatch" => entity.content_hash = Some("different-shadow".to_string()),
            "diverged" => fs::write(
                fixture.root.join("Roadmap.md"),
                canonical_markdown("page-1", "Locally changed after push."),
            )
            .expect("write local divergence"),
            _ => unreachable!(),
        }
        store.save_entity(entity).expect("save current entity");
        store
            .append_journal(journal_entry("push-1", "page-1", JournalStatus::Reconciled))
            .expect("append journal");
        let mut applier = FakeUndoApplier::default();

        let error = run_undo_with_applier(&mut store, "push-1", &mut applier)
            .expect_err("local work must block preimage restoration");

        assert_eq!(error.code(), "unsafe_undo_local_state", "case {case}");
        assert!(applier.applied_push_ids.is_empty(), "case {case}");
        assert_eq!(
            store
                .get_journal(&PushId("push-1".to_string()))
                .expect("get journal")
                .expect("journal")
                .status,
            JournalStatus::Reconciled,
            "case {case}"
        );
    }
}

#[test]
fn undo_with_applier_requires_deleted_observation_for_created_entity() {
    for observations in [
        vec![],
        vec![created_entity_observation(&HistoryFixture::new())],
    ] {
        let fixture = HistoryFixture::new();
        let mut store = fixture.store();
        let created_path = seed_created_entity_undo(&fixture, &mut store);
        let observations = observations
            .into_iter()
            .map(|mut observation| {
                observation.mount_id = fixture.mount_id.clone();
                observation
            })
            .collect();
        let mut applier = FakeUndoApplier::default()
            .with_changed_remote_ids(vec![RemoteId::new("created-page-1")])
            .with_observations(observations);

        let error = run_undo_with_applier(&mut store, "push-create", &mut applier)
            .expect_err("created entity must be observed deleted");

        assert_eq!(error.code(), "invalid_undo_observation");
        assert!(fixture.root.join(created_path).exists());
    }
}

#[test]
fn undo_with_applier_reports_reverse_apply_failure_without_status_change() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    store
        .append_journal(journal_entry("push-1", "page-1", JournalStatus::Reconciled))
        .expect("append journal");
    let mut applier = FakeUndoApplier::default()
        .with_failure(LocalityError::NotImplemented("fake reverse apply"));

    let report = run_undo_with_applier(&mut store, "push-1", &mut applier).expect("undo report");

    assert!(!report.ok);
    assert_eq!(report.action, "reverse_apply_not_implemented");
    assert_eq!(report.message, "not implemented: fake reverse apply");
    assert_eq!(
        store
            .get_journal(&PushId("push-1".to_string()))
            .expect("get journal")
            .expect("journal")
            .status,
        JournalStatus::Reconciled
    );
}

#[test]
fn undo_reports_missing_journal() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();

    let error = run_undo(&mut store, "missing-push").expect_err("missing journal");

    assert_eq!(
        error,
        HistoryError::JournalNotFound(PushId("missing-push".to_string()))
    );
    assert_eq!(error.code(), "journal_not_found");
}

#[test]
fn log_and_undo_work_with_sqlite_state_store() {
    let fixture = HistoryFixture::new();
    let mut store = SqliteStateStore::open(fixture.root.join(".state")).expect("open sqlite");
    seed_store(&mut store, &fixture);
    store
        .append_journal(journal_entry("push-1", "page-1", JournalStatus::Prepared))
        .expect("append journal");

    let log_report = run_log(&store, LogOptions::default()).expect("log report");
    assert_eq!(log_report.entries.len(), 1);
    assert_eq!(log_report.entries[0].push_id, "push-1");

    let undo_report = run_undo(&mut store, "push-1").expect("undo report");
    assert!(undo_report.ok);
    assert_eq!(
        store
            .get_journal(&PushId("push-1".to_string()))
            .expect("get journal")
            .expect("journal")
            .status,
        JournalStatus::Reverted
    );
}

struct HistoryFixture {
    root: PathBuf,
    mount_id: MountId,
}

impl HistoryFixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "loc-cli-history-{}-{unique}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("fixture root");
        fs::write(root.join("Roadmap.md"), "").expect("roadmap file");

        Self {
            root,
            mount_id: MountId::new("notion-main"),
        }
    }

    fn store(&self) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        seed_store(&mut store, self);
        store
    }
}

impl Drop for HistoryFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Debug, Default)]
struct FakeUndoApplier {
    applied_push_ids: Vec<PushId>,
    failure: Option<LocalityError>,
    changed_remote_ids: Option<Vec<RemoteId>>,
    observations: Vec<RemoteObservation>,
}

impl FakeUndoApplier {
    fn with_failure(mut self, failure: LocalityError) -> Self {
        self.failure = Some(failure);
        self
    }

    fn with_observations(mut self, observations: Vec<RemoteObservation>) -> Self {
        self.observations = observations;
        self
    }

    fn with_changed_remote_ids(mut self, changed_remote_ids: Vec<RemoteId>) -> Self {
        self.changed_remote_ids = Some(changed_remote_ids);
        self
    }
}

impl UndoApplier for FakeUndoApplier {
    fn apply_undo(&mut self, request: UndoApplyRequest<'_>) -> LocalityResult<UndoApplyResult> {
        self.applied_push_ids.push(request.target_push_id.clone());

        match &self.failure {
            Some(error) => Err(error.clone()),
            None => Ok(UndoApplyResult {
                changed_remote_ids: self
                    .changed_remote_ids
                    .clone()
                    .unwrap_or_else(|| request.plan.affected_entities.clone()),
                observations: self.observations.clone(),
            }),
        }
    }
}

fn assert_incomplete_apply_result_is_rejected(entry: JournalEntry) {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    let push_id = entry.push_id.clone();
    store.append_journal(entry).expect("append journal");
    let mut applier = FakeUndoApplier::default().with_changed_remote_ids(Vec::new());

    let error = run_undo_with_applier(&mut store, push_id.0.clone(), &mut applier)
        .expect_err("incomplete apply result must fail closed");

    assert_eq!(error.code(), "incomplete_undo_apply_result");
    assert!(error.message().contains("page-1"), "{}", error.message());
    assert_eq!(
        store
            .get_journal(&push_id)
            .expect("get journal")
            .expect("journal")
            .status,
        JournalStatus::Reconciled
    );
}

fn seed_store<S>(store: &mut S, fixture: &HistoryFixture)
where
    S: MountRepository + EntityRepository + ShadowRepository,
{
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "notion",
            fixture.root.clone(),
        ))
        .expect("save mount");
    let current_shadow =
        shadow_with_frontmatter("page-1", "# Roadmap\n\nUpdated paragraph.", "Roadmap", None);
    fs::write(
        fixture.root.join("Roadmap.md"),
        canonical_markdown("page-1", "# Roadmap\n\nUpdated paragraph."),
    )
    .expect("write seeded projection");
    store
        .save_shadow(&fixture.mount_id, current_shadow.clone())
        .expect("save seeded shadow");
    store
        .save_entity(
            entity_record(fixture, "page-1", "Roadmap.md")
                .with_content_hash(current_shadow.body_hash),
        )
        .expect("save entity");
}

fn entity_record(fixture: &HistoryFixture, remote_id: &str, path: &str) -> EntityRecord {
    EntityRecord::new(
        fixture.mount_id.clone(),
        RemoteId::new(remote_id),
        EntityKind::Page,
        "Roadmap",
        path,
    )
    .with_hydration(HydrationState::Hydrated)
}

fn seed_move_undo(fixture: &HistoryFixture, store: &mut InMemoryStateStore) -> PathBuf {
    seed_move_undo_at(
        fixture,
        store,
        PathBuf::from("teams/new/ENG-42 New title.md"),
    )
}

fn seed_move_undo_at(
    fixture: &HistoryFixture,
    store: &mut InMemoryStateStore,
    current_path: PathBuf,
) -> PathBuf {
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("team-old"),
            EntityKind::Directory,
            "Old team",
            "teams/old",
        ))
        .expect("save old parent");
    fs::create_dir_all(
        fixture
            .root
            .join(current_path.parent().expect("current parent")),
    )
    .expect("create current parent");
    fs::write(
        fixture.root.join(&current_path),
        "---\nloc:\n  id: page-1\n  type: page\n  parent: team-new\n  synced_at: now\n  remote_edited_at: now\ntitle: New title\n---\nNew body",
    )
    .expect("write current projection");
    let current_shadow = single_block_shadow("page-1", "New body").with_frontmatter(
        "loc:\n  id: page-1\n  type: page\n  parent: team-new\n  synced_at: now\n  remote_edited_at: now\ntitle: New title\n",
    );
    store
        .save_shadow(&fixture.mount_id, current_shadow.clone())
        .expect("save moved shadow");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "New title",
                current_path.clone(),
            )
            .with_hydration(HydrationState::Hydrated)
            .with_content_hash(current_shadow.body_hash),
        )
        .expect("save moved entity");
    let preimage = shadow_with_body("page-1", "# Roadmap\n\nOriginal body").with_frontmatter(
        "loc:\n  id: page-1\n  type: page\n  parent: team-old\n  synced_at: now\n  remote_edited_at: now\ntitle: Old title\n",
    );
    let entry = JournalEntry::new(
        PushId("push-move".to_string()),
        fixture.mount_id.clone(),
        vec![RemoteId::new("page-1")],
        PushPlan::new(
            vec![RemoteId::new("page-1")],
            vec![PushOperation::MoveEntity {
                entity_id: RemoteId::new("page-1"),
                new_parent_id: RemoteId::new("team-new"),
                new_parent_kind: EntityKind::Directory,
                new_title: "New title".to_string(),
                projected_path: current_path.clone(),
            }],
        ),
        JournalStatus::Reconciled,
    )
    .with_preimages(vec![JournalPreimage::from_shadow(preimage)]);
    store.append_journal(entry).expect("append move journal");
    current_path
}

fn seed_virtual_move_undo(
    fixture: &HistoryFixture,
    store: &mut InMemoryStateStore,
    projection: ProjectionMode,
) -> (PathBuf, PathBuf, PathBuf) {
    seed_virtual_move_undo_at(
        fixture,
        store,
        projection,
        PathBuf::from("teams/new/ENG-42 New title.md"),
    )
}

fn seed_virtual_move_undo_at(
    fixture: &HistoryFixture,
    store: &mut InMemoryStateStore,
    projection: ProjectionMode,
    current_path: PathBuf,
) -> (PathBuf, PathBuf, PathBuf) {
    let materialize_visible = projection == ProjectionMode::WindowsCloudFiles;
    let visible_root = fixture.root.join("provider").join("notion-main");
    let state_root = fixture.root.join("state");
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", visible_root.clone())
                .projection(projection),
        )
        .expect("save virtual mount");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("team-old"),
            EntityKind::Directory,
            "Old team",
            "teams/old",
        ))
        .expect("save old parent");
    let current_shadow = single_block_shadow("page-1", "New body").with_frontmatter(
        "loc:\n  id: page-1\n  type: page\n  parent: team-new\n  synced_at: now\n  remote_edited_at: now\ntitle: New title\n",
    );
    let current_contents = render_canonical_markdown(&CanonicalDocument::new(
        current_shadow.frontmatter.clone(),
        current_shadow.rendered_body.clone(),
    ));
    if materialize_visible {
        let visible_path = visible_root.join(&current_path);
        fs::create_dir_all(visible_path.parent().expect("visible parent"))
            .expect("create visible parent");
        fs::write(&visible_path, &current_contents).expect("write visible projection");
    }
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, &current_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("create cache parent");
    fs::write(&cache_path, current_contents).expect("write cached projection");
    store
        .save_shadow(&fixture.mount_id, current_shadow.clone())
        .expect("save current shadow");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "New title",
                current_path.clone(),
            )
            .with_hydration(HydrationState::Hydrated)
            .with_content_hash(current_shadow.body_hash),
        )
        .expect("save current entity");
    let preimage = shadow_with_body("page-1", "# Roadmap\n\nOriginal body").with_frontmatter(
        "loc:\n  id: page-1\n  type: page\n  parent: team-old\n  synced_at: now\n  remote_edited_at: now\ntitle: Old title\n",
    );
    store
        .append_journal(
            JournalEntry::new(
                PushId("push-move".to_string()),
                fixture.mount_id.clone(),
                vec![RemoteId::new("page-1")],
                PushPlan::new(
                    vec![RemoteId::new("page-1")],
                    vec![PushOperation::MoveEntity {
                        entity_id: RemoteId::new("page-1"),
                        new_parent_id: RemoteId::new("team-new"),
                        new_parent_kind: EntityKind::Directory,
                        new_title: "New title".to_string(),
                        projected_path: current_path.clone(),
                    }],
                ),
                JournalStatus::Reconciled,
            )
            .with_preimages(vec![JournalPreimage::from_shadow(preimage)]),
        )
        .expect("append move journal");

    (visible_root, state_root, current_path)
}

fn archived_entity_journal(fixture: &HistoryFixture) -> JournalEntry {
    let preimage = single_block_shadow("page-1", "Original archived body.").with_frontmatter(
        "loc:\n  id: page-1\n  type: page\n  parent: archive-root\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n",
    );
    JournalEntry::new(
        PushId("push-archive".to_string()),
        fixture.mount_id.clone(),
        vec![RemoteId::new("page-1")],
        PushPlan::new(
            vec![RemoteId::new("page-1")],
            vec![PushOperation::ArchiveEntity {
                entity_id: RemoteId::new("page-1"),
            }],
        ),
        JournalStatus::Reconciled,
    )
    .with_preimages(vec![JournalPreimage::from_shadow(preimage)])
}

fn seed_archived_parent(fixture: &HistoryFixture, store: &mut InMemoryStateStore) {
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("archive-root"),
            EntityKind::Directory,
            "Archive root",
            "",
        ))
        .expect("save archive parent");
}

fn seed_created_entity_undo(fixture: &HistoryFixture, store: &mut InMemoryStateStore) -> PathBuf {
    let created_path = PathBuf::from("Roadmap/New child/page.md");
    fs::create_dir_all(
        fixture
            .root
            .join(created_path.parent().expect("created parent")),
    )
    .expect("create projection directory");
    fs::write(
        fixture.root.join(&created_path),
        created_entity_markdown("Created child."),
    )
    .expect("write created projection");
    let shadow = single_block_shadow("created-page-1", "Created child.").with_frontmatter(
        "loc:\n  id: created-page-1\n  type: page\n  parent: page-1\n  synced_at: now\n  remote_edited_at: now\ntitle: New child\n",
    );
    store
        .save_shadow(&fixture.mount_id, shadow.clone())
        .expect("save created shadow");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("created-page-1"),
                EntityKind::Page,
                "New child",
                created_path.clone(),
            )
            .with_hydration(HydrationState::Hydrated)
            .with_content_hash(shadow.body_hash),
        )
        .expect("save created entity");
    let operation = PushOperation::CreateEntity {
        parent_id: RemoteId::new("page-1"),
        parent_kind: Some(EntityKind::Page),
        parent_workspace: false,
        title: "New child".to_string(),
        properties: Default::default(),
        body: "Created child.".to_string(),
        source_path: created_path.clone(),
    };
    let push_id = PushId("push-create".to_string());
    let operation_id = PushOperationId::for_operation(&push_id, 0, &operation);
    let entry = JournalEntry::new(
        push_id.clone(),
        fixture.mount_id.clone(),
        vec![RemoteId::new("page-1")],
        PushPlan::new(vec![RemoteId::new("page-1")], vec![operation]),
        JournalStatus::Reconciled,
    )
    .with_apply_effects(vec![JournalApplyEffect::CreatedEntity {
        operation_id,
        operation_index: 0,
        parent_id: RemoteId::new("page-1"),
        entity_id: RemoteId::new("created-page-1"),
    }]);
    store.append_journal(entry).expect("append create journal");
    created_path
}

fn seed_workspace_created_entity_undo(fixture: &HistoryFixture, store: &mut InMemoryStateStore) {
    let created_path = PathBuf::from("Workspace page/page.md");
    fs::create_dir_all(
        fixture
            .root
            .join(created_path.parent().expect("created parent")),
    )
    .expect("create workspace projection directory");
    let shadow = single_block_shadow("workspace-page-1", "Created body.").with_frontmatter(
        "loc:\n  id: workspace-page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Workspace page\n",
    );
    fs::write(
        fixture.root.join(&created_path),
        render_canonical_markdown(&CanonicalDocument::new(
            shadow.frontmatter.clone(),
            shadow.rendered_body.clone(),
        )),
    )
    .expect("write workspace projection");
    store
        .save_shadow(&fixture.mount_id, shadow.clone())
        .expect("save workspace shadow");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("workspace-page-1"),
                EntityKind::Page,
                "Workspace page",
                created_path.clone(),
            )
            .with_hydration(HydrationState::Hydrated)
            .with_content_hash(shadow.body_hash),
        )
        .expect("save workspace entity");
    let operation = PushOperation::CreateEntity {
        parent_id: RemoteId::new("workspace"),
        parent_kind: None,
        parent_workspace: true,
        title: "Workspace page".to_string(),
        properties: Default::default(),
        body: "Created body.".to_string(),
        source_path: created_path,
    };
    let push_id = PushId("push-workspace-create".to_string());
    let operation_id = PushOperationId::for_operation(&push_id, 0, &operation);
    store
        .append_journal(
            JournalEntry::new(
                push_id,
                fixture.mount_id.clone(),
                Vec::new(),
                PushPlan::new(Vec::new(), vec![operation]),
                JournalStatus::Reconciled,
            )
            .with_apply_effects(vec![JournalApplyEffect::CreatedEntity {
                operation_id,
                operation_index: 0,
                parent_id: RemoteId::new("workspace"),
                entity_id: RemoteId::new("workspace-page-1"),
            }]),
        )
        .expect("append workspace create journal");
}

fn created_entity_observation(fixture: &HistoryFixture) -> RemoteObservation {
    RemoteObservation::new(
        fixture.mount_id.clone(),
        RemoteId::new("created-page-1"),
        EntityKind::Page,
        "New child",
        "New child/page.md",
    )
    .with_parent(RemoteId::new("page-1"))
}

fn created_entity_markdown(body: &str) -> String {
    format!(
        "---\nloc:\n  id: created-page-1\n  type: page\n  parent: page-1\n  synced_at: now\n  remote_edited_at: now\ntitle: New child\n---\n{body}"
    )
}

fn journal_entry(push_id: &str, remote_id: &str, status: JournalStatus) -> JournalEntry {
    journal_entry_with_operations(
        push_id,
        remote_id,
        status,
        vec![PushOperation::UpdateBlock {
            block_id: RemoteId::new(format!("{remote_id}-paragraph-1")),
            content: "Updated paragraph.".to_string(),
        }],
    )
}

fn journal_entry_with_operations(
    push_id: &str,
    remote_id: &str,
    status: JournalStatus,
    operations: Vec<PushOperation>,
) -> JournalEntry {
    JournalEntry::new(
        PushId(push_id.to_string()),
        MountId::new("notion-main"),
        vec![RemoteId::new(remote_id)],
        PushPlan::new(vec![RemoteId::new(remote_id)], operations),
        status,
    )
    .with_preimages(vec![JournalPreimage::from_shadow(shadow(remote_id))])
}

fn shadow(remote_id: &str) -> ShadowDocument {
    shadow_with_body(remote_id, "# Roadmap\n\nOriginal paragraph.")
}

fn shadow_with_body(remote_id: &str, body: &str) -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new(remote_id),
        body,
        9,
        [
            RemoteId::new(format!("{remote_id}-heading-1")),
            RemoteId::new(format!("{remote_id}-paragraph-1")),
        ],
    )
    .expect("shadow")
}

fn single_block_shadow(remote_id: &str, body: &str) -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new(remote_id),
        body,
        9,
        [RemoteId::new(format!("{remote_id}-paragraph-1"))],
    )
    .expect("single-block shadow")
}

fn shadow_with_frontmatter(
    remote_id: &str,
    body: &str,
    title: &str,
    parent_id: Option<&str>,
) -> ShadowDocument {
    let parent = parent_id
        .map(|parent_id| format!("  parent: {parent_id}\n"))
        .unwrap_or_default();
    shadow_with_body(remote_id, body).with_frontmatter(format!(
        "loc:\n  id: {remote_id}\n  type: page\n{parent}  synced_at: now\n  remote_edited_at: now\ntitle: {title}\n"
    ))
}

fn canonical_markdown(remote_id: &str, body: &str) -> String {
    format!(
        "---\nloc:\n  id: {remote_id}\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\n{body}"
    )
}
