use std::collections::BTreeSet;

use locality_connector::{Connector, FetchRequest};
use locality_core::canonical::{
    Frontmatter, LocalityMetadata, ParsedCanonicalDocument, parse_canonical_markdown,
    render_canonical_markdown,
};
use locality_core::hydration::HydrationRequest;
use locality_core::model::{CanonicalDocument, RemoteId};
use locality_core::shadow::{ShadowDocument, rendered_bodies_equivalent, segment_markdown_body};
use locality_core::validation::{ValidationIssue, ValidationReport};
use locality_core::{LocalityError, LocalityResult};
use locality_linear::{
    LINEAR_CONNECTOR_ID, LinearAttachmentDownload, LinearConfig, LinearConnector,
    LinearIssueContext, LinearIssueContextKind, LinearNativeBundle, attachment_local_path,
    context_remote_version, remote_version, render_linear_issue, render_linear_issue_context,
};
use locality_store::{
    ConnectionRecord, ConnectionRepository, ConnectorProfileRepository, CredentialError,
    CredentialStore, MountConfig,
};

use crate::hydration::{HydratedAsset, HydratedEntity, HydrationSource};
use crate::notion::ConnectorResolveError;
use crate::source::{SourceAdapter, SourcePushValidator, SourceValidationContext};

pub(crate) const LINEAR_CONNECT_COMMAND: &str = "loc connect linear --api-key-stdin";
const MAX_LINEAR_ATTACHMENT_DOWNLOAD_BYTES: u64 = 25 * 1024 * 1024;

pub fn resolve_linear_connector_for_mount<S>(
    store: &S,
    credentials: &dyn CredentialStore,
    mount: &MountConfig,
) -> Result<LinearConnector, ConnectorResolveError>
where
    S: ConnectionRepository + ConnectorProfileRepository + ?Sized,
{
    if mount.connector != LINEAR_CONNECTOR_ID {
        return Err(ConnectorResolveError::UnsupportedConnector(
            mount.connector.clone(),
        ));
    }

    let connection = if let Some(connection_id) = &mount.connection_id {
        store
            .get_connection(connection_id)
            .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?
            .ok_or_else(|| ConnectorResolveError::MissingConnection {
                message: format!("connection `{}` was not found", connection_id.0),
                suggested_command: LINEAR_CONNECT_COMMAND.to_string(),
            })?
    } else {
        let active = active_connections(store)?;
        if active.len() != 1 {
            let message = if active.is_empty() {
                "missing Linear connection; run `loc connect linear --api-key-stdin`".to_string()
            } else {
                "mount has no connection_id and multiple Linear connections exist".to_string()
            };
            return Err(ConnectorResolveError::MissingConnection {
                message,
                suggested_command: LINEAR_CONNECT_COMMAND.to_string(),
            });
        }
        active.into_iter().next().expect("one active connection")
    };

    validate_connection_record(&connection)?;
    validate_profile(store, &connection)?;
    connector_from_connection(credentials, &connection)
}

fn active_connections<S>(store: &S) -> Result<Vec<ConnectionRecord>, ConnectorResolveError>
where
    S: ConnectionRepository + ?Sized,
{
    Ok(store
        .list_connections()
        .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?
        .into_iter()
        .filter(|connection| {
            connection.connector == LINEAR_CONNECTOR_ID
                && connection.status == "active"
                && connection.auth_kind == "api_key"
        })
        .collect())
}

fn validate_profile<S>(
    store: &S,
    connection: &ConnectionRecord,
) -> Result<(), ConnectorResolveError>
where
    S: ConnectorProfileRepository + ?Sized,
{
    let Some(profile_id) = &connection.profile_id else {
        return Ok(());
    };
    let profile = store
        .get_connector_profile(profile_id)
        .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?
        .ok_or_else(|| ConnectorResolveError::AuthProfileUnavailable {
            profile_id: profile_id.0.clone(),
            suggested_command: LINEAR_CONNECT_COMMAND.to_string(),
        })?;
    if profile.status != "active"
        || profile.connector != LINEAR_CONNECTOR_ID
        || profile.auth_kind != "api_key"
    {
        return Err(ConnectorResolveError::AuthProfileUnavailable {
            profile_id: profile.profile_id.0,
            suggested_command: LINEAR_CONNECT_COMMAND.to_string(),
        });
    }
    Ok(())
}

