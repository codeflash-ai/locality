use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use locality_connector::{
    ApplyPlanRequest, BatchObserveRequest, ChildContainer, Connector, ConnectorCheckpoint,
    EnumerateRequest, ListChildrenRequest, ObserveRequest,
};
use locality_core::journal::{JournalApplyEffect, PushId, PushOperationId};
use locality_core::model::{EntityKind, MountId, RemoteId};
use locality_core::planner::{PropertyValue, PushOperation, PushOperationKind, PushPlan};
use locality_core::push::RemotePrecondition;
use locality_core::search::RAW_SEARCH_METADATA_KEY;
use locality_linear::{
    LinearApi, LinearAttachment, LinearAttachmentDownload, LinearComment, LinearConfig,
    LinearConnector, LinearIssue, LinearIssueContext, LinearIssueContextKind,
    LinearIssueHistoryEntry, LinearIssuePage, LinearIssuePriority, LinearIssueState,
    LinearIssueUpdateInput, LinearLabel, LinearProject, LinearTeam, LinearUser,
    linear_context_remote_id, render_linear_issue, render_linear_issue_context,
};

#[test]
fn enumeration_projects_teams_statuses_and_issues_into_stable_paths() {
    let api = Arc::new(FakeLinearApi::with_issues(vec![issue()]));
    let connector = LinearConnector::with_api(LinearConfig::new("secret"), api);

    let entries = connector
        .enumerate(EnumerateRequest {
            mount_id: MountId::new("linear-main"),
            cursor: None,
        })
        .expect("enumerate");

    assert_eq!(
        entries
            .iter()
            .map(|entry| (
                entry.remote_id.as_str().to_string(),
                entry.kind.clone(),
                entry.path.to_string_lossy().to_string()
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "linear:teams".to_string(),
                EntityKind::Directory,
                "Teams".to_string()
            ),
            (
                "team:team-1".to_string(),
                EntityKind::Directory,
                "Teams/Engineering".to_string()
            ),
            (
                "team-issues:team-1".to_string(),
                EntityKind::Directory,
                "Teams/Engineering/Issues".to_string()
            ),
            (
                "team-state:team-1:state-1".to_string(),
                EntityKind::Directory,
                "Teams/Engineering/Issues/Todo".to_string()
            ),
            (
                "linear-context:issue-1:attachments".to_string(),
                EntityKind::Asset,
                "Teams/Engineering/Issues/Todo/ENG-1 Improve sync/attachments.md".to_string()
            ),
            (
                "linear-context:issue-1:comments".to_string(),
                EntityKind::Asset,
                "Teams/Engineering/Issues/Todo/ENG-1 Improve sync/comments.md".to_string()
            ),
            (
                "linear-context:issue-1:history".to_string(),
                EntityKind::Asset,
                "Teams/Engineering/Issues/Todo/ENG-1 Improve sync/history.md".to_string()
            ),
            (
                "issue-1".to_string(),
                EntityKind::Page,
                "Teams/Engineering/Issues/Todo/ENG-1 Improve sync/page.md".to_string()
            ),
            (
                "linear-context:issue-1:pull-requests".to_string(),
                EntityKind::Asset,
                "Teams/Engineering/Issues/Todo/ENG-1 Improve sync/pull-requests.md".to_string()
            ),
        ]
    );
    let issue_entry = entries
        .iter()
        .find(|entry| entry.remote_id.as_str() == "issue-1")
        .expect("issue entry");
    assert_eq!(issue_entry.title, "Improve sync");
    assert_eq!(
        issue_entry.remote_edited_at.as_deref(),
        Some("linear:issue-1:2026-07-15T12:00:00Z")
    );
    assert!(
        issue_entry
            .stub_frontmatter
            .as_deref()
            .unwrap()
            .contains("Project: \"Launch <project-1>\"")
    );
}

