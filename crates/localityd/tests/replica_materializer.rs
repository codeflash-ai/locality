use std::collections::BTreeSet;
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};

use locality_core::portable::{
    ExportAttemptId, LogicalPath, ProjectionFileKind, ProjectionId, SessionId, SourceAction,
    SourceConnectionId, SourceGenerationId,
};
use locality_protocol::{
    CanonicalControlOrderKey, CanonicalDirectoryOrderKey, CanonicalExportRecord,
    CanonicalFileOrderKey, DeliveredBodyDigestV2, ExportAttemptLimits, ExportCompletionReceipt,
    ExportTerminalControlV2, OrderedSourceGeneration, PAX_CONTENT_SHA256, PAX_EFFECTIVE_ACTIONS,
    PAX_FILE_KIND, PAX_PROJECTION_ID, PAX_SOURCE_CONNECTION_ID, PAX_WINNING_SCOPE_ORDINAL,
    SCOPE_AUTHORIZED_COMPONENT_VERSIONS, ScopeAuthorizedWritableExportMetadata, SealedExportOffer,
    TarContentEncoding, canonical_export_inventory_sha256, canonical_writable_metadata_sha256,
};
use localityd::remote_truth::{ReplicaArchive, ReplicaArchiveEncoding};
use localityd::replica_materializer::{
    ExpectedReplicaMaterializationReceipt, ReplicaMaterializationLimits,
    ReplicaMaterializationSummary, materialize_replica_archive,
    materialize_replica_archive_with_expected_receipt,
    materialize_scope_authorized_replica_archive,
};
use sha2::{Digest, Sha256};
use tar::{Builder, EntryType, Header};

static TEST_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = TEST_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "localityd-replica-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create isolated test directory");
        Self(path)
    }

    fn destination(&self) -> PathBuf {
        self.0.join("replica")
    }

    fn assert_no_staging_or_destination(&self) {
        let entries = fs::read_dir(&self.0)
            .expect("read test directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("read test entries");
        assert!(entries.is_empty(), "failed materialization leaked files");
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        make_removable(&self.0);
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone)]
struct TestMember {
    path: Vec<u8>,
    entry_type: EntryType,
    mode: u32,
    data: Vec<u8>,
    link_name: Option<Vec<u8>>,
}

impl TestMember {
    fn file(path: impl AsRef<[u8]>, data: impl AsRef<[u8]>) -> Self {
        Self {
            path: path.as_ref().to_vec(),
            entry_type: EntryType::file(),
            mode: 0o444,
            data: data.as_ref().to_vec(),
            link_name: None,
        }
    }

    fn directory(path: impl AsRef<[u8]>) -> Self {
        Self {
            path: path.as_ref().to_vec(),
            entry_type: EntryType::dir(),
            mode: 0o555,
            data: Vec::new(),
            link_name: None,
        }
    }
}

fn tar_archive(members: &[TestMember]) -> Vec<u8> {
    let mut builder = Builder::new(Vec::new());
    for member in members {
        assert!(member.path.len() <= 100, "test path fits GNU name field");
        let mut header = Header::new_gnu();
        header.set_entry_type(member.entry_type);
        header.set_mode(member.mode);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_size(member.data.len() as u64);
        {
            let bytes = header.as_mut_bytes();
            bytes[..100].fill(0);
            bytes[..member.path.len()].copy_from_slice(&member.path);
        }
        if let Some(link_name) = &member.link_name {
            header
                .set_link_name_literal(link_name)
                .expect("set raw link name");
        }
        header.set_cksum();
        builder
            .append(&header, member.data.as_slice())
            .expect("append test tar member");
    }
    builder.finish().expect("finish test tar");
    builder.into_inner().expect("collect test tar")
}

fn materialize_identity(
    bytes: Vec<u8>,
    destination: &Path,
    limits: ReplicaMaterializationLimits,
) -> Result<ReplicaMaterializationSummary, String> {
    materialize_replica_archive(
        ReplicaArchive::new(ReplicaArchiveEncoding::Identity, Cursor::new(bytes)),
        destination,
        limits,
    )
    .map_err(|error| error.to_string())
}

fn expected_receipt(decoded_tar: &[u8], entries: u64) -> ExpectedReplicaMaterializationReceipt {
    ExpectedReplicaMaterializationReceipt {
        decoded_tar_sha256: Sha256::digest(decoded_tar).into(),
        decoded_bytes: decoded_tar.len() as u64,
        entries,
    }
}

fn sha256_label(digest: [u8; 32]) -> String {
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{hex}")
}

fn rejected_identity(label: &str, bytes: Vec<u8>, limits: ReplicaMaterializationLimits) -> String {
    let root = TestDirectory::new(label);
    let error = materialize_identity(bytes, &root.destination(), limits)
        .expect_err("hostile archive must be rejected");
    root.assert_no_staging_or_destination();
    error
}

