use std::cell::Cell;
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use loc_cli::push::{
    PushOptions, push_report_exit_code, run_push, run_push_with_daemon,
    run_push_with_daemon_at_state_root, select_push_targets,
};
use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, Connector,
    ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest, NativeEntity,
    ParsedEntity,
};
use locality_core::conflict::{
    CONFLICT_LOCAL_MARKER, CONFLICT_REMOTE_MARKER, CONFLICT_SEPARATOR_MARKER,
};
use locality_core::hydration::HydrationRequest;
use locality_core::journal::JournalApplyEffect;
use locality_core::model::{
    CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry,
};
use locality_core::planner::{GuardrailDecision, PushOperation, PushPlan};
use locality_core::push::{PushPipelineAction, PushPipelineResult, PushStage};
use locality_core::shadow::ShadowDocument;
use locality_core::validation::ValidationReport;
use locality_core::{LocalityError, LocalityResult};
use locality_store::{
    EntityRecord, EntityRepository, InMemoryStateStore, JournalRepository, MountConfig,
    MountRepository, ProjectionMode, ShadowRepository, SqliteStateStore, VirtualMutationKind,
    VirtualMutationRecord, VirtualMutationRepository,
};
use localityd::execution::PushJobReport;
use localityd::hydration::{HydratedEntity, HydrationSource};
use localityd::ipc::{DaemonRequest, DaemonResponse};
use localityd::push::PushJobAction;
use localityd::virtual_fs::{virtual_fs_content_path, virtual_projection_mount_point};
use serde_json::Value;

#[test]
fn push_noop_succeeds_without_apply() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nSame paragraph.");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nSame paragraph."))
        .expect("save shadow");

    let report = run_push(&store, &path, PushOptions::default()).expect("push report");

    assert!(report.ok);
    assert_eq!(report.action, "noop");
    assert_eq!(push_report_exit_code(&report), 0);
}

#[test]
fn push_safe_plan_requires_yes() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nChanged paragraph.");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nOld paragraph."))
        .expect("save shadow");

    let report = run_push(&store, &path, PushOptions::default()).expect("push report");

    assert!(!report.ok);
    assert_eq!(report.action, "confirm_plan");
    assert_eq!(report.pipeline_action, "confirm_plan");
    assert_eq!(push_report_exit_code(&report), 4);
}

#[test]
fn push_read_only_mount_blocks_write() {
    let fixture = PushFixture::new();
    let mut store = fixture.read_only_store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nChanged paragraph.");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nOld paragraph."))
        .expect("save shadow");

    let report = run_push(
        &store,
        &path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: true,
        },
    )
    .expect("push report");

    assert!(!report.ok);
    assert_eq!(report.action, "read_only_blocked");
    assert_eq!(push_report_exit_code(&report), 4);
}

#[test]
fn push_file_with_conflict_markers_requires_manual_resolution_first() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page(
        "Roadmap.md",
        &format!(
            "{CONFLICT_LOCAL_MARKER}\n# Roadmap\n\nLocal paragraph.\n{CONFLICT_SEPARATOR_MARKER}\n# Roadmap\n\nRemote paragraph.\n{CONFLICT_REMOTE_MARKER}\n"
        ),
    );
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nRemote paragraph."))
        .expect("save shadow");
    let conflicted = store
        .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("get entity")
        .expect("entity")
        .with_hydration(HydrationState::Conflicted);
    store
        .save_entity(conflicted)
        .expect("save conflicted entity");

    let report = run_push(
        &store,
        &path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push report");

    assert!(!report.ok);
    assert_eq!(report.action, "fix_validation");
    assert_eq!(report.validation[0].code, "unresolved_conflict_markers");
    assert_eq!(push_report_exit_code(&report), 3);
}

#[test]
fn push_resolved_conflicted_entity_can_plan_normally() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nResolved paragraph.");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nRemote paragraph."))
        .expect("save shadow");
    let conflicted = store
        .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("get entity")
        .expect("entity")
        .with_hydration(HydrationState::Conflicted);
    store
        .save_entity(conflicted)
        .expect("save conflicted entity");

    let report = run_push(
        &store,
        &path,
        PushOptions {
            assume_yes: false,
            confirm_dangerous: false,
        },
    )
    .expect("push report");

    assert!(!report.ok);
    assert_eq!(report.action, "confirm_plan");
    assert!(report.validation.is_empty());
}