fn validate_connection_record(connection: &ConnectionRecord) -> Result<(), ConnectorResolveError> {
    if connection.status != "active" {
        return Err(ConnectorResolveError::ConnectionRevoked {
            connection_id: connection.connection_id.0.clone(),
            suggested_command: LINEAR_CONNECT_COMMAND.to_string(),
        });
    }
    if connection.connector != LINEAR_CONNECTOR_ID {
        return Err(ConnectorResolveError::UnsupportedConnector(
            connection.connector.clone(),
        ));
    }
    if connection.auth_kind != "api_key" {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: Some("Linear connections require an API key".to_string()),
            suggested_command: LINEAR_CONNECT_COMMAND.to_string(),
        });
    }
    Ok(())
}

fn connector_from_connection(
    credentials: &dyn CredentialStore,
    connection: &ConnectionRecord,
) -> Result<LinearConnector, ConnectorResolveError> {
    if connection.status != "active" {
        return Err(ConnectorResolveError::ConnectionRevoked {
            connection_id: connection.connection_id.0.clone(),
            suggested_command: LINEAR_CONNECT_COMMAND.to_string(),
        });
    }
    if connection.connector != LINEAR_CONNECTOR_ID {
        return Err(ConnectorResolveError::UnsupportedConnector(
            connection.connector.clone(),
        ));
    }
    if connection.auth_kind != "api_key" {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: Some("Linear connections require an API key".to_string()),
            suggested_command: LINEAR_CONNECT_COMMAND.to_string(),
        });
    }
    let token = credentials
        .get(&connection.secret_ref)
        .map_err(|error| credential_error(connection, error))?;
    Ok(LinearConnector::new(LinearConfig::new(token)))
}

fn credential_error(
    connection: &ConnectionRecord,
    error: CredentialError,
) -> ConnectorResolveError {
    match error {
        CredentialError::NotFound(_) => ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: None,
            suggested_command: LINEAR_CONNECT_COMMAND.to_string(),
        },
        CredentialError::Unavailable(message) | CredentialError::Io(message) => {
            ConnectorResolveError::CredentialStoreUnavailable(message)
        }
    }
}

impl SourcePushValidator for LinearConnector {}
impl SourceAdapter for LinearConnector {}

impl HydrationSource for LinearConnector {
    fn fetch_render(&self, request: &HydrationRequest) -> LocalityResult<HydratedEntity> {
        let native = self.fetch(FetchRequest {
            remote_id: request.remote_id.clone(),
        })?;
        let bundle = serde_json::from_slice::<LinearNativeBundle>(&native.raw)
            .map_err(|error| LocalityError::Io(format!("Linear native decode failed: {error}")))?;
        if let Some(context) = bundle.context {
            let mut context_value = context.context;
            let assets = if context.kind == LinearIssueContextKind::Attachments {
                download_linear_attachment_assets(self, &mut context_value)
            } else {
                Vec::new()
            };
            let document = render_linear_issue_context(&context_value, context.kind)?;
            return hydrated_linear_document(
                request,
                document,
                Some(context_remote_version(&context_value, context.kind)),
                assets,
            );
        }

        hydrated_linear_document(
            request,
            render_linear_issue(&bundle.issue)?,
            Some(remote_version(&bundle.issue)),
            Vec::new(),
        )
    }

    fn fetch_database_schema_yaml(
        &self,
        _database_id: &RemoteId,
    ) -> LocalityResult<Option<String>> {
        Ok(None)
    }
}

fn hydrated_linear_document(
    request: &HydrationRequest,
    document: CanonicalDocument,
    remote_edited_at: Option<String>,
    assets: Vec<HydratedAsset>,
) -> LocalityResult<HydratedEntity> {
    let block_ids: Vec<RemoteId> = segment_markdown_body(&document.body, 1)
        .into_iter()
        .filter(|block| !block.is_directive())
        .enumerate()
        .map(|(index, _)| RemoteId::new(format!("{}:body:{index}", request.remote_id.0)))
        .collect();
    let shadow = ShadowDocument::from_synced_body(
        request.remote_id.clone(),
        document.body.clone(),
        1,
        block_ids,
    )
    .map_err(|error| LocalityError::InvalidState(error.to_string()))?
    .with_frontmatter(document.frontmatter.clone());
    Ok(HydratedEntity {
        document,
        shadow,
        remote_edited_at,
        assets,
    })
}

