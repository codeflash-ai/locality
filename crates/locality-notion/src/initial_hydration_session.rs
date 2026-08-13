//! One fail-closed, job-owned initial-hydration attempt.
//!
//! The wrapper implements [`Connector`] so the ordinary engine projection
//! pipeline can use it, while inventory pagination and every fetch/render call
//! share one private [`InitialHydrationBudget`]. Its continuation checkpoints
//! are deliberately meaningful only to this live value.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard};

use locality_connector::hydration_budget::{
    HydrationResource, InitialHydrationBudget, InitialHydrationError, InitialHydrationLimits,
    InitialHydrationResult,
};
use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, Connector,
    ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest, NativeEntity,
    ParsedEntity, PortableBootstrapRequest, PortableChangeBatch, PortableCheckpoint,
    PortableCompleteness, PortableFetchReason, PortableFetchRequest, PortableFetchResult,
    PortableIncompleteReason, PortableRenderRequest, PortableRenderResult,
};
use locality_core::model::{CanonicalDocument, EntityKind, RemoteId, TreeEntry};
use locality_core::planner::PushOperationKind;
use locality_core::portable::SourceConnectionId;
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

use crate::NotionConnector;
use crate::portable::{self, CanonicalRootSet};

const SESSION_CHECKPOINT_FORMAT_VERSION: u16 = 4;
const SESSION_CHECKPOINT_COMPONENT_VERSION: u16 = 1;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EphemeralCheckpoint {
    component_version: u16,
    session_nonce: String,
    source_connection_identity_sha256: String,
    canonical_root_set_sha256: String,
    inventory_sha256: String,
    next_index: u64,
}

#[derive(Debug, Default)]
struct SessionState {
    failed: bool,
    complete: bool,
    source_connection_id: Option<SourceConnectionId>,
    inventory: Vec<locality_connector::PortableSourceChange>,
    inventory_sha256: Option<String>,
    inventory_retained_bytes: usize,
    next_index: usize,
    expected_checkpoint: Option<PortableCheckpoint>,
    emitted_fetchable: BTreeMap<RemoteId, ExpectedFetch>,
    fetched: BTreeSet<RemoteId>,
    rendered: BTreeSet<RemoteId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct ExpectedFetch {
    kind: EntityKind,
    provider_version: Option<String>,
}

#[derive(Serialize)]
struct ExpectedFetchRef<'a> {
    kind: &'a EntityKind,
    provider_version: Option<&'a str>,
}

/// Connector-compatible facade for exactly one initial-hydration job.
///
/// Do not persist this value or its nonterminal checkpoints. Dropping it loses
/// the random nonce and therefore invalidates every outstanding continuation.
pub struct NotionInitialHydrationSession {
    connector: NotionConnector,
    budget: InitialHydrationBudget,
    source_connection_identity_sha256: String,
    roots: CanonicalRootSet,
    page_size: u32,
    session_nonce: String,
    state: Mutex<SessionState>,
}

impl std::fmt::Debug for NotionInitialHydrationSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NotionInitialHydrationSession")
            .field("page_size", &self.page_size)
            .field("configured_root_count", &self.roots.roots.len())
            .finish_non_exhaustive()
    }
}

impl NotionInitialHydrationSession {
    pub(crate) fn new(
        connector: NotionConnector,
        source_connection_identity_sha256: String,
        page_size: u32,
        limits: InitialHydrationLimits,
    ) -> InitialHydrationResult<Self> {
        if page_size == 0 || !valid_sha256_identity(&source_connection_identity_sha256) {
            return Err(InitialHydrationError::ProviderResponseInvalid);
        }
        let roots = portable::canonical_root_set(&connector.explicit_root_page_ids)
            .map_err(InitialHydrationError::from_connector_error)?;
        let budget = InitialHydrationBudget::new(limits)?;
        let mut nonce = [0_u8; 32];
        getrandom::fill(&mut nonce).map_err(|_| InitialHydrationError::ProviderUnavailable)?;
        Ok(Self {
            connector,
            budget,
            source_connection_identity_sha256,
            roots,
            page_size,
            session_nonce: hex_lower(&nonce),
            state: Mutex::new(SessionState::default()),
        })
    }

    fn bootstrap_page(
        &self,
        request: PortableBootstrapRequest,
    ) -> LocalityResult<PortableChangeBatch> {
        let mut state = self.lock_state()?;
        let result = self.bootstrap_page_locked(&mut state, request);
        if result.is_err() {
            state.failed = true;
            state.expected_checkpoint = None;
        }
        result
    }

