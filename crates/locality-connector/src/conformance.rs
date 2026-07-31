//! Reusable, credential-free connector conformance checks.

use std::fmt::{self, Debug};
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
