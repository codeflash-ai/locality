use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_connector::{BatchObservationChange, BatchObserveResult, ConnectorCheckpoint};
use locality_core::hydration::{HydrationReason, HydrationRequest};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId, TreeEntry};
use locality_store::{
    ConnectorStateRecord, DiscoveryCommit, DiscoveryRepository, DiscoveryTransactionId,
    DiscoveryTransactionStatus, EntityRecord, EntityRepository, HydrationJobRecord,
    HydrationJobRepository, InMemoryStateStore, MountConfig, MountRepository,
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

#[cfg(unix)]
#[test]
fn repair_rejects_an_ancestor_symlink_inserted_after_reservation_without_mutation() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new("repair-symlink-ancestor");
    let mut store = InMemoryStateStore::new();
    store.save_mount(fixture.mount.clone()).expect("save mount");
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
    symlink(&outside, fixture.root.join("Inserted")).expect("insert ancestor symlink");

    let error = repair_plain_files_discovery_transaction(&mut store, &transaction_id, "t1")
        .expect_err("repair must reject inserted ancestor symlink");
    assert!(error.to_string().contains("is a symlink"));
    assert!(!outside.join("Child").exists());
    assert_eq!(
        store
            .get_discovery_transaction(&transaction_id)
            .expect("transaction lookup")
            .expect("transaction")
            .status,
        DiscoveryTransactionStatus::Reserved
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
        DiscoveryExecutionStep::RecoveryPayloadsRemoved
    );
    assert!(!execution.recovery_root.exists());
    assert_eq!(
        step(&mut store, &transaction_id, 10),
        DiscoveryExecutionStep::CleanupComplete
    );
    assert_eq!(
        step(&mut store, &transaction_id, 11),
        DiscoveryExecutionStep::CompletionRecorded
    );
    assert_eq!(
        step(&mut store, &transaction_id, 12),
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
    prepare_plain_files_discovery_transaction(
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

    run_steps_to_finalized(&mut store, &transaction_id);

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
    prepare_plain_files_discovery_transaction(
        &mut store,
        plan,
        transaction_id.clone(),
        "t0",
        vec![],
    )
    .expect("prepare delete");

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
        repair_plain_files_discovery_transaction(&mut store, &transaction_id, "t4")
            .expect("repair committed transaction"),
        DiscoveryExecutionTerminal::Finalized
    );
    let jobs = store.list_hydration_jobs().expect("hydration jobs");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].remote_id, RemoteId::new("page"));
    assert_eq!(
        repair_plain_files_discovery_transaction(&mut store, &transaction_id, "t5")
            .expect("repeat finalized repair"),
        DiscoveryExecutionTerminal::Finalized
    );
    assert_eq!(
        store.list_hydration_jobs().expect("hydration jobs").len(),
        1
    );
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

    let results = repair_active_plain_files_discovery_transactions(&mut store, "t1")
        .expect("repair active plain transactions");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].transaction_id, plain_id);
    assert_eq!(results[0].outcome, DiscoveryExecutionTerminal::Aborted);
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
    assert!(
        provider_error
            .to_string()
            .contains("not a plain-files projection")
    );
    assert_eq!(
        store
            .get_discovery_transaction(&provider_id)
            .expect("provider transaction lookup")
            .expect("provider transaction")
            .status,
        DiscoveryTransactionStatus::Reserved
    );
    store
        .mark_discovery_transaction_applying(&provider_id, "t3")
        .expect("mark opaque provider applying");
    let provider_step_error =
        step_plain_files_discovery_transaction(&mut store, &provider_id, "t4")
            .expect_err("plain-files step must reject provider transaction");
    assert!(
        provider_step_error
            .to_string()
            .contains("not a plain-files projection")
    );
    assert_eq!(
        store
            .get_discovery_transaction(&provider_id)
            .expect("provider transaction lookup")
            .expect("provider transaction")
            .status,
        DiscoveryTransactionStatus::Applying
    );

    let newer_fixture = Fixture::new("repair-newer-state");
    let mut newer_store = InMemoryStateStore::new();
    newer_store
        .save_mount(newer_fixture.mount.clone())
        .expect("save newer mount");
    let newer_id = DiscoveryTransactionId::new("newer-reserved");
    reserve_opaque_transaction(&mut newer_store, &newer_fixture.mount, newer_id.clone(), 2);

    let error = repair_plain_files_discovery_transaction(&mut newer_store, &newer_id, "t1")
        .expect_err("newer execution state must fail");
    assert!(error.to_string().contains("update required"));
    assert_eq!(
        newer_store
            .get_discovery_transaction(&newer_id)
            .expect("newer transaction lookup")
            .expect("newer transaction")
            .status,
        DiscoveryTransactionStatus::Reserved
    );
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
    assert!(
        validate_plain_files_discovery_transaction_record(&newer_plan)
            .expect_err("newer plan must fail")
            .to_string()
            .contains("update required")
    );

    let mut newer_effects = record.clone();
    newer_effects.effects["min_reader_version"] = serde_json::json!(2);
    newer_effects.effects["state_version"] = serde_json::json!(2);
    assert!(
        validate_plain_files_discovery_transaction_record(&newer_effects)
            .expect_err("newer effects must fail")
            .to_string()
            .contains("update required")
    );

    let mut missing_operation = record.clone();
    missing_operation.plan["components"][0]["operations"] = serde_json::json!([]);
    assert!(
        validate_plain_files_discovery_transaction_record(&missing_operation)
            .expect_err("missing operation must fail")
            .to_string()
            .contains("exactly one operation")
    );

    let mut impossible_effect = record.clone();
    let operation = impossible_effect.plan["components"][0]["operations"][0].clone();
    impossible_effect.effects["operations"] = serde_json::json!([{
        "operation_id": operation["operation_id"].clone(),
        "state": "cleaned",
        "fingerprint": operation["expected_fingerprint"].clone(),
    }]);
    assert!(
        validate_plain_files_discovery_transaction_record(&impossible_effect)
            .expect_err("unreachable operation state must fail")
            .to_string()
            .contains("unknown variant `cleaned`")
    );

    let mut redirected = record;
    redirected.plan["components"][0]["operations"][0]["destination"] =
        serde_json::json!("Elsewhere");
    assert!(
        validate_plain_files_discovery_transaction_record(&redirected)
            .expect_err("redirected operation must fail")
            .to_string()
            .contains("does not match its action")
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

fn temp_root(label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "loc-discovery-execution-{label}-{}-{timestamp}-{sequence}",
        std::process::id()
    ))
}
