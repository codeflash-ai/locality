use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use locality_connector::{BatchObservationChange, BatchObserveResult, ConnectorCheckpoint};
use locality_core::LocalityError;
use locality_core::hydration::{HydrationReason, HydrationRequest};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId, TreeEntry};
use locality_store::{
    ConnectorStateRecord, ConnectorStateRepository, DiscoveryCommit, DiscoveryRepository,
    DiscoveryTransactionId, DiscoveryTransactionStatus, EntityRecord, EntityRepository,
    HydrationJobRecord, HydrationJobRepository, InMemoryStateStore, MountConfig, MountRepository,
    PreparedDiscoveryTransaction, ProjectionMode, SqliteStateStore, TransactionalDiscoveryCommit,
};
use localityd::discovery::{
    DiscoveryPostCommitAction, DiscoverySafetySnapshot, ProjectionAssessment, plan_batch_discovery,
};
use localityd::discovery_execution::{
    DiscoveryCreateMaterialization, DiscoveryExecutionEffects, DiscoveryExecutionOperation,
    DiscoveryExecutionPlan, DiscoveryExecutionStep, DiscoveryExecutionTerminal,
    DiscoveryFilesystemMutation, DiscoveryOperationEffectState, DiscoveryPathFingerprint,
    DiscoveryPathKind, prepare_plain_files_discovery_transaction,
    repair_active_plain_files_discovery_transactions, repair_plain_files_discovery_transaction,
    run_plain_files_discovery_transaction, step_plain_files_discovery_transaction,
    validate_plain_files_discovery_transaction_record,
};

const NOW: &str = "unix_ms:100000";

#[test]
fn execution_effects_without_projection_validation_marker_decode_as_unvalidated() {
    let mut value =
        serde_json::to_value(DiscoveryExecutionEffects::default()).expect("serialize effects");
    value
        .as_object_mut()
        .expect("effects object")
        .remove("projection_validated");

    let effects: DiscoveryExecutionEffects =
        serde_json::from_value(value).expect("decode older effects");

    assert!(!effects.projection_validated);
}

#[test]
fn preparation_reserves_versioned_components_and_exact_create_materializations() {
    let fixture = Fixture::new("prepare");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    let entries = vec![
        entry("page", EntityKind::Page, "Roadmap/page.md"),
        entry("database", EntityKind::Database, "Projects"),
        entry("directory", EntityKind::Directory, "Archive"),
    ];
    let plan = discovery_plan(&store, &fixture.mount, entries.clone());
    let expected_components = plan.projection_components.clone();
    let transaction_id = DiscoveryTransactionId::new("transaction/../one");

    let record = prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "unix_ms:100001",
        vec![
            DiscoveryCreateMaterialization::Page {
                remote_id: RemoteId::new("page"),
                document: "---\ntitle: Roadmap\n---\n<!-- loc:stub -->\n".to_string(),
            },
            DiscoveryCreateMaterialization::Database {
                remote_id: RemoteId::new("database"),
                schema_yaml: Some("name: Projects\n".to_string()),
            },
            DiscoveryCreateMaterialization::Directory {
                remote_id: RemoteId::new("directory"),
            },
        ],
    )
    .expect("prepare transaction");

    assert_eq!(record.status, DiscoveryTransactionStatus::Reserved);
    let execution: DiscoveryExecutionPlan =
        serde_json::from_value(record.plan.clone()).expect("typed execution plan");
    assert_eq!(execution.state_version, 1);
    assert_eq!(execution.min_reader_version, 1);
    assert_eq!(execution.transaction_id, transaction_id);
    assert_eq!(execution.mount_id, fixture.mount.mount_id);
    assert_eq!(
        execution
            .components
            .iter()
            .map(|component| component.component.clone())
            .collect::<Vec<_>>(),
        expected_components
    );
    let recovery_base = fixture.sandbox.join(".locality-recovery").join("discovery");
    assert!(execution.recovery_root.starts_with(&recovery_base));
    let relative_recovery = execution
        .recovery_root
        .strip_prefix(&recovery_base)
        .expect("recovery root below fixed family")
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(relative_recovery.len(), 2);
    assert!(relative_recovery.iter().all(|component| {
        component.len() == 64 && component.bytes().all(|byte| byte.is_ascii_hexdigit())
    }));
    assert!(
        !execution
            .recovery_root
            .to_string_lossy()
            .contains("transaction")
    );
    assert!(!execution.recovery_root.exists());

    let effects: DiscoveryExecutionEffects =
        serde_json::from_value(record.effects).expect("typed execution effects");
    assert_eq!(effects.state_version, 1);
    assert_eq!(effects.min_reader_version, 1);
    assert!(effects.operations.is_empty());
    assert!(effects.hydration_jobs.is_empty());
    assert!(!effects.cleanup_complete);
    assert!(!effects.completion_recorded);
}

#[test]
fn preparation_rejects_projection_component_drift_before_reserving_or_committing() {
    let fixture = Fixture::new("projection-component-drift");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    let transaction_id = DiscoveryTransactionId::new("projection-component-drift");
    let mut plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![entry("page", EntityKind::Page, "Roadmap/page.md")],
    );
    plan.projection_components.clear();

    let error = prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![DiscoveryCreateMaterialization::Page {
            remote_id: RemoteId::new("page"),
            document: "roadmap\n".to_string(),
        }],
    )
    .expect_err("projection component drift must fail");

    assert_eq!(
        error,
        LocalityError::InvalidState(
            "discovery projection components do not match projection actions".to_string()
        )
    );
    assert!(
        store
            .get_discovery_transaction(&transaction_id)
            .expect("transaction lookup")
            .is_none()
    );
    assert!(
        store
            .get_entity(&fixture.mount.mount_id, &RemoteId::new("page"))
            .expect("entity lookup")
            .is_none()
    );
    assert!(
        store
            .get_connector_state("linear", "mount", fixture.mount.mount_id.as_str())
            .expect("checkpoint lookup")
            .is_none()
    );
    assert!(!fixture.root.join("Roadmap").exists());
}

#[test]
fn fingerprints_use_stable_streamed_content_digest_records() {
    let fixture = Fixture::new("fingerprint-records");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    store
        .save_entity(entity_record("existing", "Old/page.md"))
        .expect("save existing page");
    fs::create_dir_all(fixture.root.join("Old/Child")).expect("create nested page tree");
    fs::write(fixture.root.join("Old/page.md"), "parent\n").expect("write parent page");
    fs::write(fixture.root.join("Old/Child/page.md"), "child\n").expect("write child page");
    let transaction_id = DiscoveryTransactionId::new("fingerprint-records");
    let plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![
            entry("existing", EntityKind::Page, "New/page.md"),
            entry("file", EntityKind::Page, "Roadmap.md"),
            entry("directory", EntityKind::Page, "Directory/page.md"),
        ],
    );

    let record = prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id,
        "t0",
        vec![
            DiscoveryCreateMaterialization::Page {
                remote_id: RemoteId::new("file"),
                document: "roadmap\n".to_string(),
            },
            DiscoveryCreateMaterialization::Page {
                remote_id: RemoteId::new("directory"),
                document: "roadmap\n".to_string(),
            },
        ],
    )
    .expect("prepare fingerprint fixtures");
    let execution: DiscoveryExecutionPlan =
        serde_json::from_value(record.plan).expect("decode execution plan");
    let fingerprints = execution
        .components
        .iter()
        .flat_map(|component| component.operations.iter())
        .map(|operation| match operation {
            DiscoveryExecutionOperation::Create {
                remote_id,
                expected_fingerprint,
                ..
            }
            | DiscoveryExecutionOperation::CreateContainer {
                remote_id,
                expected_fingerprint,
                ..
            }
            | DiscoveryExecutionOperation::Move {
                remote_id,
                expected_fingerprint,
                ..
            } => (remote_id.0.as_str(), expected_fingerprint.clone()),
            DiscoveryExecutionOperation::Delete { .. } => unreachable!("no delete operation"),
        })
        .collect::<BTreeMap<_, _>>();

    assert_eq!(
        fingerprints["file"],
        DiscoveryPathFingerprint {
            kind: DiscoveryPathKind::File,
            sha256: "9c4f18f4299c1eb5019efd8ee0fd15ceb956e4091b2eb1e1fecd8004e16e3a8f".to_string(),
            entries: 1,
            bytes: 8,
        }
    );
    assert_eq!(
        fingerprints["directory"],
        DiscoveryPathFingerprint {
            kind: DiscoveryPathKind::Directory,
            sha256: "d0ccc454ae9eb1ff1b46582e40586908eeb20e008a5a3533344272c6c6d7302b".to_string(),
            entries: 1,
            bytes: 8,
        }
    );
    assert_eq!(
        fingerprints["existing"],
        DiscoveryPathFingerprint {
            kind: DiscoveryPathKind::Directory,
            sha256: "baa7ce70081478618abdfd1704fbcb6680da0abd97334894646a7bf0ecc9d4c1".to_string(),
            entries: 3,
            bytes: 13,
        }
    );
}

#[test]
fn preparation_rejects_incomplete_materializations_and_unsupported_projection_work() {
    let fixture = Fixture::new("prepare-rejections");
    let page_entry = entry("page", EntityKind::Page, "Roadmap/page.md");

    let mut missing_store = InMemoryStateStore::new();
    missing_store
        .save_mount(fixture.mount.clone())
        .expect("save mount");
    let missing_id = DiscoveryTransactionId::new("missing-materialization");
    let missing_plan = discovery_plan(&missing_store, &fixture.mount, vec![page_entry.clone()]);
    assert!(
        prepare_plain_files_discovery_transaction(
            &mut missing_store,
            missing_plan,
            missing_id.clone(),
            "t0",
            vec![],
        )
        .expect_err("missing materialization")
        .to_string()
        .contains("missing its materialization")
    );
    assert!(
        missing_store
            .get_discovery_transaction(&missing_id)
            .expect("transaction lookup")
            .is_none()
    );

    let extra_plan = discovery_plan(&missing_store, &fixture.mount, vec![page_entry.clone()]);
    assert!(
        prepare_plain_files_discovery_transaction(
            &mut missing_store,
            extra_plan,
            DiscoveryTransactionId::new("extra-materialization"),
            "t0",
            vec![
                DiscoveryCreateMaterialization::Page {
                    remote_id: RemoteId::new("page"),
                    document: "page\n".to_string(),
                },
                DiscoveryCreateMaterialization::Directory {
                    remote_id: RemoteId::new("extra"),
                },
            ],
        )
        .expect_err("extra materialization")
        .to_string()
        .contains("no matching create action")
    );

    let wrong_kind_plan = discovery_plan(&missing_store, &fixture.mount, vec![page_entry.clone()]);
    assert!(
        prepare_plain_files_discovery_transaction(
            &mut missing_store,
            wrong_kind_plan,
            DiscoveryTransactionId::new("wrong-kind-materialization"),
            "t0",
            vec![DiscoveryCreateMaterialization::Directory {
                remote_id: RemoteId::new("page"),
            }],
        )
        .expect_err("wrong materialization kind")
        .to_string()
        .contains("kind does not match")
    );

    let duplicate_plan = discovery_plan(&missing_store, &fixture.mount, vec![page_entry.clone()]);
    assert!(
        prepare_plain_files_discovery_transaction(
            &mut missing_store,
            duplicate_plan,
            DiscoveryTransactionId::new("duplicate-materialization"),
            "t0",
            vec![
                DiscoveryCreateMaterialization::Page {
                    remote_id: RemoteId::new("page"),
                    document: "one\n".to_string(),
                },
                DiscoveryCreateMaterialization::Page {
                    remote_id: RemoteId::new("page"),
                    document: "two\n".to_string(),
                },
            ],
        )
        .expect_err("duplicate materialization")
        .to_string()
        .contains("duplicate discovery create materialization")
    );

    let asset_plan = discovery_plan(
        &missing_store,
        &fixture.mount,
        vec![entry("asset", EntityKind::Asset, "asset.bin")],
    );
    assert!(
        prepare_plain_files_discovery_transaction(
            &mut missing_store,
            asset_plan,
            DiscoveryTransactionId::new("asset-create"),
            "t0",
            vec![],
        )
        .expect_err("asset create")
        .to_string()
        .contains("unsupported plain-files kind")
    );

    let mut provider_store = InMemoryStateStore::new();
    let provider_mount = fixture
        .mount
        .clone()
        .projection(ProjectionMode::MacosFileProvider);
    provider_store
        .save_mount(provider_mount.clone())
        .expect("save provider mount");
    let provider_plan = discovery_plan(&provider_store, &provider_mount, vec![page_entry.clone()]);
    assert!(
        prepare_plain_files_discovery_transaction(
            &mut provider_store,
            provider_plan,
            DiscoveryTransactionId::new("provider-mode"),
            "t0",
            vec![DiscoveryCreateMaterialization::Page {
                remote_id: RemoteId::new("page"),
                document: "page\n".to_string(),
            }],
        )
        .expect_err("provider projection")
        .to_string()
        .contains("supports only plain_files")
    );

    let mut invalid_post_commit = discovery_plan(&missing_store, &fixture.mount, vec![page_entry]);
    invalid_post_commit
        .post_commit
        .push(DiscoveryPostCommitAction::InvalidateProvider {
            mount_id: fixture.mount.mount_id.clone(),
            paths: vec![PathBuf::from("Roadmap/page.md")],
        });
    assert!(
        prepare_plain_files_discovery_transaction(
            &mut missing_store,
            invalid_post_commit,
            DiscoveryTransactionId::new("invalid-post-commit"),
            "t0",
            vec![DiscoveryCreateMaterialization::Page {
                remote_id: RemoteId::new("page"),
                document: "page\n".to_string(),
            }],
        )
        .expect_err("provider invalidation")
        .to_string()
        .contains("does not accept provider invalidation")
    );
}

#[test]
fn preparation_rejects_destination_collisions_and_symlink_ancestors() {
    let fixture = Fixture::new("prepare-path-guards");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    fs::create_dir_all(fixture.root.join("Occupied")).expect("create occupied destination");
    fs::write(fixture.root.join("Occupied/page.md"), "unowned\n")
        .expect("write occupied destination");
    let collision_plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![entry("page", EntityKind::Page, "Occupied/page.md")],
    );
    assert!(
        prepare_plain_files_discovery_transaction(
            &mut store,
            collision_plan,
            DiscoveryTransactionId::new("collision"),
            "t0",
            vec![DiscoveryCreateMaterialization::Page {
                remote_id: RemoteId::new("page"),
                document: "new\n".to_string(),
            }],
        )
        .expect_err("destination collision")
        .to_string()
        .contains("already exists outside a staged source")
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let outside = fixture.sandbox.join("outside");
        fs::create_dir_all(&outside).expect("create symlink target");
        symlink(&outside, fixture.root.join("Linked")).expect("create ancestor symlink");
        let symlink_plan = discovery_plan(
            &store,
            &fixture.mount,
            vec![entry("linked", EntityKind::Page, "Linked/Page/page.md")],
        );
        assert!(
            prepare_plain_files_discovery_transaction(
                &mut store,
                symlink_plan,
                DiscoveryTransactionId::new("symlink-ancestor"),
                "t0",
                vec![DiscoveryCreateMaterialization::Page {
                    remote_id: RemoteId::new("linked"),
                    document: "linked\n".to_string(),
                }],
            )
            .expect_err("ancestor symlink")
            .to_string()
            .contains("is a symlink")
        );
        assert!(!outside.join("Page").exists());
    }
}

