use std::path::{Component, Path};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_connector::oauth_broker::OAuthBrokerRefresh;
use locality_connector::{Connector, EnumerateRequest, FetchRequest};
use locality_core::diff::property_value_from_frontmatter;
use locality_core::hydration::HydrationRequest;
use locality_core::model::{RemoteId, TreeEntry};
use locality_core::planner::PropertyValue;
use locality_core::validation::{ValidationIssue, ValidationReport};
use locality_core::{LocalityError, LocalityResult};
use locality_gmail::attachments::{GmailAttachmentSpec, decode_attachment_body};
use locality_gmail::client::GmailApi;
use locality_gmail::render::{
    GmailNativeBundle, GmailThreadNativeBundle, remote_version, render_gmail_message,
    render_gmail_thread, thread_remote_version,
};
use locality_gmail::{
    GMAIL_CONNECTOR_ID, GmailConfig, GmailConnector, GmailMountSettings, GmailOAuthScopeError,
    HttpGmailOAuthBrokerClient, StoredGmailCredential,
};
use locality_store::{
    ConnectionRecord, ConnectionRepository, ConnectorProfileRepository, CredentialError,
    CredentialStore, MountConfig,
};

use crate::hydration::{HydratedAsset, HydratedEntity, HydrationSource};
use crate::notion::ConnectorResolveError;
use crate::source::{SourceAdapter, SourcePushValidator, SourceValidationContext};

const GMAIL_CONNECT_COMMAND: &str = "loc connect gmail";

pub fn resolve_gmail_connector_for_mount<S>(
    store: &S,
    credentials: &dyn CredentialStore,
    mount: &MountConfig,
) -> Result<GmailConnector, ConnectorResolveError>
where
    S: ConnectionRepository + ConnectorProfileRepository + ?Sized,
{
    if mount.connector != GMAIL_CONNECTOR_ID {
        return Err(ConnectorResolveError::UnsupportedConnector(
            mount.connector.clone(),
        ));
    }

    if let Some(connection_id) = &mount.connection_id {
        let connection = store
            .get_connection(connection_id)
            .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?
            .ok_or_else(|| ConnectorResolveError::MissingConnection {
                message: format!("connection `{}` was not found", connection_id.0),
                suggested_command: GMAIL_CONNECT_COMMAND.to_string(),
            })?;
        validate_connection_profile(store, &connection)?;
        return connector_from_connection(credentials, &connection, mount);
    }

    let active = active_gmail_connections(store)?;
    if active.len() == 1 {
        validate_connection_profile(store, &active[0])?;
        return connector_from_connection(credentials, &active[0], mount);
    }

    let message = if active.is_empty() {
        "missing Gmail connection; run `loc connect gmail`".to_string()
    } else {
        "mount has no connection_id and multiple Gmail connections exist".to_string()
    };
    Err(ConnectorResolveError::MissingConnection {
        message,
        suggested_command: GMAIL_CONNECT_COMMAND.to_string(),
    })
}

fn connector_from_connection(
    credentials: &dyn CredentialStore,
    connection: &ConnectionRecord,
    mount: &MountConfig,
) -> Result<GmailConnector, ConnectorResolveError> {
    if connection.connector != GMAIL_CONNECTOR_ID {
        return Err(ConnectorResolveError::UnsupportedConnector(
            connection.connector.clone(),
        ));
    }

    if connection.status != "active" {
        return Err(ConnectorResolveError::ConnectionRevoked {
            connection_id: connection.connection_id.0.clone(),
            suggested_command: GMAIL_CONNECT_COMMAND.to_string(),
        });
    }

    if connection.auth_kind != "oauth" {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: Some(format!(
                "Gmail connection `{}` must use OAuth credentials",
                connection.connection_id.0
            )),
            suggested_command: GMAIL_CONNECT_COMMAND.to_string(),
        });
    }

    let token = connection_access_token(credentials, connection)?;
    Ok(GmailConnector::new(gmail_config_from_mount(token, mount)?))
}

