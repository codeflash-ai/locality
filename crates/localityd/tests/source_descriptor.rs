use locality_core::model::MountId;
use locality_notion::client::DEFAULT_NOTION_TOKEN_ENV;
use locality_store::{InMemoryCredentialStore, InMemoryStateStore, MountConfig};
use localityd::source::{
    resolve_source_for_mount, source_descriptor, source_display_name, supported_source_connectors,
};

#[test]
fn notion_descriptor_exposes_cli_and_mount_metadata() {
    let descriptor = source_descriptor("notion");

    assert_eq!(descriptor.id(), "notion");
    assert_eq!(descriptor.display_name(), "Notion");
    assert_eq!(descriptor.default_mount_id(), "notion-main");
    assert_eq!(descriptor.connect_command(), Some("loc connect notion"));
    assert_eq!(descriptor.auth_env_var(), Some(DEFAULT_NOTION_TOKEN_ENV));
    assert!(descriptor.supports_oauth());
    assert!(descriptor.mount_guidance().contains("Notion facts:"));
}

#[test]
fn google_docs_descriptor_comes_from_registry() {
    let descriptor = source_descriptor("google-docs");

    assert_eq!(descriptor.id(), "google-docs");
    assert_eq!(descriptor.display_name(), "Google Docs");
    assert_eq!(descriptor.default_mount_id(), "google-docs-main");
    assert_eq!(descriptor.connect_command(), Some("loc connect google-docs"));
    assert_eq!(descriptor.auth_env_var(), None);
    assert!(descriptor.supports_oauth());
    assert!(
        descriptor
            .mount_guidance()
            .contains("# Locality Google Docs Mount")
    );
}

#[test]
fn generic_descriptor_preserves_source_id_in_guidance() {
    let descriptor = source_descriptor("linear");

    assert_eq!(descriptor.id(), "linear");
    assert_eq!(descriptor.display_name(), "Linear");
    assert_eq!(descriptor.default_mount_id(), "linear-main");
    assert_eq!(descriptor.connect_command(), None);
    assert_eq!(descriptor.auth_env_var(), None);
    assert!(!descriptor.supports_oauth());
    assert!(
        descriptor
            .mount_guidance()
            .contains("# Locality linear Mount")
    );
    assert!(descriptor.mount_guidance().contains("to linear"));
}

#[test]
fn source_display_name_uses_descriptor_registry() {
    assert_eq!(source_display_name("notion"), "Notion");
    assert_eq!(source_display_name("google-docs"), "Google Docs");
    assert_eq!(source_display_name("linear"), "Linear");
    assert_eq!(source_display_name("custom"), "custom");
}

#[test]
fn supported_source_connectors_lists_runtime_registered_connectors() {
    assert_eq!(supported_source_connectors(), vec!["notion", "google-docs"]);
}

#[test]
fn resolving_unregistered_connector_reports_unsupported_connector() {
    let store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let mount = MountConfig::new(
        MountId::new("custom-main"),
        "custom",
        "/tmp/locality/custom",
    );

    let error = resolve_source_for_mount(&store, &credentials, &mount).expect_err("unsupported");

    assert_eq!(error.code(), "unsupported_connector");
    assert_eq!(
        error.message(),
        "connector `custom` is not supported by this build"
    );
}