#[test]
fn list_hierarchical_children_returns_complete_snapshots() {
    let api = Arc::new(FakeLinearApi::with_issues(vec![issue()]));
    let connector = LinearConnector::with_api(LinearConfig::new("secret"), api);

    let root = connector
        .list_children(ListChildrenRequest {
            mount_id: MountId::new("linear-main"),
            container: ChildContainer::Root,
            parent_path: "".into(),
        })
        .expect("list root");
    assert!(root.is_complete());
    assert_eq!(entry_paths(&root.entries), vec!["Teams"]);

    let teams = connector
        .list_children(ListChildrenRequest {
            mount_id: MountId::new("linear-main"),
            container: ChildContainer::DirectoryChildren(RemoteId::new("linear:teams")),
            parent_path: "Teams".into(),
        })
        .expect("list teams");
    assert!(teams.is_complete());
    assert_eq!(entry_paths(&teams.entries), vec!["Teams/Engineering"]);

    let team = connector
        .list_children(ListChildrenRequest {
            mount_id: MountId::new("linear-main"),
            container: ChildContainer::DirectoryChildren(RemoteId::new("team:team-1")),
            parent_path: "Teams/Engineering".into(),
        })
        .expect("list team");
    assert!(team.is_complete());
    assert_eq!(entry_paths(&team.entries), vec!["Teams/Engineering/Issues"]);

    let issues = connector
        .list_children(ListChildrenRequest {
            mount_id: MountId::new("linear-main"),
            container: ChildContainer::DirectoryChildren(RemoteId::new("team-issues:team-1")),
            parent_path: "Teams/Engineering/Issues".into(),
        })
        .expect("list issue statuses");
    assert!(issues.is_complete());
    assert_eq!(
        entry_paths(&issues.entries),
        vec!["Teams/Engineering/Issues/Todo"]
    );

    let result = connector
        .list_children(ListChildrenRequest {
            mount_id: MountId::new("linear-main"),
            container: ChildContainer::DirectoryChildren(RemoteId::new(
                "team-state:team-1:state-1",
            )),
            parent_path: "Teams/Engineering/Issues/Todo".into(),
        })
        .expect("list status issues");

    assert!(result.is_complete());
    assert_eq!(
        entry_paths(&result.entries),
        vec!["Teams/Engineering/Issues/Todo/ENG-1 Improve sync/page.md"]
    );

    let sidecars = connector
        .list_children(ListChildrenRequest {
            mount_id: MountId::new("linear-main"),
            container: ChildContainer::PageChildren(RemoteId::new("issue-1")),
            parent_path: "Teams/Engineering/Issues/Todo/ENG-1 Improve sync".into(),
        })
        .expect("list issue sidecars");
    assert!(sidecars.is_complete());
    assert_eq!(
        sidecars
            .entries
            .iter()
            .map(|entry| (
                entry.remote_id.as_str().to_string(),
                entry.kind.clone(),
                entry.path.to_string_lossy().to_string()
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "linear-context:issue-1:comments".to_string(),
                EntityKind::Asset,
                "Teams/Engineering/Issues/Todo/ENG-1 Improve sync/comments.md".to_string()
            ),
            (
                "linear-context:issue-1:attachments".to_string(),
                EntityKind::Asset,
                "Teams/Engineering/Issues/Todo/ENG-1 Improve sync/attachments.md".to_string()
            ),
            (
                "linear-context:issue-1:pull-requests".to_string(),
                EntityKind::Asset,
                "Teams/Engineering/Issues/Todo/ENG-1 Improve sync/pull-requests.md".to_string()
            ),
            (
                "linear-context:issue-1:history".to_string(),
                EntityKind::Asset,
                "Teams/Engineering/Issues/Todo/ENG-1 Improve sync/history.md".to_string()
            ),
        ]
    );
}

