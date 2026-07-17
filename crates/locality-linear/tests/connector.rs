use std::sync::{Arc, Mutex};

use locality_connector::{
    ApplyPlanRequest, ChildContainer, Connector, EnumerateRequest, ListChildrenRequest,
};
use locality_core::journal::{PushId, PushOperationId};
use locality_core::model::{EntityKind, MountId, RemoteId};
use locality_core::planner::{PropertyValue, PushOperation, PushPlan};
use locality_core::push::RemotePrecondition;
use locality_linear::{
    LinearApi, LinearConfig, LinearConnector, LinearIssue, LinearIssuePage, LinearIssuePriority,
    LinearIssueState, LinearIssueUpdateInput, LinearLabel, LinearProject, LinearTeam, LinearUser,
    render_linear_issue,
};

#[test]
fn enumeration_projects_teams_and_issues_into_stable_paths() {
    let api = Arc::new(FakeLinearApi::with_issues(vec![issue()]));
    let connector = LinearConnector::with_api(LinearConfig::new("secret"), api);

    let entries = connector
        .enumerate(EnumerateRequest {
            mount_id: MountId::new("linear-main"),
            cursor: None,
        })
        .expect("enumerate");

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].kind, EntityKind::Directory);
    assert_eq!(entries[0].remote_id, RemoteId::new("team:team-1"));
    assert_eq!(entries[0].path.to_string_lossy(), "Engineering");
    assert_eq!(entries[1].kind, EntityKind::Page);
    assert_eq!(entries[1].remote_id, RemoteId::new("issue-1"));
    assert_eq!(
        entries[1].path.to_string_lossy(),
        "Engineering/ENG-1/page.md"
    );
    assert_eq!(
        entries[1].remote_edited_at.as_deref(),
        Some("linear:issue-1:2026-07-15T12:00:00Z")
    );
    assert!(
        entries[1]
            .stub_frontmatter
            .as_deref()
            .unwrap()
            .contains("Project: \"Launch <project-1>\"")
    );
}

#[test]
fn list_team_children_returns_complete_issue_snapshot() {
    let api = Arc::new(FakeLinearApi::with_issues(vec![issue()]));
    let connector = LinearConnector::with_api(LinearConfig::new("secret"), api);

    let result = connector
        .list_children(ListChildrenRequest {
            mount_id: MountId::new("linear-main"),
            container: ChildContainer::DirectoryChildren(RemoteId::new("team:team-1")),
            parent_path: "Engineering".into(),
        })
        .expect("list team issues");

    assert!(result.is_complete());
    assert_eq!(result.entries.len(), 1);
    assert_eq!(
        result.entries[0].path.to_string_lossy(),
        "Engineering/ENG-1/page.md"
    );
}

#[test]
fn capabilities_do_not_advertise_oauth_until_broker_flow_exists() {
    let api = Arc::new(FakeLinearApi::with_issues(vec![issue()]));
    let connector = LinearConnector::with_api(LinearConfig::new("secret"), api);

    let capabilities = connector.capabilities();

    assert!(capabilities.supports_entity_body_updates);
    assert!(capabilities.supports_batch_observation);
    assert!(!capabilities.supports_oauth);
}

#[test]
fn rendering_uses_uuid_reference_frontmatter_and_description_body() {
    let document = render_linear_issue(&issue()).expect("render issue");

    assert_eq!(
        document.frontmatter,
        "loc:\n  id: issue-1\n  type: page\n  connector: linear\n  synced_at: \"2026-07-15T12:00:00Z\"\n  remote_edited_at: \"2026-07-15T12:00:00Z\"\ntitle: \"Improve sync\"\nidentifier: ENG-1\nurl: \"https://linear.app/acme/issue/ENG-1/improve-sync\"\nStatus: \"Todo <state-1>\"\nTeam: \"Engineering <team-1>\"\nProject: \"Launch <project-1>\"\nAssignee: \"Ada <user-1>\"\nPriority: High\nEstimate: 3\nLabels:\n  - \"Bug <label-1>\"\n"
    );
    assert_eq!(document.body, "Existing description.\n");
}

