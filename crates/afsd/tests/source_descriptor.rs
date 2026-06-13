use afs_notion::client::DEFAULT_NOTION_TOKEN_ENV;
use afsd::source::{source_descriptor, source_display_name};

#[test]
fn notion_descriptor_exposes_cli_and_mount_metadata() {
    let descriptor = source_descriptor("notion");

    assert_eq!(descriptor.id(), "notion");
    assert_eq!(descriptor.display_name(), "Notion");
    assert_eq!(descriptor.default_mount_id(), "notion-main");
    assert_eq!(descriptor.connect_command(), Some("afs connect notion"));
    assert_eq!(descriptor.auth_env_var(), Some(DEFAULT_NOTION_TOKEN_ENV));
    assert!(descriptor.supports_oauth());
    assert!(descriptor.mount_guidance().contains("Notion facts:"));
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
            .contains("# AgentFS linear Mount")
    );
    assert!(descriptor.mount_guidance().contains("back to linear"));
}

#[test]
fn source_display_name_uses_descriptor_registry() {
    assert_eq!(source_display_name("notion"), "Notion");
    assert_eq!(source_display_name("linear"), "Linear");
    assert_eq!(source_display_name("custom"), "custom");
}