#[test]
fn preparation_rejects_absent_destination_ancestors_without_directory_operations() {
    let fixture = Fixture::new("missing-destination-ancestor");
    let mut create_store = InMemoryStateStore::new();
    create_store
        .save_mount(fixture.mount.clone())
        .expect("save create mount");
    let create_id = DiscoveryTransactionId::new("missing-create-ancestor");
    let create_plan = discovery_plan(
        &create_store,
        &fixture.mount,
        vec![entry("page", EntityKind::Page, "Missing/Child.md")],
    );
    let create_error = prepare_plain_files_discovery_transaction(
        &mut create_store,
        create_plan,
        create_id.clone(),
        "t0",
        vec![DiscoveryCreateMaterialization::Page {
            remote_id: RemoteId::new("page"),
            document: "child\n".to_string(),
        }],
    )
    .expect_err("create without ancestor operation must fail");
    assert_eq!(
        create_error,
        LocalityError::InvalidState(format!(
            "discovery destination ancestor `{}` is absent and no directory operation provides it",
            fixture.root.join("Missing").display()
        ))
    );
    assert!(
        create_store
            .get_discovery_transaction(&create_id)
            .expect("create transaction lookup")
            .is_none()
    );
    assert!(!fixture.root.join("Missing").exists());

    let move_fixture = Fixture::new("missing-move-ancestor");
    let mut move_store = InMemoryStateStore::new();
    move_store
        .save_mount(move_fixture.mount.clone())
        .expect("save move mount");
    move_store
        .save_entity(entity_record("page", "Old/page.md"))
        .expect("save move entity");
    fs::create_dir_all(move_fixture.root.join("Old")).expect("create old page");
    fs::write(move_fixture.root.join("Old/page.md"), "original\n").expect("write old page");
    let move_id = DiscoveryTransactionId::new("missing-move-ancestor");
    let move_plan = discovery_plan(
        &move_store,
        &move_fixture.mount,
        vec![entry("page", EntityKind::Page, "Missing/Child/page.md")],
    );
    let move_error = prepare_plain_files_discovery_transaction(
        &mut move_store,
        move_plan,
        move_id.clone(),
        "t0",
        vec![],
    )
    .expect_err("move without ancestor operation must fail");
    assert_eq!(
        move_error,
        LocalityError::InvalidState(format!(
            "discovery destination ancestor `{}` is absent and no directory operation provides it",
            move_fixture.root.join("Missing").display()
        ))
    );
    assert!(
        move_store
            .get_discovery_transaction(&move_id)
            .expect("move transaction lookup")
            .is_none()
    );
    assert_eq!(
        fs::read_to_string(move_fixture.root.join("Old/page.md")).expect("original remains"),
        "original\n"
    );
    assert!(!move_fixture.root.join("Missing").exists());
}

#[cfg(unix)]
#[test]
fn run_classifies_an_inserted_ancestor_symlink_as_needs_review_without_mutation() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new("repair-symlink-ancestor");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    fs::create_dir(fixture.root.join("Inserted")).expect("create destination ancestor");
    let transaction_id = DiscoveryTransactionId::new("repair-symlink-ancestor");
    let plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![entry("linked", EntityKind::Page, "Inserted/Child/page.md")],
    );
    prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![DiscoveryCreateMaterialization::Page {
            remote_id: RemoteId::new("linked"),
            document: "linked\n".to_string(),
        }],
    )
    .expect("prepare transaction before symlink insertion");
    let outside = fixture.sandbox.join("outside-after-reservation");
    fs::create_dir_all(&outside).expect("create outside target");
    fs::remove_dir(fixture.root.join("Inserted")).expect("remove destination ancestor");
    symlink(&outside, fixture.root.join("Inserted")).expect("insert ancestor symlink");

    assert_eq!(
        run_plain_files_discovery_transaction(&mut store, &transaction_id, "t1")
            .expect("classify inserted ancestor symlink"),
        DiscoveryExecutionTerminal::NeedsReview
    );
    assert!(!outside.join("Child").exists());
    assert_eq!(
        store
            .get_discovery_transaction(&transaction_id)
            .expect("transaction lookup")
            .expect("transaction")
            .status,
        DiscoveryTransactionStatus::Reserved
    );
    assert_eq!(
        repair_plain_files_discovery_transaction(&mut store, &transaction_id, "t2")
            .expect("abort reserved transaction without filesystem validation"),
        DiscoveryExecutionTerminal::Aborted
    );
}

#[test]
fn page_create_execution_exposes_every_durable_and_filesystem_boundary() {
    let fixture = Fixture::new("page-create-steps");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    let page = entry("page", EntityKind::Page, "Roadmap/page.md");
    let transaction_id = DiscoveryTransactionId::new("page-create");
    let plan = discovery_plan(&store, &fixture.mount, vec![page]);
    let reserved = prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![DiscoveryCreateMaterialization::Page {
            remote_id: RemoteId::new("page"),
            document: "---\ntitle: Roadmap\n---\n<!-- loc:stub -->\n".to_string(),
        }],
    )
    .expect("prepare create");
    let execution: DiscoveryExecutionPlan =
        serde_json::from_value(reserved.plan).expect("execution plan");
    let operation_id = "component-00000000-action-00000000".to_string();
    let payload = execution.recovery_root.join("payloads").join(&operation_id);
    let destination = fixture.root.join("Roadmap/page.md");

    assert_eq!(
        step(&mut store, &transaction_id, 1),
        DiscoveryExecutionStep::Applying
    );
    assert_eq!(
        step(&mut store, &transaction_id, 2),
        DiscoveryExecutionStep::OperationPrepared {
            operation_id: operation_id.clone(),
        }
    );
    assert!(!payload.exists());
    assert_eq!(
        effect_state(&store, &transaction_id),
        DiscoveryOperationEffectState::Prepared
    );

    assert_eq!(
        step(&mut store, &transaction_id, 3),
        DiscoveryExecutionStep::FilesystemMutation {
            operation_id: operation_id.clone(),
            mutation: DiscoveryFilesystemMutation::PayloadPublished,
        }
    );
    assert!(payload.exists());
    assert!(!destination.exists());
    assert_eq!(
        effect_state(&store, &transaction_id),
        DiscoveryOperationEffectState::Prepared
    );

    assert_eq!(
        step(&mut store, &transaction_id, 4),
        DiscoveryExecutionStep::OperationRecorded {
            operation_id: operation_id.clone(),
            state: DiscoveryOperationEffectState::Staged,
        }
    );
    assert_eq!(
        effect_state(&store, &transaction_id),
        DiscoveryOperationEffectState::Staged
    );

    assert_eq!(
        step(&mut store, &transaction_id, 5),
        DiscoveryExecutionStep::FilesystemMutation {
            operation_id: operation_id.clone(),
            mutation: DiscoveryFilesystemMutation::DestinationInstalled,
        }
    );
    assert!(!payload.exists());
    assert_eq!(
        fs::read_to_string(&destination).expect("installed page"),
        "---\ntitle: Roadmap\n---\n<!-- loc:stub -->\n"
    );
    assert_eq!(
        effect_state(&store, &transaction_id),
        DiscoveryOperationEffectState::Staged
    );

    assert_eq!(
        step(&mut store, &transaction_id, 6),
        DiscoveryExecutionStep::OperationRecorded {
            operation_id: operation_id.clone(),
            state: DiscoveryOperationEffectState::Installed,
        }
    );
    assert_eq!(
        step(&mut store, &transaction_id, 7),
        DiscoveryExecutionStep::Projected
    );
    assert_eq!(
        step(&mut store, &transaction_id, 8),
        DiscoveryExecutionStep::Committed
    );
    assert!(
        store
            .get_entity(&fixture.mount.mount_id, &RemoteId::new("page"))
            .expect("entity")
            .is_some()
    );
    assert_eq!(
        step(&mut store, &transaction_id, 9),
        DiscoveryExecutionStep::ProjectionValidated
    );
    assert!(execution.recovery_root.exists());
    let barrier_record = store
        .get_discovery_transaction(&transaction_id)
        .expect("transaction lookup")
        .expect("transaction");
    let barrier_effects: DiscoveryExecutionEffects =
        serde_json::from_value(barrier_record.effects).expect("decode barrier effects");
    assert!(barrier_effects.projection_validated);
    assert_eq!(
        step(&mut store, &transaction_id, 10),
        DiscoveryExecutionStep::RecoveryPayloadsRemoved
    );
    assert!(!execution.recovery_root.exists());
    assert_eq!(
        step(&mut store, &transaction_id, 11),
        DiscoveryExecutionStep::CleanupComplete
    );
    assert_eq!(
        step(&mut store, &transaction_id, 12),
        DiscoveryExecutionStep::CompletionRecorded
    );
    assert_eq!(
        step(&mut store, &transaction_id, 13),
        DiscoveryExecutionStep::Finalized
    );
    assert_eq!(
        store
            .get_discovery_transaction(&transaction_id)
            .expect("transaction")
            .expect("record")
            .status,
        DiscoveryTransactionStatus::Finalized
    );
}

#[test]
fn exact_create_temporary_payloads_resume_after_a_crash_for_files_and_directories() {
    for (label, projected_path) in [
        ("resume-file-temporary", "Roadmap.md"),
        ("resume-directory-temporary", "Roadmap/page.md"),
    ] {
        let fixture = Fixture::new(label);
        let mut store = InMemoryStateStore::new();
        store.save_mount(fixture.mount.clone()).expect("save mount");
        let transaction_id = DiscoveryTransactionId::new(label);
        let plan = discovery_plan(
            &store,
            &fixture.mount,
            vec![entry("page", EntityKind::Page, projected_path)],
        );
        let reserved = prepare_plain_files_discovery_transaction(
            &mut store,
            plan,
            transaction_id.clone(),
            "t0",
            vec![DiscoveryCreateMaterialization::Page {
                remote_id: RemoteId::new("page"),
                document: "roadmap\n".to_string(),
            }],
        )
        .expect("prepare create");
        let execution: DiscoveryExecutionPlan =
            serde_json::from_value(reserved.plan).expect("decode execution");
        let DiscoveryExecutionOperation::Create {
            destination,
            temporary_payload,
            ..
        } = &execution.components[0].operations[0]
        else {
            panic!("expected create operation");
        };
        assert_eq!(
            step(&mut store, &transaction_id, 1),
            DiscoveryExecutionStep::Applying
        );
        assert!(matches!(
            step(&mut store, &transaction_id, 2),
            DiscoveryExecutionStep::OperationPrepared { .. }
        ));
        let temporary = execution.recovery_root.join(temporary_payload);
        fs::create_dir_all(temporary.parent().expect("temporary parent"))
            .expect("create recovery parent");
        if projected_path.ends_with("/page.md") {
            fs::create_dir(&temporary).expect("create temporary directory");
            fs::write(temporary.join("page.md"), "roadmap\n").expect("write exact temporary page");
        } else {
            fs::write(&temporary, "roadmap\n").expect("write exact temporary file");
        }

        assert_eq!(
            repair_plain_files_discovery_transaction(&mut store, &transaction_id, "t3")
                .expect("resume exact temporary payload"),
            DiscoveryExecutionTerminal::Finalized,
            "{label}"
        );
        let destination = fixture.root.join(destination);
        let document = if destination.is_dir() {
            destination.join("page.md")
        } else {
            destination
        };
        assert_eq!(
            fs::read_to_string(document).expect("installed create document"),
            "roadmap\n",
            "{label}"
        );
    }
}

#[test]
fn mismatched_create_temporary_payloads_are_preserved_for_review() {
    for (label, projected_path) in [
        ("mismatched-file-temporary", "Roadmap.md"),
        ("partial-directory-temporary", "Roadmap/page.md"),
    ] {
        let fixture = Fixture::new(label);
        let mut store = InMemoryStateStore::new();
        store.save_mount(fixture.mount.clone()).expect("save mount");
        let transaction_id = DiscoveryTransactionId::new(label);
        let plan = discovery_plan(
            &store,
            &fixture.mount,
            vec![entry("page", EntityKind::Page, projected_path)],
        );
        let reserved = prepare_plain_files_discovery_transaction(
            &mut store,
            plan,
            transaction_id.clone(),
            "t0",
            vec![DiscoveryCreateMaterialization::Page {
                remote_id: RemoteId::new("page"),
                document: "expected\n".to_string(),
            }],
        )
        .expect("prepare create");
        let execution: DiscoveryExecutionPlan =
            serde_json::from_value(reserved.plan).expect("decode execution");
        let DiscoveryExecutionOperation::Create {
            destination,
            temporary_payload,
            ..
        } = &execution.components[0].operations[0]
        else {
            panic!("expected create operation");
        };
        assert_eq!(
            step(&mut store, &transaction_id, 1),
            DiscoveryExecutionStep::Applying
        );
        assert!(matches!(
            step(&mut store, &transaction_id, 2),
            DiscoveryExecutionStep::OperationPrepared { .. }
        ));
        let temporary = execution.recovery_root.join(temporary_payload);
        fs::create_dir_all(temporary.parent().expect("temporary parent"))
            .expect("create recovery parent");
        if projected_path.ends_with("/page.md") {
            fs::create_dir(&temporary).expect("create partial temporary directory");
            fs::write(temporary.join("page.md"), "partial\n")
                .expect("write partial temporary page");
        } else {
            fs::write(&temporary, "unrelated\n").expect("write unrelated temporary file");
        }

        assert_eq!(
            run_plain_files_discovery_transaction(&mut store, &transaction_id, "t3")
                .expect("classify mismatched temporary payload"),
            DiscoveryExecutionTerminal::NeedsReview,
            "{label}"
        );
        assert_eq!(
            store
                .get_discovery_transaction(&transaction_id)
                .expect("transaction lookup")
                .expect("transaction")
                .status,
            DiscoveryTransactionStatus::RepairPending,
            "{label}"
        );
        assert!(!fixture.root.join(destination).exists(), "{label}");
        if temporary.is_dir() {
            assert_eq!(
                fs::read_to_string(temporary.join("page.md"))
                    .expect("preserved partial directory payload"),
                "partial\n",
                "{label}"
            );
        } else {
            assert_eq!(
                fs::read_to_string(&temporary).expect("preserved unrelated file payload"),
                "unrelated\n",
                "{label}"
            );
        }
    }
}

#[test]
fn nested_creates_install_parent_first_and_validate_independent_child_ownership() {
    let fixture = Fixture::new("nested-creates");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    let plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![
            entry("a-child", EntityKind::Page, "Parent/Child/page.md"),
            entry("z-parent", EntityKind::Page, "Parent/page.md"),
        ],
    );
    let transaction_id = DiscoveryTransactionId::new("nested-creates");
    let reserved = prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![
            DiscoveryCreateMaterialization::Page {
                remote_id: RemoteId::new("a-child"),
                document: "child\n".to_string(),
            },
            DiscoveryCreateMaterialization::Page {
                remote_id: RemoteId::new("z-parent"),
                document: "parent\n".to_string(),
            },
        ],
    )
    .expect("prepare nested creates");
    let execution: DiscoveryExecutionPlan =
        serde_json::from_value(reserved.plan).expect("decode nested execution");
    let operation_owners = execution
        .components
        .iter()
        .flat_map(|component| component.operations.iter())
        .map(|operation| match operation {
            DiscoveryExecutionOperation::Create {
                operation_id,
                remote_id,
                ..
            } => (operation_id.clone(), remote_id.clone()),
            _ => panic!("expected create operation"),
        })
        .collect::<BTreeMap<_, _>>();
    let mut installed = Vec::new();
    for sequence in 1..=40 {
        let outcome = step(&mut store, &transaction_id, sequence);
        if let DiscoveryExecutionStep::FilesystemMutation {
            operation_id,
            mutation: DiscoveryFilesystemMutation::DestinationInstalled,
        } = &outcome
        {
            installed.push(operation_owners[operation_id].clone());
        }
        if outcome == DiscoveryExecutionStep::Finalized {
            break;
        }
    }
    assert_eq!(
        installed,
        vec![RemoteId::new("z-parent"), RemoteId::new("a-child")]
    );

    assert_eq!(
        fs::read_to_string(fixture.root.join("Parent/page.md")).expect("parent document"),
        "parent\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("Parent/Child/page.md")).expect("child document"),
        "child\n"
    );
}

