//! Versioned descriptive connector manifest contract.
//!
//! The manifest is discovery and conformance metadata. It never grants network,
//! credential, filesystem, or push authority; hosts must continue to enforce
//! those policies in trusted code.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::OnceLock;

use locality_core::planner::PushOperationKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ConnectorCapabilities;

pub const CONNECTOR_REGISTRY_SCHEMA_VERSION: u16 = 1;
pub const CONNECTOR_REGISTRY_JSON: &str = include_str!("../../../connectors/registry.json");
pub const CONNECTOR_REGISTRY_SCHEMA_JSON: &str =
    include_str!("../../../connectors/registry.schema.json");

const MAX_CONNECTORS: usize = 64;
const MAX_PROFILES: usize = 8;
const MAX_SCOPES: usize = 64;
const MAX_ACTIONS: usize = 16;
const MAX_ID_LEN: usize = 64;
const MAX_DISPLAY_NAME_LEN: usize = 80;
const MAX_SCOPE_LEN: usize = 256;
const MIN_DISCOVERY_SECONDS: u64 = 30;
const MAX_DISCOVERY_SECONDS: u64 = 86_400;
const MAX_BACKGROUND_WORKERS: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorRegistry {
    #[serde(rename = "$schema")]
    pub schema: String,
    pub schema_version: u16,
    pub connectors: Vec<ConnectorManifest>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorManifest {
    pub id: String,
    pub version: String,
    pub display_name: String,
    #[serde(rename = "crate")]
    pub crate_path: String,
    pub default_profile_id: String,
    pub default_connection_id: String,
    pub profiles: Vec<ConnectorProfileManifest>,
    pub mount: ConnectorMountManifest,
    pub capabilities: ManifestCapabilities,
    pub push_operations: Vec<ManifestPushOperation>,
    pub membership_operations: Vec<MembershipOperation>,
    pub projection: ProjectionPolicyManifest,
    pub ui: ConnectorUiManifest,
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorProfileManifest {
    pub id: String,
    pub display_name: String,
    pub auth_kind: AuthKind,
    pub scopes: Vec<String>,
    pub actions: Vec<ConnectorAction>,
}

impl fmt::Debug for ConnectorProfileManifest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectorProfileManifest")
            .field("id", &self.id)
            .field("display_name", &self.display_name)
            .field("auth_kind", &self.auth_kind)
            .field(
                "scopes",
                &format_args!("<{} descriptive scopes>", self.scopes.len()),
            )
            .field("actions", &self.actions)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorMountManifest {
    pub default_id: String,
    pub read_only: bool,
    pub default_projection_mode: ProjectionMode,
    pub default_settings: Value,
    pub settings_schema: Value,
}

impl fmt::Debug for ConnectorMountManifest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectorMountManifest")
            .field("default_id", &self.default_id)
            .field("read_only", &self.read_only)
            .field("default_projection_mode", &self.default_projection_mode)
            .field("default_settings", &"<descriptive settings>")
            .field("settings_schema", &"<descriptive schema>")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestCapabilities {
    pub supports_block_updates: bool,
    pub supports_entity_body_updates: bool,
    pub supports_databases: bool,
    pub supports_oauth: bool,
    pub supports_remote_observation: bool,
    pub supports_lazy_child_enumeration: bool,
    pub supports_media_download: bool,
    pub supports_undo: bool,
    pub supports_batch_observation: bool,
}

impl ManifestCapabilities {
    pub fn as_runtime_capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_block_updates: self.supports_block_updates,
            supports_entity_body_updates: self.supports_entity_body_updates,
            supports_databases: self.supports_databases,
            supports_oauth: self.supports_oauth,
            supports_remote_observation: self.supports_remote_observation,
            supports_lazy_child_enumeration: self.supports_lazy_child_enumeration,
            supports_media_download: self.supports_media_download,
            supports_undo: self.supports_undo,
            supports_batch_observation: self.supports_batch_observation,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionPolicyManifest {
    pub source_root_create_parent_kind: Option<ManifestEntityKind>,
    pub create_entity_parent_kinds: Vec<ManifestEntityKind>,
    pub move_entity_parent_kinds: Vec<ManifestEntityKind>,
    pub body_diff_mode: BodyDiffMode,
    pub virtual_rename_policy: VirtualRenamePolicy,
    pub periodic_discovery_seconds: Option<u64>,
    pub max_background_discovery_workers: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorUiManifest {
    pub icon: String,
    pub docs_slug: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    Oauth,
    Token,
    ApiKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorAction {
    Read,
    Write,
    Create,
    Send,
}

impl ConnectorAction {
    fn mutates_remote(self) -> bool {
        !matches!(self, Self::Read)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionMode {
    PlainFiles,
    MacosFileProvider,
    LinuxFuse,
    WindowsCloudFiles,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestPushOperation {
    UpdateBlock,
    ReplaceBlock,
    AppendBlock,
    MoveBlock,
    UpdateMedia,
    ArchiveBlock,
    ArchiveEntity,
    UpdateEntityBody,
    UpdateProperties,
    MoveEntity,
    CreateEntity,
    CreateDatabase,
}

/// Remote membership changes that are not content writes or push authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MembershipOperation {
    JoinPublicChannels,
}

impl ManifestPushOperation {
    pub fn as_runtime_kind(self) -> PushOperationKind {
        match self {
            Self::UpdateBlock => PushOperationKind::UpdateBlock,
            Self::ReplaceBlock => PushOperationKind::ReplaceBlock,
            Self::AppendBlock => PushOperationKind::AppendBlock,
            Self::MoveBlock => PushOperationKind::MoveBlock,
            Self::UpdateMedia => PushOperationKind::UpdateMedia,
            Self::ArchiveBlock => PushOperationKind::ArchiveBlock,
            Self::ArchiveEntity => PushOperationKind::ArchiveEntity,
            Self::UpdateEntityBody => PushOperationKind::UpdateEntityBody,
            Self::UpdateProperties => PushOperationKind::UpdateProperties,
            Self::MoveEntity => PushOperationKind::MoveEntity,
            Self::CreateEntity => PushOperationKind::CreateEntity,
            Self::CreateDatabase => PushOperationKind::CreateDatabase,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestEntityKind {
    Page,
    Database,
    Directory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyDiffMode {
    Block,
    WholeEntity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VirtualRenamePolicy {
    FilenameDerived,
    PreserveCanonical,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestError {
    Json(String),
    Validation(Vec<ManifestViolation>),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(message) => {
                write!(formatter, "connector registry JSON is invalid: {message}")
            }
            Self::Validation(violations) => {
                write!(formatter, "connector registry validation failed")?;
                for violation in violations {
                    write!(formatter, "; {}: {}", violation.path, violation.message)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ManifestError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestViolation {
    pub path: String,
    pub message: String,
}

impl ManifestViolation {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl ConnectorRegistry {
    pub fn parse(json: &str) -> Result<Self, ManifestError> {
        let registry = serde_json::from_str::<Self>(json)
            .map_err(|error| ManifestError::Json(error.to_string()))?;
        registry.validate()?;
        Ok(registry)
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
        let mut violations = Vec::new();
        if self.schema != "./registry.schema.json" {
            violations.push(ManifestViolation::new(
                "$.$schema",
                "must be ./registry.schema.json",
            ));
        }
        if self.schema_version != CONNECTOR_REGISTRY_SCHEMA_VERSION {
            violations.push(ManifestViolation::new(
                "$.schema_version",
                format!(
                    "unsupported version {}; expected {CONNECTOR_REGISTRY_SCHEMA_VERSION}",
                    self.schema_version
                ),
            ));
        }
        if self.connectors.is_empty() || self.connectors.len() > MAX_CONNECTORS {
            violations.push(ManifestViolation::new(
                "$.connectors",
                format!("must contain between 1 and {MAX_CONNECTORS} connectors"),
            ));
        }

        let mut connector_ids = BTreeSet::new();
        let mut profile_ids = BTreeSet::new();
        let mut connection_ids = BTreeSet::new();
        let mut mount_ids = BTreeSet::new();
        for (index, connector) in self.connectors.iter().enumerate() {
            let path = format!("$.connectors[{index}]");
            validate_connector(connector, &path, &mut violations);
            require_unique(
                &mut connector_ids,
                &connector.id,
                format!("{path}.id"),
                "connector id",
                &mut violations,
            );
            require_unique(
                &mut connection_ids,
                &connector.default_connection_id,
                format!("{path}.default_connection_id"),
                "default connection id",
                &mut violations,
            );
            require_unique(
                &mut mount_ids,
                &connector.mount.default_id,
                format!("{path}.mount.default_id"),
                "default mount id",
                &mut violations,
            );
            for (profile_index, profile) in connector.profiles.iter().enumerate() {
                require_unique(
                    &mut profile_ids,
                    &profile.id,
                    format!("{path}.profiles[{profile_index}].id"),
                    "profile id",
                    &mut violations,
                );
            }
        }

        if violations.is_empty() {
            Ok(())
        } else {
            Err(ManifestError::Validation(violations))
        }
    }

    pub fn connector(&self, id: &str) -> Option<&ConnectorManifest> {
        self.connectors.iter().find(|connector| connector.id == id)
    }
}

impl ConnectorManifest {
    pub fn runtime_push_operations(&self) -> BTreeSet<PushOperationKind> {
        self.push_operations
            .iter()
            .map(|operation| operation.as_runtime_kind())
            .collect()
    }

    pub fn has_oauth_profile(&self) -> bool {
        self.profiles
            .iter()
            .any(|profile| profile.auth_kind == AuthKind::Oauth)
    }
}

static BUNDLED_REGISTRY: OnceLock<Result<ConnectorRegistry, ManifestError>> = OnceLock::new();

pub fn bundled_connector_registry() -> Result<&'static ConnectorRegistry, ManifestError> {
    match BUNDLED_REGISTRY.get_or_init(|| ConnectorRegistry::parse(CONNECTOR_REGISTRY_JSON)) {
        Ok(registry) => Ok(registry),
        Err(error) => Err(error.clone()),
    }
}

pub fn is_safe_relative_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_LEN
        && value.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

fn validate_connector(
    connector: &ConnectorManifest,
    path: &str,
    violations: &mut Vec<ManifestViolation>,
) {
    validate_identifier(&connector.id, format!("{path}.id"), violations);
    validate_identifier(
        &connector.default_profile_id,
        format!("{path}.default_profile_id"),
        violations,
    );
    validate_identifier(
        &connector.default_connection_id,
        format!("{path}.default_connection_id"),
        violations,
    );
    validate_display_name(
        &connector.display_name,
        format!("{path}.display_name"),
        violations,
    );
    let crate_suffix = connector.crate_path.strip_prefix("crates/locality-");
    if crate_suffix.is_none_or(|suffix| !is_safe_relative_identifier(suffix)) {
        violations.push(ManifestViolation::new(
            format!("{path}.crate"),
            "must be a safe crates/locality-<connector> path",
        ));
    }
    let expected_version_prefix = format!("{}.v", connector.id);
    if connector.version.len() > MAX_DISPLAY_NAME_LEN
        || !connector.version.starts_with(&expected_version_prefix)
        || connector.version[expected_version_prefix.len()..]
            .parse::<u16>()
            .ok()
            .is_none_or(|version| version == 0)
    {
        violations.push(ManifestViolation::new(
            format!("{path}.version"),
            format!("must use {}<positive integer>", expected_version_prefix),
        ));
    }

    if connector.profiles.is_empty() || connector.profiles.len() > MAX_PROFILES {
        violations.push(ManifestViolation::new(
            format!("{path}.profiles"),
            format!("must contain between 1 and {MAX_PROFILES} profiles"),
        ));
    }
    let mut local_profile_ids = BTreeSet::new();
    for (index, profile) in connector.profiles.iter().enumerate() {
        let profile_path = format!("{path}.profiles[{index}]");
        validate_profile(profile, &profile_path, violations);
        require_unique(
            &mut local_profile_ids,
            &profile.id,
            format!("{profile_path}.id"),
            "connector profile id",
            violations,
        );
    }
    if !local_profile_ids.contains(connector.default_profile_id.as_str()) {
        violations.push(ManifestViolation::new(
            format!("{path}.default_profile_id"),
            "must name exactly one profile in this connector",
        ));
    }

    let has_oauth = connector.has_oauth_profile();
    if connector.capabilities.supports_oauth != has_oauth {
        violations.push(ManifestViolation::new(
            format!("{path}.capabilities.supports_oauth"),
            "must agree with the presence of an OAuth profile",
        ));
    }

    validate_mount(&connector.mount, &format!("{path}.mount"), violations);
    validate_projection(
        &connector.projection,
        &format!("{path}.projection"),
        violations,
    );
    validate_operations(connector, path, violations);

    let icon_stem = connector.ui.icon.strip_suffix(".svg");
    if icon_stem.is_none_or(|stem| !is_safe_relative_identifier(stem)) {
        violations.push(ManifestViolation::new(
            format!("{path}.ui.icon"),
            "must be a safe relative kebab-case .svg filename",
        ));
    }
    validate_identifier(
        &connector.ui.docs_slug,
        format!("{path}.ui.docs_slug"),
        violations,
    );
}

fn validate_profile(
    profile: &ConnectorProfileManifest,
    path: &str,
    violations: &mut Vec<ManifestViolation>,
) {
    validate_identifier(&profile.id, format!("{path}.id"), violations);
    validate_display_name(
        &profile.display_name,
        format!("{path}.display_name"),
        violations,
    );
    validate_unique_strings(
        &profile.scopes,
        MAX_SCOPES,
        MAX_SCOPE_LEN,
        &format!("{path}.scopes"),
        violations,
    );
    validate_unique_values(
        &profile.actions,
        MAX_ACTIONS,
        &format!("{path}.actions"),
        violations,
    );
}

fn validate_mount(
    mount: &ConnectorMountManifest,
    path: &str,
    violations: &mut Vec<ManifestViolation>,
) {
    validate_identifier(&mount.default_id, format!("{path}.default_id"), violations);
    if !mount.default_settings.is_object() {
        violations.push(ManifestViolation::new(
            format!("{path}.default_settings"),
            "must be a JSON object",
        ));
    }
    if !mount.settings_schema.is_object() {
        violations.push(ManifestViolation::new(
            format!("{path}.settings_schema"),
            "must be a JSON Schema object",
        ));
    }
    reject_sensitive_keys(
        &mount.default_settings,
        &format!("{path}.default_settings"),
        violations,
    );
    reject_sensitive_keys(
        &mount.settings_schema,
        &format!("{path}.settings_schema"),
        violations,
    );
}

fn validate_projection(
    projection: &ProjectionPolicyManifest,
    path: &str,
    violations: &mut Vec<ManifestViolation>,
) {
    validate_unique_values(
        &projection.create_entity_parent_kinds,
        3,
        &format!("{path}.create_entity_parent_kinds"),
        violations,
    );
    validate_unique_values(
        &projection.move_entity_parent_kinds,
        3,
        &format!("{path}.move_entity_parent_kinds"),
        violations,
    );
    if projection
        .periodic_discovery_seconds
        .is_some_and(|seconds| !(MIN_DISCOVERY_SECONDS..=MAX_DISCOVERY_SECONDS).contains(&seconds))
    {
        violations.push(ManifestViolation::new(
            format!("{path}.periodic_discovery_seconds"),
            format!("must be null or between {MIN_DISCOVERY_SECONDS} and {MAX_DISCOVERY_SECONDS}"),
        ));
    }
    if !(1..=MAX_BACKGROUND_WORKERS).contains(&projection.max_background_discovery_workers) {
        violations.push(ManifestViolation::new(
            format!("{path}.max_background_discovery_workers"),
            format!("must be between 1 and {MAX_BACKGROUND_WORKERS}"),
        ));
    }
    if projection.body_diff_mode == BodyDiffMode::WholeEntity
        && projection.virtual_rename_policy != VirtualRenamePolicy::PreserveCanonical
    {
        violations.push(ManifestViolation::new(
            format!("{path}.virtual_rename_policy"),
            "whole-entity projections must preserve their canonical title",
        ));
    }
}

fn validate_operations(
    connector: &ConnectorManifest,
    path: &str,
    violations: &mut Vec<ManifestViolation>,
) {
    validate_unique_values(
        &connector.push_operations,
        12,
        &format!("{path}.push_operations"),
        violations,
    );
    validate_unique_values(
        &connector.membership_operations,
        4,
        &format!("{path}.membership_operations"),
        violations,
    );
    let operations = connector
        .push_operations
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let block_operations = [
        ManifestPushOperation::UpdateBlock,
        ManifestPushOperation::ReplaceBlock,
        ManifestPushOperation::AppendBlock,
        ManifestPushOperation::MoveBlock,
        ManifestPushOperation::ArchiveBlock,
    ];
    let has_block_operation = block_operations
        .iter()
        .any(|operation| operations.contains(operation));
    if connector.capabilities.supports_block_updates != has_block_operation {
        violations.push(ManifestViolation::new(
            format!("{path}.capabilities.supports_block_updates"),
            "must agree with declared block push operations",
        ));
    }
    if connector.capabilities.supports_entity_body_updates
        != operations.contains(&ManifestPushOperation::UpdateEntityBody)
    {
        violations.push(ManifestViolation::new(
            format!("{path}.capabilities.supports_entity_body_updates"),
            "must agree with update_entity_body",
        ));
    }
    if operations.contains(&ManifestPushOperation::CreateDatabase)
        && !connector.capabilities.supports_databases
    {
        violations.push(ManifestViolation::new(
            format!("{path}.push_operations"),
            "create_database requires supports_databases",
        ));
    }
    if operations.contains(&ManifestPushOperation::UpdateMedia)
        && !connector.capabilities.supports_media_download
    {
        violations.push(ManifestViolation::new(
            format!("{path}.push_operations"),
            "update_media requires media support",
        ));
    }
    if connector.capabilities.supports_undo && operations.is_empty() {
        violations.push(ManifestViolation::new(
            format!("{path}.capabilities.supports_undo"),
            "undo support requires at least one push operation",
        ));
    }
    if connector.mount.read_only {
        if !operations.is_empty() {
            violations.push(ManifestViolation::new(
                format!("{path}.push_operations"),
                "read-only connectors cannot describe push operations",
            ));
        }
        if connector
            .profiles
            .iter()
            .flat_map(|profile| profile.actions.iter())
            .copied()
            .any(ConnectorAction::mutates_remote)
        {
            violations.push(ManifestViolation::new(
                format!("{path}.profiles"),
                "read-only connectors cannot describe mutating actions",
            ));
        }
    }
    if connector
        .membership_operations
        .contains(&MembershipOperation::JoinPublicChannels)
        && !connector
            .profiles
            .iter()
            .any(|profile| profile.scopes.iter().any(|scope| scope == "channels:join"))
    {
        violations.push(ManifestViolation::new(
            format!("{path}.membership_operations"),
            "join_public_channels requires a profile with channels:join scope",
        ));
    }
}

fn validate_identifier(value: &str, path: String, violations: &mut Vec<ManifestViolation>) {
    if !is_safe_relative_identifier(value) {
        violations.push(ManifestViolation::new(
            path,
            "must be a safe kebab-case identifier",
        ));
    }
}

fn validate_display_name(value: &str, path: String, violations: &mut Vec<ManifestViolation>) {
    if value.is_empty() || value.len() > MAX_DISPLAY_NAME_LEN || value.chars().any(char::is_control)
    {
        violations.push(ManifestViolation::new(
            path,
            format!("must be 1 to {MAX_DISPLAY_NAME_LEN} non-control characters"),
        ));
    }
}

fn validate_unique_strings(
    values: &[String],
    max_items: usize,
    max_len: usize,
    path: &str,
    violations: &mut Vec<ManifestViolation>,
) {
    if values.len() > max_items {
        violations.push(ManifestViolation::new(
            path,
            format!("must contain at most {max_items} values"),
        ));
    }
    let mut unique = BTreeSet::new();
    for (index, value) in values.iter().enumerate() {
        if value.is_empty() || value.len() > max_len || value.chars().any(char::is_control) {
            violations.push(ManifestViolation::new(
                format!("{path}[{index}]"),
                format!("must be 1 to {max_len} non-control characters"),
            ));
        }
        if !unique.insert(value) {
            violations.push(ManifestViolation::new(
                format!("{path}[{index}]"),
                "must be unique",
            ));
        }
    }
}

fn validate_unique_values<T: Ord>(
    values: &[T],
    max_items: usize,
    path: &str,
    violations: &mut Vec<ManifestViolation>,
) {
    if values.len() > max_items {
        violations.push(ManifestViolation::new(
            path,
            format!("must contain at most {max_items} values"),
        ));
    }
    let mut unique = BTreeSet::new();
    for (index, value) in values.iter().enumerate() {
        if !unique.insert(value) {
            violations.push(ManifestViolation::new(
                format!("{path}[{index}]"),
                "must be unique",
            ));
        }
    }
}

fn require_unique<'a>(
    values: &mut BTreeSet<&'a str>,
    value: &'a str,
    path: String,
    label: &str,
    violations: &mut Vec<ManifestViolation>,
) {
    if !values.insert(value) {
        violations.push(ManifestViolation::new(
            path,
            format!("duplicate {label} `{value}`"),
        ));
    }
}

fn reject_sensitive_keys(value: &Value, path: &str, violations: &mut Vec<ManifestViolation>) {
    match value {
        Value::Object(object) => {
            for (key, nested) in object {
                let nested_path = format!("{path}.{key}");
                if is_sensitive_setting_key(key) {
                    violations.push(ManifestViolation::new(
                        &nested_path,
                        "connector manifests cannot contain credential-bearing settings",
                    ));
                }
                reject_sensitive_keys(nested, &nested_path, violations);
            }
        }
        Value::Array(values) => {
            for (index, nested) in values.iter().enumerate() {
                reject_sensitive_keys(nested, &format!("{path}[{index}]"), violations);
            }
        }
        _ => {}
    }
}

fn is_sensitive_setting_key(key: &str) -> bool {
    let chars = key.chars().collect::<Vec<_>>();
    let mut normalized = String::with_capacity(key.len());
    for (index, character) in chars.iter().copied().enumerate() {
        if !character.is_ascii_alphanumeric() {
            if !normalized.is_empty() && !normalized.ends_with('_') {
                normalized.push('_');
            }
            continue;
        }
        let previous = index.checked_sub(1).and_then(|index| chars.get(index));
        let next = chars.get(index + 1);
        let camel_boundary = character.is_ascii_uppercase()
            && previous.is_some_and(|previous| {
                previous.is_ascii_lowercase()
                    || previous.is_ascii_digit()
                    || (previous.is_ascii_uppercase() && next.is_some_and(char::is_ascii_lowercase))
            });
        if camel_boundary && !normalized.is_empty() && !normalized.ends_with('_') {
            normalized.push('_');
        }
        normalized.push(character.to_ascii_lowercase());
    }

    let segments = normalized
        .split('_')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.iter().any(|segment| {
        matches!(
            *segment,
            "token"
                | "secret"
                | "password"
                | "credential"
                | "credentials"
                | "authorization"
                | "bearer"
        )
    }) {
        return true;
    }
    segments
        .windows(2)
        .any(|pair| matches!(pair, ["api" | "private", "key"]))
}
