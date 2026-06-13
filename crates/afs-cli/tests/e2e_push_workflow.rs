use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use afs_cli::diff::run_diff;
use afs_cli::mount::{MountOptions, run_mount};
use afs_cli::pull::run_pull;
use afs_cli::push::{PushOptions, run_push_with_daemon};
use afs_cli::status::{StatusOptions, run_status};
use afs_connector::{Connector, FetchRequest};
use afs_core::model::{MountId, RemoteId};
use afs_notion::client::{HttpNotionApi, NotionApi};
use afs_notion::dto::{
    BlockDto, BlockListDto, NotionPageBundle, PageDto, PageListDto, PagePropertyDto,
    PaginatedListDto, RichTextBlockDto, RichTextDto, SyncedBlockDto, SyncedFromDto,
    TextRichTextDto,
};
use afs_notion::{NotionConfig, NotionConnector};
use afs_store::{ConnectionId, InMemoryStateStore, ProjectionMode};
use serde_json::{Value, json};

const LIVE_PARENT_ENV: &str = "AFS_NOTION_LIVE_PARENT_PAGE";
const TOKEN_ENV: &str = "NOTION_TOKEN";

#[test]
fn mount_pull_mid_page_insert_push_and_status_clean() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::new());
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
        },
    )
    .expect("mount");

    let pull = run_pull(&mut store, &connector, &fixture.root).expect("pull");
    assert!(pull.ok);
    assert_eq!(pull.via, "cli");

    let page_path = fixture.page_file();
    let original = fs::read_to_string(&page_path).expect("read pulled page");
    assert!(original.contains("First paragraph."));
    fs::write(
        &page_path,
        original.replace(
            "First paragraph.\n\n",
            "First paragraph.\n\nJust Testing 101\n\n",
        ),
    )
    .expect("write local edit");

    let diff = run_diff(&store, &page_path).expect("diff");
    let plan = diff.plan.as_ref().expect("plan");
    assert_eq!(diff.action, "confirm_plan");
    assert_eq!(plan.summary.blocks_created, 1);
    assert!(plan.summary.blocks_moved >= 1);

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push");
    assert!(push.ok);
    assert_eq!(push.action, "reconciled");

    let status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status");
    assert!(status.clean, "{status:#?}");
    assert_eq!(status.summary.dirty, 0);

    let calls = api.calls.lock().expect("calls");
    assert!(
        calls
            .iter()
            .any(|call| matches!(call, WriteCall::Append { .. }))
    );
    assert!(
        calls
            .iter()
            .any(|call| matches!(call, WriteCall::Move { .. }))
    );
}

#[test]
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_scratch_page_mount_edit_push_verifies_notion() {
    std::env::var(TOKEN_ENV).expect("NOTION_TOKEN");
    let parent_page = normalize_notion_id(
        &std::env::var(LIVE_PARENT_ENV)
            .unwrap_or_else(|_| panic!("set {LIVE_PARENT_ENV} to a writable page ID or URL")),
    );
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let marker = format!("AFS live mounted edit {}", unique_suffix());
    let scratch = cleanup.create_page(
        &parent_page,
        &format!("AFS mounted e2e {}", unique_suffix()),
        vec![json!({
            "object": "block",
            "type": "paragraph",
            "paragraph": {
                "rich_text": [
                    {
                        "type": "text",
                        "text": {
                            "content": "Original paragraph created by the mounted AFS live e2e test."
                        }
                    }
                ]
            }
        })],
    );

    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let connector = NotionConnector::new(NotionConfig::default());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new(scratch.id.clone())),
            connection_id: None,
            read_only: false,
            projection: ProjectionMode::PlainFiles,
        },
    )
    .expect("mount");
    run_pull(&mut store, &connector, &fixture.root).expect("pull");
    let page_path = fixture.page_file();
    let original = fs::read_to_string(&page_path).expect("read pulled page");
    assert!(original.contains("Original paragraph created by the mounted AFS live e2e test."));
    fs::write(&page_path, format!("{original}\n\n{marker}\n")).expect("write local edit");

    let diff = run_diff(&store, &page_path).expect("diff");
    let plan = diff.plan.as_ref().expect("plan");
    assert_eq!(diff.action, "confirm_plan");
    assert!(plan.summary.blocks_created >= 1, "{plan:#?}");

    let dirty_status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("dirty status");
    assert!(!dirty_status.clean, "{dirty_status:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push");
    assert!(push.ok, "{push:#?}");

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("clean status");
    assert!(clean_status.clean, "{clean_status:#?}");

    let verified = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new(scratch.id),
        })
        .expect("verify fetch");
    let verified_render = connector
        .render_native_entity_for_path(&verified, &page_path)
        .expect("verify render");
    assert!(
        verified_render.document.body.contains(&marker),
        "{}",
        verified_render.document.body
    );
}

