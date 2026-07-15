use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use loc_cli::history::{
    HistoryError, LogOptions, run_log, run_undo, run_undo_with_applier, undo_report_exit_code,
};
use locality_core::freshness::RemoteObservation;
use locality_core::journal::{
    JournalApplyEffect, JournalEntry, JournalPreimage, JournalStatus, PushId, PushOperationId,
};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use locality_core::planner::{PushOperation, PushPlan};
use locality_core::shadow::ShadowDocument;
use locality_core::undo::{UndoApplier, UndoApplyRequest, UndoApplyResult};
use locality_core::{LocalityError, LocalityResult};
use locality_store::{
    EntityRecord, EntityRepository, InMemoryStateStore, JournalRepository, MountConfig,
    MountRepository, ShadowRepository, SqliteStateStore,
};

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
    assert!(restored.contains("# Roadmap\n\nOriginal paragraph."));
    assert!(!restored.contains("Updated paragraph."));
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
    assert!(restored.contains("Original body"));
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
        "deleted",
        "wrong_path",
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
            "deleted" => vec![observation.deleted(true)],
            "wrong_path" => {
                observation.projected_path = "teams/other/ENG-42 Old title.md".into();
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
    );
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
    assert!(contents.contains("Original archived body."));
}

#[test]
fn undo_with_applier_requires_observation_to_restore_archived_entity() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
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
    assert!(!fixture.root.join(created_path).exists());
    assert!(
        store
            .get_entity(&fixture.mount_id, &RemoteId::new("created-page-1"))
            .expect("read entity")
            .is_none()
    );
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

fn archived_entity_journal(fixture: &HistoryFixture) -> JournalEntry {
    let preimage = single_block_shadow("page-1", "Original archived body.").with_frontmatter(
        "loc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n",
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
