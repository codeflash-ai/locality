//! Notion connector.
//!
//! The connector keeps Notion API transport, DTOs, and block rendering separate
//! from the connector-neutral sync contracts in `locality-core`.

pub mod apply;
pub mod client;
pub mod database;
pub mod database_create;
pub mod dto;
pub mod fetch;
pub mod hydration;
mod initial_hydration_session;
pub mod mapping;
pub mod markdown_table;
pub mod media;
pub mod oauth;
mod portable;
pub mod projection;
pub mod render;
pub mod root_setup;
pub mod schema;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use locality_connector::hydration_budget::{InitialHydrationBudget, InitialHydrationResult};
use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, Connector,
    ConnectorCapabilities, ConnectorExecutionPolicy, ConnectorKind, EnumerateRequest, FetchRequest,
    ListChildrenRequest, ListChildrenResult, NativeEntity, ObserveRequest, ParsedEntity,
    PortableBootstrapRequest, PortableChangeBatch, PortableChangeBatchV2, PortableFetchRequest,
    PortableFetchResult, PortableRenderRequest, PortableRenderResult, PortableSyncHint,
    PortableSyncMode, PortableSyncRequest, PortableSyncRequestV2,
};
use locality_core::freshness::RemoteObservation;
use locality_core::model::{CanonicalDocument, RemoteId, TreeEntry};
use locality_core::planner::PushOperationKind;
use locality_core::{LocalityError, LocalityResult};

use crate::apply::{apply_plan, apply_undo, check_concurrency};
use crate::client::{DEFAULT_NOTION_TOKEN_ENV, HttpNotionApi, NotionApi};
use crate::fetch::fetch_page_bundle;
use crate::media::{
    MediaDownloadReport, MediaFetchReport, PortableMediaCaptureFetcher, PortableMediaCapturePolicy,
    default_portable_media_fetcher, download_media_assets, fetch_media_asset_report_with_fetcher,
};
use crate::oauth::NOTION_CONNECTOR_ID;
use crate::projection::{
    enumerate_explicit_root_trees, enumerate_shared_pages, list_container_children, observe_entity,
    resolve_notion_object_path_entries, resolve_page_path_entries,
};
use crate::render::{
    NotionRenderedEntity, RenderOptions, render_native_entity, render_native_entity_with_options,
};
use crate::root_setup::NotionRootSetup;

pub use crate::initial_hydration_session::NotionInitialHydrationSession;

#[derive(Clone, PartialEq, Eq)]
pub struct NotionConfig {
    pub workspace_id: Option<String>,
    pub root_page_id: Option<locality_core::model::RemoteId>,
    /// Resolved bearer token from a provider connection. Never log this field.
    pub token: Option<String>,
    /// Environment variable or future keychain key used to find the bearer token.
    pub token_key: String,
    pub execution_policy: ConnectorExecutionPolicy,
}

impl std::fmt::Debug for NotionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotionConfig")
            .field("workspace_id", &self.workspace_id)
            .field("root_page_id", &self.root_page_id)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("token_key", &self.token_key)
            .field("execution_policy", &self.execution_policy)
            .finish()
    }
}

impl Default for NotionConfig {
    fn default() -> Self {
        Self {
            workspace_id: None,
            root_page_id: None,
            token: None,
            token_key: DEFAULT_NOTION_TOKEN_ENV.to_string(),
            execution_policy: ConnectorExecutionPolicy::Inline,
        }
    }
}

impl NotionConfig {
    pub fn with_root_page_id(mut self, root_page_id: locality_core::model::RemoteId) -> Self {
        self.root_page_id = Some(root_page_id);
        self
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    pub fn with_execution_policy(mut self, execution_policy: ConnectorExecutionPolicy) -> Self {
        self.execution_policy = execution_policy;
        self
    }
}

#[derive(Clone)]
pub struct NotionConnector {
    config: NotionConfig,
    api: Arc<dyn NotionApi>,
    explicit_root_page_ids: Vec<RemoteId>,
    explicit_root_set: bool,
    portable_media_capture_policy: PortableMediaCapturePolicy,
    portable_media_fetcher: Option<Arc<dyn PortableMediaCaptureFetcher>>,
}

impl std::fmt::Debug for NotionConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotionConnector")
            .field("config", &self.config)
            .field(
                "portable_media_capture_policy",
                &self.portable_media_capture_policy,
            )
            .finish_non_exhaustive()
    }
}

