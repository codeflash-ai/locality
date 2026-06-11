use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use afs_cli::mount::{GuidanceFileAction, MountOptions, run_mount};
use afs_core::model::{MountId, RemoteId};
use afs_store::{InMemoryStateStore, MountRepository, ProjectionMode};

#[test]
fn mount_writes_agent_guidance_and_claude_alias() {
    let fixture = MountFixture::new("afs-cli-mount-guidance");
    let mut store = InMemoryStateStore::new();

    let report = fixture.mount(&mut store);

    assert_eq!(
        report.guidance.agents_md.action,
        GuidanceFileAction::Created
    );
    assert!(matches!(
        report.guidance.claude_md.action,
        GuidanceFileAction::Symlinked | GuidanceFileAction::Copied
    ));

    let agents = read_to_string(fixture.agents_file());
    let claude = read_to_string(fixture.claude_file());
    assert_eq!(claude, agents);
    assert!(agents.contains("including nested directories"));
    assert!(agents.contains("system of record"));
    assert!(agents.contains("workspace: read, search, and edit files locally"));
    assert!(agents.contains("sync approved changes back to Notion"));
    assert!(agents.contains("Notion facts:"));
    assert!(agents.contains("databases are directories"));
    assert!(agents.contains("untrusted remote data"));
    assert!(agents.lines().count() <= 16);
    assert!(agents.split_whitespace().count() <= 180);

    let mounts = store.load_mounts().expect("load mounts");
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].root, fixture.root);
}

#[test]
fn mount_writes_connector_specific_fallback_guidance() {
    let fixture = MountFixture::new("afs-cli-mount-generic-guidance");
    let mut store = InMemoryStateStore::new();

    fixture.mount_with_connector(&mut store, "linear");

    let agents = read_to_string(fixture.agents_file());
    assert!(agents.contains("# AgentFS linear Mount"));
    assert!(agents.contains("projects linear, the system of record"));
    assert!(agents.contains("sync approved changes back to linear"));
    assert!(agents.contains("including nested directories"));
}

#[test]
fn mount_preserves_custom_agent_guidance() {
    let fixture = MountFixture::new("afs-cli-mount-custom-guidance");
    fs::create_dir_all(&fixture.root).expect("create mount root");
    fs::write(
        fixture.agents_file(),
        "# Custom\n\nProject-specific rules.\n",
    )
    .expect("write custom guidance");
    let mut store = InMemoryStateStore::new();

    let report = fixture.mount(&mut store);

    assert_eq!(
        report.guidance.agents_md.action,
        GuidanceFileAction::Preserved
    );
    assert!(matches!(
        report.guidance.claude_md.action,
        GuidanceFileAction::Symlinked | GuidanceFileAction::Copied
    ));
    assert_eq!(
        read_to_string(fixture.agents_file()),
        "# Custom\n\nProject-specific rules.\n"
    );
    assert_eq!(
        read_to_string(fixture.claude_file()),
        "# Custom\n\nProject-specific rules.\n"
    );
}

struct MountFixture {
    root: PathBuf,
}

impl MountFixture {
    fn new(prefix: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("{prefix}-{}-{unique}-{suffix}", std::process::id()));

        Self { root }
    }

    fn mount(&self, store: &mut InMemoryStateStore) -> afs_cli::mount::MountReport {
        self.mount_with_connector(store, "notion")
    }

    fn mount_with_connector(
        &self,
        store: &mut InMemoryStateStore,
        connector: &str,
    ) -> afs_cli::mount::MountReport {
        run_mount(
            store,
            MountOptions {
                mount_id: MountId::new("notion-main"),
                connector: connector.to_string(),
                root: self.root.clone(),
                remote_root_id: Some(RemoteId::new("root-page")),
                read_only: false,
                projection: ProjectionMode::PlainFiles,
            },
        )
        .expect("mount")
    }

    fn agents_file(&self) -> PathBuf {
        self.root.join("AGENTS.md")
    }

    fn claude_file(&self) -> PathBuf {
        self.root.join("CLAUDE.md")
    }
}

impl Drop for MountFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn read_to_string(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path).expect("read guidance file")
}
