use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_core::LocalityError;
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use locality_core::planner::{PropertyValue, PushOperation};
use locality_core::push::PushPipelineAction;
use locality_core::shadow::{MarkdownBlockKind, ShadowDocument};
use locality_core::validation::ValidationReport;
use locality_notion::media::sha256_hex;
use locality_store::{
    EntityRecord, EntityRepository, InMemoryStateStore, MountConfig, MountRepository,
    ProjectionMode, ShadowRepository, StoreError, VirtualMutationKind, VirtualMutationRecord,
    VirtualMutationRepository,
};
use localityd::execution::PushJob;
use localityd::push::{PushPrepareError, prepare_push};
use localityd::source::{LocalSourceValidator, SourcePushValidator, SourceValidationContext};
use localityd::virtual_fs::{virtual_fs_content_path, virtual_fs_content_root};
use serde_json::json;

#[test]
fn prepare_push_blocks_notion_schema_violation_for_existing_database_row() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("notion");
    fixture.save_database(&mut store);
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("row-1"),
                EntityKind::Page,
                "Existing task",
                "Tasks/existing-task.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save row");
    fixture.write_tasks_schema();
    let body = "# Notes\n\nExisting body.";
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("row-1"),
                body,
                9,
                [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
            )
            .expect("shadow")
            .with_frontmatter(row_frontmatter("Todo")),
        )
        .expect("save shadow");
    let path = fixture.write_raw(
        "Tasks/existing-task.md",
        &format!("---\n{}---\n{body}", row_frontmatter("Blocked")),
    );

    let prepared =
        prepare_push(&store, &job(path), None, &LocalSourceValidator).expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::FixValidation);
    assert!(prepared.pipeline.plan.is_none());
    assert_eq!(
        prepared.pipeline.validation.issues[0].code,
        "notion_schema_option_unknown"
    );
}

#[test]
fn prepare_push_plans_new_notion_database_from_schema_file_or_directory() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("notion");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("roadmap-page"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap/page.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save parent page");
    let schema = "loc:\n  type: notion_database_schema\ntitle: Project Tasks\ndata_sources:\n  - name: Tasks\n    properties:\n      Name:\n        type: title\n      Status:\n        type: select\n        options:\n          - name: Todo\n            color: gray\n";
    let schema_path = fixture.write_raw("Roadmap/Project Tasks/_schema.yaml", schema);

    for target in [schema_path, fixture.root.join("Roadmap/Project Tasks")] {
        let prepared = prepare_push(&store, &job(target), None, &LocalSourceValidator)
            .expect("prepare database create");

        assert_eq!(prepared.pipeline.action, PushPipelineAction::ConfirmPlan);
        assert!(prepared.pipeline.validation.is_clean());
        assert_eq!(
            prepared.pipeline.plan.expect("plan").operations,
            vec![PushOperation::CreateDatabase {
                parent_id: RemoteId::new("roadmap-page"),
                title: "Project Tasks".to_string(),
                schema: schema.to_string(),
                source_path: PathBuf::from("Roadmap/Project Tasks/_schema.yaml"),
            }]
        );
    }
}

#[test]
fn prepare_push_rejects_generated_or_unsupported_database_schema_before_plan() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("notion");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("roadmap-page"),
            EntityKind::Page,
            "Roadmap",
            "Roadmap/page.md",
        ))
        .expect("save parent page");
    let path = fixture.write_raw(
        "Roadmap/Tasks/_schema.yaml",
        "loc:\n  type: notion_database_schema\n  database_id: existing\ntitle: Tasks\ndata_sources: []\n",
    );

    let prepared = prepare_push(&store, &job(path), None, &LocalSourceValidator)
        .expect("prepare invalid database create");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::FixValidation);
    assert!(prepared.pipeline.plan.is_none());
    assert_eq!(
        prepared.pipeline.validation.issues[0].code,
        "notion_database_schema_has_remote_id"
    );
}

#[test]
fn prepare_push_never_treats_tracked_database_schema_as_a_new_database() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("notion");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("database-1"),
            EntityKind::Database,
            "Tasks",
            "Roadmap/Tasks",
        ))
        .expect("save tracked database");
    let path = fixture.write_raw(
        "Roadmap/Tasks/_schema.yaml",
        "loc:\n  type: notion_database_schema\ntitle: Cloned Tasks\ndata_sources:\n  - name: Rows\n    properties:\n      Name:\n        type: title\n",
    );

    let prepared = prepare_push(&store, &job(path), None, &LocalSourceValidator)
        .expect("prepare tracked schema");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::FixValidation);
    assert!(prepared.pipeline.plan.is_none());
    assert_eq!(
        prepared.pipeline.validation.issues[0].code,
        "notion_database_schema_read_only"
    );
}

#[test]
fn prepare_push_rejects_database_draft_over_a_tracked_page_directory() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("notion");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("page-1"),
            EntityKind::Page,
            "Tasks",
            "Roadmap/Tasks/page.md",
        ))
        .expect("save tracked page");
    let path = fixture.write_raw(
        "Roadmap/Tasks/_schema.yaml",
        "loc:\n  type: notion_database_schema\ntitle: Tasks\ndata_sources:\n  - name: Rows\n    properties:\n      Name:\n        type: title\n",
    );

    let prepared = prepare_push(&store, &job(path), None, &LocalSourceValidator)
        .expect("prepare conflicting database draft");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::FixValidation);
    assert!(prepared.pipeline.plan.is_none());
    assert_eq!(
        prepared.pipeline.validation.issues[0].code,
        "notion_database_draft_path_conflict"
    );
}

#[test]
fn prepare_push_leaves_non_notion_database_schema_validation_clean() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("fake");
    fixture.save_database(&mut store);
    let path = fixture.write_raw(
        "Tasks/new-task.md",
        "---\ntitle: New task\nUnexpected: value\n---\n# Notes\n",
    );

    let prepared =
        prepare_push(&store, &job(path), None, &LocalSourceValidator).expect("prepare push");

    assert!(prepared.pipeline.validation.is_clean());
    assert_eq!(prepared.pipeline.action, PushPipelineAction::ConfirmPlan);
    let plan = prepared.pipeline.plan.expect("plan");
    assert!(matches!(
        plan.operations[0],
        PushOperation::CreateEntity { .. }
    ));
}

#[test]
fn prepare_push_plans_content_cache_absolute_media_byte_update() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.virtual_store("notion");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap/page.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save entity");
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("page-1"),
                "![Image](../.loc/media/Roadmap/image-1.png)",
                8,
                [RemoteId::new("image-1")],
            )
            .expect("shadow"),
        )
        .expect("save shadow");
    let content_root = virtual_fs_content_root(&fixture.state_root, &fixture.mount_id);
    let media_path = PathBuf::from(".loc/media/Roadmap/image-1.png");
    fixture.write_virtual_media_manifest(&media_path, "image-1", b"original image bytes");
    fixture.write_virtual_media(&media_path, b"updated image bytes");
    let absolute_media = content_root.join(&media_path);
    fixture.write_virtual_page(
        "Roadmap/page.md",
        &canonical_markdown(
            "page-1",
            &format!("![Image]({})", markdown_href(&absolute_media)),
        ),
    );

    let prepared = prepare_push(
        &store,
        &job(fixture.root.join("Roadmap/page.md")),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare push");
    let plan = prepared.pipeline.plan.expect("plan");

    assert_eq!(plan.summary.media_updated, 1);
    assert_eq!(plan.summary.blocks_updated, 0);
    assert_eq!(
        plan.operations,
        vec![PushOperation::UpdateMedia {
            block_id: RemoteId::new("image-1"),
            local_path: media_path,
            caption: "Image".to_string(),
        }]
    );
}

#[test]
fn prepare_push_plans_media_update_with_escaped_parenthesized_href() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.virtual_store("notion");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap/page.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save entity");
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("page-1"),
                "![Image](../.loc/media/Roadmap/image-\\(1\\).png)",
                8,
                [RemoteId::new("image-1")],
            )
            .expect("shadow"),
        )
        .expect("save shadow");
    let content_root = virtual_fs_content_root(&fixture.state_root, &fixture.mount_id);
    let media_path = PathBuf::from(".loc/media/Roadmap/image-(1).png");
    fixture.write_virtual_media_manifest(&media_path, "image-1", b"original image bytes");
    fixture.write_virtual_media(&media_path, b"updated image bytes");
    let absolute_media = content_root.join(&media_path);
    let escaped_absolute_media = markdown_href(&absolute_media)
        .replace('(', "\\(")
        .replace(')', "\\)");
    fixture.write_virtual_page(
        "Roadmap/page.md",
        &canonical_markdown("page-1", &format!("![Image]({escaped_absolute_media})")),
    );

    let prepared = prepare_push(
        &store,
        &job(fixture.root.join("Roadmap/page.md")),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare push");
    let plan = prepared.pipeline.plan.expect("plan");

    assert_eq!(plan.summary.media_updated, 1);
    assert_eq!(plan.summary.blocks_updated, 0);
    assert_eq!(
        plan.operations,
        vec![PushOperation::UpdateMedia {
            block_id: RemoteId::new("image-1"),
            local_path: media_path,
            caption: "Image".to_string(),
        }]
    );
}

