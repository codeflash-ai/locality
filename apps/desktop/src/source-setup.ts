import { classifyMountSetupError } from "./onboarding-errors";

export type SourceSetupState = "idle" | "connecting" | "creating" | "changing" | "success" | "error";
const SOURCE_CONNECTORS = [
  "notion",
  "google-docs",
  "google-calendar",
  "gmail",
  "granola",
  "confluence",
  "github",
  "gitlab",
  "linear",
  "slack",
] as const;
const PLANNED_SOURCE_CONNECTORS = [
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
] as const;

export type SourceConnectorId = (typeof SOURCE_CONNECTORS)[number];
export type PlannedSourceConnectorId = (typeof PLANNED_SOURCE_CONNECTORS)[number];
export type SourceCatalogConnectorId = SourceConnectorId | PlannedSourceConnectorId;
export type ApiKeySourceConnectorId = Extract<SourceConnectorId, "confluence" | "github" | "gitlab" | "granola" | "linear">;
export type SourceConnectorAvailability = "implemented" | "planned";
export type SourceConnectorCategory = "knowledge" | "action" | "hybrid";
export type SourceConnectorAuthMode = "oauth" | "api-key" | "api-token" | "personal-token" | "github-app" | "smart-oauth";

export type SourceConnectorDefinition<Id extends SourceCatalogConnectorId = SourceCatalogConnectorId> = {
  id: Id;
  name: string;
  description: string;
  availability: SourceConnectorAvailability;
  category: SourceConnectorCategory;
  authModes: readonly SourceConnectorAuthMode[];
  keywords: readonly string[];
  projection: string;
  writeModel: string;
  defaultMountId?: string;
  defaultMountDirectory?: string;
};