    fn bootstrap_page_locked(
        &self,
        state: &mut SessionState,
        request: PortableBootstrapRequest,
    ) -> LocalityResult<PortableChangeBatch> {
        if state.failed || state.complete {
            return Err(session_error(
                "initial hydration session is no longer active",
            ));
        }
        portable::validate_explicit_roots(
            &self.connector.explicit_root_page_ids,
            &request.scope.root_remote_ids,
        )?;
        if request.max_changes == 0 {
            return Err(session_error("initial hydration page size must be nonzero"));
        }

        if state.source_connection_id.is_none() {
            if request.checkpoint.is_some() {
                return Err(session_error(
                    "fresh initial hydration session accepts no checkpoint",
                ));
            }
            self.preflight_change_page(request.max_changes)?;
            state.source_connection_id = Some(request.source_connection_id.clone());
            let inventory = portable::inventory_bounded(
                self.connector.api.as_ref(),
                &request.source_connection_id,
                &self.roots.roots,
                self.connector.explicit_root_set,
                &self.budget,
            )
            .map_err(hydration_error)?;
            let inventory_retained_bytes =
                crate::hydration::encoded_len(&inventory).map_err(hydration_error)?;
            state.inventory_sha256 = Some(portable::inventory_sha256(
                &inventory,
                self.connector.explicit_root_set,
            ));
            state.inventory_retained_bytes = inventory_retained_bytes;
            state.inventory = inventory;
        } else {
            if state.source_connection_id.as_ref() != Some(&request.source_connection_id) {
                return Err(session_error("initial hydration source connection changed"));
            }
            self.validate_continuation(state, request.checkpoint.as_ref())?;
        }

        let remaining_changes = self
            .budget
            .remaining(HydrationResource::Changes)
            .map_err(hydration_error)?;
        let requested_page_size = u64::from(request.max_changes)
            .min(u64::from(self.page_size))
            .min(remaining_changes);
        if requested_page_size == 0 {
            return Err(hydration_error(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::Changes,
            }));
        }
        let page_size = usize::try_from(requested_page_size)
            .map_err(|_| session_error("initial hydration page size is invalid"))?;
        let end = state
            .next_index
            .saturating_add(page_size)
            .min(state.inventory.len());
        let page_changes = &state.inventory[state.next_index..end];
        let retained_page_bytes =
            crate::hydration::encoded_len(&page_changes).map_err(hydration_error)?;
        self.budget
            .preflight_changes(page_changes.len(), retained_page_bytes)
            .map_err(hydration_error)?;

