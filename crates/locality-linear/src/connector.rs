use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, BatchObservationChange,
    BatchObserveRequest, BatchObserveResult, ChildContainer, Connector, ConnectorCapabilities,
    ConnectorCheckpoint, ConnectorKind, EnumerateRequest, FetchRequest, ListChildrenRequest,
    ListChildrenResult, NativeEntity, ObserveRequest, ParsedEntity,
};
use locality_core::freshness::{RemoteObservation, RemoteVersion};
use locality_core::journal::JournalApplyEffect;
use locality_core::model::{
    CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry,
};
use locality_core::planner::{PropertyValue, PushOperation, PushOperationKind};
use locality_core::{LocalityError, LocalityResult};

use crate::client::{HttpLinearApiClient, LinearApi};
use crate::dto::{LinearIssue, LinearIssueState, LinearIssueUpdateInput, LinearTeam};
use crate::render::{LinearNativeBundle, remote_version, render_linear_issue};

pub const LINEAR_CONNECTOR_ID: &str = "linear";
const TEAMS_DIRECTORY_NAME: &str = "Teams";
const ISSUES_DIRECTORY_NAME: &str = "Issues";
const TEAMS_ROOT_REMOTE_ID: &str = "linear:teams";
const TEAM_REMOTE_ID_PREFIX: &str = "team:";
const TEAM_ISSUES_REMOTE_ID_PREFIX: &str = "team-issues:";
const TEAM_STATE_REMOTE_ID_PREFIX: &str = "team-state:";

#[derive(Clone, PartialEq, Eq)]
pub struct LinearConfig {
    pub token: String,
}

impl LinearConfig {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
        }
    }
}

impl fmt::Debug for LinearConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinearConfig")
            .field("token", &"<redacted>")
            .finish()
    }
}

#[derive(Clone)]
pub struct LinearConnector {
    config: LinearConfig,
    api: Arc<dyn LinearApi>,
}

impl fmt::Debug for LinearConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinearConnector")
            .field("token", &"<redacted>")
            .finish()
    }
}

impl LinearConnector {
    pub fn new(config: LinearConfig) -> Self {
        let api = Arc::new(HttpLinearApiClient::new(config.token.clone()));
        Self::with_api(config, api)
    }

    pub fn with_api(config: LinearConfig, api: Arc<dyn LinearApi>) -> Self {
        Self { config, api }
    }

    pub fn config(&self) -> &LinearConfig {
        &self.config
    }

    fn all_issues(
        &self,
        updated_after: Option<&str>,
        team_id: Option<&str>,
    ) -> LocalityResult<Vec<LinearIssue>> {
        let mut issues = Vec::new();
        let mut cursor = None;
        loop {
            let page = self
                .api
                .list_issues(cursor.as_deref(), updated_after, team_id)?;
            issues.extend(page.issues);
            if !page.has_next_page {
                break;
            }
            cursor = page.end_cursor.filter(|value| !value.is_empty());
            if cursor.is_none() {
                return Err(LocalityError::InvalidState(
                    "Linear API reported another issue page without a cursor".to_string(),
                ));
            }
        }
        dedupe_sort_issues(issues)
    }
}

