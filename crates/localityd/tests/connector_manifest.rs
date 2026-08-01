use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::Duration;

use locality_connector::Connector;
use locality_connector::conformance::{
    FixtureLayout, check_capability_operation_agreement, check_debug_redaction,
    check_fixture_layout, check_manifest_asset_paths, check_manifest_identity,
    check_read_only_rejection,
};
use locality_connector::manifest::{
    BodyDiffMode as ManifestBodyDiffMode, ManifestEntityKind,
    VirtualRenamePolicy as ManifestRenamePolicy, bundled_connector_registry,
};
use locality_core::model::{EntityKind, MountId, RemoteId};
use locality_core::push::BodyDiffMode;
use locality_gmail::{GmailConfig, GmailConnector};
use locality_google_calendar::{GoogleCalendarConfig, GoogleCalendarConnector};
use locality_google_docs::{GoogleDocsConfig, GoogleDocsConnector, StoredGoogleDocsCredential};
use locality_granola::{GranolaConfig, GranolaConnector};
use locality_linear::{LinearConfig, LinearConnector};
use locality_notion::{NotionConfig, NotionConnector};
use locality_slack::{SlackConfig, SlackConnector};
use locality_store::MountConfig;
use localityd::source::{
    VirtualRenamePolicy, registered_source_contracts, source_create_decision_for_parent_path,
    source_move_decision_for_parent_path, source_write_decision_for_path,
    supported_source_connectors,
};

fn runtime_connectors() -> Vec<(&'static str, Box<dyn Connector>)> {
    vec![
        (
            "notion",
            Box::new(NotionConnector::new(
                NotionConfig::default().with_token("notion-secret-sentinel"),
            )),
        ),
        (
            "google-docs",
            Box::new(GoogleDocsConnector::new(
                GoogleDocsConfig::new("google-docs-secret-sentinel")
                    .with_workspace_folder_id(RemoteId::new("folder")),
            )),
        ),
        (
            "google-calendar",
            Box::new(GoogleCalendarConnector::new(GoogleCalendarConfig::new(
                "google-calendar-secret-sentinel",
            ))),
        ),
        (
            "gmail",
            Box::new(GmailConnector::new(GmailConfig::new(
                "gmail-secret-sentinel",
            ))),
        ),
        (
            "granola",
            Box::new(GranolaConnector::new(GranolaConfig::new(
                "granola-secret-sentinel",
            ))),
        ),
        (
            "linear",
            Box::new(LinearConnector::new(LinearConfig::new(
                "linear-secret-sentinel",
            ))),
        ),
        (
            "slack",
            Box::new(SlackConnector::new(SlackConfig::new(
                "slack-secret-sentinel",
            ))),
        ),
    ]
}

#[test]
fn runtime_registry_has_one_manifest_per_connector_in_canonical_order() {
    let registry = bundled_connector_registry().expect("manifest registry");
    let contracts = registered_source_contracts().expect("registered source contracts");
    let manifest_ids = registry
        .connectors
        .iter()
        .map(|manifest| manifest.id.as_str())
        .collect::<Vec<_>>();
    let registration_ids = contracts
        .iter()
        .map(|contract| contract.manifest.id.as_str())
        .collect::<Vec<_>>();

    assert_eq!(registration_ids, manifest_ids);
    assert_eq!(supported_source_connectors(), manifest_ids);
    assert_eq!(
        registration_ids
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len(),
        registration_ids.len(),
        "duplicate runtime source registration"
    );
}

