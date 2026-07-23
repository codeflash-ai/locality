//! Planned first-party connector scaffolds.
//!
//! These connectors are compile-time product/implementation scaffolds, not live
//! runtime integrations. They deliberately return unsupported for remote I/O so
//! daemon code cannot accidentally mount or mutate providers before a real
//! connector crate, credential resolver, and E2E coverage exist.

use std::collections::BTreeSet;

use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, Connector,
    ConnectorCapabilities, ConnectorExecutionPolicy, ConnectorKind, EnumerateRequest, FetchRequest,
    NativeEntity, ParsedEntity,
};
use locality_core::model::{CanonicalDocument, TreeEntry};
use locality_core::planner::PushOperationKind;
use locality_core::{LocalityError, LocalityResult};

pub const JIRA_CONNECTOR_ID: &str = "jira";
pub const SHAREPOINT_CONNECTOR_ID: &str = "sharepoint";
pub const ONEDRIVE_CONNECTOR_ID: &str = "onedrive";
pub const OUTLOOK_MAIL_CONNECTOR_ID: &str = "outlook-mail";
pub const OUTLOOK_CALENDAR_CONNECTOR_ID: &str = "outlook-calendar";
pub const MICROSOFT_TEAMS_CONNECTOR_ID: &str = "microsoft-teams";
pub const GOOGLE_DRIVE_CONNECTOR_ID: &str = "google-drive";
pub const DROPBOX_CONNECTOR_ID: &str = "dropbox";
pub const BOX_CONNECTOR_ID: &str = "box";
pub const FIGMA_CONNECTOR_ID: &str = "figma";
pub const ASANA_CONNECTOR_ID: &str = "asana";
pub const CLICKUP_CONNECTOR_ID: &str = "clickup";
pub const ZENDESK_CONNECTOR_ID: &str = "zendesk";
pub const INTERCOM_CONNECTOR_ID: &str = "intercom";
pub const HUBSPOT_CONNECTOR_ID: &str = "hubspot";
pub const SALESFORCE_CONNECTOR_ID: &str = "salesforce";
pub const FHIR_CONNECTOR_ID: &str = "fhir";

const SCAFFOLD_UNSUPPORTED: &str =
    "planned connector scaffold is not enabled for runtime remote I/O";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlannedConnectorCategory {
    Knowledge,
    Action,
    Hybrid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlannedConnectorSpec {
    id: &'static str,
    display_name: &'static str,
    category: PlannedConnectorCategory,
    auth_modes: &'static [&'static str],
    projection: &'static str,
    write_model: &'static str,
}

impl PlannedConnectorSpec {
    pub const fn new(
        id: &'static str,
        display_name: &'static str,
        category: PlannedConnectorCategory,
        auth_modes: &'static [&'static str],
        projection: &'static str,
        write_model: &'static str,
    ) -> Self {
        Self {
            id,
            display_name,
            category,
            auth_modes,
            projection,
            write_model,
        }
    }

    pub fn id(&self) -> &'static str {
        self.id
    }

    pub fn display_name(&self) -> &'static str {
        self.display_name
    }

    pub fn category(&self) -> PlannedConnectorCategory {
        self.category
    }

    pub fn auth_modes(&self) -> &'static [&'static str] {
        self.auth_modes
    }

    pub fn projection(&self) -> &'static str {
        self.projection
    }

    pub fn write_model(&self) -> &'static str {
        self.write_model
    }

    pub fn uses_oauth(&self) -> bool {
        self.auth_modes
            .iter()
            .any(|mode| matches!(*mode, "oauth" | "smart-oauth" | "github-app"))
    }
}

pub const JIRA_SPEC: PlannedConnectorSpec = PlannedConnectorSpec::new(
    JIRA_CONNECTOR_ID,
    "Jira",
    PlannedConnectorCategory::Hybrid,
    &["oauth", "api-token"],
    "Projects, issues, comments, and sprint folders.",
    "Reviewed issue body, status, assignee, and comment drafts.",
);

pub const SHAREPOINT_SPEC: PlannedConnectorSpec = PlannedConnectorSpec::new(
    SHAREPOINT_CONNECTOR_ID,
    "SharePoint",
    PlannedConnectorCategory::Knowledge,
    &["oauth"],
    "Sites, document libraries, pages, and files.",
    "Start read-only, then reviewed document updates where safe.",
);

pub const ONEDRIVE_SPEC: PlannedConnectorSpec = PlannedConnectorSpec::new(
    ONEDRIVE_CONNECTOR_ID,
    "OneDrive",
    PlannedConnectorCategory::Knowledge,
    &["oauth"],
    "User and shared files as folder hierarchies.",
    "Reviewed file updates after version checks.",
);

