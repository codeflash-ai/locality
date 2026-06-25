use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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
        vec![PushOperation::UpdateProperties {
            entity_id: RemoteId::new("page-child"),
            properties: BTreeMap::from([(
                "title".to_string(),
                PropertyValue::String("Renamed Child".to_string()),
            )]),
        }]
    );
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
            title: "Scratch Hydration".to_string(),
            properties: BTreeMap::new(),
            body: "Created through Locality.\n".to_string(),
            source_path: PathBuf::from("Scratch Hydration/page.md"),
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

#[derive(Default)]
struct RecordingValidator {
    create_count: Cell<usize>,
    paths: RefCell<Vec<PathBuf>>,
    parents: RefCell<Vec<RemoteId>>,
}

impl SourcePushValidator for RecordingValidator {
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