const SOURCE_CONNECTOR_DEFINITIONS: readonly SourceConnectorDefinition<SourceConnectorId>[] = [
  {
    id: "notion",
    name: "Notion",
    description: "Pages and databases as folders with page.md files.",
    availability: "implemented",
    category: "knowledge",
    authModes: ["oauth"],
    keywords: ["notion", "wiki", "pages", "database", "docs"],
    projection: "Pages and databases as folders with page.md files.",
    writeModel: "Reviewed page, block, database row, and move updates.",
    defaultMountId: "notion-main",
    defaultMountDirectory: "notion",
  },
  {
    id: "google-docs",
    name: "Google Docs",
    description: "Docs and Drive folders through the same local file workflow.",
    availability: "implemented",
    category: "knowledge",
    authModes: ["oauth"],
    keywords: ["google", "docs", "gdocs", "drive", "documents"],
    projection: "Docs and configured Drive folders as Markdown files.",
    writeModel: "Reviewed document body updates.",
    defaultMountId: "google-docs-main",
    defaultMountDirectory: "google-docs-main",
  },
  {
    id: "google-calendar",
    name: "Google Calendar",
    description: "Primary calendar events as files, new events from reviewed drafts.",
    availability: "implemented",
    category: "action",
    authModes: ["oauth"],
    keywords: ["google", "calendar", "gcal", "events", "meet"],
    projection: "Primary calendar events plus a draft folder.",
    writeModel: "Reviewed event creates from draft files.",
    defaultMountId: "google-calendar-main",
    defaultMountDirectory: "google-calendar-main",
  },
  {
    id: "gmail",
    name: "Gmail",
    description: "Inbox and sent as readable files, drafts as reviewed outbound mail.",
    availability: "implemented",
    category: "action",
    authModes: ["oauth"],
    keywords: ["gmail", "mail", "email", "inbox", "drafts"],
    projection: "Inbox, sent mail, and draft files.",
    writeModel: "Reviewed outbound mail creates from draft files.",
    defaultMountId: "gmail-main",
    defaultMountDirectory: "gmail-main",
  },
  {
    id: "granola",
    name: "Granola",
    description: "Meeting summaries and raw transcripts as read-only files.",
    availability: "implemented",
    category: "knowledge",
    authModes: ["api-key"],
    keywords: ["granola", "meetings", "notes", "transcripts", "summaries"],
    projection: "Meetings with summary.md and transcript.md files.",
    writeModel: "Read-only.",
    defaultMountId: "granola-main",
    defaultMountDirectory: "granola",
  },
  {
    id: "confluence",
    name: "Confluence",
    description: "Spaces and pages as read-only local knowledge files.",
    availability: "implemented",
    category: "knowledge",
    authModes: ["api-key", "api-token"],
    keywords: ["atlassian", "confluence", "wiki", "spaces", "pages"],
    projection: "Spaces and pages as folders with page.md bodies.",
    writeModel: "Read-only.",
    defaultMountId: "confluence-main",
    defaultMountDirectory: "confluence",
  },
  {
    id: "github",
    name: "GitHub",
    description: "Repositories, README files, issues, and pull requests as read-only context.",
    availability: "implemented",
    category: "hybrid",
    authModes: ["api-key", "personal-token"],
    keywords: ["github", "git", "repos", "repositories", "pull requests", "issues"],
    projection: "Repositories, README files, issues, and pull requests.",
    writeModel: "Read-only; repository edits stay in git.",
    defaultMountId: "github-main",
    defaultMountDirectory: "github",
  },
  {
    id: "gitlab",
    name: "GitLab",
    description: "Projects, README files, issues, and merge requests as read-only context.",
    availability: "implemented",
    category: "hybrid",
    authModes: ["api-key", "personal-token"],
    keywords: ["gitlab", "git", "repos", "repositories", "merge requests", "issues"],
    projection: "Projects, README files, issues, and merge requests.",
    writeModel: "Read-only; repository edits stay in git.",
    defaultMountId: "gitlab-main",
    defaultMountDirectory: "gitlab",
  },
  {
    id: "linear",
    name: "Linear",
    description: "Issues and teams as editable Markdown files.",
    availability: "implemented",
    category: "hybrid",
    authModes: ["api-key"],
    keywords: ["linear", "issues", "tickets", "projects", "teams"],
    projection: "Teams, issues, status folders, and issue page.md files.",
    writeModel: "Reviewed issue body and supported frontmatter updates.",
    defaultMountId: "linear-main",
    defaultMountDirectory: "linear",
  },
  {
    id: "slack",
    name: "Slack",
    description: "Recent accessible conversations as read-only Markdown.",
    availability: "implemented",
    category: "knowledge",
    authModes: ["oauth"],
    keywords: ["slack", "channels", "private channels", "dms", "group dms", "users"],
    projection: "Channels, DMs, group DMs, and users as Markdown context.",
    writeModel: "Read-only.",
    defaultMountId: "slack-main",
    defaultMountDirectory: "slack",
  },
];

