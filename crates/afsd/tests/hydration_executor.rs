use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use afs_core::canonical::render_canonical_markdown;
use afs_core::conflict::{
    CONFLICT_LOCAL_MARKER, CONFLICT_REMOTE_MARKER, CONFLICT_SEPARATOR_MARKER,
    has_unresolved_conflict_markers,
};
use afs_core::hydration::{HydrationReason, HydrationRequest};
use afs_core::model::{CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId};
use afs_core::shadow::ShadowDocument;
use afs_core::{AfsError, AfsResult};
use afs_store::{
    EntityRecord, EntityRepository, FreshnessStateRecord, FreshnessStateRepository,
    InMemoryStateStore, MountConfig, MountRepository, ShadowRepository,
};
use afsd::hydration::{
    HydratedAsset, HydratedEntity, HydrationExecutor, HydrationOutcome, HydrationQueue,
    HydrationSource,
};

#[test]
fn executor_hydrates_stub_file_and_persists_shadow() {
    let fixture = HydrationFixture::new();
    let mut store = fixture.store(HydrationState::Stub);
    fixture.write_stub();
    let rendered = rendered_entity("Remote body.");
    let source = FakeHydrationSource::with_entity("page-1", rendered.clone());

    let mut executor = HydrationExecutor::new(&mut store, &source);
    let outcome = executor
        .hydrate_request(fixture.request())
        .expect("hydrate request");

    assert_eq!(outcome, HydrationOutcome::Hydrated);
    let contents = fs::read_to_string(fixture.page_path()).expect("hydrated file");
    assert!(contents.contains("Remote body."));
    assert!(!contents.contains(CanonicalDocument::STUB_MARKER));
    assert_eq!(
        store
            .load_shadow(&fixture.mount_id, &fixture.remote_id)
            .expect("load shadow"),
        rendered.shadow
    );
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Hydrated);
    assert_eq!(entity.content_hash, Some(rendered.shadow.body_hash.clone()));
    assert_eq!(entity.remote_edited_at, rendered.remote_edited_at);
}

#[test]
fn executor_fetches_render_using_projected_entity_path() {
    let fixture = HydrationFixture::new();
    let mut store = fixture.store(HydrationState::Stub);
    fixture.write_stub();
    let source = FakeHydrationSource::with_entity("page-1", rendered_entity("Remote body."));

    let mut executor = HydrationExecutor::new(&mut store, &source);
    executor
        .hydrate_request(fixture.request())
        .expect("hydrate request");

    assert_eq!(
        source.request_paths(),
        vec![PathBuf::from("Roadmap/page.md")]
    );
    assert!(fixture.page_path().exists());
}

#[test]
fn executor_replaces_clean_hydrated_file() {
    let fixture = HydrationFixture::new();
    let mut store = fixture.store(HydrationState::Hydrated);
    let old = rendered_entity("Old body.");
    store
        .save_shadow(&fixture.mount_id, old.shadow.clone())
        .expect("save old shadow");
    store
        .save_freshness_state(
            FreshnessStateRecord::new(
                fixture.mount_id.clone(),
                fixture.remote_id.clone(),
                afs_core::freshness::FreshnessTier::Hot,
            )
            .remote_hint_pending(true),
        )
        .expect("save freshness");
    fixture.write_markdown(&old.document);
    let new = rendered_entity("New body.");
    let source = FakeHydrationSource::with_entity("page-1", new.clone());

    let mut executor = HydrationExecutor::new(&mut store, &source);
    let outcome = executor
        .hydrate_request(fixture.request())
        .expect("hydrate request");

    assert_eq!(outcome, HydrationOutcome::Hydrated);
    let contents = fs::read_to_string(fixture.page_path()).expect("hydrated file");
    assert!(contents.contains("New body."));
    assert_eq!(
        store
            .load_shadow(&fixture.mount_id, &fixture.remote_id)
            .expect("load shadow")
            .body_hash,
        new.shadow.body_hash
    );
    assert!(
        !store
            .get_freshness_state(&fixture.mount_id, &fixture.remote_id)
            .expect("get freshness")
            .expect("freshness")
            .remote_hint_pending
    );
}

