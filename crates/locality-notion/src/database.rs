//! Notion database schema projection.
//!
//! Databases are structural directories in Locality. Their queryable data
//! sources provide the row pages and the property schema that `_schema.yaml`
//! mirrors for future frontmatter validation and row creation.

use std::collections::BTreeMap;

use locality_core::LocalityError;
use locality_core::LocalityResult;
use serde::Serialize;

use crate::client::NotionApi;
use crate::dto::{
    DataSourceDto, DataSourcePropertyDto, DatabaseDto, NotionDatabaseBundle, SelectOptionDto,
};
use crate::render::rich_text_plain_text;

pub fn database_schema_yaml(api: &dyn NotionApi, database_id: &str) -> LocalityResult<String> {
    let database = api.retrieve_database(database_id)?;
    let data_sources = database
        .data_sources
        .iter()
        .map(|summary| api.retrieve_data_source(&summary.id))
        .collect::<LocalityResult<Vec<_>>>()?;

    Ok(render_database_schema(&database, &data_sources))
}

/// Retrieves a database and every data-source schema it declares without search.
///
/// Notion can spell the same UUID with or without hyphens. Equivalent duplicate
/// summaries are retrieved once and retain their first declared position;
/// conflicting duplicates and responses that do not belong to the requested
/// database fail closed.
pub fn fetch_database_bundle(
    api: &dyn NotionApi,
    database_id: &str,
) -> LocalityResult<NotionDatabaseBundle> {
    let requested_database_id = canonical_notion_uuid(database_id).ok_or_else(|| {
        LocalityError::InvalidState(format!(
            "Notion database bundle requires a canonical UUID, got `{database_id}`"
        ))
    })?;

    let database = api.retrieve_database(database_id)?;
    let returned_database_id = canonical_notion_uuid(&database.id).ok_or_else(|| {
        LocalityError::InvalidState(format!(
            "Notion database bundle returned a non-canonical database ID `{}`",
            database.id
        ))
    })?;
    if returned_database_id != requested_database_id {
        return Err(LocalityError::InvalidState(format!(
            "Notion database bundle returned database `{}` for requested database `{database_id}`",
            database.id
        )));
    }

    let mut summaries = Vec::new();
    let mut summary_positions = BTreeMap::<String, usize>::new();
    for summary in &database.data_sources {
        if summary.id.trim().is_empty() {
            return Err(LocalityError::InvalidState(format!(
                "Notion database `{}` contains a data source without an ID",
                database.id
            )));
        }
        let canonical_id = canonical_notion_uuid(&summary.id).ok_or_else(|| {
            LocalityError::InvalidState(format!(
                "Notion database `{}` contains non-canonical data source ID `{}`",
                database.id, summary.id
            ))
        })?;
        if let Some(position) = summary_positions.get(&canonical_id).copied() {
            let existing: &crate::dto::DataSourceSummaryDto = &summaries[position];
            if existing.name != summary.name {
                return Err(LocalityError::InvalidState(format!(
                    "Notion database `{}` contains conflicting summaries for data source `{}`",
                    database.id, summary.id
                )));
            }
            continue;
        }
        summary_positions.insert(canonical_id, summaries.len());
        summaries.push(summary.clone());
    }

    let mut data_sources = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let data_source = api.retrieve_data_source(&summary.id)?;
        let returned_data_source_id = canonical_notion_uuid(&data_source.id).ok_or_else(|| {
            LocalityError::InvalidState(format!(
                "Notion database `{}` returned non-canonical data source ID `{}`",
                database.id, data_source.id
            ))
        })?;
        let declared_data_source_id =
            canonical_notion_uuid(&summary.id).expect("validated database data-source summary ID");
        if returned_data_source_id != declared_data_source_id {
            return Err(LocalityError::InvalidState(format!(
                "Notion database `{}` returned data source `{}` for declared data source `{}`",
                database.id, data_source.id, summary.id
            )));
        }
        let parent_database_id = data_source
            .parent
            .as_ref()
            .and_then(|parent| parent.database_id.as_deref())
            .ok_or_else(|| {
                LocalityError::InvalidState(format!(
                    "Notion data source `{}` did not expose its parent database",
                    data_source.id
                ))
            })?;
        let parent_database_id_canonical = canonical_notion_uuid(parent_database_id).ok_or_else(|| {
            LocalityError::InvalidState(format!(
                "Notion data source `{}` exposed non-canonical parent database ID `{parent_database_id}`",
                data_source.id
            ))
        })?;
        if parent_database_id_canonical != returned_database_id {
            return Err(LocalityError::InvalidState(format!(
                "Notion data source `{}` belongs to database `{parent_database_id}`, not `{}`",
                data_source.id, database.id
            )));
        }
        data_sources.push(data_source);
    }

    Ok(NotionDatabaseBundle {
        database,
        data_sources,
    })
}