#[test]
fn push_safe_plan_with_yes_stops_at_apply_boundary() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nChanged paragraph.");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nOld paragraph."))
        .expect("save shadow");

    let report = run_push(
        &store,
        &path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push report");

    assert!(!report.ok);
    assert_eq!(report.pipeline_action, "proceed_to_apply");
    assert_eq!(report.action, "apply_not_implemented");
    assert_eq!(push_report_exit_code(&report), 5);
}

#[test]
fn push_safe_plan_with_daemon_journals_applies_and_reconciles() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nChanged paragraph.");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nOld paragraph."))
        .expect("save shadow");
    let source = FakePushSource::with_remote_transition(
        rendered_entity("Old paragraph."),
        rendered_entity("Changed paragraph."),
    );

    let report = run_push_with_daemon(
        &mut store,
        &source,
        &path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push report");

    assert!(report.ok);
    assert_eq!(report.action, "reconciled");
    assert!(report.push_id.is_some());
    assert_eq!(report.journal_status.as_deref(), Some("reconciled"));
    assert_eq!(report.changed_remote_ids, vec!["page-1"]);
    assert_eq!(report.reconciled_remote_ids, vec!["page-1"]);
    assert_eq!(report.apply_effect_count, 1);
    assert_eq!(push_report_exit_code(&report), 0);

    let journal = store
        .list_journal()
        .expect("list journal")
        .pop()
        .expect("journal");
    assert_eq!(
        journal.status,
        locality_core::journal::JournalStatus::Reconciled
    );
    assert_eq!(journal.preimages.len(), 1);
    assert_eq!(journal.apply_effects.len(), 1);
    assert_eq!(source.checks.get(), 1);
    assert_eq!(source.applies.get(), 1);
}

#[test]
fn push_daemon_allows_equivalent_media_paths_in_synced_tree_guardrail() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let long_media_href = "/tmp/loc-content/notion-main/files/.loc/media/getting-started-3-new/image-fb3123d34d04464487428b0f2557e4a0.jpg";
    let stable_media_href =
        "../.loc/media/getting-started-3-new/image-fb3123d34d04464487428b0f2557e4a0.jpg";

    let synced_body = format!("# Roadmap\n\n![Image]({long_media_href})\n\nOld paragraph.");
    let edited_body = format!("# Roadmap\n\n![Image]({long_media_href})\n\nChanged paragraph.");
    let remote_before_apply =
        format!("# Roadmap\n\n![Image]({stable_media_href})\n\nOld paragraph.");
    let remote_after_apply =
        format!("# Roadmap\n\n![Image]({stable_media_href})\n\nChanged paragraph.");
    let path = fixture.write_page("Roadmap.md", &edited_body);
    store
        .save_shadow(&fixture.mount_id, shadow_with_blocks(&synced_body))
        .expect("save shadow");
    let source = FakePushSource::with_remote_transition(
        rendered_entity_with_image_block(&remote_before_apply),
        rendered_entity_with_image_block(&remote_after_apply),
    );

    let report = run_push_with_daemon(
        &mut store,
        &source,
        &path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push report");

    assert!(report.ok, "{report:#?}");
    assert_eq!(report.action, "reconciled");
    assert_eq!(source.checks.get(), 1);
    assert_eq!(source.applies.get(), 1);
}

#[test]
fn push_virtual_projection_direct_fallback_reconciles_content_cache() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join("state");
    let mount_root = fixture.root.join("loc");
    let mount = MountConfig::new(fixture.mount_id.clone(), "notion", &mount_root)
        .projection(ProjectionMode::LinuxFuse);
    let target_path = virtual_projection_mount_point(&mount).join("Roadmap.md");
    let content_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, Path::new("Roadmap.md"))
            .expect("content path");
    if let Some(parent) = content_path.parent() {
        fs::create_dir_all(parent).expect("content parent");
    }
    fs::write(
        &content_path,
        canonical_markdown("page-1", "# Roadmap\n\nChanged paragraph."),
    )
    .expect("content cache");

    let mut store = InMemoryStateStore::new();
    store.save_mount(mount).expect("save mount");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save entity");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nOld paragraph."))
        .expect("save shadow");
    let source = FakePushSource::with_remote_transition(
        rendered_entity("Old paragraph."),
        rendered_entity("Changed paragraph."),
    );

    let report = run_push_with_daemon_at_state_root(
        &mut store,
        &source,
        &target_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
        Some(&state_root),
    )
    .expect("push report");

    assert!(report.ok);
    assert_eq!(report.action, "reconciled");
    assert_eq!(source.applies.get(), 1);
    let reconciled = fs::read_to_string(&content_path).expect("reconciled content");
    assert!(reconciled.contains("Changed paragraph."));
    assert!(
        !target_path.exists(),
        "direct fallback should reconcile the virtual content cache, not the mounted projection path"
    );
}

