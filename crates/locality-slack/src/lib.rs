//! Slack connector.

pub mod client;
pub mod connector;
pub mod dto;
pub mod oauth;
pub mod render;
pub mod settings;

pub use connector::{SlackConfig, SlackConnector};
pub use oauth::{
    DEFAULT_SLACK_OAUTH_BROKER_URL, DEFAULT_SLACK_OAUTH_REDIRECT_URI, HttpSlackOAuthBrokerClient,
    SLACK_CONNECTOR_ID, SLACK_OAUTH_SCOPES, SlackOAuthCredentialError, SlackOAuthScopeError,
    StoredSlackCredential, slack_capabilities_json, validate_slack_oauth_refresh_path,
    validate_slack_oauth_scopes,
};
pub use render::{SlackContentKind, SlackNativeBundle, render_slack_document};
pub use settings::SlackMountSettings;