#[test]
fn prepare_push_plans_content_cache_absolute_file_media_byte_update() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.virtual_store("notion");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap/page.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save entity");
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("page-1"),
                "[Demo](../.loc/media/Roadmap/video-1.mp4)",
                8,
                [RemoteId::new("video-1")],
            )
            .expect("shadow"),
        )
        .expect("save shadow");
    let content_root = virtual_fs_content_root(&fixture.state_root, &fixture.mount_id);
    let media_path = PathBuf::from(".loc/media/Roadmap/video-1.mp4");
    fixture.write_virtual_media_manifest_with_kind(&media_path, "video", "video-1", b"old video");
    fixture.write_virtual_media(&media_path, b"new video");
    let absolute_media = content_root.join(&media_path);
    fixture.write_virtual_page(
        "Roadmap/page.md",
        &canonical_markdown(
            "page-1",
            &format!("[Demo]({})", markdown_href(&absolute_media)),
        ),
    );

    let prepared = prepare_push(
        &store,
        &job(fixture.root.join("Roadmap/page.md")),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare push");
    let plan = prepared.pipeline.plan.expect("plan");

    assert_eq!(plan.summary.media_updated, 1);
    assert_eq!(plan.summary.blocks_updated, 0);
    assert_eq!(
        plan.operations,
        vec![PushOperation::UpdateMedia {
            block_id: RemoteId::new("video-1"),
            local_path: media_path,
            caption: "Demo".to_string(),
        }]
    );
}

#[test]
fn prepare_push_blocks_rendered_child_page_link_move_before_apply() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("notion");
    let child_id = "11111111-1111-1111-1111-111111111111";
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new(child_id),
                EntityKind::Page,
                "Child Page",
                "Roadmap/child-page/page.md",
            )
            .with_hydration(HydrationState::Stub),
        )
        .expect("save child page");
    let child_link = "[Child Page](https://www.notion.so/11111111111111111111111111111111)";
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("page-1"),
                format!("Intro.\n\n{child_link}"),
                8,
                [RemoteId::new("paragraph-1"), RemoteId::new(child_id)],
            )
            .expect("shadow"),
        )
        .expect("save shadow");
    let path = fixture.write_page("Roadmap.md", &format!("{child_link}\n\nIntro."));

    let prepared =
        prepare_push(&store, &job(path), None, &LocalSourceValidator).expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::FixValidation);
    assert!(prepared.pipeline.plan.is_none());
    assert_eq!(
        prepared.pipeline.validation.issues[0].code,
        "notion_child_page_link_move_unsupported"
    );
}

#[test]
fn prepare_push_blocks_rendered_child_page_link_label_edit_before_apply() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("notion");
    let child_id = "11111111-1111-1111-1111-111111111111";
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new(child_id),
                EntityKind::Page,
                "Child Page",
                "Roadmap/child-page/page.md",
            )
            .with_hydration(HydrationState::Stub),
        )
        .expect("save child page");
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("page-1"),
                "[Child Page](https://www.notion.so/11111111111111111111111111111111)",
                8,
                [RemoteId::new(child_id)],
            )
            .expect("shadow"),
        )
        .expect("save shadow");
    let path = fixture.write_page(
        "Roadmap.md",
        "[Edited Child Page](https://www.notion.so/11111111111111111111111111111111)",
    );

    let prepared =
        prepare_push(&store, &job(path), None, &LocalSourceValidator).expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::FixValidation);
    assert!(prepared.pipeline.plan.is_none());
    assert_eq!(
        prepared.pipeline.validation.issues[0].code,
        "notion_child_page_link_edit_unsupported"
    );
}

#[test]
fn prepare_push_blocks_rendered_child_page_link_delete_before_apply() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("notion");
    let child_id = "11111111-1111-1111-1111-111111111111";
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new(child_id),
                EntityKind::Page,
                "Child Page",
                "Roadmap/child-page/page.md",
            )
            .with_hydration(HydrationState::Stub),
        )
        .expect("save child page");
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("page-1"),
                "Intro.\n\n[Child Page](https://www.notion.so/11111111111111111111111111111111)",
                8,
                [RemoteId::new("paragraph-1"), RemoteId::new(child_id)],
            )
            .expect("shadow"),
        )
        .expect("save shadow");
    let path = fixture.write_page("Roadmap.md", "Intro.");

    let prepared =
        prepare_push(&store, &job(path), None, &LocalSourceValidator).expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::FixValidation);
    assert!(prepared.pipeline.plan.is_none());
    assert_eq!(
        prepared.pipeline.validation.issues[0].code,
        "notion_child_page_link_delete_unsupported"
    );
}

#[test]
fn prepare_push_blocks_rendered_link_to_page_retarget_before_apply() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("notion");
    let original_link = "[Linked page](https://www.notion.so/11111111111111111111111111111111)";
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("page-1"),
                original_link,
                8,
                [RemoteId::new("link-to-page-1")],
            )
            .expect("shadow"),
        )
        .expect("save shadow");
    let path = fixture.write_page(
        "Roadmap.md",
        "[Linked page](https://www.notion.so/22222222222222222222222222222222)",
    );

    let prepared =
        prepare_push(&store, &job(path), None, &LocalSourceValidator).expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::FixValidation);
    assert!(prepared.pipeline.plan.is_none());
    assert_eq!(
        prepared.pipeline.validation.issues[0].code,
        "notion_link_to_page_retarget_unsupported"
    );
}

#[test]
fn prepare_push_allows_paragraph_link_labeled_like_link_to_page() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("notion");
    let original_link = "[Linked page](https://www.notion.so/11111111111111111111111111111111)";
    let mut shadow = ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        original_link,
        8,
        [RemoteId::new("paragraph-1")],
    )
    .expect("shadow");
    shadow.blocks[0].native_kind = Some("paragraph".to_string());
    store
        .save_shadow(&fixture.mount_id, shadow)
        .expect("save shadow");
    let path = fixture.write_page(
        "Roadmap.md",
        "[Linked page](https://www.notion.so/22222222222222222222222222222222)",
    );

    let prepared =
        prepare_push(&store, &job(path), None, &LocalSourceValidator).expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::ConfirmPlan);
    assert!(prepared.pipeline.validation.is_clean());
    let plan = prepared.pipeline.plan.expect("plan");
    assert_eq!(plan.summary.blocks_updated, 1);
    assert_eq!(
        plan.operations,
        vec![PushOperation::UpdateBlock {
            block_id: RemoteId::new("paragraph-1"),
            content: "[Linked page](https://www.notion.so/22222222222222222222222222222222)"
                .to_string(),
        }]
    );
}

#[test]
fn prepare_push_blocks_rendered_link_preview_edit_before_apply() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("notion");
    let mut shadow = ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        "[Preview](https://example.com/preview)",
        8,
        [RemoteId::new("link-preview-1")],
    )
    .expect("shadow");
    shadow.blocks[0].native_kind = Some("link_preview".to_string());
    store
        .save_shadow(&fixture.mount_id, shadow)
        .expect("save shadow");
    let path = fixture.write_page("Roadmap.md", "[Changed](https://example.com/preview)");

    let prepared =
        prepare_push(&store, &job(path), None, &LocalSourceValidator).expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::FixValidation);
    assert!(prepared.pipeline.plan.is_none());
    assert_eq!(
        prepared.pipeline.validation.issues[0].code,
        "notion_link_preview_edit_unsupported"
    );
}

#[test]
fn prepare_push_blocks_rendered_link_preview_move_before_apply() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("notion");
    let link = "[Preview](https://example.com/preview)";
    let mut shadow = ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        format!("Intro.\n\n{link}"),
        8,
        [
            RemoteId::new("paragraph-1"),
            RemoteId::new("link-preview-1"),
        ],
    )
    .expect("shadow");
    shadow.blocks[1].native_kind = Some("link_preview".to_string());
    store
        .save_shadow(&fixture.mount_id, shadow)
        .expect("save shadow");
    let path = fixture.write_page("Roadmap.md", &format!("{link}\n\nIntro."));

    let prepared =
        prepare_push(&store, &job(path), None, &LocalSourceValidator).expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::FixValidation);
    assert!(prepared.pipeline.plan.is_none());
    assert_eq!(
        prepared.pipeline.validation.issues[0].code,
        "notion_link_preview_move_unsupported"
    );
}