#[test]
fn executor_rehydrates_stale_dirty_file_when_projection_matches_shadow() {
    let fixture = HydrationFixture::new();
    let mut store = fixture.store(HydrationState::Dirty);
    let old = rendered_entity("Old body.");
    store
        .save_shadow(&fixture.mount_id, old.shadow.clone())
        .expect("save old shadow");
    fixture.write_markdown(&old.document);
    let new = rendered_entity("New body.");
    let source = FakeHydrationSource::with_entity("page-1", new.clone());

    let mut executor = HydrationExecutor::new(&mut store, &source);
    let outcome = executor
        .hydrate_request(fixture.request())
        .expect("hydrate request");

    assert_eq!(outcome, HydrationOutcome::Hydrated);
    let contents = fs::read_to_string(fixture.page_path()).expect("hydrated file");
    assert!(contents.contains("New body."));
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Hydrated);
}

#[test]
fn executor_rehydrates_stale_dirty_file_when_only_sync_metadata_drifted() {
    let fixture = HydrationFixture::new();
    let mut store = fixture.store(HydrationState::Dirty);
    let old = rendered_entity_with_sync(
        "Old body.",
        "2026-06-18T07:06:00.000Z",
        "2026-06-18T07:06:00.000Z",
    );
    store
        .save_shadow(&fixture.mount_id, old.shadow.clone())
        .expect("save old shadow");
    fixture.write_markdown(
        &rendered_entity_with_sync(
            "Old body.",
            "2026-06-10T23:03:00.000Z",
            "2026-06-10T23:03:00.000Z",
        )
        .document,
    );
    let new = rendered_entity("New body.");
    let source = FakeHydrationSource::with_entity("page-1", new);

    let mut executor = HydrationExecutor::new(&mut store, &source);
    let outcome = executor
        .hydrate_request(fixture.request())
        .expect("hydrate request");

    assert_eq!(outcome, HydrationOutcome::Hydrated);
    let contents = fs::read_to_string(fixture.page_path()).expect("hydrated file");
    assert!(contents.contains("New body."));
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Hydrated);
}

#[test]
fn executor_writes_hydrated_assets_under_mount_root() {
    let fixture = HydrationFixture::new();
    let mut store = fixture.store(HydrationState::Stub);
    fixture.write_stub();
    let mut rendered = rendered_entity("Remote body.");
    rendered.assets.push(HydratedAsset {
        path: PathBuf::from(".afs/media/Roadmap/image-1.png"),
        bytes: b"image-bytes".to_vec(),
        media: None,
    });
    let source = FakeHydrationSource::with_entity("page-1", rendered);

    let mut executor = HydrationExecutor::new(&mut store, &source);
    executor
        .hydrate_request(fixture.request())
        .expect("hydrate request");

    assert_eq!(
        fs::read(fixture.root.join(".afs/media/Roadmap/image-1.png")).expect("asset"),
        b"image-bytes"
    );
}

#[test]
fn executor_writes_absolute_media_hrefs_under_output_root() {
    let fixture = HydrationFixture::new();
    let mut store = fixture.store(HydrationState::Stub);
    fixture.write_stub();
    let mut rendered = rendered_entity("![Image](../.afs/media/Roadmap/image-1.png)");
    rendered.assets.push(HydratedAsset {
        path: PathBuf::from(".afs/media/Roadmap/image-1.png"),
        bytes: b"image-bytes".to_vec(),
        media: None,
    });
    let source = FakeHydrationSource::with_entity("page-1", rendered.clone());
    let output_root = fixture.root.join(".content/notion-main/files");

    let mut executor =
        HydrationExecutor::new_with_output_root(&mut store, &source, output_root.clone());
    executor
        .hydrate_request(fixture.request())
        .expect("hydrate request");

    let contents = fs::read_to_string(fixture.page_path()).expect("hydrated file");
    assert!(contents.contains(&format!(
        "![Image]({})",
        output_root.join(".afs/media/Roadmap/image-1.png").display()
    )));
    assert_eq!(
        store
            .load_shadow(&fixture.mount_id, &fixture.remote_id)
            .expect("load shadow"),
        rendered.shadow
    );
}

#[test]
fn executor_rejects_hydrated_assets_outside_mount_root() {
    let fixture = HydrationFixture::new();
    let mut store = fixture.store(HydrationState::Stub);
    fixture.write_stub();
    let mut rendered = rendered_entity("Remote body.");
    rendered.assets.push(HydratedAsset {
        path: PathBuf::from(".afs/media/../outside.png"),
        bytes: b"image-bytes".to_vec(),
        media: None,
    });
    let source = FakeHydrationSource::with_entity("page-1", rendered);

    let mut executor = HydrationExecutor::new(&mut store, &source);
    let error = executor
        .hydrate_request(fixture.request())
        .expect_err("unsafe asset path should fail");

    assert!(matches!(error, AfsError::InvalidState(_)));
    assert!(!fixture.root.join("outside.png").exists());
}