fn download_linear_attachment_assets(
    connector: &LinearConnector,
    context: &mut LinearIssueContext,
) -> Vec<HydratedAsset> {
    let mut assets = Vec::new();
    for attachment in &mut context.attachments {
        if !is_http_url(&attachment.url) {
            attachment.download = Some(LinearAttachmentDownload {
                status: "skipped".to_string(),
                local_path: None,
                error: Some("only HTTP(S) attachment URLs can be downloaded".to_string()),
            });
            continue;
        }
        let local_path = attachment_local_path(
            &context.issue_id,
            &attachment.id,
            &attachment.title,
            &attachment.url,
        );
        match connector.download_attachment(&attachment.url, MAX_LINEAR_ATTACHMENT_DOWNLOAD_BYTES) {
            Ok(bytes) => {
                attachment.download = Some(LinearAttachmentDownload {
                    status: "downloaded".to_string(),
                    local_path: Some(local_path.to_string_lossy().replace('\\', "/")),
                    error: None,
                });
                assets.push(HydratedAsset {
                    path: local_path,
                    bytes,
                    media: None,
                });
            }
            Err(error) => {
                attachment.download = Some(LinearAttachmentDownload {
                    status: if matches!(
                        error,
                        LocalityError::Guardrail(_) | LocalityError::Unsupported(_)
                    ) {
                        "skipped".to_string()
                    } else {
                        "failed".to_string()
                    },
                    local_path: None,
                    error: Some(error.to_string()),
                });
            }
        }
    }
    assets
}

