use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use afs_connector::{ApplyPlanRequest, Connector, FetchRequest};
use afs_core::journal::{PushId, PushOperationId};
use afs_core::model::{MountId, RemoteId};
use afs_core::planner::{PropertyValue, PushOperation, PushPlan};
use afs_notion::client::{DEFAULT_NOTION_API_BASE_URL, DEFAULT_NOTION_VERSION};
use afs_notion::dto::{DatabaseDto, NotionPageBundle, PageDto};
use afs_notion::{NotionConfig, NotionConnector};
use reqwest::blocking::Client;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const LIVE_PARENT_ENV: &str = "AFS_NOTION_LIVE_PARENT_PAGE";
const LIVE_DIR_ENV: &str = "AFS_NOTION_LIVE_DIR";
const TOKEN_ENV: &str = "NOTION_TOKEN";
const LIVE_IMAGE_URL: &str = "https://www.w3.org/Icons/w3c_home.png";

#[test]
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE"]
fn live_page_read_edit_write_verify_integrity_with_media_download() {
    let env = LiveEnv::from_env();
    let api = Arc::new(LiveNotion::new(env.token.clone()));
    let mut cleanup = LiveCleanup::new(api.clone());
    let connector = NotionConnector::new(NotionConfig::default());
    let title = format!("AFS live block integrity {}", unique_suffix());
    let page = cleanup.create_page(
        &env.parent_page_id,
        &title,
        rich_block_children(&env.parent_page_id),
    );
    cleanup.create_page(&page.id, "AFS nested child page", Vec::new());
    let page_id = RemoteId::new(page.id.clone());
    let page_path = Path::new("live-integrity/block-coverage.md");

    let native = connector
        .fetch(FetchRequest {
            remote_id: page_id.clone(),
        })
        .expect("live fetch");
    let rendered = connector
        .render_native_entity_for_path(&native, page_path)
        .expect("render with media paths");
    connector
        .download_rendered_media(&rendered, &env.local_dir)
        .expect("download rendered media");

    assert!(rendered.document.body.contains("# Heading one"));
    assert!(rendered.document.body.contains("## Heading two"));
    assert!(rendered.document.body.contains("### Heading three"));
    assert!(rendered.document.body.contains("#### Heading four"));
    assert!(rendered.document.body.contains("**Bold** _italic_"));
    assert!(rendered.document.body.contains("```rust"));
    assert!(rendered.document.body.contains("| Left | Right |"));
    assert!(rendered.document.body.contains("$$\nE=mc^2\n$$"));
    assert!(rendered.document.body.contains("type=image"));
    assert!(rendered.document.body.contains("type=video"));
    assert!(rendered.document.body.contains("type=file"));
    assert!(rendered.document.body.contains("type=pdf"));
    assert!(rendered.document.body.contains("type=audio"));
    assert!(rendered.document.body.contains("type=embed"));
    assert!(rendered.document.body.contains("type=table_of_contents"));
    assert!(rendered.document.body.contains("type=breadcrumb"));
    assert!(rendered.document.body.contains("type=column_list"));
    assert!(rendered.document.body.contains("type=link_to_page"));
    assert!(rendered.document.body.contains("type=child_page"));
    assert!(rendered.document.body.contains("local=\"media/"));
    assert!(
        rendered.media_assets.iter().any(|asset| {
            asset.kind == "image" && env.local_dir.join(&asset.local_path).is_file()
        })
    );

    let bundle: NotionPageBundle = serde_json::from_slice(&native.raw).expect("native bundle");
    let paragraph_id = first_block_id(&bundle, "paragraph");
    let last_block_id = bundle
        .blocks
        .last()
        .expect("at least one live block")
        .block
        .id
        .clone();
    let plan = PushPlan::new(
        vec![page_id.clone()],
        vec![
            PushOperation::UpdateBlock {
                block_id: RemoteId::new(first_block_id(&bundle, "heading_1")),
                content: "# Edited heading one".to_string(),
            },
            PushOperation::UpdateBlock {
                block_id: RemoteId::new(first_block_id(&bundle, "heading_2")),
                content: "## Edited heading two".to_string(),
            },
            PushOperation::UpdateBlock {
                block_id: RemoteId::new(first_block_id(&bundle, "heading_3")),
                content: "### Edited heading three".to_string(),
            },
            PushOperation::UpdateBlock {
                block_id: RemoteId::new(first_block_id(&bundle, "heading_4")),
                content: "#### Edited heading four".to_string(),
            },
            PushOperation::UpdateBlock {
                block_id: RemoteId::new(paragraph_id),
                content: "**Edited bold** with [external](https://example.com/) and $x+y$."
                    .to_string(),
            },
            PushOperation::UpdateBlock {
                block_id: RemoteId::new(first_block_id(&bundle, "bulleted_list_item")),
                content: "- Edited bullet".to_string(),
            },
            PushOperation::UpdateBlock {
                block_id: RemoteId::new(first_block_id(&bundle, "numbered_list_item")),
                content: "1. Edited number".to_string(),
            },
            PushOperation::UpdateBlock {
                block_id: RemoteId::new(first_block_id(&bundle, "to_do")),
                content: "- [x] Edited checkbox".to_string(),
            },
            PushOperation::UpdateBlock {
                block_id: RemoteId::new(first_block_id(&bundle, "quote")),
                content: "> Edited quote".to_string(),
            },
            PushOperation::UpdateBlock {
                block_id: RemoteId::new(first_block_id(&bundle, "code")),
                content: "```rust\nfn edited() {}\n```".to_string(),
            },
            PushOperation::UpdateBlock {
                block_id: RemoteId::new(first_block_id(&bundle, "divider")),
                content: "---".to_string(),
            },
            PushOperation::UpdateBlock {
                block_id: RemoteId::new(first_block_id(&bundle, "equation")),
                content: "$$\na^2+b^2=c^2\n$$".to_string(),
            },
            PushOperation::AppendBlock {
                parent_id: page_id.clone(),
                after: Some(RemoteId::new(last_block_id)),
                content: "Appended from AFS live integrity.".to_string(),
            },
        ],
    );
    apply_plan(&connector, &plan);

    let verified = connector
        .fetch(FetchRequest {
            remote_id: page_id.clone(),
        })
        .expect("verify fetch");
    let verified_render = connector
        .render_native_entity_for_path(&verified, page_path)
        .expect("verify render");
    assert!(
        verified_render
            .document
            .body
            .contains("# Edited heading one")
    );
    assert!(
        verified_render
            .document
            .body
            .contains("## Edited heading two")
    );
    assert!(
        verified_render
            .document
            .body
            .contains("### Edited heading three")
    );
    assert!(
        verified_render
            .document
            .body
            .contains("#### Edited heading four")
    );
    assert!(verified_render.document.body.contains("**Edited bold**"));
    assert!(verified_render.document.body.contains("- Edited bullet"));
    assert!(verified_render.document.body.contains("1. Edited number"));
    assert!(
        verified_render
            .document
            .body
            .contains("- [x] Edited checkbox")
    );
    assert!(verified_render.document.body.contains("> Edited quote"));
    assert!(verified_render.document.body.contains("fn edited() {}"));
    assert!(verified_render.document.body.contains("a^2+b^2=c^2"));
    assert!(
        verified_render
            .document
            .body
            .contains("Appended from AFS live integrity.")
    );
    assert!(verified_render.document.body.contains("type=image"));
}

