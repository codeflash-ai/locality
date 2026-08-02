use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use locality_core::model::RemoteId;
use locality_core::portable::{
    ExportAttemptId, LogicalPath, ProjectionFileKind, ProjectionId, SessionId, SourceAction,
    SourceConnectionId,
};
use locality_protocol::workspace_api_v2::{
    WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON, WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON,
    WorkspaceExportOfferV2, WorkspaceProfileSessionV2,
};
use locality_protocol::workspace_export_v2::{
    WorkspaceAuthorizedExportEntryV2, WorkspaceExportCompletionReceiptV2,
    WorkspaceExportControlMetadataV2, WorkspaceExportTerminalControlV2,
    WorkspaceNamespacedExportRecordV2, WorkspaceNamespacedInventoryV2,
    WorkspaceScopeSourceAuthorityV2,
};
use locality_protocol::{
    DeliveredBodyDigestV2, ExportCompletionReceipt, ExportV2FilePaxMetadata,
    PAX_WINNING_SCOPE_ORDINAL, ScopeAuthorizedWritableExportMetadata, WritableMetadataEntry,
    canonical_writable_metadata_sha256,
};
use localityd::remote_truth::{ReplicaArchive, ReplicaArchiveEncoding};
#[cfg(not(all(
    unix,
    any(target_vendor = "apple", target_os = "linux", target_os = "android")
)))]
use localityd::replica_materializer::ReplicaMaterializationError;
use localityd::workspace_archive::{
    WorkspaceArchiveLimits, WorkspaceArchiveSink, validate_workspace_tar,
};
use localityd::workspace_materializer::{
    PublishedWorkspace, StagedWorkspaceMaterialization, WorkspaceMaterializationError,
    WorkspaceMaterializationLimits, WorkspaceOwnershipCapability, WorkspacePublicationCheckpoint,
    WorkspacePublicationHooks, load_workspace_publication_receipt,
    materialize_workspace_archive_durable as materialize_workspace_archive_durable_secure,
    publish_staged_workspace_with_hooks as publish_staged_workspace_with_hooks_secure,
    recover_and_verify_workspace_publication_state,
    recover_workspace_publication as recover_workspace_publication_secure, stage_workspace_archive,
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tar::{Builder, EntryType, Header};

static TEST_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = TEST_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "locality-workspace-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create test directory");
        Self(path)
    }

    fn root(&self) -> PathBuf {
        self.0.join("Locality")
    }

    fn assert_only(&self, expected: &[&str]) {
        let mut names = fs::read_dir(&self.0)
            .expect("read test directory")
            .map(|entry| {
                entry
                    .expect("read entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, expected);
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        make_removable(&self.0);
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Deserialize)]
struct Fixture {
    directories: Vec<FixtureDirectory>,
    files: Vec<FixtureFile>,
}

#[derive(Deserialize)]
struct FixtureDirectory {
    path: String,
    scope_ordinal: Option<u32>,
}

#[derive(Deserialize)]
struct FixtureFile {
    path: String,
    logical_path: String,
    scope_ordinal: u32,
    projection_id: String,
    source_connection_id: String,
    file_kind: ProjectionFileKind,
    effective_actions: BTreeSet<SourceAction>,
    content: String,
}

#[derive(Default)]
struct MemorySink {
    directories: BTreeSet<String>,
    files: BTreeMap<String, Vec<u8>>,
}

impl WorkspaceArchiveSink for MemorySink {
    fn create_directory(&mut self, member_path: &str) -> io::Result<()> {
        if !self.directories.insert(member_path.to_string()) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "duplicate directory",
            ));
        }
        Ok(())
    }

    fn write_file(
        &mut self,
        member_path: &str,
        body: &mut dyn Read,
        expected_size: u64,
    ) -> io::Result<()> {
        let mut bytes = Vec::new();
        body.read_to_end(&mut bytes)?;
        if bytes.len() as u64 != expected_size {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short body"));
        }
        if self.files.insert(member_path.to_string(), bytes).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "duplicate file",
            ));
        }
        Ok(())
    }
}

struct Contract {
    session: WorkspaceProfileSessionV2,
    offer: WorkspaceExportOfferV2,
    inventory: WorkspaceNamespacedInventoryV2,
    control: WorkspaceExportTerminalControlV2,
}

fn fixture() -> Fixture {
    serde_json::from_slice(include_bytes!("../fixtures/workspace-materializer-v2.json"))
        .expect("workspace materializer fixture")
}

fn contract(fixture: &Fixture) -> Contract {
    let scope_sources = vec![
        WorkspaceScopeSourceAuthorityV2::new(0, SourceConnectionId::new("source-notion")),
        WorkspaceScopeSourceAuthorityV2::new(1, SourceConnectionId::new("source-drive")),
        WorkspaceScopeSourceAuthorityV2::new(2, SourceConnectionId::new("source-notion")),
        WorkspaceScopeSourceAuthorityV2::new(3, SourceConnectionId::new("source-drive")),
    ];
    contract_with_scope_sources(fixture, scope_sources)
}