impl Connector for LinearConnector {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind(LINEAR_CONNECTOR_ID)
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_entity_body_updates: true,
            supports_oauth: false,
            supports_remote_observation: true,
            supports_lazy_child_enumeration: true,
            supports_batch_observation: true,
            ..ConnectorCapabilities::default()
        }
    }

    fn supported_push_operations(&self) -> BTreeSet<PushOperationKind> {
        [
            PushOperationKind::UpdateEntityBody,
            PushOperationKind::UpdateProperties,
            PushOperationKind::MoveEntity,
        ]
        .into_iter()
        .collect()
    }

    fn enumerate(&self, request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        Ok(entries_for_issues(
            &request.mount_id,
            Path::new(""),
            self.all_issues(None, None)?,
        ))
    }

    fn observe(&self, request: ObserveRequest) -> LocalityResult<RemoteObservation> {
        let issue = self.api.get_issue(request.remote_id.as_str())?;
        Ok(observation_from_issue(&request.mount_id, issue))
    }

    fn observe_batch(&self, request: BatchObserveRequest) -> LocalityResult<BatchObserveResult> {
        let updated_after = request
            .checkpoint
            .as_ref()
            .and_then(|checkpoint| checkpoint_updated_after(checkpoint));
        let issues = self.all_issues(updated_after.as_deref(), None)?;
        let max_updated_at = issues
            .iter()
            .map(|issue| issue.updated_at.as_str())
            .max()
            .map(ToOwned::to_owned)
            .or(updated_after)
            .unwrap_or_default();
        let changes = issues
            .into_iter()
            .map(|issue| {
                BatchObservationChange::Upsert(issue_entry(
                    &request.mount_id,
                    Path::new(""),
                    &issue,
                ))
            })
            .collect();
        Ok(BatchObserveResult::incremental(
            changes,
            ConnectorCheckpoint {
                state_version: 1,
                min_reader_version: 1,
                state_json: serde_json::json!({ "updated_after": max_updated_at }).to_string(),
            },
        ))
    }

    fn list_children(&self, request: ListChildrenRequest) -> LocalityResult<ListChildrenResult> {
        let entries = match request.container {
            ChildContainer::Root => vec![teams_root_entry(&request.mount_id, &request.parent_path)],
            ChildContainer::DirectoryChildren(remote_id)
                if remote_id.as_str() == TEAMS_ROOT_REMOTE_ID =>
            {
                team_entries(
                    &request.mount_id,
                    &request.parent_path,
                    &self.all_issues(None, None)?,
                )
            }
            ChildContainer::DirectoryChildren(remote_id) => list_linear_directory_children(
                self,
                &request.mount_id,
                &request.parent_path,
                &remote_id,
            )?,
            _ => Vec::new(),
        };
        Ok(ListChildrenResult::complete(entries))
    }

    fn fetch(&self, request: FetchRequest) -> LocalityResult<NativeEntity> {
        let issue = self.api.get_issue(request.remote_id.as_str())?;
        let raw = serde_json::to_vec(&LinearNativeBundle { issue })
            .map_err(|error| LocalityError::Io(format!("Linear native encode failed: {error}")))?;
        Ok(NativeEntity {
            remote_id: request.remote_id,
            kind: "linear_issue".to_string(),
            raw,
        })
    }

    fn render(&self, entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        let bundle = serde_json::from_slice::<LinearNativeBundle>(&entity.raw)
            .map_err(|error| LocalityError::Io(format!("Linear native decode failed: {error}")))?;
        render_linear_issue(&bundle.issue)
    }

    fn parse(&self, _document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        Err(LocalityError::Unsupported("Linear parse"))
    }

    fn check_concurrency(&self, request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        for precondition in request.remote_preconditions {
            let Some(expected) = &precondition.remote_edited_at else {
                continue;
            };
            let current = self.api.get_issue(precondition.remote_id.as_str())?;
            let current_version = remote_version(&current);
            if &current_version != expected {
                return Err(LocalityError::Guardrail(format!(
                    "Linear issue `{}` changed remotely before apply (expected `{expected}`, found `{current_version}`)",
                    precondition.remote_id.0
                )));
            }
        }
        Ok(())
    }

    fn apply(&self, request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        self.check_concurrency(ApplyPlanRequest { ..request })?;
        let mut updates = BTreeMap::<RemoteId, LinearIssueUpdateInput>::new();
        let mut effects = Vec::new();
        for (index, operation) in request.plan.operations.iter().enumerate() {
            let operation_id =
                request.operation_ids.get(index).cloned().ok_or_else(|| {
                    LocalityError::InvalidState("missing operation id".to_string())
                })?;
            match operation {
                PushOperation::UpdateEntityBody { entity_id, body } => {
                    update_for(&mut updates, entity_id).description = Some(body.clone());
                    effects.push(JournalApplyEffect::UpdatedEntityBody {
                        operation_id,
                        operation_index: index,
                        entity_id: entity_id.clone(),
                    });
                }
                PushOperation::UpdateProperties {
                    entity_id,
                    properties,
                } => {
                    let update = update_for(&mut updates, entity_id);
                    apply_property_updates(update, properties)?;
                    effects.push(JournalApplyEffect::UpdatedProperties {
                        operation_id,
                        operation_index: index,
                        entity_id: entity_id.clone(),
                        keys: properties.keys().cloned().collect(),
                    });
                }
                PushOperation::MoveEntity {
                    entity_id,
                    new_parent_id,
                    ..
                } => {
                    let (team_id, state_id) =
                        team_state_from_remote_id(new_parent_id).ok_or_else(|| {
                            validation_error(
                                "Linear issues can only be moved into a Linear status folder"
                                    .to_string(),
                            )
                        })?;
                    let update = update_for(&mut updates, entity_id);
                    update.team_id = Some(team_id.to_string());
                    update.state_id = Some(state_id.to_string());
                    effects.push(JournalApplyEffect::MovedEntity {
                        operation_id,
                        operation_index: index,
                        entity_id: entity_id.clone(),
                        parent_id: new_parent_id.clone(),
                    });
                }
                _ => {
                    return Err(LocalityError::Unsupported(
                        "Linear connector cannot apply this operation",
                    ));
                }
            }
        }

        let mut changed_remote_ids = Vec::new();
        for (_, update) in updates {
            let issue = self.api.update_issue(update)?;
            changed_remote_ids.push(RemoteId::new(issue.id));
        }
        changed_remote_ids.sort();
        changed_remote_ids.dedup();
        Ok(ApplyPlanResult {
            changed_remote_ids,
            effects,
        })
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        Err(LocalityError::Unsupported("Linear undo"))
    }
}

