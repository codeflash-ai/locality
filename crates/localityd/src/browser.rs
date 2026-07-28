use locality_browser::{
    BROWSER_CONNECTOR_ID, BrowserConfig, BrowserConnector, BrowserMountSettings,
    BrowserNativeBundle, remote_version_for_session, remote_version_for_tab, render_browser_entity,
};
use locality_connector::{Connector, FetchRequest};
use locality_core::hydration::HydrationRequest;
use locality_core::model::RemoteId;
use locality_core::shadow::{ShadowDocument, segment_markdown_body};
use locality_core::validation::ValidationReport;
use locality_core::{LocalityError, LocalityResult};
use locality_store::{CredentialStore, MountConfig};

use crate::hydration::{HydratedEntity, HydrationSource};
use crate::notion::ConnectorResolveError;
use crate::source::{SourceAdapter, SourcePushValidator, SourceValidationContext};

pub fn resolve_browser_connector_for_mount(
    _credentials: &dyn CredentialStore,
    mount: &MountConfig,
) -> Result<BrowserConnector, ConnectorResolveError> {
    if mount.connector != BROWSER_CONNECTOR_ID {
        return Err(ConnectorResolveError::UnsupportedConnector(
            mount.connector.clone(),
        ));
    }
    let settings = BrowserMountSettings::from_json(&mount.settings_json).map_err(|error| {
        ConnectorResolveError::CredentialStoreUnavailable(format!(
            "Browser mount settings are invalid: {error}"
        ))
    })?;
    let capture_root = settings.capture_root().map_err(|error| {
        ConnectorResolveError::CredentialStoreUnavailable(format!(
            "Browser capture root is invalid: {error}"
        ))
    })?;
    Ok(BrowserConnector::new(BrowserConfig::new(capture_root)))
}

impl SourcePushValidator for BrowserConnector {}
impl SourceAdapter for BrowserConnector {}

impl HydrationSource for BrowserConnector {
    fn fetch_render(&self, request: &HydrationRequest) -> LocalityResult<HydratedEntity> {
        let native = self.fetch(FetchRequest {
            remote_id: request.remote_id.clone(),
        })?;
        let bundle = serde_json::from_slice::<BrowserNativeBundle>(&native.raw)
            .map_err(|error| LocalityError::Io(format!("Browser native decode failed: {error}")))?;
        let document = render_browser_entity(&bundle)?;
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
        let remote_edited_at = match &bundle {
            BrowserNativeBundle::Session { session, .. } => {
                Some(remote_version_for_session(session))
            }
            BrowserNativeBundle::Tab { session, tab, .. } => {
                Some(remote_version_for_tab(session, tab))
            }
        };
        Ok(HydratedEntity {
            document,
            shadow,
            remote_edited_at,
            assets: Vec::new(),
        })
    }

    fn fetch_database_schema_yaml(
        &self,
        _database_id: &RemoteId,
    ) -> LocalityResult<Option<String>> {
        Ok(None)
    }
}

pub fn validate_browser_frontmatter(
    _context: SourceValidationContext<'_>,
) -> LocalityResult<ValidationReport> {
    Ok(ValidationReport::clean())
}