fn contract_with_scope_sources(
    fixture: &Fixture,
    scope_sources: Vec<WorkspaceScopeSourceAuthorityV2>,
) -> Contract {
    let session: WorkspaceProfileSessionV2 =
        serde_json::from_slice(WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON).expect("session fixture");
    let mount_by_scope = session
        .session_layout()
        .entries()
        .iter()
        .map(|entry| (entry.scope_ordinal(), entry.mount_id().clone()))
        .collect::<BTreeMap<_, _>>();
    let mut entries = fixture
        .directories
        .iter()
        .filter_map(|directory| {
            let ordinal = directory.scope_ordinal?;
            let logical_path = directory.path.split_once('/')?.1.to_string();
            Some(WorkspaceAuthorizedExportEntryV2::Directory {
                winning_scope_ordinal: ordinal,
                mount_id: mount_by_scope[&ordinal].clone(),
                logical_path,
            })
        })
        .collect::<Vec<_>>();
    entries.extend(
        fixture
            .files
            .iter()
            .map(|file| WorkspaceAuthorizedExportEntryV2::File {
                winning_scope_ordinal: file.scope_ordinal,
                mount_id: mount_by_scope[&file.scope_ordinal].clone(),
                logical_path: file.logical_path.clone(),
                projection_id: ProjectionId::new(&file.projection_id),
                source_connection_id: SourceConnectionId::new(&file.source_connection_id),
                file_kind: file.file_kind.clone(),
                effective_actions: file.effective_actions.clone(),
                content_sha256: sha256_label(file.content.as_bytes()),
                byte_length: file.content.len() as u64,
            }),
    );

    let mut offer_json: Value =
        serde_json::from_slice(WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON).expect("offer JSON");
    let directory_count = fixture.directories.len() as u64;
    let file_count = fixture.files.len() as u64;
    let content_bytes = fixture
        .files
        .iter()
        .map(|file| file.content.len() as u64)
        .sum::<u64>();
    offer_json["offer"]["file_count"] = Value::from(file_count);
    offer_json["offer"]["directory_count"] = Value::from(directory_count);
    offer_json["offer"]["archive_entry_count"] = Value::from(directory_count + file_count + 1);
    offer_json["offer"]["selected_content_bytes"] = Value::from(content_bytes);
    offer_json["offer"]["inventory_sha256"] = Value::String(format!("sha256:{}", "0".repeat(64)));
    let placeholder_offer: WorkspaceExportOfferV2 =
        serde_json::from_value(offer_json.clone()).expect("placeholder offer");
    let inventory = WorkspaceNamespacedInventoryV2::plan(
        session.session_layout(),
        &placeholder_offer,
        &scope_sources,
        &entries,
    )
    .expect("plan fixture inventory");
    offer_json["offer"]["inventory_sha256"] =
        Value::String(inventory.inventory_sha256().to_string());

    let writable_entries = fixture
        .files
        .iter()
        .find(|file| file.projection_id == "projection-roadmap")
        .map(|file| WritableMetadataEntry {
            projection_id: ProjectionId::new(&file.projection_id),
            logical_path: LogicalPath::new(&file.logical_path).expect("logical path"),
            source_remote_ids: vec![RemoteId::new("page-roadmap")],
            delivered_content_sha256: sha256_label(file.content.as_bytes()),
            provider_precondition: "opaque-v4".to_string(),
            effective_actions: file.effective_actions.clone(),
            baseline_required: true,
        })
        .into_iter()
        .collect();
    let writable_metadata = ScopeAuthorizedWritableExportMetadata {
        versions: placeholder_offer.offer().versions,
        session_id: SessionId::new("session-scope-7"),
        export_attempt_id: ExportAttemptId::new("export-attempt-9").expect("attempt ID"),
        source_generations: placeholder_offer.offer().source_generations.clone(),
        writable_entries,
    };
    offer_json["offer"]["writable_metadata_sha256"] = Value::String(
        canonical_writable_metadata_sha256(&writable_metadata).expect("writable digest"),
    );
    let offer: WorkspaceExportOfferV2 =
        serde_json::from_value(offer_json).expect("final workspace offer");
    let inventory = WorkspaceNamespacedInventoryV2::plan(
        session.session_layout(),
        &offer,
        &scope_sources,
        &entries,
    )
    .expect("final inventory");
    inventory
        .validate_against_offer(&offer)
        .expect("inventory matches offer");
    let metadata = WorkspaceExportControlMetadataV2::new(&session, &offer, &inventory)
        .expect("control metadata");
    let mut delivered = DeliveredBodyDigestV2::new(fixture.files.len() as u64);
    for file in &fixture.files {
        delivered
            .update_file(
                &ProjectionId::new(&file.projection_id),
                file.content.as_bytes(),
            )
            .expect("delivered body");
    }
    let receipt = ExportCompletionReceipt {
        versions: offer.offer().versions,
        session_id: offer.offer().session_id.clone(),
        export_attempt_id: offer.offer().export_attempt_id.clone(),
        source_generations: offer.offer().source_generations.clone(),
        inventory_sha256: inventory.inventory_sha256().to_string(),
        writable_metadata_sha256: offer.offer().writable_metadata_sha256.clone(),
        delivered_control_entry_count: inventory.control_entry_count(),
        delivered_file_count: inventory.file_count(),
        delivered_directory_count: inventory.directory_count(),
        delivered_archive_entry_count: inventory.archive_entry_count(),
        delivered_content_bytes: inventory.selected_content_bytes(),
        delivered_body_sha256: delivered.finish().expect("body digest"),
        completed_at: "2026-07-23T19:00:04Z".to_string(),
    };
    let control = WorkspaceExportTerminalControlV2 {
        metadata: metadata.clone(),
        writable_metadata,
        completion_receipt: WorkspaceExportCompletionReceiptV2 { metadata, receipt },
    };
    control
        .validate_against(&session, &offer, &inventory)
        .expect("fixture control");
    Contract {
        session,
        offer,
        inventory,
        control,
    }
}

fn test_ownership() -> WorkspaceOwnershipCapability {
    WorkspaceOwnershipCapability::new([0x5a; 32])
}

fn materialize_workspace_archive_durable<Body: Read>(
    archive: ReplicaArchive<Body>,
    destination: &Path,
    limits: WorkspaceMaterializationLimits,
    session: &WorkspaceProfileSessionV2,
    offer: &WorkspaceExportOfferV2,
) -> Result<PublishedWorkspace, WorkspaceMaterializationError> {
    materialize_workspace_archive_durable_secure(
        archive,
        destination,
        limits,
        session,
        offer,
        &test_ownership(),
    )
}

fn publish_staged_workspace_with_hooks<H: WorkspacePublicationHooks>(
    staged: StagedWorkspaceMaterialization,
    destination: &Path,
    hooks: &mut H,
) -> Result<PublishedWorkspace, WorkspaceMaterializationError> {
    publish_staged_workspace_with_hooks_secure(staged, destination, &test_ownership(), hooks)
}

fn recover_workspace_publication(destination: &Path) -> Result<(), WorkspaceMaterializationError> {
    recover_workspace_publication_secure(destination, &test_ownership())
}