#[test]
fn prepare_push_blocks_rendered_link_preview_delete_before_apply() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("notion");
    let mut shadow = ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        "Intro.\n\n[Preview](https://example.com/preview)",
        8,
        [
            RemoteId::new("paragraph-1"),
            RemoteId::new("link-preview-1"),
        ],
    )
    .expect("shadow");
    shadow.blocks[1].native_kind = Some("link_preview".to_string());
    store
        .save_shadow(&fixture.mount_id, shadow)
        .expect("save shadow");
    let path = fixture.write_page("Roadmap.md", "Intro.");

    let prepared =
        prepare_push(&store, &job(path), None, &LocalSourceValidator).expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::FixValidation);
    assert!(prepared.pipeline.plan.is_none());
    assert_eq!(
        prepared.pipeline.validation.issues[0].code,
        "notion_link_preview_delete_unsupported"
    );
}

#[test]
fn prepare_push_blocks_google_docs_inline_image_move_before_apply() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("google-docs");
    let image = "![A circle with logo written in the center](https://example.test/circle.png)";
    let mut shadow = ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        format!("Intro.\n\n{image}"),
        8,
        [RemoteId::new("page-1:1:8"), RemoteId::new("page-1:8:9")],
    )
    .expect("shadow");
    shadow.blocks[1].native_kind = Some("google_docs_inline_object".to_string());
    store
        .save_shadow(&fixture.mount_id, shadow)
        .expect("save shadow");
    let path = fixture.write_page("Roadmap.md", &format!("{image}\n\nIntro."));

    let prepared =
        prepare_push(&store, &job(path), None, &LocalSourceValidator).expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::FixValidation);
    assert!(prepared.pipeline.plan.is_none());
    assert_eq!(
        prepared.pipeline.validation.issues[0].code,
        "google_docs_inline_object_move_unsupported"
    );
}

#[test]
fn prepare_push_blocks_google_docs_table_move_before_apply() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("google-docs");
    let table = "| Pet | Age |\n| --- | --- |\n| Luna | 4 |";
    let mut shadow = ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        format!("Intro.\n\n{table}"),
        8,
        [RemoteId::new("page-1:1:8"), RemoteId::new("page-1:8:40")],
    )
    .expect("shadow");
    shadow.blocks[1].native_kind = Some("google_docs_table".to_string());
    store
        .save_shadow(&fixture.mount_id, shadow)
        .expect("save shadow");
    let path = fixture.write_page("Roadmap.md", &format!("{table}\n\nIntro."));

    let prepared =
        prepare_push(&store, &job(path), None, &LocalSourceValidator).expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::FixValidation);
    assert!(prepared.pipeline.plan.is_none());
    assert_eq!(
        prepared.pipeline.validation.issues[0].code,
        "google_docs_table_move_unsupported"
    );
}

#[test]
fn prepare_push_blocks_notion_table_width_change_before_apply() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("notion");
    let body = "| Name | Status |\n| --- | --- |\n| Old task | Todo |";
    let mut shadow = ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        body,
        8,
        [RemoteId::new("table-1")],
    )
    .expect("shadow");
    shadow.blocks[0].kind = MarkdownBlockKind::TableWithRows {
        row_ids: vec![RemoteId::new("row-1")],
        has_column_header: true,
        has_row_header: false,
    };
    store
        .save_shadow(&fixture.mount_id, shadow)
        .expect("save shadow");
    let path = fixture.write_page(
        "Roadmap.md",
        "| Name | Status | Owner |\n| --- | --- | --- |\n| Old task | Todo | Alex |",
    );

    let prepared =
        prepare_push(&store, &job(path), None, &LocalSourceValidator).expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::FixValidation);
    assert!(prepared.pipeline.plan.is_none());
    assert_eq!(
        prepared.pipeline.validation.issues[0].code,
        "notion_table_width_change_unsupported"
    );
}

#[test]
fn prepare_push_blocks_notion_table_header_mode_change_before_apply() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("notion");
    let body = "|  |  |\n| --- | --- |\n| Old task | Todo |";
    let mut shadow = ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        body,
        8,
        [RemoteId::new("table-1")],
    )
    .expect("shadow");
    shadow.blocks[0].kind = MarkdownBlockKind::TableWithRows {
        row_ids: vec![RemoteId::new("row-1")],
        has_column_header: false,
        has_row_header: false,
    };
    store
        .save_shadow(&fixture.mount_id, shadow)
        .expect("save shadow");
    let path = fixture.write_page(
        "Roadmap.md",
        "| Name | Status |\n| --- | --- |\n| Old task | Todo |",
    );

    let prepared =
        prepare_push(&store, &job(path), None, &LocalSourceValidator).expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::FixValidation);
    assert!(prepared.pipeline.plan.is_none());
    assert_eq!(
        prepared.pipeline.validation.issues[0].code,
        "notion_table_header_mode_change_unsupported"
    );
}

#[test]
fn prepare_push_blocks_notion_table_middle_row_delete_before_apply() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("notion");
    let body =
        "| Name | Status |\n| --- | --- |\n| Alpha | Todo |\n| Beta | Doing |\n| Gamma | Done |";
    let mut shadow = ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        body,
        8,
        [RemoteId::new("table-1")],
    )
    .expect("shadow");
    shadow.blocks[0].kind = MarkdownBlockKind::TableWithRows {
        row_ids: vec![
            RemoteId::new("header-row"),
            RemoteId::new("alpha-row"),
            RemoteId::new("beta-row"),
            RemoteId::new("gamma-row"),
        ],
        has_column_header: true,
        has_row_header: false,
    };
    store
        .save_shadow(&fixture.mount_id, shadow)
        .expect("save shadow");
    let path = fixture.write_page(
        "Roadmap.md",
        "| Name | Status |\n| --- | --- |\n| Alpha | Todo |\n| Gamma | Done |",
    );

    let prepared =
        prepare_push(&store, &job(path), None, &LocalSourceValidator).expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::FixValidation);
    assert!(prepared.pipeline.plan.is_none());
    assert_eq!(
        prepared.pipeline.validation.issues[0].code,
        "notion_table_middle_row_delete_unsupported"
    );
}

#[test]
fn prepare_push_allows_notion_table_trailing_row_delete_before_apply() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("notion");
    let body =
        "| Name | Status |\n| --- | --- |\n| Alpha | Todo |\n| Beta | Doing |\n| Gamma | Done |";
    let mut shadow = ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        body,
        8,
        [RemoteId::new("table-1")],
    )
    .expect("shadow");
    shadow.blocks[0].kind = MarkdownBlockKind::TableWithRows {
        row_ids: vec![
            RemoteId::new("header-row"),
            RemoteId::new("alpha-row"),
            RemoteId::new("beta-row"),
            RemoteId::new("gamma-row"),
        ],
        has_column_header: true,
        has_row_header: false,
    };
    store
        .save_shadow(&fixture.mount_id, shadow)
        .expect("save shadow");
    let path = fixture.write_page(
        "Roadmap.md",
        "| Name | Status |\n| --- | --- |\n| Alpha | Todo |\n| Beta | Doing |",
    );

    let prepared =
        prepare_push(&store, &job(path), None, &LocalSourceValidator).expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::ConfirmPlan);
    assert!(prepared.pipeline.validation.is_clean());
    let plan = prepared.pipeline.plan.expect("plan");
    assert_eq!(plan.summary.blocks_updated, 1, "{plan:#?}");
}

#[test]
fn prepare_push_plans_pending_page_directory_rename_under_parent_scope() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.virtual_store("notion");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("page-parent"),
            EntityKind::Page,
            "Home",
            "Home/page.md",
        ))
        .expect("save parent page");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-child"),
                EntityKind::Page,
                "Renamed Child",
                "Home/Renamed Child/page.md",
            )
            .with_hydration(HydrationState::Dirty),
        )
        .expect("save renamed child page");
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("page-child"),
                "Child body.",
                8,
                [RemoteId::new("block-child")],
            )
            .expect("shadow")
            .with_frontmatter(
                "loc:\n  id: page-child\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: \"Child\"\n",
            ),
        )
        .expect("save child shadow");
    fixture.write_virtual_page(
        "Home/Renamed Child/page.md",
        "---\nloc:\n  id: page-child\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: \"Renamed Child\"\n---\nChild body.",
    );
    fs::create_dir_all(fixture.root.join("Home")).expect("visible parent dir");
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: fixture.mount_id.clone(),
            local_id: "rename:page-child".to_string(),
            mutation_kind: VirtualMutationKind::Rename,
            target_remote_id: Some(RemoteId::new("page-child")),
            parent_remote_id: None,
            original_path: Some(PathBuf::from("Home/Child/page.md")),
            projected_path: PathBuf::from("Home/Renamed Child/page.md"),
            title: "Renamed Child".to_string(),
            content_path: Some(
                virtual_fs_content_path(
                    &fixture.state_root,
                    &fixture.mount_id,
                    Path::new("Home/Renamed Child/page.md"),
                )
                .expect("content path"),
            ),
            created_at: "2026-06-12T00:00:00Z".to_string(),
            updated_at: "2026-06-12T00:00:00Z".to_string(),
        })
        .expect("save rename mutation");

    let prepared = prepare_push(
        &store,
        &job(fixture.root.join("Home")),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare parent scope rename");
    let plan = prepared.pipeline.plan.expect("plan");

    assert_eq!(
        plan.operations,
        vec![PushOperation::MoveEntity {
            entity_id: RemoteId::new("page-child"),
            new_parent_id: RemoteId::new("page-parent"),
            new_parent_kind: EntityKind::Page,
            new_title: "Renamed Child".to_string(),
            projected_path: PathBuf::from("Home/Renamed Child/page.md"),
        }]
    );
    assert_eq!(plan.summary.entities_moved, 1);
}