#[test]
fn identity_archive_publishes_only_read_only_files_and_directories() {
    let root = TestDirectory::new("identity");
    let archive = tar_archive(&[
        TestMember::directory("docs/"),
        TestMember::file("docs/readme.md", "hello\n"),
        TestMember::file("top.txt", "top\n"),
    ]);

    let summary = materialize_identity(
        archive,
        &root.destination(),
        ReplicaMaterializationLimits::default(),
    )
    .expect("materialize identity tar");

    assert_eq!(
        summary,
        ReplicaMaterializationSummary {
            entries: 3,
            files: 2,
            directories: 1,
            materialized_bytes: 10,
            decoded_bytes: 3_584,
        }
    );
    assert_eq!(
        fs::read(root.destination().join("docs/readme.md")).expect("read materialized file"),
        b"hello\n"
    );
    assert_eq!(
        fs::read(root.destination().join("top.txt")).expect("read top file"),
        b"top\n"
    );
    assert_modes(&root.destination(), 0o555);
    assert_modes(&root.destination().join("docs"), 0o555);
    assert_modes(&root.destination().join("docs/readme.md"), 0o444);
    assert_modes(&root.destination().join("top.txt"), 0o444);
}

#[test]
fn exact_receipt_identity_archive_publishes_after_decoded_tar_verification() {
    let root = TestDirectory::new("identity-receipt");
    let tar = tar_archive(&[TestMember::file("verified.txt", "identity\n")]);
    let expected = expected_receipt(&tar, 1);

    let summary = materialize_replica_archive_with_expected_receipt(
        ReplicaArchive::new(ReplicaArchiveEncoding::Identity, Cursor::new(tar)),
        &root.destination(),
        ReplicaMaterializationLimits::default(),
        expected,
    )
    .expect("materialize identity tar with exact receipt");

    assert_eq!(summary.entries, 1);
    assert_eq!(summary.decoded_bytes, expected.decoded_bytes);
    assert_eq!(
        fs::read(root.destination().join("verified.txt")).expect("read verified file"),
        b"identity\n"
    );
}

#[derive(Clone, Copy)]
enum V2ArchiveMutation {
    None,
    MissingPax,
    DuplicatePax,
    UnknownPax,
    LocDirectoryHeader,
    MissingReceipt,
    MalformedReceipt,
    NoncanonicalReceipt,
    TrailingReceipt,
    UnknownReceiptField,
    DuplicateReceipt,
    ReceiptNotFinal,
    ReceiptBodyDigestMismatch,
    ReceiptGenerationMismatch,
    ReceiptCountMismatch,
}