#[test]
fn mixed_parent_create_and_child_move_stages_every_source_then_installs_parent_first() {
    let fixture = Fixture::new("mixed-parent-create-child-move");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    store
        .save_entity(entity_record("child", "Old/page.md"))
        .expect("save child");
    fs::create_dir_all(fixture.root.join("Old")).expect("create child source");
    fs::write(fixture.root.join("Old/page.md"), "child\n").expect("write child source");
    let plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![
            entry("child", EntityKind::Page, "Parent/Child/page.md"),
            entry("parent", EntityKind::Page, "Parent/page.md"),
        ],
    );
    let transaction_id = DiscoveryTransactionId::new("mixed-parent-create-child-move");
    let reserved = prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![DiscoveryCreateMaterialization::Page {
            remote_id: RemoteId::new("parent"),
            document: "parent\n".to_string(),
        }],
    )
    .expect("prepare mixed component");
    let execution: DiscoveryExecutionPlan =
        serde_json::from_value(reserved.plan).expect("decode mixed execution");
    assert_eq!(execution.components.len(), 1);
    let operation_ids = execution.components[0]
        .operations
        .iter()
        .map(|operation| match operation {
            DiscoveryExecutionOperation::Create {
                operation_id,
                remote_id,
                ..
            }
            | DiscoveryExecutionOperation::CreateContainer {
                operation_id,
                remote_id,
                ..
            }
            | DiscoveryExecutionOperation::Move {
                operation_id,
                remote_id,
                ..
            } => (remote_id.clone(), operation_id.clone()),
            DiscoveryExecutionOperation::Delete { .. } => panic!("unexpected delete"),
        })
        .collect::<BTreeMap<_, _>>();
    let child = operation_ids[&RemoteId::new("child")].clone();
    let parent = operation_ids[&RemoteId::new("parent")].clone();

    let actual = (1..=11)
        .map(|sequence| step(&mut store, &transaction_id, sequence))
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        vec![
            DiscoveryExecutionStep::Applying,
            DiscoveryExecutionStep::OperationPrepared {
                operation_id: child.clone(),
            },
            DiscoveryExecutionStep::FilesystemMutation {
                operation_id: child.clone(),
                mutation: DiscoveryFilesystemMutation::SourceStaged,
            },
            DiscoveryExecutionStep::OperationRecorded {
                operation_id: child.clone(),
                state: DiscoveryOperationEffectState::Staged,
            },
            DiscoveryExecutionStep::OperationPrepared {
                operation_id: parent.clone(),
            },
            DiscoveryExecutionStep::FilesystemMutation {
                operation_id: parent.clone(),
                mutation: DiscoveryFilesystemMutation::PayloadPublished,
            },
            DiscoveryExecutionStep::OperationRecorded {
                operation_id: parent.clone(),
                state: DiscoveryOperationEffectState::Staged,
            },
            DiscoveryExecutionStep::FilesystemMutation {
                operation_id: parent.clone(),
                mutation: DiscoveryFilesystemMutation::DestinationInstalled,
            },
            DiscoveryExecutionStep::OperationRecorded {
                operation_id: parent,
                state: DiscoveryOperationEffectState::Installed,
            },
            DiscoveryExecutionStep::FilesystemMutation {
                operation_id: child.clone(),
                mutation: DiscoveryFilesystemMutation::DestinationInstalled,
            },
            DiscoveryExecutionStep::OperationRecorded {
                operation_id: child,
                state: DiscoveryOperationEffectState::Installed,
            },
        ]
    );
    run_steps_to_finalized_from(&mut store, &transaction_id, 12);

    assert!(!fixture.root.join("Old").exists());
    assert_eq!(
        fs::read_to_string(fixture.root.join("Parent/page.md")).expect("parent document"),
        "parent\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("Parent/Child/page.md")).expect("child document"),
        "child\n"
    );
}

#[test]
fn missing_journaled_parent_during_nested_create_rolls_back_without_recreating_it() {
    let fixture = Fixture::new("nested-create-parent-removed");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    let transaction_id = DiscoveryTransactionId::new("nested-create-parent-removed");
    let plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![
            entry("child", EntityKind::Page, "Parent/Child/page.md"),
            entry("parent", EntityKind::Page, "Parent/page.md"),
        ],
    );
    let reserved = prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![
            DiscoveryCreateMaterialization::Page {
                remote_id: RemoteId::new("child"),
                document: "child\n".to_string(),
            },
            DiscoveryCreateMaterialization::Page {
                remote_id: RemoteId::new("parent"),
                document: "parent\n".to_string(),
            },
        ],
    )
    .expect("prepare nested creates");
    let execution: DiscoveryExecutionPlan =
        serde_json::from_value(reserved.plan).expect("decode nested execution");
    let parent_operation = execution
        .components
        .iter()
        .flat_map(|component| component.operations.iter())
        .find_map(|operation| match operation {
            DiscoveryExecutionOperation::Create {
                operation_id,
                remote_id,
                ..
            } if remote_id == &RemoteId::new("parent") => Some(operation_id.clone()),
            _ => None,
        })
        .expect("parent operation");
    for sequence in 1..=20 {
        let outcome = step(&mut store, &transaction_id, sequence);
        if outcome
            == (DiscoveryExecutionStep::OperationRecorded {
                operation_id: parent_operation.clone(),
                state: DiscoveryOperationEffectState::Installed,
            })
        {
            break;
        }
    }
    assert_eq!(
        fs::read_to_string(fixture.root.join("Parent/page.md")).expect("installed parent"),
        "parent\n"
    );
    fs::remove_dir_all(fixture.root.join("Parent")).expect("remove journaled parent");

    assert_eq!(
        run_plain_files_discovery_transaction(&mut store, &transaction_id, "t30")
            .expect("rollback missing parent"),
        DiscoveryExecutionTerminal::Aborted
    );
    assert!(!fixture.root.join("Parent").exists());
}

#[test]
fn page_swap_stages_every_component_source_before_installing_destinations() {
    let fixture = Fixture::new("page-swap");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    for (remote_id, path, contents) in [
        ("a", "A/page.md", "contents-a\n"),
        ("b", "B/page.md", "contents-b\n"),
    ] {
        store
            .save_entity(entity_record(remote_id, path))
            .expect("save entity");
        let full_path = fixture.root.join(path);
        fs::create_dir_all(full_path.parent().expect("page parent")).expect("create page parent");
        fs::write(full_path, contents).expect("write page");
    }
    let plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![
            entry("a", EntityKind::Page, "B/page.md"),
            entry("b", EntityKind::Page, "A/page.md"),
        ],
    );
    let transaction_id = DiscoveryTransactionId::new("page-swap");
    prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![],
    )
    .expect("prepare swap");

    assert_eq!(
        step(&mut store, &transaction_id, 1),
        DiscoveryExecutionStep::Applying
    );
    for (sequence, operation_id) in [
        (2, "component-00000000-action-00000000"),
        (5, "component-00000000-action-00000001"),
    ] {
        assert_eq!(
            step(&mut store, &transaction_id, sequence),
            DiscoveryExecutionStep::OperationPrepared {
                operation_id: operation_id.to_string(),
            }
        );
        assert_eq!(
            step(&mut store, &transaction_id, sequence + 1),
            DiscoveryExecutionStep::FilesystemMutation {
                operation_id: operation_id.to_string(),
                mutation: DiscoveryFilesystemMutation::SourceStaged,
            }
        );
        assert_eq!(
            step(&mut store, &transaction_id, sequence + 2),
            DiscoveryExecutionStep::OperationRecorded {
                operation_id: operation_id.to_string(),
                state: DiscoveryOperationEffectState::Staged,
            }
        );
    }
    assert!(!fixture.root.join("A").exists());
    assert!(!fixture.root.join("B").exists());

    assert_eq!(
        step(&mut store, &transaction_id, 8),
        DiscoveryExecutionStep::FilesystemMutation {
            operation_id: "component-00000000-action-00000000".to_string(),
            mutation: DiscoveryFilesystemMutation::DestinationInstalled,
        }
    );
    assert_eq!(
        step(&mut store, &transaction_id, 9),
        DiscoveryExecutionStep::OperationRecorded {
            operation_id: "component-00000000-action-00000000".to_string(),
            state: DiscoveryOperationEffectState::Installed,
        }
    );
    assert_eq!(
        step(&mut store, &transaction_id, 10),
        DiscoveryExecutionStep::FilesystemMutation {
            operation_id: "component-00000000-action-00000001".to_string(),
            mutation: DiscoveryFilesystemMutation::DestinationInstalled,
        }
    );
    assert_eq!(
        step(&mut store, &transaction_id, 11),
        DiscoveryExecutionStep::OperationRecorded {
            operation_id: "component-00000000-action-00000001".to_string(),
            state: DiscoveryOperationEffectState::Installed,
        }
    );
    run_steps_to_finalized_from(&mut store, &transaction_id, 12);

    assert_eq!(
        fs::read_to_string(fixture.root.join("A/page.md")).expect("page at A"),
        "contents-b\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("B/page.md")).expect("page at B"),
        "contents-a\n"
    );
}

#[test]
fn delete_and_move_into_deleted_path_preserves_the_moved_page() {
    let fixture = Fixture::new("delete-move-replacement");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    for (remote_id, path, contents) in [
        ("moving", "Old/page.md", "moving\n"),
        ("deleted", "Target/page.md", "deleted\n"),
    ] {
        store
            .save_entity(entity_record(remote_id, path))
            .expect("save entity");
        let document = fixture.root.join(path);
        fs::create_dir_all(document.parent().expect("page parent")).expect("create page parent");
        fs::write(document, contents).expect("write page");
    }
    let transaction_id = DiscoveryTransactionId::new("delete-move-replacement");
    let plan = discovery_plan_changes(
        &store,
        &fixture.mount,
        vec![
            BatchObservationChange::Upsert(entry("moving", EntityKind::Page, "Target/page.md")),
            BatchObservationChange::Tombstone {
                remote_id: RemoteId::new("deleted"),
            },
        ],
    );
    prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![],
    )
    .expect("prepare delete and move");

    run_steps_to_finalized(&mut store, &transaction_id);

    assert_eq!(
        fs::read_to_string(fixture.root.join("Target/page.md")).expect("replacement page"),
        "moving\n"
    );
    assert!(!fixture.root.join("Old").exists());
    assert!(
        store
            .get_entity(&fixture.mount.mount_id, &RemoteId::new("deleted"))
            .expect("deleted entity lookup")
            .is_none()
    );
}

#[test]
fn delete_rejects_unrelated_content_that_reappears_at_its_source() {
    let fixture = Fixture::new("delete-source-reappeared");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    store
        .save_entity(entity_record("deleted", "Target/page.md"))
        .expect("save entity");
    fs::create_dir_all(fixture.root.join("Target")).expect("create page directory");
    fs::write(fixture.root.join("Target/page.md"), "original\n").expect("write page");
    let transaction_id = DiscoveryTransactionId::new("delete-source-reappeared");
    let plan = discovery_plan_changes(
        &store,
        &fixture.mount,
        vec![BatchObservationChange::Tombstone {
            remote_id: RemoteId::new("deleted"),
        }],
    );
    let reserved = prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![],
    )
    .expect("prepare delete");
    let execution: DiscoveryExecutionPlan =
        serde_json::from_value(reserved.plan).expect("decode delete execution");
    let staged_original = execution
        .components
        .iter()
        .flat_map(|component| component.operations.iter())
        .find_map(|operation| match operation {
            DiscoveryExecutionOperation::Delete { stage, .. } => {
                Some(execution.recovery_root.join(stage))
            }
            _ => None,
        })
        .expect("delete stage");

    for sequence in 1..=5 {
        step(&mut store, &transaction_id, sequence);
    }
    fs::create_dir_all(fixture.root.join("Target")).expect("recreate page directory");
    fs::write(fixture.root.join("Target/page.md"), "unrelated\n")
        .expect("write unrelated replacement");

    assert_eq!(
        step(&mut store, &transaction_id, 6),
        DiscoveryExecutionStep::RollbackStarted
    );
    assert_eq!(
        step(&mut store, &transaction_id, 7),
        DiscoveryExecutionStep::RepairPending
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("Target/page.md")).expect("preserved replacement"),
        "unrelated\n"
    );
    assert_eq!(
        fs::read_to_string(staged_original.join("page.md")).expect("preserved staged original"),
        "original\n"
    );
}

#[test]
fn page_moves_preserve_children_across_named_and_canonical_layouts() {
    for (label, from, to) in [
        ("named-to-named", "Old.md", "New.md"),
        ("canonical-to-named", "Old/page.md", "New.md"),
        ("named-to-canonical", "Old.md", "New/page.md"),
    ] {
        let fixture = Fixture::new(label);
        let mut store = InMemoryStateStore::new();
        store.save_mount(fixture.mount.clone()).expect("save mount");
        store
            .save_entity(entity_record("page", from))
            .expect("save entity");
        let document = fixture.root.join(from);
        fs::create_dir_all(document.parent().expect("document parent"))
            .expect("create document parent");
        fs::write(&document, "parent\n").expect("write document");
        let child = fixture.root.join("Old/Child/page.md");
        fs::create_dir_all(child.parent().expect("child parent")).expect("create child parent");
        fs::write(&child, "child\n").expect("write child");
        let transaction_id = DiscoveryTransactionId::new(label);
        let plan = discovery_plan(
            &store,
            &fixture.mount,
            vec![entry("page", EntityKind::Page, to)],
        );
        prepare_plain_files_discovery_transaction(
            &mut store,
            plan,
            transaction_id.clone(),
            "t0",
            vec![],
        )
        .expect("prepare page layout move");

        run_steps_to_finalized(&mut store, &transaction_id);

        assert_eq!(
            fs::read_to_string(fixture.root.join(to)).expect("moved document"),
            "parent\n",
            "{label}"
        );
        assert_eq!(
            fs::read_to_string(fixture.root.join("New/Child/page.md")).expect("moved child"),
            "child\n",
            "{label}"
        );
        assert!(!document.exists(), "{label}");
        assert!(!fixture.root.join("Old").exists(), "{label}");
    }
}

