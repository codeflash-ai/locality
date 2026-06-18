use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use afs_cli::inspect::{InspectOptions, run_inspect};
use afs_core::explain::{RemoteChangeAction, RemoteChangeState};
use afs_core::hydration::{HydrationReason, HydrationRequest};
use afs_core::model::{CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId};
use afs_core::shadow::ShadowDocument;
use afs_core::{AfsError, AfsResult};
use afs_store::{
    EntityRecord, EntityRepository, InMemoryStateStore, MountConfig, MountRepository,
    ProjectionMode, ShadowRepository,
};
use afsd::hydration::{HydratedEntity, HydrationSource};
use afsd::virtual_fs::virtual_fs_content_path;

#[test]
fn inspect_reports_remote_changed_only_as_safe_to_fast_forward() {
    let fixture = InspectFixture::new();
    let mut store = fixture.store(ProjectionMode::PlainFiles);
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nBase body.");
    store
        .save_shadow(
            &fixture.mount_id,
            shadow("page-1", "# Roadmap\n\nBase body."),
        )
        .expect("save shadow");
    let source = FakeInspectSource::new(rendered_entity("page-1", "# Roadmap\n\nRemote body."));

    let report = run_inspect(
        &store,
        &source,
        InspectOptions {
            path,
            state_root: None,
        },
    )
    .expect("inspect report");

    assert!(report.ok);
    assert_eq!(
        report.explanation.state,
        RemoteChangeState::RemoteChangedOnly
    );
    assert_eq!(
        report.explanation.action,
        RemoteChangeAction::SafeToFastForward
    );
    assert!(!report.explanation.local.changed);
    assert!(report.explanation.remote.changed);
    assert_eq!(
        source.requests.borrow()[0].reason,
        HydrationReason::ExplicitPull
    );
}

#[test]
fn inspect_reports_both_changed_when_local_and_remote_diverged() {
    let fixture = InspectFixture::new();
    let mut store = fixture.store(ProjectionMode::PlainFiles);
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nLocal body.");
    store
        .save_shadow(
            &fixture.mount_id,
            shadow("page-1", "# Roadmap\n\nBase body."),
        )
        .expect("save shadow");
    let source = FakeInspectSource::new(rendered_entity("page-1", "# Roadmap\n\nRemote body."));

    let report = run_inspect(
        &store,
        &source,
        InspectOptions {
            path,
            state_root: None,
        },
    )
    .expect("inspect report");

    assert!(report.ok);
    assert_eq!(report.explanation.state, RemoteChangeState::BothChanged);
    assert_eq!(
        report.explanation.action,
        RemoteChangeAction::ReviewBeforePush
    );
    assert!(report.explanation.local.changed);
    assert!(report.explanation.remote.changed);
}

#[test]
fn inspect_surfaces_local_parse_failures_as_needs_review() {
    let fixture = InspectFixture::new();
    let mut store = fixture.store(ProjectionMode::PlainFiles);
    let path = fixture.write_raw("Roadmap.md", "# Roadmap\n\nMissing frontmatter.");
    store
        .save_shadow(
            &fixture.mount_id,
            shadow("page-1", "# Roadmap\n\nBase body."),
        )
        .expect("save shadow");
    let source = FakeInspectSource::new(rendered_entity("page-1", "# Roadmap\n\nBase body."));

    let report = run_inspect(
        &store,
        &source,
        InspectOptions {
            path,
            state_root: None,
        },
    )
    .expect("inspect report");

    assert!(!report.ok);
    assert_eq!(report.explanation.state, RemoteChangeState::NeedsReview);
    assert_eq!(report.explanation.issues[0].code, "local_parse_failed");
}

#[test]
fn inspect_requires_local_frontmatter_identity() {
    let fixture = InspectFixture::new();
    let mut store = fixture.store(ProjectionMode::PlainFiles);
    let path = fixture.write_raw(
        "Roadmap.md",
        "---\ntitle: Roadmap\n---\n# Roadmap\n\nMissing identity.",
    );
    store
        .save_shadow(
            &fixture.mount_id,
            shadow("page-1", "# Roadmap\n\nBase body."),
        )
        .expect("save shadow");
    let source = FakeInspectSource::new(rendered_entity("page-1", "# Roadmap\n\nBase body."));

    let report = run_inspect(
        &store,
        &source,
        InspectOptions {
            path,
            state_root: None,
        },
    )
    .expect("inspect report");

    assert!(!report.ok);
    assert_eq!(report.explanation.state, RemoteChangeState::NeedsReview);
    assert_eq!(
        report.explanation.issues[0].code,
        "frontmatter_remote_id_missing"
    );
}

#[test]
fn inspect_reads_virtual_projection_content_cache() {
    let fixture = InspectFixture::new();
    let mut store = fixture.store(ProjectionMode::LinuxFuse);
    store
        .save_shadow(
            &fixture.mount_id,
            shadow("page-1", "# Roadmap\n\nBase body."),
        )
        .expect("save shadow");
    let cache_path = virtual_fs_content_path(
        &fixture.state_root,
        &fixture.mount_id,
        Path::new("Roadmap.md"),
    )
    .expect("content path");
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent).expect("cache parent");
    }
    fs::write(
        &cache_path,
        canonical_markdown("page-1", "# Roadmap\n\nLocal cached body."),
    )
    .expect("content cache");
    let source = FakeInspectSource::new(rendered_entity("page-1", "# Roadmap\n\nBase body."));

    let report = run_inspect(
        &store,
        &source,
        InspectOptions {
            path: fixture.root.join("Roadmap.md"),
            state_root: Some(fixture.state_root.clone()),
        },
    )
    .expect("inspect report");

    assert_eq!(report.local_read_path, cache_path.display().to_string());
    assert_eq!(
        report.explanation.state,
        RemoteChangeState::LocalChangedOnly
    );
}

