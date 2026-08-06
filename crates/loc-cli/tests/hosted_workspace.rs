use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use loc_cli::hosted_workspace::{
    HostedWorkspaceAttachOptions, deterministic_local_mount_id,
    list_hosted_workspace_attachments_at_state_root,
    recover_hosted_workspace_attachments_with_credentials_at_state_root,
    revalidate_connector_mount_placement_at_state_root, run_hosted_workspace_attach_at_state_root,
};
use loc_cli::sandbox::SandboxContentEncodingPreference;
use locality_core::model::MountId;
use locality_core::workspace_layout::{MountTarget, PortableMountId};
use locality_protocol::workspace_layout::{LayoutDigest, WorkspaceProfileId};
use locality_store::{
    CanonicalApiOrigin, CredentialStore, FileCredentialStore, HostedWorkspaceCredentialRef,
    HostedWorkspaceIdentity, HostedWorkspaceMountMapping, HostedWorkspaceRepository,
    PreparedHostedWorkspaceTransition, SqliteStateStore,
};

const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const PROFILE_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn connector_mount_preflight_shares_the_path_lock_and_rejects_hosted_roots() {
    let fixture = Fixture::new();
    let attached_root = fixture.root.join("Locality");
    let pending_root = fixture.root.join("PendingLocality");
    let mut store = fixture.open();
    begin(
        &mut store,
        "attached",
        identity(
            "https://api.one.example",
            "018f4f6e-9f2c-7b1a-8c3d-4e5f60718293",
        ),
        &attached_root,
        "local-attached",
    );
    store
        .commit_hosted_workspace_transition("attached", "2026-08-03T00:00:01Z")
        .unwrap();
    let listed = list_hosted_workspace_attachments_at_state_root(&fixture.state_root).unwrap();
    assert_eq!(listed.attachments.len(), 1);
    assert_eq!(
        listed.attachments[0].root,
        attached_root.display().to_string()
    );
    assert_eq!(listed.attachments[0].mounts.len(), 1);
    assert!(listed.attachments[0].mounts[0].active);
    begin(
        &mut store,
        "pending",
        identity(
            "https://api.two.example",
            "018f4f6e-9f2c-7b1a-8c3d-4e5f60718294",
        ),
        &pending_root,
        "local-pending",
    );
    drop(store);

    let _path_lock =
        locality_platform::DaemonRemountCoordinatorLock::try_acquire(&fixture.state_root).unwrap();
    for blocked in [
        attached_root.join("connector"),
        pending_root.join("connector"),
    ] {
        let error = revalidate_connector_mount_placement_at_state_root(
            &fixture.state_root,
            &MountId::new("connector-new"),
            &blocked,
        )
        .unwrap_err();
        assert_eq!(error.code(), "hosted_workspace_invalid_placement");
    }
    revalidate_connector_mount_placement_at_state_root(
        &fixture.state_root,
        &MountId::new("connector-new"),
        &fixture.root.join("Independent"),
    )
    .unwrap();
}

#[test]
fn attach_rejects_an_unavailable_parent_before_mutating_the_target_tree() {
    let fixture = Fixture::new();
    let missing_parent = fixture.root.join("missing-parent");
    let target = missing_parent.join("Locality");
    let error = run_hosted_workspace_attach_at_state_root(
        HostedWorkspaceAttachOptions {
            api_url: "https://api.example.com".to_string(),
            root: target.clone(),
            credential_ref: HostedWorkspaceCredentialRef::new("hosted-workspace:missing").unwrap(),
            content_encoding: SandboxContentEncodingPreference::Automatic,
        },
        &fixture.state_root,
    )
    .unwrap_err();

    assert_eq!(error.code(), "hosted_workspace_invalid_placement");
    assert!(!missing_parent.exists());
    assert!(!target.exists());
}