#[test]
fn recursive_parent_move_and_delete_actions_cover_descendant_commits() {
    {
        let fixture = Fixture::new("recursive-parent-move");
        let mut store = InMemoryStateStore::new();
        store.save_mount(fixture.mount.clone()).expect("save mount");
        for (remote_id, path, contents) in [
            ("parent", "Old/page.md", "parent\n"),
            ("child", "Old/Child/page.md", "child\n"),
        ] {
            store
                .save_entity(entity_record(remote_id, path))
                .expect("save entity");
            let document = fixture.root.join(path);
            fs::create_dir_all(document.parent().expect("document parent"))
                .expect("create document parent");
            fs::write(document, contents).expect("write document");
        }
        let plan = discovery_plan(
            &store,
            &fixture.mount,
            vec![
                entry("parent", EntityKind::Page, "New/page.md"),
                entry("child", EntityKind::Page, "New/Child/page.md"),
            ],
        );
        assert_eq!(plan.projection_actions.len(), 1);
        assert_eq!(plan.projection_components.len(), 1);
        assert_eq!(plan.projection_components[0].actions.len(), 1);
        let transaction_id = DiscoveryTransactionId::new("recursive-parent-move");
        prepare_plain_files_discovery_transaction(
            &mut store,
            plan,
            transaction_id.clone(),
            "t0",
            vec![],
        )
        .expect("prepare normalized recursive move");
        run_steps_to_finalized(&mut store, &transaction_id);

        assert!(!fixture.root.join("Old").exists());
        assert_eq!(
            fs::read_to_string(fixture.root.join("New/page.md")).expect("parent moved"),
            "parent\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.root.join("New/Child/page.md")).expect("child moved"),
            "child\n"
        );
        for (remote_id, path) in [
            (RemoteId::new("parent"), PathBuf::from("New/page.md")),
            (RemoteId::new("child"), PathBuf::from("New/Child/page.md")),
        ] {
            assert_eq!(
                store
                    .get_entity(&fixture.mount.mount_id, &remote_id)
                    .expect("entity lookup")
                    .expect("entity")
                    .path,
                path
            );
        }
    }

    {
        let fixture = Fixture::new("recursive-parent-delete");
        let mut store = InMemoryStateStore::new();
        store.save_mount(fixture.mount.clone()).expect("save mount");
        for (remote_id, path, contents) in [
            ("parent", "Deleted/page.md", "parent\n"),
            ("child", "Deleted/Child/page.md", "child\n"),
        ] {
            store
                .save_entity(entity_record(remote_id, path))
                .expect("save entity");
            let document = fixture.root.join(path);
            fs::create_dir_all(document.parent().expect("document parent"))
                .expect("create document parent");
            fs::write(document, contents).expect("write document");
        }
        let plan = discovery_plan_changes(
            &store,
            &fixture.mount,
            vec![
                BatchObservationChange::Tombstone {
                    remote_id: RemoteId::new("parent"),
                },
                BatchObservationChange::Tombstone {
                    remote_id: RemoteId::new("child"),
                },
            ],
        );
        assert_eq!(plan.projection_actions.len(), 1);
        assert_eq!(plan.projection_components.len(), 1);
        assert_eq!(plan.projection_components[0].actions.len(), 1);
        let transaction_id = DiscoveryTransactionId::new("recursive-parent-delete");
        prepare_plain_files_discovery_transaction(
            &mut store,
            plan,
            transaction_id.clone(),
            "t0",
            vec![],
        )
        .expect("prepare normalized recursive delete");
        run_steps_to_finalized(&mut store, &transaction_id);

        assert!(!fixture.root.join("Deleted").exists());
        for remote_id in [RemoteId::new("parent"), RemoteId::new("child")] {
            assert!(
                store
                    .get_entity(&fixture.mount.mount_id, &remote_id)
                    .expect("entity lookup")
                    .is_none()
            );
        }
    }
}

#[test]
fn named_leaf_move_to_canonical_path_uses_a_recoverable_container_operation() {
    let fixture = Fixture::new("named-leaf-to-canonical");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    store
        .save_entity(entity_record("page", "Old.md"))
        .expect("save page");
    fs::write(fixture.root.join("Old.md"), "leaf\n").expect("write leaf page");
    let plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![entry("page", EntityKind::Page, "New/page.md")],
    );
    let transaction_id = DiscoveryTransactionId::new("named-leaf-to-canonical");
    let reserved = prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![],
    )
    .expect("prepare named leaf move");
    let operation_types = reserved.plan["components"][0]["operations"]
        .as_array()
        .expect("operations")
        .iter()
        .map(|operation| operation["type"].as_str().expect("operation type"))
        .collect::<Vec<_>>();
    assert_eq!(operation_types, vec!["move", "create_container"]);
    let mut missing_container = reserved.clone();
    missing_container.plan["components"][0]["operations"]
        .as_array_mut()
        .expect("operations")
        .pop();
    assert_eq!(
        validate_plain_files_discovery_transaction_record(&missing_container)
            .expect_err("missing container operation must fail"),
        LocalityError::InvalidState(
            "discovery move `page` has the wrong operation count".to_string()
        )
    );
    let mut redirected_container = reserved.clone();
    redirected_container.plan["components"][0]["operations"][1]["destination"] =
        serde_json::json!("Elsewhere");
    assert_eq!(
        validate_plain_files_discovery_transaction_record(&redirected_container)
            .expect_err("redirected container operation must fail"),
        LocalityError::InvalidState(
            "discovery move `page` operation 1 does not match its action".to_string()
        )
    );

    run_steps_to_finalized(&mut store, &transaction_id);

    assert!(!fixture.root.join("Old.md").exists());
    assert_eq!(
        fs::read_to_string(fixture.root.join("New/page.md")).expect("canonical page"),
        "leaf\n"
    );
    assert_eq!(
        store
            .get_entity(&fixture.mount.mount_id, &RemoteId::new("page"))
            .expect("entity lookup")
            .expect("entity")
            .path,
        PathBuf::from("New/page.md")
    );
}

#[test]
fn named_leaf_container_install_crash_resumes_before_file_install() {
    let fixture = Fixture::new("named-leaf-container-crash");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    store
        .save_entity(entity_record("page", "Old.md"))
        .expect("save page");
    fs::write(fixture.root.join("Old.md"), "leaf\n").expect("write leaf page");
    let plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![entry("page", EntityKind::Page, "New/page.md")],
    );
    let transaction_id = DiscoveryTransactionId::new("named-leaf-container-crash");
    let reserved = prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![],
    )
    .expect("prepare named leaf move");
    let container_id = reserved.plan["components"][0]["operations"]
        .as_array()
        .expect("operations")
        .iter()
        .find(|operation| operation["type"] == "create_container")
        .and_then(|operation| operation["operation_id"].as_str())
        .expect("container operation")
        .to_string();
    for sequence in 1..=20 {
        if step(&mut store, &transaction_id, sequence)
            == (DiscoveryExecutionStep::FilesystemMutation {
                operation_id: container_id.clone(),
                mutation: DiscoveryFilesystemMutation::DestinationInstalled,
            })
        {
            break;
        }
    }
    assert!(fixture.root.join("New").is_dir());
    assert!(!fixture.root.join("New/page.md").exists());
    assert!(!fixture.root.join("Old.md").exists());

    assert_eq!(
        repair_plain_files_discovery_transaction(&mut store, &transaction_id, "t30")
            .expect("resume container install crash"),
        DiscoveryExecutionTerminal::Finalized
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("New/page.md")).expect("canonical page"),
        "leaf\n"
    );
}

#[test]
fn named_leaf_container_move_rolls_back_losslessly_on_commit_drift() {
    let fixture = Fixture::new("named-leaf-container-rollback");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    store
        .save_entity(entity_record("page", "Old.md"))
        .expect("save page");
    fs::write(fixture.root.join("Old.md"), "leaf\n").expect("write leaf page");
    let plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![entry("page", EntityKind::Page, "New/page.md")],
    );
    let transaction_id = DiscoveryTransactionId::new("named-leaf-container-rollback");
    prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![],
    )
    .expect("prepare named leaf move");
    run_steps_to_projected(&mut store, &transaction_id);
    store
        .upsert_hydration_job(HydrationJobRecord::from(HydrationRequest::new(
            fixture.mount.mount_id.clone(),
            RemoteId::new("unrelated"),
            fixture.root.join("Elsewhere/page.md"),
            HydrationState::Hydrated,
            HydrationReason::Policy,
        )))
        .expect("introduce commit drift");

    assert_eq!(
        repair_plain_files_discovery_transaction(&mut store, &transaction_id, "t30")
            .expect("rollback named leaf move"),
        DiscoveryExecutionTerminal::Aborted
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("Old.md")).expect("restored leaf"),
        "leaf\n"
    );
    assert!(!fixture.root.join("New").exists());
    assert_eq!(
        store
            .get_entity(&fixture.mount.mount_id, &RemoteId::new("page"))
            .expect("entity lookup")
            .expect("entity")
            .path,
        PathBuf::from("Old.md")
    );
}

#[test]
fn named_leaf_container_collisions_and_unknown_content_are_preserved() {
    {
        let fixture = Fixture::new("named-leaf-container-collision");
        let mut store = InMemoryStateStore::new();
        store.save_mount(fixture.mount.clone()).expect("save mount");
        store
            .save_entity(entity_record("page", "Old.md"))
            .expect("save page");
        fs::write(fixture.root.join("Old.md"), "leaf\n").expect("write leaf page");
        fs::create_dir(fixture.root.join("New")).expect("create colliding container");
        fs::write(fixture.root.join("New/unknown"), "preserve\n").expect("write collision content");
        let plan = discovery_plan(
            &store,
            &fixture.mount,
            vec![entry("page", EntityKind::Page, "New/page.md")],
        );
        let transaction_id = DiscoveryTransactionId::new("named-leaf-container-collision");

        let error = prepare_plain_files_discovery_transaction(
            &mut store,
            plan,
            transaction_id.clone(),
            "t0",
            vec![],
        )
        .expect_err("existing destination container must fail");
        assert_eq!(
            error,
            LocalityError::InvalidState(format!(
                "discovery destination `{}` already exists outside a staged source",
                fixture.root.join("New").display()
            ))
        );
        assert_eq!(
            fs::read_to_string(fixture.root.join("New/unknown")).expect("preserved collision"),
            "preserve\n"
        );
        assert!(
            store
                .get_discovery_transaction(&transaction_id)
                .expect("transaction lookup")
                .is_none()
        );
    }

    {
        let fixture = Fixture::new("named-leaf-container-unknown-after-install");
        let mut store = InMemoryStateStore::new();
        store.save_mount(fixture.mount.clone()).expect("save mount");
        store
            .save_entity(entity_record("page", "Old.md"))
            .expect("save page");
        fs::write(fixture.root.join("Old.md"), "leaf\n").expect("write leaf page");
        let plan = discovery_plan(
            &store,
            &fixture.mount,
            vec![entry("page", EntityKind::Page, "New/page.md")],
        );
        let transaction_id =
            DiscoveryTransactionId::new("named-leaf-container-unknown-after-install");
        let reserved = prepare_plain_files_discovery_transaction(
            &mut store,
            plan,
            transaction_id.clone(),
            "t0",
            vec![],
        )
        .expect("prepare named leaf move");
        let container_id = reserved.plan["components"][0]["operations"]
            .as_array()
            .expect("operations")
            .iter()
            .find(|operation| operation["type"] == "create_container")
            .and_then(|operation| operation["operation_id"].as_str())
            .expect("container operation")
            .to_string();
        for sequence in 1..=20 {
            if step(&mut store, &transaction_id, sequence)
                == (DiscoveryExecutionStep::FilesystemMutation {
                    operation_id: container_id.clone(),
                    mutation: DiscoveryFilesystemMutation::DestinationInstalled,
                })
            {
                break;
            }
        }
        fs::write(fixture.root.join("New/unknown"), "preserve\n")
            .expect("write unknown container content");

        assert_eq!(
            run_plain_files_discovery_transaction(&mut store, &transaction_id, "t30")
                .expect("classify changed container"),
            DiscoveryExecutionTerminal::NeedsReview
        );
        assert_eq!(
            fs::read_to_string(fixture.root.join("New/unknown"))
                .expect("unknown content preserved"),
            "preserve\n"
        );
    }
}

#[test]
fn committed_repair_upserts_hydration_idempotently_before_cleanup() {
    let fixture = Fixture::new("hydration");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    store
        .save_entity(
            entity_record("page", "Roadmap/page.md").with_hydration(HydrationState::Hydrated),
        )
        .expect("save hydrated entity");
    let transaction_id = DiscoveryTransactionId::new("hydration");
    let plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![entry("page", EntityKind::Page, "Roadmap/page.md")],
    );
    prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![],
    )
    .expect("prepare hydration-only transaction");

    assert_eq!(
        step(&mut store, &transaction_id, 1),
        DiscoveryExecutionStep::Applying
    );
    assert_eq!(
        step(&mut store, &transaction_id, 2),
        DiscoveryExecutionStep::Projected
    );
    assert_eq!(
        step(&mut store, &transaction_id, 3),
        DiscoveryExecutionStep::Committed
    );
    assert_eq!(
        step(&mut store, &transaction_id, 4),
        DiscoveryExecutionStep::ProjectionValidated
    );
    let barrier_record = store
        .get_discovery_transaction(&transaction_id)
        .expect("transaction lookup")
        .expect("transaction");
    let barrier_effects: DiscoveryExecutionEffects =
        serde_json::from_value(barrier_record.effects).expect("decode barrier effects");
    assert!(barrier_effects.projection_validated);
    assert!(barrier_effects.hydration_jobs.is_empty());
    assert!(
        store
            .list_hydration_jobs()
            .expect("hydration jobs")
            .is_empty()
    );
    assert_eq!(
        repair_plain_files_discovery_transaction(&mut store, &transaction_id, "t5")
            .expect("repair committed transaction"),
        DiscoveryExecutionTerminal::Finalized
    );
    let jobs = store.list_hydration_jobs().expect("hydration jobs");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].remote_id, RemoteId::new("page"));
    assert_eq!(
        repair_plain_files_discovery_transaction(&mut store, &transaction_id, "t6")
            .expect("repeat finalized repair"),
        DiscoveryExecutionTerminal::Finalized
    );
    assert_eq!(
        store.list_hydration_jobs().expect("hydration jobs").len(),
        1
    );
}

#[test]
fn projection_validation_barrier_allows_later_hydrated_bytes_without_overwrite() {
    let fixture = Fixture::new("projection-validation-hydration");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    let mut plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![entry("page", EntityKind::Page, "Roadmap/page.md")],
    );
    plan.post_commit
        .push(DiscoveryPostCommitAction::QueueHydration(
            HydrationRequest::new(
                fixture.mount.mount_id.clone(),
                RemoteId::new("page"),
                fixture.root.join("Roadmap/page.md"),
                HydrationState::Hydrated,
                HydrationReason::Policy,
            ),
        ));
    let transaction_id = DiscoveryTransactionId::new("projection-validation-hydration");
    prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![DiscoveryCreateMaterialization::Page {
            remote_id: RemoteId::new("page"),
            document: "stub\n".to_string(),
        }],
    )
    .expect("prepare transaction");
    run_steps_to_committed(&mut store, &transaction_id);
    assert_eq!(
        step_plain_files_discovery_transaction(&mut store, &transaction_id, "t20")
            .expect("record projection barrier"),
        DiscoveryExecutionStep::ProjectionValidated
    );
    fs::write(fixture.root.join("Roadmap/page.md"), "hydrated\n")
        .expect("simulate hydration replacement");

    assert_eq!(
        repair_plain_files_discovery_transaction(&mut store, &transaction_id, "t21")
            .expect("resume after hydration replacement"),
        DiscoveryExecutionTerminal::Finalized
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("Roadmap/page.md")).expect("read hydrated document"),
        "hydrated\n"
    );
    assert_eq!(
        store.list_hydration_jobs().expect("hydration jobs").len(),
        1
    );
}