fn v2_offer_and_archive(mutation: V2ArchiveMutation) -> (SealedExportOffer, Vec<u8>) {
    let body = b"scope authorized\n";
    let content_sha256 = sha256_label(Sha256::digest(body).into());
    let source_connection_id = SourceConnectionId::new("source-notion");
    let projection_id = ProjectionId::new("projection-readme");
    let actions = BTreeSet::from([SourceAction::Read, SourceAction::Search]);
    let records = vec![
        CanonicalExportRecord::Directory {
            order_key: CanonicalDirectoryOrderKey {
                depth: 1,
                logical_path: LogicalPath::new("docs").expect("directory path"),
            },
        },
        CanonicalExportRecord::File {
            order_key: CanonicalFileOrderKey {
                winning_scope_ordinal: 0,
                parent_path: Some(LogicalPath::new("docs").expect("parent path")),
                logical_path: LogicalPath::new("docs/readme.md").expect("file path"),
                projection_id: projection_id.clone(),
            },
            source_connection_id: source_connection_id.clone(),
            file_kind: ProjectionFileKind::Markdown,
            effective_actions: actions,
            content_sha256: content_sha256.clone(),
            byte_length: body.len() as u64,
        },
        CanonicalExportRecord::Control {
            order_key: CanonicalControlOrderKey { ordinal: 0 },
            member_path: locality_protocol::RESERVED_EXPORT_METADATA_PATH.to_string(),
        },
    ];
    let source_generations = vec![OrderedSourceGeneration {
        ordinal: 0,
        source_connection_id: source_connection_id.clone(),
        source_generation_id: SourceGenerationId::new("generation-9").expect("generation ID"),
    }];
    let inventory_sha256 =
        canonical_export_inventory_sha256(&records).expect("canonical inventory");
    let writable_metadata = ScopeAuthorizedWritableExportMetadata {
        versions: SCOPE_AUTHORIZED_COMPONENT_VERSIONS,
        session_id: SessionId::new("session-scope"),
        export_attempt_id: ExportAttemptId::new("attempt-9").expect("attempt ID"),
        source_generations: source_generations.clone(),
        writable_entries: Vec::new(),
    };
    let writable_metadata_sha256 =
        canonical_writable_metadata_sha256(&writable_metadata).expect("writable metadata digest");
    let offer = SealedExportOffer {
        versions: SCOPE_AUTHORIZED_COMPONENT_VERSIONS,
        session_id: writable_metadata.session_id.clone(),
        export_attempt_id: writable_metadata.export_attempt_id.clone(),
        source_generations: source_generations.clone(),
        media_type: "application/x-tar".to_string(),
        content_encoding: TarContentEncoding::Identity,
        limits: ExportAttemptLimits {
            max_files: 10,
            max_directories: 10,
            max_content_bytes: 1024,
        },
        control_entry_count: 1,
        file_count: 1,
        directory_count: 1,
        archive_entry_count: 3,
        selected_content_bytes: body.len() as u64,
        inventory_sha256: inventory_sha256.clone(),
        writable_metadata_sha256: writable_metadata_sha256.clone(),
        sealed_at: "2026-07-23T20:00:00Z".to_string(),
        expires_at: "2026-07-23T20:10:00Z".to_string(),
    };
    offer.validate_inventory(&records).expect("valid offer");

    let mut body_digest = DeliveredBodyDigestV2::new(1);
    body_digest
        .update_file(&projection_id, body)
        .expect("body digest update");
    let delivered_body_sha256 = body_digest.finish().expect("body digest");
    let mut receipt = ExportCompletionReceipt {
        versions: SCOPE_AUTHORIZED_COMPONENT_VERSIONS,
        session_id: offer.session_id.clone(),
        export_attempt_id: offer.export_attempt_id.clone(),
        source_generations,
        inventory_sha256,
        writable_metadata_sha256,
        delivered_control_entry_count: 1,
        delivered_file_count: 1,
        delivered_directory_count: 1,
        delivered_archive_entry_count: 3,
        delivered_content_bytes: body.len() as u64,
        delivered_body_sha256,
        completed_at: "2026-07-23T20:00:03Z".to_string(),
    };
    if matches!(mutation, V2ArchiveMutation::ReceiptBodyDigestMismatch) {
        receipt.delivered_body_sha256 = format!("sha256:{}", "0".repeat(64));
    }
    if matches!(mutation, V2ArchiveMutation::ReceiptGenerationMismatch) {
        receipt.source_generations[0].source_generation_id =
            SourceGenerationId::new("generation-other").expect("generation ID");
    }
    if matches!(mutation, V2ArchiveMutation::ReceiptCountMismatch) {
        receipt.delivered_file_count = 2;
        receipt.delivered_archive_entry_count = 4;
    }

    let mut builder = Builder::new(Vec::new());
    append_test_member(&mut builder, &TestMember::directory("docs/"));
    if matches!(mutation, V2ArchiveMutation::LocDirectoryHeader) {
        append_test_member(&mut builder, &TestMember::directory(".loc/"));
    }
    let mut pax = vec![
        (PAX_SOURCE_CONNECTION_ID, source_connection_id.as_str()),
        (PAX_PROJECTION_ID, projection_id.as_str()),
        (PAX_WINNING_SCOPE_ORDINAL, "0"),
        (PAX_FILE_KIND, "markdown"),
        (PAX_EFFECTIVE_ACTIONS, "[\"read\",\"search\"]"),
        (PAX_CONTENT_SHA256, content_sha256.as_str()),
    ];
    match mutation {
        V2ArchiveMutation::MissingPax => {
            pax.retain(|(key, _)| *key != PAX_PROJECTION_ID);
        }
        V2ArchiveMutation::DuplicatePax => pax.push((PAX_PROJECTION_ID, "projection-other")),
        V2ArchiveMutation::UnknownPax => pax.push(("locality.unknown", "forbidden")),
        _ => {}
    }
    builder
        .append_pax_extensions(pax.iter().map(|(key, value)| (*key, value.as_bytes())))
        .expect("append file PAX metadata");
    append_test_member(&mut builder, &TestMember::file("docs/readme.md", body));
    let terminal_control = ExportTerminalControlV2 {
        writable_metadata,
        completion_receipt: receipt,
    };
    let mut receipt_body =
        serde_json::to_vec(&terminal_control).expect("serialize terminal control");
    match mutation {
        V2ArchiveMutation::MalformedReceipt => receipt_body = b"not-json".to_vec(),
        V2ArchiveMutation::NoncanonicalReceipt => receipt_body.insert(0, b' '),
        V2ArchiveMutation::TrailingReceipt => receipt_body.push(b'\n'),
        V2ArchiveMutation::UnknownReceiptField => {
            receipt_body.splice(1..1, b"\"unknown\":true,".iter().copied());
        }
        _ => {}
    }
    let receipt_member = TestMember::file(
        locality_protocol::RESERVED_EXPORT_METADATA_PATH,
        receipt_body,
    );
    if matches!(mutation, V2ArchiveMutation::ReceiptNotFinal) {
        append_test_member(&mut builder, &receipt_member);
        append_test_member(&mut builder, &TestMember::file("after.txt", "after"));
    } else if !matches!(mutation, V2ArchiveMutation::MissingReceipt) {
        append_test_member(&mut builder, &receipt_member);
        if matches!(mutation, V2ArchiveMutation::DuplicateReceipt) {
            append_test_member(&mut builder, &receipt_member);
        }
    }
    builder.finish().expect("finish v2 tar");
    (offer, builder.into_inner().expect("collect v2 tar"))
}

