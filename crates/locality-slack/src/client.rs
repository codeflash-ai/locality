pub const DEFAULT_SLACK_API_BASE_URL: &str = "https://slack.com/api";

pub trait SlackApi {}

#[derive(Clone, Debug, Default)]
pub struct HttpSlackApiClient;

impl SlackApi for HttpSlackApiClient {}