fn sha256_label(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn archive(fixture: &Fixture, control: &WorkspaceExportTerminalControlV2) -> Vec<u8> {
    let mut builder = Builder::new(Vec::new());
    for directory in &fixture.directories {
        if let Some(ordinal) = directory.scope_ordinal {
            builder
                .append_pax_extensions([(
                    PAX_WINNING_SCOPE_ORDINAL,
                    ordinal.to_string().as_bytes(),
                )])
                .expect("directory PAX");
        }
        append_member(&mut builder, &directory.path, EntryType::dir(), 0o555, b"");
    }
    for file in &fixture.files {
        let metadata = ExportV2FilePaxMetadata {
            source_connection_id: SourceConnectionId::new(&file.source_connection_id),
            projection_id: ProjectionId::new(&file.projection_id),
            winning_scope_ordinal: file.scope_ordinal,
            file_kind: file.file_kind.clone(),
            effective_actions: file.effective_actions.clone(),
            content_sha256: sha256_label(file.content.as_bytes()),
        };
        let pax = metadata.to_records().expect("file PAX");
        builder
            .append_pax_extensions(pax.iter().map(|(key, value)| (*key, value.as_bytes())))
            .expect("append file PAX");
        append_member(
            &mut builder,
            &file.path,
            EntryType::file(),
            0o444,
            file.content.as_bytes(),
        );
    }
    append_member(
        &mut builder,
        locality_protocol::RESERVED_EXPORT_METADATA_PATH,
        EntryType::file(),
        0o444,
        &serde_json::to_vec(control).expect("control JSON"),
    );
    builder.finish().expect("finish workspace tar");
    builder.into_inner().expect("workspace tar bytes")
}

fn append_member(
    builder: &mut Builder<Vec<u8>>,
    path: &str,
    entry_type: EntryType,
    mode: u32,
    body: &[u8],
) {
    let mut header = Header::new_gnu();
    header.set_entry_type(entry_type);
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(body.len() as u64);
    header.set_path(path).expect("member path");
    header.set_cksum();
    builder.append(&header, body).expect("append member");
}

#[test]
fn production_tar_adapter_executes_the_validated_namespaced_plan() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let tar = archive(&fixture, &contract.control);
    let mut sink = MemorySink::default();

    let validated = validate_workspace_tar(
        &mut Cursor::new(tar),
        &mut sink,
        WorkspaceArchiveLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("validate workspace archive");

    assert_eq!(validated.archive_entries, 7);
    assert_eq!(validated.directories, 4);
    assert_eq!(validated.files, 2);
    assert_eq!(validated.content_bytes, 17);
    assert_eq!(validated.plan.entries().len(), 6);
    assert_eq!(validated.inventory, contract.inventory);
    assert_eq!(
        validated
            .inventory
            .records()
            .iter()
            .filter(|record| !matches!(record, WorkspaceNamespacedExportRecordV2::Control { .. }))
            .map(WorkspaceNamespacedExportRecordV2::member_path)
            .collect::<Vec<_>>(),
        validated
            .plan
            .entries()
            .iter()
            .map(|entry| entry.member_path.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        validated.inventory.inventory_sha256(),
        validated.terminal_control.metadata.inventory_sha256()
    );
    assert_eq!(sink.directories.len(), 4);
    assert_eq!(sink.files["Sales/README.md"], b"Public\n");
    assert!(
        !sink
            .files
            .contains_key(locality_protocol::RESERVED_EXPORT_METADATA_PATH)
    );
}

#[test]
fn exported_inventory_retains_empty_sources_across_shared_multi_source_mounts() {
    let fixture = Fixture {
        directories: vec![
            FixtureDirectory {
                path: "Engineering".to_string(),
                scope_ordinal: None,
            },
            FixtureDirectory {
                path: "Sales".to_string(),
                scope_ordinal: None,
            },
        ],
        files: Vec::new(),
    };
    let scope_sources = vec![
        WorkspaceScopeSourceAuthorityV2::new(0, SourceConnectionId::new("source-notion")),
        WorkspaceScopeSourceAuthorityV2::new(1, SourceConnectionId::new("source-drive")),
        WorkspaceScopeSourceAuthorityV2::new(2, SourceConnectionId::new("source-drive")),
        WorkspaceScopeSourceAuthorityV2::new(3, SourceConnectionId::new("source-notion")),
    ];
    let contract = contract_with_scope_sources(&fixture, scope_sources.clone());
    let mut sink = MemorySink::default();

    let validated = validate_workspace_tar(
        &mut Cursor::new(archive(&fixture, &contract.control)),
        &mut sink,
        WorkspaceArchiveLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("validate empty shared-mount workspace archive");

    assert_eq!(validated.inventory, contract.inventory);
    assert_eq!(validated.inventory.scope_sources(), scope_sources);
    assert_eq!(validated.inventory.file_count(), 0);
    assert_eq!(validated.inventory.directory_count(), 2);
    assert_eq!(validated.inventory.records().len(), 3);
    assert_eq!(validated.plan.entries().len(), 2);

    let mount_by_scope = contract
        .session
        .session_layout()
        .entries()
        .iter()
        .map(|entry| (entry.scope_ordinal(), entry.mount_id().as_str()))
        .collect::<BTreeMap<_, _>>();
    let mut sources_by_mount = BTreeMap::<_, BTreeSet<_>>::new();
    for authority in validated.inventory.scope_sources() {
        sources_by_mount
            .entry(mount_by_scope[&authority.scope_ordinal()])
            .or_default()
            .insert(authority.source_connection_id().as_str());
    }
    assert_eq!(sources_by_mount.len(), 2);
    assert!(sources_by_mount.values().all(|sources| sources.len() == 2));
}

#[test]
fn exported_inventory_is_portable_and_fails_closed_on_context_or_control_tampering() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let mut sink = MemorySink::default();
    let validated = validate_workspace_tar(
        &mut Cursor::new(archive(&fixture, &contract.control)),
        &mut sink,
        WorkspaceArchiveLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("validate portable inventory");
    let encoded = serde_json::to_string(&validated.inventory).expect("serialize inventory");
    assert!(!encoded.contains("provider_precondition"));
    assert!(!encoded.contains("source_remote_ids"));
    assert!(!encoded.contains("opaque-v4"));
    assert!(!encoded.contains("page-roadmap"));
    assert!(
        validated
            .inventory
            .records()
            .iter()
            .all(|record| !record.member_path().starts_with('/'))
    );

    let mut other_session_json: Value =
        serde_json::from_slice(WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON).expect("session JSON");
    other_session_json["session_id"] = Value::String("session-substituted".to_string());
    let other_session: WorkspaceProfileSessionV2 =
        serde_json::from_value(other_session_json).expect("substituted session");
    let error = validate_workspace_tar(
        &mut Cursor::new(archive(&fixture, &contract.control)),
        &mut MemorySink::default(),
        WorkspaceArchiveLimits::default(),
        &other_session,
        &contract.offer,
    )
    .expect_err("inventory must remain bound to the verified session");
    assert!(
        error.to_string().contains("does not match session"),
        "{error}"
    );

    let mut control_json = serde_json::to_value(&contract.control).expect("control JSON");
    let substituted_digest = Value::String(format!("sha256:{}", "a".repeat(64)));
    control_json["metadata"]["inventory_sha256"] = substituted_digest.clone();
    control_json["completion_receipt"]["metadata"]["inventory_sha256"] = substituted_digest.clone();
    control_json["completion_receipt"]["receipt"]["inventory_sha256"] = substituted_digest;
    let tampered_control: WorkspaceExportTerminalControlV2 =
        serde_json::from_value(control_json).expect("shape-valid tampered control");
    let error = validate_workspace_tar(
        &mut Cursor::new(archive(&fixture, &tampered_control)),
        &mut MemorySink::default(),
        WorkspaceArchiveLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect_err("inventory must remain bound to terminal control");
    assert!(
        error
            .to_string()
            .contains("control metadata does not match"),
        "{error}"
    );
}

#[test]
fn archive_limits_bound_the_inventory_that_can_be_returned() {
    let fixture = fixture();
    let contract = contract(&fixture);
    for (label, limits, expected) in [
        (
            "entries",
            WorkspaceArchiveLimits {
                max_entries: 6,
                ..WorkspaceArchiveLimits::default()
            },
            "entry limit",
        ),
        (
            "file bytes",
            WorkspaceArchiveLimits {
                max_file_bytes: 9,
                ..WorkspaceArchiveLimits::default()
            },
            "file `Sales/Projects/Roadmap/page.md`",
        ),
        (
            "content bytes",
            WorkspaceArchiveLimits {
                max_content_bytes: 16,
                ..WorkspaceArchiveLimits::default()
            },
            "content is 17 bytes",
        ),
    ] {
        let error = validate_workspace_tar(
            &mut Cursor::new(archive(&fixture, &contract.control)),
            &mut MemorySink::default(),
            limits,
            &contract.session,
            &contract.offer,
        )
        .expect_err("limit violation must not return an inventory");
        assert!(
            error.to_string().contains(expected),
            "case {label}: {error}"
        );
    }
}

#[test]
fn archive_adapter_rejects_wrong_content_before_returning_a_plan() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let mut tar = archive(&fixture, &contract.control);
    let needle = b"Public\n";
    let offset = tar
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("fixture body in tar");
    tar[offset] = b'X';
    let mut sink = MemorySink::default();

    let error = validate_workspace_tar(
        &mut Cursor::new(tar),
        &mut sink,
        WorkspaceArchiveLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect_err("corrupt body must fail");

    assert!(
        error
            .to_string()
            .contains("does not match its content digest")
    );
}

#[test]
fn archive_adapter_rejects_nonfinal_control_and_unsafe_types() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let mut builder = Builder::new(Vec::new());
    append_member(
        &mut builder,
        locality_protocol::RESERVED_EXPORT_METADATA_PATH,
        EntryType::file(),
        0o444,
        &serde_json::to_vec(&contract.control).expect("control"),
    );
    let mut link = Header::new_gnu();
    link.set_entry_type(EntryType::symlink());
    link.set_mode(0o555);
    link.set_uid(0);
    link.set_gid(0);
    link.set_mtime(0);
    link.set_size(0);
    link.set_path("Sales/escape").expect("link path");
    link.set_link_name("../../escape").expect("link target");
    link.set_cksum();
    builder.append(&link, io::empty()).expect("append link");
    builder.finish().expect("finish hostile tar");
    let mut sink = MemorySink::default();
    let error = validate_workspace_tar(
        &mut Cursor::new(builder.into_inner().expect("hostile bytes")),
        &mut sink,
        WorkspaceArchiveLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect_err("member after control must fail");
    assert!(error.to_string().contains("control member is not final"));
}

#[test]
fn archive_adapter_rejects_links_devices_and_fifos_before_staging_bodies() {
    let fixture = fixture();
    let contract = contract(&fixture);
    for (label, entry_type) in [
        ("symlink", EntryType::symlink()),
        ("hardlink", EntryType::hard_link()),
        ("block", EntryType::block_special()),
        ("character", EntryType::character_special()),
        ("fifo", EntryType::fifo()),
    ] {
        let mut builder = Builder::new(Vec::new());
        let mut header = Header::new_gnu();
        header.set_entry_type(entry_type);
        header.set_mode(0o555);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_size(0);
        header.set_path("Sales/hostile").expect("hostile path");
        if matches!(label, "symlink" | "hardlink") {
            header.set_link_name("../../escape").expect("link target");
        }
        header.set_cksum();
        builder
            .append(&header, io::empty())
            .expect("append hostile entry");
        builder.finish().expect("finish hostile archive");
        let mut sink = MemorySink::default();
        let error = validate_workspace_tar(
            &mut Cursor::new(builder.into_inner().expect("hostile bytes")),
            &mut sink,
            WorkspaceArchiveLimits::default(),
            &contract.session,
            &contract.offer,
        )
        .expect_err("special entry must fail");
        assert!(
            error.to_string().contains("special entries are forbidden"),
            "case {label}: {error}"
        );
        assert!(sink.files.is_empty(), "case {label} staged a file");
    }
}

#[test]
fn archive_adapter_rejects_trailing_data_and_casefold_collisions() {
    let valid_fixture = fixture();
    let contract = contract(&valid_fixture);
    let mut trailing = archive(&valid_fixture, &contract.control);
    trailing.extend_from_slice(b"trailing");
    let mut sink = MemorySink::default();
    let error = validate_workspace_tar(
        &mut Cursor::new(trailing),
        &mut sink,
        WorkspaceArchiveLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect_err("trailing data must fail");
    assert!(error.to_string().contains("trailing data"));

    let mut collision = fixture();
    collision.directories.push(FixtureDirectory {
        path: "Sales/projects".to_string(),
        scope_ordinal: Some(0),
    });
    let mut sink = MemorySink::default();
    let error = validate_workspace_tar(
        &mut Cursor::new(archive(&collision, &contract.control)),
        &mut sink,
        WorkspaceArchiveLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect_err("case-fold collision must fail");
    assert!(error.to_string().contains("case-fold collision"));
}

struct PanicAfterHeader {
    header: Cursor<Vec<u8>>,
}

impl Read for PanicAfterHeader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.header.position() == self.header.get_ref().len() as u64 {
            panic!("extension body was read before its raw header was rejected");
        }
        self.header.read(output)
    }
}

fn hostile_extension_header(entry_type: EntryType, size: u64) -> Vec<u8> {
    let mut header = Header::new_ustar();
    header
        .set_path("PaxHeaders/hostile")
        .expect("set extension path");
    header.set_entry_type(entry_type);
    header.set_mode(0o444);
    header.set_size(size);
    header.set_cksum();
    header.as_bytes().to_vec()
}

#[test]
fn raw_tar_gate_rejects_pax_and_gnu_metadata_before_buffering_bodies() {
    let fixture = fixture();
    let contract = contract(&fixture);
    for (label, entry_type, size, expected) in [
        ("oversized PAX", EntryType::XHeader, 16_385, "PAX extension"),
        ("GNU long name", EntryType::GNULongName, 8, "GNU long-name"),
        ("GNU long link", EntryType::GNULongLink, 8, "GNU long-name"),
    ] {
        let mut reader = PanicAfterHeader {
            header: Cursor::new(hostile_extension_header(entry_type, size)),
        };
        let mut sink = MemorySink::default();
        let error = validate_workspace_tar(
            &mut reader,
            &mut sink,
            WorkspaceArchiveLimits::default(),
            &contract.session,
            &contract.offer,
        )
        .expect_err(label);
        assert!(
            error.to_string().contains(expected),
            "case {label}: {error}"
        );
        assert!(sink.files.is_empty(), "case {label} staged content");
    }

    let mut trailing_extension = archive(&fixture, &contract.control);
    trailing_extension.truncate(trailing_extension.len() - 1024);
    trailing_extension.extend_from_slice(&hostile_extension_header(EntryType::XHeader, 6));
    let mut extension_body = [0_u8; 512];
    extension_body[..6].copy_from_slice(b"6 a=b\n");
    trailing_extension.extend_from_slice(&extension_body);
    trailing_extension.extend_from_slice(&[0_u8; 1024]);
    let error = validate_workspace_tar(
        &mut Cursor::new(trailing_extension),
        &mut MemorySink::default(),
        WorkspaceArchiveLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect_err("terminal control must be the final raw member");
    assert!(error.to_string().contains("not followed"), "{error}");
}

#[test]
fn staged_identity_and_zstd_archives_publish_complete_read_only_roots() {
    for encoding in [
        ReplicaArchiveEncoding::Identity,
        ReplicaArchiveEncoding::Zstd,
    ] {
        let fixture = fixture();
        let contract = contract(&fixture);
        let tar = archive(&fixture, &contract.control);
        let body = match encoding {
            ReplicaArchiveEncoding::Identity => tar,
            ReplicaArchiveEncoding::Zstd => {
                zstd::stream::encode_all(tar.as_slice(), 1).expect("encode workspace fixture")
            }
        };
        let directory = TestDirectory::new(match encoding {
            ReplicaArchiveEncoding::Identity => "identity",
            ReplicaArchiveEncoding::Zstd => "zstd",
        });

        let staged = stage_workspace_archive(
            ReplicaArchive::new(encoding, Cursor::new(body)),
            &directory.root(),
            WorkspaceMaterializationLimits::default(),
            &contract.session,
            &contract.offer,
        )
        .expect("stage workspace");
        assert!(!directory.root().exists());
        assert!(staged.staging_path().exists());
        let published = staged
            .publish_initial(&directory.root())
            .expect("publish workspace");

        assert_eq!(published.validated.files, 2);
        assert_eq!(
            fs::read(directory.root().join("Sales/README.md")).expect("read published file"),
            b"Public\n"
        );
        assert!(directory.root().join("Engineering").is_dir());
        directory.assert_only(&[".locality-Locality.publication.lock", "Locality"]);
        assert_read_only(&directory.root());
        assert_read_only(&directory.root().join("Sales/README.md"));
    }
}

#[cfg(unix)]
#[test]
fn destination_preflight_rejects_casefold_spelling_collisions() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("destination-spelling");
    fs::create_dir(directory.0.join("locality")).expect("create colliding sibling");
    let staged = stage_workspace_archive(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("stage workspace");
    let error = staged
        .publish_initial(&directory.root())
        .expect_err("colliding destination spelling must fail");
    assert!(error.to_string().contains("already exists"), "{error}");
    assert!(directory.0.join("locality").is_dir());
    let spellings = fs::read_dir(&directory.0)
        .expect("read destination parent")
        .map(|entry| entry.expect("read sibling").file_name())
        .collect::<Vec<_>>();
    assert!(spellings.contains(&"locality".into()));
    assert!(!spellings.contains(&"Locality".into()));
}

#[test]
fn truncated_staging_never_changes_an_existing_root() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let mut tar = archive(&fixture, &contract.control);
    tar.truncate(tar.len() - 700);
    let directory = TestDirectory::new("truncated-refresh");
    fs::create_dir(directory.root()).expect("create old root");
    fs::write(directory.root().join("old.txt"), b"old generation\n").expect("write old root");

    let error = stage_workspace_archive(
        ReplicaArchive::new(ReplicaArchiveEncoding::Identity, Cursor::new(tar)),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .err()
    .expect("truncated archive must not stage");
    assert!(error.to_string().contains("workspace tar"));

    assert_eq!(
        fs::read(directory.root().join("old.txt")).expect("old root survives"),
        b"old generation\n"
    );
    directory.assert_only(&["Locality"]);
}

#[cfg(all(
    unix,
    any(target_vendor = "apple", target_os = "linux", target_os = "android")
))]
#[test]
fn supported_unix_refresh_atomically_exchanges_complete_generation_roots() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("exchange");
    fs::create_dir(directory.root()).expect("create old root");
    fs::write(directory.root().join("old.txt"), b"old generation\n").expect("write old root");
    let staged = stage_workspace_archive(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("stage refresh");

    let published = staged
        .publish_exchange(&directory.root())
        .expect("atomic exchange");
    let old_generation = published.old_generation.expect("retained old generation");

    assert_eq!(
        fs::read(directory.root().join("Sales/README.md")).expect("new root"),
        b"Public\n"
    );
    assert_eq!(
        fs::read(old_generation.join("old.txt")).expect("old generation retained"),
        b"old generation\n"
    );
}

#[cfg(not(all(
    unix,
    any(target_vendor = "apple", target_os = "linux", target_os = "android")
)))]
#[test]
fn unsupported_exchange_preserves_the_existing_root() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("unsupported-exchange");
    fs::create_dir(directory.root()).expect("create old root");
    fs::write(directory.root().join("old.txt"), b"old generation\n").expect("write old root");
    let staged = stage_workspace_archive(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("stage refresh");
    let error = staged
        .publish_exchange(&directory.root())
        .expect_err("platform must fail closed without exchange");
    assert!(error.to_string().contains("exchange is unavailable"));
    assert_eq!(
        fs::read(directory.root().join("old.txt")).expect("old root survives"),
        b"old generation\n"
    );
}