fn update_for<'a>(
    updates: &'a mut BTreeMap<RemoteId, LinearIssueUpdateInput>,
    entity_id: &RemoteId,
) -> &'a mut LinearIssueUpdateInput {
    updates
        .entry(entity_id.clone())
        .or_insert_with(|| LinearIssueUpdateInput {
            issue_id: entity_id.0.clone(),
            ..LinearIssueUpdateInput::default()
        })
}

fn apply_property_updates(
    update: &mut LinearIssueUpdateInput,
    properties: &BTreeMap<String, PropertyValue>,
) -> LocalityResult<()> {
    for (key, value) in properties {
        match key.as_str() {
            "title" => update.title = Some(required_string(value, key)?),
            "Status" | "status" => update.state_id = Some(required_reference_id(value, key)?),
            "Project" | "project" => update.project_id = Some(optional_reference_id(value, key)?),
            "Assignee" | "assignee" => {
                update.assignee_id = Some(optional_reference_id(value, key)?)
            }
            _ => {
                return Err(LocalityError::Unsupported(
                    "Linear property is read-only or unsupported",
                ));
            }
        }
    }
    Ok(())
}

fn required_string(value: &PropertyValue, key: &str) -> LocalityResult<String> {
    match value {
        PropertyValue::String(value) => Ok(value.clone()),
        _ => Err(validation_error(format!(
            "Linear property `{key}` must be a string"
        ))),
    }
}

fn required_reference_id(value: &PropertyValue, key: &str) -> LocalityResult<String> {
    optional_reference_id(value, key)?
        .ok_or_else(|| validation_error(format!("Linear property `{key}` requires a reference id")))
}

