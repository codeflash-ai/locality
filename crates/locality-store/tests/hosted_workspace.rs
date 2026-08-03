use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_core::model::MountId;
use locality_core::workspace_layout::{MountTarget, PortableMountId};
use locality_protocol::workspace_layout::{LayoutDigest, WorkspaceProfileId};
use locality_store::{
    CanonicalApiOrigin, HostedWorkspaceCredentialRef, HostedWorkspaceIdentity,
    HostedWorkspaceMountMapping, HostedWorkspaceRepository, InMemoryStateStore, MountConfig,
    MountRepository, PreparedHostedWorkspaceTransition, ProjectionMode, SqliteStateStore,
    StoreError, WorkspaceBinding, WorkspaceBindingRecord, WorkspaceBindingRepository,
    WorkspaceHostBinding, WorkspaceHostPlatform, WorkspaceId, WorkspaceProjectionIdentity,
};
use rusqlite::Connection;

const PROFILE_ID: &str = "018f4f6e-9f2c-7b1a-8c3d-4e5f60718293";
const OTHER_PROFILE_ID: &str = "018f4f6e-9f2c-7b1a-8c3d-4e5f60718294";
const DIGEST_A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const PROFILE_KEY_CANARY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn api_origin_is_canonical_and_profile_id_is_only_half_of_identity() {
    assert_eq!(
        CanonicalApiOrigin::new("HTTPS://API.Example.COM:443/")
            .unwrap()
            .as_str(),
        "https://api.example.com"
    );
    assert_eq!(
        CanonicalApiOrigin::new("http://127.0.0.1:8080")
            .unwrap()
            .as_str(),
        "http://127.0.0.1:8080"
    );
    for invalid in [
        "https://api.example.com/v2",
        "https://user@api.example.com",
        "https://api.example.com?q=1",
        "file:///tmp/api",
        " https://api.example.com",
    ] {
        assert!(CanonicalApiOrigin::new(invalid).is_err(), "{invalid}");
    }

    let profile = WorkspaceProfileId::new(PROFILE_ID).unwrap();
    assert_ne!(
        HostedWorkspaceIdentity::new(
            CanonicalApiOrigin::new("https://api.example.com").unwrap(),
            profile.clone(),
        ),
        HostedWorkspaceIdentity::new(
            CanonicalApiOrigin::new("https://api.other.example").unwrap(),
            profile,
        )
    );
}

#[test]
fn credential_reference_syntax_cannot_accept_a_profile_key() {
    assert!(HostedWorkspaceCredentialRef::new(PROFILE_KEY_CANARY).is_err());
    assert!(HostedWorkspaceCredentialRef::new("hosted-workspace:").is_err());
    assert!(HostedWorkspaceCredentialRef::new("hosted-workspace:key with space").is_err());
    assert_eq!(
        HostedWorkspaceCredentialRef::new("hosted-workspace:api-example:profile-1")
            .unwrap()
            .as_str(),
        "hosted-workspace:api-example:profile-1"
    );
}

#[test]
fn in_memory_repository_preserves_stable_mapping_and_atomic_profile_set() {
    assert_repository_lifecycle(
        InMemoryStateStore::new(),
        Path::new("/tmp/locality-hosted-memory"),
    );
}

#[test]
fn sqlite_repository_preserves_stable_mapping_and_atomic_profile_set_across_restart() {
    let fixture = Fixture::new("lifecycle");
    assert_repository_lifecycle(fixture.open(), &fixture.workspace_root);

    let reopened = fixture.open();
    let mappings = reopened
        .list_hosted_workspace_mount_mappings(&identity("https://api.example.com", PROFILE_ID))
        .unwrap();
    assert_eq!(mappings.len(), 3);
    assert_eq!(
        mappings
            .iter()
            .find(|mapping| mapping.portable_mount_id().as_str() == "mount-beta")
            .unwrap()
            .local_mount_id()
            .as_str(),
        "hosted-local-beta"
    );
}

