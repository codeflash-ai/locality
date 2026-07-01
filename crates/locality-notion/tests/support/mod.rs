use std::path::{Path, PathBuf};

use locality_notion::NotionConfig;
use serde_json::Value;

const TOKEN_ENV: &str = "NOTION_TOKEN";
const LEGACY_TOKEN_ENV: &str = "NOTION_AT";
const LIVE_CONNECTION_ENV: &str = "LOCALITY_NOTION_LIVE_CONNECTION_ID";
const LIVE_CREDENTIAL_STATE_DIR_ENV: &str = "LOCALITY_NOTION_LIVE_CREDENTIAL_STATE_DIR";

pub fn live_notion_config() -> NotionConfig {
    NotionConfig::default().with_token(live_notion_token())
}

pub fn live_notion_token() -> String {
    for env_key in [TOKEN_ENV, LEGACY_TOKEN_ENV] {
        if let Ok(token) = std::env::var(env_key) {
            let token = token.trim();
            if !token.is_empty() {
                return token.to_string();
            }
        }
    }

    let connection_id =
        std::env::var(LIVE_CONNECTION_ENV).unwrap_or_else(|_| "notion-default".to_string());
    let state_root = live_credential_state_root();
    live_notion_token_from_state_root(&state_root, &connection_id).unwrap_or_else(|error| {
        panic!(
            "set {TOKEN_ENV}/{LEGACY_TOKEN_ENV} or store a Notion credential for `{connection_id}` in `{}` ({error})",
            state_root.join("credentials").display()
        )
    })
}

fn live_notion_token_from_state_root(
    state_root: &Path,
    connection_id: &str,
) -> Result<String, String> {
    let secret_ref = format!("connection:{connection_id}");
    let secret_hex = secret_ref
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let secret_path = state_root.join("credentials").join(secret_hex);
    let secret = std::fs::read_to_string(&secret_path)
        .map_err(|error| format!("failed to read `{}`: {error}", secret_path.display()))?;
    notion_access_token_from_secret(&secret)
}

fn live_credential_state_root() -> PathBuf {
    if let Ok(path) = std::env::var(LIVE_CREDENTIAL_STATE_DIR_ENV) {
        let path = path.trim();
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }

    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .expect("HOME or USERPROFILE is required to find ~/.loc credentials");
    PathBuf::from(home).join(".loc")
}

fn notion_access_token_from_secret(secret: &str) -> Result<String, String> {
    let secret = secret.trim();
    if secret.is_empty() {
        return Err("credential secret is empty".to_string());
    }
    if secret.starts_with('{') {
        let parsed: Value = serde_json::from_str(secret)
            .map_err(|error| format!("stored Notion credential is invalid JSON: {error}"))?;
        let token = parsed
            .get("access_token")
            .or_else(|| parsed.get("token"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if token.is_empty() {
            return Err("stored Notion credential has an empty access token".to_string());
        }
        return Ok(token.to_string());
    }
    Ok(secret.to_string())
}
