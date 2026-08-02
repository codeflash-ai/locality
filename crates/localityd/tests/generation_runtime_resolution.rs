use locality_protocol::workspace_api_v2::{
    WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON, WorkspaceProfileSessionV2,
};
use locality_store::{CredentialError, CredentialStore, InMemoryCredentialStore};
use localityd::generation_http::{
    GenerationHttpRuntimeReference, GenerationHttpRuntimeResolutionError,
};

const CREDENTIAL_REF: &str = "workspace-session:test-profile";

#[test]
fn runtime_reference_resolves_the_session_credential_on_each_call() {
    let credentials = InMemoryCredentialStore::new();
    let mut session_fixture: serde_json::Value =
        serde_json::from_slice(WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON)
            .expect("session fixture JSON");
    session_fixture["session_id"] = serde_json::json!("018f4f6e-7b8c-7d9e-8f01-23456789abcd");
    let encoded_session = serde_json::to_string(&session_fixture).expect("encoded session fixture");
    let session = WorkspaceProfileSessionV2::decode_json(encoded_session.as_bytes())
        .expect("session fixture");
    credentials
        .put(CREDENTIAL_REF, &encoded_session)
        .expect("store session credential");
    let reference = GenerationHttpRuntimeReference::new("http://127.0.0.1:9", CREDENTIAL_REF)
        .expect("runtime reference");

    let runtime = reference.resolve(&credentials).expect("resolve runtime");
    let diagnostics = format!("{runtime:?}");
    assert!(!diagnostics.contains(session.opaque_capability()));
    assert!(diagnostics.contains("<redacted>"));

    credentials
        .put(CREDENTIAL_REF, "not a generation-2 session")
        .expect("replace session credential");
    assert_eq!(
        reference.resolve(&credentials).unwrap_err(),
        GenerationHttpRuntimeResolutionError::InvalidSessionCredential
    );
}

#[test]
fn runtime_reference_reports_missing_and_empty_credential_references() {
    let credentials = InMemoryCredentialStore::new();
    let reference = GenerationHttpRuntimeReference::new("https://locality.example", CREDENTIAL_REF)
        .expect("runtime reference");

    assert_eq!(
        reference.resolve(&credentials).unwrap_err(),
        GenerationHttpRuntimeResolutionError::Credential(CredentialError::NotFound(
            CREDENTIAL_REF.to_string()
        ))
    );
    assert_eq!(
        GenerationHttpRuntimeReference::new("https://locality.example", "").unwrap_err(),
        GenerationHttpRuntimeResolutionError::EmptyCredentialReference
    );
}
