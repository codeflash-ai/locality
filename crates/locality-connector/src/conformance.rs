//! Reusable, credential-free connector conformance checks.

use std::collections::BTreeSet;
use std::fmt::{self, Debug};
use std::fs;
use std::path::{Component, Path};

use locality_core::planner::PushOperationKind;

use crate::manifest::{ConnectorManifest, is_safe_relative_identifier};
use crate::{Connector, ConnectorCapabilities};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConformanceError(pub String);

impl fmt::Display for ConformanceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ConformanceError {}

pub fn check_manifest_identity<C: Connector + ?Sized>(
    manifest: &ConnectorManifest,
    connector: &C,
) -> Result<(), ConformanceError> {
    let runtime_id = connector.kind().0;
    if manifest.id == runtime_id {
        Ok(())
    } else {
        Err(ConformanceError(format!(
            "manifest id `{}` does not match Connector::kind() `{runtime_id}`",
            manifest.id
        )))
    }
}

pub fn check_capability_operation_agreement<C: Connector + ?Sized>(
    manifest: &ConnectorManifest,
    connector: &C,
) -> Result<(), ConformanceError> {
    check_capabilities(manifest, &connector.capabilities())?;
    let runtime_operations = connector.supported_push_operations();
    let manifest_operations = manifest.runtime_push_operations();
    if manifest_operations != runtime_operations {
        let only_manifest = manifest_operations
            .difference(&runtime_operations)
            .map(PushOperationKind::as_str)
            .collect::<Vec<_>>();
        let only_runtime = runtime_operations
            .difference(&manifest_operations)
            .map(PushOperationKind::as_str)
            .collect::<Vec<_>>();
        return Err(ConformanceError(format!(
            "connector `{}` push operations drifted; manifest only: {only_manifest:?}; runtime only: {only_runtime:?}",
            manifest.id
        )));
    }
    Ok(())
}

pub fn check_capabilities(
    manifest: &ConnectorManifest,
    runtime: &ConnectorCapabilities,
) -> Result<(), ConformanceError> {
    let described = manifest.capabilities.as_runtime_capabilities();
    if described == *runtime {
        Ok(())
    } else {
        Err(ConformanceError(format!(
            "connector `{}` capabilities drifted; manifest: {described:?}; runtime: {runtime:?}",
            manifest.id
        )))
    }
}

pub fn check_manifest_asset_paths(manifest: &ConnectorManifest) -> Result<(), ConformanceError> {
    let icon_stem = manifest
        .ui
        .icon
        .strip_suffix(".svg")
        .ok_or_else(|| ConformanceError("icon must end in .svg".to_string()))?;
    if !is_safe_relative_identifier(icon_stem)
        || !is_safe_relative_identifier(&manifest.ui.docs_slug)
    {
        return Err(ConformanceError(format!(
            "connector `{}` has unsafe docs or icon identifiers",
            manifest.id
        )));
    }
    Ok(())
}

pub fn check_read_only_rejection(
    manifest: &ConnectorManifest,
    decisions_are_writable: impl IntoIterator<Item = bool>,
) -> Result<(), ConformanceError> {
    if !manifest.mount.read_only {
        return Ok(());
    }
    if decisions_are_writable.into_iter().any(|writable| writable) {
        Err(ConformanceError(format!(
            "read-only connector `{}` accepted a write decision",
            manifest.id
        )))
    } else {
        Ok(())
    }
}

