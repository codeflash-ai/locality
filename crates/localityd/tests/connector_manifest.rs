use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::Duration;

use locality_connector::Connector;
use locality_connector::conformance::{
    DirectFixtureAuth, check_capability_operation_agreement, check_debug_redaction,
    check_direct_fixture_layout, check_manifest_asset_paths, check_manifest_identity,
    check_read_only_rejection,
};
use locality_connector::manifest::{
    AuthKind, BodyDiffMode as ManifestBodyDiffMode, ManifestEntityKind,
    VirtualRenamePolicy as ManifestRenamePolicy, bundled_connector_registry,
};
use locality_core::model::{EntityKind, MountId};
use locality_core::push::BodyDiffMode;
use locality_gmail::{GMAIL_OAUTH_SCOPES, GmailConfig, GmailConnector, GmailMountSettings};
use locality_google_calendar::{
    GOOGLE_CALENDAR_OAUTH_SCOPES, GoogleCalendarConfig, GoogleCalendarConnector,
    GoogleCalendarMountSettings,
};
use locality_google_docs::{
    GOOGLE_DOCS_OAUTH_SCOPES, GoogleDocsConfig, GoogleDocsConnector, StoredGoogleDocsCredential,
};
use locality_granola::{GranolaConfig, GranolaConnector};
use locality_linear::{LinearConfig, LinearConnector};
use locality_notion::{NotionConfig, NotionConnector};
use locality_slack::{SlackConfig, SlackConnector, SlackMountSettings};
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
                    .with_document_ids(vec!["selected-doc".to_string()]),
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
fn google_docs_oauth_verification_scope_manifest_matches_runtime_profile() {
    assert!(
        !GOOGLE_DOCS_OAUTH_SCOPES
            .contains(&"https://www.googleapis.com/auth/drive.metadata.readonly"),
        "Google Docs must not request readonly Drive metadata"
    );
    assert!(
        !GOOGLE_DOCS_OAUTH_SCOPES.contains(&"https://www.googleapis.com/auth/drive.metadata"),
        "Google Docs must not request writable Drive metadata beyond drive.file app-file writes"
    );

    let repository_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let verification_path = repository_root.join("connectors/oauth-verification/google-docs.json");
    let verification_json = fs::read_to_string(&verification_path)
        .expect("read Google Docs OAuth verification manifest");
    let verification = serde_json::from_str::<serde_json::Value>(&verification_json)
        .expect("parse Google Docs OAuth verification manifest");

    assert_eq!(verification["connector"], "google-docs");
    let registry = bundled_connector_registry().expect("manifest registry");
    let docs_profile = registry
        .connector("google-docs")
        .expect("Google Docs manifest")
        .profiles
        .iter()
        .find(|profile| profile.id == "google-docs-oauth-default")
        .expect("Google Docs OAuth manifest profile");
    assert_eq!(
        docs_profile.scopes,
        GOOGLE_DOCS_OAUTH_SCOPES
            .iter()
            .map(|scope| scope.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        verification["runtime_oauth_request_scopes"],
        serde_json::json!(GOOGLE_DOCS_OAUTH_SCOPES)
    );

    let docs_api_scopes = GOOGLE_DOCS_OAUTH_SCOPES
        .iter()
        .copied()
        .filter(|scope| scope.starts_with("https://www.googleapis.com/auth/"))
        .collect::<Vec<_>>();
    assert_eq!(
        verification["google_api_verification_scopes"],
        serde_json::json!(docs_api_scopes)
    );

    let submitted = verification["google_api_verification_scopes"]
        .as_array()
        .expect("submitted Google Docs verification scopes array");
    let excluded = verification["excluded_google_api_scopes"]
        .as_array()
        .expect("excluded Google Docs verification scopes array");
    for scope in [
        "https://www.googleapis.com/auth/drive",
        "https://www.googleapis.com/auth/drive.readonly",
        "https://www.googleapis.com/auth/drive.metadata",
        "https://www.googleapis.com/auth/drive.metadata.readonly",
        "https://www.googleapis.com/auth/documents.readonly",
    ] {
        assert!(
            !GOOGLE_DOCS_OAUTH_SCOPES.contains(&scope),
            "runtime Google Docs OAuth profile must not request broad or insufficient scope `{scope}`"
        );
        assert!(
            !submitted.iter().any(|value| value == scope),
            "verification manifest must not submit broad or insufficient scope `{scope}`"
        );
        assert!(
            excluded.iter().any(|value| value == scope),
            "verification manifest should document excluded scope `{scope}`"
        );
    }
}

#[test]
fn google_calendar_oauth_verification_scope_manifest_matches_runtime_profile() {
    assert!(
        GOOGLE_CALENDAR_OAUTH_SCOPES
            .contains(&"https://www.googleapis.com/auth/calendar.events.owned"),
        "Google Calendar should request event access only for calendars the user owns"
    );
    assert!(
        !GOOGLE_CALENDAR_OAUTH_SCOPES.contains(&"https://www.googleapis.com/auth/calendar.events"),
        "Google Calendar must not request all-calendar event access for the primary-calendar-only connector"
    );

    let repository_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let verification_path =
        repository_root.join("connectors/oauth-verification/google-calendar.json");
    let verification_json = fs::read_to_string(&verification_path)
        .expect("read Google Calendar OAuth verification manifest");
    let verification = serde_json::from_str::<serde_json::Value>(&verification_json)
        .expect("parse Google Calendar OAuth verification manifest");

    assert_eq!(verification["connector"], "google-calendar");
    let registry = bundled_connector_registry().expect("manifest registry");
    let calendar_profile = registry
        .connector("google-calendar")
        .expect("Google Calendar manifest")
        .profiles
        .iter()
        .find(|profile| profile.id == "google-calendar-oauth-default")
        .expect("Google Calendar OAuth manifest profile");
    assert_eq!(
        calendar_profile.scopes,
        GOOGLE_CALENDAR_OAUTH_SCOPES
            .iter()
            .map(|scope| scope.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        verification["runtime_oauth_request_scopes"],
        serde_json::json!(GOOGLE_CALENDAR_OAUTH_SCOPES)
    );

    let calendar_api_scopes = GOOGLE_CALENDAR_OAUTH_SCOPES
        .iter()
        .copied()
        .filter(|scope| scope.starts_with("https://www.googleapis.com/auth/calendar."))
        .collect::<Vec<_>>();
    assert_eq!(
        verification["google_api_verification_scopes"],
        serde_json::json!(calendar_api_scopes)
    );

    let submitted = verification["google_api_verification_scopes"]
        .as_array()
        .expect("submitted Google Calendar verification scopes array");
    let excluded = verification["excluded_google_api_scopes"]
        .as_array()
        .expect("excluded Google Calendar verification scopes array");
    for scope in [
        "https://www.googleapis.com/auth/calendar",
        "https://www.googleapis.com/auth/calendar.readonly",
        "https://www.googleapis.com/auth/calendar.events",
        "https://www.googleapis.com/auth/calendar.events.readonly",
        "https://www.googleapis.com/auth/calendar.app.created",
    ] {
        assert!(
            !GOOGLE_CALENDAR_OAUTH_SCOPES.contains(&scope),
            "runtime Google Calendar OAuth profile must not request broad or insufficient scope `{scope}`"
        );
        assert!(
            !submitted.iter().any(|value| value == scope),
            "verification manifest must not submit broad or insufficient scope `{scope}`"
        );
        assert!(
            excluded.iter().any(|value| value == scope),
            "verification manifest should document excluded scope `{scope}`"
        );
    }
}

#[test]
fn gmail_oauth_verification_scope_manifest_matches_runtime_profile() {
    let repository_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let verification_path = repository_root.join("connectors/oauth-verification/gmail.json");
    let verification_json =
        fs::read_to_string(&verification_path).expect("read Gmail OAuth verification manifest");
    let verification = serde_json::from_str::<serde_json::Value>(&verification_json)
        .expect("parse Gmail OAuth verification manifest");

    assert_eq!(verification["connector"], "gmail");
    let registry = bundled_connector_registry().expect("manifest registry");
    let gmail_profile = registry
        .connector("gmail")
        .expect("Gmail manifest")
        .profiles
        .iter()
        .find(|profile| profile.id == "gmail-oauth-default")
        .expect("Gmail OAuth manifest profile");
    assert_eq!(
        gmail_profile.scopes,
        GMAIL_OAUTH_SCOPES
            .iter()
            .map(|scope| scope.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        verification["runtime_oauth_request_scopes"],
        serde_json::json!(GMAIL_OAUTH_SCOPES)
    );

    let gmail_api_scopes = GMAIL_OAUTH_SCOPES
        .iter()
        .copied()
        .filter(|scope| scope.starts_with("https://www.googleapis.com/auth/gmail."))
        .collect::<Vec<_>>();
    assert_eq!(
        verification["google_api_verification_scopes"],
        serde_json::json!(gmail_api_scopes)
    );

    let submitted = verification["google_api_verification_scopes"]
        .as_array()
        .expect("submitted Gmail verification scopes array");
    let excluded = verification["excluded_google_api_scopes"]
        .as_array()
        .expect("excluded Gmail verification scopes array");
    for scope in [
        "https://mail.google.com/",
        "https://www.googleapis.com/auth/gmail.modify",
        "https://www.googleapis.com/auth/gmail.drafts.create",
        "https://www.googleapis.com/auth/gmail.drafts.readonly",
        "https://www.googleapis.com/auth/gmail.metadata",
        "https://www.googleapis.com/auth/gmail.insert",
        "https://www.googleapis.com/auth/gmail.addons.current.message.metadata",
        "https://www.googleapis.com/auth/gmail.addons.current.message.readonly",
        "https://www.googleapis.com/auth/gmail.send",
    ] {
        assert!(
            !GMAIL_OAUTH_SCOPES.contains(&scope),
            "runtime Gmail OAuth profile must not request broad or unused scope `{scope}`"
        );
        assert!(
            !submitted.iter().any(|value| value == scope),
            "verification manifest must not submit broad or unused scope `{scope}`"
        );
        assert!(
            excluded.iter().any(|value| value == scope),
            "verification manifest should document excluded scope `{scope}`"
        );
    }
}

#[test]
fn representative_runtime_settings_round_trip_through_manifest_schemas() {
    assert_settings_schema_runtime_round_trip(
        "google-calendar",
        &[
            r#"{}"#,
            r#"{"google_calendar":{"date_window":{"after":"2026-07-01","before":"2026-07-31"}}}"#,
        ],
        |json| GoogleCalendarMountSettings::from_json(json)?.to_json(),
    );
    assert_settings_schema_runtime_round_trip(
        "gmail",
        &[
            r#"{}"#,
            r#"{"gmail":{"date_window":{"after":"2026-07-01","before":"2026-07-31"},"view":"threads"}}"#,
        ],
        |json| GmailMountSettings::from_json(json)?.to_json(),
    );
    assert_settings_schema_runtime_round_trip(
        "slack",
        &[
            r#"{"slack":{"history_limit":15,"types":["public_channel","private_channel","im","mpim"],"auto_join_public_channels":true}}"#,
            r#"{"slack":{"history_limit":7,"types":["public_channel","im"],"auto_join_public_channels":false}}"#,
        ],
        |json| SlackMountSettings::from_json(json)?.to_json(),
    );
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

    let grandfathered_without_direct_v1 = [
        "notion",
        "google-docs",
        "google-calendar",
        "gmail",
        "granola",
        "linear",
    ];
    let mut missing_direct_v1 = Vec::new();
    for manifest in &bundled_connector_registry().expect("registry").connectors {
        let crate_root = repository_root.join(&manifest.crate_path);
        if !crate_root.join("fixtures/direct-v1").is_dir() {
            missing_direct_v1.push(manifest.id.as_str());
            continue;
        }
        let default_profile = manifest
            .profiles
            .iter()
            .find(|profile| profile.id == manifest.default_profile_id)
            .expect("default profile");
        let auth = match default_profile.auth_kind {
            AuthKind::Oauth => DirectFixtureAuth::Oauth,
            AuthKind::Token => DirectFixtureAuth::Token,
            AuthKind::ApiKey => DirectFixtureAuth::ApiKey,
        };
        check_direct_fixture_layout(&crate_root, "direct-v1", auth)
            .unwrap_or_else(|error| panic!("{} direct-v1: {error}", manifest.id));
    }
    assert_eq!(missing_direct_v1, grandfathered_without_direct_v1);
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

fn assert_settings_schema_runtime_round_trip(
    connector_id: &str,
    examples: &[&str],
    runtime_round_trip: impl Fn(&str) -> locality_core::LocalityResult<String>,
) {
    let registry = bundled_connector_registry().expect("manifest registry");
    let manifest = registry
        .connector(connector_id)
        .expect("connector manifest");
    let validator = jsonschema::validator_for(&manifest.mount.settings_schema)
        .unwrap_or_else(|error| panic!("{connector_id} settings schema: {error}"));

    for example in examples {
        let input = serde_json::from_str::<serde_json::Value>(example).expect("settings JSON");
        let input_errors = validator
            .iter_errors(&input)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        assert!(
            input_errors.is_empty(),
            "{connector_id} representative input violates schema: {input_errors:?}"
        );

        let encoded = runtime_round_trip(example).unwrap_or_else(|error| {
            panic!("{connector_id} runtime rejected schema input: {error}")
        });
        let runtime_value =
            serde_json::from_str::<serde_json::Value>(&encoded).expect("runtime settings JSON");
        let output_errors = validator
            .iter_errors(&runtime_value)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        assert!(
            output_errors.is_empty(),
            "{connector_id} runtime output violates schema: {output_errors:?}"
        );

        let encoded_again = runtime_round_trip(&encoded)
            .unwrap_or_else(|error| panic!("{connector_id} runtime rejected its output: {error}"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&encoded_again).expect("second output"),
            runtime_value,
            "{connector_id} settings normalization is not stable"
        );
    }
}