        let inventory_sha256 = state
            .inventory_sha256
            .as_ref()
            .ok_or_else(|| session_error("initial hydration inventory is unavailable"))?;
        let terminal = end == state.inventory.len();
        let next_checkpoint = if terminal {
            portable::terminal_bootstrap_checkpoint(
                &self.roots,
                inventory_sha256.clone(),
                state.inventory.len(),
                self.connector.explicit_root_set,
            )?
        } else {
            encode_ephemeral_checkpoint(&EphemeralCheckpoint {
                component_version: SESSION_CHECKPOINT_COMPONENT_VERSION,
                session_nonce: self.session_nonce.clone(),
                source_connection_identity_sha256: self.source_connection_identity_sha256.clone(),
                canonical_root_set_sha256: self.roots.identity.clone(),
                inventory_sha256: inventory_sha256.clone(),
                next_index: u64::try_from(end)
                    .map_err(|_| session_error("initial hydration index is too large"))?,
            })?
        };
        let mut completeness = inventory_completeness(&state.inventory);
        if !terminal {
            completeness.merge(PortableCompleteness::incomplete(
                PortableIncompleteReason::CheckpointContinuation,
            ));
        }
        let changes = page_changes.to_vec();
        self.budget
            .account_changes(changes.len(), retained_page_bytes)
            .map_err(hydration_error)?;
        for change in &changes {
            if change.requires_fetch {
                insert_expected_fetch(
                    &mut state.emitted_fetchable,
                    &change.source_object.remote_id,
                    &change.source_object.kind,
                    change.source_object.opaque_version.as_deref(),
                    &self.budget,
                )
                .map_err(hydration_error)?;
            }
        }
        state.next_index = end;
        state.expected_checkpoint = (!terminal).then(|| next_checkpoint.clone());
        if terminal {
            state.complete = true;
            self.budget
                .release_retained_bytes(state.inventory_retained_bytes)
                .map_err(hydration_error)?;
            state.inventory_retained_bytes = 0;
            state.inventory.clear();
        }
        Ok(PortableChangeBatch {
            changes,
            next_checkpoint,
            completeness,
        })
    }

    fn preflight_change_page(&self, request_max_changes: u32) -> LocalityResult<()> {
        self.budget.check_deadline().map_err(hydration_error)?;
        let remaining = self
            .budget
            .remaining(HydrationResource::Changes)
            .map_err(hydration_error)?;
        if u64::from(request_max_changes)
            .min(u64::from(self.page_size))
            .min(remaining)
            == 0
        {
            return Err(hydration_error(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::Changes,
            }));
        }
        Ok(())
    }

    fn validate_continuation(
        &self,
        state: &SessionState,
        supplied: Option<&PortableCheckpoint>,
    ) -> LocalityResult<()> {
        let supplied = supplied.ok_or_else(|| {
            session_error("active initial hydration session requires its next checkpoint")
        })?;
        if state.expected_checkpoint.as_ref() != Some(supplied)
            || supplied.format_version != SESSION_CHECKPOINT_FORMAT_VERSION
        {
            return Err(session_error(
                "initial hydration checkpoint is replayed, skipped, or out of order",
            ));
        }
        let decoded: EphemeralCheckpoint = serde_json::from_str(&supplied.opaque)
            .map_err(|_| session_error("initial hydration checkpoint is invalid"))?;
        let expected_index = u64::try_from(state.next_index)
            .map_err(|_| session_error("initial hydration index is too large"))?;
        if decoded.component_version != SESSION_CHECKPOINT_COMPONENT_VERSION
            || decoded.session_nonce != self.session_nonce
            || decoded.source_connection_identity_sha256 != self.source_connection_identity_sha256
            || decoded.canonical_root_set_sha256 != self.roots.identity
            || decoded.inventory_sha256 != state.inventory_sha256.as_deref().unwrap_or_default()
            || decoded.next_index != expected_index
        {
            return Err(session_error(
                "initial hydration checkpoint does not belong to this session and scope",
            ));
        }
        Ok(())
    }

    fn fetch_once(&self, request: PortableFetchRequest) -> LocalityResult<PortableFetchResult> {
        let mut state = self.lock_state()?;
        if state.failed
            || state.source_connection_id.as_ref() != Some(&request.source_connection_id)
            || request.reason != PortableFetchReason::Bootstrap
            || !state.emitted_fetchable.contains_key(&request.remote_id)
            || state.fetched.contains(&request.remote_id)
        {
            state.failed = true;
            state.expected_checkpoint = None;
            return Err(session_error(
                "initial hydration fetch is outside the active emitted inventory",
            ));
        }
        if let Err(error) =
            insert_remote_id_index(&mut state.fetched, &request.remote_id, &self.budget)
        {
            state.failed = true;
            state.expected_checkpoint = None;
            return Err(hydration_error(error));
        }
        let result = portable::fetch_bounded(
            self.connector.api.as_ref(),
            self.connector.portable_media_capture_policy,
            self.connector.portable_media_fetcher.as_deref(),
            request,
            &self.budget,
        )
        .map_err(hydration_error);
        let result = result.and_then(|result| {
            let expected = state
                .emitted_fetchable
                .get(&result.native.remote_id)
                .ok_or_else(|| session_error("initial hydration fetch changed source identity"))?;
            let kind_matches = match &expected.kind {
                EntityKind::Page => matches!(
                    result.native.kind.as_str(),
                    "notion_page" | "notion_page_portable_media_v1"
                ),
                EntityKind::Database => result.native.kind == "notion_database",
                _ => false,
            };
            if !kind_matches
                || result.provider_version.as_deref() != expected.provider_version.as_deref()
            {
                return Err(session_error(
                    "initial hydration source changed after inventory",
                ));
            }
            Ok(result)
        });
        if result.is_err() {
            state.failed = true;
            state.expected_checkpoint = None;
        }
        result
    }

    fn render_once(&self, request: &PortableRenderRequest) -> LocalityResult<PortableRenderResult> {
        let mut state = self.lock_state()?;
        if state.failed
            || state.source_connection_id.as_ref() != Some(&request.source_connection_id)
            || !state.fetched.contains(&request.native.remote_id)
            || state.rendered.contains(&request.native.remote_id)
        {
            state.failed = true;
            state.expected_checkpoint = None;
            return Err(session_error(
                "initial hydration render is outside the active fetched inventory",
            ));
        }
        if let Err(error) =
            insert_remote_id_index(&mut state.rendered, &request.native.remote_id, &self.budget)
        {
            state.failed = true;
            state.expected_checkpoint = None;
            return Err(hydration_error(error));
        }
        let result = crate::hydration::render_portable_bounded(request, &self.budget)
            .map_err(hydration_error);
        if result.is_err() {
            state.failed = true;
            state.expected_checkpoint = None;
        }
        result
    }

    fn lock_state(&self) -> LocalityResult<MutexGuard<'_, SessionState>> {
        self.state
            .lock()
            .map_err(|_| session_error("initial hydration session state is unavailable"))
    }
}