#[test]
fn recovery_does_not_cancel_a_live_staging_transition_or_block_on_unrelated_credentials() {
    let fixture = Fixture::new();
    let live_identity = identity(
        "https://api.live.example",
        "018f4f6e-9f2c-7b1a-8c3d-4e5f60718293",
    );
    let missing_identity = identity(
        "https://api.missing.example",
        "018f4f6e-9f2c-7b1a-8c3d-4e5f60718294",
    );
    let mut store = fixture.open();
    begin(
        &mut store,
        "live-staging",
        live_identity.clone(),
        &fixture.root.join("LiveWorkspace"),
        "live-local",
    );
    begin(
        &mut store,
        "missing-credential",
        missing_identity.clone(),
        &fixture.root.join("MissingWorkspace"),
        "missing-local",
    );
    drop(store);
    let credentials = FileCredentialStore::new(&fixture.state_root);
    credentials
        .put("hosted-workspace:live-staging", PROFILE_KEY)
        .unwrap();
    let liveness = locality_platform::HostedWorkspaceTransitionLock::try_acquire(
        &fixture.state_root,
        "live-staging",
    )
    .unwrap();

    recover_hosted_workspace_attachments_with_credentials_at_state_root(
        &fixture.state_root,
        &credentials,
    )
    .unwrap();
    let store = fixture.open();
    assert!(
        store
            .get_pending_hosted_workspace_transition(&live_identity)
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .get_pending_hosted_workspace_transition(&missing_identity)
            .unwrap()
            .is_some()
    );
    drop(store);

    drop(liveness);
    recover_hosted_workspace_attachments_with_credentials_at_state_root(
        &fixture.state_root,
        &credentials,
    )
    .unwrap();
    let store = fixture.open();
    assert!(
        store
            .get_pending_hosted_workspace_transition(&live_identity)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_pending_hosted_workspace_transition(&missing_identity)
            .unwrap()
            .is_some()
    );
}

#[test]
fn deterministic_mount_id_is_stable_across_fresh_stores_and_scoped_by_identity() {
    let first = Fixture::new();
    let second = Fixture::new();
    let hosted_identity = identity(
        "HTTPS://API.Example.COM:443/",
        "018f4f6e-9f2c-7b1a-8c3d-4e5f60718293",
    );
    let portable = PortableMountId::new("portable-docs").unwrap();
    let expected = deterministic_local_mount_id(&hosted_identity, &portable);
    for fixture in [&first, &second] {
        let mut store = fixture.open();
        store
            .begin_hosted_workspace_transition(
                PreparedHostedWorkspaceTransition::new(
                    "fresh-transition",
                    hosted_identity.clone(),
                    HostedWorkspaceCredentialRef::new("hosted-workspace:fresh").unwrap(),
                    fixture.root.join("Workspace"),
                    1,
                    1,
                    LayoutDigest::new(DIGEST).unwrap(),
                    vec![
                        HostedWorkspaceMountMapping::proposal(
                            portable.clone(),
                            deterministic_local_mount_id(&hosted_identity, &portable),
                            MountTarget::new("docs").unwrap(),
                            1,
                        )
                        .unwrap(),
                    ],
                    "2026-08-03T00:00:00Z",
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            store
                .get_pending_hosted_workspace_transition(&hosted_identity)
                .unwrap()
                .unwrap()
                .prepared()
                .mounts()[0]
                .local_mount_id(),
            &expected
        );
    }
    let other_origin = identity(
        "https://api.other.example",
        "018f4f6e-9f2c-7b1a-8c3d-4e5f60718293",
    );
    assert_ne!(
        deterministic_local_mount_id(&other_origin, &portable),
        expected
    );
}

fn begin(
    store: &mut SqliteStateStore,
    transition_id: &str,
    identity: HostedWorkspaceIdentity,
    root: &Path,
    local_mount_id: &str,
) {
    store
        .begin_hosted_workspace_transition(
            PreparedHostedWorkspaceTransition::new(
                transition_id,
                identity,
                HostedWorkspaceCredentialRef::new(format!("hosted-workspace:{transition_id}"))
                    .unwrap(),
                root,
                1,
                1,
                LayoutDigest::new(DIGEST).unwrap(),
                vec![
                    HostedWorkspaceMountMapping::proposal(
                        PortableMountId::new("portable").unwrap(),
                        MountId::new(local_mount_id),
                        MountTarget::new("docs").unwrap(),
                        1,
                    )
                    .unwrap(),
                ],
                "2026-08-03T00:00:00Z",
            )
            .unwrap(),
        )
        .unwrap();
}

fn identity(origin: &str, profile_id: &str) -> HostedWorkspaceIdentity {
    HostedWorkspaceIdentity::new(
        CanonicalApiOrigin::new(origin).unwrap(),
        WorkspaceProfileId::new(profile_id).unwrap(),
    )
}

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    state_root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "locality-cli-hosted-workspace-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self {
            state_root: root.join("state"),
            root,
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