#[test]
fn prepare_linear_move_lowers_title_body_and_properties_without_duplicate_title_update() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.virtual_store("linear");
    for (id, title, path) in [
        ("team-a", "Team A", "Team A/page.md"),
        ("team-b", "Team B", "Team B/page.md"),
    ] {
        store
            .save_entity(EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new(id),
                EntityKind::Page,
                title,
                path,
            ))
            .expect("save team");
    }
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("issue-1"),
                EntityKind::Page,
                "Original title",
                "Team B/ENG-1-new/page.md",
            )
            .with_hydration(HydrationState::Dirty),
        )
        .expect("save moved issue");
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("issue-1"), "Old body.", 8, [RemoteId::new("body-1")],
            )
            .expect("shadow")
            .with_frontmatter(
                "loc:\n  id: issue-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Original title\nstatus: Todo\n",
            ),
        )
        .expect("save shadow");
    let cache = fixture.write_virtual_page(
        "Team B/ENG-1-new/page.md",
        "---\nloc:\n  id: issue-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Explicit edited title\nstatus: Done\n---\nNew body.\n",
    );
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: fixture.mount_id.clone(),
            local_id: "move:issue-1".to_string(),
            mutation_kind: VirtualMutationKind::Move,
            target_remote_id: Some(RemoteId::new("issue-1")),
            parent_remote_id: Some(RemoteId::new("team-b")),
            original_path: Some(PathBuf::from("Team A/ENG-1-old/page.md")),
            projected_path: PathBuf::from("Team B/ENG-1-new/page.md"),
            title: "Original title".to_string(),
            content_path: Some(cache),
            created_at: "2026-06-12T00:00:00Z".to_string(),
            updated_at: "2026-06-12T00:00:00Z".to_string(),
        })
        .expect("save move");
    fs::create_dir_all(fixture.root.join("Team B")).expect("visible destination team");

    let validator = RecordingValidator::default();
    let prepared = prepare_push(
        &store,
        &job(fixture.root.join("Team B")),
        Some(&fixture.state_root),
        &validator,
    )
    .expect("prepare move with edits");

    assert!(prepared.pipeline.validation.is_clean());
    assert_eq!(prepared.pipeline.action, PushPipelineAction::ConfirmPlan);
    let plan = prepared.pipeline.plan.expect("plan");
    assert_eq!(
        plan.operations,
        vec![
            PushOperation::MoveEntity {
                entity_id: RemoteId::new("issue-1"),
                new_parent_id: RemoteId::new("team-b"),
                new_parent_kind: EntityKind::Page,
                new_title: "Explicit edited title".to_string(),
                projected_path: PathBuf::from("Team B/ENG-1-new/page.md"),
            },
            PushOperation::UpdateProperties {
                entity_id: RemoteId::new("issue-1"),
                properties: BTreeMap::from([(
                    "status".to_string(),
                    PropertyValue::String("Done".to_string()),
                )]),
            },
            PushOperation::UpdateEntityBody {
                entity_id: RemoteId::new("issue-1"),
                body: "New body.\n".to_string(),
            },
        ]
    );
    assert_eq!(plan.affected_entities, vec![RemoteId::new("issue-1")]);
    let diff = prepared.readable_diff.expect("readable diff");
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].path, "Team B/ENG-1-new/page.md");
    assert!(diff.text.contains("+status: Done"));
    assert!(diff.text.contains("+New body."));
    assert_eq!(validator.changed_count.get(), 1);
    assert_eq!(
        validator.changed_parents.borrow().as_slice(),
        &[RemoteId::new("team-b")]
    );
}

#[test]
fn prepare_linear_cacheless_move_uses_shadow_but_missing_shadow_requires_materialization() {
    let (fixture, store) = linear_move_store(None, true);
    let prepared = prepare_push(
        &store,
        &job(fixture.root.join("Team B")),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare structural move");
    assert_eq!(
        prepared.pipeline.plan.expect("plan").operations,
        vec![PushOperation::MoveEntity {
            entity_id: RemoteId::new("issue-1"),
            new_parent_id: RemoteId::new("team-b"),
            new_parent_kind: EntityKind::Page,
            new_title: "Original title".to_string(),
            projected_path: PathBuf::from("Team B/ENG-1-new/page.md"),
        }]
    );
    assert!(prepared.readable_diff.is_none());

    let (fixture, store) = linear_move_store(None, false);
    let error = prepare_push(
        &store,
        &job(fixture.root.join("Team B")),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect_err("cacheless move without shadow must fail");
    assert!(
        matches!(error, PushPrepareError::Core(LocalityError::InvalidState(message)) if message.contains("materialized"))
    );
}

#[test]
fn prepare_linear_move_fails_closed_for_invalid_cached_content() {
    for (name, contents, code) in [
        (
            "parse",
            "---\n[invalid\n---\nBody\n",
            "canonical_invalid_frontmatter_yaml",
        ),
        (
            "conflict",
            "<<<<<<< LOCAL\nlocal\n=======\nremote\n>>>>>>> REMOTE\n",
            "unresolved_conflict_markers",
        ),
        (
            "identity",
            "---\nloc:\n  id: another-issue\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Original title\n---\nOld body.\n",
            "frontmatter_remote_id_mismatch",
        ),
    ] {
        let (fixture, store) = linear_move_store(Some(contents), true);
        let prepared = prepare_push(
            &store,
            &job(fixture.root.join("Team B")),
            Some(&fixture.state_root),
            &LocalSourceValidator,
        )
        .unwrap_or_else(|error| panic!("{name}: {error:?}"));
        assert_eq!(
            prepared.pipeline.action,
            PushPipelineAction::FixValidation,
            "{name}"
        );
        assert!(prepared.pipeline.plan.is_none(), "{name}");
        assert_eq!(prepared.pipeline.validation.issues[0].code, code, "{name}");
    }
}

#[test]
fn prepare_linear_move_empty_body_keeps_destructive_guardrail() {
    let contents = "---\nloc:\n  id: issue-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Original title\nstatus: Todo\n---\n";
    let (fixture, store) = linear_move_store(Some(contents), true);
    let prepared = prepare_push(
        &store,
        &job(fixture.root.join("Team B")),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare empty body move");
    assert_eq!(
        prepared.pipeline.action,
        PushPipelineAction::ConfirmDangerousPlan
    );
    assert!(matches!(
        prepared.pipeline.plan.expect("plan").operations.as_slice(),
        [PushOperation::MoveEntity { .. }, PushOperation::UpdateEntityBody { body, .. }]
            if body.is_empty()
    ));
}

#[test]
fn prepare_pending_scope_rejects_duplicate_remote_targets() {
    let (fixture, mut store) = linear_move_store(None, true);
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: fixture.mount_id.clone(),
            local_id: "rename:issue-1".to_string(),
            mutation_kind: VirtualMutationKind::Rename,
            target_remote_id: Some(RemoteId::new("issue-1")),
            parent_remote_id: Some(RemoteId::new("team-b")),
            original_path: Some(PathBuf::from("Team A/ENG-1-old/page.md")),
            projected_path: PathBuf::from("Team B/ENG-1-duplicate/page.md"),
            title: "Original title".to_string(),
            content_path: None,
            created_at: "2026-06-12T00:00:00Z".to_string(),
            updated_at: "2026-06-12T00:00:00Z".to_string(),
        })
        .expect("save duplicate mutation");

    let prepared = prepare_push(
        &store,
        &job(fixture.root.join("Team B")),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare duplicate scope");
    assert_eq!(prepared.pipeline.action, PushPipelineAction::FixValidation);
    assert!(prepared.pipeline.plan.is_none());
    assert_eq!(
        prepared.pipeline.validation.issues[0].code,
        "duplicate_virtual_mutation_target"
    );
}

#[test]
fn prepare_stale_pending_move_rechecks_source_and_mount_write_policy() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.virtual_store("gmail");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("sent-folder"),
            EntityKind::Directory,
            "sent",
            "sent",
        ))
        .expect("save sent folder");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("draft-1"),
                EntityKind::Page,
                "Draft subject",
                "sent/ENG-1.md",
            )
            .with_hydration(HydrationState::Dirty),
        )
        .expect("save moved draft");
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("draft-1"),
                "Draft body",
                8,
                [RemoteId::new("body-1")],
            )
            .expect("shadow"),
        )
        .expect("save shadow");
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: fixture.mount_id.clone(),
            local_id: "move:draft-1".to_string(),
            mutation_kind: VirtualMutationKind::Move,
            target_remote_id: Some(RemoteId::new("draft-1")),
            parent_remote_id: Some(RemoteId::new("sent-folder")),
            original_path: Some(PathBuf::from("draft/ENG-1.md")),
            projected_path: PathBuf::from("sent/ENG-1.md"),
            title: "Draft subject".to_string(),
            content_path: None,
            created_at: "2026-06-12T00:00:00Z".to_string(),
            updated_at: "2026-06-12T00:00:00Z".to_string(),
        })
        .expect("save stale move");
    fs::create_dir_all(fixture.root.join("sent")).expect("visible sent folder");
    let prepared = prepare_push(
        &store,
        &job(fixture.root.join("sent")),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare stale Gmail move");
    assert_eq!(prepared.pipeline.action, PushPipelineAction::FixValidation);
    assert!(prepared.pipeline.plan.is_none());
    assert!(
        prepared
            .pipeline
            .validation
            .issues
            .iter()
            .any(|issue| issue.code == "source_path_read_only")
    );

    let (fixture, mut store) = linear_move_store(None, true);
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "linear", fixture.root.clone())
                .projection(ProjectionMode::LinuxFuse)
                .read_only(true),
        )
        .expect("replace with read-only mount");
    let prepared = prepare_push(
        &store,
        &job(fixture.root.join("Team B")),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare read-only move");
    assert_eq!(
        prepared.pipeline.action,
        PushPipelineAction::ReadOnlyBlocked
    );
    assert!(prepared.pipeline.plan.is_none());
}

