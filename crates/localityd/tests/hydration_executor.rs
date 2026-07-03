use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use locality_core::canonical::render_canonical_markdown;
use locality_core::conflict::{
    CONFLICT_LOCAL_MARKER, CONFLICT_REMOTE_MARKER, CONFLICT_SEPARATOR_MARKER,
    has_unresolved_conflict_markers,
};
use locality_core::hydration::{HydrationReason, HydrationRequest};
use locality_core::model::{CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId};
use locality_core::shadow::{ShadowDocument, segment_markdown_body};
use locality_core::{LocalityError, LocalityResult};
use locality_store::{
    EntityRecord, EntityRepository, FreshnessStateRecord, FreshnessStateRepository,
    InMemoryStateStore, MountConfig, MountRepository, ShadowRepository,
};
use localityd::hydration::{
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
fn executor_removes_clean_stub_when_remote_is_not_found() {
    let fixture = HydrationFixture::new();
    let mut store = fixture.store(HydrationState::Stub);
    fixture.write_stub();
    let source = FakeHydrationSource::remote_not_found();

    let mut executor = HydrationExecutor::new(&mut store, &source);
    let outcome = executor
        .hydrate_request(fixture.request())
        .expect("hydrate deleted stub");

    assert_eq!(outcome, HydrationOutcome::RemoteDeleted);
    assert!(!fixture.page_path().exists());
    assert!(
        store
            .get_entity(&fixture.mount_id, &fixture.remote_id)
            .expect("get deleted entity")
            .is_none()
    );
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
                locality_core::freshness::FreshnessTier::Hot,
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
fn live_mode_fast_forward_keeps_remote_hint_when_new_version_renders_same_tree() {
    let fixture = HydrationFixture::new();
    let mut store = fixture.store(HydrationState::Hydrated);
    let old = rendered_entity_with_sync(
        "Old body.",
        "2026-06-11T00:00:00Z",
        "2026-06-11T00:00:00Z",
    );
    store
        .save_shadow(&fixture.mount_id, old.shadow.clone())
        .expect("save old shadow");
    store
        .save_freshness_state(
            FreshnessStateRecord::new(
                fixture.mount_id.clone(),
                fixture.remote_id.clone(),
                locality_core::freshness::FreshnessTier::Immediate,
            )
            .remote_hint_pending(true),
        )
        .expect("save freshness");
    let mut entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    entity.remote_edited_at = Some("2026-06-11T00:00:00Z".to_string());
    entity.content_hash = Some(old.shadow.body_hash.clone());
    store.save_entity(entity).expect("save entity");
    fixture.write_markdown(&old.document);

    let mut stale_render = rendered_entity_with_sync(
        "Old body.",
        "2026-07-03T08:48:00.000Z",
        "2026-07-03T08:48:00.000Z",
    );
    stale_render.remote_edited_at = Some("2026-07-03T08:48:00.000Z".to_string());
    let source = FakeHydrationSource::with_entity("page-1", stale_render);
    let mut request = fixture.request();
    request.reason = HydrationReason::LiveModeRemoteFastForward;

    let mut executor = HydrationExecutor::new(&mut store, &source);
    let outcome = executor
        .hydrate_request(request)
        .expect("hydrate likely stale live render");

    assert_eq!(outcome, HydrationOutcome::Hydrated);
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(
        entity.remote_edited_at.as_deref(),
        Some("2026-06-11T00:00:00Z")
    );
    assert_eq!(
        store
            .load_shadow(&fixture.mount_id, &fixture.remote_id)
            .expect("load shadow"),
        old.shadow
    );
    assert!(
        store
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
        path: PathBuf::from(".loc/media/Roadmap/image-1.png"),
        bytes: b"image-bytes".to_vec(),
        media: None,
    });
    let source = FakeHydrationSource::with_entity("page-1", rendered);

    let mut executor = HydrationExecutor::new(&mut store, &source);
    executor
        .hydrate_request(fixture.request())
        .expect("hydrate request");

    assert_eq!(
        fs::read(fixture.root.join(".loc/media/Roadmap/image-1.png")).expect("asset"),
        b"image-bytes"
    );
}

#[test]
fn executor_writes_absolute_media_hrefs_under_output_root() {
    let fixture = HydrationFixture::new();
    let mut store = fixture.store(HydrationState::Stub);
    fixture.write_stub();
    let mut rendered = rendered_entity("![Image](../.loc/media/Roadmap/image-1.png)");
    rendered.assets.push(HydratedAsset {
        path: PathBuf::from(".loc/media/Roadmap/image-1.png"),
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
    let expected_href = output_root
        .join(".loc/media/Roadmap/image-1.png")
        .to_string_lossy()
        .replace('\\', "/");
    assert!(contents.contains(&format!("![Image]({expected_href})")));
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
        path: PathBuf::from(".loc/media/../outside.png"),
        bytes: b"image-bytes".to_vec(),
        media: None,
    });
    let source = FakeHydrationSource::with_entity("page-1", rendered);

    let mut executor = HydrationExecutor::new(&mut store, &source);
    let error = executor
        .hydrate_request(fixture.request())
        .expect_err("unsafe asset path should fail");

    assert!(matches!(error, LocalityError::InvalidState(_)));
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
    fixture.write_raw("---\nloc:\n  id: page-1\n  type: page\ntitle: Roadmap\n---\nLocal edit.\n");
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
    fixture.write_raw("---\nloc:\n  id: page-1\n  type: page\ntitle: Roadmap\n---\nLocal edit.\n");
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
fn executor_merges_non_overlapping_dirty_and_remote_changes_without_conflict_markers() {
    let fixture = HydrationFixture::new();
    let mut store = fixture.store(HydrationState::Hydrated);
    let old = rendered_entity("Intro.\n\nOld middle.\n\nFooter.");
    store
        .save_shadow(&fixture.mount_id, old.shadow.clone())
        .expect("save old shadow");
    store
        .save_freshness_state(
            FreshnessStateRecord::new(
                fixture.mount_id.clone(),
                fixture.remote_id.clone(),
                locality_core::freshness::FreshnessTier::Hot,
            )
            .remote_hint_pending(true),
        )
        .expect("save freshness");
    let local_document = CanonicalDocument::new(
        old.document.frontmatter.clone(),
        old.document.body.replace("Old middle.", "Local middle."),
    );
    fixture.write_markdown(&local_document);
    let new = rendered_entity("Remote intro.\n\nOld middle.\n\nFooter.");
    let source = FakeHydrationSource::with_entity("page-1", new.clone());

    let mut executor = HydrationExecutor::new(&mut store, &source);
    let outcome = executor
        .hydrate_request(fixture.request())
        .expect("merge non-overlapping drift");

    assert_eq!(outcome, HydrationOutcome::SkippedDirty);
    let contents = fs::read_to_string(fixture.page_path()).expect("merged file");
    assert!(contents.contains("Remote intro."), "{contents}");
    assert!(contents.contains("Local middle."), "{contents}");
    assert!(!has_unresolved_conflict_markers(&contents), "{contents}");
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Dirty);
    assert_eq!(entity.remote_edited_at, new.remote_edited_at);
    assert!(
        !store
            .get_freshness_state(&fixture.mount_id, &fixture.remote_id)
            .expect("get freshness")
            .expect("freshness")
            .remote_hint_pending
    );
}

#[test]
fn executor_writes_inline_conflict_markers_and_marks_entity_conflicted() {
    let fixture = HydrationFixture::new();
    let mut store = fixture.store(HydrationState::Hydrated);
    let old = rendered_entity("Old body.");
    store
        .save_shadow(&fixture.mount_id, old.shadow.clone())
        .expect("save old shadow");
    fixture.write_raw("---\nloc:\n  id: page-1\n  type: page\ntitle: Roadmap\n---\nLocal edit.\n");
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
    fixture.write_raw("---\nloc:\n  id: page-1\n  type: page\ntitle: Roadmap\n---\nLocal edit.\n");
    let new = rendered_entity("Remote body.");
    let source = FakeHydrationSource::with_entity("page-1", new.clone());

    let mut executor = HydrationExecutor::new(&mut store, &source);
    executor
        .hydrate_request(fixture.request())
        .expect("materialize conflict");
    drop(executor);
    let conflicted_contents =
        fs::read_to_string(fixture.page_path()).expect("conflict file contents");

    let mut changed = rendered_entity_with_sync(
        "Remote body v2.",
        "2026-06-12T00:00:00Z",
        "2026-06-12T00:00:00Z",
    );
    changed.remote_edited_at = Some("2026-06-12T00:00:00Z".to_string());
    let changed_source = FakeHydrationSource::with_entity("page-1", changed);
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
fn executor_refreshes_inline_conflict_when_same_remote_version_shadow_drifted() {
    let fixture = HydrationFixture::new();
    let version = "2026-06-11T00:00:00Z";
    let mut store = fixture.store(HydrationState::Conflicted);
    let mut stale = rendered_entity_with_sync("Old body.", version, version);
    stale.remote_edited_at = Some(version.to_string());
    store
        .save_shadow(&fixture.mount_id, stale.shadow.clone())
        .expect("save stale shadow");
    let mut entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    entity.remote_edited_at = Some(version.to_string());
    entity.content_hash = Some(stale.shadow.body_hash.clone());
    store.save_entity(entity).expect("save entity");
    fixture.write_raw(concat!(
        "---\n",
        "loc:\n",
        "  id: page-1\n",
        "  type: page\n",
        "  synced_at: \"2026-06-11T00:00:00Z\"\n",
        "  remote_edited_at: \"2026-06-11T00:00:00Z\"\n",
        "title: Roadmap\n",
        "---\n",
        "<<<<<<< LOCAL\n",
        "# Roadmap\n\n",
        "Local edit.\n",
        "=======\n",
        "# Roadmap\n\n",
        "Old body.\n",
        ">>>>>>> REMOTE\n",
    ));
    let mut fresh = rendered_entity_with_sync("Remote body.\n\n---", version, version);
    fresh.remote_edited_at = Some(version.to_string());
    let source = FakeHydrationSource::with_entity("page-1", fresh.clone());

    let mut executor = HydrationExecutor::new(&mut store, &source);
    let outcome = executor
        .hydrate_request(fixture.request())
        .expect("refresh stale conflict");

    assert_eq!(outcome, HydrationOutcome::SkippedDirty);
    let contents = fs::read_to_string(fixture.page_path()).expect("conflict file contents");
    assert!(contents.contains("Local edit."), "{contents}");
    assert!(contents.contains("Remote body.\n\n---"), "{contents}");
    assert!(has_unresolved_conflict_markers(&contents), "{contents}");
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Conflicted);
    assert_eq!(entity.remote_edited_at.as_deref(), Some(version));
    let shadow = store
        .load_shadow(&fixture.mount_id, &fixture.remote_id)
        .expect("load shadow");
    assert_eq!(shadow.body_hash, fresh.shadow.body_hash);
}

#[test]
fn executor_compacts_same_version_inline_conflict_when_shadow_is_current() {
    let fixture = HydrationFixture::new();
    let version = "2026-06-11T00:00:00Z";
    let mut store = fixture.store(HydrationState::Conflicted);
    let mut fresh =
        rendered_entity_with_sync("Shared before.\n\n---\n\nShared after.", version, version);
    fresh.remote_edited_at = Some(version.to_string());
    store
        .save_shadow(&fixture.mount_id, fresh.shadow.clone())
        .expect("save current shadow");
    let mut entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    entity.remote_edited_at = Some(version.to_string());
    entity.content_hash = Some(fresh.shadow.body_hash.clone());
    store.save_entity(entity).expect("save entity");
    fixture.write_raw(concat!(
        "---\n",
        "loc:\n",
        "  id: page-1\n",
        "  type: page\n",
        "  synced_at: \"2026-06-11T00:00:00Z\"\n",
        "  remote_edited_at: \"2026-06-11T00:00:00Z\"\n",
        "title: Roadmap\n",
        "---\n",
        "<<<<<<< LOCAL\n",
        "# Roadmap\n\n",
        "Shared before.\n\n",
        "Shared after.\n",
        "=======\n",
        "# Roadmap\n\n",
        "Shared before.\n\n",
        "---\n\n",
        "Shared after.\n",
        ">>>>>>> REMOTE\n",
    ));
    let source = FakeHydrationSource::with_entity("page-1", fresh);

    let mut executor = HydrationExecutor::new(&mut store, &source);
    let outcome = executor
        .hydrate_request(fixture.request())
        .expect("compact same-version conflict");

    assert_eq!(outcome, HydrationOutcome::SkippedDirty);
    let contents = fs::read_to_string(fixture.page_path()).expect("conflict file contents");
    assert_eq!(
        contents.matches(CONFLICT_LOCAL_MARKER).count(),
        1,
        "{contents}"
    );
    assert!(
        contents.contains(concat!(
            "# Roadmap\n\n",
            "Shared before.\n\n",
            "<<<<<<< LOCAL\n",
            "=======\n",
            "---\n\n",
            ">>>>>>> REMOTE\n",
            "Shared after.\n",
        )),
        "{contents}"
    );
    assert!(!contents.contains("<<<<<<< LOCAL\n# Roadmap"), "{contents}");
}

#[test]
fn executor_cleans_same_version_conflict_when_local_side_retains_current_blocks() {
    let fixture = HydrationFixture::new();
    let version = "2026-06-11T00:00:00Z";
    let mut store = fixture.store(HydrationState::Conflicted);
    let mut fresh = rendered_entity_with_sync("---<br>---", version, version);
    fresh.remote_edited_at = Some(version.to_string());
    store
        .save_shadow(&fixture.mount_id, fresh.shadow.clone())
        .expect("save current shadow");
    let mut entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    entity.remote_edited_at = Some(version.to_string());
    entity.content_hash = Some(fresh.shadow.body_hash.clone());
    store.save_entity(entity).expect("save entity");
    fixture.write_raw(concat!(
        "---\n",
        "loc:\n",
        "  id: page-1\n",
        "  type: page\n",
        "  synced_at: \"2026-06-11T00:00:00Z\"\n",
        "  remote_edited_at: \"2026-06-11T00:00:00Z\"\n",
        "title: Roadmap\n",
        "---\n",
        "<<<<<<< LOCAL\n",
        "# Roadmap\n\n",
        "---\n\n",
        "---\n",
        "=======\n",
        "# Roadmap\n\n",
        "---<br>---\n",
        ">>>>>>> REMOTE\n",
    ));
    let source = FakeHydrationSource::with_entity("page-1", fresh);

    let mut executor = HydrationExecutor::new(&mut store, &source);
    let outcome = executor
        .hydrate_request(fixture.request())
        .expect("clean same-version local-only conflict");

    assert_eq!(outcome, HydrationOutcome::SkippedDirty);
    let contents = fs::read_to_string(fixture.page_path()).expect("conflict file contents");
    assert!(!has_unresolved_conflict_markers(&contents), "{contents}");
    assert!(contents.contains("# Roadmap\n\n---\n\n---\n"), "{contents}");
    assert!(!contents.contains("---<br>---"), "{contents}");
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Dirty);
}