#[test]
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE"]
fn live_database_row_property_create_edit_verify_integrity() {
    let env = LiveEnv::from_env();
    let api = Arc::new(LiveNotion::new(env.token.clone()));
    let mut cleanup = LiveCleanup::new(api.clone());
    let connector = NotionConnector::new(NotionConfig::default());
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live database scratch {}", unique_suffix()),
        Vec::new(),
    );
    let database =
        cleanup.create_database(&scratch.id, &format!("AFS live rows {}", unique_suffix()));
    let database_id = RemoteId::new(database.id.clone());

    let plan = PushPlan::new(
        vec![database_id.clone()],
        vec![PushOperation::CreateEntity {
            parent_id: database_id.clone(),
            title: "AFS created row".to_string(),
            properties: BTreeMap::from([
                (
                    "Notes".to_string(),
                    PropertyValue::String("Rich row notes".to_string()),
                ),
                (
                    "Points".to_string(),
                    PropertyValue::Number("42".to_string()),
                ),
                (
                    "Status".to_string(),
                    PropertyValue::String("Todo".to_string()),
                ),
                (
                    "State".to_string(),
                    PropertyValue::String("Not started".to_string()),
                ),
                (
                    "Tags".to_string(),
                    PropertyValue::List(vec!["Alpha".to_string(), "Beta".to_string()]),
                ),
                ("Done".to_string(), PropertyValue::Bool(false)),
                (
                    "Due".to_string(),
                    PropertyValue::String("2026-06-10".to_string()),
                ),
                (
                    "URL".to_string(),
                    PropertyValue::String("https://example.com/afs-live".to_string()),
                ),
                (
                    "Email".to_string(),
                    PropertyValue::String("agentfs@example.com".to_string()),
                ),
                (
                    "Phone".to_string(),
                    PropertyValue::String("+1 415 555 0100".to_string()),
                ),
            ]),
            body: "# Row body\n\nCreated from live integration.\n".to_string(),
            source_path: "Rows/afs-created-row.md".into(),
        }],
    );
    let result = apply_plan(&connector, &plan);
    let row_id = result
        .effects
        .iter()
        .find_map(|effect| match effect {
            afs_core::journal::JournalApplyEffect::CreatedEntity { entity_id, .. } => {
                Some(entity_id.clone())
            }
            _ => None,
        })
        .expect("created row id");

    let native = connector
        .fetch(FetchRequest {
            remote_id: row_id.clone(),
        })
        .expect("fetch created row");
    let rendered = connector
        .render_native_entity_for_path(&native, "Rows/afs-created-row.md")
        .expect("render created row");
    assert!(
        rendered
            .document
            .frontmatter
            .contains("title: \"AFS created row\"")
    );
    assert!(rendered.document.frontmatter.contains("\"Points\": 42"));
    assert!(rendered.document.frontmatter.contains("\"Done\": false"));
    assert!(
        rendered
            .document
            .frontmatter
            .contains("\"State\": \"Not started\"")
    );
    assert!(
        rendered
            .document
            .frontmatter
            .contains("\"URL\": \"https://example.com/afs-live\"")
    );

    let update = PushPlan::new(
        vec![row_id.clone()],
        vec![PushOperation::UpdateProperties {
            entity_id: row_id.clone(),
            properties: BTreeMap::from([
                (
                    "Points".to_string(),
                    PropertyValue::Number("43".to_string()),
                ),
                ("Done".to_string(), PropertyValue::Bool(true)),
                (
                    "State".to_string(),
                    PropertyValue::String("In progress".to_string()),
                ),
                (
                    "URL".to_string(),
                    PropertyValue::String("https://example.com/afs-live-updated".to_string()),
                ),
            ]),
        }],
    );
    apply_plan(&connector, &update);

    let verified = connector
        .fetch(FetchRequest { remote_id: row_id })
        .expect("fetch updated row");
    let verified_render = connector
        .render_native_entity_for_path(&verified, "Rows/afs-created-row.md")
        .expect("render updated row");
    assert!(
        verified_render
            .document
            .frontmatter
            .contains("\"Points\": 43")
    );
    assert!(
        verified_render
            .document
            .frontmatter
            .contains("\"Done\": true")
    );
    assert!(
        verified_render
            .document
            .frontmatter
            .contains("\"State\": \"In progress\"")
    );
    assert!(
        verified_render
            .document
            .frontmatter
            .contains("\"URL\": \"https://example.com/afs-live-updated\"")
    );
}