#[test]
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_cyclic_diverse_page_read_noop_preserves_notion() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let target = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS cyclic link target {}", unique_suffix()),
        vec![paragraph_child("Target page for live link checks.")],
    );
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS cyclic diverse read {}", unique_suffix()),
        diverse_page_children(&target.id),
    );
    cleanup.create_page(
        &source.id,
        &format!("AFS cyclic nested child {}", unique_suffix()),
        vec![paragraph_child(
            "Nested child page for directory projection checks.",
        )],
    );

    let connector = NotionConnector::new(NotionConfig::default());
    let before = live_block_snapshot(&connector, &source.id);
    let (_fixture, mut store, page_path, markdown) = pull_live_page(&connector, &source.id);

    for expected in [
        "Cyclic paragraph",
        "# Cyclic heading one",
        "## Cyclic heading two",
        "### Cyclic heading three",
        "#### Cyclic heading four",
        "- Cyclic bullet",
        "1. Cyclic number",
        "- [ ] Cyclic todo",
        "> Cyclic quote",
        "> [!NOTE]\n> Cyclic callout",
        "```rust\nfn cyclic() {}\n```",
        "$$\na^2+b^2=c^2\n$$",
        "| Left | Right |",
        "[Linked page](https://www.notion.so/",
        "target mention [AFS cyclic link target",
        "![Cyclic image](https://www.w3.org/Icons/w3c_home.png)",
        "[Cyclic video](https://www.youtube.com/watch?v=dQw4w9WgXcQ)",
        "[Cyclic file](https://www.w3.org/WAI/ER/tests/xhtml/testfiles/resources/pdf/dummy.pdf)",
        "[Cyclic PDF](https://www.w3.org/WAI/ER/tests/xhtml/testfiles/resources/pdf/dummy.pdf)",
    ] {
        assert!(
            markdown.contains(expected),
            "missing {expected:?}\n{markdown}"
        );
    }
    assert!(
        !markdown.contains("type=link_to_page"),
        "link_to_page should render as a Markdown link, not a directive:\n{markdown}"
    );

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("clean status");
    assert!(clean_status.clean, "{clean_status:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("noop push");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "noop", "{push:#?}");

    let after = live_block_snapshot(&connector, &source.id);
    assert_eq!(
        after, before,
        "read/noop cyclic path must not modify Notion block JSON"
    );
}

#[test]
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_cyclic_supported_block_edits_push_and_verify_notion() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS cyclic supported edits {}", unique_suffix()),
        supported_edit_children(),
    );

    let connector = NotionConnector::new(NotionConfig::default());
    let (fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    let edited = original
        .replace(
            "Editable paragraph original.",
            "Editable paragraph changed.",
        )
        .replace("# Editable heading one", "# Editable heading one changed")
        .replace("## Editable heading two", "## Editable heading two changed")
        .replace(
            "### Editable heading three",
            "### Editable heading three changed",
        )
        .replace(
            "#### Editable heading four",
            "#### Editable heading four changed",
        )
        .replace("- Editable bullet", "- Editable bullet changed")
        .replace("1. Editable number", "1. Editable number changed")
        .replace("- [ ] Editable todo", "- [x] Editable todo changed")
        .replace("> Editable quote", "> Editable quote changed")
        .replace(
            "> [!NOTE]\n> Editable callout",
            "> [!NOTE]\n> Editable callout changed",
        )
        .replace("fn editable() {}", "fn editable_changed() {}")
        .replace("x+y=z", "x-y=z");
    fs::write(&page_path, edited).expect("write cyclic edits");

    let dirty_status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("dirty status");
    assert!(!dirty_status.clean, "{dirty_status:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push cyclic edits");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("clean status");
    assert!(clean_status.clean, "{clean_status:#?}");

    let verified = render_live_page(&connector, &source.id, &page_path);
    for expected in [
        "Editable paragraph changed.",
        "# Editable heading one changed",
        "## Editable heading two changed",
        "### Editable heading three changed",
        "#### Editable heading four changed",
        "- Editable bullet changed",
        "1. Editable number changed",
        "- [x] Editable todo changed",
        "> Editable quote changed",
        "> [!NOTE]\n> Editable callout changed",
        "fn editable_changed() {}",
        "x-y=z",
    ] {
        assert!(
            verified.contains(expected),
            "missing {expected:?}\n{verified}"
        );
    }
}