#[test]
fn push_daemon_reports_connector_not_implemented_with_reverted_journal() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nChanged paragraph.");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nOld paragraph."))
        .expect("save shadow");
    let source = FakePushSource::with_remote(rendered_entity("Changed paragraph."))
        .with_concurrency_failure(LocalityError::NotImplemented("fake concurrency"));

    let report = run_push_with_daemon(
        &mut store,
        &source,
        &path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push report");

    assert!(!report.ok);
    assert_eq!(report.action, "apply_not_implemented");
    assert_eq!(report.journal_status.as_deref(), Some("reverted"));
    assert_eq!(report.apply_effect_count, 0);
    assert_eq!(push_report_exit_code(&report), 5);
    assert_eq!(
        store
            .list_journal()
            .expect("list journal")
            .pop()
            .expect("journal")
            .status,
        locality_core::journal::JournalStatus::Reverted
    );
}

#[test]
fn push_daemon_suggests_pull_when_remote_changed_since_last_sync() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nChanged paragraph.");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nOld paragraph."))
        .expect("save shadow");
    let source = FakePushSource::with_remote(rendered_entity("Changed paragraph."))
        .with_concurrency_failure(LocalityError::Guardrail(
            "remote entity `page-1` changed since last sync (expected remote_edited_at `old`, found `new`)"
                .to_string(),
        ));

    let report = run_push_with_daemon(
        &mut store,
        &source,
        &path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push report");

    assert!(!report.ok);
    assert_eq!(report.action, "apply_failed");
    let expected = format!(
        "run `loc pull {}` to update from remote, resolve any conflicts, then rerun `loc push {} -y`",
        path.display(),
        path.display()
    );
    assert_eq!(report.suggested_fix.as_deref(), Some(expected.as_str()));
}

#[test]
fn push_daemon_suggests_parent_pull_when_new_page_parent_changed_since_last_sync() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-parent"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap/page.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save page directory parent");
    let path = fixture.write_raw(
        "Roadmap/Draft/page.md",
        "---\ntitle: Draft\n---\n# Draft\n\nCreated locally.\n",
    );
    fixture.write_raw(
        "Roadmap/page.md",
        &canonical_markdown("page-parent", "# Roadmap\n\nParent body.\n"),
    );
    let source = FakePushSource::with_remote(rendered_entity("Changed paragraph."))
        .with_concurrency_failure(LocalityError::Guardrail(
            "remote entity `page-parent` changed since last sync (expected remote_edited_at `old`, found `new`)"
                .to_string(),
        ));

    let report = run_push_with_daemon(
        &mut store,
        &source,
        &path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push report");

    assert!(!report.ok);
    assert_eq!(report.action, "apply_failed");
    let parent_path = fixture.root.join("Roadmap").join("page.md");
    let expected = format!(
        "run `loc pull {}` to update the parent from remote, then rerun `loc push {} -y`",
        parent_path.display(),
        path.display()
    );
    assert_eq!(report.suggested_fix.as_deref(), Some(expected.as_str()));
}

#[test]
fn push_dangerous_plan_requires_confirm() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "");
    store
        .save_shadow(&fixture.mount_id, large_shadow())
        .expect("save shadow");

    let report = run_push(
        &store,
        &path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push report");

    assert!(!report.ok);
    assert_eq!(report.action, "confirm_dangerous_plan");
    assert_eq!(report.guardrail.decision, "confirm_required");
    assert_eq!(push_report_exit_code(&report), 4);
}

