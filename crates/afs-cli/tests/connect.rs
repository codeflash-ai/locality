use afs_cli::connect::{
    ConnectOptions, NotionConnectionProbe, NotionConnectionProbeResult, run_connect_notion,
    run_disconnect,
};
use afs_store::{
    ConnectionId, ConnectionRepository, CredentialStore, InMemoryCredentialStore,
    InMemoryStateStore,
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
    assert_eq!(connection.status, "active");

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