#[derive(Clone, Debug)]
struct LiveEnv {
    token: String,
    parent_page_id: String,
    local_dir: PathBuf,
}

impl LiveEnv {
    fn from_env() -> Self {
        let token = std::env::var(TOKEN_ENV).expect("set NOTION_TOKEN");
        let parent_page = std::env::var(LIVE_PARENT_ENV).unwrap_or_else(|_| {
            panic!("set {LIVE_PARENT_ENV} to a writable Notion page ID or URL")
        });
        let local_dir = std::env::var(LIVE_DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::temp_dir().join(format!("afs-notion-live-{}", unique_suffix()))
            });
        std::fs::create_dir_all(&local_dir).expect("live local dir");

        Self {
            token,
            parent_page_id: normalize_notion_id(&parent_page),
            local_dir,
        }
    }
}

#[derive(Debug)]
struct LiveCleanup {
    api: Arc<LiveNotion>,
    block_ids: Vec<String>,
}

impl LiveCleanup {
    fn new(api: Arc<LiveNotion>) -> Self {
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
                    "title": rich_text(title),
                },
            },
        });
        if !children.is_empty() {
            body["children"] = Value::Array(children);
        }
        let page = self.api.create_page(body).expect("create live page");
        self.block_ids.push(page.id.clone());
        page
    }

    fn create_database(&mut self, parent_page_id: &str, title: &str) -> DatabaseDto {
        let database = self
            .api
            .create_database(json!({
                "parent": {
                    "type": "page_id",
                    "page_id": parent_page_id,
                },
                "title": rich_text(title),
                "initial_data_source": {
                    "title": rich_text("Rows"),
                    "properties": {
                        "Name": { "title": {} },
                        "Notes": { "rich_text": {} },
                        "Points": { "number": { "format": "number" } },
                        "Status": {
                            "select": {
                                "options": [
                                    { "name": "Todo", "color": "gray" },
                                    { "name": "Done", "color": "green" }
                                ]
                            }
                        },
                        "State": { "status": {} },
                        "Tags": {
                            "multi_select": {
                                "options": [
                                    { "name": "Alpha", "color": "blue" },
                                    { "name": "Beta", "color": "purple" }
                                ]
                            }
                        },
                        "Done": { "checkbox": {} },
                        "Due": { "date": {} },
                        "URL": { "url": {} },
                        "Email": { "email": {} },
                        "Phone": { "phone_number": {} },
                        "Files": { "files": {} },
                        "People": { "people": {} },
                        "Unique": { "unique_id": { "prefix": "AFS" } }
                    }
                }
            }))
            .expect("create live database");
        self.block_ids.push(database.id.clone());
        database
    }
}