#[test]
fn push_confirmed_dangerous_plan_stops_at_apply_boundary() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "");
    store
        .save_shadow(&fixture.mount_id, large_shadow())
        .expect("save shadow");

    let report = run_push(
        &store,
        &path,
        PushOptions {
            assume_yes: false,
            confirm_dangerous: true,
        },
    )
    .expect("push report");

    assert!(!report.ok);
    assert_eq!(report.pipeline_action, "proceed_to_apply");
    assert_eq!(report.action, "apply_not_implemented");
}

#[test]
fn push_validation_failure_returns_fix_validation() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_raw(
        "Roadmap.md",
        "---\ntitle: Missing Locality\n---\n# Roadmap\n",
    );
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nSame paragraph."))
        .expect("save shadow");

    let report = run_push(&store, &path, PushOptions::default()).expect("push report");

    assert!(!report.ok);
    assert_eq!(report.action, "fix_validation");
    assert_eq!(report.validation[0].code, "frontmatter_missing_loc");
    assert_eq!(push_report_exit_code(&report), 3);
}

#[test]
fn push_runner_works_with_sqlite_state_store() {
    let fixture = PushFixture::new();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nChanged paragraph.");
    let mut store = SqliteStateStore::open(fixture.root.join(".state")).expect("open sqlite");
    seed_store(&mut store, &fixture, false);
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nOld paragraph."))
        .expect("save shadow");

    let report = run_push(
        &store,
        &path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push report");

    assert_eq!(report.pipeline_action, "proceed_to_apply");
    assert_eq!(report.action, "apply_not_implemented");
}

#[test]
fn push_directory_targets_only_pending_page_changes_under_scope() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let clean_path = fixture.write_raw(
        "Team/Clean.md",
        &canonical_markdown("page-clean", "# Clean\n\nSame paragraph."),
    );
    let dirty_path = fixture.write_raw(
        "Team/Dirty.md",
        &canonical_markdown("page-dirty", "# Dirty\n\nChanged paragraph."),
    );
    let outside_path = fixture.write_raw(
        "Other.md",
        &canonical_markdown("page-outside", "# Outside\n\nChanged paragraph."),
    );
    let pending_path = fixture.root.join("Team/Draft.md");

    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-clean"),
                EntityKind::Page,
                "Clean",
                "Team/Clean.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save clean entity");
    store
        .save_shadow(
            &fixture.mount_id,
            shadow_for("page-clean", "# Clean\n\nSame paragraph."),
        )
        .expect("save clean shadow");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-dirty"),
                EntityKind::Page,
                "Dirty",
                "Team/Dirty.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save dirty entity");
    store
        .save_shadow(
            &fixture.mount_id,
            shadow_for("page-dirty", "# Dirty\n\nOld paragraph."),
        )
        .expect("save dirty shadow");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-outside"),
                EntityKind::Page,
                "Outside",
                "Other.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save outside entity");
    store
        .save_shadow(
            &fixture.mount_id,
            shadow_for("page-outside", "# Outside\n\nOld paragraph."),
        )
        .expect("save outside shadow");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "create:draft",
            VirtualMutationKind::Create,
            "Team/Draft.md",
            "Draft",
        ))
        .expect("save pending create");

    let team_scope = fixture.root.join("Team");
    let selection = select_push_targets(&store, &team_scope, None).expect("select scoped targets");

    assert!(selection.scoped);
    assert_eq!(selection.requested_path, team_scope);
    assert_eq!(selection.targets, vec![dirty_path, pending_path]);
    assert!(clean_path.exists());
    assert!(outside_path.exists());
}