fn is_http_url(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use locality_core::hydration::{HydrationReason, HydrationRequest};
    use locality_core::model::{HydrationState, MountId};
    use locality_linear::{
        LinearApi, LinearAttachment, LinearComment, LinearIssue, LinearIssueContext,
        LinearIssueHistoryEntry, LinearIssuePage, LinearIssuePriority, LinearIssueState,
        LinearIssueUpdateInput, LinearLabel, LinearProject, LinearTeam, LinearUser,
    };

    use super::*;

    #[test]
    fn linear_hydration_downloads_attachment_sidecar_assets_and_renders_status() {
        let issue = issue();
        let context = issue_context(&issue);
        let api = Arc::new(
            FakeLinearApi::new(issue, context)
                .with_download("https://files.linear.app/spec.pdf", b"pdf-bytes"),
        );
        let connector = LinearConnector::with_api(LinearConfig::new("secret"), api.clone());
        let request = HydrationRequest::new(
            MountId::new("linear-main"),
            RemoteId::new("linear-context:issue-1:attachments"),
            "Teams/Engineering/Issues/Todo/ENG-1 Improve sync/attachments.md",
            HydrationState::Hydrated,
            HydrationReason::ExplicitPull,
        );

        let hydrated = connector.fetch_render(&request).expect("hydrate sidecar");

        assert_eq!(hydrated.assets.len(), 1);
        assert_eq!(hydrated.assets[0].bytes, b"pdf-bytes");
        assert_eq!(
            hydrated.assets[0].path,
            attachment_local_path(
                "issue-1",
                "attach-file",
                "Spec PDF",
                "https://files.linear.app/spec.pdf"
            )
        );
        assert!(
            hydrated
                .document
                .body
                .contains("- download_status: downloaded")
        );
        assert!(
            hydrated
                .document
                .body
                .contains("- download_status: skipped")
        );
        assert!(
            hydrated
                .document
                .body
                .contains("only HTTP(S) attachment URLs can be downloaded")
        );
        assert_eq!(
            api.download_calls.lock().unwrap().as_slice(),
            &["https://files.linear.app/spec.pdf".to_string()]
        );
    }

    #[derive(Debug)]
    struct FakeLinearApi {
        issue: LinearIssue,
        context: LinearIssueContext,
        downloads: Mutex<BTreeMap<String, Vec<u8>>>,
        download_calls: Mutex<Vec<String>>,
    }

    impl FakeLinearApi {
        fn new(issue: LinearIssue, context: LinearIssueContext) -> Self {
            Self {
                issue,
                context,
                downloads: Mutex::new(BTreeMap::new()),
                download_calls: Mutex::new(Vec::new()),
            }
        }

        fn with_download(self, url: &str, bytes: &[u8]) -> Self {
            self.downloads
                .lock()
                .unwrap()
                .insert(url.to_string(), bytes.to_vec());
            self
        }
    }

    impl LinearApi for FakeLinearApi {
        fn list_issues(
            &self,
            _cursor: Option<&str>,
            _updated_after: Option<&str>,
            _team_id: Option<&str>,
        ) -> LocalityResult<LinearIssuePage> {
            Ok(LinearIssuePage {
                issues: vec![self.issue.clone()],
                has_next_page: false,
                end_cursor: None,
            })
        }

        fn get_issue(&self, issue_id: &str) -> LocalityResult<LinearIssue> {
            if self.issue.id == issue_id {
                Ok(self.issue.clone())
            } else {
                Err(LocalityError::RemoteNotFound(issue_id.to_string()))
            }
        }

        fn get_issue_context(&self, issue_id: &str) -> LocalityResult<LinearIssueContext> {
            if self.context.issue_id == issue_id {
                Ok(self.context.clone())
            } else {
                Err(LocalityError::RemoteNotFound(issue_id.to_string()))
            }
        }

        fn download_attachment(&self, url: &str, _max_bytes: u64) -> LocalityResult<Vec<u8>> {
            self.download_calls.lock().unwrap().push(url.to_string());
            self.downloads
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .ok_or_else(|| LocalityError::Io("missing fake download".to_string()))
        }

        fn update_issue(&self, _input: LinearIssueUpdateInput) -> LocalityResult<LinearIssue> {
            Err(LocalityError::Unsupported("fake Linear update"))
        }
    }

    fn issue() -> LinearIssue {
        LinearIssue {
            id: "issue-1".to_string(),
            identifier: "ENG-1".to_string(),
            title: "Improve sync".to_string(),
            description: Some("Existing description.".to_string()),
            url: "https://linear.app/acme/issue/ENG-1/improve-sync".to_string(),
            created_at: "2026-07-14T12:00:00Z".to_string(),
            updated_at: "2026-07-15T12:00:00Z".to_string(),
            archived_at: None,
            started_at: None,
            completed_at: None,
            canceled_at: None,
            auto_archived_at: None,
            auto_closed_at: None,
            started_triage_at: None,
            triaged_at: None,
            snoozed_until_at: None,
            added_to_cycle_at: None,
            added_to_project_at: None,
            added_to_team_at: None,
            due_date: None,
            priority: Some(LinearIssuePriority {
                value: 3,
                label: "High".to_string(),
            }),
            estimate: Some(3.0),
            team: LinearTeam {
                id: "team-1".to_string(),
                key: "ENG".to_string(),
                name: "Engineering".to_string(),
            },
            state: LinearIssueState {
                id: "state-1".to_string(),
                name: "Todo".to_string(),
                state_type: Some("unstarted".to_string()),
            },
            project: Some(LinearProject {
                id: "project-1".to_string(),
                name: "Launch".to_string(),
            }),
            assignee: Some(LinearUser {
                id: "user-1".to_string(),
                name: "Ada".to_string(),
                email: Some("ada@example.com".to_string()),
            }),
            labels: vec![LinearLabel {
                id: "label-1".to_string(),
                name: "Bug".to_string(),
            }],
        }
    }

    fn issue_context(issue: &LinearIssue) -> LinearIssueContext {
        LinearIssueContext {
            issue_id: issue.id.clone(),
            issue_identifier: issue.identifier.clone(),
            issue_title: issue.title.clone(),
            issue_updated_at: issue.updated_at.clone(),
            branch_name: "eng-1-improve-sync".to_string(),
            comments: Vec::<LinearComment>::new(),
            attachments: vec![
                LinearAttachment {
                    id: "attach-file".to_string(),
                    title: "Spec PDF".to_string(),
                    url: "https://files.linear.app/spec.pdf".to_string(),
                    created_at: "2026-07-15T14:00:00Z".to_string(),
                    updated_at: "2026-07-15T14:00:00Z".to_string(),
                    source_type: Some("url".to_string()),
                    subtitle: Some("Spec".to_string()),
                    creator: issue.assignee.clone(),
                    external_user_creator: None,
                    metadata: serde_json::json!({ "kind": "file" }),
                    download: None,
                },
                LinearAttachment {
                    id: "attach-skip".to_string(),
                    title: "Local file".to_string(),
                    url: "file:///tmp/spec.pdf".to_string(),
                    created_at: "2026-07-15T15:00:00Z".to_string(),
                    updated_at: "2026-07-15T15:00:00Z".to_string(),
                    source_type: Some("url".to_string()),
                    subtitle: None,
                    creator: None,
                    external_user_creator: None,
                    metadata: serde_json::json!({}),
                    download: None,
                },
            ],
            history: Vec::<LinearIssueHistoryEntry>::new(),
        }
    }
}

const LINEAR_LIFECYCLE_FRONTMATTER_KEYS: &[&str] = &[
    "created_at",
    "updated_at",
    "archived_at",
    "started_at",
    "completed_at",
    "canceled_at",
    "auto_archived_at",
    "auto_closed_at",
    "started_triage_at",
    "triaged_at",
    "snoozed_until_at",
    "added_to_cycle_at",
    "added_to_project_at",
    "added_to_team_at",
    "due_date",
];