impl Drop for LiveCleanup {
    fn drop(&mut self) {
        for block_id in self.block_ids.iter().rev() {
            let _ = self.api.archive_block(block_id);
        }
    }
}

#[derive(Debug)]
struct LiveNotion {
    token: String,
    client: Client,
}

impl LiveNotion {
    fn new(token: String) -> Self {
        Self {
            token,
            client: Client::new(),
        }
    }

    fn create_page(&self, body: Value) -> Result<PageDto, String> {
        self.send_json(reqwest::Method::POST, "/v1/pages", Some(body))
    }

    fn create_database(&self, body: Value) -> Result<DatabaseDto, String> {
        self.send_json(reqwest::Method::POST, "/v1/databases", Some(body))
    }

    fn archive_block(&self, block_id: &str) -> Result<Value, String> {
        self.send_json::<Value, Value>(
            reqwest::Method::DELETE,
            &format!("/v1/blocks/{block_id}"),
            None,
        )
    }

    fn send_json<T, B>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<B>,
    ) -> Result<T, String>
    where
        T: DeserializeOwned,
        B: Serialize,
    {
        let url = format!(
            "{}/{}",
            DEFAULT_NOTION_API_BASE_URL,
            path.trim_start_matches('/')
        );
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(&self.token)
            .header("Notion-Version", DEFAULT_NOTION_VERSION);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().map_err(|error| error.to_string())?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
            return Err(format!("notion returned HTTP {status}: {body}"));
        }
        response.json().map_err(|error| error.to_string())
    }
}