struct E2eFixture {
    root: PathBuf,
    mount_id: MountId,
}

impl E2eFixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "afs-cli-e2e-push-{}-{unique}-{suffix}",
            std::process::id()
        ));
        Self {
            root,
            mount_id: MountId::new("notion-main"),
        }
    }

    fn page_file(&self) -> PathBuf {
        fs::read_dir(&self.root)
            .expect("read mount")
            .map(|entry| entry.expect("dir entry").path())
            .find(|path| {
                path.extension().is_some_and(|extension| extension == "md")
                    && file_name(path) != "AGENTS.md"
                    && file_name(path) != "CLAUDE.md"
            })
            .expect("page file")
    }
}

impl Drop for E2eFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Debug)]
struct LiveCleanup {
    api: HttpNotionApi,
    block_ids: Vec<String>,
}

impl LiveCleanup {
    fn new(api: HttpNotionApi) -> Self {
        Self {
            api,
            block_ids: Vec::new(),
        }
    }

    fn create_page(&mut self, parent_page_id: &str, title: &str, children: Vec<Value>) -> PageDto {
        let mut body = json!({
            "parent": {
                "type": "page_id",
                "page_id": parent_page_id,
            },
            "properties": {
                "title": {
                    "title": [
                        {
                            "type": "text",
                            "text": {
                                "content": title,
                            }
                        }
                    ]
                },
            },
        });
        if !children.is_empty() {
            body["children"] = Value::Array(children);
        }
        let page = self
            .api
            .create_page(body)
            .expect("create live scratch page");
        self.block_ids.push(page.id.clone());
        page
    }
}

impl Drop for LiveCleanup {
    fn drop(&mut self) {
        for block_id in self.block_ids.iter().rev() {
            let _ = self.api.delete_block(block_id);
        }
    }
}

#[derive(Debug)]
struct LiveEnv {
    parent_page_id: String,
}

impl LiveEnv {
    fn from_env() -> Self {
        std::env::var(TOKEN_ENV).expect("NOTION_TOKEN");
        let parent_page = std::env::var(LIVE_PARENT_ENV)
            .unwrap_or_else(|_| panic!("set {LIVE_PARENT_ENV} to a writable page ID or URL"));
        Self {
            parent_page_id: normalize_notion_id(&parent_page),
        }
    }
}

fn pull_live_page(
    connector: &NotionConnector,
    page_id: &str,
) -> (E2eFixture, InMemoryStateStore, PathBuf, String) {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new(page_id.to_string())),
            connection_id: None,
            read_only: false,
            projection: ProjectionMode::PlainFiles,
        },
    )
    .expect("mount live page");
    run_pull(&mut store, connector, &fixture.root).expect("pull live page");
    let page_path = fixture.page_file();
    let markdown = fs::read_to_string(&page_path).expect("read live page markdown");
    (fixture, store, page_path, markdown)
}

fn live_block_snapshot(connector: &NotionConnector, page_id: &str) -> Value {
    let native = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new(page_id.to_string()),
        })
        .expect("fetch live snapshot");
    let bundle: NotionPageBundle = serde_json::from_slice(&native.raw).expect("snapshot bundle");
    serde_json::to_value(bundle.blocks).expect("snapshot json")
}

fn render_live_page(connector: &NotionConnector, page_id: &str, page_path: &Path) -> String {
    let native = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new(page_id.to_string()),
        })
        .expect("fetch live page");
    connector
        .render_native_entity_for_path(&native, page_path)
        .expect("render live page")
        .document
        .body
}