#[test]
fn capabilities_do_not_advertise_oauth_until_broker_flow_exists() {
    let api = Arc::new(FakeLinearApi::with_issues(vec![issue()]));
    let connector = LinearConnector::with_api(LinearConfig::new("secret"), api);

    let capabilities = connector.capabilities();

    assert!(capabilities.supports_entity_body_updates);
    assert!(capabilities.supports_batch_observation);
    assert!(capabilities.supports_media_download);
    assert!(!capabilities.supports_oauth);
    assert!(
        connector
            .supported_push_operations()
            .contains(&PushOperationKind::MoveEntity)
    );
}

#[test]
fn observe_and_observe_batch_use_hierarchical_issue_path() {
    let api = Arc::new(FakeLinearApi::with_issues(vec![issue()]));
    let connector = LinearConnector::with_api(LinearConfig::new("secret"), api);

    let observed = connector
        .observe(ObserveRequest {
            mount_id: MountId::new("linear-main"),
            remote_id: RemoteId::new("issue-1"),
        })
        .expect("observe");
    assert_eq!(
        observed.projected_path.to_string_lossy(),
        "Teams/Engineering/Issues/Todo/ENG-1 Improve sync/page.md"
    );
    assert_eq!(
        observed.parent_remote_id,
        Some(RemoteId::new("team-state:team-1:state-1"))
    );
    let raw_metadata = serde_json::from_str::<serde_json::Value>(&observed.raw_metadata_json)
        .expect("raw metadata json");
    assert_eq!(
        raw_metadata[RAW_SEARCH_METADATA_KEY]["source_url"],
        serde_json::json!("https://linear.app/acme/issue/ENG-1/improve-sync")
    );
    assert_eq!(
        raw_metadata[RAW_SEARCH_METADATA_KEY]["aliases"],
        serde_json::json!(["ENG-1"])
    );
    let search_terms = raw_metadata[RAW_SEARCH_METADATA_KEY]["metadata_text"]
        .as_array()
        .expect("metadata_text array");
    for expected in [
        "ENG",
        "Engineering",
        "Todo",
        "Launch",
        "Ada",
        "ada@example.com",
        "Bug",
        "High",
    ] {
        assert!(
            search_terms.contains(&serde_json::json!(expected)),
            "missing search term {expected}"
        );
    }

    let batch = connector
        .observe_batch(BatchObserveRequest {
            mount_id: MountId::new("linear-main"),
            checkpoint: Some(ConnectorCheckpoint {
                state_version: 1,
                min_reader_version: 1,
                state_json: serde_json::json!({ "updated_after": "2026-07-14T00:00:00Z" })
                    .to_string(),
            }),
        })
        .expect("observe batch");
    let entry = match &batch.changes[0] {
        locality_connector::BatchObservationChange::Upsert(entry) => entry,
        locality_connector::BatchObservationChange::Tombstone { .. } => {
            panic!("expected issue upsert")
        }
    };
    assert_eq!(
        entry.path.to_string_lossy(),
        "Teams/Engineering/Issues/Todo/ENG-1 Improve sync/page.md"
    );
}

#[test]
fn rendering_uses_uuid_reference_frontmatter_and_description_body() {
    let document = render_linear_issue(&issue()).expect("render issue");

    assert_eq!(
        document.frontmatter,
        "loc:\n  id: issue-1\n  type: page\n  connector: linear\n  synced_at: \"2026-07-15T12:00:00Z\"\n  remote_edited_at: \"2026-07-15T12:00:00Z\"\ntitle: \"Improve sync\"\nidentifier: ENG-1\nurl: \"https://linear.app/acme/issue/ENG-1/improve-sync\"\ncreated_at: \"2026-07-14T12:00:00Z\"\nupdated_at: \"2026-07-15T12:00:00Z\"\narchived_at: null\nstarted_at: null\ncompleted_at: null\ncanceled_at: null\nauto_archived_at: null\nauto_closed_at: null\nstarted_triage_at: null\ntriaged_at: null\nsnoozed_until_at: null\nadded_to_cycle_at: null\nadded_to_project_at: null\nadded_to_team_at: null\ndue_date: null\nStatus: \"Todo <state-1>\"\nTeam: \"Engineering <team-1>\"\nProject: \"Launch <project-1>\"\nAssignee: \"Ada <user-1>\"\nPriority: High\nEstimate: 3\nLabels:\n  - \"Bug <label-1>\"\n"
    );
    assert_eq!(document.body, "Existing description.\n");
}