impl NotionConnector {
    pub fn new(config: NotionConfig) -> Self {
        Self::with_api(config.clone(), Arc::new(HttpNotionApi::new(config)))
    }

    pub fn with_api(config: NotionConfig, api: Arc<dyn NotionApi>) -> Self {
        let explicit_root_page_ids = config.root_page_id.iter().cloned().collect();
        Self {
            config,
            api,
            explicit_root_page_ids,
            explicit_root_set: false,
            portable_media_capture_policy: PortableMediaCapturePolicy::Disabled,
            portable_media_fetcher: None,
        }
    }

    pub fn config(&self) -> &NotionConfig {
        &self.config
    }

    /// Return the provider-specific, metadata-only facade used to choose and
    /// revalidate explicit Notion roots during source setup.
    pub fn root_setup(&self) -> NotionRootSetup {
        NotionRootSetup::with_api(Arc::clone(&self.api))
    }

    pub fn with_root_page_id(&self, root_page_id: locality_core::model::RemoteId) -> Self {
        let mut config = self.config.clone();
        config.root_page_id = Some(root_page_id.clone());
        Self {
            config,
            api: Arc::clone(&self.api),
            explicit_root_page_ids: vec![root_page_id],
            explicit_root_set: false,
            portable_media_capture_policy: self.portable_media_capture_policy,
            portable_media_fetcher: self.portable_media_fetcher.clone(),
        }
    }

    /// Select up to 16 explicit page or full-page database roots without using
    /// provider search. Validation is deferred to enumeration/bootstrap so
    /// malformed scopes fail through the normal connector result channel.
    pub fn with_root_ids(&self, root_ids: impl IntoIterator<Item = RemoteId>) -> Self {
        let explicit_root_page_ids = root_ids.into_iter().collect::<Vec<_>>();
        let mut config = self.config.clone();
        config.root_page_id =
            (explicit_root_page_ids.len() == 1).then(|| explicit_root_page_ids[0].clone());
        Self {
            config,
            api: Arc::clone(&self.api),
            explicit_root_page_ids,
            explicit_root_set: true,
            portable_media_capture_policy: self.portable_media_capture_policy,
            portable_media_fetcher: self.portable_media_fetcher.clone(),
        }
    }

    /// Compatibility alias for [`Self::with_root_ids`].
    pub fn with_root_page_ids(&self, root_page_ids: impl IntoIterator<Item = RemoteId>) -> Self {
        self.with_root_ids(root_page_ids)
    }

    pub fn explicit_root_page_ids(&self) -> &[RemoteId] {
        &self.explicit_root_page_ids
    }

    /// Enable or disable the portable hosted-media capture policy.
    ///
    /// This does not change direct fetch/render or desktop media materialization.
    pub fn with_portable_media_capture(&self, policy: PortableMediaCapturePolicy) -> Self {
        let mut connector = self.clone();
        connector.portable_media_capture_policy = policy;
        connector
    }

    /// Inject a deterministic portable media fetcher while retaining the same
    /// connector-side URL validation and pilot byte limits.
    pub fn with_portable_media_capture_fetcher(
        &self,
        policy: PortableMediaCapturePolicy,
        fetcher: Arc<dyn PortableMediaCaptureFetcher>,
    ) -> Self {
        let mut connector = self.with_portable_media_capture(policy);
        connector.portable_media_fetcher = Some(fetcher);
        connector
    }

    pub fn portable_media_capture_policy(&self) -> PortableMediaCapturePolicy {
        self.portable_media_capture_policy
    }

