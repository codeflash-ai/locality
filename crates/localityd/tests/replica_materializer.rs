use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};

use localityd::remote_truth::{ReplicaArchive, ReplicaArchiveEncoding};
use localityd::replica_materializer::{
    ExpectedReplicaMaterializationReceipt, ReplicaMaterializationLimits,
    ReplicaMaterializationSummary, materialize_replica_archive,
    materialize_replica_archive_with_expected_receipt,
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
