pub mod client;
pub mod connector;
pub mod dto;
pub mod oauth;
pub mod render;
pub mod settings;

pub use connector::{GmailConfig, GmailConnector};
pub use oauth::{
    DEFAULT_GMAIL_OAUTH_BROKER_URL, DEFAULT_GMAIL_OAUTH_REDIRECT_URI, GMAIL_CONNECTOR_ID,
    GMAIL_FULL_MAILBOX_SCOPE, GMAIL_OAUTH_SCOPES, GmailOAuthScopeError, HttpGmailOAuthBrokerClient,
    StoredGmailCredential, gmail_capabilities_json, validate_gmail_oauth_scopes,
};
pub use settings::{
    GmailDateWindow, GmailMountSettings, GmailProjectionView, GmailSearchDate, GmailSettings,
};