#[test]
fn committed_repair_rejects_missing_or_mismatched_create_and_move_destinations_before_side_effects()
{
    for operation_kind in ["create", "move"] {
        for corruption in ["missing", "mismatched"] {
            let label = format!("committed-{operation_kind}-{corruption}");
            let fixture = Fixture::new(&label);
            let mut store = InMemoryStateStore::new();
            store.save_mount(fixture.mount.clone()).expect("save mount");
            if operation_kind == "move" {
                store
                    .save_entity(entity_record("page", "Old/page.md"))
                    .expect("save existing page");
                fs::create_dir_all(fixture.root.join("Old")).expect("create old page");
                fs::write(fixture.root.join("Old/page.md"), "original\n").expect("write old page");
            }
            let mut plan = discovery_plan(
                &store,
                &fixture.mount,
                vec![entry("page", EntityKind::Page, "New/page.md")],
            );
            plan.post_commit
                .push(DiscoveryPostCommitAction::QueueHydration(
                    HydrationRequest::new(
                        fixture.mount.mount_id.clone(),
                        RemoteId::new("page"),
                        fixture.root.join("New/page.md"),
                        HydrationState::Hydrated,
                        HydrationReason::Policy,
                    ),
                ));
            let transaction_id = DiscoveryTransactionId::new(&label);
            let materializations = if operation_kind == "create" {
                vec![DiscoveryCreateMaterialization::Page {
                    remote_id: RemoteId::new("page"),
                    document: "original\n".to_string(),
                }]
            } else {
                vec![]
            };
            let reserved = prepare_plain_files_discovery_transaction(
                &mut store,
                plan,
                transaction_id.clone(),
                "t0",
                materializations,
            )
            .expect("prepare transaction");
            let execution: DiscoveryExecutionPlan =
                serde_json::from_value(reserved.plan).expect("decode execution");
            let destination = execution
                .components
                .iter()
                .flat_map(|component| component.operations.iter())
                .find_map(|operation| match (operation_kind, operation) {
                    ("create", DiscoveryExecutionOperation::Create { destination, .. })
                    | ("move", DiscoveryExecutionOperation::Move { destination, .. }) => {
                        Some(execution.mount_root.join(destination))
                    }
                    _ => None,
                })
                .expect("destination operation");
            run_steps_to_committed(&mut store, &transaction_id);

            if corruption == "missing" {
                fs::remove_dir_all(&destination).expect("remove installed destination");
            } else {
                fs::write(destination.join("page.md"), "tampered\n")
                    .expect("replace installed bytes");
            }
            let transaction_before = store
                .get_discovery_transaction(&transaction_id)
                .expect("transaction lookup")
                .expect("transaction");
            let visible_before = filesystem_snapshot(&fixture.root);
            let recovery_before = filesystem_snapshot(&execution.recovery_root);

            assert_eq!(
                repair_plain_files_discovery_transaction(&mut store, &transaction_id, "t30")
                    .expect("classify committed projection drift"),
                DiscoveryExecutionTerminal::NeedsReview,
                "{label}"
            );
            assert_eq!(
                store
                    .get_discovery_transaction(&transaction_id)
                    .expect("transaction lookup")
                    .expect("transaction"),
                transaction_before,
                "{label}"
            );
            assert_eq!(
                filesystem_snapshot(&fixture.root),
                visible_before,
                "{label}"
            );
            assert_eq!(
                filesystem_snapshot(&execution.recovery_root),
                recovery_before,
                "{label}"
            );
            assert!(
                store
                    .list_hydration_jobs()
                    .expect("hydration jobs")
                    .is_empty(),
                "{label}"
            );
        }
    }
}

#[test]
fn committed_repair_rejects_reappeared_delete_source_before_side_effects() {
    let fixture = Fixture::new("committed-delete-source-reappeared");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    store
        .save_entity(entity_record("page", "Deleted/page.md"))
        .expect("save existing page");
    fs::create_dir_all(fixture.root.join("Deleted")).expect("create deleted page");
    fs::write(fixture.root.join("Deleted/page.md"), "original\n").expect("write deleted page");
    let mut plan = discovery_plan_changes(
        &store,
        &fixture.mount,
        vec![BatchObservationChange::Tombstone {
            remote_id: RemoteId::new("page"),
        }],
    );
    plan.post_commit
        .push(DiscoveryPostCommitAction::QueueHydration(
            HydrationRequest::new(
                fixture.mount.mount_id.clone(),
                RemoteId::new("page"),
                fixture.root.join("Deleted/page.md"),
                HydrationState::Hydrated,
                HydrationReason::Policy,
            ),
        ));
    let transaction_id = DiscoveryTransactionId::new("committed-delete-source-reappeared");
    let reserved = prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![],
    )
    .expect("prepare delete transaction");
    let execution: DiscoveryExecutionPlan =
        serde_json::from_value(reserved.plan).expect("decode execution");
    run_steps_to_committed(&mut store, &transaction_id);
    fs::create_dir_all(fixture.root.join("Deleted")).expect("recreate deleted source");
    fs::write(fixture.root.join("Deleted/page.md"), "reappeared\n")
        .expect("write reappeared source");
    let transaction_before = store
        .get_discovery_transaction(&transaction_id)
        .expect("transaction lookup")
        .expect("transaction");
    let visible_before = filesystem_snapshot(&fixture.root);
    let recovery_before = filesystem_snapshot(&execution.recovery_root);

    assert_eq!(
        repair_plain_files_discovery_transaction(&mut store, &transaction_id, "t30")
            .expect("classify committed delete drift"),
        DiscoveryExecutionTerminal::NeedsReview
    );
    assert_eq!(
        store
            .get_discovery_transaction(&transaction_id)
            .expect("transaction lookup")
            .expect("transaction"),
        transaction_before
    );
    assert_eq!(filesystem_snapshot(&fixture.root), visible_before);
    assert_eq!(
        filesystem_snapshot(&execution.recovery_root),
        recovery_before
    );
    assert!(
        store
            .list_hydration_jobs()
            .expect("hydration jobs")
            .is_empty()
    );
}

#[test]
fn delete_recovery_root_is_required_before_commit_and_projection_barrier() {
    for phase in ["projected", "committed"] {
        let fixture = Fixture::new(&format!("delete-recovery-required-{phase}"));
        let mut store = InMemoryStateStore::new();
        store.save_mount(fixture.mount.clone()).expect("save mount");
        store
            .save_entity(entity_record("page", "Deleted/page.md"))
            .expect("save page");
        fs::create_dir_all(fixture.root.join("Deleted")).expect("create deleted page");
        fs::write(fixture.root.join("Deleted/page.md"), "original\n").expect("write deleted page");
        let mut plan = discovery_plan_changes(
            &store,
            &fixture.mount,
            vec![BatchObservationChange::Tombstone {
                remote_id: RemoteId::new("page"),
            }],
        );
        plan.post_commit
            .push(DiscoveryPostCommitAction::QueueHydration(
                HydrationRequest::new(
                    fixture.mount.mount_id.clone(),
                    RemoteId::new("page"),
                    fixture.root.join("Deleted/page.md"),
                    HydrationState::Hydrated,
                    HydrationReason::Policy,
                ),
            ));
        let transaction_id = DiscoveryTransactionId::new(format!("delete-{phase}"));
        let reserved = prepare_plain_files_discovery_transaction(
            &mut store,
            plan,
            transaction_id.clone(),
            "t0",
            vec![],
        )
        .expect("prepare delete");
        let execution: DiscoveryExecutionPlan =
            serde_json::from_value(reserved.plan).expect("decode execution");
        if phase == "projected" {
            run_steps_to_projected(&mut store, &transaction_id);
        } else {
            run_steps_to_committed(&mut store, &transaction_id);
        }
        fs::remove_dir_all(&execution.recovery_root).expect("remove recovery quarantine");
        let before = store
            .get_discovery_transaction(&transaction_id)
            .expect("transaction lookup")
            .expect("transaction");

        assert_eq!(
            repair_plain_files_discovery_transaction(&mut store, &transaction_id, "t30")
                .expect("classify missing recovery quarantine"),
            DiscoveryExecutionTerminal::NeedsReview,
            "{phase}"
        );
        let after = store
            .get_discovery_transaction(&transaction_id)
            .expect("transaction lookup")
            .expect("transaction");
        if phase == "projected" {
            assert_ne!(after.status, DiscoveryTransactionStatus::Committed);
            assert!(
                store
                    .get_entity(&fixture.mount.mount_id, &RemoteId::new("page"))
                    .expect("entity lookup")
                    .is_some()
            );
        } else {
            assert_eq!(after, before);
            assert!(
                !after.effects["projection_validated"]
                    .as_bool()
                    .expect("projection marker")
            );
        }
        assert!(
            store
                .list_hydration_jobs()
                .expect("hydration jobs")
                .is_empty()
        );
        assert!(!execution.recovery_root.exists());
    }
}

#[test]
fn zero_operation_transaction_does_not_require_a_recovery_root() {
    let fixture = Fixture::new("zero-operation-recovery");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    let plan = discovery_plan(&store, &fixture.mount, vec![]);
    let transaction_id = DiscoveryTransactionId::new("zero-operation-recovery");
    let reserved = prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![],
    )
    .expect("prepare zero-operation transaction");
    let execution: DiscoveryExecutionPlan =
        serde_json::from_value(reserved.plan).expect("decode execution");

    assert!(execution.components.is_empty());
    assert!(!execution.recovery_root.exists());
    assert_eq!(
        run_plain_files_discovery_transaction(&mut store, &transaction_id, "t1")
            .expect("run zero-operation transaction"),
        DiscoveryExecutionTerminal::Finalized
    );
    assert!(!execution.recovery_root.exists());
}

#[test]
fn projected_commit_drift_rolls_projection_back_and_aborts() {
    let fixture = Fixture::new("rollback-commit-drift");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    store
        .save_entity(entity_record("page", "Old/page.md"))
        .expect("save entity");
    fs::create_dir_all(fixture.root.join("Old")).expect("create old page");
    fs::write(fixture.root.join("Old/page.md"), "original\n").expect("write old page");
    let transaction_id = DiscoveryTransactionId::new("rollback-commit-drift");
    let plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![entry("page", EntityKind::Page, "New/page.md")],
    );
    prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![],
    )
    .expect("prepare move");
    for sequence in 1..=7 {
        let outcome = step(&mut store, &transaction_id, sequence);
        if sequence == 7 {
            assert_eq!(outcome, DiscoveryExecutionStep::Projected);
        }
    }
    store
        .upsert_hydration_job(HydrationJobRecord::from(HydrationRequest::new(
            fixture.mount.mount_id.clone(),
            RemoteId::new("unrelated"),
            "Elsewhere/page.md",
            HydrationState::Hydrated,
            HydrationReason::Policy,
        )))
        .expect("change reserved hydration state");

    assert_eq!(
        repair_plain_files_discovery_transaction(&mut store, &transaction_id, "t8")
            .expect("repair projected commit drift"),
        DiscoveryExecutionTerminal::Aborted
    );

    assert_eq!(
        store
            .get_discovery_transaction(&transaction_id)
            .expect("transaction lookup")
            .expect("transaction")
            .status,
        DiscoveryTransactionStatus::Aborted
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("Old/page.md")).expect("restored page"),
        "original\n"
    );
    assert!(!fixture.root.join("New").exists());
    assert_eq!(
        store
            .get_entity(&fixture.mount.mount_id, &RemoteId::new("page"))
            .expect("entity")
            .expect("existing entity")
            .path,
        PathBuf::from("Old/page.md")
    );
}

#[test]
fn committed_cleanup_refuses_unknown_recovery_content_without_rolling_back() {
    let fixture = Fixture::new("committed-cleanup-guard");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    store
        .save_entity(entity_record("page", "Deleted/page.md"))
        .expect("save entity");
    fs::create_dir_all(fixture.root.join("Deleted")).expect("create deleted page");
    fs::write(fixture.root.join("Deleted/page.md"), "deleted\n").expect("write deleted page");
    let transaction_id = DiscoveryTransactionId::new("committed-cleanup-guard");
    let plan = discovery_plan_changes(
        &store,
        &fixture.mount,
        vec![BatchObservationChange::Tombstone {
            remote_id: RemoteId::new("page"),
        }],
    );
    let reserved = prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![],
    )
    .expect("prepare delete");
    let execution: DiscoveryExecutionPlan =
        serde_json::from_value(reserved.plan).expect("execution plan");
    for sequence in 1..=7 {
        let outcome = step(&mut store, &transaction_id, sequence);
        if sequence == 7 {
            assert_eq!(outcome, DiscoveryExecutionStep::Committed);
        }
    }
    fs::write(
        execution.recovery_root.join("unexpected"),
        "do not delete\n",
    )
    .expect("write unknown recovery content");

    let error = step_plain_files_discovery_transaction(&mut store, &transaction_id, "t8")
        .expect_err("unknown recovery content must block cleanup");

    assert!(error.to_string().contains("unexpected recovery entry"));
    assert!(execution.recovery_root.join("unexpected").exists());
    assert_eq!(
        store
            .get_discovery_transaction(&transaction_id)
            .expect("transaction lookup")
            .expect("transaction")
            .status,
        DiscoveryTransactionStatus::Committed
    );
    assert!(!fixture.root.join("Deleted").exists());

    fs::remove_file(execution.recovery_root.join("unexpected"))
        .expect("remove reviewed recovery content");
    run_steps_to_finalized_from(&mut store, &transaction_id, 9);
    assert!(!execution.recovery_root.exists());
}

#[test]
fn recovery_root_appearing_after_reservation_is_never_adopted() {
    let fixture = Fixture::new("recovery-root-after-reservation");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    let transaction_id = DiscoveryTransactionId::new("recovery-root-after-reservation");
    let plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![entry("page", EntityKind::Page, "Roadmap/page.md")],
    );
    let reserved = prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![DiscoveryCreateMaterialization::Page {
            remote_id: RemoteId::new("page"),
            document: "roadmap\n".to_string(),
        }],
    )
    .expect("prepare transaction");
    let execution: DiscoveryExecutionPlan =
        serde_json::from_value(reserved.plan.clone()).expect("decode execution");
    fs::create_dir_all(&execution.recovery_root).expect("create colliding recovery root");
    let unknown = execution.recovery_root.join("unknown");
    fs::write(&unknown, "preserve\n").expect("write unknown recovery content");

    assert_eq!(
        step_plain_files_discovery_transaction(&mut store, &transaction_id, "t1")
            .expect_err("colliding recovery root must fail before applying"),
        LocalityError::InvalidState(format!(
            "discovery recovery root `{}` already exists",
            execution.recovery_root.display()
        ))
    );
    assert_eq!(
        store
            .get_discovery_transaction(&transaction_id)
            .expect("transaction lookup")
            .expect("transaction"),
        reserved
    );
    assert_eq!(
        fs::read_to_string(&unknown).expect("unknown content remains"),
        "preserve\n"
    );
    assert!(!fixture.root.join("Roadmap").exists());
}

#[test]
fn unknown_recovery_content_appearing_during_execution_blocks_commit_and_cleanup() {
    let fixture = Fixture::new("unknown-recovery-before-commit");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    let transaction_id = DiscoveryTransactionId::new("unknown-recovery-before-commit");
    let plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![entry("page", EntityKind::Page, "Roadmap/page.md")],
    );
    let reserved = prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![DiscoveryCreateMaterialization::Page {
            remote_id: RemoteId::new("page"),
            document: "roadmap\n".to_string(),
        }],
    )
    .expect("prepare transaction");
    let execution: DiscoveryExecutionPlan =
        serde_json::from_value(reserved.plan).expect("decode execution");
    for sequence in 1..=7 {
        let outcome = step(&mut store, &transaction_id, sequence);
        if sequence == 7 {
            assert_eq!(outcome, DiscoveryExecutionStep::Projected);
        }
    }
    let unknown = execution.recovery_root.join("unknown");
    fs::write(&unknown, "preserve\n").expect("write unknown recovery content");

    assert_eq!(
        step(&mut store, &transaction_id, 8),
        DiscoveryExecutionStep::RollbackStarted
    );
    assert_eq!(
        store
            .get_discovery_transaction(&transaction_id)
            .expect("transaction lookup")
            .expect("transaction")
            .status,
        DiscoveryTransactionStatus::Projected
    );
    assert!(
        store
            .get_entity(&fixture.mount.mount_id, &RemoteId::new("page"))
            .expect("entity lookup")
            .is_none()
    );
    assert_eq!(
        run_plain_files_discovery_transaction(&mut store, &transaction_id, "t9")
            .expect("rollback transaction"),
        DiscoveryExecutionTerminal::NeedsReview
    );
    assert!(!fixture.root.join("Roadmap").exists());
    assert_eq!(
        fs::read_to_string(&unknown).expect("unknown content remains"),
        "preserve\n"
    );
}

