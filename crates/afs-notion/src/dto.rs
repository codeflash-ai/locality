//! Notion API transfer objects.
//!
//! These structs intentionally mirror the JSON boundary. Rendering and sync
//! behavior live in separate modules so Notion API churn stays contained here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotionPageBundle {
    pub page: PageDto,
    pub blocks: Vec<BlockTreeDto>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockTreeDto {
    pub block: BlockDto,
    pub children: Vec<BlockTreeDto>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageDto {
    pub id: String,
    #[serde(default)]
    pub created_time: Option<String>,
    #[serde(default)]
    pub last_edited_time: Option<String>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub in_trash: bool,
    #[serde(default)]
    pub properties: BTreeMap<String, PagePropertyDto>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PagePropertyDto {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub title: Vec<RichTextDto>,
    #[serde(default)]
    pub rich_text: Vec<RichTextDto>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
pub struct PaginatedListDto<T> {
    #[serde(default)]
    pub results: Vec<T>,
    #[serde(default)]
    pub next_cursor: Option<String>,
    #[serde(default)]
    pub has_more: bool,
}

impl<T> Default for PaginatedListDto<T> {
    fn default() -> Self {
        Self {
            results: Vec::new(),
            next_cursor: None,
            has_more: false,
        }
    }
}

pub type BlockListDto = PaginatedListDto<BlockDto>;
pub type PageListDto = PaginatedListDto<PageDto>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockDto {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub has_children: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub in_trash: bool,
    #[serde(default)]
    pub paragraph: Option<RichTextBlockDto>,
    #[serde(default)]
    pub heading_1: Option<RichTextBlockDto>,
    #[serde(default)]
    pub heading_2: Option<RichTextBlockDto>,
    #[serde(default)]
    pub heading_3: Option<RichTextBlockDto>,
    #[serde(default)]
    pub bulleted_list_item: Option<RichTextBlockDto>,
    #[serde(default)]
    pub numbered_list_item: Option<RichTextBlockDto>,
    #[serde(default)]
    pub to_do: Option<ToDoBlockDto>,
    #[serde(default)]
    pub quote: Option<RichTextBlockDto>,
    #[serde(default)]
    pub callout: Option<RichTextBlockDto>,
    #[serde(default)]
    pub code: Option<CodeBlockDto>,
    #[serde(default)]
    pub child_page: Option<TitleBlockDto>,
    #[serde(default)]
    pub child_database: Option<TitleBlockDto>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichTextBlockDto {
    #[serde(default)]
    pub rich_text: Vec<RichTextDto>,
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToDoBlockDto {
    #[serde(default)]
    pub rich_text: Vec<RichTextDto>,
    #[serde(default)]
    pub checked: bool,
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeBlockDto {
    #[serde(default)]
    pub rich_text: Vec<RichTextDto>,
    #[serde(default)]
    pub language: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TitleBlockDto {
    #[serde(default)]
    pub title: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichTextDto {
    #[serde(default)]
    pub plain_text: String,
    #[serde(default)]
    pub href: Option<String>,
    #[serde(default)]
    pub annotations: RichTextAnnotationsDto,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichTextAnnotationsDto {
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub italic: bool,
    #[serde(default)]
    pub strikethrough: bool,
    #[serde(default)]
    pub underline: bool,
    #[serde(default)]
    pub code: bool,
    #[serde(default)]
    pub color: Option<String>,
}
