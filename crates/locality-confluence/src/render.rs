use locality_connector::ConnectorCapabilities;
use locality_core::LocalityResult;
use locality_core::model::CanonicalDocument;
use serde::{Deserialize, Serialize};

use crate::connector::CONFLUENCE_CONNECTOR_ID;
use crate::dto::{ConfluencePage, ConfluenceSpace};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfluenceNativeBundle {
    Space {
        space: ConfluenceSpace,
    },
    Page {
        space: ConfluenceSpace,
        page: ConfluencePage,
    },
}

pub fn confluence_capabilities_json() -> Result<String, serde_json::Error> {
    serde_json::to_string(&ConnectorCapabilities::read_only())
}

pub fn render_confluence_entity(
    bundle: &ConfluenceNativeBundle,
) -> LocalityResult<CanonicalDocument> {
    match bundle {
        ConfluenceNativeBundle::Space { space } => render_space(space),
        ConfluenceNativeBundle::Page { space, page } => render_page(space, page),
    }
}

pub fn remote_version_for_space(space: &ConfluenceSpace) -> String {
    format!(
        "confluence:space:{}:{}:{}",
        space.id, space.key, space.status
    )
}

pub fn remote_version_for_page(page: &ConfluencePage) -> String {
    format!(
        "confluence:page:{}:{}",
        page.id,
        page.version
            .as_ref()
            .map(|version| version.number)
            .unwrap_or_default()
    )
}

fn render_space(space: &ConfluenceSpace) -> LocalityResult<CanonicalDocument> {
    Ok(CanonicalDocument::new(
        format!(
            "loc:\n  id: {}\n  type: asset\n  connector: {}\n  synced_at: null\n  remote_edited_at: {}\ntitle: {}\nconfluence:\n  kind: space\n  id: {}\n  key: {}\n  name: {}\n  type: {}\n  status: {}\n  homepage_id: {}\n  webui: {}\n",
            yaml_string(&format!("confluence:space-summary:{}", space.id)),
            CONFLUENCE_CONNECTOR_ID,
            yaml_string(&remote_version_for_space(space)),
            yaml_string(&space.name),
            yaml_string(&space.id),
            yaml_string(&space.key),
            yaml_string(&space.name),
            yaml_string(&space.r#type),
            yaml_string(&space.status),
            optional_yaml_string(space.homepage_id.as_deref()),
            optional_yaml_string(space.links.webui.as_deref()),
        ),
        space_body(space),
    ))
}

fn render_page(
    space: &ConfluenceSpace,
    page: &ConfluencePage,
) -> LocalityResult<CanonicalDocument> {
    Ok(CanonicalDocument::new(
        format!(
            "loc:\n  id: {}\n  type: page\n  connector: {}\n  synced_at: null\n  remote_edited_at: {}\ntitle: {}\nconfluence:\n  kind: page\n  id: {}\n  space_id: {}\n  space_key: {}\n  status: {}\n  parent_id: {}\n  author_id: {}\n  version: {}\n  created_at: {}\n  version_created_at: {}\n  webui: {}\n  body_representation: {}\n",
            yaml_string(&format!("confluence:page:{}", page.id)),
            CONFLUENCE_CONNECTOR_ID,
            yaml_string(&remote_version_for_page(page)),
            yaml_string(&page.title),
            yaml_string(&page.id),
            yaml_string(&page.space_id),
            yaml_string(&space.key),
            yaml_string(&page.status),
            optional_yaml_string(page.parent_id.as_deref()),
            optional_yaml_string(page.author_id.as_deref()),
            page.version
                .as_ref()
                .map(|version| version.number)
                .unwrap_or_default(),
            optional_yaml_string(page.created_at.as_deref()),
            optional_yaml_string(
                page.version
                    .as_ref()
                    .and_then(|version| version.created_at.as_deref())
            ),
            optional_yaml_string(page.links.webui.as_deref()),
            yaml_string(page_body_representation(page)),
        ),
        page_body(page),
    ))
}

fn space_body(space: &ConfluenceSpace) -> String {
    ensure_trailing_newline(format!(
        "# {}\n\n- Key: `{}`\n- Status: {}\n- Type: {}\n",
        space.name, space.key, space.status, space.r#type
    ))
}

fn page_body(page: &ConfluencePage) -> String {
    let body = page
        .body
        .as_ref()
        .and_then(|body| body.storage.as_ref())
        .map(|storage| storage.value.clone())
        .or_else(|| {
            page.body
                .as_ref()
                .and_then(|body| body.atlas_doc_format.as_ref())
                .map(|adf| adf.value.clone())
        })
        .unwrap_or_default();
    ensure_trailing_newline(body)
}

fn page_body_representation(page: &ConfluencePage) -> &str {
    page.body
        .as_ref()
        .and_then(|body| body.storage.as_ref())
        .map(|storage| storage.representation.as_str())
        .or_else(|| {
            page.body
                .as_ref()
                .and_then(|body| body.atlas_doc_format.as_ref())
                .map(|adf| adf.representation.as_str())
        })
        .unwrap_or("")
}

fn ensure_trailing_newline(mut value: String) -> String {
    if !value.ends_with('\n') {
        value.push('\n');
    }
    value
}

fn optional_yaml_string(value: Option<&str>) -> String {
    value
        .filter(|value| !value.trim().is_empty())
        .map(yaml_string)
        .unwrap_or_else(|| "null".to_string())
}

fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}
