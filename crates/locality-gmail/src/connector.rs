use std::fmt;

#[derive(Clone, PartialEq, Eq)]
pub struct GmailConfig {
    pub access_token: String,
}

impl fmt::Debug for GmailConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GmailConfig")
            .field("access_token", &"<redacted>")
            .finish()
    }
}

#[derive(Clone)]
pub struct GmailConnector {
    config: GmailConfig,
}

impl fmt::Debug for GmailConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GmailConnector")
            .field("config", &self.config)
            .finish()
    }
}

impl GmailConnector {
    pub fn new(config: GmailConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &GmailConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::{GmailConfig, GmailConnector};

    #[test]
    fn debug_redacts_connector_access_token() {
        let config = GmailConfig {
            access_token: "connector-access-token".to_string(),
        };
        let connector = GmailConnector::new(config.clone());

        let config_debug = format!("{config:?}");
        assert!(!config_debug.contains("connector-access-token"));
        assert!(config_debug.contains("<redacted>"));

        let connector_debug = format!("{connector:?}");
        assert!(!connector_debug.contains("connector-access-token"));
        assert!(connector_debug.contains("<redacted>"));
    }
}