fn assert_repository_lifecycle<S>(mut store: S, root: &Path)
where
    S: HostedWorkspaceRepository + MountRepository,
{
    store
        .save_mount(MountConfig::new(
            MountId::new("connector-notion"),
            "notion",
            root.with_file_name("connector-notion"),
        ))
        .unwrap();
    let identity = identity("https://api.example.com", PROFILE_ID);
    let first = transition(
        "transition-1",
        identity.clone(),
        root,
        1,
        DIGEST_A,
        &[
            ("mount-alpha", "hosted-local-alpha", "alpha"),
            ("mount-beta", "hosted-local-beta", "beta"),
        ],
    );
    let pending = store
        .begin_hosted_workspace_transition(first.clone())
        .unwrap();
    assert_eq!(pending.prepared(), &first);
    assert_eq!(
        store.begin_hosted_workspace_transition(first).unwrap(),
        pending,
        "exact transition replay is idempotent"
    );
    assert!(matches!(
        store.begin_hosted_workspace_transition(transition(
            "different-transition",
            identity.clone(),
            root,
            1,
            DIGEST_A,
            &[
                ("mount-alpha", "hosted-local-alpha", "alpha"),
                ("mount-beta", "hosted-local-beta", "beta"),
            ],
        )),
        Err(StoreError::InvalidState(message)) if message.contains("different pending")
    ));
    let attached = store
        .commit_hosted_workspace_transition("transition-1", "2026-08-03T00:00:01Z")
        .unwrap();
    assert_eq!(attached.profile_revision(), 1);
    assert_eq!(store.load_mounts().unwrap().len(), 1);
    assert_eq!(
        store.load_mounts().unwrap()[0].mount_id.as_str(),
        "connector-notion"
    );

    let second = transition(
        "transition-2",
        identity.clone(),
        root,
        2,
        DIGEST_B,
        &[
            ("mount-alpha", "hosted-local-alpha", "renamed-alpha"),
            ("mount-gamma", "hosted-local-gamma", "gamma"),
        ],
    );
    store.begin_hosted_workspace_transition(second).unwrap();
    store
        .commit_hosted_workspace_transition("transition-2", "2026-08-03T00:00:02Z")
        .unwrap();
    let mappings = store
        .list_hosted_workspace_mount_mappings(&identity)
        .unwrap();
    assert_mapping(&mappings, "mount-alpha", "hosted-local-alpha", true, 1, 2);
    assert_mapping(&mappings, "mount-beta", "hosted-local-beta", false, 1, 1);
    assert_mapping(&mappings, "mount-gamma", "hosted-local-gamma", true, 2, 2);

    let third = transition(
        "transition-3",
        identity.clone(),
        root,
        3,
        DIGEST_A,
        &[("mount-beta", "hosted-local-beta", "beta-again")],
    );
    store.begin_hosted_workspace_transition(third).unwrap();
    store
        .commit_hosted_workspace_transition("transition-3", "2026-08-03T00:00:03Z")
        .unwrap();
    let mappings = store
        .list_hosted_workspace_mount_mappings(&identity)
        .unwrap();
    assert_mapping(&mappings, "mount-beta", "hosted-local-beta", true, 1, 3);
    assert_mapping(&mappings, "mount-alpha", "hosted-local-alpha", false, 1, 2);

    let before = store.get_hosted_workspace_attachment(&identity).unwrap();
    assert!(matches!(
        store.begin_hosted_workspace_transition(transition(
            "replacement-local-id",
            identity.clone(),
            root,
            4,
            DIGEST_B,
            &[("mount-beta", "replacement-id", "beta")],
        )),
        Err(StoreError::InvalidState(message)) if message.contains("already mapped")
    ));
    assert_eq!(
        store.get_hosted_workspace_attachment(&identity).unwrap(),
        before
    );
    assert!(
        store
            .get_pending_hosted_workspace_transition(&identity)
            .unwrap()
            .is_none()
    );

    assert!(matches!(
        store.begin_hosted_workspace_transition(transition(
            "connector-id-collision",
            identity.clone(),
            root,
            4,
            DIGEST_B,
            &[("mount-delta", "connector-notion", "delta")],
        )),
        Err(StoreError::InvalidState(message)) if message.contains("reserved outside")
    ));
}