const PLANNED_SOURCE_CONNECTOR_DEFINITIONS: readonly SourceConnectorDefinition<PlannedSourceConnectorId>[] = [
  {
    id: "jira",
    name: "Jira",
    description: "Projects, issues, comments, and sprint context for planning workflows.",
    availability: "planned",
    category: "hybrid",
    authModes: ["oauth", "api-token"],
    keywords: ["atlassian", "jira", "issues", "sprints", "projects"],
    projection: "Projects, issues, comments, and sprint folders.",
    writeModel: "Reviewed issue body, status, assignee, and comment drafts.",
  },
  {
    id: "sharepoint",
    name: "SharePoint",
    description: "Team sites, libraries, pages, and enterprise documents.",
    availability: "planned",
    category: "knowledge",
    authModes: ["oauth"],
    keywords: ["microsoft", "sharepoint", "sites", "libraries", "documents"],
    projection: "Sites, document libraries, pages, and files.",
    writeModel: "Start read-only, then reviewed document updates where safe.",
  },
  {
    id: "onedrive",
    name: "OneDrive",
    description: "User and team files projected into agent-friendly folders.",
    availability: "planned",
    category: "knowledge",
    authModes: ["oauth"],
    keywords: ["microsoft", "onedrive", "files", "drive", "documents"],
    projection: "User and shared files as folder hierarchies.",
    writeModel: "Reviewed file updates after version checks.",
  },
  {
    id: "outlook-mail",
    name: "Outlook Mail",
    description: "Mailbox context with reviewed draft creation for outbound updates.",
    availability: "planned",
    category: "action",
    authModes: ["oauth"],
    keywords: ["microsoft", "outlook", "mail", "email", "drafts"],
    projection: "Mailbox folders plus reviewed draft files.",
    writeModel: "Reviewed outbound mail creates from draft files.",
  },
  {
    id: "outlook-calendar",
    name: "Outlook Calendar",
    description: "Calendar events and scheduling drafts for Microsoft 365 teams.",
    availability: "planned",
    category: "action",
    authModes: ["oauth"],
    keywords: ["microsoft", "outlook", "calendar", "events", "meetings"],
    projection: "Calendar events plus scheduling drafts.",
    writeModel: "Reviewed event creates and updates after conflict checks.",
  },
  {
    id: "microsoft-teams",
    name: "Microsoft Teams",
    description: "Channels, chats, meetings, and team knowledge for enterprise collaboration.",
    availability: "planned",
    category: "knowledge",
    authModes: ["oauth"],
    keywords: ["microsoft", "teams", "channels", "chats", "meetings"],
    projection: "Teams, channels, chats, meetings, and users as context.",
    writeModel: "Start read-only, then reviewed message drafts if approved.",
  },
  {
    id: "google-drive",
    name: "Google Drive",
    description: "Drive files, shared folders, PDFs, sheets, and presentations.",
    availability: "planned",
    category: "knowledge",
    authModes: ["oauth"],
    keywords: ["google", "drive", "files", "shared drives", "docs"],
    projection: "Drive files, folders, shared drives, PDFs, sheets, and slides.",
    writeModel: "Reviewed file updates where a canonical representation is safe.",
  },
  {
    id: "dropbox",
    name: "Dropbox",
    description: "Shared team files and folder hierarchies for document workflows.",
    availability: "planned",
    category: "knowledge",
    authModes: ["oauth"],
    keywords: ["dropbox", "files", "folders", "documents", "team"],
    projection: "Shared files and folders.",
    writeModel: "Reviewed file updates after revision checks.",
  },
  {
    id: "box",
    name: "Box",
    description: "Enterprise file libraries with permission-aware local projection.",
    availability: "planned",
    category: "knowledge",
    authModes: ["oauth"],
    keywords: ["box", "files", "folders", "enterprise", "documents"],
    projection: "Enterprise files, folders, and shared collections.",
    writeModel: "Reviewed file updates after version checks.",
  },
  {
    id: "figma",
    name: "Figma",
    description: "Design files, comments, components, and product design context.",
    availability: "planned",
    category: "knowledge",
    authModes: ["oauth", "personal-token"],
    keywords: ["figma", "design", "comments", "components", "files"],
    projection: "Design files, comments, components, and project metadata.",
    writeModel: "Start read-only, then reviewed comment drafts.",
  },
  {
    id: "asana",
    name: "Asana",
    description: "Projects, tasks, comments, and status updates for team workflows.",
    availability: "planned",
    category: "hybrid",
    authModes: ["oauth", "personal-token"],
    keywords: ["asana", "tasks", "projects", "status", "workflows"],
    projection: "Projects, tasks, comments, sections, and status updates.",
    writeModel: "Reviewed task field updates and comment drafts.",
  },
  {
    id: "clickup",
    name: "ClickUp",
    description: "Spaces, lists, tasks, docs, and comments for operating teams.",
    availability: "planned",
    category: "hybrid",
    authModes: ["oauth", "api-token"],
    keywords: ["clickup", "tasks", "docs", "lists", "spaces"],
    projection: "Spaces, folders, lists, tasks, docs, and comments.",
    writeModel: "Reviewed task field updates and comment drafts.",
  },
  {
    id: "zendesk",
    name: "Zendesk",
    description: "Tickets, help-center articles, macros, and support context.",
    availability: "planned",
    category: "hybrid",
    authModes: ["oauth", "api-token"],
    keywords: ["zendesk", "support", "tickets", "help center", "macros"],
    projection: "Tickets, help-center articles, macros, users, and organizations.",
    writeModel: "Reviewed ticket reply drafts and article edits.",
  },
  {
    id: "intercom",
    name: "Intercom",
    description: "Conversations, help articles, contacts, and support workflows.",
    availability: "planned",
    category: "hybrid",
    authModes: ["oauth"],
    keywords: ["intercom", "support", "conversations", "help center", "contacts"],
    projection: "Conversations, help articles, contacts, and companies.",
    writeModel: "Reviewed conversation reply drafts and article edits.",
  },
  {
    id: "hubspot",
    name: "HubSpot",
    description: "CRM records, notes, tasks, emails, deals, and customer context.",
    availability: "planned",
    category: "hybrid",
    authModes: ["oauth", "api-token"],
    keywords: ["hubspot", "crm", "contacts", "deals", "tasks"],
    projection: "CRM objects, notes, tasks, emails, deals, and companies.",
    writeModel: "Reviewed note, task, and selected CRM field updates.",
  },
  {
    id: "salesforce",
    name: "Salesforce",
    description: "Accounts, opportunities, cases, notes, tasks, and CRM knowledge.",
    availability: "planned",
    category: "hybrid",
    authModes: ["oauth"],
    keywords: ["salesforce", "crm", "accounts", "opportunities", "cases"],
    projection: "Accounts, opportunities, cases, notes, tasks, and knowledge records.",
    writeModel: "Reviewed note/task updates first; object writes require strict schema guards.",
  },
  {
    id: "fhir",
    name: "FHIR",
    description: "Healthcare records through SMART on FHIR profiles and scoped access.",
    availability: "planned",
    category: "knowledge",
    authModes: ["smart-oauth"],
    keywords: ["fhir", "smart", "healthcare", "clinical", "ehr"],
    projection: "Scoped FHIR resources as normalized read-only clinical context.",
    writeModel: "Read-only until audit, consent, and safety workflows are designed.",
  },
];

