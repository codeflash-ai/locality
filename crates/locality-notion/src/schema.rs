//! Notion database schema validation for row frontmatter.
//!
//! `_schema.yaml` is the local contract for database row writes. The validator
//! keeps that contract close to the Notion connector while returning ordinary
//! Locality validation issues that CLI and daemon paths can surface uniformly.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use locality_core::canonical::{ParsedCanonicalDocument, parse_canonical_markdown};
use locality_core::diff::property_value_from_frontmatter;
use locality_core::model::CanonicalDocument;
use locality_core::planner::PropertyValue;
use locality_core::shadow::ShadowDocument;
use locality_core::validation::{ValidationIssue, ValidationReport};
use serde::Deserialize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseSchema {
    data_sources: Vec<DataSourceSchema>,
}

impl DatabaseSchema {
    pub fn parse(input: &str) -> Result<Self, SchemaParseError> {
        let raw =
            yaml_serde::from_str::<RawDatabaseSchema>(input).map_err(|error| SchemaParseError {
                message: format!("invalid Notion database schema YAML: {error}"),
            })?;

        Ok(Self {
            data_sources: raw
                .data_sources
                .into_iter()
                .map(DataSourceSchema::from)
                .collect(),
        })
    }

    fn single_data_source(&self, file: &Path) -> Result<&DataSourceSchema, ValidationIssue> {
        match self.data_sources.as_slice() {
            [] => Err(issue(
                "notion_schema_no_data_source",
                file,
                Some(1),
                "Notion database schema has no data sources",
                "pull the database again so `_schema.yaml` contains its data source schema",
            )),
            [data_source] => Ok(data_source),
            _ => Err(issue(
                "notion_schema_ambiguous_data_source",
                file,
                Some(1),
                "Notion database schema has multiple data sources; Locality cannot choose one for row writes yet",
                "split the target into a single-data-source database or wait for data-source selection support",
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaParseError {
    pub message: String,
}

pub fn validate_create_row_frontmatter(
    schema_yaml: &str,
    parsed: &ParsedCanonicalDocument,
    file: impl Into<PathBuf>,
) -> ValidationReport {
    let file = file.into();
    let mut values = parsed
        .frontmatter
        .properties
        .iter()
        .map(|(key, value)| (key.clone(), property_value_from_frontmatter(value)))
        .collect::<BTreeMap<_, _>>();

    if let Some(title) = parsed.frontmatter.title.clone() {
        values.insert("title".to_string(), PropertyValue::String(title));
    }

    validate_values(schema_yaml, &parsed.document.frontmatter, &values, &file)
}

pub fn validate_changed_row_frontmatter(
    schema_yaml: &str,
    shadow: &ShadowDocument,
    parsed: &ParsedCanonicalDocument,
    file: impl Into<PathBuf>,
) -> ValidationReport {
    let file = file.into();
    let synced = match parse_shadow_frontmatter(shadow) {
        Ok(synced) => synced,
        Err(message) => {
            let mut report = ValidationReport::clean();
            report.push(issue(
                "notion_schema_shadow_unparseable",
                &file,
                Some(1),
                message,
                "pull the row again before pushing property edits",
            ));
            return report;
        }
    };

    let mut values = BTreeMap::new();
    if synced.frontmatter.title != parsed.frontmatter.title {
        values.insert(
            "title".to_string(),
            parsed
                .frontmatter
                .title
                .clone()
                .map(PropertyValue::String)
                .unwrap_or(PropertyValue::Null),
        );
    }

    let keys = synced
        .frontmatter
        .properties
        .keys()
        .chain(parsed.frontmatter.properties.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for key in keys {
        let synced_value = synced.frontmatter.properties.get(&key);
        let edited_value = parsed.frontmatter.properties.get(&key);
        if synced_value != edited_value {
            values.insert(
                key.clone(),
                edited_value
                    .map(property_value_from_frontmatter)
                    .unwrap_or(PropertyValue::Null),
            );
        }
    }

    validate_values(schema_yaml, &parsed.document.frontmatter, &values, &file)
}

fn validate_values(
    schema_yaml: &str,
    frontmatter: &str,
    values: &BTreeMap<String, PropertyValue>,
    file: &Path,
) -> ValidationReport {
    let mut report = ValidationReport::clean();
    if values.is_empty() {
        return report;
    }

    let schema = match DatabaseSchema::parse(schema_yaml) {
        Ok(schema) => schema,
        Err(error) => {
            report.push(issue(
                "notion_schema_invalid",
                file,
                Some(1),
                error.message,
                "pull the database again to regenerate `_schema.yaml`",
            ));
            return report;
        }
    };
    let data_source = match schema.single_data_source(file) {
        Ok(data_source) => data_source,
        Err(issue) => {
            report.push(issue);
            return report;
        }
    };

    for (key, value) in values {
        let line = top_level_key_line(frontmatter, key).or(Some(1));
        validate_property(data_source, key, value, file, line, &mut report);
    }

    report
}

fn validate_property(
    data_source: &DataSourceSchema,
    key: &str,
    value: &PropertyValue,
    file: &Path,
    line: Option<usize>,
    report: &mut ValidationReport,
) {
    let property = if key == "title" {
        match data_source.title_property() {
            Some(property) => property,
            None => {
                report.push(issue(
                    "notion_schema_missing_title_property",
                    file,
                    line,
                    "Notion data source has no title property for canonical `title`",
                    "pull the database again or choose a database with a title property",
                ));
                return;
            }
        }
    } else {
        match data_source.properties.get(key) {
            Some(property) => property,
            None => {
                report.push(issue(
                    "notion_schema_property_unknown",
                    file,
                    line,
                    format!("Notion property `{key}` is not present in `_schema.yaml`"),
                    "remove the property or pull the database again if the schema changed",
                ));
                return;
            }
        }
    };

    if key != "title" && property.kind == "title" {
        report.push(issue(
            "notion_schema_title_property_reserved",
            file,
            line,
            format!("Notion property `{key}` is the database title property"),
            "edit canonical `title` instead of the Notion title property name",
        ));
        return;
    }

    if !property.is_writable() {
        report.push(issue(
            "notion_schema_property_read_only",
            file,
            line,
            format!(
                "Notion property `{key}` has read-only or unsupported type `{}`",
                property.kind
            ),
            "restore this property value or edit it in Notion until Locality supports this type",
        ));
        return;
    }

    match validate_value_for_property(property, value) {
        Ok(()) => {}
        Err((code, message, suggested_fix)) => report.push(issue(
            code,
            file,
            line,
            format!("Notion property `{key}` {message}"),
            suggested_fix,
        )),
    }
}

fn validate_value_for_property(
    property: &PropertySchema,
    value: &PropertyValue,
) -> Result<(), (&'static str, String, &'static str)> {
    match property.kind.as_str() {
        "title" => validate_required_string(value, "must be a non-empty string"),
        "rich_text" => validate_string(value, "must be a string"),
        "number" => validate_number(value),
        "select" | "status" => validate_single_option(property, value),
        "multi_select" => validate_multi_select(property, value),
        "checkbox" => validate_bool(value),
        "date" => validate_date(value),
        "url" => validate_nullable_string_shape(value, "URL", valid_url),
        "email" => validate_nullable_string_shape(value, "email address", valid_email),
        "phone_number" => validate_nullable_string(value, "phone number"),
        "files" => validate_files(value),
        "people" => validate_people(value),
        "relation" => validate_relation(value),
        _ => Ok(()),
    }
}

fn validate_required_string(
    value: &PropertyValue,
    message: &'static str,
) -> Result<(), (&'static str, String, &'static str)> {
    match value {
        PropertyValue::String(value) if !value.trim().is_empty() => Ok(()),
        _ => Err((
            "notion_schema_property_type_mismatch",
            message.to_string(),
            "use a non-empty string value",
        )),
    }
}

fn validate_string(
    value: &PropertyValue,
    message: &'static str,
) -> Result<(), (&'static str, String, &'static str)> {
    match value {
        PropertyValue::String(_) => Ok(()),
        _ => Err((
            "notion_schema_property_type_mismatch",
            message.to_string(),
            "use a string value",
        )),
    }
}

fn validate_number(value: &PropertyValue) -> Result<(), (&'static str, String, &'static str)> {
    match value {
        PropertyValue::Null => Ok(()),
        PropertyValue::Number(value) | PropertyValue::String(value) => {
            value.parse::<f64>().map(|_| ()).map_err(|_| {
                (
                    "notion_schema_property_number_invalid",
                    "must be numeric".to_string(),
                    "use a numeric value or null",
                )
            })
        }
        _ => Err((
            "notion_schema_property_type_mismatch",
            "must be a number or null".to_string(),
            "use a numeric value or null",
        )),
    }
}

fn validate_single_option(
    property: &PropertySchema,
    value: &PropertyValue,
) -> Result<(), (&'static str, String, &'static str)> {
    match value {
        PropertyValue::Null => Ok(()),
        PropertyValue::String(value) if value.trim().is_empty() => Ok(()),
        PropertyValue::String(value) if property.has_option(value) => Ok(()),
        PropertyValue::String(value) => Err((
            "notion_schema_option_unknown",
            format!("uses option `{value}` that is not present in `_schema.yaml`"),
            "use one of the options listed in `_schema.yaml` or add the option in Notion and pull again",
        )),
        _ => Err((
            "notion_schema_property_type_mismatch",
            "must be an option name string or null".to_string(),
            "use an option name from `_schema.yaml`",
        )),
    }
}

fn validate_multi_select(
    property: &PropertySchema,
    value: &PropertyValue,
) -> Result<(), (&'static str, String, &'static str)> {
    match value {
        PropertyValue::Null => Ok(()),
        PropertyValue::String(value) if value.trim().is_empty() => Ok(()),
        PropertyValue::List(values) => {
            for value in values {
                if !property.has_option(value) {
                    return Err((
                        "notion_schema_option_unknown",
                        format!("uses option `{value}` that is not present in `_schema.yaml`"),
                        "use only options listed in `_schema.yaml` or add the option in Notion and pull again",
                    ));
                }
            }
            Ok(())
        }
        _ => Err((
            "notion_schema_property_type_mismatch",
            "must be a list of option names or null".to_string(),
            "use a YAML list of option names from `_schema.yaml`",
        )),
    }
}

fn validate_bool(value: &PropertyValue) -> Result<(), (&'static str, String, &'static str)> {
    match value {
        PropertyValue::Bool(_) => Ok(()),
        _ => Err((
            "notion_schema_property_type_mismatch",
            "must be a boolean".to_string(),
            "use true or false",
        )),
    }
}

fn validate_date(value: &PropertyValue) -> Result<(), (&'static str, String, &'static str)> {
    match value {
        PropertyValue::Null => Ok(()),
        PropertyValue::String(value) if value.trim().is_empty() => Ok(()),
        PropertyValue::String(_) => Ok(()),
        PropertyValue::Object(fields) => match fields.get("start") {
            Some(PropertyValue::String(start)) if !start.trim().is_empty() => {
                validate_optional_string_field(fields, "end")?;
                validate_optional_string_field(fields, "time_zone")?;
                Ok(())
            }
            _ => Err((
                "notion_schema_property_type_mismatch",
                "must be a date string or object with string `start`".to_string(),
                "use a date string or `{ start: ..., end: ..., time_zone: ... }`",
            )),
        },
        _ => Err((
            "notion_schema_property_type_mismatch",
            "must be a date string, date object, or null".to_string(),
            "use a date string, date object, or null",
        )),
    }
}

fn validate_optional_string_field(
    fields: &BTreeMap<String, PropertyValue>,
    key: &str,
) -> Result<(), (&'static str, String, &'static str)> {
    match fields.get(key) {
        None | Some(PropertyValue::String(_)) => Ok(()),
        Some(_) => Err((
            "notion_schema_property_type_mismatch",
            format!("date object field `{key}` must be a string"),
            "use string values for date object fields",
        )),
    }
}

fn validate_nullable_string(
    value: &PropertyValue,
    label: &'static str,
) -> Result<(), (&'static str, String, &'static str)> {
    match value {
        PropertyValue::Null | PropertyValue::String(_) => Ok(()),
        _ => Err((
            "notion_schema_property_type_mismatch",
            format!("must be a {label} string or null"),
            "use a string value or null",
        )),
    }
}

fn validate_nullable_string_shape(
    value: &PropertyValue,
    label: &'static str,
    valid: fn(&str) -> bool,
) -> Result<(), (&'static str, String, &'static str)> {
    validate_nullable_string(value, label)?;
    if let PropertyValue::String(value) = value
        && !value.trim().is_empty()
        && !valid(value)
    {
        return Err((
            "notion_schema_property_shape_invalid",
            format!("must be a valid {label}"),
            "use a valid string value or null",
        ));
    }
    Ok(())
}

fn validate_files(value: &PropertyValue) -> Result<(), (&'static str, String, &'static str)> {
    let entries = match value {
        PropertyValue::Null => return Ok(()),
        PropertyValue::String(value) if value.trim().is_empty() => return Ok(()),
        PropertyValue::String(value) => vec![value.as_str()],
        PropertyValue::List(values) => values.iter().map(String::as_str).collect(),
        _ => {
            return Err((
                "notion_schema_property_type_mismatch",
                "must be a file URL string or list".to_string(),
                "use HTTP(S) URLs or `name <url>` list entries",
            ));
        }
    };

    if entries.iter().all(|entry| {
        let (_, url) = parse_external_file_entry(entry);
        valid_url(url)
    }) {
        Ok(())
    } else {
        Err((
            "notion_schema_property_shape_invalid",
            "must contain valid HTTP(S) file URLs".to_string(),
            "use HTTP(S) URLs or `name <url>` list entries",
        ))
    }
}

fn parse_external_file_entry(entry: &str) -> (&str, &str) {
    let trimmed = entry.trim();
    if let Some(without_close) = trimmed.strip_suffix('>')
        && let Some((name, url)) = without_close.rsplit_once(" <")
    {
        return (name, url);
    }
    ("", trimmed)
}

fn valid_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

fn validate_people(value: &PropertyValue) -> Result<(), (&'static str, String, &'static str)> {
    let entries = match value {
        PropertyValue::Null => return Ok(()),
        PropertyValue::String(value) if value.trim().is_empty() => return Ok(()),
        PropertyValue::String(value) => vec![value.as_str()],
        PropertyValue::List(values) => values.iter().map(String::as_str).collect(),
        _ => {
            return Err((
                "notion_schema_property_type_mismatch",
                "must be a Notion user ID string or list".to_string(),
                "use Notion user IDs from the rendered people property",
            ));
        }
    };

    if entries
        .iter()
        .all(|entry| valid_notion_id(parse_named_id_entry(entry).trim()))
    {
        Ok(())
    } else {
        Err((
            "notion_schema_property_shape_invalid",
            "must contain valid Notion user IDs".to_string(),
            "use 32-character or hyphenated Notion user IDs",
        ))
    }
}

fn validate_relation(value: &PropertyValue) -> Result<(), (&'static str, String, &'static str)> {
    let entries = match value {
        PropertyValue::Null => return Ok(()),
        PropertyValue::String(value) if value.trim().is_empty() => return Ok(()),
        PropertyValue::String(value) => vec![value.as_str()],
        PropertyValue::List(values) => values.iter().map(String::as_str).collect(),
        _ => {
            return Err((
                "notion_schema_property_type_mismatch",
                "must be a Notion page ID string or list".to_string(),
                "use Notion page IDs from the rendered relation property",
            ));
        }
    };

    if entries
        .iter()
        .all(|entry| valid_notion_id(parse_named_id_entry(entry).trim()))
    {
        Ok(())
    } else {
        Err((
            "notion_schema_property_shape_invalid",
            "must contain valid Notion page IDs".to_string(),
            "use 32-character or hyphenated Notion page IDs",
        ))
    }
}

fn parse_named_id_entry(entry: &str) -> &str {
    let trimmed = entry.trim();
    if let Some(without_close) = trimmed.strip_suffix('>')
        && let Some((_, id)) = without_close.rsplit_once(" <")
    {
        return id;
    }
    trimmed
}

fn valid_notion_id(value: &str) -> bool {
    let compact = value.replace('-', "");
    compact.len() == 32 && compact.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_email(value: &str) -> bool {
    let value = value.trim();
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.ends_with('.')
}

fn parse_shadow_frontmatter(shadow: &ShadowDocument) -> Result<ParsedCanonicalDocument, String> {
    parse_canonical_markdown(&locality_core::canonical::render_canonical_markdown(
        &CanonicalDocument::new(shadow.frontmatter.clone(), shadow.rendered_body.clone()),
    ))
    .map_err(|error| format!("synced row frontmatter is no longer parseable: {error}"))
}

fn top_level_key_line(frontmatter: &str, key: &str) -> Option<usize> {
    for (index, line) in frontmatter.lines().enumerate() {
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let Some((raw_key, _)) = line.split_once(':') else {
            continue;
        };
        if unquote_yaml_key(raw_key.trim()) == key {
            return Some(index + 2);
        }
    }
    None
}

fn unquote_yaml_key(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

fn issue(
    code: impl Into<String>,
    file: impl Into<PathBuf>,
    line: Option<usize>,
    message: impl Into<String>,
    suggested_fix: impl Into<String>,
) -> ValidationIssue {
    ValidationIssue::new(code, file, line, message, Some(suggested_fix.into()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DataSourceSchema {
    properties: BTreeMap<String, PropertySchema>,
}

impl DataSourceSchema {
    fn title_property(&self) -> Option<&PropertySchema> {
        self.properties
            .values()
            .find(|property| property.kind == "title")
    }
}

impl From<RawDataSourceSchema> for DataSourceSchema {
    fn from(value: RawDataSourceSchema) -> Self {
        Self {
            properties: value
                .properties
                .into_iter()
                .map(|(name, property)| (name, PropertySchema::from(property)))
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PropertySchema {
    kind: String,
    options: BTreeSet<String>,
}

impl PropertySchema {
    fn is_writable(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "title"
                | "rich_text"
                | "number"
                | "select"
                | "status"
                | "multi_select"
                | "checkbox"
                | "date"
                | "url"
                | "email"
                | "phone_number"
                | "files"
                | "people"
                | "relation"
        )
    }

    fn has_option(&self, name: &str) -> bool {
        self.options.contains(name)
    }
}

impl From<RawPropertySchema> for PropertySchema {
    fn from(value: RawPropertySchema) -> Self {
        Self {
            kind: value.kind,
            options: value
                .options
                .into_iter()
                .map(|option| option.name)
                .collect(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawDatabaseSchema {
    #[serde(default)]
    data_sources: Vec<RawDataSourceSchema>,
}

#[derive(Debug, Default, Deserialize)]
struct RawDataSourceSchema {
    #[serde(default)]
    properties: BTreeMap<String, RawPropertySchema>,
}

#[derive(Debug, Default, Deserialize)]
struct RawPropertySchema {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    options: Vec<RawOptionSchema>,
}

#[derive(Debug, Default, Deserialize)]
struct RawOptionSchema {
    name: String,
}

#[cfg(test)]
mod tests {
    use locality_core::canonical::parse_canonical_markdown;
    use locality_core::shadow::ShadowDocument;

    use super::{validate_changed_row_frontmatter, validate_create_row_frontmatter};

    #[test]
    fn validates_create_row_against_schema_options_and_types() {
        let parsed = parse_canonical_markdown(
            "---\ntitle: New task\nStatus: Todo\nTags:\n  - Backend\nDone: false\nPoints: 5\nDue:\n  start: \"2026-06-10\"\nURL: https://example.com/loc\nEmail: locality@example.com\nPhone: \"+1 415 555 0100\"\nFiles:\n  - Spec <https://example.com/spec.pdf>\nPeople:\n  - Ada <11111111111111111111111111111111>\nRelation:\n  - \"11111111111111111111111111111111\"\n---\n# Body\n",
        )
        .expect("parse row");

        let report = validate_create_row_frontmatter(schema_yaml(), &parsed, "Tasks/new.md");

        assert!(report.is_clean(), "{report:?}");
    }

    #[test]
    fn rejects_unknown_options_and_read_only_properties() {
        let parsed = parse_canonical_markdown(
            "---\ntitle: New task\nStatus: Blocked\nFormula: edited\nFiles:\n  - not-a-url\nPeople:\n  - not-a-user-id\nRelation:\n  - bad-id\n---\n# Body\n",
        )
        .expect("parse row");

        let report = validate_create_row_frontmatter(schema_yaml(), &parsed, "Tasks/new.md");

        assert_eq!(
            report
                .issues
                .iter()
                .map(|issue| issue.code.as_str())
                .collect::<Vec<_>>(),
            vec![
                "notion_schema_property_shape_invalid",
                "notion_schema_property_read_only",
                "notion_schema_property_shape_invalid",
                "notion_schema_property_shape_invalid",
                "notion_schema_option_unknown"
            ]
        );
    }

    #[test]
    fn validates_only_changed_properties_for_existing_rows() {
        let shadow = ShadowDocument::from_synced_body(
            locality_core::model::RemoteId::new("row-1"),
            "",
            3,
            std::iter::empty::<locality_core::model::RemoteId>(),
        )
        .expect("shadow")
        .with_frontmatter("loc:\n  id: row-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Task\nFormula: computed\nStatus: Todo\n");
        let parsed = parse_canonical_markdown(
            "---\nloc:\n  id: row-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Task\nFormula: computed\nStatus: Blocked\n---\n# Body\n",
        )
        .expect("parse row");

        let report =
            validate_changed_row_frontmatter(schema_yaml(), &shadow, &parsed, "Tasks/task.md");

        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].code, "notion_schema_option_unknown");
    }

    #[test]
    fn rejects_multi_data_source_schema_until_selection_exists() {
        let parsed =
            parse_canonical_markdown("---\ntitle: New task\n---\n# Body\n").expect("parse row");
        let schema = format!(
            "{}\n  - id: \"source-2\"\n    properties:\n      Name:\n        type: \"title\"\n",
            schema_yaml()
        );

        let report = validate_create_row_frontmatter(&schema, &parsed, "Tasks/new.md");

        assert_eq!(report.issues[0].code, "notion_schema_ambiguous_data_source");
    }

    #[test]
    fn rejects_title_property_by_notion_name() {
        let parsed = parse_canonical_markdown(
            "---\ntitle: New task\nName: Conflicting title\n---\n# Body\n",
        )
        .expect("parse row");

        let report = validate_create_row_frontmatter(schema_yaml(), &parsed, "Tasks/new.md");

        assert_eq!(
            report.issues[0].code,
            "notion_schema_title_property_reserved"
        );
    }

    fn schema_yaml() -> &'static str {
        r#"loc:
  type: notion_database_schema
  database_id: "database-1"
title: "Tasks"
data_sources:
  - id: "source-1"
    name: "Tasks"
    properties:
      Name:
        id: "title-id"
        type: "title"
      Status:
        id: "status-id"
        type: "select"
        options:
          - name: "Todo"
            id: "todo-id"
      Tags:
        id: "tags-id"
        type: "multi_select"
        options:
          - name: "Backend"
            id: "backend-id"
      Done:
        id: "done-id"
        type: "checkbox"
      Points:
        id: "points-id"
        type: "number"
      Due:
        id: "due-id"
        type: "date"
      URL:
        id: "url-id"
        type: "url"
      Email:
        id: "email-id"
        type: "email"
      Phone:
        id: "phone-id"
        type: "phone_number"
      Files:
        id: "files-id"
        type: "files"
      People:
        id: "people-id"
        type: "people"
      Relation:
        id: "relation-id"
        type: "relation"
      Formula:
        id: "formula-id"
        type: "formula"
"#
    }
}