    /// Start one fail-closed initial-hydration job over this connector's
    /// configured explicit roots.
    ///
    /// `source_connection_identity_sha256` is produced by the trusted caller
    /// and is the only connection identity serialized into ephemeral progress
    /// checkpoints. The returned wrapper privately owns the shared budget used
    /// by enumeration, native fetch, hosted media, render, and projection.
    pub fn initial_hydration_session(
        &self,
        source_connection_identity_sha256: impl Into<String>,
        page_size: u32,
        limits: locality_connector::hydration_budget::InitialHydrationLimits,
    ) -> InitialHydrationResult<NotionInitialHydrationSession> {
        NotionInitialHydrationSession::new(
            self.clone(),
            source_connection_identity_sha256.into(),
            page_size,
            limits,
        )
    }

    /// Opt-in initial-hydration page fetch. The caller owns the job-scoped
    /// budget and must reuse it for every later media/render/projection stage.
    pub fn fetch_page_native_bounded(
        &self,
        page_id: &RemoteId,
        budget: &InitialHydrationBudget,
    ) -> InitialHydrationResult<NativeEntity> {
        hydration::fetch_page_native_bounded(self.api.as_ref(), page_id.as_str(), budget)
    }

    /// Opt-in initial-hydration database/schema fetch.
    pub fn fetch_database_native_bounded(
        &self,
        database_id: &RemoteId,
        budget: &InitialHydrationBudget,
    ) -> InitialHydrationResult<NativeEntity> {
        hydration::fetch_database_native_bounded(self.api.as_ref(), database_id.as_str(), budget)
    }

    /// Bounded row inventory only; the caller must not infer deletion from
    /// omissions or assign snapshot authority to this result.
    pub fn query_data_source_rows_bounded(
        &self,
        data_source_id: &RemoteId,
        budget: &InitialHydrationBudget,
    ) -> InitialHydrationResult<Vec<dto::PageDto>> {
        hydration::query_data_source_rows_bounded(
            self.api.as_ref(),
            data_source_id.as_str(),
            budget,
        )
    }

    /// Fetch one hosted asset under the connector's configured transport and
    /// the caller's shared initial-hydration budget.
    pub fn fetch_portable_media_bounded(
        &self,
        hosted_url: &str,
        per_asset_max_bytes: usize,
        budget: &InitialHydrationBudget,
    ) -> InitialHydrationResult<media::PortableMediaCapture> {
        let default_fetcher = default_portable_media_fetcher();
        let fetcher = self
            .portable_media_fetcher
            .as_deref()
            .unwrap_or(default_fetcher.as_ref());
        hydration::fetch_media_bounded(fetcher, hosted_url, per_asset_max_bytes, budget)
    }

    pub fn render_native_entity_bounded(
        &self,
        entity: &NativeEntity,
        budget: &InitialHydrationBudget,
    ) -> InitialHydrationResult<NotionRenderedEntity> {
        hydration::render_native_entity_bounded(entity, budget)
    }

    pub fn render_portable_bounded(
        &self,
        request: &PortableRenderRequest,
        budget: &InitialHydrationBudget,
    ) -> InitialHydrationResult<PortableRenderResult> {
        hydration::render_portable_bounded(request, budget)
    }

    pub fn render_native_entity(
        &self,
        entity: &NativeEntity,
    ) -> LocalityResult<NotionRenderedEntity> {
        render_native_entity(entity)
    }

    pub fn render_native_entity_for_path(
        &self,
        entity: &NativeEntity,
        page_path: impl AsRef<Path>,
    ) -> LocalityResult<NotionRenderedEntity> {
        render_native_entity_with_options(
            entity,
            &RenderOptions::with_page_path(page_path.as_ref()),
        )
    }

    pub fn render_native_entity_for_path_with_local_media_blocks(
        &self,
        entity: &NativeEntity,
        page_path: impl AsRef<Path>,
        block_ids: impl IntoIterator<Item = String>,
    ) -> LocalityResult<NotionRenderedEntity> {
        render_native_entity_with_options(
            entity,
            &RenderOptions::with_page_path(page_path.as_ref())
                .with_local_media_block_ids(block_ids),
        )
    }