fn diverse_page_children(target_page_id: &str) -> Vec<Value> {
    vec![
        json!({
            "object": "block",
            "type": "paragraph",
            "paragraph": {
                "rich_text": [
                    text_part("Cyclic paragraph with "),
                    annotated_text("bold", "bold"),
                    text_part(" and a target mention "),
                    page_mention_part("Target page", target_page_id),
                    text_part(" plus inline math "),
                    equation_part("a^2+b^2=c^2")
                ]
            }
        }),
        rich_text_child("heading_1", "Cyclic heading one"),
        rich_text_child("heading_2", "Cyclic heading two"),
        rich_text_child("heading_3", "Cyclic heading three"),
        rich_text_child("heading_4", "Cyclic heading four"),
        rich_text_child("bulleted_list_item", "Cyclic bullet"),
        rich_text_child("numbered_list_item", "Cyclic number"),
        json!({
            "object": "block",
            "type": "to_do",
            "to_do": { "rich_text": rich_text_json("Cyclic todo"), "checked": false }
        }),
        rich_text_child("quote", "Cyclic quote"),
        rich_text_child("callout", "Cyclic callout"),
        json!({
            "object": "block",
            "type": "toggle",
            "toggle": {
                "rich_text": rich_text_json("Cyclic toggle"),
                "children": [paragraph_child("Cyclic toggle child")]
            }
        }),
        json!({
            "object": "block",
            "type": "code",
            "code": { "rich_text": rich_text_json("fn cyclic() {}"), "language": "rust" }
        }),
        json!({ "object": "block", "type": "divider", "divider": {} }),
        json!({
            "object": "block",
            "type": "equation",
            "equation": { "expression": "a^2+b^2=c^2" }
        }),
        json!({
            "object": "block",
            "type": "bookmark",
            "bookmark": { "url": "https://example.com/cyclic-bookmark", "caption": rich_text_json("Cyclic bookmark") }
        }),
        json!({
            "object": "block",
            "type": "embed",
            "embed": { "url": "https://example.com/cyclic-embed", "caption": rich_text_json("Cyclic embed") }
        }),
        json!({
            "object": "block",
            "type": "table",
            "table": {
                "table_width": 2,
                "has_column_header": true,
                "has_row_header": false,
                "children": [
                    table_row_child("Left", "Right"),
                    table_row_child("Cell A", "Cell B")
                ]
            }
        }),
        json!({
            "object": "block",
            "type": "column_list",
            "column_list": {
                "children": [
                    { "object": "block", "type": "column", "column": { "children": [paragraph_child("Cyclic column one")] } },
                    { "object": "block", "type": "column", "column": { "children": [paragraph_child("Cyclic column two")] } }
                ]
            }
        }),
        json!({
            "object": "block",
            "type": "table_of_contents",
            "table_of_contents": { "color": "default" }
        }),
        json!({ "object": "block", "type": "breadcrumb", "breadcrumb": {} }),
        json!({
            "object": "block",
            "type": "link_to_page",
            "link_to_page": { "type": "page_id", "page_id": target_page_id }
        }),
        media_child(
            "image",
            "https://www.w3.org/Icons/w3c_home.png",
            "Cyclic image",
        ),
        media_child(
            "video",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "Cyclic video",
        ),
        media_child(
            "file",
            "https://www.w3.org/WAI/ER/tests/xhtml/testfiles/resources/pdf/dummy.pdf",
            "Cyclic file",
        ),
        media_child(
            "pdf",
            "https://www.w3.org/WAI/ER/tests/xhtml/testfiles/resources/pdf/dummy.pdf",
            "Cyclic PDF",
        ),
        media_child(
            "audio",
            "https://www.soundhelix.com/examples/mp3/SoundHelix-Song-1.mp3",
            "Cyclic audio",
        ),
    ]
}

fn supported_edit_children() -> Vec<Value> {
    vec![
        paragraph_child("Editable paragraph original."),
        rich_text_child("heading_1", "Editable heading one"),
        rich_text_child("heading_2", "Editable heading two"),
        rich_text_child("heading_3", "Editable heading three"),
        rich_text_child("heading_4", "Editable heading four"),
        rich_text_child("bulleted_list_item", "Editable bullet"),
        rich_text_child("numbered_list_item", "Editable number"),
        json!({
            "object": "block",
            "type": "to_do",
            "to_do": { "rich_text": rich_text_json("Editable todo"), "checked": false }
        }),
        rich_text_child("quote", "Editable quote"),
        rich_text_child("callout", "Editable callout"),
        json!({
            "object": "block",
            "type": "code",
            "code": { "rich_text": rich_text_json("fn editable() {}"), "language": "rust" }
        }),
        json!({ "object": "block", "type": "divider", "divider": {} }),
        json!({
            "object": "block",
            "type": "equation",
            "equation": { "expression": "x+y=z" }
        }),
    ]
}

fn paragraph_child(text: &str) -> Value {
    rich_text_child("paragraph", text)
}

