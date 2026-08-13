use locality_connector::{Connector, ConnectorExecutionPolicy};
use locality_linear::{LinearConfig, LinearConnector};

#[test]
fn linear_config_struct_literal_remains_source_compatible() {
    let config = LinearConfig {
        token: "secret".to_string(),
    };

    let connector = LinearConnector::new(config);

    assert_eq!(connector.config().token, "secret");
    assert_eq!(
        connector.execution_policy(),
        ConnectorExecutionPolicy::Inline
    );
}

#[test]
fn linear_connector_can_defer_provider_cooldown_without_config_field() {
    let connector = LinearConnector::new(LinearConfig {
        token: "secret".to_string(),
    })
    .with_execution_policy(ConnectorExecutionPolicy::DeferProviderCooldown);

    assert_eq!(
        connector.execution_policy(),
        ConnectorExecutionPolicy::DeferProviderCooldown
    );
}