fn append_test_member(builder: &mut Builder<Vec<u8>>, member: &TestMember) {
    let mut header = Header::new_gnu();
    header.set_entry_type(member.entry_type);
    header.set_mode(member.mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(member.data.len() as u64);
    assert!(member.path.len() <= 100, "test path fits GNU name field");
    {
        let bytes = header.as_mut_bytes();
        bytes[..100].fill(0);
        bytes[..member.path.len()].copy_from_slice(&member.path);
    }
    header.set_cksum();
    builder
        .append(&header, member.data.as_slice())
        .expect("append test member");
}

fn materialize_v2(
    archive: Vec<u8>,
    encoding: ReplicaArchiveEncoding,
    offer: &SealedExportOffer,
    destination: &Path,
) -> Result<ReplicaMaterializationSummary, String> {
    materialize_scope_authorized_replica_archive(
        ReplicaArchive::new(encoding, Cursor::new(archive)),
        destination,
        ReplicaMaterializationLimits::default(),
        offer,
    )
    .map_err(|error| error.to_string())
}

#[test]
fn scope_authorized_identity_and_zstd_publish_without_exposing_receipt() {
    for (label, encoding) in [
        ("v2-identity", ReplicaArchiveEncoding::Identity),
        ("v2-zstd", ReplicaArchiveEncoding::Zstd),
    ] {
        let root = TestDirectory::new(label);
        let (mut offer, tar) = v2_offer_and_archive(V2ArchiveMutation::None);
        let archive = match encoding {
            ReplicaArchiveEncoding::Identity => tar,
            ReplicaArchiveEncoding::Zstd => {
                offer.content_encoding = TarContentEncoding::Zstd;
                zstd::stream::encode_all(tar.as_slice(), 1).expect("compress v2 tar")
            }
        };
        let summary = materialize_v2(archive, encoding, &offer, &root.destination())
            .expect("materialize v2 archive");
        assert_eq!(summary.entries, 3);
        assert_eq!(summary.files, 1);
        assert_eq!(summary.directories, 1);
        assert_eq!(summary.materialized_bytes, 17);
        assert_eq!(
            fs::read(root.destination().join("docs/readme.md")).expect("read v2 file"),
            b"scope authorized\n"
        );
        assert!(!root.destination().join(".loc").exists());
    }
}

#[test]
fn scope_authorized_malformed_metadata_receipts_and_order_roll_back() {
    for (label, mutation, expected) in [
        (
            "v2-missing-pax",
            V2ArchiveMutation::MissingPax,
            "invalid locality PAX metadata",
        ),
        (
            "v2-duplicate-pax",
            V2ArchiveMutation::DuplicatePax,
            "invalid locality PAX metadata",
        ),
        (
            "v2-unknown-pax",
            V2ArchiveMutation::UnknownPax,
            "invalid locality PAX metadata",
        ),
        (
            "v2-loc-directory-header",
            V2ArchiveMutation::LocDirectoryHeader,
            "reserved .loc directory header is forbidden",
        ),
        (
            "v2-missing-receipt",
            V2ArchiveMutation::MissingReceipt,
            "completion receipt is missing",
        ),
        (
            "v2-malformed-receipt",
            V2ArchiveMutation::MalformedReceipt,
            "completion receipt is malformed",
        ),
        (
            "v2-noncanonical-receipt",
            V2ArchiveMutation::NoncanonicalReceipt,
            "completion receipt is not canonical JSON",
        ),
        (
            "v2-trailing-receipt",
            V2ArchiveMutation::TrailingReceipt,
            "completion receipt is not canonical JSON",
        ),
        (
            "v2-unknown-receipt-field",
            V2ArchiveMutation::UnknownReceiptField,
            "completion receipt is not canonical JSON",
        ),
        (
            "v2-duplicate-receipt",
            V2ArchiveMutation::DuplicateReceipt,
            "completion receipt is not the final member",
        ),
        (
            "v2-receipt-not-final",
            V2ArchiveMutation::ReceiptNotFinal,
            "completion receipt is not the final member",
        ),
        (
            "v2-body-digest-mismatch",
            V2ArchiveMutation::ReceiptBodyDigestMismatch,
            "delivered-body digest does not match",
        ),
        (
            "v2-generation-mismatch",
            V2ArchiveMutation::ReceiptGenerationMismatch,
            "completion receipt does not match sealed export offer",
        ),
        (
            "v2-count-mismatch",
            V2ArchiveMutation::ReceiptCountMismatch,
            "completion receipt does not match sealed export offer",
        ),
    ] {
        let root = TestDirectory::new(label);
        let (offer, archive) = v2_offer_and_archive(mutation);
        let error = materialize_v2(
            archive,
            ReplicaArchiveEncoding::Identity,
            &offer,
            &root.destination(),
        )
        .expect_err("malformed v2 archive must fail");
        assert!(error.contains(expected), "{label}: {error}");
        root.assert_no_staging_or_destination();
    }
}

#[test]
fn scope_authorized_truncation_rolls_back() {
    let root = TestDirectory::new("v2-truncated");
    let (offer, mut archive) = v2_offer_and_archive(V2ArchiveMutation::None);
    archive.truncate(archive.len() - 700);
    materialize_v2(
        archive,
        ReplicaArchiveEncoding::Identity,
        &offer,
        &root.destination(),
    )
    .expect_err("truncated v2 archive must fail");
    root.assert_no_staging_or_destination();
}

struct ChunkedReader<R> {
    inner: R,
    chunk_size: usize,
}

impl<R: Read> Read for ChunkedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let allowed = buffer.len().min(self.chunk_size);
        self.inner.read(&mut buffer[..allowed])
    }
}

#[cfg(unix)]
struct ComponentSwapReader {
    inner: Cursor<Vec<u8>>,
    staging_parent: PathBuf,
    outside: PathBuf,
    swapped: Arc<AtomicBool>,
}

#[cfg(unix)]
impl Read for ComponentSwapReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        const FIRST_DIRECTORY_HEADER_END: u64 = 512;
        if self.inner.position() == FIRST_DIRECTORY_HEADER_END
            && !self.swapped.swap(true, Ordering::SeqCst)
        {
            let staging = fs::read_dir(&self.staging_parent)?
                .filter_map(Result::ok)
                .find(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".locality-stage-")
                })
                .expect("private staging directory exists before archive extraction");
            let component = staging.path().join("legitimate");
            assert!(component.is_dir(), "first tar entry created its directory");
            fs::remove_dir(&component)?;
            std::os::unix::fs::symlink(&self.outside, component)?;
        }

        let remaining_before_swap =
            FIRST_DIRECTORY_HEADER_END.saturating_sub(self.inner.position());
        let allowed = if remaining_before_swap == 0 {
            buffer.len()
        } else {
            buffer.len().min(remaining_before_swap as usize)
        };
        self.inner.read(&mut buffer[..allowed])
    }
}