/// Renders the same exact `_schema.yaml` bytes as the direct database path.
pub fn render_database_bundle_schema(bundle: &NotionDatabaseBundle) -> String {
    render_database_schema(&bundle.database, &bundle.data_sources)
}

/// Returns the opaque, deterministic provider-version material for a bundle.
///
/// A database container's edit time alone does not cover changes to its data
/// source schemas, so the material includes the exact IDs and edit times for
/// both layers in bundle order.
pub fn database_bundle_provider_version(bundle: &NotionDatabaseBundle) -> LocalityResult<String> {
    #[derive(Serialize)]
    struct ObjectVersion<'a> {
        id: &'a str,
        last_edited_time: Option<&'a str>,
    }

    #[derive(Serialize)]
    struct DatabaseVersion<'a> {
        format_version: u16,
        database: ObjectVersion<'a>,
        data_sources: Vec<ObjectVersion<'a>>,
    }

    let database_id = canonical_notion_uuid(&bundle.database.id).ok_or_else(|| {
        LocalityError::InvalidState(format!(
            "Notion database bundle contains non-canonical database ID `{}`",
            bundle.database.id
        ))
    })?;
    let data_source_ids = bundle
        .data_sources
        .iter()
        .map(|data_source| {
            canonical_notion_uuid(&data_source.id).ok_or_else(|| {
                LocalityError::InvalidState(format!(
                    "Notion database bundle contains non-canonical data source ID `{}`",
                    data_source.id
                ))
            })
        })
        .collect::<LocalityResult<Vec<_>>>()?;
    let material = DatabaseVersion {
        format_version: 1,
        database: ObjectVersion {
            id: &database_id,
            last_edited_time: bundle.database.last_edited_time.as_deref(),
        },
        data_sources: bundle
            .data_sources
            .iter()
            .zip(&data_source_ids)
            .map(|(data_source, canonical_id)| ObjectVersion {
                id: canonical_id,
                last_edited_time: data_source.last_edited_time.as_deref(),
            })
            .collect(),
    };

    serde_json::to_string(&material).map_err(|error| {
        LocalityError::Io(format!(
            "Notion database provider version encode failed: {error}"
        ))
    })
}

fn canonical_notion_uuid(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let valid = match bytes.len() {
        32 => bytes.iter().all(u8::is_ascii_hexdigit),
        36 => bytes.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        }),
        _ => false,
    };
    valid.then(|| {
        value
            .bytes()
            .filter(|byte| *byte != b'-')
            .map(|byte| char::from(byte.to_ascii_lowercase()))
            .collect()
    })
}

