use afs_cli::connect::{
    ConnectOptions, DEFAULT_NOTION_PROFILE_ID, NotionConnectionProbe, NotionConnectionProbeResult,
    run_connect_notion, run_disconnect, run_profiles,
};
use afs_store::{
    ConnectionId, ConnectionRepository, ConnectorProfileId, ConnectorProfileRepository,
    CredentialStore, InMemoryCredentialStore, InMemoryStateStore,
};

#[test]
fn connect_notion_stores_metadata_and_secret_separately() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let probe = FakeProbe;

    let report = run_connect_notion(
        &mut store,
        &credentials,
        ConnectOptions {
            connection_id: Some(ConnectionId::new("work")),
            token: "ntn_secret_test_token".to_string(),
        },
        &probe,
    )
    .expect("connect");

    assert_eq!(report.connection_id, "work");
    assert_eq!(report.profile_id, DEFAULT_NOTION_PROFILE_ID);
    assert_eq!(report.workspace_name.as_deref(), Some("AgentFS"));
    assert_eq!(
        credentials
            .get("connection:work")
            .expect("credential saved"),
        "ntn_secret_test_token"
    );

    let connection = store
        .get_connection(&ConnectionId::new("work"))
        .expect("get connection")
        .expect("connection");
    assert_eq!(connection.secret_ref, "connection:work");
    assert_eq!(
        connection.profile_id,
        Some(ConnectorProfileId::new(DEFAULT_NOTION_PROFILE_ID))
    );
    assert_eq!(connection.status, "active");
    let profile = store
        .get_connector_profile(&ConnectorProfileId::new(DEFAULT_NOTION_PROFILE_ID))
        .expect("get profile")
        .expect("profile");
    assert_eq!(profile.connector, "notion");
    assert_eq!(profile.auth_kind, "token");

    let json = serde_json::to_string(&report).expect("json");
    assert!(!json.contains("ntn_secret_test_token"));
    assert!(!json.contains("secret_ref"));
}

#[test]
fn profiles_list_auth_configs_without_secrets() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let probe = FakeProbe;
    run_connect_notion(
        &mut store,
        &credentials,
        ConnectOptions {
            connection_id: Some(ConnectionId::new("work")),
            token: "ntn_secret_test_token".to_string(),
        },
        &probe,
    )
    .expect("connect");

    let report = run_profiles(&store).expect("profiles");

    assert_eq!(report.profiles.len(), 1);
    assert_eq!(report.profiles[0].profile_id, DEFAULT_NOTION_PROFILE_ID);
    assert_eq!(report.profiles[0].connector_version, "notion.v1");
    let json = serde_json::to_string(&report).expect("json");
    assert!(!json.contains("ntn_secret_test_token"));
    assert!(!json.contains("secret_ref"));
}

#[test]
fn disconnect_revokes_connection_and_deletes_credential() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let probe = FakeProbe;
    run_connect_notion(
        &mut store,
        &credentials,
        ConnectOptions {
            connection_id: Some(ConnectionId::new("work")),
            token: "ntn_secret_test_token".to_string(),
        },
        &probe,
    )
    .expect("connect");

    let report =
        run_disconnect(&mut store, &credentials, ConnectionId::new("work")).expect("disconnect");

    assert_eq!(report.status, "revoked");
    assert!(credentials.get("connection:work").is_err());
    assert_eq!(
        store
            .get_connection(&ConnectionId::new("work"))
            .expect("get connection")
            .expect("connection")
            .status,
        "revoked"
    );
}

#[derive(Clone, Debug)]
struct FakeProbe;

impl NotionConnectionProbe for FakeProbe {
    fn probe(
        &self,
        token: &str,
    ) -> Result<NotionConnectionProbeResult, afs_cli::connect::ConnectError> {
        assert_eq!(token, "ntn_secret_test_token");
        Ok(NotionConnectionProbeResult {
            account_label: Some("agent@example.com".to_string()),
            workspace_id: Some("workspace-1".to_string()),
            workspace_name: Some("AgentFS".to_string()),
        })
    }
}