export type SourceMountRetryOutcome =
  | { kind: "retry" }
  | { kind: "success" | "error"; message: string };

type SourceConnectionLike = {
  connector: string;
  status: string;
};

type SourceMountLike = {
  connector: string;
  status?: string | null;
};

type SourceSnapshotLike = {
  connection?: SourceConnectionLike | null;
  connections?: SourceConnectionLike[] | null;
  mount?: SourceMountLike | null;
  mounts?: SourceMountLike[] | null;
};

export function sourceConnectorIds(): SourceConnectorId[] {
  return [...SOURCE_CONNECTORS];
}

export function plannedSourceConnectorIds(): PlannedSourceConnectorId[] {
  return [...PLANNED_SOURCE_CONNECTORS];
}

export function sourceConnectorDefinitions(): SourceConnectorDefinition<SourceConnectorId>[] {
  return SOURCE_CONNECTOR_DEFINITIONS.map((definition) => ({ ...definition }));
}

export function plannedSourceConnectorDefinitions(): SourceConnectorDefinition<PlannedSourceConnectorId>[] {
  return PLANNED_SOURCE_CONNECTOR_DEFINITIONS.map((definition) => ({ ...definition }));
}

export function sourceConnectorCatalogDefinitions(): SourceConnectorDefinition[] {
  return [
    ...sourceConnectorDefinitions(),
    ...plannedSourceConnectorDefinitions(),
  ];
}

export function sourceConnectorCatalogIds(): SourceCatalogConnectorId[] {
  return [
    ...SOURCE_CONNECTORS,
    ...PLANNED_SOURCE_CONNECTORS,
  ];
}

export function sourceConnectorDefinition(connector: SourceConnectorId): SourceConnectorDefinition<SourceConnectorId> {
  const definition = SOURCE_CONNECTOR_DEFINITIONS.find((candidate) => candidate.id === connector);
  if (!definition) {
    throw new Error(`Unknown source connector: ${connector}`);
  }
  return { ...definition };
}

export function sourceCatalogConnectorDefinition(connector: SourceCatalogConnectorId): SourceConnectorDefinition {
  const definition = sourceConnectorCatalogDefinitions().find((candidate) => candidate.id === connector);
  if (!definition) {
    throw new Error(`Unknown source connector: ${connector}`);
  }
  return definition;
}

export function sourceConnectorName(connector: SourceConnectorId): string {
  return sourceConnectorDefinition(connector).name;
}