#[test]
fn prepare_pending_scope_aggregates_move_create_delete_and_joins_diffs() {
    let unchanged = "---\nloc:\n  id: issue-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Original title\nstatus: Todo\n---\nOld body.";
    let (fixture, mut store) = linear_move_store(Some(unchanged), true);
    let create_cache = fixture.write_virtual_page(
        "Team B/ENG-2.md",
        "---\ntitle: New issue\nstatus: Todo\n---\nNew issue body.\n",
    );
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: fixture.mount_id.clone(),
            local_id: "local:new-issue".to_string(),
            mutation_kind: VirtualMutationKind::Create,
            target_remote_id: None,
            parent_remote_id: Some(RemoteId::new("team-b")),
            original_path: None,
            projected_path: PathBuf::from("Team B/ENG-2.md"),
            title: "New issue".to_string(),
            content_path: Some(create_cache),
            created_at: "2026-06-12T00:00:00Z".to_string(),
            updated_at: "2026-06-12T00:00:00Z".to_string(),
        })
        .expect("save create");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("issue-old"),
            EntityKind::Page,
            "Obsolete",
            "Team B/obsolete.md",
        ))
        .expect("save obsolete issue");
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("issue-old"), "Obsolete body.", 8, [RemoteId::new("old-body")],
            )
            .expect("shadow")
            .with_frontmatter(
                "loc:\n  id: issue-old\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Obsolete\n",
            ),
        )
        .expect("save obsolete shadow");
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: fixture.mount_id.clone(),
            local_id: "delete:issue-old".to_string(),
            mutation_kind: VirtualMutationKind::Delete,
            target_remote_id: Some(RemoteId::new("issue-old")),
            parent_remote_id: None,
            original_path: Some(PathBuf::from("Team B/obsolete.md")),
            projected_path: PathBuf::from("Team B/obsolete.md"),
            title: "Obsolete".to_string(),
            content_path: None,
            created_at: "2026-06-12T00:00:00Z".to_string(),
            updated_at: "2026-06-12T00:00:00Z".to_string(),
        })
        .expect("save delete");

    let prepared = prepare_push(
        &store,
        &job(fixture.root.clone()),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare mixed pending scope");
    let plan = prepared.pipeline.plan.expect("combined plan");
    assert!(matches!(
        plan.operations[0],
        PushOperation::MoveEntity { .. }
    ));
    assert!(matches!(
        plan.operations[1],
        PushOperation::CreateEntity { .. }
    ));
    assert_eq!(
        plan.operations[2],
        PushOperation::ArchiveEntity {
            entity_id: RemoteId::new("issue-old")
        }
    );
    assert_eq!(
        plan.affected_entities,
        vec![
            RemoteId::new("issue-1"),
            RemoteId::new("team-b"),
            RemoteId::new("issue-old"),
        ]
    );
    assert_eq!(prepared.shadows.len(), 2);
    let diff = prepared.readable_diff.expect("joined diff");
    assert_eq!(
        diff.files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["Team B/ENG-2.md", "Team B/obsolete.md"]
    );
}

#[test]
fn prepare_push_plans_pending_page_directory_move_as_move_entity() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.virtual_store("notion");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("page-home"),
            EntityKind::Page,
            "Home",
            "Home/page.md",
        ))
        .expect("save home page");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("page-archive"),
            EntityKind::Page,
            "Archive",
            "Archive/page.md",
        ))
        .expect("save archive page");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-child"),
                EntityKind::Page,
                "Moved Child",
                "Archive/Moved Child/page.md",
            )
            .with_hydration(HydrationState::Dirty),
        )
        .expect("save moved child page");
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("page-child"),
                "Child body.",
                8,
                [RemoteId::new("block-child")],
            )
            .expect("shadow")
            .with_frontmatter(
                "loc:\n  id: page-child\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: \"Child\"\n",
            ),
        )
        .expect("save child shadow");
    fixture.write_virtual_page(
        "Archive/Moved Child/page.md",
        "---\nloc:\n  id: page-child\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: \"Moved Child\"\n---\nChild body.",
    );
    fs::create_dir_all(fixture.root.join("Archive")).expect("visible archive dir");
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: fixture.mount_id.clone(),
            local_id: "move:page-child".to_string(),
            mutation_kind: VirtualMutationKind::Move,
            target_remote_id: Some(RemoteId::new("page-child")),
            parent_remote_id: Some(RemoteId::new("page-archive")),
            original_path: Some(PathBuf::from("Home/Child/page.md")),
            projected_path: PathBuf::from("Archive/Moved Child/page.md"),
            title: "Moved Child".to_string(),
            content_path: Some(
                virtual_fs_content_path(
                    &fixture.state_root,
                    &fixture.mount_id,
                    Path::new("Archive/Moved Child/page.md"),
                )
                .expect("content path"),
            ),
            created_at: "2026-06-12T00:00:00Z".to_string(),
            updated_at: "2026-06-12T00:00:00Z".to_string(),
        })
        .expect("save move mutation");

    let prepared = prepare_push(
        &store,
        &job(fixture.root.join("Archive")),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare parent scope move");
    let plan = prepared.pipeline.plan.expect("plan");

    assert_eq!(
        plan.operations,
        vec![PushOperation::MoveEntity {
            entity_id: RemoteId::new("page-child"),
            new_parent_id: RemoteId::new("page-archive"),
            new_parent_kind: EntityKind::Page,
            new_title: "Moved Child".to_string(),
            projected_path: PathBuf::from("Archive/Moved Child/page.md"),
        }]
    );
    assert_eq!(plan.summary.entities_moved, 1);
}

#[test]
fn prepare_push_uses_shared_validator_for_direct_and_virtual_creates() {
    let fixture = PrepareFixture::new();
    let validator = RecordingValidator::default();

    let mut direct_store = fixture.store("fake");
    fixture.save_parent_page(&mut direct_store);
    let direct_path = fixture.write_raw("Roadmap/Draft.md", "---\ntitle: Draft\n---\n# Draft\n");
    let direct =
        prepare_push(&direct_store, &job(direct_path), None, &validator).expect("direct prepare");
    assert_eq!(direct.pipeline.action, PushPipelineAction::ConfirmPlan);

    let mut virtual_store = fixture.virtual_store("fake");
    fixture.save_parent_page(&mut virtual_store);
    let cache_path = fixture.write_cache("Draft.md", "---\ntitle: Draft\n---\n# Draft\n");
    virtual_store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:draft",
            Some(RemoteId::new("page-parent")),
            "Roadmap/Draft.md",
            cache_path,
        ))
        .expect("save mutation");
    let virtual_prepared = prepare_push(
        &virtual_store,
        &job(fixture.root.join("Roadmap/Draft.md")),
        None,
        &validator,
    )
    .expect("virtual prepare");
    assert_eq!(
        virtual_prepared.pipeline.action,
        PushPipelineAction::ConfirmPlan
    );

    assert_eq!(validator.create_count.get(), 2);
    assert_eq!(
        validator.paths.borrow().as_slice(),
        &[
            PathBuf::from("Roadmap/Draft.md"),
            PathBuf::from("Roadmap/Draft.md")
        ]
    );
    assert_eq!(
        validator.parents.borrow().as_slice(),
        &[RemoteId::new("page-parent"), RemoteId::new("page-parent")]
    );
}

