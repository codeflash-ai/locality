use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_store::{
    ConnectionId, ConnectionRecord, ConnectionRepository, CredentialStore, FileCredentialStore,
    SqliteStateStore, reset_locality_state_storage,
};

#[test]
fn reset_storage_clears_state_root_and_connection_credentials() {
    let fixture = ResetFixture::new();
    fs::create_dir_all(fixture.state_root.join("content/notion-main")).expect("create content");
    fs::create_dir_all(&fixture.mount_root).expect("create visible mount");
    fs::write(
        fixture.state_root.join("content/notion-main/page.md"),
        b"cached",
    )
    .expect("write cache");
    fs::write(fixture.mount_root.join("page.md"), b"user visible").expect("write visible file");

    let mut store = SqliteStateStore::open(fixture.state_root.clone()).expect("open state");
    store
        .save_connection(connection_record("notion-main", "connection:notion-main"))
        .expect("save first connection");
    store
        .save_connection(connection_record("docs-main", "connection:docs-main"))
        .expect("save second connection");
    drop(store);

    let credentials = FileCredentialStore::new(&fixture.state_root);
    credentials
        .put("connection:notion-main", "notion-secret")
        .expect("write first credential");
    credentials
        .put("connection:docs-main", "docs-secret")
        .expect("write second credential");

    let report = reset_locality_state_storage(&fixture.state_root).expect("reset storage");

    assert_eq!(report.deleted_secret_refs.len(), 4, "{report:#?}");
    assert!(
        report
            .deleted_secret_refs
            .contains(&"connection:notion-main".to_string()),
        "{report:#?}"
    );
    assert!(
        report
            .deleted_secret_refs
            .contains(&"connection:docs-main".to_string()),
        "{report:#?}"
    );
    assert!(fixture.state_root.exists());
    assert!(
        fs::read_dir(&fixture.state_root)
            .expect("read state root")
            .next()
            .is_none(),
        "state root should be empty after reset"
    );
    assert_eq!(
        fs::read(fixture.mount_root.join("page.md")).expect("read visible file"),
        b"user visible"
    );
}

fn connection_record(id: &str, secret_ref: &str) -> ConnectionRecord {
    ConnectionRecord {
        connection_id: ConnectionId::new(id),
        profile_id: None,
        connector: "notion".to_string(),
        display_name: id.to_string(),
        account_label: None,
        workspace_id: None,
        workspace_name: None,
        auth_kind: "token".to_string(),
        secret_ref: secret_ref.to_string(),
        scopes: vec![],
        capabilities_json: "{}".to_string(),
        status: "active".to_string(),
        created_at: "2026-07-09T00:00:00Z".to_string(),
        updated_at: "2026-07-09T00:00:00Z".to_string(),
        expires_at: None,
    }
}

struct ResetFixture {
    state_root: PathBuf,
    mount_root: PathBuf,
}

impl ResetFixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "locality-store-reset-{}-{unique}-{suffix}",
            std::process::id()
        ));
        Self {
            state_root: root.join("state"),
            mount_root: root.join("visible-mount"),
        }
    }
}

impl Drop for ResetFixture {
    fn drop(&mut self) {
        let Some(root) = self.state_root.parent() else {
            return;
        };
        let _ = remove_dir_all_if_exists(root);
    }
}

fn remove_dir_all_if_exists(path: &Path) -> std::io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
