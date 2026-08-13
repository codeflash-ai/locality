use locality_connector::ConnectorExecutionPolicy;
use locality_granola::{GranolaConfig, GranolaConnector};

#[test]
fn granola_config_struct_literal_remains_source_compatible() {
    let config = GranolaConfig {
        api_key: "secret".to_string(),
        updated_after: Some("2026-07-12T00:00:00Z".to_string()),
        execution_policy: ConnectorExecutionPolicy::Inline,
    };

    let connector = GranolaConnector::new(config);

    assert_eq!(connector.config().api_key, "secret");
    assert_eq!(
        connector.config().updated_after.as_deref(),
        Some("2026-07-12T00:00:00Z")
    );
    assert_eq!(
        connector.execution_policy(),
        ConnectorExecutionPolicy::Inline
    );
}

#[test]
fn granola_connector_can_defer_provider_cooldown_with_config_builder() {
    let connector = GranolaConnector::new(
        GranolaConfig {
            api_key: "secret".to_string(),
            updated_after: None,
            execution_policy: ConnectorExecutionPolicy::Inline,
        }
        .with_execution_policy(ConnectorExecutionPolicy::DeferProviderCooldown),
    );

    assert_eq!(
        connector.execution_policy(),
        ConnectorExecutionPolicy::DeferProviderCooldown
    );
}
