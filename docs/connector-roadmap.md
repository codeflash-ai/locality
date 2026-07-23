# Connector Roadmap

Locality connectors must preserve the product contract: remote systems become
local files and folders, while the source of truth remains protected by review,
conflict checks, and connector-owned concurrency guards.

This document tracks the connector catalog added to the desktop, daemon metadata
layer, runtime connector crates, and the `locality-planned-connectors` scaffold
crate. A connector in the planned catalog is visible as product direction and
exists as a compile-time connector scaffold, but it is not mountable until it
has a real provider client, credential resolver, rendering fixtures, sync tests,
and provider API coverage.

## Current Runtime Connectors

These connectors are registered in the daemon `SOURCE_REGISTRY` and can resolve
credentials in the current build.

| Connector | Type | Auth | Current write model |
| --- | --- | --- | --- |
| Notion | Knowledge | OAuth or token environment | Pages, databases, block edits, creates, moves, undo for supported plans |
| Google Docs | Knowledge | OAuth | Docs as Markdown, workspace-folder based mounting |
| Google Calendar | Action | OAuth | Read events, create reviewed drafts |
| Gmail | Action | OAuth | Read inbox/sent, create reviewed drafts |
| Confluence | Knowledge | Atlassian email plus API token | Read-only spaces and pages |
| GitHub | Hybrid | Personal access token | Read-only repositories, README files, issues, and pull requests |
| GitLab | Hybrid | Personal access token | Read-only projects, README files, issues, and merge requests |
| Granola | Knowledge | API key | Read-only summaries and transcripts |
| Linear | Hybrid | API key | Editable issue pages and supported issue moves |
| Slack | Knowledge | OAuth | Read-only accessible conversations |

## Planned Catalog

The planned catalog is intentionally separate from the runtime registry. Desktop
can show the direction in onboarding and Add Source, and daemon/tests can reason
about the catalog, without allowing unsupported providers to mount or push.
Each planned connector has a scaffolded source type, auth model, first
projection, and intended write model so implementation work starts from the
same product contract.

| Connector | Type | Auth options | First useful projection |
| --- | --- | --- | --- |
| Jira | Hybrid | OAuth, API token | Projects, issues, comments, sprints, and status context |
| SharePoint | Knowledge | OAuth | Sites, libraries, pages, and documents |
| OneDrive | Knowledge | OAuth | User and shared-drive files |
| Outlook Mail | Action | OAuth | Mail folders and reviewed draft creation |
| Outlook Calendar | Action | OAuth | Calendar events and reviewed scheduling drafts |
| Microsoft Teams | Knowledge | OAuth | Teams, channels, chats, meetings, and user context |
| Google Drive | Knowledge | OAuth | Drive files, PDFs, sheets, slides, and shared drives |
| Dropbox | Knowledge | OAuth | Shared files and folders |
| Box | Knowledge | OAuth | Enterprise files and folders |
| Figma | Knowledge | OAuth, personal token | Design files, comments, components, and product context |
| Asana | Hybrid | OAuth, personal token | Projects, tasks, comments, and status updates |
| ClickUp | Hybrid | OAuth, API token | Spaces, lists, tasks, docs, and comments |
| Zendesk | Hybrid | OAuth, API token | Tickets, help-center articles, macros, and customer context |
| Intercom | Hybrid | OAuth | Conversations, help articles, contacts, and support context |
| HubSpot | Hybrid | OAuth, API token | CRM records, notes, tasks, emails, deals, and customer context |
| Salesforce | Hybrid | OAuth | Accounts, opportunities, cases, notes, tasks, and CRM knowledge |
| FHIR | Knowledge | SMART OAuth | Scoped clinical resources projected for healthcare workflows |

## Official API References

These are the provider docs that should drive implementation. A connector should
not move out of the planned scaffold until its client, renderer, and E2E tests
are built against the relevant official API contract.

| Connector | Official docs to implement against |
| --- | --- |
| Confluence (runtime read-only v1) | Atlassian basic auth with account email/API token and Confluence Cloud REST API v2 spaces/pages |
| Jira | Atlassian OAuth 2.0 and Jira Cloud REST API v3 issue/search resources |
| SharePoint | Microsoft Graph auth, permissions, sites, lists, drives, and driveItem APIs |
| OneDrive | Microsoft Graph auth, permissions, drives, and driveItem APIs |
| Outlook Mail | Microsoft Graph auth, permissions, mail folders, and message APIs |
| Outlook Calendar | Microsoft Graph auth, permissions, calendars, and event APIs |
| Microsoft Teams | Microsoft Graph auth, permissions, Teams, channel, chat, and message APIs |
| GitHub (runtime read-only v1) | GitHub REST API, repository contents, issues, pull requests, and PAT auth |
| GitLab (runtime read-only v1) | GitLab REST API authentication, projects, repository files, issues, merge requests, and PAT auth |
| Google Drive | Google Drive API v3 files, drives, changes, comments, revisions, and permissions |
| Dropbox | Dropbox API v2 HTTP docs, OAuth, files, folders, sharing, and revisions |
| Box | Box API reference, OAuth 2.0, files, folders, versions, and collaborations |
| Figma | Figma REST API OpenAPI spec, OAuth, files, comments, components, and projects |
| Asana | Asana auth docs, OAuth/PAT, projects, tasks, comments, and sections |
| ClickUp | ClickUp auth docs, OAuth/personal token, spaces, folders, lists, tasks, docs |
| Zendesk | Zendesk OAuth/API-token docs, tickets, users, organizations, macros, help center |
| Intercom | Intercom REST/OpenAPI docs, OAuth, conversations, contacts, companies, articles |
| HubSpot | HubSpot OAuth and CRM object APIs for contacts, companies, deals, notes, tasks |
| Salesforce | Salesforce REST API OAuth/connected apps and object/record APIs |
| FHIR | HL7 SMART App Launch and SMART on FHIR scopes/launch context |

## Promotion Bar

A planned connector can move into the runtime registry only when it has all of
the following:

1. A first-party connector crate implementing `Connector`.
2. Credential setup through desktop and CLI, with profile/account records and
   credential-store backed secrets.
3. A conservative network policy using the shared network gate.
4. Canonical Markdown rendering fixtures and exact expected output tests.
5. Parser and validation tests for every editable field, or an explicit
   read-only policy that blocks parse/apply/push.
6. Batch observation or explicit documented fallback behavior.
7. Push-plan tests for every supported mutation, if writes are supported.
8. Conflict/concurrency tests using stale remote observations, if writes are
   supported.
9. A source descriptor with mount guidance, write policy, body diff mode, and
   virtual rename policy.
10. One end-to-end test that connects, mounts, hydrates, edits where supported,
    plans review, pushes where supported, and verifies local state reconciliation.

Until every item above exists, the connector should stay visible as planned
metadata only. It can live in `locality-planned-connectors`, but it must not be
added to `SOURCE_REGISTRY`, desktop runtime setup, or automatic mount creation.

## Implementation Order

The highest leverage next connectors are:

1. Jira, because it pairs naturally with engineering status workflows.
2. SharePoint and OneDrive, because Microsoft 365 content is common in larger
   teams.
3. Google Drive, because Drive folders and files complete the Google workspace
   story beyond Docs, Calendar, and Gmail.
4. Zendesk or Intercom, because support workflows benefit from local search,
   review, and safe response drafts.

Healthcare-specific work should start with a FHIR read-only connector and a
strict scope model. Write support should remain out of scope until the audit,
consent, and patient-safety model is designed.