fn rich_text_child(kind: &str, text: &str) -> Value {
    let mut block = json!({
        "object": "block",
        "type": kind
    });
    block[kind] = json!({ "rich_text": rich_text_json(text) });
    block
}

fn table_row_child(left: &str, right: &str) -> Value {
    json!({
        "object": "block",
        "type": "table_row",
        "table_row": {
            "cells": [rich_text_json(left), rich_text_json(right)]
        }
    })
}

fn media_child(kind: &str, url: &str, caption: &str) -> Value {
    let mut block = json!({
        "object": "block",
        "type": kind
    });
    block[kind] = json!({
        "type": "external",
        "external": { "url": url },
        "caption": rich_text_json(caption)
    });
    block
}

fn rich_text_json(text: &str) -> Vec<Value> {
    vec![text_part(text)]
}

fn text_part(text: &str) -> Value {
    json!({
        "type": "text",
        "text": { "content": text }
    })
}

fn annotated_text(text: &str, annotation: &str) -> Value {
    let mut annotations = serde_json::Map::new();
    annotations.insert(annotation.to_string(), json!(true));
    json!({
        "type": "text",
        "text": { "content": text },
        "annotations": Value::Object(annotations)
    })
}

fn equation_part(expression: &str) -> Value {
    json!({
        "type": "equation",
        "equation": { "expression": expression }
    })
}

fn page_mention_part(label: &str, page_id: &str) -> Value {
    json!({
        "type": "mention",
        "mention": {
            "type": "page",
            "page": { "id": page_id }
        },
        "plain_text": label
    })
}

#[derive(Debug)]
struct MutableNotionApi {
    page: PageDto,
    blocks: Mutex<Vec<BlockDto>>,
    append_count: Mutex<usize>,
    calls: Mutex<Vec<WriteCall>>,
}

impl MutableNotionApi {
    fn new() -> Self {
        Self {
            page: page("page-1", "Initial Idea"),
            blocks: Mutex::new(vec![
                paragraph_block("block-1", "First paragraph."),
                synced_block("synced-1", "source-block-1"),
                paragraph_block("block-2", "Second paragraph."),
                paragraph_block("block-3", "Third paragraph."),
                paragraph_block("block-4", "Fourth paragraph."),
                paragraph_block("block-5", "Fifth paragraph."),
                paragraph_block("block-6", "Sixth paragraph."),
            ]),
            append_count: Mutex::new(0),
            calls: Mutex::new(Vec::new()),
        }
    }
}

impl NotionApi for MutableNotionApi {
    fn retrieve_page(&self, page_id: &str) -> afs_core::AfsResult<PageDto> {
        if page_id == self.page.id {
            Ok(self.page.clone())
        } else {
            Err(afs_core::AfsError::InvalidState(format!(
                "missing page {page_id}"
            )))
        }
    }

    fn retrieve_block_children(
        &self,
        block_id: &str,
        _start_cursor: Option<&str>,
    ) -> afs_core::AfsResult<BlockListDto> {
        if block_id == self.page.id {
            Ok(PaginatedListDto {
                results: self.blocks.lock().expect("blocks").clone(),
                next_cursor: None,
                has_more: false,
            })
        } else {
            Ok(PaginatedListDto::default())
        }
    }

    fn search_pages(&self, _start_cursor: Option<&str>) -> afs_core::AfsResult<PageListDto> {
        Ok(PaginatedListDto {
            results: vec![self.page.clone()],
            next_cursor: None,
            has_more: false,
        })
    }

    fn update_block(&self, block_id: &str, body: Value) -> afs_core::AfsResult<BlockDto> {
        self.calls.lock().expect("calls").push(WriteCall::Update {
            block_id: block_id.to_string(),
        });
        let text = body_text(&body).unwrap_or_default();
        let mut blocks = self.blocks.lock().expect("blocks");
        if let Some(block) = blocks.iter_mut().find(|block| block.id == block_id) {
            *block = paragraph_block(block_id, &text);
            return Ok(block.clone());
        }
        Ok(paragraph_block(block_id, &text))
    }