#[test]
fn prepare_push_prefers_content_cache_for_linux_fuse_pending_create() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.virtual_store("fake");
    fixture.save_parent_page(&mut store);
    let source_path = Path::new("Roadmap/Draft/page.md");
    let cache_path = fixture.write_virtual_page(
        source_path.to_str().expect("source path"),
        "---\ntitle: Cached Draft\n---\nCached body.\n",
    );
    let projected_path = fixture.write_raw(
        source_path.to_str().expect("source path"),
        "---\ntitle: Projected Draft\n---\nProjected body.\n",
    );
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:draft",
            Some(RemoteId::new("page-parent")),
            source_path.to_str().expect("source path"),
            cache_path,
        ))
        .expect("save mutation");

    let prepared = prepare_push(
        &store,
        &job(projected_path),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare push");

    let plan = prepared.pipeline.plan.expect("plan");
    match &plan.operations[0] {
        PushOperation::CreateEntity { title, body, .. } => {
            assert_eq!(title, "Cached Draft");
            assert_eq!(body, "Cached body.\n");
        }
        operation => panic!("unexpected operation: {operation:?}"),
    }
}

#[cfg(target_os = "macos")]
#[test]
fn prepare_push_ignores_legacy_app_group_content_path_outside_sandbox() {
    let fixture = PrepareFixture::new_macos_default_state_root();
    let mut store = fixture.virtual_store("fake");
    fixture.save_parent_page(&mut store);
    let source_path = Path::new("Roadmap/Draft/page.md");
    let current_path = fixture.write_virtual_page(
        source_path.to_str().expect("source path"),
        "---\ntitle: Current Draft\n---\nCurrent body.\n",
    );
    let legacy_path = fixture.write_legacy_app_group_page(
        source_path.to_str().expect("source path"),
        "---\ntitle: Legacy Draft\n---\nLegacy body.\n",
    );
    let projected_path = fixture.write_raw(
        source_path.to_str().expect("source path"),
        "---\ntitle: Projected Draft\n---\nProjected body.\n",
    );
    assert_ne!(legacy_path, current_path);
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:draft",
            Some(RemoteId::new("page-parent")),
            source_path.to_str().expect("source path"),
            legacy_path,
        ))
        .expect("save mutation");

    let prepared = prepare_push(
        &store,
        &job(projected_path),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare push");

    let plan = prepared.pipeline.plan.expect("plan");
    match &plan.operations[0] {
        PushOperation::CreateEntity { title, body, .. } => {
            assert_eq!(title, "Current Draft");
            assert_eq!(body, "Current body.\n");
        }
        operation => panic!("unexpected operation: {operation:?}"),
    }
}

#[cfg(target_os = "macos")]
#[test]
fn prepare_push_recovers_pending_create_from_legacy_app_group_cache() {
    let fixture = PrepareFixture::new_macos_default_state_root();
    let mut store = fixture.virtual_store("fake");
    fixture.save_parent_page(&mut store);
    let source_path = Path::new("Roadmap/Draft/page.md");
    let current_path = virtual_fs_content_path(&fixture.state_root, &fixture.mount_id, source_path)
        .expect("current path");
    let legacy_path = fixture.write_legacy_app_group_page(
        source_path.to_str().expect("source path"),
        "---\ntitle: Legacy Draft\n---\nLegacy body.\n",
    );
    let projected_path = fixture.write_raw(
        source_path.to_str().expect("source path"),
        "---\ntitle: Projected Draft\n---\nProjected body.\n",
    );
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:draft",
            Some(RemoteId::new("page-parent")),
            source_path.to_str().expect("source path"),
            legacy_path,
        ))
        .expect("save mutation");
    assert!(!current_path.exists());

    let prepared = prepare_push(
        &store,
        &job(projected_path),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare push");

    let plan = prepared.pipeline.plan.expect("plan");
    match &plan.operations[0] {
        PushOperation::CreateEntity { title, body, .. } => {
            assert_eq!(title, "Legacy Draft");
            assert_eq!(body, "Legacy body.\n");
        }
        operation => panic!("unexpected operation: {operation:?}"),
    }
    assert_eq!(
        fs::read_to_string(&current_path).expect("read migrated cache"),
        "---\ntitle: Legacy Draft\n---\nLegacy body.\n"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn prepare_push_recovers_dirty_entity_from_legacy_app_group_cache() {
    let fixture = PrepareFixture::new_macos_default_state_root();
    let mut store = fixture.virtual_store("fake");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap/page.md",
            )
            .with_hydration(HydrationState::Dirty),
        )
        .expect("save dirty entity");
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("page-1"),
                "Original body.",
                8,
                [RemoteId::new("paragraph-1")],
            )
            .expect("shadow"),
        )
        .expect("save shadow");
    let source_path = Path::new("Roadmap/page.md");
    let current_path = virtual_fs_content_path(&fixture.state_root, &fixture.mount_id, source_path)
        .expect("current path");
    let legacy_path = fixture.write_legacy_app_group_page(
        source_path.to_str().expect("source path"),
        &canonical_markdown("page-1", "Updated from legacy cache."),
    );
    assert_ne!(legacy_path, current_path);
    assert!(!current_path.exists());

    let prepared = prepare_push(
        &store,
        &job(fixture.root.join(source_path)),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare push");

    let plan = prepared.pipeline.plan.expect("plan");
    assert_eq!(plan.summary.blocks_updated, 1, "{plan:#?}");
    assert_eq!(
        plan.operations,
        vec![PushOperation::UpdateBlock {
            block_id: RemoteId::new("paragraph-1"),
            content: "Updated from legacy cache.".to_string(),
        }]
    );
    assert_eq!(
        fs::read_to_string(&current_path).expect("read migrated cache"),
        canonical_markdown("page-1", "Updated from legacy cache.")
    );
}

#[test]
fn prepare_push_plans_virtual_create_under_mount_remote_root() {
    let fixture = PrepareFixture::new();
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(
                fixture.mount_id.clone(),
                "google-docs",
                fixture.root.clone(),
            )
            .with_remote_root_id(RemoteId::new("workspace-folder"))
            .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    let cache_path = fixture.write_virtual_page(
        "Scratch Hydration/page.md",
        "---\ntitle: Scratch Hydration\n---\nCreated through Locality.\n",
    );
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:scratch",
            Some(RemoteId::new("workspace-folder")),
            "Scratch Hydration/page.md",
            cache_path,
        ))
        .expect("save mutation");

    let prepared = prepare_push(
        &store,
        &job(fixture.root.join("Scratch Hydration/page.md")),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare root create");

    assert_eq!(prepared.entity.remote_id, RemoteId::new("workspace-folder"));
    assert_eq!(prepared.entity.kind, EntityKind::Directory);
    let plan = prepared.pipeline.plan.expect("plan");
    assert_eq!(
        plan.operations,
        vec![PushOperation::CreateEntity {
            parent_id: RemoteId::new("workspace-folder"),
            parent_kind: Some(EntityKind::Directory),
            parent_workspace: false,
            title: "Scratch Hydration".to_string(),
            properties: BTreeMap::new(),
            body: "Created through Locality.\n".to_string(),
            source_path: PathBuf::from("Scratch Hydration/page.md"),
        }]
    );
}

#[test]
fn prepare_gmail_draft_create_keeps_subject_and_recipients_as_properties() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("gmail");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("gmail-folder:draft"),
                EntityKind::Directory,
                "draft",
                "draft",
            )
            .with_hydration(HydrationState::Stub)
            .with_remote_edited_at("folder:draft"),
        )
        .expect("save draft folder");
    let draft_path = fixture.write_raw(
        "draft/reply.md",
        "---\nloc:\n  type: page\n  connector: gmail\ntitle: Reply\nsubject: Reply subject\nto: [\"ann@example.com\"]\ncc: [\"copy@example.com\"]\nbcc: []\n---\nBody\n",
    );

    let prepared = prepare_push(
        &store,
        &job(draft_path),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::ConfirmPlan);
    let plan = prepared.pipeline.plan.expect("plan");
    match &plan.operations[0] {
        PushOperation::CreateEntity {
            parent_id,
            parent_kind,
            title,
            properties,
            body,
            source_path,
            ..
        } => {
            assert_eq!(parent_id, &RemoteId::new("gmail-folder:draft"));
            assert_eq!(parent_kind, &Some(EntityKind::Directory));
            assert_eq!(title, "Reply");
            assert_eq!(body, "Body\n");
            assert_eq!(source_path, &PathBuf::from("draft/reply.md"));
            assert_eq!(
                properties.get("subject"),
                Some(&PropertyValue::String("Reply subject".to_string()))
            );
            assert_eq!(
                properties.get("to"),
                Some(&PropertyValue::List(vec!["ann@example.com".to_string()]))
            );
            assert_eq!(
                properties.get("cc"),
                Some(&PropertyValue::List(vec!["copy@example.com".to_string()]))
            );
            assert_eq!(properties.get("bcc"), Some(&PropertyValue::List(vec![])));
        }
        operation => panic!("unexpected operation: {operation:?}"),
    }
}