#[test]
fn executor_refreshes_nested_inline_conflict_when_shadow_is_current() {
    let fixture = HydrationFixture::new();
    let version = "2026-06-11T00:00:00Z";
    let mut store = fixture.store(HydrationState::Conflicted);
    let mut fresh = rendered_entity_with_sync("Remote body.\n\n---", version, version);
    fresh.remote_edited_at = Some(version.to_string());
    store
        .save_shadow(&fixture.mount_id, fresh.shadow.clone())
        .expect("save current shadow");
    let mut entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    entity.remote_edited_at = Some(version.to_string());
    entity.content_hash = Some(fresh.shadow.body_hash.clone());
    store.save_entity(entity).expect("save entity");
    fixture.write_raw(concat!(
        "---\n",
        "loc:\n",
        "  id: page-1\n",
        "  type: page\n",
        "  synced_at: \"2026-06-11T00:00:00Z\"\n",
        "  remote_edited_at: \"2026-06-11T00:00:00Z\"\n",
        "title: Roadmap\n",
        "---\n",
        "<<<<<<< LOCAL\n",
        "<<<<<<< LOCAL\n",
        "# Roadmap\n\n",
        "Local edit.\n",
        "=======\n",
        "# Roadmap\n\n",
        "Old body.\n",
        ">>>>>>> REMOTE\n",
        "=======\n",
        "# Roadmap\n\n",
        "Old body.\n",
        ">>>>>>> REMOTE\n",
    ));
    let source = FakeHydrationSource::with_entity("page-1", fresh.clone());

    let mut executor = HydrationExecutor::new(&mut store, &source);
    let outcome = executor
        .hydrate_request(fixture.request())
        .expect("refresh nested stale conflict");

    assert_eq!(outcome, HydrationOutcome::SkippedDirty);
    let contents = fs::read_to_string(fixture.page_path()).expect("conflict file contents");
    assert_eq!(
        contents.matches(CONFLICT_LOCAL_MARKER).count(),
        1,
        "{contents}"
    );
    assert!(contents.contains("Local edit."), "{contents}");
    assert!(contents.contains("Remote body.\n\n---"), "{contents}");
    assert!(has_unresolved_conflict_markers(&contents), "{contents}");
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
        "loc:\n  id: page-1\n  type: page\ntitle: Updated Roadmap\n",
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
        LocalityError::InvalidState("temporary fetch failure".to_string())
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
        "---\nloc:\n  id: page-2\n  type: page\ntitle: Dirty\n---\nLocal edit.\n",
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
            "loc-hydration-executor-{}-{unique}",
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
            "---\nloc:\n  id: page-1\n  type: page\ntitle: Roadmap\n---\n{}\n",
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
    failure: Option<LocalityError>,
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
            failure: Some(LocalityError::InvalidState(message.to_string())),
            request_paths: RefCell::new(Vec::new()),
        }
    }

    fn remote_not_found() -> Self {
        Self {
            entities: BTreeMap::new(),
            failure: Some(LocalityError::RemoteNotFound("missing page".to_string())),
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
    fn fetch_render(&self, request: &HydrationRequest) -> LocalityResult<HydratedEntity> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        self.request_paths.borrow_mut().push(request.path.clone());

        self.entities
            .get(&request.remote_id)
            .cloned()
            .ok_or_else(|| LocalityError::InvalidState("missing fake entity".to_string()))
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
        "loc:\n  id: {}\n  type: page\n  synced_at: \"{synced_at}\"\n  remote_edited_at: \"{remote_edited_at}\"\ntitle: Roadmap\n",
        entity.shadow.entity_id.0
    );
    entity.shadow.frontmatter = entity.document.frontmatter.clone();
    entity
}

fn rendered_entity_for(remote_id: &str, body: &str) -> HydratedEntity {
    let body = format!("# Roadmap\n\n{body}\n");
    let native_block_count = segment_markdown_body(&body, 7).len();
    let document = CanonicalDocument::new(
        format!("loc:\n  id: {remote_id}\n  type: page\ntitle: Roadmap\n"),
        body.clone(),
    );
    let shadow = ShadowDocument::from_synced_body(
        RemoteId::new(remote_id),
        body.clone(),
        7,
        (0..native_block_count).map(|index| RemoteId::new(format!("{remote_id}-{index}"))),
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
