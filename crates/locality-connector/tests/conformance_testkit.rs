use std::fmt;
use std::path::Path;

use locality_connector::conformance::{
    FixtureLayout, check_debug_redaction, check_fixture_layout, is_safe_relative_path,
};

struct RedactedConfig {
    secret: String,
}

impl fmt::Debug for RedactedConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RedactedConfig")
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[test]
fn redaction_check_rejects_secret_bearing_debug_output() {
    let safe = RedactedConfig {
        secret: "connector-secret-sentinel".to_string(),
    };
    check_debug_redaction(&safe, &[&safe.secret]).expect("redacted debug");
    assert!(check_debug_redaction(&safe.secret, &[&safe.secret]).is_err());
}

#[test]
fn fixture_paths_are_portable_and_traversal_free() {
    assert!(is_safe_relative_path(Path::new("native/page.json")));
    assert!(!is_safe_relative_path(Path::new("../page.json")));
    assert!(!is_safe_relative_path(Path::new("/tmp/page.json")));

    let missing = check_fixture_layout(
        Path::new(env!("CARGO_MANIFEST_DIR")),
        &FixtureLayout {
            version_directory: "direct-v1",
            required_files: &["native-page.json"],
        },
    )
    .expect_err("missing layout must fail");
    assert!(missing.to_string().contains("fixture directory"));
}
