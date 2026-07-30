use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Cursor, Read};

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
    WorkspaceNamespacedInventoryV2, WorkspaceScopeSourceAuthorityV2,
};
use locality_protocol::{
    DeliveredBodyDigestV2, ExportCompletionReceipt, ExportV2FilePaxMetadata,
    PAX_WINNING_SCOPE_ORDINAL, ScopeAuthorizedWritableExportMetadata, WritableMetadataEntry,
    canonical_writable_metadata_sha256,
};
use localityd::workspace_archive::{
    WorkspaceArchiveLimits, WorkspaceArchiveSink, validate_workspace_tar,
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tar::{Builder, EntryType, Header};

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
    control: WorkspaceExportTerminalControlV2,
}

fn fixture() -> Fixture {
    serde_json::from_slice(include_bytes!("../fixtures/workspace-materializer-v2.json"))
        .expect("workspace materializer fixture")
}

fn contract(fixture: &Fixture) -> Contract {
    let session: WorkspaceProfileSessionV2 =
        serde_json::from_slice(WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON).expect("session fixture");
    let scope_sources = vec![
        WorkspaceScopeSourceAuthorityV2::new(0, SourceConnectionId::new("source-notion")),
        WorkspaceScopeSourceAuthorityV2::new(1, SourceConnectionId::new("source-drive")),
        WorkspaceScopeSourceAuthorityV2::new(2, SourceConnectionId::new("source-notion")),
        WorkspaceScopeSourceAuthorityV2::new(3, SourceConnectionId::new("source-drive")),
    ];
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

    let writable_metadata = ScopeAuthorizedWritableExportMetadata {
        versions: placeholder_offer.offer().versions,
        session_id: SessionId::new("session-scope-7"),
        export_attempt_id: ExportAttemptId::new("export-attempt-9").expect("attempt ID"),
        source_generations: placeholder_offer.offer().source_generations.clone(),
        writable_entries: vec![WritableMetadataEntry {
            projection_id: ProjectionId::new("projection-roadmap"),
            logical_path: LogicalPath::new("Projects/Roadmap/page.md").expect("logical path"),
            source_remote_ids: vec![RemoteId::new("page-roadmap")],
            delivered_content_sha256: sha256_label(b"# Roadmap\n"),
            provider_precondition: "opaque-v4".to_string(),
            effective_actions: BTreeSet::from([SourceAction::Read, SourceAction::Update]),
            baseline_required: true,
        }],
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
        control,
    }
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
    assert_eq!(sink.directories.len(), 4);
    assert_eq!(sink.files["Sales/README.md"], b"Public\n");
    assert!(
        !sink
            .files
            .contains_key(locality_protocol::RESERVED_EXPORT_METADATA_PATH)
    );
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