#[test]
fn prepare_gmail_draft_create_accepts_subject_without_title() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("gmail");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("gmail-folder:draft"),
                EntityKind::Directory,
                "draft",
                "draft",
            )
            .with_hydration(HydrationState::Stub)
            .with_remote_edited_at("folder:draft"),
        )
        .expect("save draft folder");
    let draft_path = fixture.write_raw(
        "draft/quarterly-update.md",
        "---\nloc:\n  type: page\n  connector: gmail\nsubject: Quarterly update\nto: [\"ann@example.com\"]\n---\nBody\n",
    );

    let prepared = prepare_push(
        &store,
        &job(draft_path),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::ConfirmPlan);
    assert!(prepared.pipeline.validation.is_clean());
    let plan = prepared.pipeline.plan.expect("plan");
    match &plan.operations[0] {
        PushOperation::CreateEntity {
            parent_id,
            parent_kind,
            title,
            properties,
            body,
            source_path,
            ..
        } => {
            assert_eq!(parent_id, &RemoteId::new("gmail-folder:draft"));
            assert_eq!(parent_kind, &Some(EntityKind::Directory));
            assert_eq!(title, "Quarterly update");
            assert_eq!(body, "Body\n");
            assert_eq!(source_path, &PathBuf::from("draft/quarterly-update.md"));
            assert_eq!(
                properties.get("subject"),
                Some(&PropertyValue::String("Quarterly update".to_string()))
            );
            assert_eq!(
                properties.get("to"),
                Some(&PropertyValue::List(vec!["ann@example.com".to_string()]))
            );
        }
        operation => panic!("unexpected operation: {operation:?}"),
    }
}

#[test]
fn prepare_push_plans_notion_private_create_with_workspace_parent() {
    let fixture = PrepareFixture::new();
    let store = fixture.store("notion");
    let path = fixture.write_raw(
        "Private Draft/page.md",
        "---\nloc:\n  private: true\ntitle: Private Draft\n---\nPrivate body.\n",
    );

    let prepared =
        prepare_push(&store, &job(path), None, &LocalSourceValidator).expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::ConfirmPlan);
    let plan = prepared.pipeline.plan.expect("plan");
    assert_eq!(plan.affected_entities, Vec::<RemoteId>::new());
    match &plan.operations[0] {
        PushOperation::CreateEntity {
            parent_id,
            parent_kind,
            parent_workspace,
            title,
            body,
            source_path,
            ..
        } => {
            assert_eq!(parent_id, &RemoteId::new("workspace"));
            assert_eq!(parent_kind, &None);
            assert!(*parent_workspace);
            assert_eq!(title, "Private Draft");
            assert_eq!(body, "Private body.\n");
            assert_eq!(source_path, &PathBuf::from("Private Draft/page.md"));
        }
        operation => panic!("unexpected operation: {operation:?}"),
    }
}

#[test]
fn prepare_push_direct_root_create_on_notion_reports_actionable_error() {
    let fixture = PrepareFixture::new();
    let store = fixture.store("notion");
    let path = fixture.write_raw(
        "Daily Practice/page.md",
        "---\ntitle: Daily Practice\n---\nPlan body.\n",
    );

    let error = prepare_push(&store, &job(path), None, &LocalSourceValidator)
        .expect_err("root create without --private should be rejected");

    match error {
        PushPrepareError::Core(LocalityError::InvalidState(message)) => {
            assert!(
                message.contains("does not support creating"),
                "unexpected message: {message}"
            );
            assert!(
                message.contains("--private"),
                "expected a --private hint for notion mounts: {message}"
            );
        }
        other => panic!("expected an actionable InvalidState error, got {other:?}"),
    }
}

#[test]
fn prepare_push_direct_root_create_reports_missing_remote_root_id() {
    let fixture = PrepareFixture::new();
    let store = fixture.store("google-docs");
    let path = fixture.write_raw(
        "Daily Practice/page.md",
        "---\ntitle: Daily Practice\n---\nPlan body.\n",
    );

    let error = prepare_push(&store, &job(path), None, &LocalSourceValidator)
        .expect_err("root create without a remote root id should be rejected");

    match error {
        PushPrepareError::Core(LocalityError::InvalidState(message)) => {
            assert!(
                message.contains("no known remote root id"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected an actionable InvalidState error, got {other:?}"),
    }
}

#[test]
fn prepare_push_plans_virtual_notion_private_create_without_parent_remote_id() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.virtual_store("notion");
    let source_path = Path::new("Private Root Draft/page.md");
    let cache_path = fixture.write_virtual_page(
        source_path.to_str().expect("source path"),
        "---\nloc:\n  private: true\ntitle: Private Root Draft\n---\nPrivate body.\n",
    );
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:private-root-draft",
            None,
            source_path.to_str().expect("source path"),
            cache_path,
        ))
        .expect("save mutation");

    let prepared = prepare_push(
        &store,
        &job(fixture.root.join(source_path)),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::ConfirmPlan);
    let plan = prepared.pipeline.plan.expect("plan");
    assert_eq!(plan.affected_entities, Vec::<RemoteId>::new());
    assert_eq!(
        plan.operations,
        vec![PushOperation::CreateEntity {
            parent_id: RemoteId::new("workspace"),
            parent_kind: None,
            parent_workspace: true,
            title: "Private Root Draft".to_string(),
            properties: BTreeMap::new(),
            body: "Private body.\n".to_string(),
            source_path: source_path.to_path_buf(),
        }]
    );
}

#[test]
fn prepare_push_uses_projected_file_when_pending_create_cache_has_not_settled() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.virtual_store("fake");
    fixture.save_parent_page(&mut store);
    let source_path = Path::new("Roadmap/Draft/page.md");
    let cache_path = fixture.write_virtual_page(source_path.to_str().expect("source path"), "");
    let projected_path = fixture.write_raw(
        source_path.to_str().expect("source path"),
        "---\ntitle: Projected Draft\n---\nProjected body.\n",
    );
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:draft",
            Some(RemoteId::new("page-parent")),
            source_path.to_str().expect("source path"),
            cache_path,
        ))
        .expect("save mutation");

    let prepared = prepare_push(
        &store,
        &job(projected_path),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare push");

    let plan = prepared.pipeline.plan.expect("plan");
    match &plan.operations[0] {
        PushOperation::CreateEntity { title, body, .. } => {
            assert_eq!(title, "Projected Draft");
            assert_eq!(body, "Projected body.\n");
        }
        operation => panic!("unexpected operation: {operation:?}"),
    }
}

#[test]
fn prepare_push_uses_page_directory_parent_for_new_page_document() {
    let fixture = PrepareFixture::new();
    let validator = RecordingValidator::default();
    let mut store = fixture.store("fake");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("page-parent"),
            EntityKind::Page,
            "Roadmap",
            "Roadmap/page.md",
        ))
        .expect("save page directory parent");
    let path = fixture.write_raw("Roadmap/Draft/page.md", "---\ntitle: Draft\n---\n# Draft\n");

    let prepared = prepare_push(&store, &job(path), None, &validator).expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::ConfirmPlan);
    assert_eq!(
        validator.parents.borrow().as_slice(),
        &[RemoteId::new("page-parent")]
    );
    let plan = prepared.pipeline.plan.expect("plan");
    match &plan.operations[0] {
        PushOperation::CreateEntity {
            parent_id,
            parent_kind,
            source_path,
            ..
        } => {
            assert_eq!(parent_id, &RemoteId::new("page-parent"));
            assert_eq!(parent_kind, &Some(EntityKind::Page));
            assert_eq!(source_path, &PathBuf::from("Roadmap/Draft/page.md"));
        }
        operation => panic!("unexpected operation: {operation:?}"),
    }
}

#[test]
fn prepare_push_preserves_structured_missing_shadow_error() {
    let fixture = PrepareFixture::new();
    let store = fixture.store("fake");
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nSame paragraph.");

    let error =
        prepare_push(&store, &job(path), None, &LocalSourceValidator).expect_err("missing shadow");

    assert_eq!(
        error,
        PushPrepareError::Store(StoreError::ShadowMissing {
            mount_id: fixture.mount_id.clone(),
            entity_id: RemoteId::new("page-1"),
        })
    );
}

#[test]
fn prepare_push_uses_source_descriptor_body_diff_mode_for_existing_entities() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("linear");
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("page-1"),
                "Old description.",
                8,
                [RemoteId::new("paragraph-1")],
            )
            .expect("shadow"),
        )
        .expect("save shadow");
    let path = fixture.write_page(
        "Roadmap.md",
        "First changed paragraph.\n\nSecond changed paragraph.",
    );
    let validator = RecordingValidator::default();

    let prepared = prepare_push(&store, &job(path), None, &validator).expect("prepare push");

    assert!(matches!(
        prepared.pipeline.plan.expect("plan").operations.as_slice(),
        [PushOperation::UpdateEntityBody { body, .. }]
            if body == "First changed paragraph.\n\nSecond changed paragraph."
    ));
}