    pub fn download_rendered_media(
        &self,
        rendered: &NotionRenderedEntity,
        mount_root: impl AsRef<Path>,
    ) -> LocalityResult<MediaDownloadReport> {
        download_media_assets(mount_root.as_ref(), &rendered.media_assets)
    }

    /// Fetch the hosted assets selected by the shared renderer.
    ///
    /// The optional injected fetcher is also used by daemon hydration tests;
    /// production connectors use the hardened default transport.
    pub fn fetch_rendered_media(&self, rendered: &NotionRenderedEntity) -> MediaFetchReport {
        let default_fetcher = default_portable_media_fetcher();
        let fetcher = self
            .portable_media_fetcher
            .as_deref()
            .unwrap_or(default_fetcher.as_ref());
        fetch_media_asset_report_with_fetcher(&rendered.media_assets, fetcher)
    }

    pub fn database_schema_yaml(&self, database_id: &RemoteId) -> LocalityResult<String> {
        database::database_schema_yaml(self.api.as_ref(), database_id.as_str())
    }

    pub fn resolve_page_path_entries(
        &self,
        mount_id: locality_core::model::MountId,
        page_id: &RemoteId,
    ) -> LocalityResult<Vec<TreeEntry>> {
        resolve_page_path_entries(
            self.api.as_ref(),
            mount_id,
            self.config.root_page_id.as_ref(),
            page_id,
        )
    }

    pub fn resolve_object_path_entries(
        &self,
        mount_id: locality_core::model::MountId,
        object_id: &RemoteId,
    ) -> LocalityResult<Vec<TreeEntry>> {
        resolve_notion_object_path_entries(
            self.api.as_ref(),
            mount_id,
            self.config.root_page_id.as_ref(),
            object_id,
        )
    }
}

impl Connector for NotionConnector {
    fn with_execution_policy(&self, policy: ConnectorExecutionPolicy) -> Self {
        let connector = Self::new(self.config.clone().with_execution_policy(policy));
        let mut connector = if self.explicit_root_set {
            connector.with_root_ids(self.explicit_root_page_ids.clone())
        } else {
            connector
        };
        connector.portable_media_capture_policy = self.portable_media_capture_policy;
        connector.portable_media_fetcher = self.portable_media_fetcher.clone();
        connector
    }