#[test]
fn connector_crates_cannot_omit_manifest_runtime_docs_or_icon_registration() {
    let repository_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let infrastructure_crates = [
        "locality-connector",
        "locality-core",
        "locality-engine",
        "locality-platform",
        "locality-protocol",
        "locality-store",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    let mut connector_crates = fs::read_dir(repository_root.join("crates"))
        .expect("read crates directory")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            name.starts_with("locality-") && !infrastructure_crates.contains(name.as_str())
        })
        .filter(|name| {
            fs::read_to_string(repository_root.join("crates").join(name).join("Cargo.toml"))
                .is_ok_and(|cargo| cargo.contains("locality-connector.workspace = true"))
        })
        .map(|name| format!("crates/{name}"))
        .collect::<Vec<_>>();
    connector_crates.sort();

    let mut manifest_crates = bundled_connector_registry()
        .expect("registry")
        .connectors
        .iter()
        .map(|manifest| manifest.crate_path.clone())
        .collect::<Vec<_>>();
    manifest_crates.sort();
    assert_eq!(connector_crates, manifest_crates);

    let runtime_ids = supported_source_connectors()
        .into_iter()
        .collect::<BTreeSet<_>>();
    for manifest in &bundled_connector_registry().expect("registry").connectors {
        assert!(runtime_ids.contains(manifest.id.as_str()));
        assert!(
            repository_root
                .join("docs-site/connectors")
                .join(format!("{}.mdx", manifest.ui.docs_slug))
                .is_file()
        );
        assert!(
            repository_root
                .join("apps/desktop/src/assets/connectors")
                .join(&manifest.ui.icon)
                .is_file()
        );
    }
}

#[test]
fn connector_kinds_capabilities_and_push_operations_match_manifests() {
    let registry = bundled_connector_registry().expect("manifest registry");
    for (id, connector) in runtime_connectors() {
        let manifest = registry.connector(id).expect("connector manifest");
        check_manifest_identity(manifest, connector.as_ref()).expect("manifest identity");
        check_capability_operation_agreement(manifest, connector.as_ref())
            .expect("capability and operation agreement");
        assert_eq!(
            manifest.has_oauth_profile(),
            manifest.capabilities.supports_oauth
        );
    }
}

#[test]
fn source_descriptors_match_manifest_defaults_and_projection_policy() {
    for contract in registered_source_contracts().expect("registered source contracts") {
        let manifest = contract.manifest;
        let descriptor = contract.descriptor;
        assert_eq!(descriptor.id(), manifest.id);
        assert_eq!(descriptor.display_name(), manifest.display_name);
        assert_eq!(descriptor.default_mount_id(), manifest.mount.default_id);
        assert_eq!(descriptor.supports_oauth(), manifest.has_oauth_profile());
        assert_eq!(
            descriptor.source_root_create_parent_kind(),
            manifest
                .projection
                .source_root_create_parent_kind
                .map(runtime_entity_kind)
        );
        assert_eq!(
            descriptor.create_entity_parent_kinds(),
            manifest
                .projection
                .create_entity_parent_kinds
                .iter()
                .copied()
                .map(runtime_entity_kind)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            descriptor.move_entity_parent_kinds(),
            manifest
                .projection
                .move_entity_parent_kinds
                .iter()
                .copied()
                .map(runtime_entity_kind)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            descriptor.periodic_discovery_interval(),
            manifest
                .projection
                .periodic_discovery_seconds
                .map(Duration::from_secs)
        );
        assert_eq!(
            descriptor.max_background_discovery_workers(),
            manifest.projection.max_background_discovery_workers
        );
        assert_eq!(
            descriptor.body_diff_mode(),
            match manifest.projection.body_diff_mode {
                ManifestBodyDiffMode::Block => BodyDiffMode::Block,
                ManifestBodyDiffMode::WholeEntity => BodyDiffMode::WholeEntity,
            }
        );
        assert_eq!(
            descriptor.virtual_rename_policy(),
            match manifest.projection.virtual_rename_policy {
                ManifestRenamePolicy::FilenameDerived => VirtualRenamePolicy::FilenameDerived,
                ManifestRenamePolicy::PreserveCanonical => VirtualRenamePolicy::PreserveCanonical,
            }
        );
    }
}