#[test]
fn changed_precommit_source_enters_repair_pending_without_deleting_content() {
    let fixture = Fixture::new("repair-pending");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    store
        .save_entity(entity_record("page", "Old/page.md"))
        .expect("save entity");
    fs::create_dir_all(fixture.root.join("Old")).expect("create old page");
    fs::write(fixture.root.join("Old/page.md"), "original\n").expect("write old page");
    let transaction_id = DiscoveryTransactionId::new("repair-pending");
    let plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![entry("page", EntityKind::Page, "New/page.md")],
    );
    prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![],
    )
    .expect("prepare move");
    assert_eq!(
        step(&mut store, &transaction_id, 1),
        DiscoveryExecutionStep::Applying
    );
    assert!(matches!(
        step(&mut store, &transaction_id, 2),
        DiscoveryExecutionStep::OperationPrepared { .. }
    ));
    fs::write(fixture.root.join("Old/page.md"), "locally changed\n").expect("change source");

    assert_eq!(
        step(&mut store, &transaction_id, 3),
        DiscoveryExecutionStep::RollbackStarted
    );
    assert_eq!(
        step(&mut store, &transaction_id, 4),
        DiscoveryExecutionStep::RepairPending
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("Old/page.md")).expect("preserved source"),
        "locally changed\n"
    );
    assert_eq!(
        store
            .get_discovery_transaction(&transaction_id)
            .expect("transaction lookup")
            .expect("transaction")
            .status,
        DiscoveryTransactionStatus::RepairPending
    );
}

#[test]
fn public_run_and_repair_apis_handle_lifecycle_states_conservatively() {
    let run_fixture = Fixture::new("public-run");
    let mut run_store = InMemoryStateStore::new();
    run_store
        .save_mount(run_fixture.mount.clone())
        .expect("save run mount");
    let run_id = DiscoveryTransactionId::new("public-run");
    let run_plan = discovery_plan(
        &run_store,
        &run_fixture.mount,
        vec![entry("page", EntityKind::Page, "Roadmap/page.md")],
    );
    prepare_plain_files_discovery_transaction(
        &mut run_store,
        run_plan,
        run_id.clone(),
        "t0",
        vec![DiscoveryCreateMaterialization::Page {
            remote_id: RemoteId::new("page"),
            document: "roadmap\n".to_string(),
        }],
    )
    .expect("prepare run transaction");

    assert_eq!(
        run_plain_files_discovery_transaction(&mut run_store, &run_id, "t1")
            .expect("run transaction"),
        DiscoveryExecutionTerminal::Finalized
    );
    assert_eq!(
        repair_plain_files_discovery_transaction(&mut run_store, &run_id, "t2")
            .expect("repair finalized transaction"),
        DiscoveryExecutionTerminal::Finalized
    );

    let reserved_fixture = Fixture::new("repair-reserved");
    let mut reserved_store = InMemoryStateStore::new();
    reserved_store
        .save_mount(reserved_fixture.mount.clone())
        .expect("save reserved mount");
    let reserved_id = DiscoveryTransactionId::new("repair-reserved");
    let reserved_plan = discovery_plan(
        &reserved_store,
        &reserved_fixture.mount,
        vec![entry("page", EntityKind::Page, "Never/page.md")],
    );
    prepare_plain_files_discovery_transaction(
        &mut reserved_store,
        reserved_plan,
        reserved_id.clone(),
        "t0",
        vec![DiscoveryCreateMaterialization::Page {
            remote_id: RemoteId::new("page"),
            document: "never projected\n".to_string(),
        }],
    )
    .expect("prepare reserved transaction");

    assert_eq!(
        repair_plain_files_discovery_transaction(&mut reserved_store, &reserved_id, "t1")
            .expect("repair reserved transaction"),
        DiscoveryExecutionTerminal::Aborted
    );
    assert!(!reserved_fixture.root.join("Never").exists());
    assert_eq!(
        reserved_store
            .get_discovery_transaction(&reserved_id)
            .expect("reserved transaction lookup")
            .expect("reserved transaction")
            .status,
        DiscoveryTransactionStatus::Aborted
    );

    let review_fixture = Fixture::new("repair-review");
    let mut review_store = InMemoryStateStore::new();
    review_store
        .save_mount(review_fixture.mount.clone())
        .expect("save review mount");
    let review_id = DiscoveryTransactionId::new("repair-review");
    let review_plan = discovery_plan(
        &review_store,
        &review_fixture.mount,
        vec![entry("page", EntityKind::Page, "Review/page.md")],
    );
    prepare_plain_files_discovery_transaction(
        &mut review_store,
        review_plan,
        review_id.clone(),
        "t0",
        vec![DiscoveryCreateMaterialization::Page {
            remote_id: RemoteId::new("page"),
            document: "review\n".to_string(),
        }],
    )
    .expect("prepare review transaction");
    review_store
        .mark_discovery_transaction_applying(&review_id, "t1")
        .expect("mark applying");
    review_store
        .mark_discovery_transaction_repair_pending(
            &review_id,
            DiscoveryTransactionStatus::Applying,
            serde_json::json!({"reason": "external review"}),
            "t2",
        )
        .expect("mark repair pending");

    assert_eq!(
        repair_plain_files_discovery_transaction(&mut review_store, &review_id, "t3")
            .expect("inspect repair pending transaction"),
        DiscoveryExecutionTerminal::NeedsReview
    );
}

#[test]
fn reserved_repair_aborts_after_local_source_edit_without_refingerprinting_or_touching_bytes() {
    let fixture = Fixture::new("reserved-local-edit");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    store
        .save_entity(entity_record("page", "Old/page.md"))
        .expect("save entity");
    fs::create_dir_all(fixture.root.join("Old")).expect("create old page");
    fs::write(fixture.root.join("Old/page.md"), "reserved\n").expect("write source");
    let transaction_id = DiscoveryTransactionId::new("reserved-local-edit");
    let plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![entry("page", EntityKind::Page, "New/page.md")],
    );
    prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![],
    )
    .expect("prepare reserved move");
    fs::write(
        fixture.root.join("Old/page.md"),
        "edited after reservation\n",
    )
    .expect("edit reserved source");

    assert_eq!(
        repair_plain_files_discovery_transaction(&mut store, &transaction_id, "t1")
            .expect("abort stale reservation"),
        DiscoveryExecutionTerminal::Aborted
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("Old/page.md")).expect("preserved edit"),
        "edited after reservation\n"
    );
    assert!(!fixture.root.join("New").exists());
    let record = store
        .get_discovery_transaction(&transaction_id)
        .expect("transaction lookup")
        .expect("transaction");
    assert_eq!(record.status, DiscoveryTransactionStatus::Aborted);
    assert!(!record.active);

    let next_id = DiscoveryTransactionId::new("reserved-local-edit-next");
    let next_plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![entry("page", EntityKind::Page, "Next/page.md")],
    );
    prepare_plain_files_discovery_transaction(&mut store, next_plan, next_id, "t2", vec![])
        .expect("aborted reservation must unblock mount");
}

#[test]
fn repair_resumes_applying_and_lossless_repair_pending_rename_boundaries() {
    for (label, mark_repair_pending, expected) in [
        (
            "repair-applying-rename",
            false,
            DiscoveryExecutionTerminal::Finalized,
        ),
        (
            "repair-pending-rename",
            true,
            DiscoveryExecutionTerminal::Aborted,
        ),
    ] {
        let fixture = Fixture::new(label);
        let mut store = InMemoryStateStore::new();
        store.save_mount(fixture.mount.clone()).expect("save mount");
        store
            .save_entity(entity_record("page", "Old/page.md"))
            .expect("save entity");
        fs::create_dir_all(fixture.root.join("Old")).expect("create old page");
        fs::write(fixture.root.join("Old/page.md"), "original\n").expect("write old page");
        let transaction_id = DiscoveryTransactionId::new(label);
        let plan = discovery_plan(
            &store,
            &fixture.mount,
            vec![entry("page", EntityKind::Page, "New/page.md")],
        );
        prepare_plain_files_discovery_transaction(
            &mut store,
            plan,
            transaction_id.clone(),
            "t0",
            vec![],
        )
        .expect("prepare move");
        assert_eq!(
            step(&mut store, &transaction_id, 1),
            DiscoveryExecutionStep::Applying
        );
        assert!(matches!(
            step(&mut store, &transaction_id, 2),
            DiscoveryExecutionStep::OperationPrepared { .. }
        ));
        assert!(matches!(
            step(&mut store, &transaction_id, 3),
            DiscoveryExecutionStep::FilesystemMutation {
                mutation: DiscoveryFilesystemMutation::SourceStaged,
                ..
            }
        ));

        if mark_repair_pending {
            let record = store
                .get_discovery_transaction(&transaction_id)
                .expect("transaction lookup")
                .expect("transaction");
            let mut effects: DiscoveryExecutionEffects =
                serde_json::from_value(record.effects).expect("decode effects");
            effects.rollback_reason = Some("resume lossless rollback".to_string());
            store
                .record_discovery_transaction_effects(
                    &transaction_id,
                    DiscoveryTransactionStatus::Applying,
                    serde_json::to_value(effects).expect("encode effects"),
                    "t4",
                )
                .expect("record rollback reason");
            store
                .mark_discovery_transaction_repair_pending(
                    &transaction_id,
                    DiscoveryTransactionStatus::Applying,
                    serde_json::json!({"reason": "restart repair"}),
                    "t5",
                )
                .expect("mark repair pending");
        }

        assert_eq!(
            repair_plain_files_discovery_transaction(&mut store, &transaction_id, "t6")
                .expect("repair rename boundary"),
            expected,
            "{label}"
        );
        let expected_path = if mark_repair_pending {
            "Old/page.md"
        } else {
            "New/page.md"
        };
        assert_eq!(
            fs::read_to_string(fixture.root.join(expected_path)).expect("recovered page"),
            "original\n",
            "{label}"
        );
    }
}

#[test]
fn active_repair_filters_provider_transactions_and_propagates_newer_state_without_mutation() {
    let fixture = Fixture::new("active-repair-filter");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(fixture.mount.clone())
        .expect("save plain mount");
    let plain_id = DiscoveryTransactionId::new("plain-reserved");
    let plain_plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![entry("plain", EntityKind::Page, "Plain/page.md")],
    );
    prepare_plain_files_discovery_transaction(
        &mut store,
        plain_plan,
        plain_id.clone(),
        "t0",
        vec![DiscoveryCreateMaterialization::Page {
            remote_id: RemoteId::new("plain"),
            document: "plain\n".to_string(),
        }],
    )
    .expect("prepare plain transaction");

    let provider_mount = MountConfig::new(
        MountId::new("provider-main"),
        "linear",
        fixture.sandbox.join("provider"),
    )
    .projection(ProjectionMode::MacosFileProvider);
    fs::create_dir_all(&provider_mount.root).expect("create provider root");
    store
        .save_mount(provider_mount.clone())
        .expect("save provider mount");
    let provider_id = DiscoveryTransactionId::new("provider-reserved");
    reserve_opaque_transaction(&mut store, &provider_mount, provider_id.clone(), 1);
    let provider_before = store
        .get_discovery_transaction(&provider_id)
        .expect("provider transaction lookup")
        .expect("provider transaction");
    let provider_reservation_before = store
        .capture_discovery_reservation(&provider_mount.mount_id)
        .expect("capture provider state");
    let provider_files_before = filesystem_snapshot(&provider_mount.root);

    let results = repair_active_plain_files_discovery_transactions(&mut store, "t1")
        .expect("repair active plain transactions");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].transaction_id, plain_id);
    assert_eq!(results[0].outcome, DiscoveryExecutionTerminal::Aborted);
    let provider_after_filter = store
        .get_discovery_transaction(&provider_id)
        .expect("provider transaction lookup")
        .expect("provider transaction");
    assert_eq!(provider_after_filter, provider_before);
    assert_eq!(
        store
            .capture_discovery_reservation(&provider_mount.mount_id)
            .expect("capture provider state after filtering"),
        provider_reservation_before
    );
    assert_eq!(
        filesystem_snapshot(&provider_mount.root),
        provider_files_before
    );
    assert_eq!(
        store
            .get_discovery_transaction(&provider_id)
            .expect("provider transaction lookup")
            .expect("provider transaction")
            .status,
        DiscoveryTransactionStatus::Reserved
    );
    let provider_error = repair_plain_files_discovery_transaction(&mut store, &provider_id, "t2")
        .expect_err("direct provider repair must fail clearly");
    assert_eq!(
        provider_error,
        LocalityError::InvalidState(
            "discovery transaction `provider-reserved` is not a plain-files projection".to_string()
        )
    );
    assert_eq!(
        store
            .get_discovery_transaction(&provider_id)
            .expect("provider transaction lookup")
            .expect("provider transaction"),
        provider_before
    );
    store
        .mark_discovery_transaction_applying(&provider_id, "t3")
        .expect("mark opaque provider applying");
    let applying_provider_before = store
        .get_discovery_transaction(&provider_id)
        .expect("provider transaction lookup")
        .expect("provider transaction");
    let provider_step_error =
        step_plain_files_discovery_transaction(&mut store, &provider_id, "t4")
            .expect_err("plain-files step must reject provider transaction");
    assert_eq!(
        provider_step_error,
        LocalityError::InvalidState(
            "discovery transaction `provider-reserved` is not a plain-files projection".to_string()
        )
    );
    assert_eq!(
        store
            .get_discovery_transaction(&provider_id)
            .expect("provider transaction lookup")
            .expect("provider transaction"),
        applying_provider_before
    );
    assert_eq!(
        filesystem_snapshot(&provider_mount.root),
        provider_files_before
    );

    let newer_fixture = Fixture::new("repair-newer-state");
    let mut newer_store = InMemoryStateStore::new();
    newer_store
        .save_mount(newer_fixture.mount.clone())
        .expect("save newer mount");
    let newer_id = DiscoveryTransactionId::new("newer-reserved");
    reserve_opaque_transaction(&mut newer_store, &newer_fixture.mount, newer_id.clone(), 2);
    let newer_before = newer_store
        .get_discovery_transaction(&newer_id)
        .expect("newer transaction lookup")
        .expect("newer transaction");
    let newer_reservation_before = newer_store
        .capture_discovery_reservation(&newer_fixture.mount.mount_id)
        .expect("capture newer state");
    let newer_files_before = filesystem_snapshot(&newer_fixture.root);

    let error = repair_plain_files_discovery_transaction(&mut newer_store, &newer_id, "t1")
        .expect_err("newer execution state must fail");
    assert_eq!(
        error,
        LocalityError::UpdateRequired {
            component: "daemon:discovery_execution_plan".to_string(),
            found: 2,
            supported: 1,
        }
    );
    assert_eq!(
        newer_store
            .get_discovery_transaction(&newer_id)
            .expect("newer transaction lookup")
            .expect("newer transaction"),
        newer_before
    );
    assert_eq!(
        newer_store
            .capture_discovery_reservation(&newer_fixture.mount.mount_id)
            .expect("capture newer state after error"),
        newer_reservation_before
    );
    assert_eq!(filesystem_snapshot(&newer_fixture.root), newer_files_before);
}