#[test]
fn same_profile_id_on_distinct_origins_has_distinct_attachment_and_mount_identity() {
    let mut store = InMemoryStateStore::new();
    let left = identity("https://api.one.example", PROFILE_ID);
    let right = identity("https://api.two.example", PROFILE_ID);
    for (transition_id, identity, root, local_id) in [
        ("left", left.clone(), "/tmp/hosted-left", "left-local"),
        ("right", right.clone(), "/tmp/hosted-right", "right-local"),
    ] {
        store
            .begin_hosted_workspace_transition(transition(
                transition_id,
                identity,
                Path::new(root),
                1,
                DIGEST_A,
                &[("portable", local_id, "docs")],
            ))
            .unwrap();
        store
            .commit_hosted_workspace_transition(transition_id, "2026-08-03T00:00:00Z")
            .unwrap();
    }
    assert_eq!(store.list_hosted_workspace_attachments().unwrap().len(), 2);
    assert_ne!(
        store.list_hosted_workspace_mount_mappings(&left).unwrap()[0].local_mount_id(),
        store.list_hosted_workspace_mount_mappings(&right).unwrap()[0].local_mount_id(),
    );
}

#[test]
fn pending_profiles_cannot_reserve_the_same_local_mount_identity() {
    assert_pending_profiles_cannot_share_local_id(InMemoryStateStore::new());
    let fixture = Fixture::new("pending-local-collision");
    assert_pending_profiles_cannot_share_local_id(fixture.open());
}

#[test]
fn connector_mount_cannot_claim_committed_or_pending_hosted_mount_identity() {
    for committed in [false, true] {
        let fixture = Fixture::new(if committed {
            "connector-committed-id-collision"
        } else {
            "connector-pending-id-collision"
        });
        let mut store = fixture.open();
        store
            .begin_hosted_workspace_transition(transition(
                "hosted-reservation",
                identity("https://api.example.com", PROFILE_ID),
                &fixture.workspace_root,
                1,
                DIGEST_A,
                &[("portable", "hosted-reserved", "docs")],
            ))
            .unwrap();
        if committed {
            store
                .commit_hosted_workspace_transition("hosted-reservation", "2026-08-03T00:00:00Z")
                .unwrap();
        }

        assert!(matches!(
            store.save_mount(MountConfig::new(
                MountId::new("hosted-reserved"),
                "notion",
                fixture.root.join("connector")
            )),
            Err(StoreError::InvalidState(message)) if message.contains("reserved by a hosted workspace")
        ));
        assert!(store.load_mounts().unwrap().is_empty());
    }
}

#[test]
fn connector_repair_commit_rechecks_pending_hosted_mount_identity() {
    let fixture = Fixture::new("connector-repair-id-collision");
    let mut store = fixture.open();
    store
        .begin_hosted_workspace_transition(transition(
            "hosted-repair-reservation",
            identity("https://api.example.com", PROFILE_ID),
            &fixture.workspace_root,
            1,
            DIGEST_A,
            &[("portable", "repair-local", "hosted-docs")],
        ))
        .unwrap();
    let trusted_root = fixture.root.join("ConnectorWorkspace");
    let mount_root = trusted_root.join("connector-docs");
    fs::create_dir_all(&mount_root).unwrap();
    let mount_id = MountId::new("repair-local");
    let workspace_id = WorkspaceId::new("locality.workspace.hosted-repair-test").unwrap();
    let host = WorkspaceHostBinding::new(
        WorkspaceHostPlatform::current(),
        workspace_id.clone(),
        &trusted_root,
        WorkspaceProjectionIdentity::new("linux-fuse:hosted-repair-test").unwrap(),
        1,
    )
    .unwrap();
    let record = WorkspaceBindingRecord::new(
        mount_id.clone(),
        WorkspaceBinding::for_workspace(workspace_id, MountTarget::new("connector-docs").unwrap()),
    );
    let mount = MountConfig::new(mount_id.clone(), "notion", mount_root)
        .projection(ProjectionMode::LinuxFuse);
    let connection = Connection::open(&store.db_path).unwrap();
    connection
        .execute(
            "INSERT INTO mounts (
                mount_id, connector, root, remote_root_id, read_only,
                projection_json, connection_id, settings_json
             ) VALUES (?1, 'notion', ?2, NULL, 0, ?3, NULL, '{}')",
            rusqlite::params![
                mount_id.as_str(),
                mount.root.display().to_string(),
                serde_json::to_string(&ProjectionMode::LinuxFuse).unwrap(),
            ],
        )
        .unwrap();
    drop(connection);
    store
        .begin_workspace_remount_recovery("repair-attempt", &mount_id)
        .unwrap();
    let mut cleanup_called = false;
    let mut cleanup = || {
        cleanup_called = true;
        Ok(())
    };

    assert!(matches!(
        store.save_mount_with_workspace_binding_and_cleanup(
            mount,
            host,
            record,
            &mut cleanup
        ),
        Err(StoreError::InvalidState(message)) if message.contains("reserved by a hosted workspace")
    ));
    assert!(!cleanup_called);
    assert!(store.get_mount(&mount_id).unwrap().is_some());
}