#[test]
fn apply_updates_description_title_status_project_and_assignee() {
    let api = Arc::new(FakeLinearApi::with_issues(vec![issue()]));
    let connector = LinearConnector::with_api(LinearConfig::new("secret"), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("issue-1")],
        vec![
            PushOperation::UpdateEntityBody {
                entity_id: RemoteId::new("issue-1"),
                body: "Updated description.\n".to_string(),
            },
            PushOperation::UpdateProperties {
                entity_id: RemoteId::new("issue-1"),
                properties: [
                    (
                        "title".to_string(),
                        PropertyValue::String("New title".to_string()),
                    ),
                    (
                        "Status".to_string(),
                        PropertyValue::String("Done <state-2>".to_string()),
                    ),
                    (
                        "Project".to_string(),
                        PropertyValue::String("Growth <project-2>".to_string()),
                    ),
                    (
                        "Assignee".to_string(),
                        PropertyValue::String("Grace <user-2>".to_string()),
                    ),
                ]
                .into_iter()
                .collect(),
            },
        ],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = [
        PushOperationId("op-body".to_string()),
        PushOperationId("op-props".to_string()),
    ];
    let preconditions = [RemotePrecondition {
        remote_id: RemoteId::new("issue-1"),
        remote_edited_at: Some("linear:issue-1:2026-07-15T12:00:00Z".to_string()),
    }];

    let result = connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &MountId::new("linear-main"),
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &preconditions,
            local_root: None,
        })
        .expect("apply");

    assert_eq!(result.changed_remote_ids, vec![RemoteId::new("issue-1")]);
    assert_eq!(
        api.updates.lock().unwrap().as_slice(),
        &[LinearIssueUpdateInput {
            issue_id: "issue-1".to_string(),
            title: Some("New title".to_string()),
            description: Some("Updated description.\n".to_string()),
            state_id: Some("state-2".to_string()),
            project_id: Some(Some("project-2".to_string())),
            assignee_id: Some(Some("user-2".to_string())),
        }]
    );
}

#[derive(Debug, Default)]
struct FakeLinearApi {
    issues: Mutex<Vec<LinearIssue>>,
    updates: Mutex<Vec<LinearIssueUpdateInput>>,
}

impl FakeLinearApi {
    fn with_issues(issues: Vec<LinearIssue>) -> Self {
        Self {
            issues: Mutex::new(issues),
            updates: Mutex::new(Vec::new()),
        }
    }
}

impl LinearApi for FakeLinearApi {
    fn list_issues(
        &self,
        _cursor: Option<&str>,
        _updated_after: Option<&str>,
        team_id: Option<&str>,
    ) -> locality_core::LocalityResult<LinearIssuePage> {
        let issues = self
            .issues
            .lock()
            .unwrap()
            .iter()
            .filter(|issue| team_id.is_none_or(|team_id| issue.team.id == team_id))
            .cloned()
            .collect();
        Ok(LinearIssuePage {
            issues,
            has_next_page: false,
            end_cursor: None,
        })
    }

    fn get_issue(&self, issue_id: &str) -> locality_core::LocalityResult<LinearIssue> {
        self.issues
            .lock()
            .unwrap()
            .iter()
            .find(|issue| issue.id == issue_id)
            .cloned()
            .ok_or_else(|| locality_core::LocalityError::RemoteNotFound(issue_id.to_string()))
    }

    fn update_issue(
        &self,
        input: LinearIssueUpdateInput,
    ) -> locality_core::LocalityResult<LinearIssue> {
        self.updates.lock().unwrap().push(input.clone());
        let mut issue = self.get_issue(&input.issue_id)?;
        if let Some(title) = &input.title {
            issue.title = title.clone();
        }
        if let Some(description) = &input.description {
            issue.description = Some(description.clone());
        }
        if let Some(state_id) = &input.state_id {
            issue.state.id = state_id.clone();
            issue.state.name = "Done".to_string();
        }
        if let Some(project_id) = &input.project_id {
            issue.project = project_id.as_ref().map(|id| LinearProject {
                id: id.clone(),
                name: "Growth".to_string(),
            });
        }
        if let Some(assignee_id) = &input.assignee_id {
            issue.assignee = assignee_id.as_ref().map(|id| LinearUser {
                id: id.clone(),
                name: "Grace".to_string(),
                email: Some("grace@example.com".to_string()),
            });
        }
        Ok(issue)
    }
}

fn issue() -> LinearIssue {
    LinearIssue {
        id: "issue-1".to_string(),
        identifier: "ENG-1".to_string(),
        title: "Improve sync".to_string(),
        description: Some("Existing description.".to_string()),
        url: "https://linear.app/acme/issue/ENG-1/improve-sync".to_string(),
        created_at: "2026-07-14T12:00:00Z".to_string(),
        updated_at: "2026-07-15T12:00:00Z".to_string(),
        archived_at: None,
        priority: Some(LinearIssuePriority {
            value: 3,
            label: "High".to_string(),
        }),
        estimate: Some(3.0),
        team: LinearTeam {
            id: "team-1".to_string(),
            key: "ENG".to_string(),
            name: "Engineering".to_string(),
        },
        state: LinearIssueState {
            id: "state-1".to_string(),
            name: "Todo".to_string(),
            state_type: Some("unstarted".to_string()),
        },
        project: Some(LinearProject {
            id: "project-1".to_string(),
            name: "Launch".to_string(),
        }),
        assignee: Some(LinearUser {
            id: "user-1".to_string(),
            name: "Ada".to_string(),
            email: Some("ada@example.com".to_string()),
        }),
        labels: vec![LinearLabel {
            id: "label-1".to_string(),
            name: "Bug".to_string(),
        }],
    }
}