#[test]
fn terminal_outcomes_ignore_mutable_bytes_but_still_reject_future_versions_without_mutation() {
    let fixture = Fixture::new("terminal-stability");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    let transaction_id = DiscoveryTransactionId::new("terminal-stability");
    let plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![entry("page", EntityKind::Page, "Roadmap/page.md")],
    );
    prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![DiscoveryCreateMaterialization::Page {
            remote_id: RemoteId::new("page"),
            document: "original\n".to_string(),
        }],
    )
    .expect("prepare terminal transaction");
    assert_eq!(
        run_plain_files_discovery_transaction(&mut store, &transaction_id, "t1")
            .expect("finalize transaction"),
        DiscoveryExecutionTerminal::Finalized
    );
    fs::write(
        fixture.root.join("Roadmap/page.md"),
        "changed after finalization\n",
    )
    .expect("change finalized bytes");
    assert_eq!(
        run_plain_files_discovery_transaction(&mut store, &transaction_id, "t2")
            .expect("read stable run terminal"),
        DiscoveryExecutionTerminal::Finalized
    );
    assert_eq!(
        repair_plain_files_discovery_transaction(&mut store, &transaction_id, "t3")
            .expect("read stable repair terminal"),
        DiscoveryExecutionTerminal::Finalized
    );
    fs::remove_dir_all(&fixture.root).expect("remove finalized mount root");
    assert_eq!(
        step_plain_files_discovery_transaction(&mut store, &transaction_id, "t4")
            .expect("read stable step terminal without mount root"),
        DiscoveryExecutionStep::Finalized
    );

    let aborted_fixture = Fixture::new("aborted-terminal-step");
    let mut aborted_store = InMemoryStateStore::new();
    aborted_store
        .save_mount(aborted_fixture.mount.clone())
        .expect("save aborted mount");
    let aborted_id = DiscoveryTransactionId::new("aborted-terminal-step");
    let aborted_plan = discovery_plan(
        &aborted_store,
        &aborted_fixture.mount,
        vec![entry("page", EntityKind::Page, "Roadmap/page.md")],
    );
    prepare_plain_files_discovery_transaction(
        &mut aborted_store,
        aborted_plan,
        aborted_id.clone(),
        "t0",
        vec![DiscoveryCreateMaterialization::Page {
            remote_id: RemoteId::new("page"),
            document: "roadmap\n".to_string(),
        }],
    )
    .expect("prepare aborted transaction");
    assert_eq!(
        repair_plain_files_discovery_transaction(&mut aborted_store, &aborted_id, "t1")
            .expect("abort reserved transaction"),
        DiscoveryExecutionTerminal::Aborted
    );
    let aborted_before = aborted_store
        .get_discovery_transaction(&aborted_id)
        .expect("aborted transaction lookup")
        .expect("aborted transaction");
    fs::remove_dir_all(&aborted_fixture.root).expect("remove aborted mount root");
    assert_eq!(
        step_plain_files_discovery_transaction(&mut aborted_store, &aborted_id, "t2")
            .expect("read stable aborted step terminal without mount root"),
        DiscoveryExecutionStep::Aborted
    );
    assert_eq!(
        aborted_store
            .get_discovery_transaction(&aborted_id)
            .expect("aborted transaction lookup")
            .expect("aborted transaction"),
        aborted_before
    );

    let future_fixture = Fixture::new("future-terminal");
    let mut future_store = InMemoryStateStore::new();
    future_store
        .save_mount(future_fixture.mount.clone())
        .expect("save future mount");
    let future_id = DiscoveryTransactionId::new("future-terminal");
    reserve_opaque_transaction(
        &mut future_store,
        &future_fixture.mount,
        future_id.clone(),
        2,
    );
    future_store
        .mark_discovery_transaction_applying(&future_id, "t1")
        .expect("mark future applying");
    future_store
        .mark_discovery_transaction_projected(
            &future_id,
            DiscoveryTransactionStatus::Applying,
            "t2",
        )
        .expect("mark future projected");
    future_store
        .commit_discovery_transaction(&future_id, "t3")
        .expect("commit future transaction");
    future_store
        .mark_discovery_transaction_finalized(&future_id, "t4")
        .expect("finalize future transaction");
    let before = future_store
        .get_discovery_transaction(&future_id)
        .expect("future transaction lookup")
        .expect("future transaction");
    let before_reservation = future_store
        .capture_discovery_reservation(&future_fixture.mount.mount_id)
        .expect("capture future state");
    fs::remove_dir_all(&future_fixture.root).expect("remove future terminal mount root");

    assert_eq!(
        step_plain_files_discovery_transaction(&mut future_store, &future_id, "t5")
            .expect_err("future terminal step must require an update"),
        LocalityError::UpdateRequired {
            component: "daemon:discovery_execution_plan".to_string(),
            found: 2,
            supported: 1,
        }
    );

    assert_eq!(
        repair_plain_files_discovery_transaction(&mut future_store, &future_id, "t6")
            .expect_err("future terminal must require an update"),
        LocalityError::UpdateRequired {
            component: "daemon:discovery_execution_plan".to_string(),
            found: 2,
            supported: 1,
        }
    );
    assert_eq!(
        future_store
            .get_discovery_transaction(&future_id)
            .expect("future transaction lookup")
            .expect("future transaction"),
        before
    );
    assert_eq!(
        future_store
            .capture_discovery_reservation(&future_fixture.mount.mount_id)
            .expect("capture future state after error"),
        before_reservation
    );
    assert!(!future_fixture.root.exists());
}

#[test]
fn decoder_rejects_newer_state_and_tampered_operation_coverage() {
    let fixture = Fixture::new("decoder-tamper");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
    let transaction_id = DiscoveryTransactionId::new("decoder-tamper");
    let plan = discovery_plan(
        &store,
        &fixture.mount,
        vec![entry("page", EntityKind::Page, "Roadmap/page.md")],
    );
    let record = prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id,
        "t0",
        vec![DiscoveryCreateMaterialization::Page {
            remote_id: RemoteId::new("page"),
            document: "roadmap\n".to_string(),
        }],
    )
    .expect("prepare create");

    let mut newer_plan = record.clone();
    newer_plan.plan["state_version"] = serde_json::json!(2);
    assert_eq!(
        validate_plain_files_discovery_transaction_record(&newer_plan)
            .expect_err("newer plan must fail"),
        LocalityError::UpdateRequired {
            component: "daemon:discovery_execution_plan".to_string(),
            found: 2,
            supported: 1,
        }
    );

    let mut newer_effects = record.clone();
    newer_effects.effects["min_reader_version"] = serde_json::json!(2);
    newer_effects.effects["state_version"] = serde_json::json!(2);
    assert_eq!(
        validate_plain_files_discovery_transaction_record(&newer_effects)
            .expect_err("newer effects must fail"),
        LocalityError::UpdateRequired {
            component: "daemon:discovery_execution_effects".to_string(),
            found: 2,
            supported: 1,
        }
    );

    let mut missing_operation = record.clone();
    missing_operation.plan["components"][0]["operations"] = serde_json::json!([]);
    assert_eq!(
        validate_plain_files_discovery_transaction_record(&missing_operation)
            .expect_err("missing operation must fail"),
        LocalityError::InvalidState(
            "discovery create `page` does not have exactly one operation".to_string()
        )
    );

    let mut cleared_components = record.clone();
    cleared_components.plan["components"] = serde_json::json!([]);
    assert_eq!(
        validate_plain_files_discovery_transaction_record(&cleared_components)
            .expect_err("cleared components must fail"),
        LocalityError::InvalidState(
            "discovery projection actions do not cover structural commit changes".to_string()
        )
    );

    let mut impossible_effect = record.clone();
    let operation = impossible_effect.plan["components"][0]["operations"][0].clone();
    impossible_effect.effects["operations"] = serde_json::json!([{
        "operation_id": operation["operation_id"].clone(),
        "state": "cleaned",
        "fingerprint": operation["expected_fingerprint"].clone(),
    }]);
    assert_eq!(
        validate_plain_files_discovery_transaction_record(&impossible_effect)
            .expect_err("unreachable operation state must fail"),
        LocalityError::InvalidState(
            "unknown variant `cleaned`, expected one of `prepared`, `staged`, `installed`, `rolled_back`"
                .to_string()
        )
    );

    let mut redirected = record;
    redirected.plan["components"][0]["operations"][0]["destination"] =
        serde_json::json!("Elsewhere");
    assert_eq!(
        validate_plain_files_discovery_transaction_record(&redirected)
            .expect_err("redirected operation must fail"),
        LocalityError::InvalidState(
            "discovery create `page` operation does not match its action".to_string()
        )
    );
}

#[test]
fn sqlite_execution_recovers_when_reopened_between_every_microstep() {
    let fixture = Fixture::new("sqlite-restart");
    let state_root = fixture.sandbox.join("state");
    let transaction_id = DiscoveryTransactionId::new("sqlite-restart");
    {
        let mut store = SqliteStateStore::open(state_root.clone()).expect("open sqlite");
        store.save_mount(fixture.mount.clone()).expect("save mount");
        let plan = discovery_plan_sqlite(
            &store,
            &fixture.mount,
            vec![entry("page", EntityKind::Page, "Roadmap/page.md")],
        );
        prepare_plain_files_discovery_transaction(
            &mut store,
            plan,
            transaction_id.clone(),
            "t0",
            vec![DiscoveryCreateMaterialization::Page {
                remote_id: RemoteId::new("page"),
                document: "roadmap\n".to_string(),
            }],
        )
        .expect("prepare sqlite transaction");
    }

    for sequence in 1..=20 {
        let mut store = SqliteStateStore::open(state_root.clone()).expect("reopen sqlite");
        let outcome = step_plain_files_discovery_transaction(
            &mut store,
            &transaction_id,
            &format!("t{sequence}"),
        )
        .expect("restart-safe execution step");
        if outcome == DiscoveryExecutionStep::Finalized {
            break;
        }
    }

    let store = SqliteStateStore::open(state_root).expect("final sqlite reopen");
    assert_eq!(
        store
            .get_discovery_transaction(&transaction_id)
            .expect("transaction lookup")
            .expect("transaction")
            .status,
        DiscoveryTransactionStatus::Finalized
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("Roadmap/page.md")).expect("created page"),
        "roadmap\n"
    );
}

#[test]
fn sqlite_swap_recovers_when_reopened_after_each_rename_before_effect_recording() {
    let fixture = Fixture::new("sqlite-swap-restart");
    let state_root = fixture.sandbox.join("state");
    let transaction_id = DiscoveryTransactionId::new("sqlite-swap-restart");
    {
        let mut store = SqliteStateStore::open(state_root.clone()).expect("open sqlite");
        store.save_mount(fixture.mount.clone()).expect("save mount");
        store
            .save_entity(entity_record("page-a", "A/page.md"))
            .expect("save page A");
        store
            .save_entity(entity_record("page-b", "B/page.md"))
            .expect("save page B");
        fs::create_dir_all(fixture.root.join("A")).expect("create A");
        fs::create_dir_all(fixture.root.join("B")).expect("create B");
        fs::write(fixture.root.join("A/page.md"), "contents-a\n").expect("write A");
        fs::write(fixture.root.join("B/page.md"), "contents-b\n").expect("write B");
        let plan = discovery_plan_sqlite(
            &store,
            &fixture.mount,
            vec![
                entry("page-a", EntityKind::Page, "B/page.md"),
                entry("page-b", EntityKind::Page, "A/page.md"),
            ],
        );
        prepare_plain_files_discovery_transaction(
            &mut store,
            plan,
            transaction_id.clone(),
            "t0",
            vec![],
        )
        .expect("prepare sqlite swap");
    }

    let mut staged_renames = 0;
    let mut finalized = false;
    for sequence in 1..=48 {
        let mut store = SqliteStateStore::open(state_root.clone()).expect("reopen sqlite");
        let outcome = step_plain_files_discovery_transaction(
            &mut store,
            &transaction_id,
            &format!("t{sequence}"),
        )
        .expect("restart-safe swap step");
        if matches!(
            outcome,
            DiscoveryExecutionStep::FilesystemMutation {
                mutation: DiscoveryFilesystemMutation::SourceStaged,
                ..
            }
        ) {
            staged_renames += 1;
            let record = store
                .get_discovery_transaction(&transaction_id)
                .expect("transaction lookup")
                .expect("transaction");
            let effects: DiscoveryExecutionEffects =
                serde_json::from_value(record.effects).expect("decode effects");
            assert_eq!(
                effects
                    .operations
                    .iter()
                    .filter(|effect| effect.state == DiscoveryOperationEffectState::Prepared)
                    .count(),
                1,
                "the rename must precede its durable staged effect"
            );
        }
        if outcome == DiscoveryExecutionStep::Finalized {
            finalized = true;
            break;
        }
    }

    assert!(
        finalized,
        "swap transaction must finalize after restart recovery"
    );
    assert_eq!(staged_renames, 2);
    assert_eq!(
        fs::read_to_string(fixture.root.join("A/page.md")).expect("page at A"),
        "contents-b\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("B/page.md")).expect("page at B"),
        "contents-a\n"
    );
    let store = SqliteStateStore::open(state_root).expect("final sqlite reopen");
    assert_eq!(
        store
            .get_entity(&fixture.mount.mount_id, &RemoteId::new("page-a"))
            .expect("page A lookup")
            .expect("page A")
            .path,
        PathBuf::from("B/page.md")
    );
    assert_eq!(
        store
            .get_entity(&fixture.mount.mount_id, &RemoteId::new("page-b"))
            .expect("page B lookup")
            .expect("page B")
            .path,
        PathBuf::from("A/page.md")
    );
}