fn optional_reference_id(value: &PropertyValue, key: &str) -> LocalityResult<Option<String>> {
    match value {
        PropertyValue::Null => Ok(None),
        PropertyValue::String(value) => extract_reference_id(value).map(Some).ok_or_else(|| {
            validation_error(format!(
                "Linear property `{key}` must be `Label <id>` or a raw id"
            ))
        }),
        _ => Err(validation_error(format!(
            "Linear property `{key}` must be a string reference or null"
        ))),
    }
}

fn validation_error(message: String) -> LocalityError {
    LocalityError::Validation(vec![locality_core::validation::ValidationIssue::new(
        "linear_property_shape",
        "",
        None,
        message,
        None,
    )])
}

fn extract_reference_id(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(inner) = value
        .strip_suffix('>')
        .and_then(|value| value.rsplit_once('<').map(|(_, id)| id.trim()))
    {
        return (!inner.is_empty()).then(|| inner.to_string());
    }
    Some(value.to_string())
}

fn entries_for_issues(
    mount_id: &MountId,
    parent: &Path,
    issues: Vec<LinearIssue>,
) -> Vec<TreeEntry> {
    let mut entries = vec![teams_root_entry(mount_id, parent)];
    let teams_parent = parent.join(TEAMS_DIRECTORY_NAME);
    for team in unique_teams(&issues) {
        let team_path = teams_parent.join(team_directory_name(&team));
        entries.push(team_entry(mount_id, &teams_parent, &team));
        entries.push(team_issues_entry(mount_id, &team_path, &team.id));
    }
    for (team_id, state) in unique_states(&issues) {
        let Some(team) = issues.iter().find(|issue| issue.team.id == team_id) else {
            continue;
        };
        let issues_path = teams_parent
            .join(team_directory_name(&team.team))
            .join(ISSUES_DIRECTORY_NAME);
        entries.push(status_entry(mount_id, &issues_path, &team_id, &state));
    }
    entries.extend(
        issues
            .iter()
            .map(|issue| issue_entry(mount_id, parent, issue)),
    );
    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.remote_id.cmp(&right.remote_id))
    });
    entries
}

fn team_entries(mount_id: &MountId, parent: &Path, issues: &[LinearIssue]) -> Vec<TreeEntry> {
    unique_teams(issues)
        .into_iter()
        .map(|team| team_entry(mount_id, parent, &team))
        .collect()
}

fn teams_root_entry(mount_id: &MountId, parent: &Path) -> TreeEntry {
    directory_entry(
        mount_id,
        RemoteId::new(TEAMS_ROOT_REMOTE_ID),
        TEAMS_DIRECTORY_NAME,
        parent.join(TEAMS_DIRECTORY_NAME),
    )
}

fn team_entry(mount_id: &MountId, parent: &Path, team: &LinearTeam) -> TreeEntry {
    directory_entry(
        mount_id,
        RemoteId::new(team_remote_id(&team.id)),
        team.name.clone(),
        parent.join(team_directory_name(team)),
    )
}

fn team_issues_entry(mount_id: &MountId, parent: &Path, team_id: &str) -> TreeEntry {
    directory_entry(
        mount_id,
        RemoteId::new(team_issues_remote_id(team_id)),
        ISSUES_DIRECTORY_NAME,
        parent.join(ISSUES_DIRECTORY_NAME),
    )
}

fn status_entries(
    mount_id: &MountId,
    parent: &Path,
    team_id: &str,
    issues: &[LinearIssue],
) -> Vec<TreeEntry> {
    unique_states(issues)
        .into_iter()
        .filter(|(state_team_id, _)| state_team_id == team_id)
        .map(|(_, state)| status_entry(mount_id, parent, team_id, &state))
        .collect()
}

fn status_entry(
    mount_id: &MountId,
    parent: &Path,
    team_id: &str,
    state: &LinearIssueState,
) -> TreeEntry {
    directory_entry(
        mount_id,
        RemoteId::new(team_state_remote_id(team_id, &state.id)),
        state.name.clone(),
        parent.join(status_directory_name(state)),
    )
}