#[test]
fn rendering_quotes_populated_lifecycle_and_date_frontmatter() {
    let mut issue = issue();
    issue.archived_at = Some("2026-08-01T12:00:00Z".to_string());
    issue.started_at = Some("2026-07-15T13:00:00Z".to_string());
    issue.completed_at = Some("2026-07-20T10:00:00Z".to_string());
    issue.canceled_at = Some("2026-07-21T10:00:00Z".to_string());
    issue.auto_archived_at = Some("2026-08-15T00:00:00Z".to_string());
    issue.auto_closed_at = Some("2026-07-25T00:00:00Z".to_string());
    issue.started_triage_at = Some("2026-07-14T13:00:00Z".to_string());
    issue.triaged_at = Some("2026-07-14T14:00:00Z".to_string());
    issue.snoozed_until_at = Some("2026-07-22T09:00:00Z".to_string());
    issue.added_to_cycle_at = Some("2026-07-14T15:00:00Z".to_string());
    issue.added_to_project_at = Some("2026-07-14T16:00:00Z".to_string());
    issue.added_to_team_at = Some("2026-07-14T17:00:00Z".to_string());
    issue.due_date = Some("2026-07-31".to_string());

    let document = render_linear_issue(&issue).expect("render issue");

    assert_eq!(
        document.frontmatter,
        "loc:\n  id: issue-1\n  type: page\n  connector: linear\n  synced_at: \"2026-07-15T12:00:00Z\"\n  remote_edited_at: \"2026-07-15T12:00:00Z\"\ntitle: \"Improve sync\"\nidentifier: ENG-1\nurl: \"https://linear.app/acme/issue/ENG-1/improve-sync\"\ncreated_at: \"2026-07-14T12:00:00Z\"\nupdated_at: \"2026-07-15T12:00:00Z\"\narchived_at: \"2026-08-01T12:00:00Z\"\nstarted_at: \"2026-07-15T13:00:00Z\"\ncompleted_at: \"2026-07-20T10:00:00Z\"\ncanceled_at: \"2026-07-21T10:00:00Z\"\nauto_archived_at: \"2026-08-15T00:00:00Z\"\nauto_closed_at: \"2026-07-25T00:00:00Z\"\nstarted_triage_at: \"2026-07-14T13:00:00Z\"\ntriaged_at: \"2026-07-14T14:00:00Z\"\nsnoozed_until_at: \"2026-07-22T09:00:00Z\"\nadded_to_cycle_at: \"2026-07-14T15:00:00Z\"\nadded_to_project_at: \"2026-07-14T16:00:00Z\"\nadded_to_team_at: \"2026-07-14T17:00:00Z\"\ndue_date: \"2026-07-31\"\nStatus: \"Todo <state-1>\"\nTeam: \"Engineering <team-1>\"\nProject: \"Launch <project-1>\"\nAssignee: \"Ada <user-1>\"\nPriority: High\nEstimate: 3\nLabels:\n  - \"Bug <label-1>\"\n"
    );
}