struct FailAt(WorkspacePublicationCheckpoint);

impl WorkspacePublicationHooks for FailAt {
    fn checkpoint(&mut self, checkpoint: WorkspacePublicationCheckpoint) -> io::Result<()> {
        if checkpoint == self.0 {
            return Err(io::Error::other("injected durable-boundary failure"));
        }
        Ok(())
    }
}

struct PauseAtJournal {
    reached: mpsc::Sender<()>,
    resume: mpsc::Receiver<()>,
}

#[cfg(unix)]
struct RetargetAncestorBeforePublication {
    visible: PathBuf,
    replacement: PathBuf,
}

#[cfg(unix)]
impl WorkspacePublicationHooks for RetargetAncestorBeforePublication {
    fn before_publication(&mut self) -> io::Result<()> {
        use std::os::unix::fs::symlink;

        fs::remove_file(&self.visible)?;
        symlink(&self.replacement, &self.visible)
    }

    fn checkpoint(&mut self, _checkpoint: WorkspacePublicationCheckpoint) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(unix)]
struct ReplaceLockAtJournal {
    lock: PathBuf,
}

#[cfg(unix)]
impl WorkspacePublicationHooks for ReplaceLockAtJournal {
    fn checkpoint(&mut self, checkpoint: WorkspacePublicationCheckpoint) -> io::Result<()> {
        if checkpoint == WorkspacePublicationCheckpoint::JournalDurable {
            fs::remove_file(&self.lock)?;
            fs::write(&self.lock, b"replacement lock\n")?;
        }
        Ok(())
    }
}