#[test]
fn push_json_preview_uses_cli_diff_plan_when_running_daemon_has_stale_planner() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let mut store = SqliteStateStore::open(state_root.clone()).expect("open sqlite");
    seed_store(&mut store, &fixture, false);
    let shadow_body = "# Roadmap\n\nIntro paragraph.\n\n```markdown\n---\n```\n\n# Focus\n\n---\n\nTail paragraph.";
    let edited_body = "# Roadmap\n\nIntro paragraph.\n\ntemp\n\n```markdown\n---\n```\n\n# Focus\n\n---\n\nTail paragraph.";
    store
        .save_shadow(&fixture.mount_id, shadow_with_ids(shadow_body, 6))
        .expect("save shadow");
    let path = fixture.write_page("Roadmap.md", edited_body);
    let stale_report = stale_daemon_push_report(&path, &fixture.mount_id);
    let (tcp_addr, daemon_requests) = start_fake_push_daemon(stale_report);

    let output = Command::new(env!("CARGO_BIN_EXE_loc"))
        .env("LOCALITY_STATE_DIR", &state_root)
        .env("LOCALITY_DAEMON_TCP_ADDR", tcp_addr)
        .env_remove("LOCALITY_DAEMON_DISABLE")
        .arg("push")
        .arg(&path)
        .arg("--json")
        .output()
        .expect("run loc push");
    assert!(
        !output.status.success(),
        "confirmation-required preview should use a non-zero exit code"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: Value = serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!(
            "failed to parse loc push JSON: {error}\nstdout:\n{}\nstderr:\n{}",
            stdout,
            String::from_utf8_lossy(&output.stderr)
        )
    });

    assert_eq!(value["via"], "cli");
    assert_eq!(value["action"], "confirm_plan");
    assert_eq!(value["plan"]["summary"]["blocks_created"], 1);
    assert_eq!(value["plan"]["summary"]["blocks_replaced"], 0);
    assert_eq!(value["plan"]["summary"]["blocks_archived"], 0);
    assert_eq!(value["plan"]["operations"][0]["type"], "append_block");
    assert_eq!(value["plan"]["operations"][0]["content"], "temp");
    assert!(
        daemon_requests
            .recv_timeout(Duration::from_secs(2))
            .is_err(),
        "push preview should not ask the daemon to plan"
    );
}

#[test]
fn approved_push_refuses_stale_daemon_plan_before_apply() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let mut store = SqliteStateStore::open(state_root.clone()).expect("open sqlite");
    seed_store(&mut store, &fixture, false);
    let shadow_body = "# Roadmap\n\nIntro paragraph.\n\n```markdown\n---\n```\n\n# Focus\n\n---\n\nTail paragraph.";
    let edited_body = "# Roadmap\n\nIntro paragraph.\n\ntemp\n\n```markdown\n---\n```\n\n# Focus\n\n---\n\nTail paragraph.";
    store
        .save_shadow(&fixture.mount_id, shadow_with_ids(shadow_body, 6))
        .expect("save shadow");
    let path = fixture.write_page("Roadmap.md", edited_body);
    let stale_report = stale_daemon_push_report(&path, &fixture.mount_id);
    let (tcp_addr, daemon_requests) = start_fake_push_daemon(stale_report);

    let output = Command::new(env!("CARGO_BIN_EXE_loc"))
        .env("LOCALITY_STATE_DIR", &state_root)
        .env("LOCALITY_DAEMON_TCP_ADDR", tcp_addr)
        .env_remove("LOCALITY_DAEMON_DISABLE")
        .arg("push")
        .arg(&path)
        .arg("-y")
        .arg("--json")
        .output()
        .expect("run loc push");
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: Value = serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!(
            "failed to parse loc push JSON: {error}\nstdout:\n{}\nstderr:\n{}",
            stdout,
            String::from_utf8_lossy(&output.stderr)
        )
    });

    assert_eq!(value["command"], "push");
    assert_eq!(value["code"], "daemon_plan_mismatch");
    assert!(
        value["message"]
            .as_str()
            .is_some_and(|message| message.contains("daemon push plan differs"))
    );
    let request = daemon_requests
        .recv_timeout(Duration::from_secs(2))
        .expect("approved push should ask daemon for a non-mutating preview");
    match request {
        DaemonRequest::Push {
            assume_yes,
            confirm_dangerous,
            ..
        } => {
            assert!(!assume_yes, "daemon preview must not be approved");
            assert!(
                !confirm_dangerous,
                "daemon preview must not confirm dangerous plans"
            );
        }
        request => panic!("unexpected daemon request {request:?}"),
    }
}

struct PushFixture {
    root: PathBuf,
    mount_id: MountId,
}

impl PushFixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "loc-cli-push-{}-{unique}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("fixture root");
        Self {
            root,
            mount_id: MountId::new("notion-main"),
        }
    }

    fn store(&self) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        seed_store(&mut store, self, false);
        store
    }

    fn read_only_store(&self) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        seed_store(&mut store, self, true);
        store
    }

    fn write_page(&self, relative_path: &str, body: &str) -> PathBuf {
        self.write_raw(relative_path, &canonical_markdown("page-1", body))
    }

    fn write_raw(&self, relative_path: &str, contents: &str) -> PathBuf {
        let path = self.root.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent");
        }
        fs::write(&path, contents).expect("fixture file");
        path
    }
}