#[test]
fn sqlite_commit_rechecks_mount_id_after_prepare_race() {
    let fixture = Fixture::new("commit-id-race");
    let identity = identity("https://api.example.com", PROFILE_ID);
    let mut store = fixture.open();
    store
        .begin_hosted_workspace_transition(transition(
            "raced-transition",
            identity.clone(),
            &fixture.workspace_root,
            1,
            DIGEST_A,
            &[("portable", "raced-local-id", "docs")],
        ))
        .unwrap();

    let connection = Connection::open(&store.db_path).unwrap();
    connection
        .execute(
            "INSERT INTO mounts (
                mount_id, connector, root, remote_root_id, read_only,
                projection_json, connection_id, settings_json
             ) VALUES (?1, 'notion', ?2, NULL, 0, ?3, NULL, '{}')",
            rusqlite::params![
                "raced-local-id",
                fixture.root.join("connector").display().to_string(),
                serde_json::to_string(&locality_store::ProjectionMode::PlainFiles).unwrap(),
            ],
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        store.commit_hosted_workspace_transition(
            "raced-transition",
            "2026-08-03T00:00:01Z"
        ),
        Err(StoreError::InvalidState(message)) if message.contains("became reserved outside")
    ));
    assert!(
        store
            .get_hosted_workspace_attachment(&identity)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_pending_hosted_workspace_transition(&identity)
            .unwrap()
            .is_some()
    );
}

fn assert_pending_profiles_cannot_share_local_id<S: HostedWorkspaceRepository>(mut store: S) {
    store
        .begin_hosted_workspace_transition(transition(
            "first-pending",
            identity("https://api.one.example", PROFILE_ID),
            Path::new("/tmp/first-pending"),
            1,
            DIGEST_A,
            &[("portable", "shared-local", "first")],
        ))
        .unwrap();
    assert!(matches!(
        store.begin_hosted_workspace_transition(transition(
            "second-pending",
            identity("https://api.two.example", PROFILE_ID),
            Path::new("/tmp/second-pending"),
            1,
            DIGEST_A,
            &[("portable", "shared-local", "second")],
        )),
        Err(StoreError::InvalidState(message)) if message.contains("reserved outside")
    ));
}

#[test]
fn pending_transition_survives_restart_and_cancel_is_explicit() {
    let fixture = Fixture::new("pending-restart");
    let identity = identity("https://api.example.com", PROFILE_ID);
    let mut store = fixture.open();
    store
        .begin_hosted_workspace_transition(transition(
            "pending-restart",
            identity.clone(),
            &fixture.workspace_root,
            1,
            DIGEST_A,
            &[("mount-alpha", "local-alpha", "alpha")],
        ))
        .unwrap();
    drop(store);

    let mut reopened = fixture.open();
    assert_eq!(
        reopened
            .get_pending_hosted_workspace_transition(&identity)
            .unwrap()
            .unwrap()
            .prepared()
            .transition_id(),
        "pending-restart"
    );
    reopened
        .cancel_hosted_workspace_transition("pending-restart")
        .unwrap();
    assert!(
        reopened
            .get_pending_hosted_workspace_transition(&identity)
            .unwrap()
            .is_none()
    );
}