struct ForgeMarkerAt {
    checkpoint: WorkspacePublicationCheckpoint,
    generation: PathBuf,
}

impl WorkspacePublicationHooks for ForgeMarkerAt {
    fn checkpoint(&mut self, checkpoint: WorkspacePublicationCheckpoint) -> io::Result<()> {
        if checkpoint == self.checkpoint {
            make_removable(&self.generation);
            fs::write(
                self.generation.join(".locality-ownership-v4"),
                [0x91_u8; 32],
            )?;
        }
        Ok(())
    }
}

impl WorkspacePublicationHooks for PauseAtJournal {
    fn checkpoint(&mut self, checkpoint: WorkspacePublicationCheckpoint) -> io::Result<()> {
        if checkpoint == WorkspacePublicationCheckpoint::JournalDurable {
            self.reached
                .send(())
                .map_err(|_| io::Error::other("journal pause observer disappeared"))?;
            self.resume
                .recv()
                .map_err(|_| io::Error::other("journal pause was not resumed"))?;
        }
        Ok(())
    }
}

struct SubstituteAt {
    checkpoint: WorkspacePublicationCheckpoint,
    path: PathBuf,
    retained: PathBuf,
}

impl WorkspacePublicationHooks for SubstituteAt {
    fn checkpoint(&mut self, checkpoint: WorkspacePublicationCheckpoint) -> io::Result<()> {
        if checkpoint == self.checkpoint {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&self.path, fs::Permissions::from_mode(0o755))?;
            }
            fs::rename(&self.path, &self.retained)?;
            fs::create_dir(&self.path)?;
            fs::write(self.path.join("substitute.txt"), b"must survive\n")?;
        }
        Ok(())
    }
}

#[test]
fn durable_entry_point_persists_receipt_for_loc_and_desktop_callers() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("durable-entry-point");

    let published = materialize_workspace_archive_durable(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("durably materialize workspace");
    let receipt = load_workspace_publication_receipt(&directory.root())
        .expect("load publication receipt")
        .expect("active receipt");

    assert_eq!(receipt.terminal_control, contract.control);
    assert_eq!(receipt.decoded_bytes, published.decoded_bytes);
    assert_eq!(receipt.version, 4);
    let receipt_json = serde_json::to_value(&receipt).expect("serialize receipt");
    assert!(receipt_json["generation_identity"]["inode"].is_u64());
    assert!(receipt_json["ownership_marker_identity"]["inode"].is_u64());
    assert_eq!(
        receipt_json["ownership_marker_nonce"]
            .as_str()
            .expect("marker nonce")
            .len(),
        64
    );
    assert!(receipt_json["ownership_tag"].as_str().is_some());
    assert!(
        !directory
            .0
            .join(".locality-Locality.publication.json")
            .exists()
    );
}