#[cfg(unix)]
struct RootSwapReader {
    inner: Cursor<Vec<u8>>,
    staging_parent: PathBuf,
    outside: PathBuf,
    swapped: Arc<AtomicBool>,
}

#[cfg(unix)]
impl Read for RootSwapReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if read == 0 && !self.swapped.swap(true, Ordering::SeqCst) {
            let staging = fs::read_dir(&self.staging_parent)?
                .filter_map(Result::ok)
                .find(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".locality-stage-")
                })
                .expect("private staging directory exists before publication");
            let detached = self.staging_parent.join("attacker-detached-staging");
            fs::rename(staging.path(), &detached)?;
            std::os::unix::fs::symlink(&self.outside, staging.path())?;
        }
        Ok(read)
    }
}

#[cfg(unix)]
#[test]
fn rejects_component_replaced_by_symlink_without_writing_or_chmodding_outside() {
    use std::os::unix::fs::PermissionsExt;

    let root = TestDirectory::new("component-swap");
    let outside = TestDirectory::new("component-swap-outside");
    fs::write(outside.0.join("sentinel.txt"), b"outside\n").expect("write outside sentinel");
    fs::set_permissions(&outside.0, fs::Permissions::from_mode(0o711))
        .expect("set distinctive outside mode");
    let swapped = Arc::new(AtomicBool::new(false));
    let archive = tar_archive(&[
        TestMember::directory("legitimate/"),
        TestMember::file("legitimate/escaped.txt", "must stay contained\n"),
    ]);
    let reader = ComponentSwapReader {
        inner: Cursor::new(archive),
        staging_parent: root.0.clone(),
        outside: outside.0.clone(),
        swapped: Arc::clone(&swapped),
    };

    let error = materialize_replica_archive(
        ReplicaArchive::new(ReplicaArchiveEncoding::Identity, reader),
        &root.destination(),
        ReplicaMaterializationLimits::default(),
    )
    .expect_err("a symlink substituted for an opened component must be rejected");

    assert!(swapped.load(Ordering::SeqCst), "test performed the swap");
    assert!(
        error.to_string().contains("legitimate/escaped.txt"),
        "rejection identifies the affected logical path: {error}"
    );
    root.assert_no_staging_or_destination();
    assert_eq!(
        fs::read(outside.0.join("sentinel.txt")).expect("read untouched sentinel"),
        b"outside\n"
    );
    assert!(!outside.0.join("escaped.txt").exists());
    assert_modes(&outside.0, 0o711);
}

#[cfg(unix)]
#[test]
fn rejects_staging_root_replaced_by_symlink_before_publication() {
    use std::os::unix::fs::PermissionsExt;

    let root = TestDirectory::new("root-swap");
    let outside = TestDirectory::new("root-swap-outside");
    fs::write(outside.0.join("sentinel.txt"), b"outside\n").expect("write outside sentinel");
    fs::set_permissions(&outside.0, fs::Permissions::from_mode(0o711))
        .expect("set distinctive outside mode");
    let swapped = Arc::new(AtomicBool::new(false));
    let reader = RootSwapReader {
        inner: Cursor::new(tar_archive(&[TestMember::file(
            "payload.txt",
            "must stay in the held staging root\n",
        )])),
        staging_parent: root.0.clone(),
        outside: outside.0.clone(),
        swapped: Arc::clone(&swapped),
    };

    let error = materialize_replica_archive(
        ReplicaArchive::new(ReplicaArchiveEncoding::Identity, reader),
        &root.destination(),
        ReplicaMaterializationLimits::default(),
    )
    .expect_err("a symlink substituted for the staging root must be rejected");

    assert!(swapped.load(Ordering::SeqCst), "test performed the swap");
    assert!(
        error
            .to_string()
            .contains("staging root identity changed before publication"),
        "rejection identifies the root publication race: {error}"
    );
    root.assert_no_staging_or_destination();
    assert_eq!(
        fs::read(outside.0.join("sentinel.txt")).expect("read untouched sentinel"),
        b"outside\n"
    );
    assert!(!outside.0.join("payload.txt").exists());
    assert_modes(&outside.0, 0o711);
}

#[test]
fn single_frame_zstd_stream_materializes_from_small_chunks() {
    let root = TestDirectory::new("zstd");
    let tar = tar_archive(&[TestMember::file("answer.txt", "42\n")]);
    let compressed = zstd::stream::encode_all(tar.as_slice(), 1).expect("encode zstd fixture");
    let input = ChunkedReader {
        inner: Cursor::new(compressed),
        chunk_size: 7,
    };

    let summary = materialize_replica_archive(
        ReplicaArchive::new(ReplicaArchiveEncoding::Zstd, input),
        &root.destination(),
        ReplicaMaterializationLimits::default(),
    )
    .expect("materialize streaming zstd tar");

    assert_eq!(summary.files, 1);
    assert_eq!(summary.materialized_bytes, 3);
    assert_eq!(summary.decoded_bytes, tar.len() as u64);
    assert_eq!(
        fs::read(root.destination().join("answer.txt")).expect("read zstd result"),
        b"42\n"
    );
}