fn directory_entry(
    mount_id: &MountId,
    remote_id: RemoteId,
    title: impl Into<String>,
    path: impl Into<std::path::PathBuf>,
) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id,
        kind: EntityKind::Directory,
        title: title.into(),
        path: path.into(),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: None,
        stub_frontmatter: None,
    }
}

fn issue_entry(mount_id: &MountId, parent: &Path, issue: &LinearIssue) -> TreeEntry {
    let frontmatter = render_linear_issue(issue)
        .ok()
        .map(|document| document.frontmatter);
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(issue.id.clone()),
        kind: EntityKind::Page,
        title: issue.title.clone(),
        path: parent
            .join(TEAMS_DIRECTORY_NAME)
            .join(team_directory_name(&issue.team))
            .join(ISSUES_DIRECTORY_NAME)
            .join(status_directory_name(&issue.state))
            .join(issue_directory_name(issue))
            .join("page.md"),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some(remote_version(issue)),
        stub_frontmatter: frontmatter,
    }
}

fn status_child_issue_entry(mount_id: &MountId, parent: &Path, issue: &LinearIssue) -> TreeEntry {
    let mut entry = issue_entry(mount_id, Path::new(""), issue);
    entry.path = parent.join(issue_directory_name(issue)).join("page.md");
    entry
}

fn observation_from_issue(mount_id: &MountId, issue: LinearIssue) -> RemoteObservation {
    let entry = issue_entry(mount_id, Path::new(""), &issue);
    RemoteObservation::new(
        entry.mount_id,
        entry.remote_id,
        entry.kind,
        entry.title,
        entry.path,
    )
    .with_parent(RemoteId::new(team_state_remote_id(
        &issue.team.id,
        &issue.state.id,
    )))
    .with_remote_version(RemoteVersion::new(remote_version(&issue)))
    .deleted(issue.archived_at.is_some())
    .with_raw_metadata_json(serde_json::to_string(&issue).unwrap_or_else(|_| "{}".to_string()))
}

fn list_linear_directory_children(
    connector: &LinearConnector,
    mount_id: &MountId,
    parent_path: &Path,
    remote_id: &RemoteId,
) -> LocalityResult<Vec<TreeEntry>> {
    if let Some(team_id) = team_id_from_remote_id(remote_id) {
        return Ok(vec![team_issues_entry(mount_id, parent_path, team_id)]);
    }
    if let Some(team_id) = team_issues_team_id_from_remote_id(remote_id) {
        let issues = connector.all_issues(None, Some(team_id))?;
        return Ok(status_entries(mount_id, parent_path, team_id, &issues));
    }
    if let Some((team_id, state_id)) = team_state_from_remote_id(remote_id) {
        return Ok(connector
            .all_issues(None, Some(team_id))?
            .into_iter()
            .filter(|issue| issue.state.id == state_id)
            .map(|issue| status_child_issue_entry(mount_id, parent_path, &issue))
            .collect());
    }
    Ok(Vec::new())
}

fn team_id_from_remote_id(remote_id: &RemoteId) -> Option<&str> {
    remote_id.0.strip_prefix(TEAM_REMOTE_ID_PREFIX)
}

fn team_issues_team_id_from_remote_id(remote_id: &RemoteId) -> Option<&str> {
    remote_id.0.strip_prefix(TEAM_ISSUES_REMOTE_ID_PREFIX)
}

fn team_state_from_remote_id(remote_id: &RemoteId) -> Option<(&str, &str)> {
    let value = remote_id.0.strip_prefix(TEAM_STATE_REMOTE_ID_PREFIX)?;
    let (team_id, state_id) = value.split_once(':')?;
    (!team_id.is_empty() && !state_id.is_empty()).then_some((team_id, state_id))
}