impl Drop for PushFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Debug, Default)]
struct FakePushSource {
    remote_before_apply: Option<HydratedEntity>,
    remote_after_apply: Option<HydratedEntity>,
    checks: Cell<usize>,
    applies: Cell<usize>,
    concurrency_failure: Option<LocalityError>,
}

impl FakePushSource {
    fn with_remote(remote: HydratedEntity) -> Self {
        Self {
            remote_before_apply: Some(remote.clone()),
            remote_after_apply: Some(remote),
            checks: Cell::new(0),
            applies: Cell::new(0),
            concurrency_failure: None,
        }
    }

    fn with_remote_transition(
        remote_before_apply: HydratedEntity,
        remote_after_apply: HydratedEntity,
    ) -> Self {
        Self {
            remote_before_apply: Some(remote_before_apply),
            remote_after_apply: Some(remote_after_apply),
            checks: Cell::new(0),
            applies: Cell::new(0),
            concurrency_failure: None,
        }
    }

    fn with_concurrency_failure(mut self, failure: LocalityError) -> Self {
        self.concurrency_failure = Some(failure);
        self
    }
}

impl HydrationSource for FakePushSource {
    fn fetch_render(&self, request: &HydrationRequest) -> LocalityResult<HydratedEntity> {
        if request.remote_id != RemoteId::new("page-1") {
            return Err(LocalityError::InvalidState(
                "unexpected remote id".to_string(),
            ));
        }

        let remote = if self.applies.get() == 0 {
            self.remote_before_apply.clone()
        } else {
            self.remote_after_apply.clone()
        };
        remote.ok_or_else(|| LocalityError::InvalidState("missing remote fixture".to_string()))
    }
}

impl Connector for FakePushSource {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind("fake")
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_block_updates: true,
            supports_databases: false,
            supports_oauth: false,
            ..ConnectorCapabilities::default()
        }
    }

    fn enumerate(&self, _request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        Err(LocalityError::NotImplemented("fake enumerate"))
    }

    fn fetch(&self, _request: FetchRequest) -> LocalityResult<NativeEntity> {
        Err(LocalityError::NotImplemented("fake fetch"))
    }

    fn render(&self, _entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        Err(LocalityError::NotImplemented("fake render"))
    }

    fn parse(&self, _document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        Err(LocalityError::NotImplemented("fake parse"))
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        self.checks.set(self.checks.get() + 1);
        match &self.concurrency_failure {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    fn apply(&self, request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        self.applies.set(self.applies.get() + 1);
        Ok(ApplyPlanResult {
            changed_remote_ids: request.plan.affected_entities.clone(),
            effects: vec![JournalApplyEffect::UpdatedBlock {
                operation_id: request.operation_ids[0].clone(),
                operation_index: 0,
                block_id: RemoteId::new("paragraph-1"),
            }],
        })
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        Err(LocalityError::NotImplemented("fake undo"))
    }
}

fn rendered_entity(body: &str) -> HydratedEntity {
    let body = format!("# Roadmap\n\n{body}");
    rendered_entity_with_body(&body)
}

fn rendered_entity_with_body(body: &str) -> HydratedEntity {
    HydratedEntity {
        document: CanonicalDocument::new(
            "loc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n",
            body.to_string(),
        ),
        shadow: shadow(body),
        remote_edited_at: Some("2026-06-11T00:00:00Z".to_string()),
        assets: Vec::new(),
    }
}

fn rendered_entity_with_image_block(body: &str) -> HydratedEntity {
    HydratedEntity {
        document: CanonicalDocument::new(
            "loc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n",
            body.to_string(),
        ),
        shadow: shadow_with_blocks(body),
        remote_edited_at: Some("2026-06-11T00:00:00Z".to_string()),
        assets: Vec::new(),
    }
}

fn seed_store<S>(store: &mut S, fixture: &PushFixture, read_only: bool)
where
    S: MountRepository + EntityRepository,
{
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", fixture.root.clone())
                .read_only(read_only),
        )
        .expect("save mount");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save entity");
}