fn gmail_config_from_mount(
    token: String,
    mount: &MountConfig,
) -> Result<GmailConfig, ConnectorResolveError> {
    let settings = GmailMountSettings::from_json(&mount.settings_json).map_err(|error| {
        ConnectorResolveError::CredentialStoreUnavailable(format!(
            "Gmail mount `{}` settings are invalid: {}",
            mount.mount_id.0,
            gmail_settings_error_message(error)
        ))
    })?;
    Ok(GmailConfig::new(token).with_settings(settings))
}

fn gmail_settings_error_message(error: LocalityError) -> String {
    match error {
        LocalityError::Validation(issues) => issues
            .into_iter()
            .map(|issue| issue.message)
            .collect::<Vec<_>>()
            .join("; "),
        other => other.to_string(),
    }
}

fn connection_access_token(
    credentials: &dyn CredentialStore,
    connection: &ConnectionRecord,
) -> Result<String, ConnectorResolveError> {
    let secret = credentials
        .get(&connection.secret_ref)
        .map_err(|error| credential_error(connection, error))?;
    let mut stored = serde_json::from_str::<StoredGmailCredential>(&secret)
        .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?;
    if stored.expires_soon(timestamp_secs()) {
        let refreshed = refresh_oauth_credential(connection, &stored)?;
        stored = stored
            .refreshed(refreshed, timestamp_secs())
            .map_err(|error| gmail_refresh_scope_error(connection, error))?;
        let secret = serde_json::to_string(&stored).map_err(|error| {
            ConnectorResolveError::CredentialStoreUnavailable(error.to_string())
        })?;
        credentials
            .put(&connection.secret_ref, &secret)
            .map_err(|error| credential_error(connection, error))?;
    }
    Ok(stored.access_token)
}

fn refresh_oauth_credential(
    connection: &ConnectionRecord,
    stored: &StoredGmailCredential,
) -> Result<locality_connector::oauth_broker::OAuthBrokerToken, ConnectorResolveError> {
    let Some(refresh_token_handle) = stored.refresh_token_handle.clone() else {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: None,
            suggested_command: GMAIL_CONNECT_COMMAND.to_string(),
        });
    };
    let Some(broker_url) = stored.oauth_broker_url.clone() else {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: None,
            suggested_command: GMAIL_CONNECT_COMMAND.to_string(),
        });
    };

    HttpGmailOAuthBrokerClient::new(broker_url.clone())
        .refresh_token(&OAuthBrokerRefresh {
            connector: GMAIL_CONNECTOR_ID.to_string(),
            refresh_token_handle: Some(refresh_token_handle),
        })
        .map_err(|error| gmail_refresh_error(connection, &broker_url, error))
}

fn gmail_refresh_scope_error(
    connection: &ConnectionRecord,
    error: GmailOAuthScopeError,
) -> ConnectorResolveError {
    ConnectorResolveError::AuthRequired {
        connection_id: connection.connection_id.0.clone(),
        message: Some(format!(
            "Gmail credential for connection `{}` could not be refreshed through OAuth broker: {error}",
            connection.connection_id.0
        )),
        suggested_command: GMAIL_CONNECT_COMMAND.to_string(),
    }
}

fn gmail_refresh_error(
    connection: &ConnectionRecord,
    broker_url: &str,
    error: LocalityError,
) -> ConnectorResolveError {
    let hint = if is_loopback_broker_url(broker_url) {
        "reconnect with the default hosted broker or keep the local broker running"
    } else {
        "reconnect to issue a fresh Gmail refresh handle"
    };
    ConnectorResolveError::AuthRequired {
        connection_id: connection.connection_id.0.clone(),
        message: Some(format!(
            "Gmail credential for connection `{}` could not be refreshed through OAuth broker at `{broker_url}`: {error}; {hint}",
            connection.connection_id.0
        )),
        suggested_command: GMAIL_CONNECT_COMMAND.to_string(),
    }
}