fn team_remote_id(team_id: &str) -> String {
    format!("{TEAM_REMOTE_ID_PREFIX}{team_id}")
}

fn team_issues_remote_id(team_id: &str) -> String {
    format!("{TEAM_ISSUES_REMOTE_ID_PREFIX}{team_id}")
}

fn team_state_remote_id(team_id: &str, state_id: &str) -> String {
    format!("{TEAM_STATE_REMOTE_ID_PREFIX}{team_id}:{state_id}")
}

fn checkpoint_updated_after(checkpoint: &ConnectorCheckpoint) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(&checkpoint.state_json)
        .ok()?
        .get("updated_after")?
        .as_str()
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn dedupe_sort_issues(issues: Vec<LinearIssue>) -> LocalityResult<Vec<LinearIssue>> {
    let mut issues = issues
        .into_iter()
        .map(|issue| (issue.id.clone(), issue))
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect::<Vec<_>>();
    issues.sort_by(|left, right| {
        team_directory_name(&left.team)
            .cmp(&team_directory_name(&right.team))
            .then_with(|| {
                status_directory_name(&left.state).cmp(&status_directory_name(&right.state))
            })
            .then_with(|| left.identifier.cmp(&right.identifier))
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(issues)
}

fn unique_teams(issues: &[LinearIssue]) -> Vec<LinearTeam> {
    let mut teams = issues
        .iter()
        .map(|issue| (issue.team.id.clone(), issue.team.clone()))
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect::<Vec<_>>();
    teams.sort_by(|left, right| {
        team_directory_name(left)
            .cmp(&team_directory_name(right))
            .then_with(|| left.id.cmp(&right.id))
    });
    teams
}

fn unique_states(issues: &[LinearIssue]) -> Vec<(String, LinearIssueState)> {
    let mut states = issues
        .iter()
        .map(|issue| {
            (
                (issue.team.id.clone(), issue.state.id.clone()),
                issue.state.clone(),
            )
        })
        .collect::<BTreeMap<_, _>>()
        .into_iter()
        .map(|((team_id, _), state)| (team_id, state))
        .collect::<Vec<_>>();
    states.sort_by(|(left_team_id, left), (right_team_id, right)| {
        left_team_id
            .cmp(right_team_id)
            .then_with(|| status_directory_name(left).cmp(&status_directory_name(right)))
            .then_with(|| left.id.cmp(&right.id))
    });
    states
}

fn team_directory_name(team: &LinearTeam) -> String {
    safe_filename(&team.name, 120)
}

fn status_directory_name(state: &LinearIssueState) -> String {
    safe_filename(&state.name, 120)
}

fn issue_directory_name(issue: &LinearIssue) -> String {
    let identifier = safe_filename(&issue.identifier, 48);
    if issue.title.trim().is_empty() {
        return identifier;
    }
    let separator = " ";
    let title_limit = 120usize.saturating_sub(identifier.len() + separator.len());
    if title_limit == 0 {
        return identifier;
    }
    let title = safe_filename(&issue.title, title_limit);
    if title.is_empty() {
        identifier
    } else {
        format!("{identifier}{separator}{title}")
    }
}

fn safe_filename(value: &str, byte_limit: usize) -> String {
    let mut name = String::new();
    let mut pending_separator = false;
    for character in value.chars() {
        if character.is_control()
            || matches!(
                character,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            )
        {
            pending_separator = true;
            continue;
        }
        if character.is_whitespace() {
            pending_separator = true;
            continue;
        }
        let separator = if pending_separator && !name.is_empty() {
            " "
        } else {
            ""
        };
        if !name.is_empty() && name.len() + separator.len() + character.len_utf8() > byte_limit {
            break;
        }
        name.push_str(separator);
        name.push(character);
        pending_separator = false;
    }
    let name = name.trim_matches([' ', '.', '-']);
    if name.is_empty() {
        "Untitled".to_string()
    } else {
        name.to_string()
    }
}
