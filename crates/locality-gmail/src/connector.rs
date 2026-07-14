#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GmailConfig {
    pub access_token: String,
}

#[derive(Clone, Debug)]
pub struct GmailConnector {
    config: GmailConfig,
}

impl GmailConnector {
    pub fn new(config: GmailConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &GmailConfig {
        &self.config
    }
}