fn linear_move_store(
    contents: Option<&str>,
    with_shadow: bool,
) -> (PrepareFixture, InMemoryStateStore) {
    let fixture = PrepareFixture::new();
    let mut store = fixture.virtual_store("linear");
    for (id, title, path) in [
        ("team-a", "Team A", "Team A/page.md"),
        ("team-b", "Team B", "Team B/page.md"),
    ] {
        store
            .save_entity(EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new(id),
                EntityKind::Page,
                title,
                path,
            ))
            .expect("save team");
    }
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("issue-1"),
                EntityKind::Page,
                "Original title",
                "Team B/ENG-1-new/page.md",
            )
            .with_hydration(HydrationState::Dirty),
        )
        .expect("save issue");
    if with_shadow {
        store
            .save_shadow(
                &fixture.mount_id,
                ShadowDocument::from_synced_body(
                    RemoteId::new("issue-1"), "Old body.", 8, [RemoteId::new("body-1")],
                )
                .expect("shadow")
                .with_frontmatter(
                    "loc:\n  id: issue-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Original title\nstatus: Todo\n",
                ),
            )
            .expect("save shadow");
    }
    let content_path =
        contents.map(|contents| fixture.write_virtual_page("Team B/ENG-1-new/page.md", contents));
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: fixture.mount_id.clone(),
            local_id: "move:issue-1".to_string(),
            mutation_kind: VirtualMutationKind::Move,
            target_remote_id: Some(RemoteId::new("issue-1")),
            parent_remote_id: Some(RemoteId::new("team-b")),
            original_path: Some(PathBuf::from("Team A/ENG-1-old/page.md")),
            projected_path: PathBuf::from("Team B/ENG-1-new/page.md"),
            title: "Original title".to_string(),
            content_path,
            created_at: "2026-06-12T00:00:00Z".to_string(),
            updated_at: "2026-06-12T00:00:00Z".to_string(),
        })
        .expect("save move");
    fs::create_dir_all(fixture.root.join("Team B")).expect("visible destination team");
    (fixture, store)
}

#[derive(Default)]
struct RecordingValidator {
    create_count: Cell<usize>,
    changed_count: Cell<usize>,
    changed_parents: RefCell<Vec<RemoteId>>,
    paths: RefCell<Vec<PathBuf>>,
    parents: RefCell<Vec<RemoteId>>,
}

impl SourcePushValidator for RecordingValidator {
    fn validate_changed_frontmatter(
        &self,
        context: SourceValidationContext<'_>,
    ) -> locality_core::LocalityResult<ValidationReport> {
        self.changed_count.set(self.changed_count.get() + 1);
        if let Some(parent) = context.parent {
            self.changed_parents
                .borrow_mut()
                .push(parent.remote_id.clone());
        }
        Ok(ValidationReport::clean())
    }

    fn validate_create_frontmatter(
        &self,
        context: SourceValidationContext<'_>,
    ) -> locality_core::LocalityResult<ValidationReport> {
        self.create_count.set(self.create_count.get() + 1);
        self.paths
            .borrow_mut()
            .push(context.relative_path.to_path_buf());
        if let Some(parent) = context.parent {
            self.parents.borrow_mut().push(parent.remote_id.clone());
        }
        Ok(ValidationReport::clean())
    }
}

struct PrepareFixture {
    root: PathBuf,
    state_root: PathBuf,
    mount_id: MountId,
}

impl PrepareFixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "localityd-push-preparation-{}-{unique}-{suffix}",
            std::process::id()
        ));
        let state_root = std::env::temp_dir().join(format!(
            "localityd-push-preparation-state-{}-{unique}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("fixture root");
        fs::create_dir_all(&state_root).expect("fixture state root");
        Self {
            root,
            state_root,
            mount_id: MountId::new("notion-main"),
        }
    }

    #[cfg(target_os = "macos")]
    fn new_macos_default_state_root() -> Self {
        let mut fixture = Self::new();
        let home = fixture.root.join("home");
        fixture.state_root = home.join(".loc");
        fs::create_dir_all(&fixture.state_root).expect("fixture state root");
        fixture
    }

    fn store(&self, connector: &str) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(MountConfig::new(
                self.mount_id.clone(),
                connector,
                self.root.clone(),
            ))
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

    fn virtual_store(&self, connector: &str) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(
                MountConfig::new(self.mount_id.clone(), connector, self.root.clone())
                    .projection(ProjectionMode::LinuxFuse),
            )
            .expect("save mount");
        store
    }

    fn save_database(&self, store: &mut InMemoryStateStore) {
        store
            .save_entity(EntityRecord::new(
                self.mount_id.clone(),
                RemoteId::new("database-1"),
                EntityKind::Database,
                "Tasks",
                "Tasks",
            ))
            .expect("save database");
    }

    fn save_parent_page(&self, store: &mut InMemoryStateStore) {
        store
            .save_entity(EntityRecord::new(
                self.mount_id.clone(),
                RemoteId::new("page-parent"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap",
            ))
            .expect("save parent page");
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

    fn write_cache(&self, relative_path: &str, contents: &str) -> PathBuf {
        self.write_raw(&format!(".content/{relative_path}"), contents)
    }

    fn write_virtual_page(&self, relative_path: &str, contents: &str) -> PathBuf {
        let path =
            virtual_fs_content_path(&self.state_root, &self.mount_id, Path::new(relative_path))
                .expect("content path");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("content parent");
        }
        fs::write(&path, contents).expect("write virtual page");
        path
    }

    #[cfg(target_os = "macos")]
    fn write_legacy_app_group_page(&self, relative_path: &str, contents: &str) -> PathBuf {
        let home = self
            .state_root
            .parent()
            .expect("state root parent")
            .to_path_buf();
        let path = home
            .join("Library")
            .join("Group Containers")
            .join("C484HB7Q6S.group.ai.codeflash.locality")
            .join("content")
            .join(&self.mount_id.0)
            .join("files")
            .join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("legacy parent");
        }
        fs::write(&path, contents).expect("write legacy page");
        path
    }

    fn write_virtual_media(&self, relative_path: &Path, contents: &[u8]) -> PathBuf {
        let path = virtual_fs_content_root(&self.state_root, &self.mount_id).join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("media parent");
        }
        fs::write(&path, contents).expect("write virtual media");
        path
    }

    fn write_virtual_media_manifest(&self, local_path: &Path, block_id: &str, bytes: &[u8]) {
        self.write_virtual_media_manifest_with_kind(local_path, "image", block_id, bytes);
    }

    fn write_virtual_media_manifest_with_kind(
        &self,
        local_path: &Path,
        kind: &str,
        block_id: &str,
        bytes: &[u8],
    ) {
        let manifest_path = virtual_fs_content_root(&self.state_root, &self.mount_id)
            .join(".loc/media/manifest.json");
        if let Some(parent) = manifest_path.parent() {
            fs::create_dir_all(parent).expect("manifest parent");
        }
        let key = local_path.to_string_lossy().replace('\\', "/");
        fs::write(
            manifest_path,
            serde_json::to_vec_pretty(&json!({
                "version": 1,
                "assets": {
                    key: {
                        "block_id": block_id,
                        "kind": kind,
                        "source_url": format!("https://example.com/{kind}"),
                        "local_path": local_path,
                        "sha256": sha256_hex(bytes),
                        "size": bytes.len(),
                    }
                }
            }))
            .expect("manifest json"),
        )
        .expect("write manifest");
    }

    fn write_tasks_schema(&self) {
        self.write_raw("Tasks/_schema.yaml", tasks_schema());
    }
}

impl Drop for PrepareFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
        let _ = fs::remove_dir_all(&self.state_root);
    }
}

fn job(target_path: PathBuf) -> PushJob {
    PushJob {
        target_path,
        assume_yes: false,
        confirm_dangerous: false,
    }
}

fn canonical_markdown(remote_id: &str, body: &str) -> String {
    format!(
        "---\nloc:\n  id: {remote_id}\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\n{body}"
    )
}

fn markdown_href(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn row_frontmatter(status: &str) -> String {
    format!(
        "loc:\n  id: row-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Existing task\nStatus: {status}\n"
    )
}

fn tasks_schema() -> &'static str {
    r#"loc:
  type: notion_database_schema
  database_id: "database-1"
title: "Tasks"
data_sources:
  - id: "source-1"
    name: "Tasks"
    properties:
      Name:
        id: "name-id"
        type: "title"
      Status:
        id: "status-id"
        type: "select"
        options:
          - name: "Todo"
            id: "todo-id"
"#
}

fn virtual_mutation(
    mount_id: &MountId,
    local_id: &str,
    parent_remote_id: Option<RemoteId>,
    path: &str,
    content_path: PathBuf,
) -> VirtualMutationRecord {
    VirtualMutationRecord {
        mount_id: mount_id.clone(),
        local_id: local_id.to_string(),
        mutation_kind: VirtualMutationKind::Create,
        target_remote_id: None,
        parent_remote_id,
        original_path: None,
        projected_path: PathBuf::from(path),
        title: "Draft".to_string(),
        content_path: Some(content_path),
        created_at: "2026-06-12T00:00:00Z".to_string(),
        updated_at: "2026-06-12T00:00:00Z".to_string(),
    }
}
