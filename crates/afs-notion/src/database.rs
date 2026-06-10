//! Notion database schema projection.
//!
//! Databases are structural directories in AgentFS. Their queryable data
//! sources provide the row pages and the property schema that `_schema.yaml`
//! mirrors for future frontmatter validation and row creation.

use afs_core::AfsResult;

use crate::client::NotionApi;
use crate::dto::{DataSourceDto, DataSourcePropertyDto, DatabaseDto, SelectOptionDto};
use crate::render::rich_text_plain_text;

pub fn database_schema_yaml(api: &dyn NotionApi, database_id: &str) -> AfsResult<String> {
    let database = api.retrieve_database(database_id)?;
    let data_sources = database
        .data_sources
        .iter()
        .map(|summary| api.retrieve_data_source(&summary.id))
        .collect::<AfsResult<Vec<_>>>()?;

    Ok(render_database_schema(&database, &data_sources))
}

fn render_database_schema(database: &DatabaseDto, data_sources: &[DataSourceDto]) -> String {
    let mut out = String::new();
    out.push_str("afs:\n");
    out.push_str("  type: notion_database_schema\n");
    out.push_str(&format!("  database_id: {}\n", yaml_string(&database.id)));
    out.push_str(&format!(
        "title: {}\n",
        yaml_string(&rich_text_plain_text(&database.title))
    ));
    if data_sources.is_empty() {
        out.push_str("data_sources: []\n");
        return out;
    }

    out.push_str("data_sources:\n");
    for data_source in data_sources {
        out.push_str(&format!("  - id: {}\n", yaml_string(&data_source.id)));
        out.push_str(&format!(
            "    name: {}\n",
            yaml_string(data_source.name.as_deref().unwrap_or(""))
        ));
        if data_source.properties.is_empty() {
            out.push_str("    properties: {}\n");
            continue;
        }

        out.push_str("    properties:\n");
        for (name, property) in &data_source.properties {
            out.push_str(&format!("      {}:\n", yaml_string(name)));
            out.push_str(&format!("        id: {}\n", yaml_string(&property.id)));
            out.push_str(&format!("        type: {}\n", yaml_string(&property.kind)));
            render_property_options(property, &mut out);
        }
    }

    out
}

fn render_property_options(property: &DataSourcePropertyDto, out: &mut String) {
    let options = match property.kind.as_str() {
        "select" => property
            .select
            .as_ref()
            .map(|schema| schema.options.as_slice()),
        "multi_select" => property
            .multi_select
            .as_ref()
            .map(|schema| schema.options.as_slice()),
        "status" => property
            .status
            .as_ref()
            .map(|schema| schema.options.as_slice()),
        _ => None,
    };

    let Some(options) = options else {
        return;
    };

    if options.is_empty() {
        out.push_str("        options: []\n");
        return;
    }

    out.push_str("        options:\n");
    for option in options {
        render_option(option, out);
    }
}

fn render_option(option: &SelectOptionDto, out: &mut String) {
    out.push_str(&format!(
        "          - name: {}\n",
        yaml_string(&option.name)
    ));
    out.push_str(&format!("            id: {}\n", yaml_string(&option.id)));
    if let Some(color) = option.color.as_deref() {
        out.push_str(&format!("            color: {}\n", yaml_string(color)));
    }
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
    use std::collections::BTreeMap;

    use crate::dto::{
        DataSourceDto, DataSourcePropertyDto, DatabaseDto, SelectOptionDto, SelectPropertySchemaDto,
    };

    use super::render_database_schema;

    #[test]
    fn renders_database_schema_with_select_options() {
        let database = DatabaseDto {
            id: "database-1".to_string(),
            ..Default::default()
        };
        let data_source = DataSourceDto {
            id: "source-1".to_string(),
            name: Some("Tasks".to_string()),
            properties: BTreeMap::from([(
                "Status".to_string(),
                DataSourcePropertyDto {
                    id: "status-id".to_string(),
                    kind: "select".to_string(),
                    select: Some(SelectPropertySchemaDto {
                        options: vec![SelectOptionDto {
                            id: "todo-id".to_string(),
                            name: "Todo".to_string(),
                            color: Some("red".to_string()),
                        }],
                    }),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };

        let schema = render_database_schema(&database, &[data_source]);

        assert!(schema.contains("database_id: \"database-1\""));
        assert!(schema.contains("name: \"Tasks\""));
        assert!(schema.contains("\"Status\":"));
        assert!(schema.contains("type: \"select\""));
        assert!(schema.contains("name: \"Todo\""));
    }
}
