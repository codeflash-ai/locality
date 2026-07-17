//! Validation and request construction for new Notion database drafts.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::{Map, Value, json};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseDraft {
    pub title: String,
    pub data_source_name: String,
    pub properties: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseDraftError {
    pub code: &'static str,
    pub message: String,
}

impl DatabaseDraftError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawDraft {
    #[serde(default)]
    loc: RawLoc,
    #[serde(default)]
    title: String,
    #[serde(default)]
    data_sources: Vec<RawDataSource>,
}

#[derive(Debug, Default, Deserialize)]
struct RawLoc {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    database_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawDataSource {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    properties: BTreeMap<String, RawProperty>,
}

#[derive(Debug, Default, Deserialize)]
struct RawProperty {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    options: Vec<RawOption>,
}

#[derive(Debug, Default, Deserialize)]
struct RawOption {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    color: Option<String>,
}

pub fn default_database_draft_yaml(title: &str) -> String {
    format!(
        "loc:\n  type: notion_database_schema\ntitle: {}\ndata_sources:\n  - name: Rows\n    properties:\n      Name:\n        type: title\n",
        yaml_string(title)
    )
}

pub fn parse_database_draft(input: &str) -> Result<DatabaseDraft, DatabaseDraftError> {
    let raw = yaml_serde::from_str::<RawDraft>(input).map_err(|error| {
        DatabaseDraftError::new(
            "notion_database_schema_invalid_yaml",
            format!("invalid Notion database draft YAML: {error}"),
        )
    })?;

    if raw.loc.kind != "notion_database_schema" {
        return Err(DatabaseDraftError::new(
            "notion_database_schema_type_invalid",
            "database drafts require `loc.type: notion_database_schema`",
        ));
    }
    if raw.loc.database_id.is_some() {
        return Err(DatabaseDraftError::new(
            "notion_database_schema_has_remote_id",
            "new database drafts must not contain generated `loc.database_id` metadata",
        ));
    }
    let title = raw.title.trim();
    if title.is_empty() {
        return Err(DatabaseDraftError::new(
            "notion_database_schema_title_missing",
            "new database drafts require a non-empty `title`",
        ));
    }
    if raw.data_sources.len() != 1 {
        return Err(DatabaseDraftError::new(
            "notion_database_schema_data_source_count",
            "new database drafts require exactly one initial data source",
        ));
    }

    let source = raw.data_sources.into_iter().next().expect("length checked");
    if source.id.is_some() {
        return Err(DatabaseDraftError::new(
            "notion_database_schema_has_remote_id",
            "new database drafts must not contain generated data source IDs",
        ));
    }
    let data_source_name = source.name.trim();
    if data_source_name.is_empty() {
        return Err(DatabaseDraftError::new(
            "notion_database_schema_data_source_name_missing",
            "the initial data source requires a non-empty `name`",
        ));
    }
    if source.properties.is_empty() {
        return Err(DatabaseDraftError::new(
            "notion_database_schema_properties_missing",
            "the initial data source requires at least one property",
        ));
    }

    let mut title_properties = 0;
    let mut properties = Map::new();
    for (name, property) in source.properties {
        if name.trim().is_empty() {
            return Err(DatabaseDraftError::new(
                "notion_database_schema_property_name_missing",
                "database property names cannot be empty",
            ));
        }
        if property.id.is_some() {
            return Err(DatabaseDraftError::new(
                "notion_database_schema_has_remote_id",
                format!("new property `{name}` must not contain a generated ID"),
            ));
        }
        if property.kind == "title" {
            title_properties += 1;
        }
        properties.insert(name.clone(), property_payload(&name, property)?);
    }
    if title_properties != 1 {
        return Err(DatabaseDraftError::new(
            "notion_database_schema_title_property_count",
            "the initial data source requires exactly one `title` property",
        ));
    }

    Ok(DatabaseDraft {
        title: title.to_string(),
        data_source_name: data_source_name.to_string(),
        properties,
    })
}

impl DatabaseDraft {
    pub fn create_request_body(&self, parent_page_id: &str) -> Value {
        json!({
            "parent": {
                "type": "page_id",
                "page_id": parent_page_id,
            },
            "title": rich_text(&self.title),
            "initial_data_source": {
                "title": rich_text(&self.data_source_name),
                "properties": self.properties,
            }
        })
    }
}

fn property_payload(name: &str, property: RawProperty) -> Result<Value, DatabaseDraftError> {
    let unsupported = || {
        DatabaseDraftError::new(
            "notion_database_schema_property_type_unsupported",
            format!(
                "property `{name}` has unsupported create type `{}`; supported types are title, rich_text, number, select, multi_select, status, checkbox, date, url, email, phone_number, files, and people",
                property.kind
            ),
        )
    };

    match property.kind.as_str() {
        "title" | "rich_text" | "checkbox" | "date" | "url" | "email" | "phone_number"
        | "files" | "people" => {
            reject_extra_configuration(name, &property)?;
            Ok(json!({ property.kind: {} }))
        }
        "number" => {
            if !property.options.is_empty() {
                return Err(DatabaseDraftError::new(
                    "notion_database_schema_property_options_invalid",
                    format!("number property `{name}` cannot define options"),
                ));
            }
            Ok(match property.format {
                Some(format) if valid_number_format(&format) => {
                    json!({ "number": { "format": format } })
                }
                Some(format) => {
                    return Err(DatabaseDraftError::new(
                        "notion_database_schema_number_format_invalid",
                        format!("number property `{name}` has invalid format `{format}`"),
                    ));
                }
                None => json!({ "number": {} }),
            })
        }
        "select" | "multi_select" => {
            if property.format.is_some() {
                return Err(DatabaseDraftError::new(
                    "notion_database_schema_property_format_invalid",
                    format!("property `{name}` cannot define a number format"),
                ));
            }
            let options = option_payloads(name, property.options)?;
            Ok(json!({ property.kind: { "options": options } }))
        }
        "status" => {
            if property.format.is_some() || !property.options.is_empty() {
                return Err(DatabaseDraftError::new(
                    "notion_database_schema_status_configuration_unsupported",
                    format!(
                        "status property `{name}` must use Notion's default status configuration"
                    ),
                ));
            }
            Ok(json!({ "status": {} }))
        }
        _ => Err(unsupported()),
    }
}

fn valid_number_format(format: &str) -> bool {
    matches!(
        format,
        "number"
            | "number_with_commas"
            | "percent"
            | "dollar"
            | "australian_dollar"
            | "canadian_dollar"
            | "euro"
            | "pound"
            | "yen"
            | "ruble"
            | "rupee"
            | "won"
            | "yuan"
            | "real"
            | "lira"
            | "rupiah"
            | "franc"
            | "hong_kong_dollar"
            | "new_zealand_dollar"
            | "krona"
            | "norwegian_krone"
            | "mexican_peso"
            | "rand"
            | "new_taiwan_dollar"
            | "danish_krone"
            | "zloty"
            | "baht"
            | "forint"
            | "koruna"
            | "shekel"
            | "chilean_peso"
            | "philippine_peso"
            | "dirham"
            | "colombian_peso"
            | "riyal"
            | "ringgit"
            | "leu"
            | "argentine_peso"
            | "uruguayan_peso"
            | "singapore_dollar"
    )
}

fn reject_extra_configuration(
    name: &str,
    property: &RawProperty,
) -> Result<(), DatabaseDraftError> {
    if property.format.is_some() || !property.options.is_empty() {
        return Err(DatabaseDraftError::new(
            "notion_database_schema_property_configuration_invalid",
            format!("property `{name}` does not accept `format` or `options`"),
        ));
    }
    Ok(())
}

fn option_payloads(name: &str, options: Vec<RawOption>) -> Result<Vec<Value>, DatabaseDraftError> {
    let mut names = BTreeSet::new();
    let mut values = Vec::with_capacity(options.len());
    for option in options {
        if option.id.is_some() {
            return Err(DatabaseDraftError::new(
                "notion_database_schema_has_remote_id",
                format!("option in property `{name}` must not contain a generated ID"),
            ));
        }
        let option_name = option.name.trim();
        if option_name.is_empty() || !names.insert(option_name.to_string()) {
            return Err(DatabaseDraftError::new(
                "notion_database_schema_option_name_invalid",
                format!("property `{name}` requires non-empty, unique option names"),
            ));
        }
        let mut value = json!({ "name": option_name });
        if let Some(color) = option.color {
            if !matches!(
                color.as_str(),
                "default"
                    | "gray"
                    | "brown"
                    | "orange"
                    | "yellow"
                    | "green"
                    | "blue"
                    | "purple"
                    | "pink"
                    | "red"
            ) {
                return Err(DatabaseDraftError::new(
                    "notion_database_schema_option_color_invalid",
                    format!(
                        "option `{option_name}` in property `{name}` has invalid color `{color}`"
                    ),
                ));
            }
            value["color"] = Value::String(color);
        }
        values.push(value);
    }
    Ok(values)
}

fn rich_text(value: &str) -> Value {
    json!([{ "type": "text", "text": { "content": value } }])
}

fn yaml_string(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    )
}