fn is_loopback_broker_url(url: &str) -> bool {
    let Some(authority) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    else {
        return false;
    };
    let host_port = authority
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(authority)
        .to_ascii_lowercase();
    let host = if host_port.starts_with('[') {
        host_port
            .split(']')
            .next()
            .map(|value| format!("{value}]"))
            .unwrap_or(host_port)
    } else {
        host_port
            .split(':')
            .next()
            .unwrap_or(host_port.as_str())
            .to_string()
    };
    matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]")
}

fn active_gmail_connections<S>(store: &S) -> Result<Vec<ConnectionRecord>, ConnectorResolveError>
where
    S: ConnectionRepository + ?Sized,
{
    let connections = store
        .list_connections()
        .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?;
    Ok(connections
        .into_iter()
        .filter(|connection| {
            connection.connector == GMAIL_CONNECTOR_ID
                && connection.status == "active"
                && connection.auth_kind == "oauth"
        })
        .collect())
}

fn validate_connection_profile<S>(
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
            suggested_command: GMAIL_CONNECT_COMMAND.to_string(),
        })?;
    if profile.status != "active"
        || profile.connector != connection.connector
        || profile.auth_kind != connection.auth_kind
    {
        return Err(ConnectorResolveError::AuthProfileUnavailable {
            profile_id: profile.profile_id.0,
            suggested_command: GMAIL_CONNECT_COMMAND.to_string(),
        });
    }
    Ok(())
}

fn credential_error(
    connection: &ConnectionRecord,
    error: CredentialError,
) -> ConnectorResolveError {
    match error {
        CredentialError::NotFound(_) => ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: None,
            suggested_command: GMAIL_CONNECT_COMMAND.to_string(),
        },
        CredentialError::Unavailable(message) | CredentialError::Io(message) => {
            ConnectorResolveError::CredentialStoreUnavailable(message)
        }
    }
}

fn timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

impl SourcePushValidator for GmailConnector {
    fn validate_changed_frontmatter(
        &self,
        context: SourceValidationContext<'_>,
    ) -> LocalityResult<ValidationReport> {
        validate_gmail_changed_frontmatter(context)
    }

    fn validate_create_frontmatter(
        &self,
        context: SourceValidationContext<'_>,
    ) -> LocalityResult<ValidationReport> {
        validate_gmail_create_frontmatter(context)
    }
}

impl SourceAdapter for GmailConnector {}

pub(crate) fn validate_gmail_changed_frontmatter(
    context: SourceValidationContext<'_>,
) -> LocalityResult<ValidationReport> {
    let mut report = ValidationReport::clean();
    if gmail_mailbox_from_path(context.relative_path)
        .is_some_and(|mailbox| matches!(mailbox, "inbox" | "sent"))
    {
        report.push(ValidationIssue::new(
            "gmail_read_only_mailbox",
            context.relative_path,
            Some(1),
            "Gmail inbox and sent items are read-only",
            Some("create a new Markdown file directly under draft/ to send mail".to_string()),
        ));
    }
    Ok(report)
}

