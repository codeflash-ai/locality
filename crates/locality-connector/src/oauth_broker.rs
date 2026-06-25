use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthBrokerStart {
    pub connector: String,
    pub redirect_uri: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthBrokerStartResponse {
    pub connector: String,
    pub client_id: String,
    pub authorization_url: String,
    pub redirect_uri: String,
    pub session: String,
    pub state: String,
    pub expires_in: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthBrokerCodeExchange {
    pub connector: String,
    pub session: String,
    pub state: String,
    pub code: String,
    pub redirect_uri: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthBrokerRefresh {
    pub connector: String,
    pub refresh_token_handle: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthBrokerToken {
    pub access_token: String,
    pub token_type: Option<String>,
    pub expires_in: Option<u64>,
    pub refresh_token_handle: Option<String>,
    pub account_id: Option<String>,
    pub account_label: Option<String>,
    pub workspace_id: Option<String>,
    pub workspace_name: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::{OAuthBrokerStart, OAuthBrokerToken};

    #[test]
    fn start_request_carries_connector_and_redirect_uri() {
        let request = OAuthBrokerStart {
            connector: "google-docs".to_string(),
            redirect_uri: "http://localhost:8757/oauth/google-docs/callback".to_string(),
        };

        let json = serde_json::to_value(&request).expect("serialize request");

        assert_eq!(json["connector"], "google-docs");
        assert_eq!(
            json["redirect_uri"],
            "http://localhost:8757/oauth/google-docs/callback"
        );
    }

    #[test]
    fn token_payload_can_carry_refresh_handle_and_scopes_without_refresh_token() {
        let payload = serde_json::json!({
            "access_token": "access",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token_handle": "handle-1",
            "account_id": "acct-1",
            "account_label": "user@example.com",
            "workspace_id": "google-drive",
            "workspace_name": "Google Drive",
            "scopes": ["openid", "https://www.googleapis.com/auth/drive"]
        });

        let token: OAuthBrokerToken = serde_json::from_value(payload).expect("decode token");

        assert_eq!(token.access_token, "access");
        assert_eq!(token.refresh_token_handle.as_deref(), Some("handle-1"));
        assert_eq!(token.account_label.as_deref(), Some("user@example.com"));
        assert_eq!(token.scopes.len(), 2);
    }
}