#[test]
fn executor_skips_dirty_file_and_marks_entity_dirty_when_remote_matches_shadow() {
    let fixture = HydrationFixture::new();
    let mut store = fixture.store(HydrationState::Hydrated);
    let old = rendered_entity("Old body.");
    store
        .save_shadow(&fixture.mount_id, old.shadow.clone())
        .expect("save old shadow");
    fixture.write_raw("---\nafs:\n  id: page-1\n  type: page\ntitle: Roadmap\n---\nLocal edit.\n");
    let source = FakeHydrationSource::with_entity("page-1", old);

    let mut executor = HydrationExecutor::new(&mut store, &source);
    let outcome = executor
        .hydrate_request(fixture.request())
        .expect("skip dirty file");

    assert_eq!(outcome, HydrationOutcome::SkippedDirty);
    let contents = fs::read_to_string(fixture.page_path()).expect("dirty file");
    assert!(contents.contains("Local edit."));
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Dirty);
    assert!(!fixture.page_path().with_extension("remote.md").exists());
}

#[test]
fn executor_remote_fast_forward_skips_dirty_file_without_materializing_conflict() {
    let fixture = HydrationFixture::new();
    let mut store = fixture.store(HydrationState::Hydrated);
    let old = rendered_entity("Old body.");
    store
        .save_shadow(&fixture.mount_id, old.shadow.clone())
        .expect("save old shadow");
    fixture.write_raw("---\nafs:\n  id: page-1\n  type: page\ntitle: Roadmap\n---\nLocal edit.\n");
    let source = FakeHydrationSource::with_entity("page-1", rendered_entity("Remote body."));
    let mut request = fixture.request();
    request.reason = HydrationReason::RemoteFastForward;

    let mut executor = HydrationExecutor::new(&mut store, &source);
    let outcome = executor
        .hydrate_request(request)
        .expect("skip dirty auto fast-forward");

    assert_eq!(outcome, HydrationOutcome::SkippedDirty);
    let contents = fs::read_to_string(fixture.page_path()).expect("dirty file");
    assert!(contents.contains("Local edit."));
    assert!(!contents.contains("Remote body."));
    assert!(!contents.contains(CONFLICT_LOCAL_MARKER));
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Dirty);
}

#[test]
fn executor_writes_inline_conflict_markers_and_marks_entity_conflicted() {
    let fixture = HydrationFixture::new();
    let mut store = fixture.store(HydrationState::Hydrated);
    let old = rendered_entity("Old body.");
    store
        .save_shadow(&fixture.mount_id, old.shadow.clone())
        .expect("save old shadow");
    fixture.write_raw("---\nafs:\n  id: page-1\n  type: page\ntitle: Roadmap\n---\nLocal edit.\n");
    let new = rendered_entity("Remote body.");
    let source = FakeHydrationSource::with_entity("page-1", new.clone());

    let mut executor = HydrationExecutor::new(&mut store, &source);
    let outcome = executor
        .hydrate_request(fixture.request())
        .expect("materialize conflict");

    assert_eq!(outcome, HydrationOutcome::SkippedDirty);
    let contents = fs::read_to_string(fixture.page_path()).expect("local file");
    assert!(contents.contains("Local edit."));
    assert!(contents.contains("Remote body."));
    assert!(contents.contains(CONFLICT_LOCAL_MARKER));
    assert!(contents.contains(CONFLICT_SEPARATOR_MARKER));
    assert!(contents.contains(CONFLICT_REMOTE_MARKER));
    assert!(has_unresolved_conflict_markers(&contents));
    assert!(!fixture.page_path().with_extension("remote.md").exists());
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Conflicted);
    assert_eq!(entity.remote_edited_at, new.remote_edited_at);
    let shadow = store
        .load_shadow(&fixture.mount_id, &fixture.remote_id)
        .expect("load shadow");
    assert_eq!(shadow.body_hash, new.shadow.body_hash);
}