#[cfg(test)]
mod tests {
    use super::{default_database_draft_yaml, parse_database_draft};

    #[test]
    fn default_draft_is_valid_and_creates_one_title_property() {
        let yaml = default_database_draft_yaml("Tasks");
        let draft = parse_database_draft(&yaml).expect("parse default draft");

        assert_eq!(draft.title, "Tasks");
        assert_eq!(draft.data_source_name, "Rows");
        assert_eq!(draft.properties["Name"], serde_json::json!({ "title": {} }));
    }

    #[test]
    fn parses_supported_database_create_schema_exactly() {
        let draft = parse_database_draft(
            r#"loc:
  type: notion_database_schema
title: Tasks
data_sources:
  - name: Tasks
    properties:
      Name:
        type: title
      Notes:
        type: rich_text
      Points:
        type: number
        format: number_with_commas
      Status:
        type: select
        options:
          - name: Todo
            color: gray
          - name: Done
            color: green
      Tags:
        type: multi_select
        options: []
      State:
        type: status
      Done:
        type: checkbox
      Due:
        type: date
      URL:
        type: url
      Email:
        type: email
      Phone:
        type: phone_number
      Files:
        type: files
      People:
        type: people
"#,
        )
        .expect("parse draft");

        assert_eq!(
            draft.create_request_body("parent"),
            serde_json::json!({
                "parent": { "type": "page_id", "page_id": "parent" },
                "title": [{ "type": "text", "text": { "content": "Tasks" } }],
                "initial_data_source": {
                    "title": [{ "type": "text", "text": { "content": "Tasks" } }],
                    "properties": {
                        "Name": { "title": {} },
                        "Notes": { "rich_text": {} },
                        "Points": { "number": { "format": "number_with_commas" } },
                        "Status": { "select": { "options": [
                            { "name": "Todo", "color": "gray" },
                            { "name": "Done", "color": "green" }
                        ] } },
                        "Tags": { "multi_select": { "options": [] } },
                        "State": { "status": {} },
                        "Done": { "checkbox": {} },
                        "Due": { "date": {} },
                        "URL": { "url": {} },
                        "Email": { "email": {} },
                        "Phone": { "phone_number": {} },
                        "Files": { "files": {} },
                        "People": { "people": {} }
                    }
                }
            })
        );
    }

    #[test]
    fn rejects_generated_ids_and_unsupported_schema_types() {
        let generated = "loc:\n  type: notion_database_schema\n  database_id: db\ntitle: Tasks\ndata_sources: []\n";
        assert_eq!(
            parse_database_draft(generated)
                .expect_err("generated schema")
                .code,
            "notion_database_schema_has_remote_id"
        );

        let unsupported = "loc:\n  type: notion_database_schema\ntitle: Tasks\ndata_sources:\n  - name: Rows\n    properties:\n      Name:\n        type: title\n      Formula:\n        type: formula\n";
        assert_eq!(
            parse_database_draft(unsupported)
                .expect_err("unsupported schema")
                .code,
            "notion_database_schema_property_type_unsupported"
        );
    }
}