impl Connector for NotionInitialHydrationSession {
    fn kind(&self) -> ConnectorKind {
        self.connector.kind()
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_databases: true,
            ..ConnectorCapabilities::default()
        }
    }

    fn supported_push_operations(&self) -> BTreeSet<PushOperationKind> {
        BTreeSet::new()
    }

    fn enumerate(&self, _request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        Err(session_error(
            "initial hydration session supports only portable bootstrap",
        ))
    }

    fn bootstrap_portable(
        &self,
        request: PortableBootstrapRequest,
    ) -> LocalityResult<PortableChangeBatch> {
        self.bootstrap_page(request)
    }

    fn fetch_portable(&self, request: PortableFetchRequest) -> LocalityResult<PortableFetchResult> {
        self.fetch_once(request)
    }

    fn render_portable(
        &self,
        request: &PortableRenderRequest,
    ) -> LocalityResult<PortableRenderResult> {
        self.render_once(request)
    }

    fn fetch(&self, _request: FetchRequest) -> LocalityResult<NativeEntity> {
        Err(session_error(
            "initial hydration session rejects the unbounded fetch API",
        ))
    }

    fn render(&self, _entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        Err(session_error(
            "initial hydration session rejects the unbounded render API",
        ))
    }

    fn parse(&self, _document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        Err(session_error("initial hydration session does not parse"))
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        Err(session_error("initial hydration session is read-only"))
    }

    fn apply(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        Err(session_error("initial hydration session is read-only"))
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        Err(session_error("initial hydration session is read-only"))
    }
}

fn insert_expected_fetch(
    index: &mut BTreeMap<RemoteId, ExpectedFetch>,
    remote_id: &RemoteId,
    kind: &EntityKind,
    provider_version: Option<&str>,
    budget: &InitialHydrationBudget,
) -> InitialHydrationResult<()> {
    if index.contains_key(remote_id) {
        return Err(InitialHydrationError::ProviderResponseInvalid);
    }
    let retained_bytes = crate::hydration::encoded_len(&(
        remote_id,
        ExpectedFetchRef {
            kind,
            provider_version,
        },
    ))?;
    budget.account_retained_bytes(retained_bytes)?;
    index.insert(
        remote_id.clone(),
        ExpectedFetch {
            kind: kind.clone(),
            provider_version: provider_version.map(str::to_string),
        },
    );
    Ok(())
}

fn insert_remote_id_index(
    index: &mut BTreeSet<RemoteId>,
    remote_id: &RemoteId,
    budget: &InitialHydrationBudget,
) -> InitialHydrationResult<()> {
    if index.contains(remote_id) {
        return Err(InitialHydrationError::ProviderResponseInvalid);
    }
    budget.account_retained_bytes(crate::hydration::encoded_len(remote_id)?)?;
    index.insert(remote_id.clone());
    Ok(())
}

fn inventory_completeness(
    inventory: &[locality_connector::PortableSourceChange],
) -> PortableCompleteness {
    let mut completeness = PortableCompleteness::complete();
    for change in inventory {
        if !change.requires_fetch {
            completeness.merge(PortableCompleteness::incomplete(
                PortableIncompleteReason::UnsupportedSourceKind {
                    remote_id: change.source_object.remote_id.clone(),
                    source_kind: match &change.source_object.kind {
                        EntityKind::Page => "page".to_string(),
                        EntityKind::Database => "database".to_string(),
                        EntityKind::Directory => "directory".to_string(),
                        EntityKind::Asset => "asset".to_string(),
                        EntityKind::Unknown(kind) => format!("unknown:{kind}"),
                    },
                },
            ));
        }
    }
    completeness
}