pub const OUTLOOK_MAIL_SPEC: PlannedConnectorSpec = PlannedConnectorSpec::new(
    OUTLOOK_MAIL_CONNECTOR_ID,
    "Outlook Mail",
    PlannedConnectorCategory::Action,
    &["oauth"],
    "Mailbox folders plus reviewed draft files.",
    "Reviewed outbound mail creates from draft files.",
);

pub const OUTLOOK_CALENDAR_SPEC: PlannedConnectorSpec = PlannedConnectorSpec::new(
    OUTLOOK_CALENDAR_CONNECTOR_ID,
    "Outlook Calendar",
    PlannedConnectorCategory::Action,
    &["oauth"],
    "Calendar events plus scheduling drafts.",
    "Reviewed event creates and updates after conflict checks.",
);

pub const MICROSOFT_TEAMS_SPEC: PlannedConnectorSpec = PlannedConnectorSpec::new(
    MICROSOFT_TEAMS_CONNECTOR_ID,
    "Microsoft Teams",
    PlannedConnectorCategory::Knowledge,
    &["oauth"],
    "Teams, channels, chats, meetings, and users as context.",
    "Start read-only, then reviewed message drafts if approved.",
);

pub const GOOGLE_DRIVE_SPEC: PlannedConnectorSpec = PlannedConnectorSpec::new(
    GOOGLE_DRIVE_CONNECTOR_ID,
    "Google Drive",
    PlannedConnectorCategory::Knowledge,
    &["oauth"],
    "Drive files, folders, shared drives, PDFs, sheets, and slides.",
    "Reviewed file updates where a canonical representation is safe.",
);

pub const DROPBOX_SPEC: PlannedConnectorSpec = PlannedConnectorSpec::new(
    DROPBOX_CONNECTOR_ID,
    "Dropbox",
    PlannedConnectorCategory::Knowledge,
    &["oauth"],
    "Shared files and folders.",
    "Reviewed file updates after revision checks.",
);

pub const BOX_SPEC: PlannedConnectorSpec = PlannedConnectorSpec::new(
    BOX_CONNECTOR_ID,
    "Box",
    PlannedConnectorCategory::Knowledge,
    &["oauth"],
    "Enterprise files, folders, and shared collections.",
    "Reviewed file updates after version checks.",
);

pub const FIGMA_SPEC: PlannedConnectorSpec = PlannedConnectorSpec::new(
    FIGMA_CONNECTOR_ID,
    "Figma",
    PlannedConnectorCategory::Knowledge,
    &["oauth", "personal-token"],
    "Design files, comments, components, and project metadata.",
    "Start read-only, then reviewed comment drafts.",
);

pub const ASANA_SPEC: PlannedConnectorSpec = PlannedConnectorSpec::new(
    ASANA_CONNECTOR_ID,
    "Asana",
    PlannedConnectorCategory::Hybrid,
    &["oauth", "personal-token"],
    "Projects, tasks, comments, sections, and status updates.",
    "Reviewed task field updates and comment drafts.",
);

pub const CLICKUP_SPEC: PlannedConnectorSpec = PlannedConnectorSpec::new(
    CLICKUP_CONNECTOR_ID,
    "ClickUp",
    PlannedConnectorCategory::Hybrid,
    &["oauth", "api-token"],
    "Spaces, folders, lists, tasks, docs, and comments.",
    "Reviewed task field updates and comment drafts.",
);

pub const ZENDESK_SPEC: PlannedConnectorSpec = PlannedConnectorSpec::new(
    ZENDESK_CONNECTOR_ID,
    "Zendesk",
    PlannedConnectorCategory::Hybrid,
    &["oauth", "api-token"],
    "Tickets, help-center articles, macros, users, and organizations.",
    "Reviewed ticket reply drafts and article edits.",
);

pub const INTERCOM_SPEC: PlannedConnectorSpec = PlannedConnectorSpec::new(
    INTERCOM_CONNECTOR_ID,
    "Intercom",
    PlannedConnectorCategory::Hybrid,
    &["oauth"],
    "Conversations, help articles, contacts, and companies.",
    "Reviewed conversation reply drafts and article edits.",
);

pub const HUBSPOT_SPEC: PlannedConnectorSpec = PlannedConnectorSpec::new(
    HUBSPOT_CONNECTOR_ID,
    "HubSpot",
    PlannedConnectorCategory::Hybrid,
    &["oauth", "api-token"],
    "CRM objects, notes, tasks, emails, deals, and companies.",
    "Reviewed note, task, and selected CRM field updates.",
);

pub const SALESFORCE_SPEC: PlannedConnectorSpec = PlannedConnectorSpec::new(
    SALESFORCE_CONNECTOR_ID,
    "Salesforce",
    PlannedConnectorCategory::Hybrid,
    &["oauth"],
    "Accounts, opportunities, cases, notes, tasks, and knowledge records.",
    "Reviewed note/task updates first; object writes require strict schema guards.",
);

