use locality_linear::{
    HttpLinearApiClient, LinearApi, LinearIssueContextKind, LinearNativeBundle,
    render_linear_issue, render_linear_issue_context,
};

#[test]
#[ignore = "requires LINEAR_API_KEY and a stable scratch issue; performs read-only API calls"]
fn live_issue_hierarchy_context_and_canonical_rendering_are_consistent() {
    let token = required_env("LINEAR_API_KEY");
    let issue_id = required_env("LOCALITY_LINEAR_LIVE_ISSUE_ID");
    let api = HttpLinearApiClient::new(token);

    let issue = api
        .get_issue(&issue_id)
        .unwrap_or_else(|error| panic!("Linear issue read failed: {error}"));
    assert_eq!(
        issue.id, issue_id,
        "Linear issue read changed durable identity"
    );
    assert!(
        !issue.team.id.is_empty(),
        "Linear issue omitted team hierarchy identity"
    );
    assert!(
        !issue.state.id.is_empty(),
        "Linear issue omitted workflow-state identity"
    );

    let page = api
        .list_issues(None, None, Some(&issue.team.id))
        .unwrap_or_else(|error| panic!("Linear team issue list failed: {error}"));
    assert!(
        page.issues.iter().any(|listed| listed.id == issue_id),
        "configured Linear scratch issue was absent from its team hierarchy"
    );

    let rendered = render_linear_issue(&issue).expect("render live Linear issue");
    assert!(rendered.frontmatter.contains("  connector: linear\n"));
    assert!(rendered.frontmatter.contains(&issue.id));

    let context = api
        .get_issue_context(&issue_id)
        .unwrap_or_else(|error| panic!("Linear issue context read failed: {error}"));
    assert_eq!(
        context.issue_id, issue.id,
        "Linear context changed issue identity"
    );
    for kind in [
        LinearIssueContextKind::Comments,
        LinearIssueContextKind::Attachments,
        LinearIssueContextKind::PullRequests,
        LinearIssueContextKind::History,
    ] {
        let document = render_linear_issue_context(&context, kind)
            .unwrap_or_else(|error| panic!("render Linear {} sidecar: {error}", kind.filename()));
        assert!(document.frontmatter.contains("  read_only: true\n"));
        assert!(document.frontmatter.contains(&issue.id));
    }

    let rerendered = render_linear_issue(&issue).expect("repeat render live Linear issue");
    assert_eq!(
        rendered, rerendered,
        "Linear canonical rendering was not deterministic"
    );
    let _bundle_type_check = LinearNativeBundle {
        issue,
        context: None,
    };
}

fn required_env(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| panic!("set {name} to run the live Linear integrity test"))
}