#[test]
fn sqlite_committed_repair_recovers_hydration_upsert_and_cleanup_before_effect_boundaries() {
    let hydration_fixture = Fixture::new("sqlite-committed-hydration-boundary");
    let hydration_state = hydration_fixture.sandbox.join("state");
    let hydration_id = DiscoveryTransactionId::new("sqlite-committed-hydration-boundary");
    let hydration_request = HydrationRequest::new(
        hydration_fixture.mount.mount_id.clone(),
        RemoteId::new("page"),
        hydration_fixture.root.join("Roadmap/page.md"),
        HydrationState::Hydrated,
        HydrationReason::Policy,
    );
    {
        let mut store = SqliteStateStore::open(hydration_state.clone()).expect("open sqlite");
        store
            .save_mount(hydration_fixture.mount.clone())
            .expect("save mount");
        let mut plan = discovery_plan_sqlite(
            &store,
            &hydration_fixture.mount,
            vec![entry("page", EntityKind::Page, "Roadmap/page.md")],
        );
        plan.post_commit
            .push(DiscoveryPostCommitAction::QueueHydration(
                hydration_request.clone(),
            ));
        let reserved = prepare_plain_files_discovery_transaction(
            &mut store,
            plan,
            hydration_id.clone(),
            "t0",
            vec![DiscoveryCreateMaterialization::Page {
                remote_id: RemoteId::new("page"),
                document: "stub\n".to_string(),
            }],
        )
        .expect("prepare hydration transaction");
        let execution: DiscoveryExecutionPlan =
            serde_json::from_value(reserved.plan).expect("decode hydration execution");
        assert_eq!(execution.hydration_jobs.len(), 1);
        for sequence in 1..=20 {
            if step_plain_files_discovery_transaction(
                &mut store,
                &hydration_id,
                &format!("t{sequence}"),
            )
            .expect("advance hydration transaction")
                == DiscoveryExecutionStep::Committed
            {
                break;
            }
        }
        assert_eq!(
            step_plain_files_discovery_transaction(&mut store, &hydration_id, "t21")
                .expect("record projection barrier"),
            DiscoveryExecutionStep::ProjectionValidated
        );
        let record = store
            .get_discovery_transaction(&hydration_id)
            .expect("transaction lookup")
            .expect("transaction");
        let effects: DiscoveryExecutionEffects =
            serde_json::from_value(record.effects).expect("decode effects");
        assert!(effects.projection_validated);
        assert!(effects.hydration_jobs.is_empty());
        assert!(
            store
                .list_hydration_jobs()
                .expect("hydration jobs")
                .is_empty()
        );
    }
    {
        let mut store = SqliteStateStore::open(hydration_state.clone()).expect("reopen at barrier");
        let record = store
            .get_discovery_transaction(&hydration_id)
            .expect("transaction lookup")
            .expect("transaction");
        let effects: DiscoveryExecutionEffects =
            serde_json::from_value(record.effects).expect("decode reopened effects");
        assert!(effects.projection_validated);
        fs::write(hydration_fixture.root.join("Roadmap/page.md"), "hydrated\n")
            .expect("simulate hydrated bytes");
        store
            .upsert_hydration_job(HydrationJobRecord::from(hydration_request.clone()))
            .expect("simulate hydration upsert before effect crash");
        let record = store
            .get_discovery_transaction(&hydration_id)
            .expect("transaction lookup")
            .expect("transaction");
        let effects: DiscoveryExecutionEffects =
            serde_json::from_value(record.effects).expect("decode effects after job upsert");
        assert!(effects.projection_validated);
        assert!(effects.hydration_jobs.is_empty());
    }
    {
        let mut store = SqliteStateStore::open(hydration_state).expect("reopen after job upsert");
        assert_eq!(
            repair_plain_files_discovery_transaction(&mut store, &hydration_id, "t22")
                .expect("repair hydration boundary"),
            DiscoveryExecutionTerminal::Finalized
        );
        assert_eq!(
            store.list_hydration_jobs().expect("hydration jobs").len(),
            1
        );
        assert_eq!(
            fs::read_to_string(hydration_fixture.root.join("Roadmap/page.md"))
                .expect("read hydrated bytes"),
            "hydrated\n"
        );
    }

    let cleanup_fixture = Fixture::new("sqlite-committed-cleanup-boundary");
    let cleanup_state = cleanup_fixture.sandbox.join("state");
    let cleanup_id = DiscoveryTransactionId::new("sqlite-committed-cleanup-boundary");
    let recovery_root;
    {
        let mut store = SqliteStateStore::open(cleanup_state.clone()).expect("open sqlite");
        store
            .save_mount(cleanup_fixture.mount.clone())
            .expect("save mount");
        store
            .save_entity(entity_record("deleted", "Deleted/page.md"))
            .expect("save deleted entity");
        fs::create_dir_all(cleanup_fixture.root.join("Deleted")).expect("create deleted page");
        fs::write(cleanup_fixture.root.join("Deleted/page.md"), "deleted\n")
            .expect("write deleted page");
        let plan = discovery_plan_changes_sqlite(
            &store,
            &cleanup_fixture.mount,
            vec![BatchObservationChange::Tombstone {
                remote_id: RemoteId::new("deleted"),
            }],
        );
        let reserved = prepare_plain_files_discovery_transaction(
            &mut store,
            plan,
            cleanup_id.clone(),
            "t0",
            vec![],
        )
        .expect("prepare cleanup transaction");
        let execution: DiscoveryExecutionPlan =
            serde_json::from_value(reserved.plan).expect("decode cleanup execution");
        recovery_root = execution.recovery_root;
        for sequence in 1..=20 {
            if step_plain_files_discovery_transaction(
                &mut store,
                &cleanup_id,
                &format!("t{sequence}"),
            )
            .expect("advance cleanup transaction")
                == DiscoveryExecutionStep::Committed
            {
                break;
            }
        }
        assert_eq!(
            step_plain_files_discovery_transaction(&mut store, &cleanup_id, "t21")
                .expect("record cleanup projection barrier"),
            DiscoveryExecutionStep::ProjectionValidated
        );
        assert!(recovery_root.exists());
        fs::remove_dir_all(&recovery_root).expect("simulate cleanup before effect crash");
        let record = store
            .get_discovery_transaction(&cleanup_id)
            .expect("transaction lookup")
            .expect("transaction");
        assert_eq!(record.status, DiscoveryTransactionStatus::Committed);
        let effects: DiscoveryExecutionEffects =
            serde_json::from_value(record.effects).expect("decode cleanup effects");
        assert!(effects.projection_validated);
        assert!(!effects.cleanup_complete);
    }
    {
        let mut store = SqliteStateStore::open(cleanup_state).expect("reopen sqlite");
        assert_eq!(
            repair_plain_files_discovery_transaction(&mut store, &cleanup_id, "t30")
                .expect("repair cleanup boundary"),
            DiscoveryExecutionTerminal::Finalized
        );
        assert!(!recovery_root.exists());
        assert!(
            store
                .get_entity(&cleanup_fixture.mount.mount_id, &RemoteId::new("deleted"))
                .expect("deleted entity lookup")
                .is_none()
        );
    }
}

fn step(
    store: &mut InMemoryStateStore,
    transaction_id: &DiscoveryTransactionId,
    sequence: u32,
) -> DiscoveryExecutionStep {
    step_plain_files_discovery_transaction(store, transaction_id, &format!("t{sequence}"))
        .expect("execution step")
}

fn effect_state(
    store: &InMemoryStateStore,
    transaction_id: &DiscoveryTransactionId,
) -> DiscoveryOperationEffectState {
    let record = store
        .get_discovery_transaction(transaction_id)
        .expect("transaction lookup")
        .expect("transaction");
    let effects: DiscoveryExecutionEffects =
        serde_json::from_value(record.effects).expect("effects");
    effects.operations[0].state
}

fn run_steps_to_finalized(store: &mut InMemoryStateStore, transaction_id: &DiscoveryTransactionId) {
    let mut last = None;
    for sequence in 1..=40 {
        let outcome = step(store, transaction_id, sequence);
        if outcome == DiscoveryExecutionStep::Finalized {
            return;
        }
        last = Some(outcome);
    }
    panic!("transaction did not finalize within bounded steps; last={last:?}");
}

fn run_steps_to_committed(store: &mut InMemoryStateStore, transaction_id: &DiscoveryTransactionId) {
    let mut last = None;
    for sequence in 1..=30 {
        let outcome = step(store, transaction_id, sequence);
        if outcome == DiscoveryExecutionStep::Committed {
            return;
        }
        last = Some(outcome);
    }
    panic!("transaction did not commit within bounded steps; last={last:?}");
}

fn run_steps_to_projected(store: &mut InMemoryStateStore, transaction_id: &DiscoveryTransactionId) {
    let mut last = None;
    for sequence in 1..=30 {
        let outcome = step(store, transaction_id, sequence);
        if outcome == DiscoveryExecutionStep::Projected {
            return;
        }
        last = Some(outcome);
    }
    panic!("transaction did not project within bounded steps; last={last:?}");
}

fn run_steps_to_finalized_from(
    store: &mut InMemoryStateStore,
    transaction_id: &DiscoveryTransactionId,
    first_sequence: u32,
) {
    for sequence in first_sequence..=first_sequence + 40 {
        if step(store, transaction_id, sequence) == DiscoveryExecutionStep::Finalized {
            return;
        }
    }
    panic!("transaction did not finalize within bounded steps");
}

fn discovery_plan(
    store: &InMemoryStateStore,
    mount: &MountConfig,
    entries: Vec<TreeEntry>,
) -> localityd::discovery::DiscoveryPlan {
    let assessments = entries
        .iter()
        .map(|entry| (entry.remote_id.clone(), ProjectionAssessment::Safe))
        .collect::<BTreeMap<_, _>>();
    plan_batch_discovery(
        store,
        mount,
        BatchObserveResult::incremental(
            entries
                .into_iter()
                .map(BatchObservationChange::Upsert)
                .collect(),
            checkpoint(1),
        ),
        NOW,
        None,
        &assessments,
        &DiscoverySafetySnapshot::default(),
    )
    .expect("plan discovery")
}

fn discovery_plan_changes(
    store: &InMemoryStateStore,
    mount: &MountConfig,
    changes: Vec<BatchObservationChange>,
) -> localityd::discovery::DiscoveryPlan {
    let assessments = changes
        .iter()
        .map(|change| match change {
            BatchObservationChange::Upsert(entry) => entry.remote_id.clone(),
            BatchObservationChange::Tombstone { remote_id } => remote_id.clone(),
        })
        .map(|remote_id| (remote_id, ProjectionAssessment::Safe))
        .collect::<BTreeMap<_, _>>();
    plan_batch_discovery(
        store,
        mount,
        BatchObserveResult::incremental(changes, checkpoint(1)),
        NOW,
        None,
        &assessments,
        &DiscoverySafetySnapshot::default(),
    )
    .expect("plan discovery changes")
}

fn discovery_plan_sqlite(
    store: &SqliteStateStore,
    mount: &MountConfig,
    entries: Vec<TreeEntry>,
) -> localityd::discovery::DiscoveryPlan {
    let assessments = entries
        .iter()
        .map(|entry| (entry.remote_id.clone(), ProjectionAssessment::Safe))
        .collect::<BTreeMap<_, _>>();
    plan_batch_discovery(
        store,
        mount,
        BatchObserveResult::incremental(
            entries
                .into_iter()
                .map(BatchObservationChange::Upsert)
                .collect(),
            checkpoint(1),
        ),
        NOW,
        None,
        &assessments,
        &DiscoverySafetySnapshot::default(),
    )
    .expect("plan sqlite discovery")
}

fn discovery_plan_changes_sqlite(
    store: &SqliteStateStore,
    mount: &MountConfig,
    changes: Vec<BatchObservationChange>,
) -> localityd::discovery::DiscoveryPlan {
    let assessments = changes
        .iter()
        .map(|change| match change {
            BatchObservationChange::Upsert(entry) => entry.remote_id.clone(),
            BatchObservationChange::Tombstone { remote_id } => remote_id.clone(),
        })
        .map(|remote_id| (remote_id, ProjectionAssessment::Safe))
        .collect::<BTreeMap<_, _>>();
    plan_batch_discovery(
        store,
        mount,
        BatchObserveResult::incremental(changes, checkpoint(1)),
        NOW,
        None,
        &assessments,
        &DiscoverySafetySnapshot::default(),
    )
    .expect("plan sqlite discovery changes")
}

fn entry(remote_id: &str, kind: EntityKind, path: &str) -> TreeEntry {
    TreeEntry {
        mount_id: MountId::new("linear-main"),
        remote_id: RemoteId::new(remote_id),
        kind,
        title: remote_id.to_string(),
        path: PathBuf::from(path),
        hydration: HydrationState::Stub,
        content_hash: Some(format!("hash:{remote_id}")),
        remote_edited_at: Some("remote-v1".to_string()),
        stub_frontmatter: None,
    }
}

fn entity_record(remote_id: &str, path: &str) -> EntityRecord {
    EntityRecord::new(
        MountId::new("linear-main"),
        RemoteId::new(remote_id),
        EntityKind::Page,
        remote_id,
        path,
    )
    .with_hydration(HydrationState::Stub)
    .with_synced_tree_remote_version("remote-v0")
}

fn checkpoint(state_version: i64) -> ConnectorCheckpoint {
    ConnectorCheckpoint {
        state_version,
        min_reader_version: 1,
        state_json: format!(r#"{{"cursor":{state_version}}}"#),
    }
}

fn reserve_opaque_transaction(
    store: &mut InMemoryStateStore,
    mount: &MountConfig,
    transaction_id: DiscoveryTransactionId,
    state_version: i64,
) {
    let reservation = store
        .capture_discovery_reservation(&mount.mount_id)
        .expect("capture opaque reservation");
    let commit = DiscoveryCommit {
        mount_id: mount.mount_id.clone(),
        entity_upserts: vec![],
        entity_deletes: vec![],
        observation_upserts: vec![],
        freshness_upserts: vec![],
        auto_save_upserts: vec![],
        metadata_discovery_deletes: vec![],
        virtual_mutation_deletes: vec![],
        checkpoint: ConnectorStateRecord {
            connector: mount.connector.clone(),
            scope_kind: "mount".to_string(),
            scope_id: mount.mount_id.0.clone(),
            state_version: 1,
            min_reader_version: 1,
            state_json: "{}".to_string(),
            updated_at: "t0".to_string(),
        },
    };
    let plan = if mount.projection == ProjectionMode::PlainFiles {
        serde_json::json!({
            "state_version": state_version,
            "min_reader_version": state_version,
            "future_layout": {"cannot": "deserialize as the current plan"},
        })
    } else {
        serde_json::json!({"opaque_provider_plan": true})
    };
    let prepared = PreparedDiscoveryTransaction::new(
        TransactionalDiscoveryCommit::new(transaction_id, commit),
        mount.projection.clone(),
        plan,
        reservation,
        "t0",
    )
    .with_effects(
        serde_json::to_value(DiscoveryExecutionEffects::default())
            .expect("serialize opaque effects"),
    );
    store
        .reserve_discovery_transaction(prepared)
        .expect("reserve opaque transaction");
}

struct Fixture {
    sandbox: PathBuf,
    root: PathBuf,
    mount: MountConfig,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let sandbox = temp_root(label);
        let root = sandbox.join("mount");
        fs::create_dir_all(&root).expect("create mount root");
        Self {
            mount: MountConfig::new(MountId::new("linear-main"), "linear", root.clone()),
            sandbox,
            root,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.sandbox);
    }
}

fn filesystem_snapshot(root: &std::path::Path) -> Vec<(PathBuf, bool, Vec<u8>)> {
    fn collect(
        root: &std::path::Path,
        directory: &std::path::Path,
        entries: &mut Vec<(PathBuf, bool, Vec<u8>)>,
    ) {
        let mut children = fs::read_dir(directory)
            .expect("read snapshot directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect snapshot directory");
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let path = child.path();
            let relative = path
                .strip_prefix(root)
                .expect("snapshot path below root")
                .to_path_buf();
            let metadata = fs::symlink_metadata(&path).expect("snapshot metadata");
            if metadata.is_dir() {
                entries.push((relative, true, Vec::new()));
                collect(root, &path, entries);
            } else {
                entries.push((
                    relative,
                    false,
                    fs::read(&path).expect("snapshot file bytes"),
                ));
            }
        }
    }

    let mut entries = Vec::new();
    collect(root, root, &mut entries);
    entries
}

fn temp_root(_label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let process_id = std::process::id();

    #[cfg(windows)]
    {
        let base = std::env::var_os("RUNNER_TEMP")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("SystemDrive")
                    .map(PathBuf::from)
                    .map(|drive| drive.join("loc-test"))
            })
            .unwrap_or_else(std::env::temp_dir);
        return base.join(format!("lde-{process_id}-{sequence}"));
    }

    #[cfg(not(windows))]
    {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "loc-discovery-execution-{}-{process_id}-{timestamp}-{sequence}",
            _label
        ))
    }
}