export function sourceConnectorDefaultMountId(connector: SourceConnectorId): string {
  return sourceConnectorDefinition(connector).defaultMountId ?? `${connector}-main`;
}

export function sourceConnectorDefaultMountDirectory(connector: SourceConnectorId): string {
  return sourceConnectorDefinition(connector).defaultMountDirectory ?? sourceConnectorDefaultMountId(connector);
}

export function sourceRequiresApiKey(connector: SourceConnectorId): connector is ApiKeySourceConnectorId {
  return sourceConnectorDefinition(connector).authModes.includes("api-key");
}

export function sourceSkipsManualMountStep(connector: SourceConnectorId): boolean {
  return connector !== "notion";
}

export function sourceMountRetryOutcome(
  report: { ok: boolean; message: string },
): SourceMountRetryOutcome {
  if (report.ok) {
    return { kind: "success", message: report.message };
  }
  if (classifyMountSetupError(report.message).kind === "file-provider-disabled") {
    return { kind: "retry" };
  }
  return { kind: "error", message: report.message };
}

export function sourceSetupIsBusy(state: SourceSetupState): boolean {
  return state === "connecting" || state === "creating" || state === "changing";
}

export function sourceSetupIsActiveConnector(
  state: SourceSetupState,
  activeConnector: SourceConnectorId | null,
  connector: SourceConnectorId,
): boolean {
  return sourceSetupIsBusy(state) && activeConnector === connector;
}

export function sourceSetupProgressLabel(state: SourceSetupState, mounted: boolean): string {
  if (state === "changing") {
    return "Updating access";
  }
  if (mounted) {
    return "Finishing setup";
  }
  if (state === "creating") {
    return "Mounting";
  }
  if (state === "connecting") {
    return "Connecting";
  }
  return "";
}

export function isSourceConnectorId(value: string): value is SourceConnectorId {
  return SOURCE_CONNECTORS.includes(value as SourceConnectorId);
}

export function isSourceCatalogConnectorId(value: string): value is SourceCatalogConnectorId {
  return sourceConnectorCatalogIds().includes(value as SourceCatalogConnectorId);
}

export function sourceConnectionReady(
  snapshot: SourceSnapshotLike,
  connector: SourceConnectorId,
): boolean {
  return sourceConnections(snapshot).some(
    (connection) => connection.connector === connector && sourceConnectionStatusReady(connection.status),
  );
}

export function sourceMounted(
  snapshot: SourceSnapshotLike,
  connector: SourceConnectorId,
): boolean {
  return sourceMounts(snapshot).some(
    (mount) => mount.connector === connector && sourceMountStatusMounted(mount.status),
  );
}

export function connectedSourcesReadyToMount(snapshot: SourceSnapshotLike): SourceConnectorId[] {
  return SOURCE_CONNECTORS.filter(
    (connector) => sourceConnectionReady(snapshot, connector) && !sourceMounted(snapshot, connector),
  );
}

function sourceConnections(snapshot: SourceSnapshotLike): SourceConnectionLike[] {
  const byConnector = new Map<string, SourceConnectionLike>();
  for (const connection of snapshot.connections ?? []) {
    if (connection?.connector) {
      byConnector.set(connection.connector, connection);
    }
  }
  if (snapshot.connection?.connector && !byConnector.has(snapshot.connection.connector)) {
    byConnector.set(snapshot.connection.connector, snapshot.connection);
  }
  return Array.from(byConnector.values());
}

function sourceMounts(snapshot: SourceSnapshotLike): SourceMountLike[] {
  const mounts = [...(snapshot.mounts ?? [])];
  if (snapshot.mount?.connector && !mounts.some((mount) => mount.connector === snapshot.mount?.connector)) {
    mounts.push(snapshot.mount);
  }
  return mounts;
}

function sourceConnectionStatusReady(status: string): boolean {
  const normalized = status.trim().toLowerCase();
  return normalized === "active" || normalized === "ready";
}

function sourceMountStatusMounted(status?: string | null): boolean {
  const normalized = status?.trim().toLowerCase() ?? "";
  return normalized !== "not_mounted" && normalized !== "reconnect_needed";
}