fn render_database_schema(database: &DatabaseDto, data_sources: &[DataSourceDto]) -> String {
    let mut out = String::new();
    out.push_str("loc:\n");
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
    use std::sync::Mutex;

    use locality_core::{LocalityError, LocalityResult};

    use crate::client::NotionApi;
    use crate::dto::{
        BlockDto, BlockListDto, DataSourceDto, DataSourcePropertyDto, DataSourceSummaryDto,
        DatabaseDto, PageDto, PageListDto, ParentDto, RichTextDto, SelectOptionDto,
        SelectPropertySchemaDto,
    };

    use super::{
        database_bundle_provider_version, fetch_database_bundle, render_database_bundle_schema,
        render_database_schema,
    };

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

        assert_eq!(
            schema,
            concat!(
                "loc:\n",
                "  type: notion_database_schema\n",
                "  database_id: \"database-1\"\n",
                "title: \"\"\n",
                "data_sources:\n",
                "  - id: \"source-1\"\n",
                "    name: \"Tasks\"\n",
                "    properties:\n",
                "      \"Status\":\n",
                "        id: \"status-id\"\n",
                "        type: \"select\"\n",
                "        options:\n",
                "          - name: \"Todo\"\n",
                "            id: \"todo-id\"\n",
                "            color: \"red\"\n",
            )
        );
    }

    #[test]
    fn database_bundle_is_ordered_deduplicated_and_exactly_versioned() {
        let database_id = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let database = DatabaseDto {
            id: "AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA".to_string(),
            last_edited_time: Some("2026-07-21T01:02:03.000Z".to_string()),
            title: vec![plain_text("Roadmap")],
            data_sources: vec![
                DataSourceSummaryDto {
                    id: "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_string(),
                    name: Some("Tasks".to_string()),
                },
                DataSourceSummaryDto {
                    id: "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB".to_string(),
                    name: Some("Tasks".to_string()),
                },
                DataSourceSummaryDto {
                    id: "cccccccc-cccc-cccc-cccc-cccccccccccc".to_string(),
                    name: Some("Archive".to_string()),
                },
            ],
            ..Default::default()
        };
        let api = FixtureApi::new(
            database,
            [
                (
                    "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                    data_source(
                        "BBBBBBBB-BBBB-BBBB-BBBB-BBBBBBBBBBBB",
                        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                        "Tasks",
                        Some("2026-07-21T01:03:00.000Z"),
                        BTreeMap::from([(
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
                    ),
                ),
                (
                    "cccccccc-cccc-cccc-cccc-cccccccccccc",
                    data_source(
                        "cccccccccccccccccccccccccccccccc",
                        database_id,
                        "Archive",
                        None,
                        BTreeMap::new(),
                    ),
                ),
            ],
        );

        let bundle = fetch_database_bundle(&api, database_id).expect("database bundle");

        assert_eq!(
            api.retrieved_data_sources.into_inner().expect("calls"),
            vec![
                "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_string(),
                "cccccccc-cccc-cccc-cccc-cccccccccccc".to_string(),
            ]
        );
        assert_eq!(
            bundle
                .data_sources
                .iter()
                .map(|data_source| data_source.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "BBBBBBBB-BBBB-BBBB-BBBB-BBBBBBBBBBBB",
                "cccccccccccccccccccccccccccccccc",
            ]
        );
        assert_eq!(
            render_database_bundle_schema(&bundle),
            concat!(
                "loc:\n",
                "  type: notion_database_schema\n",
                "  database_id: \"AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA\"\n",
                "title: \"Roadmap\"\n",
                "data_sources:\n",
                "  - id: \"BBBBBBBB-BBBB-BBBB-BBBB-BBBBBBBBBBBB\"\n",
                "    name: \"Tasks\"\n",
                "    properties:\n",
                "      \"Status\":\n",
                "        id: \"status-id\"\n",
                "        type: \"select\"\n",
                "        options:\n",
                "          - name: \"Todo\"\n",
                "            id: \"todo-id\"\n",
                "            color: \"red\"\n",
                "  - id: \"cccccccccccccccccccccccccccccccc\"\n",
                "    name: \"Archive\"\n",
                "    properties: {}\n",
            )
        );
        let provider_version = database_bundle_provider_version(&bundle).expect("provider version");
        assert_eq!(
            provider_version,
            concat!(
                "{\"format_version\":1,",
                "\"database\":{\"id\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",",
                "\"last_edited_time\":\"2026-07-21T01:02:03.000Z\"},",
                "\"data_sources\":[",
                "{\"id\":\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\",",
                "\"last_edited_time\":\"2026-07-21T01:03:00.000Z\"},",
                "{\"id\":\"cccccccccccccccccccccccccccccccc\",",
                "\"last_edited_time\":null}]}",
            )
        );

        let mut alternate_spelling = bundle.clone();
        alternate_spelling.database.id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        alternate_spelling.data_sources[0].id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        alternate_spelling.data_sources[1].id = "CCCCCCCC-CCCC-CCCC-CCCC-CCCCCCCCCCCC".to_string();
        assert_eq!(
            database_bundle_provider_version(&alternate_spelling).expect("alternate version"),
            provider_version
        );
    }

    #[test]
    fn database_bundle_rejects_conflicting_duplicate_summaries() {
        let database = DatabaseDto {
            id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            data_sources: vec![
                DataSourceSummaryDto {
                    id: "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_string(),
                    name: Some("Tasks".to_string()),
                },
                DataSourceSummaryDto {
                    id: "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB".to_string(),
                    name: Some("Other".to_string()),
                },
            ],
            ..Default::default()
        };
        let api = FixtureApi::new(database, []);

        let error = fetch_database_bundle(&api, "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
            .expect_err("conflicting summary");

        assert_eq!(
            error,
            LocalityError::InvalidState(
                "Notion database `aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa` contains conflicting summaries for data source `BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB`".to_string()
            )
        );
        assert!(
            api.retrieved_data_sources
                .into_inner()
                .expect("calls")
                .is_empty()
        );
    }

    #[test]
    fn database_bundle_rejects_data_source_from_another_database() {
        let database = DatabaseDto {
            id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            data_sources: vec![DataSourceSummaryDto {
                id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
                name: Some("Tasks".to_string()),
            }],
            ..Default::default()
        };
        let api = FixtureApi::new(
            database,
            [(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                data_source(
                    "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                    "cccccccc-cccc-cccc-cccc-cccccccccccc",
                    "Tasks",
                    Some("version-1"),
                    BTreeMap::new(),
                ),
            )],
        );

        let error = fetch_database_bundle(&api, "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
            .expect_err("foreign data source");

        assert_eq!(
            error,
            LocalityError::InvalidState(
                "Notion data source `bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb` belongs to database `cccccccc-cccc-cccc-cccc-cccccccccccc`, not `aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa`".to_string()
            )
        );
    }

    #[test]
    fn database_bundle_rejects_non_uuid_identity_spelling() {
        let api = FixtureApi::new(DatabaseDto::default(), []);

        let error = fetch_database_bundle(&api, "database-1").expect_err("non-UUID database ID");

        assert_eq!(
            error,
            LocalityError::InvalidState(
                "Notion database bundle requires a canonical UUID, got `database-1`".to_string()
            )
        );
    }

    fn plain_text(value: &str) -> RichTextDto {
        RichTextDto {
            kind: "text".to_string(),
            plain_text: value.to_string(),
            ..Default::default()
        }
    }

    fn data_source(
        id: &str,
        database_id: &str,
        name: &str,
        last_edited_time: Option<&str>,
        properties: BTreeMap<String, DataSourcePropertyDto>,
    ) -> DataSourceDto {
        DataSourceDto {
            id: id.to_string(),
            parent: Some(ParentDto {
                kind: "database_id".to_string(),
                database_id: Some(database_id.to_string()),
                ..Default::default()
            }),
            name: Some(name.to_string()),
            last_edited_time: last_edited_time.map(str::to_string),
            properties,
            ..Default::default()
        }
    }

    #[derive(Debug)]
    struct FixtureApi {
        database: DatabaseDto,
        data_sources: BTreeMap<String, DataSourceDto>,
        retrieved_data_sources: Mutex<Vec<String>>,
    }

    impl FixtureApi {
        fn new<'a>(
            database: DatabaseDto,
            data_sources: impl IntoIterator<Item = (&'a str, DataSourceDto)>,
        ) -> Self {
            Self {
                database,
                data_sources: data_sources
                    .into_iter()
                    .map(|(id, data_source)| (id.to_string(), data_source))
                    .collect(),
                retrieved_data_sources: Mutex::new(Vec::new()),
            }
        }
    }

    impl NotionApi for FixtureApi {
        fn retrieve_page(&self, page_id: &str) -> LocalityResult<PageDto> {
            Err(LocalityError::RemoteNotFound(page_id.to_string()))
        }

        fn retrieve_database(&self, _database_id: &str) -> LocalityResult<DatabaseDto> {
            Ok(self.database.clone())
        }

        fn retrieve_data_source(&self, data_source_id: &str) -> LocalityResult<DataSourceDto> {
            self.retrieved_data_sources
                .lock()
                .expect("record data-source call")
                .push(data_source_id.to_string());
            self.data_sources
                .get(data_source_id)
                .cloned()
                .ok_or_else(|| LocalityError::RemoteNotFound(data_source_id.to_string()))
        }

        fn retrieve_block_children(
            &self,
            _block_id: &str,
            _start_cursor: Option<&str>,
        ) -> LocalityResult<BlockListDto> {
            Err(LocalityError::NotImplemented("fixture block children"))
        }

        fn search_pages(&self, _start_cursor: Option<&str>) -> LocalityResult<PageListDto> {
            Err(LocalityError::NotImplemented("fixture page search"))
        }

        fn update_block(
            &self,
            _block_id: &str,
            _body: serde_json::Value,
        ) -> LocalityResult<BlockDto> {
            Err(LocalityError::NotImplemented("fixture block update"))
        }

        fn append_block_children(
            &self,
            _block_id: &str,
            _body: serde_json::Value,
        ) -> LocalityResult<BlockListDto> {
            Err(LocalityError::NotImplemented("fixture block append"))
        }

        fn delete_block(&self, _block_id: &str) -> LocalityResult<BlockDto> {
            Err(LocalityError::NotImplemented("fixture block delete"))
        }
    }
}