pub(crate) fn validate_gmail_create_frontmatter(
    context: SourceValidationContext<'_>,
) -> LocalityResult<ValidationReport> {
    let mut report = ValidationReport::clean();

    if !is_direct_draft_child(context.relative_path) {
        report.push(ValidationIssue::new(
            "gmail_create_outside_draft",
            context.relative_path,
            Some(1),
            "Gmail creates are only supported directly inside draft/",
            Some("move the new email Markdown file directly under draft/".to_string()),
        ));
    }

    let has_subject = frontmatter_string(&context.parsed.frontmatter.properties, "subject")
        .as_deref()
        .is_some_and(|subject| !subject.trim().is_empty())
        || context
            .parsed
            .frontmatter
            .title
            .as_deref()
            .is_some_and(|title| !title.trim().is_empty());
    if !has_subject {
        report.push(ValidationIssue::new(
            "gmail_draft_missing_subject",
            context.relative_path,
            Some(1),
            "Gmail draft requires `subject` or `title` frontmatter",
            Some("add `subject: \"Subject text\"` or a non-empty `title`".to_string()),
        ));
    }

    if frontmatter_string_list(&context.parsed.frontmatter.properties, "to").is_empty() {
        report.push(ValidationIssue::new(
            "gmail_draft_missing_to",
            context.relative_path,
            Some(1),
            "Gmail draft requires `to` frontmatter",
            Some("add `to: [\"name@example.com\"]` to the frontmatter".to_string()),
        ));
    }

    if context
        .parsed
        .frontmatter
        .properties
        .contains_key("attachment")
        || context
            .parsed
            .frontmatter
            .properties
            .contains_key("attachments")
    {
        report.push(ValidationIssue::new(
            "gmail_attachments_unsupported",
            context.relative_path,
            Some(1),
            "Gmail draft sends do not support attachments",
            Some("remove `attachment` or `attachments` frontmatter".to_string()),
        ));
    }

    Ok(report)
}

fn frontmatter_string_list(
    properties: &locality_core::canonical::FrontmatterProperties,
    key: &str,
) -> Vec<String> {
    properties
        .get(key)
        .map(property_value_from_frontmatter)
        .map(|value| match value {
            PropertyValue::String(value) => vec![value],
            PropertyValue::List(values) => values,
            _ => Vec::new(),
        })
        .unwrap_or_default()
        .into_iter()
        .filter(|value| !value.trim().is_empty())
        .collect()
}

fn frontmatter_string(
    properties: &locality_core::canonical::FrontmatterProperties,
    key: &str,
) -> Option<String> {
    properties
        .get(key)
        .map(property_value_from_frontmatter)
        .and_then(|value| match value {
            PropertyValue::String(value) => Some(value),
            _ => None,
        })
}