fn encode_ephemeral_checkpoint(
    checkpoint: &EphemeralCheckpoint,
) -> LocalityResult<PortableCheckpoint> {
    let opaque = serde_json::to_string(checkpoint)
        .map_err(|_| session_error("initial hydration checkpoint encode failed"))?;
    Ok(PortableCheckpoint {
        format_version: SESSION_CHECKPOINT_FORMAT_VERSION,
        opaque,
    })
}

fn valid_sha256_identity(value: &str) -> bool {
    value.len() == "sha256:".len() + 64
        && value.starts_with("sha256:")
        && value["sha256:".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn hydration_error(error: InitialHydrationError) -> LocalityError {
    LocalityError::InvalidState(error.to_string())
}

fn session_error(message: &'static str) -> LocalityError {
    LocalityError::InvalidState(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::{ExpectedFetch, ExpectedFetchRef, insert_expected_fetch, insert_remote_id_index};
    use locality_connector::hydration_budget::{
        HydrationResource, InitialHydrationBudget, InitialHydrationError, InitialHydrationLimits,
    };
    use locality_core::model::{EntityKind, RemoteId};
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn lifecycle_indexes_have_exact_caps_and_large_cumulative_accounting() {
        fn expected(index: usize) -> (RemoteId, ExpectedFetch) {
            (
                RemoteId::new(format!("{index:032x}")),
                ExpectedFetch {
                    kind: EntityKind::Page,
                    provider_version: Some(format!("version-{index:04}")),
                },
            )
        }

        let entries = (0..256).map(expected).collect::<Vec<_>>();
        let expected_bytes = entries
            .iter()
            .try_fold(0_u64, |total, (remote_id, expected)| {
                let bytes = u64::try_from(
                    crate::hydration::encoded_len(&(
                        remote_id,
                        ExpectedFetchRef {
                            kind: &expected.kind,
                            provider_version: expected.provider_version.as_deref(),
                        },
                    ))
                    .expect("encoded entry"),
                )
                .expect("entry bytes");
                total.checked_add(bytes)
            })
            .expect("expected bytes");
        let remote_id_bytes =
            u64::try_from(crate::hydration::encoded_len(&entries[0].0).expect("encoded remote ID"))
                .expect("remote ID bytes");
        let exact = expected_bytes + remote_id_bytes.saturating_mul(2);

        let budget = InitialHydrationBudget::new(test_limits(exact)).expect("budget");
        let mut emitted = BTreeMap::new();
        for (remote_id, expected) in &entries {
            insert_expected_fetch(
                &mut emitted,
                remote_id,
                &expected.kind,
                expected.provider_version.as_deref(),
                &budget,
            )
            .expect("large emitted inventory");
        }
        let mut fetched = BTreeSet::new();
        let mut rendered = BTreeSet::new();
        insert_remote_id_index(&mut fetched, &entries[0].0, &budget).expect("fetched index");
        insert_remote_id_index(&mut rendered, &entries[0].0, &budget).expect("rendered index");

        let short = InitialHydrationBudget::new(test_limits(exact - 1)).expect("budget");
        let mut emitted = BTreeMap::new();
        for (remote_id, expected) in &entries {
            insert_expected_fetch(
                &mut emitted,
                remote_id,
                &expected.kind,
                expected.provider_version.as_deref(),
                &short,
            )
            .expect("emitted inventory fits before lifecycle tail");
        }
        let mut fetched = BTreeSet::new();
        let mut rendered = BTreeSet::new();
        insert_remote_id_index(&mut fetched, &entries[0].0, &short).expect("fetched index");
        let error = insert_remote_id_index(&mut rendered, &entries[0].0, &short)
            .expect_err("cap minus one");
        assert_eq!(
            error,
            InitialHydrationError::LimitExceeded {
                resource: HydrationResource::RetainedBytes
            }
        );
    }

    fn test_limits(max_retained_bytes: u64) -> InitialHydrationLimits {
        InitialHydrationLimits {
            max_response_body_bytes: 1_000_000,
            max_provider_calls: 1_000_000,
            provider_deadline_ms: 60_000,
            max_inventory_items: 1_000_000,
            max_inventory_encoded_bytes: 1_000_000,
            max_traversal_nodes: 1_000_000,
            max_traversal_depth: 1_000_000,
            max_native_bytes: 1_000_000,
            max_media_assets: 1_000_000,
            max_media_decoded_bytes: 1_000_000,
            max_rendered_content_bytes: 1_000_000,
            max_projections: 1_000_000,
            max_changes: 1_000_000,
            max_retained_bytes,
        }
    }
}
