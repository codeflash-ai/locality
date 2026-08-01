use locality_connector::manifest::{
    CONNECTOR_REGISTRY_JSON, CONNECTOR_REGISTRY_SCHEMA_JSON, ConnectorRegistry, ManifestError,
    MembershipOperation, bundled_connector_registry,
};
use serde_json::{Value, json};

fn registry_value() -> Value {
    serde_json::from_str(CONNECTOR_REGISTRY_JSON).expect("registry JSON")
}

fn validation_messages(error: ManifestError) -> String {
    match error {
        ManifestError::Json(message) => message,
        ManifestError::Validation(violations) => violations
            .into_iter()
            .map(|violation| format!("{}: {}", violation.path, violation.message))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

#[test]
fn bundled_registry_is_strict_valid_v1() {
    let registry = bundled_connector_registry().expect("bundled registry");
    assert_eq!(registry.schema_version, 1);
    assert_eq!(
        registry
            .connectors
            .iter()
            .map(|connector| connector.id.as_str())
            .collect::<Vec<_>>(),
        [
            "notion",
            "google-docs",
            "google-calendar",
            "gmail",
            "granola",
            "linear",
            "slack",
        ]
    );
}

#[test]
fn registry_matches_the_published_json_schema() {
    let schema = serde_json::from_str::<Value>(CONNECTOR_REGISTRY_SCHEMA_JSON)
        .expect("registry schema JSON");
    let instance = registry_value();
    let validator = jsonschema::validator_for(&schema).expect("compile registry schema");
    let errors = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(errors.is_empty(), "schema errors: {errors:#?}");
}

#[test]
fn every_default_mount_setting_matches_its_connector_schema() {
    for connector in &bundled_connector_registry().expect("registry").connectors {
        let validator = jsonschema::validator_for(&connector.mount.settings_schema)
            .unwrap_or_else(|error| panic!("{} settings schema: {error}", connector.id));
        let errors = validator
            .iter_errors(&connector.mount.default_settings)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        assert!(
            errors.is_empty(),
            "{} default settings do not match schema: {errors:#?}",
            connector.id
        );
    }
}

#[test]
fn slack_membership_mutation_is_separate_from_content_push_operations() {
    let registry = bundled_connector_registry().expect("registry");
    let slack = registry.connector("slack").expect("Slack manifest");

    assert_eq!(
        slack.membership_operations,
        [MembershipOperation::JoinPublicChannels]
    );
    assert!(slack.mount.read_only);
    assert!(slack.push_operations.is_empty());
    assert!(!slack.capabilities.supports_block_updates);
    assert!(!slack.capabilities.supports_entity_body_updates);
}

#[test]
fn strict_parser_rejects_unknown_fields_and_enums() {
    let mut unknown_field = registry_value();
    unknown_field["connectors"][0]["executable"] = json!("/tmp/plugin");
    assert!(
        ConnectorRegistry::parse(&unknown_field.to_string())
            .expect_err("unknown field")
            .to_string()
            .contains("unknown field")
    );

    let mut unknown_enum = registry_value();
    unknown_enum["connectors"][0]["profiles"][0]["auth_kind"] = json!("shell");
    assert!(
        ConnectorRegistry::parse(&unknown_enum.to_string())
            .expect_err("unknown enum")
            .to_string()
            .contains("unknown variant")
    );
}

#[test]
fn validation_rejects_duplicate_defaults_and_missing_default_profile() {
    let mut duplicate = registry_value();
    duplicate["connectors"][1]["default_connection_id"] = json!("notion-default");
    duplicate["connectors"][1]["mount"]["default_id"] = json!("notion-main");
    duplicate["connectors"][1]["default_profile_id"] = json!("missing-profile");

    let messages = validation_messages(
        ConnectorRegistry::parse(&duplicate.to_string()).expect_err("duplicates must fail"),
    );
    assert!(messages.contains("duplicate default connection id"));
    assert!(messages.contains("duplicate default mount id"));
    assert!(messages.contains("must name exactly one profile"));
}

#[test]
fn validation_rejects_unsafe_assets_credentials_and_inconsistent_capabilities() {
    let mut invalid = registry_value();
    invalid["connectors"][0]["ui"]["icon"] = json!("../notion.svg");
    invalid["connectors"][0]["ui"]["docs_slug"] = json!("https://example.test");
    invalid["connectors"][0]["mount"]["default_settings"] =
        json!({"access_token": "must-never-live-here"});
    invalid["connectors"][0]["capabilities"]["supports_block_updates"] = json!(false);

    let messages = validation_messages(
        ConnectorRegistry::parse(&invalid.to_string()).expect_err("invalid contract must fail"),
    );
    assert!(messages.contains("safe relative kebab-case .svg filename"));
    assert!(messages.contains("safe kebab-case identifier"));
    assert!(messages.contains("cannot contain credential-bearing settings"));
    assert!(messages.contains("must agree with declared block push operations"));
}

#[test]
fn public_channel_membership_mutation_requires_its_oauth_scope() {
    let mut invalid = registry_value();
    let scopes = invalid["connectors"][6]["profiles"][0]["scopes"]
        .as_array_mut()
        .expect("Slack scopes");
    scopes.retain(|scope| scope != "channels:join");

    let messages = validation_messages(
        ConnectorRegistry::parse(&invalid.to_string()).expect_err("missing scope must fail"),
    );
    assert!(messages.contains("join_public_channels requires a profile with channels:join scope"));
}

#[test]
fn debug_omits_settings_and_scope_values() {
    let registry = bundled_connector_registry().expect("registry");
    let gmail = registry.connector("gmail").expect("gmail manifest");
    let debug = format!("{gmail:?}");

    assert!(debug.contains("<descriptive settings>"));
    assert!(debug.contains("<5 descriptive scopes>"));
    assert!(!debug.contains("gmail.compose"));
}

#[test]
fn sensitive_setting_keys_are_rejected_across_common_naming_styles() {
    for key in [
        "accessToken",
        "refresh-token",
        "client_secret",
        "private_key",
        "privateKeyPem",
        "apiKey",
        "APIKey",
        "bearer",
        "bearerToken",
        "authorizationHeader",
        "credentials",
        "apikey",
        "PRIVATEKEY",
        "accesstoken",
        "clientsecret",
        "bearertoken",
        "accesstokenvalue",
        "ACCESSTOKENVALUE",
        "apikeyid",
        "privatekeypath",
        "clientsecrethandle",
        "bearertokenreference",
        "access_token_value",
        "AccessTokenValue",
    ] {
        let mut invalid = registry_value();
        invalid["connectors"][0]["mount"]["default_settings"] = json!({key: "sentinel"});
        let error = ConnectorRegistry::parse(&invalid.to_string())
            .expect_err(&format!("sensitive key `{key}` was accepted"));
        let messages = validation_messages(error);
        assert!(
            messages.contains("cannot contain credential-bearing settings"),
            "sensitive key `{key}` was not classified: {messages}"
        );
    }

    for key in [
        "monkey",
        "hockey",
        "keynote",
        "keyboard_layout",
        "tokenizer",
        "secretary",
        "secretariat",
        "bearberry",
        "accessibility",
        "private_mode",
        "api_latency",
        "api_keyboard_layout",
        "private_keynote",
        "privatekeynote",
        "PrivateKeynote",
        "apikeyboard",
        "accesstokenizer",
        "clientsecretary",
        "serviceapikey",
        "sessiontoken",
        "databasepassword",
    ] {
        let mut valid = registry_value();
        valid["connectors"][0]["mount"]["default_settings"] = json!({key: true});
        ConnectorRegistry::parse(&valid.to_string())
            .unwrap_or_else(|error| panic!("safe key `{key}` was rejected: {error}"));
    }
}
