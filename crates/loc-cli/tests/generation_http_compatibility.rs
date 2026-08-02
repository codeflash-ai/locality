use loc_cli::generation_http::{
    GENERATION_BASELINE_CACHE_CONTROL, GENERATION_BASELINE_CONTENT_TYPE,
    GenerationBaselineHttpClient, GenerationHttpError, GenerationHttpOperation,
    GenerationHttpOptions, GenerationHttpRemoteCode, GenerationHttpResponseProblem,
    GenerationHttpRetryClassification, GenerationHttpRuntime, GenerationHttpTransport,
    GenerationHttpTransportFailure,
};

#[test]
fn legacy_generation_http_imports_are_exact_daemon_reexports() {
    fn accepts_daemon_options(_: localityd::generation_http::GenerationHttpOptions) {}
    fn accepts_daemon_baseline(
        _: Option<localityd::generation_http::GenerationBaselineHttpClient>,
    ) {
    }
    fn accepts_daemon_transport(_: Option<localityd::generation_http::GenerationHttpTransport>) {}

    accepts_daemon_options(GenerationHttpOptions::default());
    accepts_daemon_baseline(None::<GenerationBaselineHttpClient>);
    accepts_daemon_transport(None::<GenerationHttpTransport>);

    let _: Option<GenerationHttpRuntime> = None;
    let _: Option<GenerationHttpError> = None;
    let _: Option<GenerationHttpOperation> = None;
    let _: Option<GenerationHttpRemoteCode> = None;
    let _: Option<GenerationHttpResponseProblem> = None;
    let _: Option<GenerationHttpRetryClassification> = None;
    let _: Option<GenerationHttpTransportFailure> = None;
    assert_eq!(GENERATION_BASELINE_CONTENT_TYPE, "application/json");
    assert_eq!(GENERATION_BASELINE_CACHE_CONTROL, "no-store");
}
