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

/// A Notion database container.
///
/// Newer Notion API versions separate database containers from queryable data
/// sources. The database DTO carries the stable child block/database identity
/// plus the data source summaries needed to enumerate row pages.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseDto {
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
    pub title: Vec<RichTextDto>,
    #[serde(default)]
    pub data_sources: Vec<DataSourceSummaryDto>,
}

/// Minimal data source reference embedded in a database response.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataSourceSummaryDto {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// A queryable Notion data source and its property schema.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataSourceDto {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub created_time: Option<String>,
    #[serde(default)]
    pub last_edited_time: Option<String>,
    #[serde(default)]
    pub properties: BTreeMap<String, DataSourcePropertyDto>,
}

/// Notion data source property schema.
///
/// The schema details are intentionally shallow for now: AFS needs the stable
/// property ID, type, and user-visible option names for read projection and
/// future validation. Connector-specific write validation can add stricter
/// typed payloads here as it grows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataSourcePropertyDto {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub select: Option<SelectPropertySchemaDto>,
    #[serde(default)]
    pub multi_select: Option<SelectPropertySchemaDto>,
    #[serde(default)]
    pub status: Option<StatusPropertySchemaDto>,
    #[serde(default)]
    pub relation: Option<serde_json::Value>,
    #[serde(default)]
    pub formula: Option<serde_json::Value>,
    #[serde(default)]
    pub rollup: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectPropertySchemaDto {
    #[serde(default)]
    pub options: Vec<SelectOptionDto>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusPropertySchemaDto {
    #[serde(default)]
    pub options: Vec<SelectOptionDto>,
    #[serde(default)]
    pub groups: Vec<StatusGroupDto>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusGroupDto {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub option_ids: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectOptionDto {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PagePropertyDto {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub title: Vec<RichTextDto>,
    #[serde(default)]
    pub rich_text: Vec<RichTextDto>,
    #[serde(default)]
    pub number: Option<serde_json::Number>,
    #[serde(default)]
    pub select: Option<SelectOptionDto>,
    #[serde(default)]
    pub multi_select: Vec<SelectOptionDto>,
    #[serde(default)]
    pub status: Option<SelectOptionDto>,
    #[serde(default)]
    pub checkbox: Option<bool>,
    #[serde(default)]
    pub date: Option<DateMentionDto>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub phone_number: Option<String>,
    #[serde(default)]
    pub people: Vec<UserMentionDto>,
    #[serde(default)]
    pub relation: Vec<IdRefDto>,
    #[serde(default)]
    pub created_time: Option<String>,
    #[serde(default)]
    pub last_edited_time: Option<String>,
    #[serde(default)]
    pub created_by: Option<UserMentionDto>,
    #[serde(default)]
    pub last_edited_by: Option<UserMentionDto>,
    #[serde(default)]
    pub formula: Option<serde_json::Value>,
    #[serde(default)]
    pub rollup: Option<serde_json::Value>,
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
    pub table: Option<TableBlockDto>,
    #[serde(default)]
    pub table_row: Option<TableRowBlockDto>,
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
pub struct TableBlockDto {
    #[serde(default)]
    pub table_width: u16,
    #[serde(default)]
    pub has_column_header: bool,
    #[serde(default)]
    pub has_row_header: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableRowBlockDto {
    #[serde(default)]
    pub cells: Vec<Vec<RichTextDto>>,
}

/// One Notion rich text segment.
///
/// Notion stores text, mention, and equation payloads under variant-specific
/// keys while sharing `plain_text`, `href`, and annotations across variants.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichTextDto {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub text: Option<TextRichTextDto>,
    #[serde(default)]
    pub mention: Option<MentionRichTextDto>,
    #[serde(default)]
    pub equation: Option<EquationRichTextDto>,
    #[serde(default)]
    pub plain_text: String,
    #[serde(default)]
    pub href: Option<String>,
    #[serde(default)]
    pub annotations: RichTextAnnotationsDto,
}

/// Payload for a rich text segment whose `type` is `text`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextRichTextDto {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub link: Option<LinkDto>,
}

/// Notion link payload used by text links and link-preview mentions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkDto {
    #[serde(default)]
    pub url: String,
}

/// Payload for an inline Notion equation segment.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EquationRichTextDto {
    #[serde(default)]
    pub expression: String,
}

/// Payload for a Notion mention segment.
///
/// The supported fields cover the mention variants that currently render to a
/// stable Markdown representation. Unknown API fields are intentionally ignored
/// by serde and can still fall back to the segment's `plain_text`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MentionRichTextDto {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub page: Option<IdRefDto>,
    #[serde(default)]
    pub database: Option<IdRefDto>,
    #[serde(default)]
    pub user: Option<UserMentionDto>,
    #[serde(default)]
    pub date: Option<DateMentionDto>,
    #[serde(default)]
    pub link_preview: Option<LinkDto>,
}

/// Minimal reference to another Notion object by remote ID.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdRefDto {
    #[serde(default)]
    pub id: String,
}

/// Minimal user mention payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMentionDto {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// Date mention payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DateMentionDto {
    #[serde(default)]
    pub start: String,
    #[serde(default)]
    pub end: Option<String>,
    #[serde(default)]
    pub time_zone: Option<String>,
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