    fn kind(&self) -> ConnectorKind {
        ConnectorKind(NOTION_CONNECTOR_ID)
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_block_updates: true,
            supports_entity_body_updates: false,
            supports_databases: true,
            supports_oauth: true,
            supports_remote_observation: true,
            supports_lazy_child_enumeration: true,
            supports_media_download: true,
            supports_undo: true,
            supports_batch_observation: false,
        }
    }

    fn supported_push_operations(&self) -> BTreeSet<PushOperationKind> {
        [
            PushOperationKind::UpdateBlock,
            PushOperationKind::ReplaceBlock,
            PushOperationKind::AppendBlock,
            PushOperationKind::MoveBlock,
            PushOperationKind::UpdateMedia,
            PushOperationKind::ArchiveBlock,
            PushOperationKind::ArchiveEntity,
            PushOperationKind::UpdateProperties,
            PushOperationKind::MoveEntity,
            PushOperationKind::CreateEntity,
            PushOperationKind::CreateDatabase,
        ]
        .into_iter()
        .collect()
    }

    fn enumerate(&self, request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        if self.explicit_root_set || !self.explicit_root_page_ids.is_empty() {
            portable::validate_configured_roots(&self.explicit_root_page_ids)?;
            Ok(enumerate_explicit_root_trees(
                self.api.as_ref(),
                request.mount_id,
                &self.explicit_root_page_ids,
            )?
            .into_iter()
            .map(|projected| projected.entry)
            .collect())
        } else {
            enumerate_shared_pages(self.api.as_ref(), request.mount_id)
        }
    }

    fn bootstrap_portable(
        &self,
        request: PortableBootstrapRequest,
    ) -> LocalityResult<PortableChangeBatch> {
        portable::bootstrap(
            self.api.as_ref(),
            &self.explicit_root_page_ids,
            self.explicit_root_set,
            request,
        )
    }

    fn sync_portable(&self, request: PortableSyncRequest) -> LocalityResult<PortableChangeBatch> {
        portable::synchronize(
            self.api.as_ref(),
            &self.explicit_root_page_ids,
            self.explicit_root_set,
            request,
        )
    }

    fn sync_portable_v2_impl(
        &self,
        request: PortableSyncRequestV2,
    ) -> LocalityResult<PortableChangeBatchV2> {
        if request.mode == PortableSyncMode::HintsOnly {
            return portable::synchronize_v2_hints(
                self.api.as_ref(),
                &self.explicit_root_page_ids,
                request,
            );
        }

        // ReconcileScope intentionally retains the compatibility adapter for
        // now: legacy incremental behavior, with no omission authority or root
        // coverage claim. A future exhaustive implementation can replace this
        // branch without weakening HintsOnly's bounded metadata path.
        portable::synchronize(
            self.api.as_ref(),
            &self.explicit_root_page_ids,
            self.explicit_root_set,
            PortableSyncRequest {
                source_connection_id: request.source_connection_id,
                scope: request.scope,
                checkpoint: request.checkpoint,
                hints: request
                    .hints
                    .into_iter()
                    .map(|hint| PortableSyncHint {
                        remote_id: hint.remote_id,
                    })
                    .collect(),
                max_changes: request.max_changes,
            },
        )
        .map(Into::into)
    }

    fn fetch_portable(&self, request: PortableFetchRequest) -> LocalityResult<PortableFetchResult> {
        portable::fetch(
            self.api.as_ref(),
            self.portable_media_capture_policy,
            self.portable_media_fetcher.as_deref(),
            request,
        )
    }

    fn render_portable(
        &self,
        request: &PortableRenderRequest,
    ) -> LocalityResult<PortableRenderResult> {
        portable::render(request)
    }

    fn list_children(&self, request: ListChildrenRequest) -> LocalityResult<ListChildrenResult> {
        if self.explicit_root_set || !self.explicit_root_page_ids.is_empty() {
            portable::validate_configured_roots(&self.explicit_root_page_ids)?;
        }
        let entries = list_container_children(
            self.api.as_ref(),
            request.mount_id,
            &self.explicit_root_page_ids,
            request.container,
            &request.parent_path,
        )?;

        Ok(ListChildrenResult::complete(entries))
    }

    fn observe(&self, request: ObserveRequest) -> LocalityResult<RemoteObservation> {
        observe_entity(self.api.as_ref(), request.mount_id, &request.remote_id)
    }

    fn fetch(&self, request: FetchRequest) -> LocalityResult<NativeEntity> {
        let bundle = fetch_page_bundle(self.api.as_ref(), request.remote_id.as_str())?;
        let remote_id = locality_core::model::RemoteId::new(bundle.page.id.clone());
        let raw = serde_json::to_vec(&bundle)
            .map_err(|error| LocalityError::Io(format!("notion native encode failed: {error}")))?;

        Ok(NativeEntity {
            remote_id,
            kind: "notion_page".to_string(),
            raw,
        })
    }

    fn render(&self, entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        self.render_native_entity(entity)
            .map(|rendered| rendered.document)
    }

    fn parse(&self, _document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        Err(LocalityError::NotImplemented("Notion parse"))
    }

    fn check_concurrency(&self, request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        check_concurrency(self.api.as_ref(), request)
    }

    fn apply(&self, request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        apply_plan(self.api.as_ref(), request)
    }

    fn apply_undo(&self, request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        apply_undo(self.api.as_ref(), request)
    }
}

#[cfg(test)]
mod tests {
    use locality_connector::Connector;

    use super::{NotionConfig, NotionConnector};
    use crate::oauth::NOTION_CONNECTOR_ID;

    #[test]
    fn notion_connector_kind_matches_oauth_connector_id() {
        let connector = NotionConnector::new(NotionConfig::default());

        assert_eq!(connector.kind().0, NOTION_CONNECTOR_ID);
    }
}
