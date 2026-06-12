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
use afs_core::model::{MountId, RemoteId};
use afs_notion::client::NotionApi;
use afs_notion::dto::{
    BlockDto, BlockListDto, PageDto, PageListDto, PagePropertyDto, PaginatedListDto,
    RichTextBlockDto, RichTextDto, SyncedBlockDto, SyncedFromDto, TextRichTextDto,
};
use afs_notion::{NotionConfig, NotionConnector};
use afs_store::{ConnectionId, InMemoryStateStore, ProjectionMode};
use serde_json::Value;

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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_PAGE_ID; mutates the target page"]
fn live_mid_page_insert_push_reconciles() {
    let page_id = std::env::var("AFS_NOTION_PAGE_ID").expect("AFS_NOTION_PAGE_ID");
    std::env::var("NOTION_TOKEN").expect("NOTION_TOKEN");
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let connector = NotionConnector::new(NotionConfig::default());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new(page_id)),
            connection_id: None,
            read_only: false,
            projection: ProjectionMode::PlainFiles,
        },
    )
    .expect("mount");
    run_pull(&mut store, &connector, &fixture.root).expect("pull");
    let page_path = fixture.page_file();
    let original = fs::read_to_string(&page_path).expect("read pulled page");
    fs::write(
        &page_path,
        insert_after_frontmatter(&original, "Live AgentFS ignored test paragraph.\n\n"),
    )
    .expect("write local edit");

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

fn insert_after_frontmatter(document: &str, insertion: &str) -> String {
    if let Some(rest) = document.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---\n")
    {
        let body_start = 4 + end + 5;
        let mut edited = String::with_capacity(document.len() + insertion.len());
        edited.push_str(&document[..body_start]);
        edited.push_str(insertion);
        edited.push_str(&document[body_start..]);
        return edited;
    }

    format!("{insertion}{document}")
}

fn file_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
}