#[test]
fn exact_receipt_zstd_archive_hashes_the_decoded_tar_bytes() {
    let root = TestDirectory::new("zstd-receipt");
    let tar = tar_archive(&[TestMember::file("verified.txt", "zstd\n")]);
    let expected = expected_receipt(&tar, 1);
    let compressed = zstd::stream::encode_all(tar.as_slice(), 1).expect("encode zstd fixture");

    let summary = materialize_replica_archive_with_expected_receipt(
        ReplicaArchive::new(ReplicaArchiveEncoding::Zstd, Cursor::new(compressed)),
        &root.destination(),
        ReplicaMaterializationLimits::default(),
        expected,
    )
    .expect("materialize Zstd tar with decoded receipt");

    assert_eq!(summary.entries, 1);
    assert_eq!(summary.decoded_bytes, tar.len() as u64);
    assert_eq!(
        fs::read(root.destination().join("verified.txt")).expect("read verified file"),
        b"zstd\n"
    );
}

#[test]
fn exact_receipt_mismatches_each_roll_back_staging_and_destination() {
    let tar = tar_archive(&[TestMember::file("never-published.txt", "body")]);
    let actual_digest: [u8; 32] = Sha256::digest(&tar).into();
    let cases = [
        (
            "digest-mismatch",
            ExpectedReplicaMaterializationReceipt {
                decoded_tar_sha256: [0_u8; 32],
                decoded_bytes: tar.len() as u64,
                entries: 1,
            },
            format!(
                "replica decoded tar digest mismatch: expected {}, actual {}",
                sha256_label([0_u8; 32]),
                sha256_label(actual_digest)
            ),
        ),
        (
            "decoded-byte-mismatch",
            ExpectedReplicaMaterializationReceipt {
                decoded_tar_sha256: actual_digest,
                decoded_bytes: tar.len() as u64 + 1,
                entries: 1,
            },
            format!(
                "replica decoded-byte receipt mismatch: expected {}, actual {}",
                tar.len() + 1,
                tar.len()
            ),
        ),
        (
            "entry-count-mismatch",
            ExpectedReplicaMaterializationReceipt {
                decoded_tar_sha256: actual_digest,
                decoded_bytes: tar.len() as u64,
                entries: 2,
            },
            "replica entry-count receipt mismatch: expected 2, actual 1".to_string(),
        ),
    ];

    for (label, expected, expected_error) in cases {
        let root = TestDirectory::new(label);
        let error = materialize_replica_archive_with_expected_receipt(
            ReplicaArchive::new(ReplicaArchiveEncoding::Identity, Cursor::new(tar.clone())),
            &root.destination(),
            ReplicaMaterializationLimits::default(),
            expected,
        )
        .expect_err("receipt mismatch must fail before publish")
        .to_string();

        assert_eq!(error, expected_error, "case {label}");
        root.assert_no_staging_or_destination();
    }
}

#[test]
fn zstd_receipt_digest_mismatch_rolls_back_staging_and_destination() {
    let root = TestDirectory::new("zstd-digest-mismatch");
    let tar = tar_archive(&[TestMember::file("never-published.txt", "body")]);
    let compressed = zstd::stream::encode_all(tar.as_slice(), 1).expect("encode zstd fixture");
    let actual_digest: [u8; 32] = Sha256::digest(&tar).into();
    let expected = ExpectedReplicaMaterializationReceipt {
        decoded_tar_sha256: [0_u8; 32],
        decoded_bytes: tar.len() as u64,
        entries: 1,
    };

    let error = materialize_replica_archive_with_expected_receipt(
        ReplicaArchive::new(ReplicaArchiveEncoding::Zstd, Cursor::new(compressed)),
        &root.destination(),
        ReplicaMaterializationLimits::default(),
        expected,
    )
    .expect_err("decoded Zstd digest mismatch must fail before publish")
    .to_string();

    assert_eq!(
        error,
        format!(
            "replica decoded tar digest mismatch: expected {}, actual {}",
            sha256_label([0_u8; 32]),
            sha256_label(actual_digest)
        )
    );
    root.assert_no_staging_or_destination();
}

#[test]
fn rejects_multiple_zstd_frames_and_rolls_back() {
    let root = TestDirectory::new("zstd-multiple");
    let tar = tar_archive(&[TestMember::file("first.txt", "first")]);
    let mut compressed = zstd::stream::encode_all(tar.as_slice(), 1).expect("encode first frame");
    compressed.extend(zstd::stream::encode_all(tar.as_slice(), 1).expect("encode second frame"));

    let error = materialize_replica_archive(
        ReplicaArchive::new(ReplicaArchiveEncoding::Zstd, Cursor::new(compressed)),
        &root.destination(),
        ReplicaMaterializationLimits::default(),
    )
    .expect_err("multiple frames must fail")
    .to_string();

    assert_eq!(
        error,
        "invalid Zstd replica stream: multiple frames or trailing data"
    );
    root.assert_no_staging_or_destination();
}