pub fn check_debug_redaction(
    value: &impl Debug,
    secret_values: &[&str],
) -> Result<(), ConformanceError> {
    let rendered = format!("{value:?}");
    for secret in secret_values {
        if !secret.is_empty() && rendered.contains(secret) {
            return Err(ConformanceError(
                "Debug output contains credential or secret material".to_string(),
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixtureLayout<'a> {
    pub version_directory: &'a str,
    pub required_files: &'a [&'a str],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectFixtureAuth {
    Oauth,
    Token,
    ApiKey,
}

pub fn check_direct_fixture_layout(
    connector_crate_root: &Path,
    version_directory: &str,
    auth: DirectFixtureAuth,
) -> Result<(), ConformanceError> {
    if !is_versioned_direct_fixture_directory(version_directory) {
        return Err(ConformanceError(format!(
            "direct fixture directory `{version_directory}` must use direct-v<positive integer>"
        )));
    }
    let auth_file = match auth {
        DirectFixtureAuth::Oauth => "auth-scopes.json",
        DirectFixtureAuth::Token | DirectFixtureAuth::ApiKey => "auth-kind.txt",
    };
    check_fixture_layout(
        connector_crate_root,
        &FixtureLayout {
            version_directory,
            required_files: &[
                ".gitattributes",
                "tree-paths.txt",
                "settings-default.json",
                auth_file,
            ],
        },
    )?;

    let root = connector_crate_root
        .join("fixtures")
        .join(version_directory);
    for obsolete_or_conflicting in match auth {
        DirectFixtureAuth::Oauth => ["oauth-scopes.json", "auth-kind.txt"],
        DirectFixtureAuth::Token | DirectFixtureAuth::ApiKey => {
            ["oauth-scopes.json", "auth-scopes.json"]
        }
    } {
        if root.join(obsolete_or_conflicting).exists() {
            return Err(ConformanceError(format!(
                "fixture `{}` conflicts with standardized auth file `{auth_file}`",
                root.join(obsolete_or_conflicting).display()
            )));
        }
    }

    match auth {
        DirectFixtureAuth::Oauth => validate_oauth_scope_fixture(&root.join(auth_file))?,
        DirectFixtureAuth::Token | DirectFixtureAuth::ApiKey => {
            let expected = match auth {
                DirectFixtureAuth::Token => "token",
                DirectFixtureAuth::ApiKey => "api_key",
                DirectFixtureAuth::Oauth => unreachable!(),
            };
            let actual = fs::read_to_string(root.join(auth_file)).map_err(|error| {
                ConformanceError(format!("failed to read `{auth_file}`: {error}"))
            })?;
            if actual.trim() != expected {
                return Err(ConformanceError(format!(
                    "`{auth_file}` must contain exactly `{expected}`"
                )));
            }
        }
    }

    let mut native_cases = BTreeSet::new();
    for entry in fs::read_dir(&root).map_err(|error| {
        ConformanceError(format!("failed to read `{}`: {error}", root.display()))
    })? {
        let entry = entry.map_err(|error| ConformanceError(error.to_string()))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(case) = name
            .strip_prefix("native-")
            .and_then(|name| name.strip_suffix(".json"))
        else {
            continue;
        };
        if !is_safe_relative_identifier(case) {
            return Err(ConformanceError(format!(
                "native fixture case `{case}` must be a safe identifier"
            )));
        }
        native_cases.insert(case.to_string());
    }
    if native_cases.is_empty() {
        return Err(ConformanceError(format!(
            "fixture directory `{}` must contain at least one native-<case>.json",
            root.display()
        )));
    }
    for case in native_cases {
        let rendered = root.join(format!("{case}.md"));
        if !rendered.is_file() {
            return Err(ConformanceError(format!(
                "native fixture case `{case}` is missing `{}`",
                rendered.display()
            )));
        }
    }
    Ok(())
}

fn is_versioned_direct_fixture_directory(value: &str) -> bool {
    value
        .strip_prefix("direct-v")
        .and_then(|version| version.parse::<u16>().ok().map(|parsed| (version, parsed)))
        .is_some_and(|(version, parsed)| parsed > 0 && version == parsed.to_string())
}

fn validate_oauth_scope_fixture(path: &Path) -> Result<(), ConformanceError> {
    let bytes = fs::read(path).map_err(|error| {
        ConformanceError(format!("failed to read `{}`: {error}", path.display()))
    })?;
    let scopes = serde_json::from_slice::<Vec<String>>(&bytes).map_err(|error| {
        ConformanceError(format!(
            "OAuth scope fixture `{}` is invalid: {error}",
            path.display()
        ))
    })?;
    if scopes.is_empty() || scopes.iter().any(String::is_empty) {
        return Err(ConformanceError(
            "OAuth scope fixture must contain non-empty scope names".to_string(),
        ));
    }
    if scopes.iter().collect::<BTreeSet<_>>().len() != scopes.len() {
        return Err(ConformanceError(
            "OAuth scope fixture must not contain duplicate scopes".to_string(),
        ));
    }
    Ok(())
}

pub fn check_fixture_layout(
    connector_crate_root: &Path,
    layout: &FixtureLayout<'_>,
) -> Result<(), ConformanceError> {
    if !is_safe_relative_identifier(layout.version_directory) {
        return Err(ConformanceError(format!(
            "fixture version directory `{}` is unsafe",
            layout.version_directory
        )));
    }
    let root = connector_crate_root
        .join("fixtures")
        .join(layout.version_directory);
    if !root.is_dir() {
        return Err(ConformanceError(format!(
            "fixture directory `{}` is missing",
            root.display()
        )));
    }
    for relative in layout.required_files {
        let relative = Path::new(relative);
        if !is_safe_relative_path(relative) {
            return Err(ConformanceError(format!(
                "fixture path `{}` is unsafe",
                relative.display()
            )));
        }
        let fixture = root.join(relative);
        if !fixture.is_file() {
            return Err(ConformanceError(format!(
                "required fixture `{}` is missing",
                fixture.display()
            )));
        }
    }
    Ok(())
}

pub fn is_safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path.components().all(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .is_some_and(|value| !value.is_empty() && !value.chars().any(char::is_control)),
            _ => false,
        })
}