#[test]
fn read_only_manifests_reject_host_write_create_and_move_paths() {
    for contract in registered_source_contracts().expect("registered source contracts") {
        let manifest = contract.manifest;
        assert_eq!(
            contract.content_read_only_reason.is_some(),
            manifest.mount.read_only,
            "code-owned host policy drifted for {}",
            manifest.id
        );
        if !manifest.mount.read_only {
            continue;
        }

        assert!(manifest.push_operations.is_empty());
        let mut mount = MountConfig::new(
            MountId::new(format!("{}-conformance", manifest.id)),
            &manifest.id,
            "/tmp/source",
        );
        mount.read_only = false;
        check_read_only_rejection(
            &manifest,
            [
                source_write_decision_for_path(&mount, Path::new("item/page.md")).is_writable(),
                source_create_decision_for_parent_path(&mount, Path::new("item")).is_writable(),
                source_move_decision_for_parent_path(&mount, Path::new("item")).is_writable(),
            ],
        )
        .expect("read-only host rejection");
    }
}

#[test]
fn manifest_docs_icons_and_existing_direct_fixture_layout_exist() {
    let repository_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for manifest in &bundled_connector_registry().expect("registry").connectors {
        check_manifest_asset_paths(manifest).expect("safe manifest assets");
        assert!(
            repository_root
                .join("apps/desktop/src/assets/connectors")
                .join(&manifest.ui.icon)
                .is_file(),
            "missing icon for {}",
            manifest.id
        );
        assert!(
            repository_root
                .join("docs-site/connectors")
                .join(format!("{}.mdx", manifest.ui.docs_slug))
                .is_file(),
            "missing docs for {}",
            manifest.id
        );
    }

    check_fixture_layout(
        &repository_root.join("crates/locality-slack"),
        &FixtureLayout {
            version_directory: "direct-v1",
            required_files: &[
                "tree-paths.txt",
                "native-recent.json",
                "recent.md",
                "settings-default.json",
                "oauth-scopes.json",
            ],
        },
    )
    .expect("Slack direct fixture layout");
}

#[test]
fn connector_configuration_and_oauth_debug_output_redact_secrets() {
    let google_docs_config = GoogleDocsConfig::new("google-docs-secret-sentinel");
    check_debug_redaction(&google_docs_config, &["google-docs-secret-sentinel"])
        .expect("Google Docs config redaction");

    let credential = StoredGoogleDocsCredential {
        kind: "oauth".to_string(),
        connector: "google-docs".to_string(),
        access_token: "google-docs-access-secret".to_string(),
        token_type: Some("Bearer".to_string()),
        oauth_client_id: Some("public-client-id".to_string()),
        oauth_broker_url: Some("https://auth.locality.dev".to_string()),
        account_id: None,
        account_label: None,
        workspace_id: None,
        workspace_name: None,
        scopes: Vec::new(),
        refresh_token_handle: Some("google-docs-refresh-secret".to_string()),
        acquired_at: 0,
        expires_at: None,
    };
    check_debug_redaction(
        &credential,
        &["google-docs-access-secret", "google-docs-refresh-secret"],
    )
    .expect("Google Docs OAuth credential redaction");

    check_debug_redaction(
        &NotionConfig::default().with_token("notion-secret-sentinel"),
        &["notion-secret-sentinel"],
    )
    .expect("Notion config redaction");
    check_debug_redaction(
        &GoogleCalendarConfig::new("calendar-secret-sentinel"),
        &["calendar-secret-sentinel"],
    )
    .expect("Google Calendar config redaction");
    check_debug_redaction(
        &GmailConfig::new("gmail-secret-sentinel"),
        &["gmail-secret-sentinel"],
    )
    .expect("Gmail config redaction");
    check_debug_redaction(
        &GranolaConfig::new("granola-secret-sentinel"),
        &["granola-secret-sentinel"],
    )
    .expect("Granola config redaction");
    check_debug_redaction(
        &LinearConfig::new("linear-secret-sentinel"),
        &["linear-secret-sentinel"],
    )
    .expect("Linear config redaction");
    check_debug_redaction(
        &SlackConfig::new("slack-secret-sentinel"),
        &["slack-secret-sentinel"],
    )
    .expect("Slack config redaction");
}

fn runtime_entity_kind(kind: ManifestEntityKind) -> EntityKind {
    match kind {
        ManifestEntityKind::Page => EntityKind::Page,
        ManifestEntityKind::Database => EntityKind::Database,
        ManifestEntityKind::Directory => EntityKind::Directory,
    }
}