#[test]
fn rejects_truncated_zstd_and_rolls_back() {
    let root = TestDirectory::new("zstd-truncated");
    let tar = tar_archive(&[TestMember::file("first.txt", "first")]);
    let mut compressed = zstd::stream::encode_all(tar.as_slice(), 1).expect("encode frame");
    compressed.truncate(compressed.len() - 1);

    let error = materialize_replica_archive(
        ReplicaArchive::new(ReplicaArchiveEncoding::Zstd, Cursor::new(compressed)),
        &root.destination(),
        ReplicaMaterializationLimits::default(),
    )
    .expect_err("truncated frame must fail")
    .to_string();

    assert_eq!(error, "invalid replica tar stream: incomplete frame");
    root.assert_no_staging_or_destination();
}

#[test]
fn existing_destination_is_never_replaced() {
    let root = TestDirectory::new("destination-exists");
    fs::create_dir(root.destination()).expect("create existing destination");
    fs::write(root.destination().join("sentinel"), b"original").expect("write sentinel");
    let error = materialize_identity(
        tar_archive(&[TestMember::file("replacement", "new")]),
        &root.destination(),
        ReplicaMaterializationLimits::default(),
    )
    .expect_err("existing destination must be rejected");

    assert_eq!(
        error,
        format!(
            "replica destination already exists: {}",
            root.destination().display()
        )
    );
    assert_eq!(
        fs::read(root.destination().join("sentinel")).expect("read unchanged sentinel"),
        b"original"
    );
    assert!(!root.destination().join("replacement").exists());
}

#[test]
fn rejects_unsafe_and_non_utf8_paths_exactly() {
    let cases = [
        (
            "traversal",
            b"../escape.txt".as_slice(),
            "invalid replica path `../escape.txt`: logical path contains non-normalized component `..`",
        ),
        (
            "absolute",
            b"/etc/passwd".as_slice(),
            "invalid replica path `/etc/passwd`: logical path must be relative",
        ),
        (
            "backslash",
            b"docs\\escape.txt".as_slice(),
            "invalid replica path `docs\\escape.txt`: logical path must use forward slashes",
        ),
        (
            "windows-prefix",
            b"C:/escape.txt".as_slice(),
            "invalid replica path `C:/escape.txt`: logical path contains a Windows prefix",
        ),
        (
            "reserved-metadata",
            b".loc/session.json".as_slice(),
            "invalid replica path `.loc/session.json`: logical path is reserved for export metadata: .loc/session.json",
        ),
        (
            "non-utf8",
            b"bad-\xff.txt".as_slice(),
            "replica tar entry path is not valid UTF-8",
        ),
    ];

    for (label, path, expected) in cases {
        let error = rejected_identity(
            label,
            tar_archive(&[TestMember::file(path, "body")]),
            ReplicaMaterializationLimits::default(),
        );
        assert_eq!(error, expected, "case {label}");
    }
}

#[test]
fn rejects_links_devices_and_writable_modes_exactly() {
    let hostile_types = [
        ("symlink", EntryType::symlink()),
        ("hardlink", EntryType::hard_link()),
        ("character-device", EntryType::character_special()),
        ("block-device", EntryType::block_special()),
        ("fifo", EntryType::fifo()),
    ];
    for (label, entry_type) in hostile_types {
        let error = rejected_identity(
            label,
            tar_archive(&[TestMember {
                path: b"hostile".to_vec(),
                entry_type,
                mode: 0o555,
                data: Vec::new(),
                link_name: None,
            }]),
            ReplicaMaterializationLimits::default(),
        );
        assert_eq!(
            error, "replica entry `hostile` is not a regular file or directory",
            "case {label}"
        );
    }

    let writable_file = rejected_identity(
        "writable-file",
        tar_archive(&[TestMember {
            mode: 0o644,
            ..TestMember::file("writable.txt", "body")
        }]),
        ReplicaMaterializationLimits::default(),
    );
    assert_eq!(
        writable_file,
        "replica file `writable.txt` has mode 0644; expected 0444"
    );

    let writable_directory = rejected_identity(
        "writable-directory",
        tar_archive(&[TestMember {
            mode: 0o755,
            ..TestMember::directory("writable/")
        }]),
        ReplicaMaterializationLimits::default(),
    );
    assert_eq!(
        writable_directory,
        "replica directory `writable` has mode 0755; expected 0555"
    );

    let link_metadata = rejected_identity(
        "link-metadata",
        tar_archive(&[TestMember {
            link_name: Some(b"target".to_vec()),
            ..TestMember::file("regular.txt", "body")
        }]),
        ReplicaMaterializationLimits::default(),
    );
    assert_eq!(
        link_metadata,
        "replica entry `regular.txt` contains link metadata"
    );
}

#[test]
fn rejects_duplicate_case_unicode_and_file_directory_collisions_exactly() {
    let duplicate = rejected_identity(
        "duplicate",
        tar_archive(&[
            TestMember::file("same.txt", "one"),
            TestMember::file("same.txt", "two"),
        ]),
        ReplicaMaterializationLimits::default(),
    );
    assert_eq!(duplicate, "replica path is duplicated: `same.txt`");

    let case = rejected_identity(
        "case",
        tar_archive(&[
            TestMember::file("Docs/one.txt", "one"),
            TestMember::file("docs/two.txt", "two"),
        ]),
        ReplicaMaterializationLimits::default(),
    );
    assert_eq!(case, "replica paths collide by case: `Docs` and `docs`");

    let unicode = rejected_identity(
        "unicode",
        tar_archive(&[
            TestMember::file("caf\u{e9}.md", "one"),
            TestMember::file("cafe\u{301}.md", "two"),
        ]),
        ReplicaMaterializationLimits::default(),
    );
    assert_eq!(
        unicode,
        "replica paths collide after Unicode normalization: `caf\u{e9}.md` and `cafe\u{301}.md`"
    );

    let type_collision = rejected_identity(
        "path-type",
        tar_archive(&[
            TestMember::file("parent", "file"),
            TestMember::file("parent/child.txt", "child"),
        ]),
        ReplicaMaterializationLimits::default(),
    );
    assert_eq!(
        type_collision,
        "replica path is used as both a file and directory: `parent`"
    );
}