fn gmail_mailbox_from_path(path: &Path) -> Option<&str> {
    path.components()
        .next()
        .and_then(|component| match component {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
}

fn is_direct_draft_child(path: &Path) -> bool {
    let mut components = path.components();
    matches!(
        components.next(),
        Some(Component::Normal(component)) if component == "draft"
    ) && matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
}

impl HydrationSource for GmailConnector {
    fn fetch_render(&self, request: &HydrationRequest) -> LocalityResult<HydratedEntity> {
        let native = self.fetch(FetchRequest {
            remote_id: request.remote_id.clone(),
        })?;
        if native.kind == "gmail_thread" {
            let bundle = serde_json::from_slice::<GmailThreadNativeBundle>(&native.raw).map_err(
                |error| LocalityError::Io(format!("gmail thread native decode failed: {error}")),
            )?;
            let rendered = render_gmail_thread(&bundle)?;
            let assets = gmail_attachment_assets(self.api(), &rendered.attachment_specs)?;
            return Ok(HydratedEntity {
                document: rendered.document,
                shadow: rendered.shadow,
                remote_edited_at: Some(thread_remote_version(&bundle.thread)),
                assets,
            });
        }

        let bundle = serde_json::from_slice::<GmailNativeBundle>(&native.raw)
            .map_err(|error| LocalityError::Io(format!("gmail native decode failed: {error}")))?;
        let rendered = render_gmail_message(&bundle)?;
        let assets = gmail_attachment_assets(self.api(), &rendered.attachment_specs)?;
        Ok(HydratedEntity {
            document: rendered.document,
            shadow: rendered.shadow,
            remote_edited_at: Some(remote_version(&bundle.message)),
            assets,
        })
    }

    fn fetch_database_schema_yaml(
        &self,
        _database_id: &RemoteId,
    ) -> LocalityResult<Option<String>> {
        Ok(None)
    }
}

fn gmail_attachment_assets(
    api: &dyn GmailApi,
    attachment_specs: &[GmailAttachmentSpec],
) -> LocalityResult<Vec<HydratedAsset>> {
    let mut assets = Vec::new();
    for spec in attachment_specs {
        let body = api.get_attachment(&spec.message_id, &spec.attachment_id)?;
        assets.push(HydratedAsset {
            path: spec.local_path.clone(),
            bytes: decode_attachment_body(&body)?,
            media: None,
        });
    }
    Ok(assets)
}

impl crate::reconcile::ScheduledPullSource for GmailConnector {
    fn enumerate_mount(&self, mount: &MountConfig) -> LocalityResult<Vec<TreeEntry>> {
        self.enumerate(EnumerateRequest {
            mount_id: mount.mount_id.clone(),
            cursor: None,
        })
    }

    fn database_schema_yaml(
        &self,
        _mount: &MountConfig,
        _remote_id: &RemoteId,
    ) -> LocalityResult<Option<String>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use locality_core::hydration::{HydrationReason, HydrationRequest};
    use locality_core::model::{HydrationState, MountId, RemoteId};
    use locality_gmail::attachments::attachment_local_path;
    use locality_gmail::client::GmailApi;
    use locality_gmail::dto::{
        GmailDraft, GmailDraftCreateRequest, GmailDraftSendRequest, GmailMessage, GmailMessageList,
        GmailMessagePartBody, GmailThread, GmailThreadList,
    };

    use super::*;
    use crate::hydration::HydrationSource;

    #[test]
    fn gmail_hydration_downloads_message_attachments_as_assets() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let request = HydrationRequest::new(
            MountId::new("gmail-main"),
            RemoteId::new("msg-attach"),
            "inbox/msg-attach.md",
            HydrationState::Hydrated,
            HydrationReason::ExplicitPull,
        );

        let hydrated = connector.fetch_render(&request).expect("hydrate");
        let expected_path = attachment_local_path("msg-attach", "attach-1", "Invoice.pdf");

        assert_eq!(hydrated.assets.len(), 1);
        assert_eq!(hydrated.assets[0].path, expected_path);
        assert_eq!(hydrated.assets[0].bytes, b"attachment bytes");
        assert_eq!(hydrated.assets[0].media, None);
        let calls = api.calls.lock().expect("calls");
        assert_eq!(
            calls.attachments,
            vec![("msg-attach".to_string(), "attach-1".to_string())]
        );
    }

    #[test]
    fn gmail_hydration_propagates_attachment_body_decode_errors() {
        let api = Arc::new(FakeGmailApi::with_attachment_data(None));
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api);
        let request = HydrationRequest::new(
            MountId::new("gmail-main"),
            RemoteId::new("msg-attach"),
            "inbox/msg-attach.md",
            HydrationState::Hydrated,
            HydrationReason::ExplicitPull,
        );

        let error = connector
            .fetch_render(&request)
            .expect_err("missing attachment body data");

        assert!(
            matches!(
                error,
                LocalityError::Io(ref message)
                    if message.contains("gmail attachment response did not include body data")
            ),
            "{error}"
        );
    }

    #[test]
    fn gmail_hydration_downloads_thread_attachments_as_assets() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let request = HydrationRequest::new(
            MountId::new("gmail-main"),
            RemoteId::new("gmail-thread:inbox:thread-attach"),
            "inbox/thread-attach/page.md",
            HydrationState::Hydrated,
            HydrationReason::ExplicitPull,
        );

        let hydrated = connector.fetch_render(&request).expect("hydrate thread");
        let expected_path = attachment_local_path("msg-attach", "attach-1", "Invoice.pdf");

        assert_eq!(hydrated.assets.len(), 1);
        assert_eq!(hydrated.assets[0].path, expected_path);
        assert_eq!(hydrated.assets[0].bytes, b"attachment bytes");
        assert_eq!(
            hydrated.remote_edited_at,
            Some(locality_gmail::render::thread_remote_version(
                &thread_fixture("thread-attach")
            ))
        );
        assert!(
            hydrated
                .document
                .frontmatter
                .contains("thread_id: \"thread-attach\"")
        );
        let calls = api.calls.lock().expect("calls");
        assert_eq!(
            calls.attachments,
            vec![("msg-attach".to_string(), "attach-1".to_string())]
        );
    }

    #[derive(Debug)]
    struct FakeGmailApi {
        calls: Mutex<FakeCalls>,
        attachment_data: Mutex<Option<String>>,
    }

    impl FakeGmailApi {
        fn with_attachment_data(attachment_data: Option<String>) -> Self {
            Self {
                calls: Mutex::new(FakeCalls::default()),
                attachment_data: Mutex::new(attachment_data),
            }
        }
    }

    impl Default for FakeGmailApi {
        fn default() -> Self {
            Self::with_attachment_data(Some(URL_SAFE_NO_PAD.encode(b"attachment bytes")))
        }
    }

    #[derive(Default, Debug)]
    struct FakeCalls {
        attachments: Vec<(String, String)>,
    }

    impl GmailApi for FakeGmailApi {
        fn list_messages(
            &self,
            _label_id: &str,
            _max_results: u32,
            _page_token: Option<&str>,
            _query: Option<&str>,
        ) -> LocalityResult<GmailMessageList> {
            Ok(GmailMessageList::default())
        }

        fn list_threads(
            &self,
            _label_id: &str,
            _max_results: u32,
            _page_token: Option<&str>,
            _query: Option<&str>,
        ) -> LocalityResult<GmailThreadList> {
            Ok(GmailThreadList::default())
        }

        fn get_message_metadata(&self, message_id: &str) -> LocalityResult<GmailMessage> {
            Ok(message_fixture(message_id))
        }

        fn get_message_full(&self, message_id: &str) -> LocalityResult<GmailMessage> {
            Ok(message_fixture(message_id))
        }

        fn get_thread_metadata(&self, _thread_id: &str) -> LocalityResult<GmailThread> {
            Ok(thread_fixture(_thread_id))
        }

        fn get_thread_full(&self, _thread_id: &str) -> LocalityResult<GmailThread> {
            Ok(thread_fixture(_thread_id))
        }

        fn get_attachment(
            &self,
            message_id: &str,
            attachment_id: &str,
        ) -> LocalityResult<GmailMessagePartBody> {
            self.calls
                .lock()
                .expect("calls")
                .attachments
                .push((message_id.to_string(), attachment_id.to_string()));
            let data = self
                .attachment_data
                .lock()
                .expect("attachment data")
                .clone();
            Ok(GmailMessagePartBody {
                attachment_id: Some(attachment_id.to_string()),
                size: Some(16),
                data,
            })
        }

        fn create_draft(&self, _request: GmailDraftCreateRequest) -> LocalityResult<GmailDraft> {
            panic!("not used")
        }

        fn send_draft(&self, _request: GmailDraftSendRequest) -> LocalityResult<GmailMessage> {
            panic!("not used")
        }
    }

    fn message_fixture(id: &str) -> GmailMessage {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "threadId": "thread-attach",
            "labelIds": ["INBOX"],
            "internalDate": "1720900000000",
            "payload": {
                "mimeType": "multipart/mixed",
                "headers": [
                    { "name": "Subject", "value": "Attachments" }
                ],
                "parts": [
                    {
                        "mimeType": "text/plain",
                        "body": { "data": "Qm9keQo" }
                    },
                    {
                        "filename": "Invoice.pdf",
                        "mimeType": "application/pdf",
                        "body": { "attachmentId": "attach-1", "size": 16 }
                    }
                ]
            }
        }))
        .expect("message")
    }

    fn thread_fixture(thread_id: &str) -> GmailThread {
        GmailThread {
            id: thread_id.to_string(),
            history_id: Some("h1".to_string()),
            messages: vec![message_fixture("msg-attach")],
        }
    }
}