#[test]
fn concurrent_publication_waits_for_the_exclusive_publication_lock() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("concurrent-publication-lock");
    let root = directory.root();
    let staged = stage_workspace_archive(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &root,
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("stage publication");
    let (reached_tx, reached_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let publisher_root = root.clone();
    let publisher = std::thread::spawn(move || {
        let mut pause = PauseAtJournal {
            reached: reached_tx,
            resume: resume_rx,
        };
        publish_staged_workspace_with_hooks_secure(
            staged,
            &publisher_root,
            &test_ownership(),
            &mut pause,
        )
    });
    reached_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("publisher reached durable journal while holding lock");
    #[cfg(windows)]
    assert!(
        fs::rename(
            directory.0.join(".locality-Locality.publication.lock"),
            directory.0.join("replaced-publication.lock"),
        )
        .is_err(),
        "Windows lock object must deny rename/delete sharing while held"
    );

    let (started_tx, started_rx) = mpsc::channel();
    let (verified_tx, verified_rx) = mpsc::channel();
    let verifier_root = root.clone();
    let verifier = std::thread::spawn(move || {
        started_tx.send(()).expect("announce verifier");
        let result =
            recover_and_verify_workspace_publication_state(&verifier_root, &test_ownership());
        verified_tx.send(result).expect("report verifier result");
    });
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("verifier started");
    assert!(
        verified_rx
            .recv_timeout(Duration::from_millis(200))
            .is_err(),
        "concurrent verifier must wait for the publication lock"
    );

    resume_tx.send(()).expect("resume publication");
    publisher
        .join()
        .expect("publisher thread")
        .expect("publish under lock");
    assert!(
        verified_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("verifier completed after lock release")
            .expect("verify published workspace")
    );
    verifier.join().expect("verifier thread");
}

#[cfg(unix)]
#[test]
fn publication_fails_closed_when_named_lock_is_unlinked_and_replaced() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("replaced-publication-lock");
    let root = directory.root();
    let staged = stage_workspace_archive(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &root,
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("stage publication");
    let lock = directory.0.join(".locality-Locality.publication.lock");
    let mut replace = ReplaceLockAtJournal { lock: lock.clone() };

    let error = publish_staged_workspace_with_hooks(staged, &root, &mut replace)
        .expect_err("replacement lock must detach the held lock identity");
    assert!(error.to_string().contains("lock detached"));
    assert!(!root.exists());
    assert_eq!(
        fs::read(lock).expect("replacement lock survives"),
        b"replacement lock\n"
    );
    assert!(
        fs::read_dir(&directory.0)
            .expect("read publication parent")
            .all(|entry| !entry
                .expect("publication entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".locality-stage-"))
    );
}

#[cfg(unix)]
#[test]
fn publication_fails_closed_when_parent_is_renamed_and_replaced() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("renamed-publication-parent");
    let root = directory.root();
    let staged = stage_workspace_archive(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &root,
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("stage publication");
    let (reached_tx, reached_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let publisher_root = root.clone();
    let publisher = std::thread::spawn(move || {
        let mut pause = PauseAtJournal {
            reached: reached_tx,
            resume: resume_rx,
        };
        publish_staged_workspace_with_hooks_secure(
            staged,
            &publisher_root,
            &test_ownership(),
            &mut pause,
        )
    });
    reached_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("publisher reached durable journal");

    let retained = directory.0.with_extension("retained-parent");
    fs::rename(&directory.0, &retained).expect("rename locked parent");
    fs::create_dir(&directory.0).expect("create replacement parent");
    fs::write(directory.0.join("substitute.txt"), b"must survive\n")
        .expect("write replacement sentinel");
    resume_tx.send(()).expect("resume anchored publication");
    let error = publisher
        .join()
        .expect("publisher thread")
        .expect_err("detached visible parent must not report publication success");
    assert!(error.to_string().contains("parent detached"));

    assert_eq!(
        fs::read(directory.0.join("substitute.txt")).expect("replacement survives"),
        b"must survive\n"
    );
    assert!(!directory.root().exists());
    assert!(!retained.join("Locality").exists());
    assert!(!retained.join(".locality-Locality.receipt.json").exists());
    assert!(
        retained
            .join(".locality-Locality.publication.json")
            .exists()
    );
    assert!(
        fs::read_dir(&retained)
            .expect("read retained parent")
            .all(|entry| !entry
                .expect("retained entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".locality-stage-"))
    );

    make_removable(&retained);
    fs::remove_dir_all(&retained).expect("remove retained parent");
}

#[cfg(unix)]
#[test]
fn generation2_rejects_ancestor_symlink_retarget_at_publication_commit() {
    use std::os::unix::fs::symlink;

    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("generation2-parent-retarget");
    let original = directory.0.join("original");
    let replacement = directory.0.join("replacement");
    let original_parent = original.join("workspace");
    let replacement_parent = replacement.join("workspace");
    fs::create_dir_all(&original_parent).expect("create original parent");
    fs::create_dir_all(&replacement_parent).expect("create replacement parent");
    let visible = directory.0.join("visible");
    symlink(&original, &visible).expect("create visible ancestor symlink");
    let root = visible.join("workspace/Locality");
    let staged = stage_workspace_archive(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &root,
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("stage publication through symlink ancestor");
    let mut retarget = RetargetAncestorBeforePublication {
        visible,
        replacement,
    };

    let error =
        publish_staged_workspace_with_hooks_secure(staged, &root, &test_ownership(), &mut retarget)
            .expect_err("retargeted publication parent must fail closed");

    assert!(error.to_string().contains("parent detached"), "{error}");
    assert!(!original_parent.join("Locality").exists());
    assert!(!replacement_parent.join("Locality").exists());
}

#[cfg(unix)]
#[test]
fn fifo_publication_state_is_rejected_without_blocking() {
    let directory = TestDirectory::new("fifo-publication-state");
    #[cfg(target_vendor = "apple")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let receipt_path = directory.0.join(".locality-Locality.receipt.json");
        let receipt = CString::new(receipt_path.as_os_str().as_bytes()).expect("FIFO path CString");
        // SAFETY: the path is a live NUL-terminated C string and the mode is valid.
        assert_eq!(unsafe { libc::mkfifo(receipt.as_ptr(), 0o600) }, 0);
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        use rustix::fs::{Mode, OFlags};

        let parent = rustix::fs::open(
            &directory.0,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("open test parent");
        rustix::fs::mkfifoat(
            &parent,
            ".locality-Locality.receipt.json",
            Mode::from_raw_mode(0o600),
        )
        .expect("create publication-state fifo");
    }
    let root = directory.root();
    let (result_tx, result_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        result_tx
            .send(load_workspace_publication_receipt(&root))
            .expect("report fifo read result");
    });
    let error = result_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("FIFO state read must not block")
        .expect_err("FIFO must not be accepted as publication state");
    assert!(error.to_string().contains("not an ordinary file"));
    reader.join().expect("FIFO reader thread");
}

#[test]
fn oversized_publication_state_is_rejected_before_reading() {
    let directory = TestDirectory::new("oversized-publication-state");
    let receipt = directory.0.join(".locality-Locality.receipt.json");
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&receipt)
        .expect("create oversized receipt");
    file.set_len(8 * 1024 * 1024 + 1)
        .expect("size oversized receipt");

    let error = load_workspace_publication_receipt(&directory.root())
        .expect_err("oversized publication state must fail closed");
    assert!(error.to_string().contains("fixed size limit"));
}

#[cfg(unix)]
#[test]
fn symlink_publication_state_is_rejected_without_following() {
    use std::os::unix::fs::symlink;

    let directory = TestDirectory::new("symlink-publication-state");
    let target = directory.0.join("attacker-state.json");
    fs::write(&target, b"{}\n").expect("write symlink target");
    symlink(&target, directory.0.join(".locality-Locality.receipt.json"))
        .expect("create state symlink");

    load_workspace_publication_receipt(&directory.root())
        .expect_err("publication state symlink must not be followed");
    assert_eq!(fs::read(target).expect("target survives"), b"{}\n");
}

#[cfg(windows)]
#[test]
fn windows_reparse_publication_state_is_rejected_without_following() {
    use std::os::windows::fs::symlink_file;

    let directory = TestDirectory::new("reparse-publication-state");
    let target = directory.0.join("attacker-state.json");
    fs::write(&target, b"{}\r\n").expect("write reparse target");
    let receipt = directory.0.join(".locality-Locality.receipt.json");
    symlink_file(&target, &receipt).unwrap_or_else(|error| {
        panic!(
            "create state reparse point (Windows CI must enable symlink privilege for this \
             security regression): {error}"
        )
    });

    let error = load_workspace_publication_receipt(&directory.root())
        .expect_err("publication state reparse point must not be followed");
    assert!(error.to_string().contains("reparse point"));
    assert_eq!(fs::read(target).expect("target survives"), b"{}\r\n");
}

#[cfg(windows)]
#[test]
fn windows_verifies_the_sealed_marker_through_read_only_handles() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("windows-read-only-marker");
    materialize_workspace_archive_durable(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("publish read-only workspace");
    assert_read_only(&directory.root().join(".locality-ownership-v4"));
    assert!(
        recover_and_verify_workspace_publication_state(&directory.root(), &test_ownership())
            .expect("verify read-only marker without create/delete access")
    );
}

#[test]
fn stale_receipt_cannot_authorize_exchange_after_root_rename() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("stale-receipt-rename");
    materialize_workspace_archive_durable(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("publish identity-bound receipt");
    let renamed = directory.0.join("renamed-generation");
    make_removable(&directory.root());
    fs::rename(directory.root(), &renamed).expect("rename published generation");
    fs::create_dir(directory.root()).expect("create unrelated replacement root");
    fs::write(directory.root().join("unrelated.txt"), b"must survive\n")
        .expect("write unrelated replacement");

    let error = materialize_workspace_archive_durable(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect_err("stale sibling receipt must not authorize exchange");

    assert!(error.to_string().contains(".locality-ownership-v4"));
    assert_eq!(
        fs::read(directory.root().join("unrelated.txt")).expect("replacement survives"),
        b"must survive\n"
    );
    assert!(renamed.join("Sales/README.md").is_file());
}

#[test]
fn stale_receipt_cannot_authorize_exchange_after_root_delete_and_recreate() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("stale-receipt-delete");
    materialize_workspace_archive_durable(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("publish identity-bound receipt");
    make_removable(&directory.root());
    fs::remove_dir_all(directory.root()).expect("delete published generation");
    fs::create_dir(directory.root()).expect("recreate unrelated root");
    fs::write(directory.root().join("unrelated.txt"), b"must survive\n")
        .expect("write unrelated replacement");

    let error = materialize_workspace_archive_durable(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect_err("receipt for deleted generation must not authorize exchange");

    assert!(error.to_string().contains(".locality-ownership-v4"));
    assert_eq!(
        fs::read(directory.root().join("unrelated.txt")).expect("replacement survives"),
        b"must survive\n"
    );
}

#[test]
fn forged_matching_receipt_cannot_authorize_exchange_or_cleanup() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("forged-receipt-identity");
    materialize_workspace_archive_durable(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("publish identity-bound receipt");
    let receipt_path = directory.0.join(".locality-Locality.receipt.json");
    let mut receipt: Value =
        serde_json::from_slice(&fs::read(&receipt_path).expect("read receipt"))
            .expect("decode receipt");
    let decoded_bytes = receipt["decoded_bytes"]
        .as_u64()
        .expect("receipt decoded bytes");
    receipt["decoded_bytes"] = Value::from(decoded_bytes.wrapping_add(1));
    fs::write(
        &receipt_path,
        serde_json::to_vec(&receipt).expect("encode forged receipt"),
    )
    .expect("forge matching receipt");

    let error = materialize_workspace_archive_durable(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect_err("forged receipt must not authorize refresh");

    assert!(error.to_string().contains("authenticated"));
    assert_eq!(
        fs::read(directory.root().join("Sales/README.md")).expect("old root survives"),
        b"Public\n"
    );
}

#[test]
fn forged_marker_content_with_same_file_id_cannot_authorize_cleanup() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("forged-marker-content");
    materialize_workspace_archive_durable(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("publish nonce-bound receipt");
    make_removable(&directory.root());
    let marker_path = directory.root().join(".locality-ownership-v4");
    #[cfg(unix)]
    let marker_inode = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(&marker_path).expect("marker metadata").ino()
    };
    fs::write(&marker_path, [0x44_u8; 32]).expect("replace marker contents in place");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            fs::metadata(&marker_path)
                .expect("forged marker metadata")
                .ino(),
            marker_inode,
            "content forgery retains the marker inode"
        );
    }

    let error = materialize_workspace_archive_durable(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect_err("forged marker content must not authorize refresh or cleanup");

    assert!(error.to_string().contains("authenticated"));
    assert_eq!(
        fs::read(directory.root().join("Sales/README.md")).expect("owned root survives"),
        b"Public\n"
    );
}

#[test]
fn marker_forged_after_journal_is_rejected_immediately_before_exchange() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("forged-marker-before-exchange");
    materialize_workspace_archive_durable(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("publish old generation");
    let staged = stage_workspace_archive(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("stage replacement");
    let staging_path = staged.staging_path().to_path_buf();
    let mut forge = ForgeMarkerAt {
        checkpoint: WorkspacePublicationCheckpoint::JournalDurable,
        generation: staging_path,
    };

    let error = publish_staged_workspace_with_hooks(staged, &directory.root(), &mut forge)
        .expect_err("forged staged marker must stop exchange");

    assert!(error.to_string().contains("authenticated"));
    assert_eq!(
        fs::read(directory.root().join("Sales/README.md")).expect("old root survives"),
        b"Public\n"
    );
}

#[cfg(all(
    unix,
    any(target_vendor = "apple", target_os = "linux", target_os = "android")
))]
#[test]
fn marker_forged_after_exchange_is_rejected_before_cleanup() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("forged-marker-before-cleanup");
    materialize_workspace_archive_durable(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("publish old generation");
    let staged = stage_workspace_archive(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("stage replacement");
    let old_generation = staged.staging_path().to_path_buf();
    let mut forge = ForgeMarkerAt {
        checkpoint: WorkspacePublicationCheckpoint::ReceiptDurable,
        generation: old_generation.clone(),
    };

    let error = publish_staged_workspace_with_hooks(staged, &directory.root(), &mut forge)
        .expect_err("forged old marker must stop cleanup");

    assert!(error.to_string().contains("authenticated"));
    assert!(old_generation.exists(), "old generation is retained");
    assert_eq!(
        fs::read(old_generation.join("Sales/README.md")).expect("old generation survives"),
        b"Public\n"
    );
}

#[cfg(not(all(
    unix,
    any(target_vendor = "apple", target_os = "linux", target_os = "android")
)))]
#[test]
fn post_exchange_marker_hook_fails_closed_when_exchange_is_unsupported() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("unsupported-forged-marker-before-cleanup");
    materialize_workspace_archive_durable(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("publish old generation");
    let active_receipt = load_workspace_publication_receipt(&directory.root())
        .expect("load active receipt")
        .expect("active receipt");
    let staged = stage_workspace_archive(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("stage replacement");
    let staging_path = staged.staging_path().to_path_buf();
    let mut forge = ForgeMarkerAt {
        checkpoint: WorkspacePublicationCheckpoint::ReceiptDurable,
        generation: staging_path.clone(),
    };

    let error = publish_staged_workspace_with_hooks(staged, &directory.root(), &mut forge)
        .expect_err("platform must fail closed before a post-exchange hook");
    match error {
        WorkspaceMaterializationError::Filesystem(ReplicaMaterializationError::Publish(source)) => {
            assert_eq!(source.kind(), io::ErrorKind::Unsupported)
        }
        other => panic!("expected typed unsupported exchange error, got {other:?}"),
    }

    assert_eq!(
        load_workspace_publication_receipt(&directory.root())
            .expect("reload active receipt")
            .expect("active receipt survives"),
        active_receipt
    );
    assert_eq!(
        fs::read(directory.root().join("Sales/README.md")).expect("active root survives"),
        b"Public\n"
    );
    assert!(!staging_path.exists(), "recovery removes unused staging");
    assert!(
        !directory
            .0
            .join(".locality-Locality.publication.json")
            .exists(),
        "recovery clears the failed publication journal"
    );
}

#[test]
fn reissued_same_profile_secret_recovers_but_rotated_key_fails_closed() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("receipt-capability-reuse");
    materialize_workspace_archive_durable(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("publish capability-bound receipt");

    let error = materialize_workspace_archive_durable_secure(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
        &WorkspaceOwnershipCapability::new([0x6b; 32]),
    )
    .expect_err("another profile capability must not reuse the receipt");

    assert!(error.to_string().contains("authenticated"));
    assert_eq!(
        fs::read(directory.root().join("Sales/README.md")).expect("owned root survives"),
        b"Public\n"
    );
    assert!(
        recover_and_verify_workspace_publication_state(
            &directory.root(),
            &WorkspaceOwnershipCapability::new([0x5a; 32]),
        )
        .expect("a reissued key with the same profile secret recovers ownership")
    );
}

#[test]
fn injected_barrier_failure_leaves_old_root_and_recovery_discards_staging() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("barrier-failure");
    materialize_workspace_archive_durable(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("publish old root");
    let old_body = fs::read(directory.root().join("Sales/README.md")).expect("old body");
    let staged = stage_workspace_archive(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("stage refresh");

    publish_staged_workspace_with_hooks(
        staged,
        &directory.root(),
        &mut FailAt(WorkspacePublicationCheckpoint::JournalDurable),
    )
    .expect_err("injected barrier failure");
    assert_eq!(
        fs::read(directory.root().join("Sales/README.md")).expect("old root survives"),
        old_body
    );

    recover_workspace_publication(&directory.root()).expect("recover failed refresh");
    assert!(
        !directory
            .0
            .join(".locality-Locality.publication.json")
            .exists()
    );
    assert_eq!(
        fs::read(directory.root().join("Sales/README.md")).expect("old root remains"),
        old_body
    );
}

#[test]
fn crash_after_initial_publish_recovers_receipt_without_partial_tree() {
    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("initial-crash");
    let staged = stage_workspace_archive(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("stage initial root");

    publish_staged_workspace_with_hooks(
        staged,
        &directory.root(),
        &mut FailAt(WorkspacePublicationCheckpoint::PublicationComplete),
    )
    .expect_err("simulate crash after publish");
    assert_eq!(
        fs::read(directory.root().join("Sales/README.md")).expect("complete new root"),
        b"Public\n"
    );
    assert!(
        load_workspace_publication_receipt(&directory.root())
            .expect("receipt query")
            .is_none()
    );

    recover_workspace_publication(&directory.root()).expect("recover initial publication");
    assert_eq!(
        load_workspace_publication_receipt(&directory.root())
            .expect("receipt query")
            .expect("recovered receipt")
            .terminal_control,
        contract.control
    );
}

#[cfg(all(
    unix,
    any(target_vendor = "apple", target_os = "linux", target_os = "android")
))]
#[test]
fn crash_after_exchange_retains_old_generation_until_receipt_recovery() {
    use std::os::unix::fs::MetadataExt;

    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("exchange-crash");
    materialize_workspace_archive_durable(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("publish old generation");
    let old_inode = fs::metadata(directory.root())
        .expect("old root metadata")
        .ino();
    let staged = stage_workspace_archive(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("stage refresh");
    let staging_path = staged.staging_path().to_path_buf();

    publish_staged_workspace_with_hooks(
        staged,
        &directory.root(),
        &mut FailAt(WorkspacePublicationCheckpoint::PublicationComplete),
    )
    .expect_err("simulate crash after exchange");
    assert_ne!(
        fs::metadata(directory.root())
            .expect("new root metadata")
            .ino(),
        old_inode
    );
    assert_eq!(
        fs::metadata(&staging_path)
            .expect("retained old generation")
            .ino(),
        old_inode
    );

    recover_workspace_publication(&directory.root()).expect("recover exchanged root");
    assert!(!staging_path.exists());
    assert!(
        load_workspace_publication_receipt(&directory.root())
            .expect("receipt query")
            .is_some()
    );
}

#[cfg(all(
    unix,
    any(target_vendor = "apple", target_os = "linux", target_os = "android")
))]
#[test]
fn recovery_accepts_new_root_after_durable_receipt_and_completed_cleanup() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("post-cleanup-recovery");
    materialize_workspace_archive_durable(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("publish old generation");
    let staged = stage_workspace_archive(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("stage refresh");
    let staging = staged.staging_path().to_path_buf();
    publish_staged_workspace_with_hooks(
        staged,
        &directory.root(),
        &mut FailAt(WorkspacePublicationCheckpoint::CleanupComplete),
    )
    .expect_err("simulate crash after old-generation cleanup");
    assert!(!staging.exists(), "old generation cleanup completed");

    fs::set_permissions(&directory.root(), fs::Permissions::from_mode(0o755))
        .expect("damage published root mode");
    recover_workspace_publication(&directory.root()).expect("recover post-cleanup state");
    assert_read_only(&directory.root());
    assert!(
        !directory
            .0
            .join(".locality-Locality.publication.json")
            .exists()
    );
}

#[cfg(all(
    unix,
    any(target_vendor = "apple", target_os = "linux", target_os = "android")
))]
#[test]
fn publication_and_cleanup_never_delete_substituted_generation_paths() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = fixture();
    let contract = contract(&fixture);
    let directory = TestDirectory::new("identity-substitution");
    materialize_workspace_archive_durable(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("publish old generation");
    fs::set_permissions(&directory.root(), fs::Permissions::from_mode(0o755))
        .expect("make root replaceable for race injection");

    let staged = stage_workspace_archive(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("stage substituted publication");
    let staging = staged.staging_path().to_path_buf();
    let retained_destination = directory.0.join("retained-destination");
    publish_staged_workspace_with_hooks(
        staged,
        &directory.root(),
        &mut SubstituteAt {
            checkpoint: WorkspacePublicationCheckpoint::JournalDurable,
            path: directory.root(),
            retained: retained_destination.clone(),
        },
    )
    .expect_err("destination substitution must fail closed");
    let substitute = [directory.root(), staging.clone()]
        .into_iter()
        .find(|path| path.join("substitute.txt").exists())
        .expect("substitute survives at one side of an atomic exchange");
    assert_eq!(
        fs::read(substitute.join("substitute.txt")).unwrap(),
        b"must survive\n"
    );
    assert!(retained_destination.join("Sales/README.md").exists());

    make_removable(&substitute);
    fs::remove_dir_all(&substitute).expect("remove test substitute");
    if directory.root().exists() {
        make_removable(&directory.root());
        fs::remove_dir_all(directory.root()).expect("remove exchanged root");
    }
    fs::rename(&retained_destination, directory.root()).expect("restore journal destination");
    recover_workspace_publication(&directory.root()).expect("recover failed exchange");

    let staged = stage_workspace_archive(
        ReplicaArchive::new(
            ReplicaArchiveEncoding::Identity,
            Cursor::new(archive(&fixture, &contract.control)),
        ),
        &directory.root(),
        WorkspaceMaterializationLimits::default(),
        &contract.session,
        &contract.offer,
    )
    .expect("stage cleanup substitution");
    let staging = staged.staging_path().to_path_buf();
    let retained_old = directory.0.join("retained-old-generation");
    publish_staged_workspace_with_hooks(
        staged,
        &directory.root(),
        &mut SubstituteAt {
            checkpoint: WorkspacePublicationCheckpoint::ReceiptDurable,
            path: staging.clone(),
            retained: retained_old.clone(),
        },
    )
    .expect_err("cleanup substitution must fail closed");
    assert_eq!(
        fs::read(staging.join("substitute.txt")).expect("cleanup substitute survives"),
        b"must survive\n"
    );
    assert!(retained_old.join("Sales/README.md").exists());
}

fn assert_read_only(path: &Path) {
    assert!(
        fs::metadata(path)
            .expect("read path metadata")
            .permissions()
            .readonly(),
        "{} is not read-only",
        path.display()
    );
}

fn make_removable(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        let mut permissions = metadata.permissions();
        permissions.set_readonly(false);
        let _ = fs::set_permissions(path, permissions);
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                make_removable(&entry.path());
            }
        }
    } else {
        let mut permissions = metadata.permissions();
        permissions.set_readonly(false);
        let _ = fs::set_permissions(path, permissions);
    }
}