fn canonical_markdown(remote_id: &str, body: &str) -> String {
    format!(
        "---\nloc:\n  id: {remote_id}\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\n{body}"
    )
}

fn shadow(body: &str) -> ShadowDocument {
    shadow_for("page-1", body)
}

fn shadow_for(remote_id: &str, body: &str) -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new(remote_id),
        body,
        9,
        [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
    )
    .expect("shadow")
}

fn shadow_with_blocks(body: &str) -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        body,
        9,
        [
            RemoteId::new("heading-1"),
            RemoteId::new("image-1"),
            RemoteId::new("paragraph-1"),
        ],
    )
    .expect("shadow")
}

fn shadow_with_ids(body: &str, count: usize) -> ShadowDocument {
    let block_ids = (0..count)
        .map(|index| RemoteId::new(format!("block-{index}")))
        .collect::<Vec<_>>();
    ShadowDocument::from_synced_body(RemoteId::new("page-1"), body, 9, block_ids).expect("shadow")
}

fn stale_daemon_push_report(path: &Path, mount_id: &MountId) -> PushJobReport {
    let plan = PushPlan::new(
        vec![RemoteId::new("page-1")],
        vec![
            PushOperation::ReplaceBlock {
                block_id: RemoteId::new("block-1"),
                content: "temp".to_string(),
            },
            PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("block-1")),
                content: "```markdown\n---\n```".to_string(),
            },
            PushOperation::ArchiveBlock {
                block_id: RemoteId::new("block-2"),
            },
        ],
    );
    PushJobReport {
        target_path: path.to_path_buf(),
        mount_id: mount_id.clone(),
        entity_id: RemoteId::new("page-1"),
        pipeline: PushPipelineResult {
            validation: ValidationReport::clean(),
            plan: Some(plan),
            guardrail: GuardrailDecision::Proceed,
            action: PushPipelineAction::ConfirmPlan,
            completed_stages: vec![
                PushStage::ParseAndValidate,
                PushStage::Diff,
                PushStage::PlanAndConfirm,
            ],
        },
        action: PushJobAction::NotReady,
        execution: None,
        push_id: None,
        journal_status: None,
        error: None,
    }
}

fn start_fake_push_daemon(report: PushJobReport) -> (String, mpsc::Receiver<DaemonRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake daemon");
    listener
        .set_nonblocking(true)
        .expect("fake daemon nonblocking");
    let addr = listener.local_addr().expect("fake daemon addr").to_string();
    let (requests_tx, requests_rx) = mpsc::channel();
    thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_nonblocking(false)
                        .expect("fake daemon blocking client stream");
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .expect("fake daemon request read timeout");
                    let mut line = String::new();
                    let mut reader =
                        BufReader::new(stream.try_clone().expect("clone fake daemon stream"));
                    reader.read_line(&mut line).expect("read daemon request");
                    let request: DaemonRequest =
                        serde_json::from_str(&line).expect("decode daemon request");
                    let _ = requests_tx.send(request);
                    let response = DaemonResponse::ok(report);
                    localityd::ipc::write_response(&mut stream, &response)
                        .expect("write daemon response");
                    return;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("fake daemon accept failed: {error}"),
            }
        }
    });
    (addr, requests_rx)
}

fn large_shadow() -> ShadowDocument {
    let body = (0..11)
        .map(|index| format!("Paragraph {index}."))
        .collect::<Vec<_>>()
        .join("\n\n");
    let block_ids = (0..11)
        .map(|index| RemoteId::new(format!("block-{index}")))
        .collect::<Vec<_>>();

    ShadowDocument::from_synced_body(RemoteId::new("page-1"), body, 9, block_ids).expect("shadow")
}

fn virtual_mutation(
    mount_id: &MountId,
    local_id: &str,
    kind: VirtualMutationKind,
    path: &str,
    title: &str,
) -> VirtualMutationRecord {
    VirtualMutationRecord {
        mount_id: mount_id.clone(),
        local_id: local_id.to_string(),
        mutation_kind: kind,
        target_remote_id: None,
        parent_remote_id: Some(RemoteId::new("parent-1")),
        original_path: None,
        projected_path: path.into(),
        title: title.to_string(),
        content_path: None,
        created_at: "now".to_string(),
        updated_at: "now".to_string(),
    }
}