#[test]
fn inspect_treats_equivalent_media_paths_as_unchanged_remote() {
    let fixture = InspectFixture::new();
    let mut store = fixture.store(ProjectionMode::PlainFiles);
    let long_media_href = "../../../../../../../.afs/media/home/mohammed/.afs/content/notion-main/files/getting-started-3-new/image-fb3123d34d04464487428b0f2557e4a0.jpg";
    let stable_media_href =
        "../.afs/media/getting-started-3-new/image-fb3123d34d04464487428b0f2557e4a0.jpg";
    let synced_body = format!("# Roadmap\n\n![Image]({long_media_href})\n\nBase body.");
    let local_body = format!("# Roadmap\n\n![Image]({long_media_href})\n\nLocal body.");
    let remote_body = format!("# Roadmap\n\n![Image]({stable_media_href})\n\nBase body.");
    let path = fixture.write_page("Roadmap.md", &local_body);
    store
        .save_shadow(
            &fixture.mount_id,
            shadow_with_blocks("page-1", &synced_body),
        )
        .expect("save shadow");
    let source = FakeInspectSource::new(rendered_entity_with_image_block("page-1", &remote_body));

    let report = run_inspect(
        &store,
        &source,
        InspectOptions {
            path,
            state_root: None,
        },
    )
    .expect("inspect report");

    assert_eq!(
        report.explanation.state,
        RemoteChangeState::LocalChangedOnly
    );
    assert!(report.explanation.local.changed);
    assert!(!report.explanation.remote.changed);
    assert_eq!(
        report
            .explanation
            .remote
            .plan
            .as_ref()
            .expect("remote plan")
            .operations
            .len(),
        0
    );
}

struct InspectFixture {
    root: PathBuf,
    state_root: PathBuf,
    mount_id: MountId,
}

impl InspectFixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "afs-cli-inspect-{}-{unique}-{suffix}",
            std::process::id()
        ));
        let state_root = root.join(".state");
        fs::create_dir_all(&root).expect("fixture root");
        Self {
            root,
            state_root,
            mount_id: MountId::new("notion-main"),
        }
    }

    fn store(&self, projection: ProjectionMode) -> InMemoryStateStore {
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
                .with_hydration(HydrationState::Hydrated),
            )
            .expect("save entity");
        store
    }

    fn write_page(&self, relative_path: &str, body: &str) -> PathBuf {
        self.write_raw(relative_path, &canonical_markdown("page-1", body))
    }

    fn write_raw(&self, relative_path: &str, contents: &str) -> PathBuf {
        let path = self.root.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent");
        }
        fs::write(&path, contents).expect("fixture file");
        path
    }
}

impl Drop for InspectFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Debug)]
struct FakeInspectSource {
    remote: HydratedEntity,
    requests: RefCell<Vec<HydrationRequest>>,
}

impl FakeInspectSource {
    fn new(remote: HydratedEntity) -> Self {
        Self {
            remote,
            requests: RefCell::new(Vec::new()),
        }
    }
}

impl HydrationSource for FakeInspectSource {
    fn fetch_render(&self, request: &HydrationRequest) -> AfsResult<HydratedEntity> {
        if request.remote_id != RemoteId::new("page-1") {
            return Err(AfsError::InvalidState("unexpected remote id".to_string()));
        }
        self.requests.borrow_mut().push(request.clone());
        Ok(self.remote.clone())
    }
}

fn rendered_entity(remote_id: &str, body: &str) -> HydratedEntity {
    HydratedEntity {
        document: CanonicalDocument::new(frontmatter(remote_id), body),
        shadow: shadow(remote_id, body),
        remote_edited_at: Some("2026-06-11T00:00:00Z".to_string()),
        assets: Vec::new(),
    }
}

fn rendered_entity_with_image_block(remote_id: &str, body: &str) -> HydratedEntity {
    HydratedEntity {
        document: CanonicalDocument::new(frontmatter(remote_id), body),
        shadow: shadow_with_blocks(remote_id, body),
        remote_edited_at: Some("2026-06-11T00:00:00Z".to_string()),
        assets: Vec::new(),
    }
}

fn canonical_markdown(remote_id: &str, body: &str) -> String {
    format!("---\n{}---\n{body}", frontmatter(remote_id))
}

fn shadow(remote_id: &str, body: &str) -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new(remote_id),
        body,
        9,
        [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
    )
    .expect("shadow")
    .with_frontmatter(frontmatter(remote_id))
}

fn shadow_with_blocks(remote_id: &str, body: &str) -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new(remote_id),
        body,
        9,
        [
            RemoteId::new("heading-1"),
            RemoteId::new("image-1"),
            RemoteId::new("paragraph-1"),
        ],
    )
    .expect("shadow")
    .with_frontmatter(frontmatter(remote_id))
}

fn frontmatter(remote_id: &str) -> String {
    format!(
        "afs:\n  id: {remote_id}\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n"
    )
}