#[test]
fn executor_leaves_inline_conflict_unchanged_when_remote_changes_again() {
    let fixture = HydrationFixture::new();
    let mut store = fixture.store(HydrationState::Hydrated);
    let old = rendered_entity("Old body.");
    store
        .save_shadow(&fixture.mount_id, old.shadow.clone())
        .expect("save old shadow");
    fixture.write_raw("---\nafs:\n  id: page-1\n  type: page\ntitle: Roadmap\n---\nLocal edit.\n");
    let new = rendered_entity("Remote body.");
    let source = FakeHydrationSource::with_entity("page-1", new.clone());

    let mut executor = HydrationExecutor::new(&mut store, &source);
    executor
        .hydrate_request(fixture.request())
        .expect("materialize conflict");
    drop(executor);
    let conflicted_contents =
        fs::read_to_string(fixture.page_path()).expect("conflict file contents");

    let changed_source =
        FakeHydrationSource::with_entity("page-1", rendered_entity("Remote body v2."));
    let mut executor = HydrationExecutor::new(&mut store, &changed_source);
    let outcome = executor
        .hydrate_request(fixture.request())
        .expect("skip unresolved conflict");

    assert_eq!(outcome, HydrationOutcome::SkippedDirty);
    assert_eq!(
        fs::read_to_string(fixture.page_path()).expect("conflict file contents"),
        conflicted_contents
    );
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Conflicted);
}

#[test]
fn executor_skips_frontmatter_only_edit_even_when_body_matches_shadow() {
    let fixture = HydrationFixture::new();
    let mut store = fixture.store(HydrationState::Hydrated);
    let old = rendered_entity("Old body.");
    store
        .save_shadow(&fixture.mount_id, old.shadow.clone())
        .expect("save old shadow");
    fixture.write_markdown(&CanonicalDocument::new(
        "afs:\n  id: page-1\n  type: page\ntitle: Updated Roadmap\n",
        old.document.body.clone(),
    ));
    let source = FakeHydrationSource::with_entity("page-1", old);

    let mut executor = HydrationExecutor::new(&mut store, &source);
    let outcome = executor
        .hydrate_request(fixture.request())
        .expect("skip dirty file");

    assert_eq!(outcome, HydrationOutcome::SkippedDirty);
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Dirty);
}

#[test]
fn drain_queue_requeues_failed_source_request() {
    let fixture = HydrationFixture::new();
    let mut store = fixture.store(HydrationState::Stub);
    let source = FakeHydrationSource::failing("temporary fetch failure");
    let mut queue = HydrationQueue::new();
    queue.queue_request(fixture.request());

    let mut executor = HydrationExecutor::new(&mut store, &source);
    let error = executor
        .drain_queue(&mut queue)
        .expect_err("source failure");

    assert_eq!(
        error,
        AfsError::InvalidState("temporary fetch failure".to_string())
    );
    assert_eq!(queue.len(), 1);
    assert_eq!(
        queue.peek_ready().expect("requeued").remote_id,
        fixture.remote_id
    );
}

#[test]
fn drain_queue_counts_hydrated_and_dirty_skips() {
    let fixture = HydrationFixture::new();
    let mut store = fixture.store(HydrationState::Stub);
    let dirty = EntityRecord::new(
        fixture.mount_id.clone(),
        RemoteId::new("page-2"),
        EntityKind::Page,
        "Dirty",
        "Dirty.md",
    )
    .with_hydration(HydrationState::Hydrated);
    store.save_entity(dirty).expect("save dirty entity");
    let old_dirty = rendered_entity_for("page-2", "Old dirty body.");
    store
        .save_shadow(&fixture.mount_id, old_dirty.shadow)
        .expect("save dirty shadow");
    fixture.write_raw_at(
        "Dirty.md",
        "---\nafs:\n  id: page-2\n  type: page\ntitle: Dirty\n---\nLocal edit.\n",
    );

    let mut source = FakeHydrationSource::new();
    source.insert("page-1", rendered_entity("Remote body."));
    source.insert(
        "page-2",
        rendered_entity_for("page-2", "Remote dirty body."),
    );
    let mut queue = HydrationQueue::new();
    queue.queue_request(fixture.request());
    queue.queue_request(HydrationRequest::new(
        fixture.mount_id.clone(),
        RemoteId::new("page-2"),
        fixture.root.join("Dirty.md"),
        HydrationState::Hydrated,
        HydrationReason::StubRead,
    ));

    let mut executor = HydrationExecutor::new(&mut store, &source);
    let report = executor.drain_queue(&mut queue).expect("drain queue");

    assert_eq!(report.hydrated, 1);
    assert_eq!(report.skipped_dirty, 1);
    assert!(queue.is_empty());
}

#[derive(Clone, Debug)]
struct HydrationFixture {
    root: PathBuf,
    mount_id: MountId,
    remote_id: RemoteId,
}

