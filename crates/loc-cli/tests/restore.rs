use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use loc_cli::restore::{RestoreOptions, run_restore};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use locality_core::shadow::ShadowDocument;
use locality_store::{
    EntityRecord, EntityRepository, InMemoryStateStore, MountConfig, MountRepository,
    ProjectionMode, ShadowRepository,
};
use localityd::virtual_fs::virtual_fs_content_path;

#[test]
fn restore_rewrites_file_from_shadow_and_marks_entity_hydrated() {
    let fixture = RestoreFixture::new();
    let mut store = fixture.store(HydrationState::Dirty);
    let path = fixture.write_page("# Roadmap\n\nLocal edit.");

    let report = run_restore(&mut store, &path, RestoreOptions::default()).expect("restore report");

    assert!(report.ok);
    assert_eq!(report.action, "restored");
    let contents = fs::read_to_string(&path).expect("restored file");
    assert!(contents.contains("# Roadmap\n\nSynced body."));
    assert!(!contents.contains("Local edit"));

    let entity = store
        .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Hydrated);
    assert_eq!(entity.content_hash, Some(shadow().body_hash));
}

#[test]
fn restore_page_directory_targets_page_document() {
    let fixture = RestoreFixture::new();
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "notion",
            fixture.root.clone(),
        ))
        .expect("save mount");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap/page.md",
            )
            .with_hydration(HydrationState::Dirty)
            .with_content_hash("dirty")
            .with_remote_edited_at("2026-06-10T00:00:00Z"),
        )
        .expect("save page-directory entity");
    store
        .save_shadow(&fixture.mount_id, shadow())
        .expect("save shadow");
    let page_dir = fixture.root.join("Roadmap");
    fs::create_dir_all(&page_dir).expect("page dir");
    let page_path = page_dir.join("page.md");
    fs::write(&page_path, canonical_markdown("# Roadmap\n\nLocal edit.")).expect("write page");

    let report =
        run_restore(&mut store, &page_dir, RestoreOptions::default()).expect("restore report");

    assert!(report.ok);
    assert!(
        fs::read_to_string(&page_path)
            .expect("restored page")
            .contains("# Roadmap\n\nSynced body.")
    );
    let entity = store
        .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Hydrated);
}

#[test]
fn restore_requires_force_for_conflicted_entity() {
    let fixture = RestoreFixture::new();
    let mut store = fixture.store(HydrationState::Conflicted);
    let path = fixture.write_page("# Roadmap\n\nLocal edit.");

    let error =
        run_restore(&mut store, &path, RestoreOptions::default()).expect_err("conflicted restore");

    assert_eq!(error.code(), "restore_conflicted_requires_force");
}

#[test]
fn restore_virtual_projection_writes_content_cache_instead_of_mount_file() {
    let fixture = RestoreFixture::new();
    let mut store = fixture.store_with_projection(HydrationState::Dirty, ProjectionMode::LinuxFuse);
    let mount_path = fixture.write_page("# Roadmap\n\nFUSE projection body.");
    let cache_path = virtual_fs_content_path(
        &fixture.state_root,
        &fixture.mount_id,
        "Roadmap.md".as_ref(),
    )
    .expect("content cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        canonical_markdown("# Roadmap\n\nLocal cache edit."),
    )
    .expect("write cache");

    let report = run_restore(
        &mut store,
        &mount_path,
        RestoreOptions {
            force: false,
            state_root: Some(fixture.state_root.clone()),
        },
    )
    .expect("restore report");

    assert!(report.ok);
    assert!(
        fs::read_to_string(&cache_path)
            .expect("restored cache")
            .contains("# Roadmap\n\nSynced body.")
    );
    assert!(
        fs::read_to_string(&mount_path)
            .expect("mount file")
            .contains("FUSE projection body.")
    );
}

struct RestoreFixture {
    root: PathBuf,
    state_root: PathBuf,
    mount_id: MountId,
}

impl RestoreFixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "loc-cli-restore-{}-{unique}-{suffix}",
            std::process::id()
        ));
        let root = base.join("mount");
        let state_root = base.join("state");
        fs::create_dir_all(&root).expect("fixture root");
        fs::create_dir_all(&state_root).expect("fixture state root");
        Self {
            root,
            state_root,
            mount_id: MountId::new("notion-main"),
        }
    }

    fn store(&self, hydration: HydrationState) -> InMemoryStateStore {
        self.store_with_projection(hydration, ProjectionMode::PlainFiles)
    }

    fn store_with_projection(
        &self,
        hydration: HydrationState,
        projection: ProjectionMode,
    ) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(
                MountConfig::new(self.mount_id.clone(), "notion", self.root.clone())
                    .projection(projection),
            )
            .expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    self.mount_id.clone(),
                    RemoteId::new("page-1"),
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap.md",
                )
                .with_hydration(hydration)
                .with_content_hash("dirty")
                .with_remote_edited_at("2026-06-10T00:00:00Z"),
            )
            .expect("save entity");
        store
            .save_shadow(&self.mount_id, shadow())
            .expect("save shadow");
        store
    }

    fn write_page(&self, body: &str) -> PathBuf {
        let path = self.root.join("Roadmap.md");
        fs::write(&path, canonical_markdown(body)).expect("write file");
        path
    }
}

impl Drop for RestoreFixture {
    fn drop(&mut self) {
        if let Some(base) = self.root.parent() {
            let _ = fs::remove_dir_all(base);
        }
    }
}

fn canonical_markdown(body: &str) -> String {
    format!(
        "---\nloc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\n{body}"
    )
}

fn shadow() -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        "# Roadmap\n\nSynced body.",
        9,
        [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
    )
    .expect("shadow")
    .with_frontmatter(
        "loc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n",
    )
}