    fn move_block(
        &self,
        block_id: &str,
        _parent_id: &str,
        after: Option<&str>,
    ) -> afs_core::AfsResult<BlockDto> {
        self.calls.lock().expect("calls").push(WriteCall::Move {
            block_id: block_id.to_string(),
            after: after.map(str::to_string),
        });
        let mut blocks = self.blocks.lock().expect("blocks");
        let Some(index) = blocks.iter().position(|block| block.id == block_id) else {
            return Ok(paragraph_block(block_id, ""));
        };
        let block = blocks.remove(index);
        let insert_at = after
            .and_then(|after| blocks.iter().position(|block| block.id == after))
            .map_or(0, |index| index + 1);
        blocks.insert(insert_at, block.clone());
        Ok(block)
    }

    fn append_block_children(
        &self,
        block_id: &str,
        body: Value,
    ) -> afs_core::AfsResult<BlockListDto> {
        self.calls.lock().expect("calls").push(WriteCall::Append {
            parent_id: block_id.to_string(),
        });
        let mut append_count = self.append_count.lock().expect("append count");
        *append_count += 1;
        let created_id = format!("created-{}", *append_count);
        let text = body_text(&body).unwrap_or_else(|| "Created.".to_string());
        let block = paragraph_block(&created_id, &text);
        let after = body
            .pointer("/position/after_block/id")
            .and_then(serde_json::Value::as_str);
        let mut blocks = self.blocks.lock().expect("blocks");
        let insert_at = after
            .and_then(|after| blocks.iter().position(|block| block.id == after))
            .map_or(0, |index| index + 1);
        blocks.insert(insert_at, block.clone());
        Ok(PaginatedListDto {
            results: vec![block],
            next_cursor: None,
            has_more: false,
        })
    }

    fn delete_block(&self, block_id: &str) -> afs_core::AfsResult<BlockDto> {
        self.calls.lock().expect("calls").push(WriteCall::Delete {
            block_id: block_id.to_string(),
        });
        let mut blocks = self.blocks.lock().expect("blocks");
        if let Some(index) = blocks.iter().position(|block| block.id == block_id) {
            return Ok(blocks.remove(index));
        }
        Ok(paragraph_block(block_id, ""))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WriteCall {
    Update {
        block_id: String,
    },
    Append {
        parent_id: String,
    },
    Move {
        block_id: String,
        after: Option<String>,
    },
    Delete {
        block_id: String,
    },
}

fn page(id: &str, title: &str) -> PageDto {
    PageDto {
        id: id.to_string(),
        parent: None,
        created_time: Some("2026-06-10T00:00:00.000Z".to_string()),
        last_edited_time: Some("2026-06-10T00:00:00.000Z".to_string()),
        archived: false,
        in_trash: false,
        properties: BTreeMap::from([(
            "title".to_string(),
            PagePropertyDto {
                kind: "title".to_string(),
                title: vec![rich_text(title)],
                ..Default::default()
            },
        )]),
    }
}

fn paragraph_block(id: &str, text: &str) -> BlockDto {
    let mut block = BlockDto {
        id: id.to_string(),
        kind: "paragraph".to_string(),
        ..Default::default()
    };
    block.paragraph = Some(RichTextBlockDto {
        rich_text: vec![rich_text(text)],
        color: None,
    });
    block
}

fn synced_block(id: &str, source_block_id: &str) -> BlockDto {
    let mut block = BlockDto {
        id: id.to_string(),
        kind: "synced_block".to_string(),
        ..Default::default()
    };
    block.synced_block = Some(SyncedBlockDto {
        synced_from: Some(SyncedFromDto {
            kind: "block_id".to_string(),
            block_id: Some(source_block_id.to_string()),
        }),
    });
    block
}

fn rich_text(text: &str) -> RichTextDto {
    RichTextDto {
        kind: "text".to_string(),
        text: Some(TextRichTextDto {
            content: text.to_string(),
            link: None,
        }),
        mention: None,
        equation: None,
        plain_text: text.to_string(),
        href: None,
        annotations: Default::default(),
    }
}

fn body_text(body: &Value) -> Option<String> {
    body.pointer("/paragraph/rich_text/0/text/content")
        .or_else(|| body.pointer("/children/0/paragraph/rich_text/0/text/content"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn normalize_notion_id(input: &str) -> String {
    let trimmed = input.trim().trim_end_matches('/');
    let candidate = trimmed
        .split(['?', '#'])
        .next()
        .unwrap_or(trimmed)
        .rsplit('/')
        .next()
        .unwrap_or(trimmed);
    let hex = candidate
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .collect::<String>();
    if hex.len() >= 32 {
        hex[hex.len() - 32..].to_string()
    } else {
        candidate.to_string()
    }
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}

fn file_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
}