fn apply_plan(connector: &NotionConnector, plan: &PushPlan) -> afs_connector::ApplyPlanResult {
    let push_id = PushId(format!("live-{}", unique_suffix()));
    let operation_ids = plan
        .operations
        .iter()
        .enumerate()
        .map(|(index, operation)| PushOperationId::for_operation(&push_id, index, operation))
        .collect::<Vec<_>>();

    connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &MountId::new("live-notion"),
            plan,
            operation_ids: &operation_ids,
            remote_preconditions: &[],
        })
        .expect("apply live plan")
}

fn rich_block_children(parent_page_id: &str) -> Vec<Value> {
    vec![
        json!({
            "object": "block",
            "type": "heading_1",
            "heading_1": { "rich_text": rich_text("Heading one") }
        }),
        json!({
            "object": "block",
            "type": "heading_2",
            "heading_2": { "rich_text": rich_text("Heading two") }
        }),
        json!({
            "object": "block",
            "type": "heading_3",
            "heading_3": { "rich_text": rich_text("Heading three") }
        }),
        json!({
            "object": "block",
            "type": "heading_4",
            "heading_4": { "rich_text": rich_text("Heading four") }
        }),
        json!({
            "object": "block",
            "type": "paragraph",
            "paragraph": {
                "rich_text": [
                    annotated_text("Bold", "bold"),
                    text_part(" "),
                    annotated_text("italic", "italic"),
                    text_part(" and "),
                    linked_text("link", "https://example.com/"),
                    text_part(" plus "),
                    equation_part("E=mc^2")
                ]
            }
        }),
        json!({
            "object": "block",
            "type": "bulleted_list_item",
            "bulleted_list_item": { "rich_text": rich_text("Bullet item") }
        }),
        json!({
            "object": "block",
            "type": "numbered_list_item",
            "numbered_list_item": { "rich_text": rich_text("Number item") }
        }),
        json!({
            "object": "block",
            "type": "to_do",
            "to_do": { "rich_text": rich_text("Checkbox item"), "checked": false }
        }),
        json!({
            "object": "block",
            "type": "quote",
            "quote": { "rich_text": rich_text("Quote item") }
        }),
        json!({
            "object": "block",
            "type": "callout",
            "callout": { "rich_text": rich_text("Callout item") }
        }),
        json!({
            "object": "block",
            "type": "toggle",
            "toggle": {
                "rich_text": rich_text("Toggle item"),
                "children": [
                    {
                        "object": "block",
                        "type": "paragraph",
                        "paragraph": { "rich_text": rich_text("Nested toggle child") }
                    }
                ]
            }
        }),
        json!({
            "object": "block",
            "type": "code",
            "code": { "rich_text": rich_text("fn main() {}"), "language": "rust" }
        }),
        json!({
            "object": "block",
            "type": "divider",
            "divider": {}
        }),
        json!({
            "object": "block",
            "type": "equation",
            "equation": { "expression": "E=mc^2" }
        }),
        json!({
            "object": "block",
            "type": "bookmark",
            "bookmark": { "url": "https://example.com/" }
        }),
        json!({
            "object": "block",
            "type": "embed",
            "embed": { "url": "https://example.com/embed" }
        }),
        json!({
            "object": "block",
            "type": "table",
            "table": {
                "table_width": 2,
                "has_column_header": true,
                "has_row_header": false,
                "children": [
                    {
                        "object": "block",
                        "type": "table_row",
                        "table_row": {
                            "cells": [rich_text("Left"), rich_text("Right")]
                        }
                    },
                    {
                        "object": "block",
                        "type": "table_row",
                        "table_row": {
                            "cells": [rich_text("A"), rich_text("B")]
                        }
                    }
                ]
            }
        }),
        json!({
            "object": "block",
            "type": "column_list",
            "column_list": {
                "children": [
                    {
                        "object": "block",
                        "type": "column",
                        "column": {
                            "children": [
                                {
                                    "object": "block",
                                    "type": "paragraph",
                                    "paragraph": { "rich_text": rich_text("Column one") }
                                }
                            ]
                        }
                    },
                    {
                        "object": "block",
                        "type": "column",
                        "column": {
                            "children": [
                                {
                                    "object": "block",
                                    "type": "paragraph",
                                    "paragraph": { "rich_text": rich_text("Column two") }
                                }
                            ]
                        }
                    }
                ]
            }
        }),
        json!({
            "object": "block",
            "type": "table_of_contents",
            "table_of_contents": { "color": "default" }
        }),
        json!({
            "object": "block",
            "type": "breadcrumb",
            "breadcrumb": {}
        }),
        json!({
            "object": "block",
            "type": "link_to_page",
            "link_to_page": {
                "type": "page_id",
                "page_id": parent_page_id
            }
        }),
        json!({
            "object": "block",
            "type": "image",
            "image": {
                "type": "external",
                "external": { "url": LIVE_IMAGE_URL },
                "caption": rich_text("W3C test image")
            }
        }),
        json!({
            "object": "block",
            "type": "video",
            "video": {
                "type": "external",
                "external": { "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ" },
                "caption": rich_text("External video")
            }
        }),
        json!({
            "object": "block",
            "type": "file",
            "file": {
                "type": "external",
                "external": { "url": "https://www.w3.org/WAI/ER/tests/xhtml/testfiles/resources/pdf/dummy.pdf" },
                "caption": rich_text("External file")
            }
        }),
        json!({
            "object": "block",
            "type": "pdf",
            "pdf": {
                "type": "external",
                "external": { "url": "https://www.w3.org/WAI/ER/tests/xhtml/testfiles/resources/pdf/dummy.pdf" },
                "caption": rich_text("External PDF")
            }
        }),
        json!({
            "object": "block",
            "type": "audio",
            "audio": {
                "type": "external",
                "external": { "url": "https://www.soundhelix.com/examples/mp3/SoundHelix-Song-1.mp3" },
                "caption": rich_text("External audio")
            }
        }),
    ]
}

fn first_block_id(bundle: &NotionPageBundle, kind: &str) -> String {
    bundle
        .blocks
        .iter()
        .find(|tree| tree.block.kind == kind)
        .map(|tree| tree.block.id.clone())
        .unwrap_or_else(|| panic!("missing {kind} block"))
}

fn rich_text(content: &str) -> Vec<Value> {
    vec![text_part(content)]
}

fn text_part(content: &str) -> Value {
    json!({
        "type": "text",
        "text": { "content": content }
    })
}

fn linked_text(content: &str, url: &str) -> Value {
    json!({
        "type": "text",
        "text": {
            "content": content,
            "link": { "url": url }
        }
    })
}

fn annotated_text(content: &str, annotation: &str) -> Value {
    let mut annotations = serde_json::Map::new();
    annotations.insert(annotation.to_string(), json!(true));
    json!({
        "type": "text",
        "text": { "content": content },
        "annotations": Value::Object(annotations)
    })
}

fn equation_part(expression: &str) -> Value {
    json!({
        "type": "equation",
        "equation": { "expression": expression }
    })
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