pub(crate) fn linear_shadow_matches_with_legacy_lifecycle_frontmatter(
    synced_tree_shadow: &ShadowDocument,
    remote_tree_shadow: &ShadowDocument,
) -> bool {
    if !rendered_bodies_equivalent(
        &synced_tree_shadow.rendered_body,
        &remote_tree_shadow.rendered_body,
    ) {
        return false;
    }
    let Some(synced) = parse_shadow_frontmatter(synced_tree_shadow) else {
        return false;
    };
    let Some(remote) = parse_shadow_frontmatter(remote_tree_shadow) else {
        return false;
    };
    if !loc_metadata_matches_ignoring_sync_metadata(&synced.frontmatter, &remote.frontmatter)
        || synced.frontmatter.title != remote.frontmatter.title
    {
        return false;
    }

    let lifecycle_keys = LINEAR_LIFECYCLE_FRONTMATTER_KEYS
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let mut repaired_missing_lifecycle_key = false;
    for key in synced
        .frontmatter
        .properties
        .keys()
        .chain(remote.frontmatter.properties.keys())
        .collect::<BTreeSet<_>>()
    {
        let synced_value = synced.frontmatter.properties.get(key);
        let remote_value = remote.frontmatter.properties.get(key);
        if lifecycle_keys.contains(key.as_str()) {
            if synced_value.is_none() && remote_value.is_some() {
                repaired_missing_lifecycle_key = true;
                continue;
            }
            if synced_value == remote_value {
                continue;
            }
            return false;
        }
        if synced_value != remote_value {
            return false;
        }
    }

    repaired_missing_lifecycle_key
}

fn parse_shadow_frontmatter(shadow: &ShadowDocument) -> Option<ParsedCanonicalDocument> {
    parse_canonical_markdown(&render_canonical_markdown(&CanonicalDocument::new(
        shadow.frontmatter.clone(),
        shadow.rendered_body.clone(),
    )))
    .ok()
}

fn loc_metadata_matches_ignoring_sync_metadata(left: &Frontmatter, right: &Frontmatter) -> bool {
    match (&left.loc, &right.loc) {
        (None, None) => true,
        (Some(left), Some(right)) => locality_metadata_matches_ignoring_sync_metadata(left, right),
        _ => false,
    }
}

fn locality_metadata_matches_ignoring_sync_metadata(
    left: &LocalityMetadata,
    right: &LocalityMetadata,
) -> bool {
    left.id == right.id
        && left.entity_type == right.entity_type
        && left.raw_entity_type == right.raw_entity_type
        && left.parent == right.parent
}

pub(crate) fn validate_linear_frontmatter(
    context: SourceValidationContext<'_>,
) -> LocalityResult<ValidationReport> {
    let Some(shadow) = context.shadow else {
        return Ok(ValidationReport::clean());
    };
    let mut shadow_markdown = String::from("---\n");
    shadow_markdown.push_str(&shadow.frontmatter);
    if !shadow_markdown.ends_with('\n') {
        shadow_markdown.push('\n');
    }
    shadow_markdown.push_str("---\n");
    let shadow_parsed = parse_canonical_markdown(&shadow_markdown).map_err(|error| {
        LocalityError::InvalidState(format!(
            "synced Linear shadow frontmatter is no longer parseable: {error}"
        ))
    })?;
    let mut report = ValidationReport::clean();
    for key in shadow_parsed
        .frontmatter
        .properties
        .keys()
        .chain(context.parsed.frontmatter.properties.keys())
        .collect::<std::collections::BTreeSet<_>>()
    {
        if matches!(key.as_str(), "Status" | "Project" | "Assignee") {
            continue;
        }
        let synced = shadow_parsed.frontmatter.properties.get(key);
        let edited = context.parsed.frontmatter.properties.get(key);
        if synced != edited {
            report.push(ValidationIssue::new(
                "linear_read_only_frontmatter",
                context.relative_path,
                Some(1),
                format!("Linear frontmatter `{key}` is read-only"),
                Some(format!("restore generated Linear `{key}` frontmatter")),
            ));
        }
    }
    Ok(report)
}

pub(crate) fn validate_linear_create_frontmatter(
    context: SourceValidationContext<'_>,
) -> LocalityResult<ValidationReport> {
    let mut report = ValidationReport::clean();
    report.push(ValidationIssue::new(
        "linear_create_unsupported",
        context.relative_path,
        Some(1),
        "Linear issue creates are not supported yet",
        Some("create the Linear issue remotely, then refresh the mount".to_string()),
    ));
    Ok(report)
}