#[test]
fn rendering_context_sidecars_uses_read_only_asset_frontmatter() {
    let context = issue_context(&issue());

    let document = render_linear_issue_context(&context, LinearIssueContextKind::Comments)
        .expect("render comments");

    assert_eq!(
        document.frontmatter,
        "loc:\n  id: \"linear-context:issue-1:comments\"\n  type: asset\n  connector: linear\n  synced_at: \"2026-07-15T12:00:00Z\"\n  remote_edited_at: \"2026-07-15T12:00:00Z\"\ntitle: \"ENG-1 Comments\"\nlinear:\n  issue_id: issue-1\n  issue_identifier: ENG-1\n  context: comments\n  read_only: true\n"
    );
    assert_eq!(
        document.body,
        "# Comments\n\n## 2026-07-15T13:00:00Z - Ada <user-1>\n\n- id: `comment-1`\n- url: https://linear.app/acme/issue/ENG-1/improve-sync#comment-comment-1\n- updated_at: 2026-07-15T13:05:00Z\n- edited_at: 2026-07-15T13:05:00Z\n\nLooks good.\n\n"
    );
}

#[test]
fn rendering_attachments_includes_download_status_and_metadata() {
    let mut context = issue_context(&issue());
    context.attachments[0].download = Some(LinearAttachmentDownload {
        status: "failed".to_string(),
        local_path: None,
        error: Some("Linear attachment download returned HTTP 403".to_string()),
    });

    let document = render_linear_issue_context(&context, LinearIssueContextKind::Attachments)
        .expect("render attachments");

    assert_eq!(
        document.body,
        "# Attachments\n\n## GitHub PR #42\n\n- id: `attach-1`\n- url: https://github.com/acme/app/pull/42\n- source_type: github\n- created_at: 2026-07-15T14:00:00Z\n- updated_at: 2026-07-15T14:10:00Z\n- creator: Ada <user-1>\n- subtitle: Open pull request\n- download_status: failed\n- download_error: Linear attachment download returned HTTP 403\n- metadata:\n\n```json\n{\n  \"branch\": \"eng-1-improve-sync\",\n  \"number\": 42,\n  \"repository\": \"acme/app\",\n  \"status\": \"open\"\n}\n```\n\n"
    );
}

#[test]
fn rendering_pull_requests_includes_branch_even_without_pr_attachments() {
    let mut context = issue_context(&issue());
    context.attachments.clear();

    let document = render_linear_issue_context(&context, LinearIssueContextKind::PullRequests)
        .expect("render pull requests");

    assert_eq!(
        document.body,
        "# Pull Requests\n\nSuggested branch: `eng-1-improve-sync`\n\n_No pull request attachments found._\n"
    );
}

#[test]
fn rendering_history_includes_all_supported_change_fields_and_raw_changes() {
    let context = issue_context(&issue());

    let document = render_linear_issue_context(&context, LinearIssueContextKind::History)
        .expect("render history");

    assert_eq!(
        document.body,
        "# History\n\n## 2026-07-15T15:00:00Z - Ada <user-1>\n\n- id: `history-1`\n- updated_at: 2026-07-15T15:01:00Z\n- status: Todo <state-1> -> Done <state-2>\n- title: Improve sync -> Improve sync quickly\n- assignee: null -> Ada <user-1>\n- project: null -> Launch <project-1>\n- due_date: null -> 2026-07-31\n- estimate: null -> 3\n- priority: null -> 3\n- description_updated: true\n- labels_added: Bug <label-1>\n- attachment: GitHub PR #42 <attach-1>\n- attachment_url: https://github.com/acme/app/pull/42\n- changes:\n\n```json\n{\n  \"description\": true\n}\n```\n\n"
    );
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
            team_id: None,
            state_id: Some("state-2".to_string()),
            project_id: Some(Some("project-2".to_string())),
            assignee_id: Some(Some("user-2".to_string())),
        }]
    );
}