pub const FHIR_SPEC: PlannedConnectorSpec = PlannedConnectorSpec::new(
    FHIR_CONNECTOR_ID,
    "FHIR",
    PlannedConnectorCategory::Knowledge,
    &["smart-oauth"],
    "Scoped FHIR resources as normalized read-only clinical context.",
    "Read-only until audit, consent, and safety workflows are designed.",
);

pub const PLANNED_CONNECTOR_SPECS: &[PlannedConnectorSpec] = &[
    JIRA_SPEC,
    SHAREPOINT_SPEC,
    ONEDRIVE_SPEC,
    OUTLOOK_MAIL_SPEC,
    OUTLOOK_CALENDAR_SPEC,
    MICROSOFT_TEAMS_SPEC,
    GOOGLE_DRIVE_SPEC,
    DROPBOX_SPEC,
    BOX_SPEC,
    FIGMA_SPEC,
    ASANA_SPEC,
    CLICKUP_SPEC,
    ZENDESK_SPEC,
    INTERCOM_SPEC,
    HUBSPOT_SPEC,
    SALESFORCE_SPEC,
    FHIR_SPEC,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlannedConnector {
    spec: &'static PlannedConnectorSpec,
    execution_policy: ConnectorExecutionPolicy,
}

impl PlannedConnector {
    pub fn new(id: &str) -> Option<Self> {
        planned_connector_spec(id).map(Self::from_spec)
    }

    pub fn from_spec(spec: &'static PlannedConnectorSpec) -> Self {
        Self {
            spec,
            execution_policy: ConnectorExecutionPolicy::Inline,
        }
    }

    pub fn spec(&self) -> &'static PlannedConnectorSpec {
        self.spec
    }

    pub fn execution_policy(&self) -> ConnectorExecutionPolicy {
        self.execution_policy
    }
}

pub fn planned_connector_spec(id: &str) -> Option<&'static PlannedConnectorSpec> {
    PLANNED_CONNECTOR_SPECS.iter().find(|spec| spec.id() == id)
}

pub fn planned_connector_ids() -> Vec<&'static str> {
    PLANNED_CONNECTOR_SPECS
        .iter()
        .map(PlannedConnectorSpec::id)
        .collect()
}

impl Connector for PlannedConnector {
    fn with_execution_policy(&self, policy: ConnectorExecutionPolicy) -> Self {
        Self {
            spec: self.spec,
            execution_policy: policy,
        }
    }

    fn kind(&self) -> ConnectorKind {
        ConnectorKind(self.spec.id())
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_oauth: self.spec.uses_oauth(),
            ..ConnectorCapabilities::default()
        }
    }

    fn supported_push_operations(&self) -> BTreeSet<PushOperationKind> {
        BTreeSet::new()
    }

    fn enumerate(&self, _request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        unsupported()
    }

    fn fetch(&self, _request: FetchRequest) -> LocalityResult<NativeEntity> {
        unsupported()
    }

    fn render(&self, _entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        unsupported()
    }

    fn parse(&self, _document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        unsupported()
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        unsupported()
    }

    fn apply(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        unsupported()
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        unsupported()
    }
}

fn unsupported<T>() -> LocalityResult<T> {
    Err(LocalityError::Unsupported(SCAFFOLD_UNSUPPORTED))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planned_connector_ids_are_stable() {
        assert_eq!(
            planned_connector_ids(),
            vec![
                "jira",
                "sharepoint",
                "onedrive",
                "outlook-mail",
                "outlook-calendar",
                "microsoft-teams",
                "google-drive",
                "dropbox",
                "box",
                "figma",
                "asana",
                "clickup",
                "zendesk",
                "intercom",
                "hubspot",
                "salesforce",
                "fhir",
            ]
        );
    }

    #[test]
    fn every_planned_connector_has_scaffold_metadata() {
        for spec in PLANNED_CONNECTOR_SPECS {
            assert!(!spec.id().is_empty());
            assert!(!spec.display_name().is_empty(), "{}", spec.id());
            assert!(!spec.auth_modes().is_empty(), "{}", spec.id());
            assert!(!spec.projection().is_empty(), "{}", spec.id());
            assert!(!spec.write_model().is_empty(), "{}", spec.id());
        }
    }

    #[test]
    fn planned_connector_implements_connector_but_blocks_remote_io() {
        let connector = PlannedConnector::new(JIRA_CONNECTOR_ID).expect("jira connector");

        assert_eq!(connector.kind().0, JIRA_CONNECTOR_ID);
        assert!(connector.capabilities().supports_oauth);
        assert!(connector.supported_push_operations().is_empty());
        assert!(matches!(
            connector.enumerate(EnumerateRequest {
                mount_id: locality_core::model::MountId::new("github-main"),
                cursor: None,
            }),
            Err(LocalityError::Unsupported(message))
                if message.contains("planned connector scaffold")
        ));
    }
}