#[test]
fn sqlite_commit_rolls_back_attachment_and_all_mount_changes_on_mid_commit_failure() {
    let fixture = Fixture::new("atomic-rollback");
    let identity = identity("https://api.example.com", PROFILE_ID);
    let mut store = fixture.open();
    store
        .begin_hosted_workspace_transition(transition(
            "initial",
            identity.clone(),
            &fixture.workspace_root,
            1,
            DIGEST_A,
            &[("mount-alpha", "local-alpha", "alpha")],
        ))
        .unwrap();
    store
        .commit_hosted_workspace_transition("initial", "2026-08-03T00:00:00Z")
        .unwrap();
    store
        .begin_hosted_workspace_transition(transition(
            "failing",
            identity.clone(),
            &fixture.workspace_root,
            2,
            DIGEST_B,
            &[
                ("mount-alpha", "local-alpha", "alpha-new"),
                ("mount-beta", "local-beta", "beta"),
            ],
        ))
        .unwrap();
    let connection = Connection::open(&store.db_path).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_second_hosted_mount
             BEFORE INSERT ON hosted_workspace_mount_mappings
             WHEN NEW.portable_mount_id = 'mount-beta'
             BEGIN SELECT RAISE(ABORT, 'injected hosted mount failure'); END;",
        )
        .unwrap();

    assert!(
        store
            .commit_hosted_workspace_transition("failing", "2026-08-03T00:00:01Z")
            .is_err()
    );
    assert_eq!(
        store
            .get_hosted_workspace_attachment(&identity)
            .unwrap()
            .unwrap()
            .profile_revision(),
        1
    );
    let mappings = store
        .list_hosted_workspace_mount_mappings(&identity)
        .unwrap();
    assert_eq!(mappings.len(), 1);
    assert_mapping(&mappings, "mount-alpha", "local-alpha", true, 1, 1);
    assert!(
        store
            .get_pending_hosted_workspace_transition(&identity)
            .unwrap()
            .is_some()
    );
}

#[test]
fn sqlite_contains_only_credential_reference_and_never_profile_key_canary() {
    let fixture = Fixture::new("credential-boundary");
    let mut store = fixture.open();
    let identity = identity("https://api.example.com", OTHER_PROFILE_ID);
    store
        .begin_hosted_workspace_transition(transition(
            "credential-boundary",
            identity,
            &fixture.workspace_root,
            1,
            DIGEST_A,
            &[("mount", "local", "docs")],
        ))
        .unwrap();
    drop(store);
    let bytes = fs::read(fixture.state_root.join("state.sqlite3")).unwrap();
    assert!(
        !bytes
            .windows(PROFILE_KEY_CANARY.len())
            .any(|window| window == PROFILE_KEY_CANARY.as_bytes())
    );
    assert!(
        bytes
            .windows("hosted-workspace:test-profile".len())
            .any(|window| window == b"hosted-workspace:test-profile")
    );
}

#[test]
fn schema_27_migrates_additively_and_preserves_connector_mounts() {
    let fixture = Fixture::new("schema-27");
    let mut store = fixture.open();
    store
        .save_mount(MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            fixture.root.join("notion-main"),
        ))
        .unwrap();
    drop(store);
    let connection = Connection::open(fixture.state_root.join("state.sqlite3")).unwrap();
    connection
        .execute_batch(
            "DROP TABLE hosted_workspace_pending_mounts;
             DROP TABLE hosted_workspace_pending_transitions;
             DROP TABLE hosted_workspace_mount_mappings;
             DROP TABLE hosted_workspace_attachments;
             DELETE FROM state_components WHERE component_id = 'durable:hosted_workspaces';
             UPDATE state_components SET version = 27 WHERE component_id = 'core:schema';
             PRAGMA user_version = 27;",
        )
        .unwrap();
    drop(connection);

    let migrated = fixture.open();
    assert_eq!(SqliteStateStore::current_schema_version(), 28);
    assert_eq!(
        migrated.load_mounts().unwrap()[0].mount_id.as_str(),
        "notion-main"
    );
    assert!(
        migrated
            .list_hosted_workspace_attachments()
            .unwrap()
            .is_empty()
    );
    let raw = Connection::open(&migrated.db_path).unwrap();
    let component: i64 = raw
        .query_row(
            "SELECT version FROM state_components WHERE component_id = 'durable:hosted_workspaces'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(component, 1);
}