#[test]
fn apply_move_updates_issue_team_and_status_and_reports_moved_effect() {
    let api = Arc::new(FakeLinearApi::with_issues(vec![issue()]));
    let connector = LinearConnector::with_api(LinearConfig::new("secret"), api.clone());
    let plan = PushPlan::new(
        vec![RemoteId::new("issue-1")],
        vec![PushOperation::MoveEntity {
            entity_id: RemoteId::new("issue-1"),
            new_parent_id: RemoteId::new("team-state:team-2:state-2"),
            new_parent_kind: EntityKind::Directory,
            new_title: "Edited issue title".to_string(),
            projected_path: "Teams/Platform/Issues/Done/ENG-1 Edited issue title/page.md".into(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = [PushOperationId("op-move".to_string())];
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
        .expect("apply move");

    assert_eq!(result.changed_remote_ids, vec![RemoteId::new("issue-1")]);
    assert_eq!(
        result.effects,
        vec![JournalApplyEffect::MovedEntity {
            operation_id: PushOperationId("op-move".to_string()),
            operation_index: 0,
            entity_id: RemoteId::new("issue-1"),
            parent_id: RemoteId::new("team-state:team-2:state-2"),
        }]
    );
    assert_eq!(
        api.updates.lock().unwrap().as_slice(),
        &[LinearIssueUpdateInput {
            issue_id: "issue-1".to_string(),
            title: Some("Edited issue title".to_string()),
            description: None,
            team_id: Some("team-2".to_string()),
            state_id: Some("state-2".to_string()),
            project_id: None,
            assignee_id: None,
        }]
    );
}

#[test]
fn apply_rejects_linear_context_remote_ids_as_read_only() {
    let api = Arc::new(FakeLinearApi::with_issues(vec![issue()]));
    let connector = LinearConnector::with_api(LinearConfig::new("secret"), api.clone());
    let context_id = RemoteId::new(linear_context_remote_id(
        "issue-1",
        LinearIssueContextKind::Comments,
    ));
    let plan = PushPlan::new(
        vec![context_id.clone()],
        vec![PushOperation::UpdateEntityBody {
            entity_id: context_id.clone(),
            body: "Edited comments.\n".to_string(),
        }],
    );
    let push_id = PushId("push-1".to_string());
    let operation_ids = [PushOperationId("op-body".to_string())];
    let preconditions = [RemotePrecondition {
        remote_id: context_id,
        remote_edited_at: Some("linear-context:issue-1:comments:2026-07-15T12:00:00Z".to_string()),
    }];

    let error = connector
        .apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &MountId::new("linear-main"),
            plan: &plan,
            operation_ids: &operation_ids,
            remote_preconditions: &preconditions,
            local_root: None,
        })
        .expect_err("context sidecar push should be rejected");

    assert!(matches!(error, locality_core::LocalityError::Validation(_)));
    assert!(api.updates.lock().unwrap().is_empty());
}

fn entry_paths(entries: &[locality_core::model::TreeEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| entry.path.to_string_lossy().to_string())
        .collect()
}

#[derive(Debug, Default)]
struct FakeLinearApi {
    issues: Mutex<Vec<LinearIssue>>,
    contexts: Mutex<BTreeMap<String, LinearIssueContext>>,
    updates: Mutex<Vec<LinearIssueUpdateInput>>,
}

impl FakeLinearApi {
    fn with_issues(issues: Vec<LinearIssue>) -> Self {
        let contexts = issues
            .iter()
            .map(|issue| (issue.id.clone(), issue_context(issue)))
            .collect();
        Self {
            issues: Mutex::new(issues),
            contexts: Mutex::new(contexts),
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

    fn get_issue_context(
        &self,
        issue_id: &str,
    ) -> locality_core::LocalityResult<LinearIssueContext> {
        self.contexts
            .lock()
            .unwrap()
            .get(issue_id)
            .cloned()
            .ok_or_else(|| locality_core::LocalityError::RemoteNotFound(issue_id.to_string()))
    }

    fn download_attachment(
        &self,
        _url: &str,
        _max_bytes: u64,
    ) -> locality_core::LocalityResult<Vec<u8>> {
        Err(locality_core::LocalityError::Unsupported(
            "fake Linear attachment download",
        ))
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
        if let Some(team_id) = &input.team_id {
            issue.team = LinearTeam {
                id: team_id.clone(),
                key: "PLAT".to_string(),
                name: "Platform".to_string(),
            };
            issue.identifier = "PLAT-1".to_string();
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
        started_at: None,
        completed_at: None,
        canceled_at: None,
        auto_archived_at: None,
        auto_closed_at: None,
        started_triage_at: None,
        triaged_at: None,
        snoozed_until_at: None,
        added_to_cycle_at: None,
        added_to_project_at: None,
        added_to_team_at: None,
        due_date: None,
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

fn issue_context(issue: &LinearIssue) -> LinearIssueContext {
    LinearIssueContext {
        issue_id: issue.id.clone(),
        issue_identifier: issue.identifier.clone(),
        issue_title: issue.title.clone(),
        issue_updated_at: issue.updated_at.clone(),
        branch_name: "eng-1-improve-sync".to_string(),
        comments: vec![LinearComment {
            id: "comment-1".to_string(),
            body: "Looks good.".to_string(),
            url: "https://linear.app/acme/issue/ENG-1/improve-sync#comment-comment-1".to_string(),
            created_at: "2026-07-15T13:00:00Z".to_string(),
            updated_at: "2026-07-15T13:05:00Z".to_string(),
            edited_at: Some("2026-07-15T13:05:00Z".to_string()),
            parent_id: None,
            resolved_at: None,
            user: issue.assignee.clone(),
            external_user: None,
            bot_actor: None,
        }],
        attachments: vec![LinearAttachment {
            id: "attach-1".to_string(),
            title: "GitHub PR #42".to_string(),
            url: "https://github.com/acme/app/pull/42".to_string(),
            created_at: "2026-07-15T14:00:00Z".to_string(),
            updated_at: "2026-07-15T14:10:00Z".to_string(),
            source_type: Some("github".to_string()),
            subtitle: Some("Open pull request".to_string()),
            creator: issue.assignee.clone(),
            external_user_creator: None,
            metadata: serde_json::json!({
                "branch": "eng-1-improve-sync",
                "number": 42,
                "repository": "acme/app",
                "status": "open"
            }),
            download: None,
        }],
        history: vec![LinearIssueHistoryEntry {
            id: "history-1".to_string(),
            created_at: "2026-07-15T15:00:00Z".to_string(),
            updated_at: "2026-07-15T15:01:00Z".to_string(),
            actor: issue.assignee.clone(),
            bot_actor: None,
            from_state: Some(issue.state.clone()),
            to_state: Some(LinearIssueState {
                id: "state-2".to_string(),
                name: "Done".to_string(),
                state_type: Some("completed".to_string()),
            }),
            from_title: Some("Improve sync".to_string()),
            to_title: Some("Improve sync quickly".to_string()),
            from_assignee: None,
            to_assignee: issue.assignee.clone(),
            from_project: None,
            to_project: issue.project.clone(),
            from_team: None,
            to_team: None,
            from_due_date: None,
            to_due_date: Some("2026-07-31".to_string()),
            from_estimate: None,
            to_estimate: Some(3.0),
            from_priority: None,
            to_priority: Some(3.0),
            updated_description: Some(true),
            attachment_id: Some("attach-1".to_string()),
            attachment: Some(LinearAttachment {
                id: "attach-1".to_string(),
                title: "GitHub PR #42".to_string(),
                url: "https://github.com/acme/app/pull/42".to_string(),
                created_at: "2026-07-15T14:00:00Z".to_string(),
                updated_at: "2026-07-15T14:10:00Z".to_string(),
                source_type: Some("github".to_string()),
                subtitle: Some("Open pull request".to_string()),
                creator: issue.assignee.clone(),
                external_user_creator: None,
                metadata: serde_json::json!({ "number": 42 }),
                download: None,
            }),
            added_labels: issue.labels.clone(),
            removed_labels: Vec::new(),
            changes: Some(serde_json::json!({ "description": true })),
        }],
    }
}
