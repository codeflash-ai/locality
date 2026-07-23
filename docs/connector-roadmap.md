# Connector Roadmap

Locality connectors must preserve the product contract: remote systems become
local files and folders, while the source of truth remains protected by review,
conflict checks, and connector-owned concurrency guards.

This document tracks the connector catalog added to the desktop, daemon metadata
layer, and `locality-planned-connectors` scaffold crate. A connector in the
planned catalog is visible as product direction and exists as a compile-time
connector scaffold, but it is not mountable until it has a real provider client,
credential resolver, rendering/parsing fixtures, sync tests, and provider API
coverage.

## Current Runtime Connectors

These connectors are registered in the daemon `SOURCE_REGISTRY` and can resolve
credentials in the current build.

| Connector | Type | Auth | Current write model |
| --- | --- | --- | --- |
| Notion | Knowledge | OAuth or token environment | Pages, databases, block edits, creates, moves, undo for supported plans |
| Google Docs | Knowledge | OAuth | Docs as Markdown, workspace-folder based mounting |
| Google Calendar | Action | OAuth | Read events, create reviewed drafts |
| Gmail | Action | OAuth | Read inbox/sent, create reviewed drafts |
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
| Confluence | Knowledge | OAuth, API token | Spaces and pages as folders with `page.md` bodies |
| Jira | Hybrid | OAuth, API token | Projects, issues, comments, sprints, and status context |
| SharePoint | Knowledge | OAuth | Sites, libraries, pages, and documents |
| OneDrive | Knowledge | OAuth | User and shared-drive files |
| Outlook Mail | Action | OAuth | Mail folders and reviewed draft creation |
| Outlook Calendar | Action | OAuth | Calendar events and reviewed scheduling drafts |
| Microsoft Teams | Knowledge | OAuth | Teams, channels, chats, meetings, and user context |
| GitHub | Hybrid | OAuth, GitHub App, personal token | Repos, PRs, issues, discussions, and reviews |
| GitLab | Hybrid | OAuth, personal token | Projects, MRs, issues, pipelines, and releases |
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

## Promotion Bar

A planned connector can move into the runtime registry only when it has all of
the following:

1. A first-party connector crate implementing `Connector`.
2. Credential setup through desktop and CLI, with profile/account records and
   credential-store backed secrets.
3. A conservative network policy using the shared network gate.
4. Canonical Markdown rendering fixtures and exact expected output tests.
5. Parser and validation tests for every editable field.
6. Batch observation or explicit documented fallback behavior.
7. Push-plan tests for every supported mutation.
8. Conflict/concurrency tests using stale remote observations.
9. A source descriptor with mount guidance, write policy, body diff mode, and
   virtual rename policy.
10. One end-to-end test that connects, mounts, hydrates, edits where supported,
    plans review, pushes where supported, and verifies local state reconciliation.

Until every item above exists, the connector should stay visible as planned
metadata only. It can live in `locality-planned-connectors`, but it must not be
added to `SOURCE_REGISTRY`, desktop runtime setup, or automatic mount creation.

## Implementation Order

The highest leverage next connectors are:

1. Confluence, because it is the closest enterprise wiki analogue to Notion.
2. Jira, because it pairs naturally with engineering status workflows.
3. SharePoint and OneDrive, because Microsoft 365 content is common in larger
   teams.
4. GitHub, because repository and PR context can become a first-class local
   knowledge source instead of only shell/git context.
5. Zendesk or Intercom, because support workflows benefit from local search,
   review, and safe response drafts.

Healthcare-specific work should start with a FHIR read-only connector and a
strict scope model. Write support should remain out of scope until the audit,
consent, and patient-safety model is designed.