impl HydrationFixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "afs-hydration-executor-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("fixture root");

        Self {
            root,
            mount_id: MountId::new("notion-main"),
            remote_id: RemoteId::new("page-1"),
        }
    }

    fn store(&self, hydration: HydrationState) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(MountConfig::new(
                self.mount_id.clone(),
                "notion",
                self.root.clone(),
            ))
            .expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    self.mount_id.clone(),
                    self.remote_id.clone(),
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap/page.md",
                )
                .with_hydration(hydration),
            )
            .expect("save entity");
        store
    }

    fn request(&self) -> HydrationRequest {
        HydrationRequest::new(
            self.mount_id.clone(),
            self.remote_id.clone(),
            self.page_path(),
            HydrationState::Hydrated,
            HydrationReason::StubRead,
        )
    }

    fn page_path(&self) -> PathBuf {
        self.root.join("Roadmap/page.md")
    }

    fn write_stub(&self) {
        self.write_raw(&format!(
            "---\nafs:\n  id: page-1\n  type: page\ntitle: Roadmap\n---\n{}\n",
            CanonicalDocument::STUB_MARKER
        ));
    }

    fn write_markdown(&self, document: &CanonicalDocument) {
        self.write_raw(&render_canonical_markdown(document));
    }

    fn write_raw(&self, contents: &str) {
        self.write_raw_at("Roadmap/page.md", contents);
    }

    fn write_raw_at(&self, relative_path: &str, contents: &str) {
        let path = self.root.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent");
        }
        fs::write(path, contents).expect("fixture file");
    }
}

impl Drop for HydrationFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Clone, Debug, Default)]
struct FakeHydrationSource {
    entities: BTreeMap<RemoteId, HydratedEntity>,
    failure: Option<String>,
    request_paths: RefCell<Vec<PathBuf>>,
}

impl FakeHydrationSource {
    fn new() -> Self {
        Self::default()
    }

    fn with_entity(remote_id: &str, entity: HydratedEntity) -> Self {
        let mut source = Self::new();
        source.insert(remote_id, entity);
        source
    }

    fn failing(message: &str) -> Self {
        Self {
            entities: BTreeMap::new(),
            failure: Some(message.to_string()),
            request_paths: RefCell::new(Vec::new()),
        }
    }

    fn insert(&mut self, remote_id: &str, entity: HydratedEntity) {
        self.entities.insert(RemoteId::new(remote_id), entity);
    }

    fn request_paths(&self) -> Vec<PathBuf> {
        self.request_paths.borrow().clone()
    }
}

impl HydrationSource for FakeHydrationSource {
    fn fetch_render(&self, request: &HydrationRequest) -> AfsResult<HydratedEntity> {
        if let Some(message) = &self.failure {
            return Err(AfsError::InvalidState(message.clone()));
        }
        self.request_paths.borrow_mut().push(request.path.clone());

        self.entities
            .get(&request.remote_id)
            .cloned()
            .ok_or_else(|| AfsError::InvalidState("missing fake entity".to_string()))
    }
}

fn rendered_entity(body: &str) -> HydratedEntity {
    rendered_entity_for("page-1", body)
}

fn rendered_entity_with_sync(
    body: &str,
    synced_at: &str,
    remote_edited_at: &str,
) -> HydratedEntity {
    let mut entity = rendered_entity(body);
    entity.document.frontmatter = format!(
        "afs:\n  id: {}\n  type: page\n  synced_at: \"{synced_at}\"\n  remote_edited_at: \"{remote_edited_at}\"\ntitle: Roadmap\n",
        entity.shadow.entity_id.0
    );
    entity.shadow.frontmatter = entity.document.frontmatter.clone();
    entity
}

fn rendered_entity_for(remote_id: &str, body: &str) -> HydratedEntity {
    let body = format!("# Roadmap\n\n{body}\n");
    let document = CanonicalDocument::new(
        format!("afs:\n  id: {remote_id}\n  type: page\ntitle: Roadmap\n"),
        body.clone(),
    );
    let shadow = ShadowDocument::from_synced_body(
        RemoteId::new(remote_id),
        body,
        7,
        [
            RemoteId::new(format!("{remote_id}-heading")),
            RemoteId::new(format!("{remote_id}-body")),
        ],
    )
    .expect("shadow")
    .with_frontmatter(document.frontmatter.clone());

    HydratedEntity {
        document,
        shadow,
        remote_edited_at: Some("2026-06-11T00:00:00Z".to_string()),
        assets: Vec::new(),
    }
}