#[test]
fn equal_revision_cannot_be_reinterpreted_and_revision_cannot_move_backward() {
    let mut store = InMemoryStateStore::new();
    let identity = identity("https://api.example.com", PROFILE_ID);
    store
        .begin_hosted_workspace_transition(transition(
            "initial",
            identity.clone(),
            Path::new("/tmp/revision-fence"),
            2,
            DIGEST_A,
            &[("mount", "local", "docs")],
        ))
        .unwrap();
    store
        .commit_hosted_workspace_transition("initial", "2026-08-03T00:00:00Z")
        .unwrap();

    for (revision, digest, target, expected) in [
        (2, DIGEST_B, "docs", "cannot be reinterpreted"),
        (2, DIGEST_A, "renamed", "cannot be reinterpreted"),
        (1, DIGEST_A, "docs", "cannot move backward"),
    ] {
        assert!(matches!(
            store.begin_hosted_workspace_transition(transition(
                "invalid-revision",
                identity.clone(),
                Path::new("/tmp/revision-fence"),
                revision,
                digest,
                &[("mount", "local", target)],
            )),
            Err(StoreError::InvalidState(message)) if message.contains(expected)
        ));
    }
}

fn identity(origin: &str, profile_id: &str) -> HostedWorkspaceIdentity {
    HostedWorkspaceIdentity::new(
        CanonicalApiOrigin::new(origin).unwrap(),
        WorkspaceProfileId::new(profile_id).unwrap(),
    )
}

fn transition(
    transition_id: &str,
    identity: HostedWorkspaceIdentity,
    root: &Path,
    revision: u64,
    digest: &str,
    mounts: &[(&str, &str, &str)],
) -> PreparedHostedWorkspaceTransition {
    let mappings = mounts
        .iter()
        .map(|(portable, local, target)| {
            HostedWorkspaceMountMapping::proposal(
                PortableMountId::new(*portable).unwrap(),
                MountId::new(*local),
                MountTarget::new(*target).unwrap(),
                revision,
            )
            .unwrap()
        })
        .collect();
    PreparedHostedWorkspaceTransition::new(
        transition_id,
        identity,
        HostedWorkspaceCredentialRef::new("hosted-workspace:test-profile").unwrap(),
        root,
        revision,
        1,
        LayoutDigest::new(digest).unwrap(),
        mappings,
        "2026-08-03T00:00:00Z",
    )
    .unwrap()
}

fn assert_mapping(
    mappings: &[HostedWorkspaceMountMapping],
    portable: &str,
    local: &str,
    active: bool,
    first: u64,
    last: u64,
) {
    let mapping = mappings
        .iter()
        .find(|mapping| mapping.portable_mount_id().as_str() == portable)
        .unwrap();
    assert_eq!(mapping.local_mount_id().as_str(), local);
    assert_eq!(mapping.is_active(), active);
    assert_eq!(mapping.first_seen_revision(), first);
    assert_eq!(mapping.last_seen_revision(), last);
}

static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    state_root: PathBuf,
    workspace_root: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "locality-hosted-workspace-{label}-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        let state_root = root.join("state");
        let workspace_root = root.join("Locality");
        fs::create_dir_all(&root).unwrap();
        Self {
            root,
            state_root,
            workspace_root,
        }
    }

    fn open(&self) -> SqliteStateStore {
        SqliteStateStore::open(self.state_root.clone()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