#[test]
fn rejects_malformed_truncated_and_trailing_tar_exactly() {
    let mut malformed = tar_archive(&[TestMember::file("file.txt", "body")]);
    malformed[0] ^= 1;
    let malformed_error = rejected_identity(
        "malformed",
        malformed,
        ReplicaMaterializationLimits::default(),
    );
    assert_eq!(
        malformed_error,
        "invalid replica tar stream: archive header checksum mismatch"
    );

    let mut truncated = tar_archive(&[TestMember::file("file.txt", "body")]);
    truncated.truncate(truncated.len() - 512);
    let truncated_error = rejected_identity(
        "truncated",
        truncated,
        ReplicaMaterializationLimits::default(),
    );
    assert_eq!(
        truncated_error,
        "invalid replica tar stream: missing two-block end marker"
    );

    let mut trailing = tar_archive(&[TestMember::file("file.txt", "body")]);
    trailing.extend_from_slice(b"trailing");
    let trailing_error = rejected_identity(
        "trailing",
        trailing,
        ReplicaMaterializationLimits::default(),
    );
    assert_eq!(
        trailing_error,
        "invalid replica tar stream: trailing data after end marker"
    );
}

#[test]
fn enforces_entry_file_decoded_and_disk_limits_exactly() {
    let archive = tar_archive(&[TestMember::file("large.txt", "12345")]);

    let entry_error = rejected_identity(
        "entry-limit",
        archive.clone(),
        ReplicaMaterializationLimits {
            max_entries: 0,
            ..ReplicaMaterializationLimits::default()
        },
    );
    assert_eq!(entry_error, "replica entry limit exceeded: 0");

    let file_error = rejected_identity(
        "file-limit",
        archive.clone(),
        ReplicaMaterializationLimits {
            max_file_bytes: 4,
            ..ReplicaMaterializationLimits::default()
        },
    );
    assert_eq!(
        file_error,
        "replica file `large.txt` is 5 bytes, exceeding limit 4"
    );

    let disk_error = rejected_identity(
        "disk-limit",
        archive.clone(),
        ReplicaMaterializationLimits {
            max_disk_bytes: 4,
            ..ReplicaMaterializationLimits::default()
        },
    );
    assert_eq!(
        disk_error,
        "replica materialized bytes 5 exceed disk limit 4"
    );

    let decoded_error = rejected_identity(
        "decoded-limit",
        archive.clone(),
        ReplicaMaterializationLimits {
            max_decoded_bytes: archive.len() as u64 - 1,
            ..ReplicaMaterializationLimits::default()
        },
    );
    assert_eq!(
        decoded_error,
        format!("replica decoded-byte limit exceeded: {}", archive.len() - 1)
    );
}

#[test]
fn materializes_ten_thousand_files_with_a_constant_size_summary() {
    let root = TestDirectory::new("ten-thousand");
    let members = (0..10_000)
        .map(|index| TestMember::file(format!("files/{index:05}.txt"), b"x"))
        .collect::<Vec<_>>();
    let archive = tar_archive(&members);

    let summary = materialize_identity(
        archive,
        &root.destination(),
        ReplicaMaterializationLimits::default(),
    )
    .expect("materialize ten thousand files");

    assert_eq!(summary.entries, 10_000);
    assert_eq!(summary.files, 10_000);
    assert_eq!(summary.directories, 1);
    assert_eq!(summary.materialized_bytes, 10_000);
    assert_eq!(
        std::mem::size_of::<ReplicaMaterializationSummary>(),
        5 * std::mem::size_of::<u64>()
    );
    assert_eq!(
        fs::read(root.destination().join("files/09999.txt")).expect("read last file"),
        b"x"
    );
    assert!(!root.destination().join(".loc").exists());
}

#[cfg(unix)]
fn assert_modes(path: &Path, expected: u32) {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(path).expect("read mode").permissions().mode() & 0o7777,
        expected,
        "mode for {}",
        path.display()
    );
}

#[cfg(not(unix))]
fn assert_modes(path: &Path, _expected: u32) {
    assert!(
        fs::metadata(path)
            .expect("read permissions")
            .permissions()
            .readonly(),
        "{} should be read-only",
        path.display()
    );
}

fn make_removable(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        make_directory_writable(path);
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                make_removable(&entry.path());
            }
        }
    } else {
        make_file_writable(path);
    }
}

#[cfg(unix)]
fn make_directory_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn make_directory_writable(path: &Path) {
    let Ok(mut permissions) = fs::metadata(path).map(|metadata| metadata.permissions()) else {
        return;
    };
    permissions.set_readonly(false);
    let _ = fs::set_permissions(path, permissions);
}

#[cfg(unix)]
fn make_file_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn make_file_writable(path: &Path) {
    make_directory_writable(path);
}
