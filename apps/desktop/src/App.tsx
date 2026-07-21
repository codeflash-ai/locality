import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { relaunch } from "@tauri-apps/plugin-process";
import { check, type Update } from "@tauri-apps/plugin-updater";
import {
  AlertTriangle,
  Bot,
  Check,
  ChevronRight,
  ChevronUp,
  Clipboard,
  Clock3,
  Code2,
  Copy,
  Download,
  EyeOff,
  FolderOpen,
  Home,
  LayoutGrid,
  ListChecks,
  List,
  Loader2,
  Minus,
  Monitor,
  Moon,
  Plus,
  Power,
  RefreshCw,
  RotateCcw,
  Search,
  Settings,
  ShieldCheck,
  Square,
  Sun,
  PanelLeftClose,
  PanelLeftOpen,
  Trash2,
  Zap,
  X,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import {
  compactPath,
  mountEntityCountLabel,
  mountFileIndexProgressLabel,
  mountFileIndexProgressValue,
  mountAccessLabel,
  mountRows,
  mountStatusLabel,
  mountStatusTone,
  selectedMountIdAfterOpenViewEvent,
  selectedMountIdAfterViewChange,
  selectedMountRow,
  sourceDestructiveConfirmation,
  sourceDestructiveConfirmationMatches,
  type MountRow,
  type MountSummary,
  type ProviderRuntimeSummary,
  type SourceDestructiveAction,
} from "./mounts";
import { connectionMissing, connectionReady } from "./connection-state";
import { copyLoginLinkDisabled, loginLinkFlowMode } from "./onboarding-connect";
import { mountRecoveryEnabled, shouldAutoCreateMount } from "./onboarding-flow";
import { classifyMountSetupError } from "./onboarding-errors";
import {
  createFileProviderEnablementPoller,
  fileProviderEnablementHeadline,
  fileProviderEnablementStatusLabel,
  type FileProviderEnablementReport,
} from "./file-provider-enablement";
import {
  failedMountOnboardingReport,
  mountOnboardingHeadline,
  mountOnboardingInstructions,
  mountOnboardingNeedsInstructions,
  mountOnboardingNextAction,
  mountOnboardingPrimaryLabel,
  mountOnboardingSupplementaryNote,
  type WorkspaceMountOnboardingReport,
} from "./onboarding-mount";
import {
  availableUpdateStatus,
  downloadedUpdateMessage,
  emptyUpdateStatus,
  installingUpdateMessage,
  nextUpdateDownloadProgress,
  updateDownloadMessage,
  updateErrorMessage,
  updateInstallActionDisabled,
  updateInstallActionLabel,
  updateNoticeVisible,
  updateSidebarSubtitle,
  updateSidebarTitle,
  queueIdleForAutoUpdateInstall,
  updateStatusLabel,
  type UpdateStatus,
} from "./updater";
import {
  connectedSourcesReadyToMount,
  isSourceConnectorId,
  sourceConnectionReady,
  sourceConnectorIds,
  sourceMountRetryOutcome,
  sourceMounted,
  sourceRequiresApiKey,
  sourceSkipsManualMountStep,
  sourceSetupIsActiveConnector,
  sourceSetupIsBusy,
  sourceSetupProgressLabel,
  type SourceConnectorId,
  type SourceSetupState,
} from "./source-setup";
import gmailIconUrl from "./assets/connectors/gmail.svg";
import googleCalendarIconUrl from "./assets/connectors/google-calendar.svg";
import googleDocsIconUrl from "./assets/connectors/google-docs.svg";
import granolaIconUrl from "./assets/connectors/granola.svg";
import linearIconUrl from "./assets/connectors/linear.svg";
import notionIconUrl from "./assets/connectors/notion.svg";
import slackIconUrl from "./assets/connectors/slack.svg";
import localityShortDarkUrl from "./assets/brand/locality-short-dark.svg";
import localityShortLightUrl from "./assets/brand/locality-short-light.svg";

const distributionChannel = (import.meta.env.VITE_LOCALITY_DISTRIBUTION_CHANNEL || "direct").toLowerCase();
const appStoreDistribution = distributionChannel === "mas";
const onboardingDemoVideoUrl = import.meta.env.VITE_LOCALITY_ONBOARDING_DEMO_VIDEO_URL?.trim() || "";

type AppView = "home" | "files" | "mount" | "pending" | "review" | "activity" | "settings";
type LocateState = "idle" | "preparing" | "ready" | "error";
type OnboardingStep = 1 | 2 | 3 | 4 | 5;
type OnboardingConnectorId = SourceConnectorId;
type ReviewFilter = "all" | "approvals" | "problems";
type FileStatusFilter = "all" | "review" | "conflict" | "synced";
type DestructiveSettingsAction = "reset" | "uninstall";
type SettingsSection = "general" | "sources" | "sync" | "activity" | "agents" | "advanced" | "about";
type SourceListViewMode = "list" | "tiles";
type AppTheme = "system" | "light" | "dark";

const APP_THEME_STORAGE_KEY = "locality.desktop.theme";

type ConnectorOption = {
  id: SourceConnectorId;
  name: string;
  description: string;
  status: string;
  keywords: string[];
  mounted: boolean;
};

const CONNECTOR_ICON_URLS: Record<SourceConnectorId, string> = {
  notion: notionIconUrl,
  "google-docs": googleDocsIconUrl,
  "google-calendar": googleCalendarIconUrl,
  gmail: gmailIconUrl,
  granola: granolaIconUrl,
  linear: linearIconUrl,
  slack: slackIconUrl,
};

const PRODUCT_TERMS = {
  home: "Home",
  files: "Files",
  sources: "Sources",
  sourceDetail: "Source detail",
  reviewCenter: "Review Center",
  pushApproval: "Push approval",
  activity: "Activity",
  settings: "Settings",
  liveMode: "Live Mode",
  connectedSource: "Connected source",
  localWorkspace: "Local workspace",
  reviewNeeded: "Needs review",
} as const;

type DesktopSnapshot = {
  health: {
    state: string;
    attentionCount: number;
  };
  connection: ConnectionSummary;
  connections?: ConnectionSummary[];
  mount: MountSummary;
  mounts: MountSummary[];
  activeMountId?: string | null;
  liveMode: MountLiveMode;
  needsOnboarding: boolean;
  settings: {
    launchAtLogin: boolean;
    showMenuBar: boolean;
  };
  pendingChanges: PendingChange[];
  recentFiles: LocatedItem[];
  activity: ActivityItem[];
  suggestions: ConnectorSuggestion[];
};

type ConnectionSummary = {
  connector: string;
  workspaceName: string;
  accountLabel: string;
  status: string;
};

type MountLiveMode = {
  enabled: boolean;
  state: "off" | "active" | "syncing" | "error";
  label: string;
  reason?: string | null;
  lastRunAt?: string | null;
  pendingCount: number;
  reviewCount: number;
  coveredCount: number;
};

type PendingChange = {
  mountId: string;
  entityId: string;
  title: string;
  localPath: string;
  summary: string;
  state: "safe" | "needs_review" | "conflict" | "blocked";
  issueCodes: string[];
  liveMode: {
    enabled: boolean;
    state: "off" | "active" | "blocked" | "paused_remote_changed" | "paused_failure";
    label: string;
    reason?: string | null;
  };
};

type ActivityItem = {
  title: string;
  detail: string;
  when: string;
  occurredAt?: string | null;
  kind: string;
};

type DebugQueueStatus = {
  generatedAtUnixMs: number;
  active: DebugQueueActive[];
  sections: DebugQueueSection[];
  schedulerMode: string;
  activeIntervalMs: number;
  coldIntervalMs: number;
  liveMode: DebugLiveModeStatus;
};

type DebugLiveModeStatus = {
  mountId?: string | null;
  enabled: boolean;
  state: string;
  label: string;
  reason?: string | null;
  lastRunAt?: string | null;
  trackedFiles: DebugLiveModeFile[];
};

type DebugLiveModeFile = {
  path: string;
  title: string;
  remoteId: string;
  hydration: string;
  status: string;
  syncState: string;
  activeForPolling: boolean;
  remoteCheckDue: boolean;
  pollingReason?: string | null;
  freshnessTier?: string | null;
  lastCheckedAt?: string | null;
  lastOpenedAt?: string | null;
  lastLocalChangeAt?: string | null;
  remoteHintPending: boolean;
  autoSaveState?: string | null;
  autoSaveReason?: string | null;
  issueCodes: string[];
};

type DebugQueueActive = {
  kind: string;
  target?: string | null;
  elapsedMs: number;
  startedAtUnixMs: number;
};

type DebugQueueSection = {
  name: string;
  label: string;
  total: number;
  ready?: number | null;
  deferred?: number | null;
  items: DebugQueueItem[];
};

type DebugQueueItem = {
  kind: string;
  target?: string | null;
  mountId?: string | null;
  remoteId?: string | null;
  path?: string | null;
  reason?: string | null;
  priority?: string | null;
  nextEligibleAt?: string | null;
};

type ConnectorSuggestion = {
  connector: string;
  description: string;
  state: string;
};

type LocatedItem = {
  title: string;
  kind: string;
  localPath: string;
  state:
    | "ready"
    | "online_only"
    | "pending_changes"
    | "conflict"
    | "remote_update_available"
    | "preparing"
    | "no_access"
    | "not_found";
};

type PushPlan = {
  title: string;
  summary: string;
  pagesUpdated: number;
  databaseRowsUpdated: number;
  pagesDeleted: number;
  canPush: boolean;
  guardrailState: string;
  files: PendingChange[];
};

type ActionReport = {
  ok: boolean;
  message: string;
};

type InstallStateReview = {
  shouldPrompt: boolean;
  stateExists: boolean;
  sqliteExists: boolean;
  previousBuildId?: string | null;
  currentBuildId: string;
};

type AppUpdateCheckOptions = {
  silent?: boolean;
  autoDownload?: boolean;
  installWhenHidden?: boolean;
};

type FileDetailReport = {
  ok: boolean;
  path: string;
  hasConflictMarkers: boolean;
  conflictPreview?: string | null;
  message: string;
};

type FileEditorReport = {
  ok: boolean;
  path: string;
  contents: string;
  hasConflictMarkers: boolean;
  message: string;
};

type AgentGuidanceStatus = "installed" | "available" | "failed";

type AgentGuidanceTarget = {
  agent: string;
  status: AgentGuidanceStatus;
  path?: string | null;
  detail: string;
};

type AgentGuidanceInstallReport = {
  ok: boolean;
  command: string;
  targets: AgentGuidanceTarget[];
  prompt: string;
};

const sampleMount: MountSummary = {
  mountId: "notion-main",
  connector: "notion",
  connectorName: "Notion",
  connectionId: "notion-main",
  workspaceName: "CodeFlash",
  localPath: "~/Library/CloudStorage/Locality/notion",
  notionUrl: "https://www.notion.so/37b3ac0ebb88802cbcf4d53c9cfc4972",
  accessScope: "Initial Idea",
  remoteRootId: "37b3ac0ebb88802cbcf4d53c9cfc4972",
  projection: "macOS File Provider",
  readOnly: false,
  status: "ready",
  rootExists: true,
  entityCount: 24,
  hydrationProgress: {
    indexedFiles: 16,
    remainingFiles: 4,
    totalFiles: 20,
  },
  pendingChangeCount: 3,
  provider: null,
};

const sampleGoogleMount: MountSummary = {
  mountId: "google-docs-main",
  connector: "google-docs",
  connectorName: "Google Docs",
  connectionId: "google-docs-default",
  workspaceName: "Drive",
  localPath: "~/Library/CloudStorage/Locality/google-docs-main",
  notionUrl: null,
  accessScope: "Workspace folder",
  remoteRootId: "drive-folder-1",
  projection: "macOS File Provider",
  readOnly: false,
  status: "ready",
  rootExists: true,
  entityCount: 8,
  pendingChangeCount: 0,
  provider: null,
};

const sampleSnapshot: DesktopSnapshot = {
  health: {
    state: "ready",
    attentionCount: 3,
  },
  connection: {
    connector: "notion",
    workspaceName: "CodeFlash",
    accountLabel: "saurabh@codeflash.ai",
    status: "active",
  },
  connections: [
    {
      connector: "notion",
      workspaceName: "CodeFlash",
      accountLabel: "saurabh@codeflash.ai",
      status: "active",
    },
  ],
  mount: sampleMount,
  mounts: [sampleMount, sampleGoogleMount],
  activeMountId: sampleMount.mountId,
  liveMode: {
    enabled: false,
    state: "off",
    label: "Live Mode off",
    reason: null,
    lastRunAt: null,
    pendingCount: 3,
    reviewCount: 1,
    coveredCount: 2,
  },
  needsOnboarding: false,
  settings: {
    launchAtLogin: true,
    showMenuBar: true,
  },
  pendingChanges: [
    {
      mountId: "notion-main",
      entityId: "roadmap-2026",
      title: "Roadmap 2026",
      localPath: "Engineering/Roadmap 2026/page.md",
      summary: "2 text edits",
      state: "safe",
      issueCodes: [],
      liveMode: { enabled: false, state: "off", label: "Live Mode off" },
    },
    {
      mountId: "notion-main",
      entityId: "launch-plan",
      title: "Launch Plan",
      localPath: "Marketing/Launch Plan/page.md",
      summary: "needs review: large deletion",
      state: "needs_review",
      issueCodes: ["large_deletion"],
      liveMode: { enabled: false, state: "off", label: "Live Mode off" },
    },
    {
      mountId: "notion-main",
      entityId: "customer-notes",
      title: "Customer Notes",
      localPath: "Sales/Customer Notes/page.md",
      summary: "1 property edit",
      state: "safe",
      issueCodes: [],
      liveMode: { enabled: true, state: "active", label: "Live Mode on" },
    },
  ],
  recentFiles: [
    {
      title: "Standups with Locality",
      kind: "Page",
      localPath: "~/Library/CloudStorage/Locality/notion/General/Standups with Locality/page.md",
      state: "ready",
    },
    {
      title: "Roadmap 2026",
      kind: "Page",
      localPath: "~/Library/CloudStorage/Locality/notion/Engineering/Roadmap 2026/page.md",
      state: "pending_changes",
    },
  ],
  activity: [
    {
      title: "Pushed Roadmap 2026 to Notion",
      detail: "2 block edits",
      when: "Today",
      occurredAt: "unix_ms:1782033300000",
      kind: "push",
    },
    {
      title: "Located Launch Plan",
      detail: "Prepared local path for an agent",
      when: "Today",
      occurredAt: "unix_ms:1782028800000",
      kind: "locate",
    },
    {
      title: "Connected Notion workspace CodeFlash",
      detail: "Credentials stored in the OS credential store",
      when: "Earlier",
      occurredAt: "unix_ms:1781942400000",
      kind: "connect",
    },
  ],
  suggestions: [
    {
      connector: "Linear",
      description: "Mount issues and projects as local files.",
      state: "available",
    },
  ],
};

const sampleDebugQueueStatus: DebugQueueStatus = {
  generatedAtUnixMs: 1782033300000,
  active: [
    {
      kind: "hydration",
      target: "~/Library/CloudStorage/Locality/notion/Launch Plan/page.md",
      elapsedMs: 842,
      startedAtUnixMs: 1782033299158,
    },
  ],
  sections: [
    {
      name: "hydrations",
      label: "Hydration fetches",
      total: 2,
      ready: 2,
      deferred: null,
      items: [
        {
          kind: "hydration",
          target: "Launch Plan/page.md",
          mountId: "notion-main",
          remoteId: "launch-plan",
          path: "Launch Plan/page.md",
          reason: "live_mode_remote_fast_forward",
          priority: "high",
        },
        {
          kind: "hydration",
          target: "Roadmap/page.md",
          mountId: "notion-main",
          remoteId: "roadmap",
          path: "Roadmap/page.md",
          reason: "policy",
          priority: "normal",
        },
      ],
    },
    {
      name: "freshness",
      label: "Freshness observations",
      total: 1,
      ready: 1,
      deferred: 0,
      items: [
        {
          kind: "ObserveEntity",
          target: "notion-main:launch-plan",
          mountId: "notion-main",
          remoteId: "launch-plan",
          reason: "RemoteMaybeChanged",
          priority: "hot",
        },
      ],
    },
  ],
  schedulerMode: "polling",
  activeIntervalMs: 5000,
  coldIntervalMs: 60000,
  liveMode: {
    mountId: "notion-main",
    enabled: true,
    state: "syncing",
    label: "Live Mode syncing",
    reason: null,
    lastRunAt: "unix_ms:1782033299158",
    trackedFiles: [
      {
        path: "Launch Plan/page.md",
        title: "Launch Plan",
        remoteId: "launch-plan",
        hydration: "dirty",
        status: "pending_changes",
        syncState: "pendinglocalchanges",
        activeForPolling: true,
        remoteCheckDue: true,
        pollingReason: "recent local edit",
        freshnessTier: "immediate",
        lastCheckedAt: "unix_ms:1782033299158",
        lastOpenedAt: "unix_ms:1782033285000",
        lastLocalChangeAt: "unix_ms:1782033290000",
        remoteHintPending: false,
        autoSaveState: "active",
        autoSaveReason: null,
        issueCodes: ["local_body_changed"],
      },
    ],
  },
};

const loadingSnapshot: DesktopSnapshot = {
  ...sampleSnapshot,
  health: {
    state: "checking_freshness",
    attentionCount: 0,
  },
  connection: {
    ...sampleSnapshot.connection,
    workspaceName: "Loading",
    accountLabel: "",
    status: "loading",
  },
  connections: [],
  mount: {
    ...sampleSnapshot.mount,
    workspaceName: "Loading",
    localPath: "~/Library/CloudStorage/Locality/notion",
    notionUrl: null,
    accessScope: "Checking access",
    status: "loading",
    provider: null,
  },
  liveMode: {
    enabled: false,
    state: "off",
    label: "Live Mode off",
    reason: null,
    lastRunAt: null,
    pendingCount: 0,
    reviewCount: 0,
    coveredCount: 0,
  },
  needsOnboarding: false,
  pendingChanges: [],
  recentFiles: [],
  activity: [],
};

const snapshotLoadFailed: DesktopSnapshot = {
  ...loadingSnapshot,
  health: {
    state: "stopped",
    attentionCount: 0,
  },
  connection: {
    ...loadingSnapshot.connection,
    status: "unknown",
  },
  mount: {
    ...loadingSnapshot.mount,
    status: "unknown",
  },
  needsOnboarding: false,
};

const samplePushPlan: PushPlan = {
  title: "Review Push",
  summary: "3 files will update Notion.",
  pagesUpdated: 2,
  databaseRowsUpdated: 1,
  pagesDeleted: 0,
  canPush: true,
  guardrailState: "safe",
  files: sampleSnapshot.pendingChanges,
};

const sampleSearchResults: LocatedItem[] = [
  {
    title: "Roadmap 2026",
    kind: "Page",
    localPath: "~/Library/CloudStorage/Locality/notion/Engineering/Roadmap 2026/page.md",
    state: "ready",
  },
  {
    title: "Launch Plan",
    kind: "Page",
    localPath: "~/Library/CloudStorage/Locality/notion/Marketing/Launch Plan/page.md",
    state: "online_only",
  },
];

function suggestedAgentPrompt(mountPath: string, connector: OnboardingConnectorId = "notion") {
  switch (connector) {
    case "granola":
      return `Use Locality to read my Granola meetings. Open the files under ${mountPath}, search summaries and transcripts with normal file tools, and cite the meeting files you used. Granola is read-only in Locality, so do not try to push edits back.`;
    case "slack":
      return `Use Locality to read my Slack conversations. Open the files under ${mountPath}, search channels, private channels, DMs, group DMs, and users with normal file tools, and cite the conversation files you used. Slack is read-only in Locality, so do not try to push edits back.`;
    case "google-docs":
      return `Use Locality to edit my Google Docs workspace. Open the files under ${mountPath}, make the requested edits directly in Markdown, and leave changes pending for Locality review before pushing.`;
    case "google-calendar":
      return `Use Locality to inspect my Google Calendar source. Open the files under ${mountPath}, review calendar events with normal file tools, and prepare new event drafts for Locality review before creating them.`;
    case "gmail":
      return `Use Locality to inspect my Gmail source. Open the files under ${mountPath}, search mail with normal file tools, and prepare draft updates only when the mounted draft files support it. Leave outbound changes for Locality review.`;
    case "linear":
      return `Use Locality to edit my Linear issues. Open the files under ${mountPath}, update issue Markdown and editable frontmatter, and leave changes pending for Locality review before pushing.`;
    case "notion":
      return `Use Locality to edit my Notion workspace. Open the files under ${mountPath}, make the requested edits directly in Markdown, and leave changes pending for Locality review.`;
  }
}

function isOnboardingConnector(value?: string | null): value is OnboardingConnectorId {
  return sourceConnectorIds().includes(value as SourceConnectorId);
}

function onboardingConnectorFromSnapshot(snapshot: DesktopSnapshot): OnboardingConnectorId {
  if (isOnboardingConnector(snapshot.mount.connector)) {
    return snapshot.mount.connector;
  }
  if (isOnboardingConnector(snapshot.connection.connector)) {
    return snapshot.connection.connector;
  }
  return "notion";
}

function connectorUsesOAuth(connector: OnboardingConnectorId) {
  return !sourceRequiresApiKey(connector);
}

function connectorSkipsMountStep(connector: OnboardingConnectorId) {
  return sourceSkipsManualMountStep(connector);
}

function onboardingConnectorTitle(
  connector: OnboardingConnectorId,
  ready: boolean,
  busy: boolean,
) {
  if (ready) {
    return `Your ${sourceDisplayName(connector)} source is connected`;
  }
  if (busy) {
    return sourceRequiresApiKey(connector)
      ? `Checking ${sourceDisplayName(connector)} access.`
      : `Finish connecting in ${sourceDisplayName(connector)}.`;
  }
  return `Start with ${sourceDisplayName(connector)}.`;
}

function onboardingConnectorDescription(
  connector: OnboardingConnectorId,
  ready: boolean,
  busy: boolean,
  workspaceLabel: string,
) {
  if (ready) {
    switch (connector) {
      case "notion":
        return `${workspaceLabel} is ready. Locality will now create the Notion folder under CloudStorage and prepare the local workspace.`;
      case "google-docs":
        return "Google Docs is ready. Locality mounted the selected Drive folder as local files under CloudStorage.";
      case "google-calendar":
        return "Google Calendar is ready. Locality mounted primary calendar events as local files under CloudStorage.";
      case "gmail":
        return "Gmail is ready. Locality mounted mailboxes as local files under CloudStorage.";
      case "granola":
        return "Granola is ready. Locality mounted meeting summaries and transcripts as read-only files under CloudStorage.";
      case "linear":
        return "Linear is ready. Locality mounted issues by team as editable local files under CloudStorage.";
      case "slack":
        return "Slack is ready. Locality mounted recent accessible conversations as read-only files under CloudStorage.";
    }
  }

  if (busy) {
    switch (connector) {
      case "notion":
        return "A browser window is open. Choose the workspace and pages Locality can access, then approve.";
      case "google-docs":
        return "A browser window is open. Approve Google Docs access, then Locality will create the local folder.";
      case "google-calendar":
        return "A browser window is open. Approve Google Calendar access, then Locality will create the local calendar folder.";
      case "gmail":
        return "A browser window is open. Approve Gmail access, then Locality will create the local mailbox folder.";
      case "granola":
        return "Locality is validating the API key and creating a read-only Granola folder.";
      case "linear":
        return "Locality is validating the API key and creating an editable Linear folder.";
      case "slack":
        return "A browser window is open. Approve Slack access, then Locality will create the read-only conversation folder.";
    }
  }

  switch (connector) {
    case "notion":
      return "Connect the source you want agents to help with. Your machine talks directly to Notion, and app credentials are protected by macOS Keychain.";
    case "google-docs":
      return "Connect Google Docs during setup so agents can work with docs through the same local file workflow.";
    case "google-calendar":
      return "Connect Google Calendar during setup so agents can review events and prepare new event drafts through local files.";
    case "gmail":
      return "Connect Gmail during setup so agents can search mailboxes and prepare reviewed draft work from local files.";
    case "granola":
      return "Paste a Granola API key to mount meeting summaries and transcripts as local read-only files. Keys are stored in your local credential store.";
    case "linear":
      return "Paste a Linear API key to mount issues by team as editable local files. Keys are stored in your local credential store.";
    case "slack":
      return "Connect Slack during setup so agents can search recent accessible conversations from local read-only Markdown files.";
  }
}

function onboardingConnectorPills(connector: OnboardingConnectorId) {
  switch (connector) {
    case "notion":
      return ["Scoped access", "Credentials in Keychain", "Direct app connection"];
    case "google-docs":
      return ["Google OAuth", "Drive folder", "Markdown edits"];
    case "google-calendar":
      return ["Google OAuth", "Primary calendar", "Event drafts"];
    case "gmail":
      return ["Google OAuth", "Mailbox files", "Draft review"];
    case "granola":
      return ["Read-only", "Meeting summaries", "Transcripts"];
    case "linear":
      return ["API key", "Issues by team", "Review before push"];
    case "slack":
      return ["Slack OAuth", "Read-only", "Conversations"];
  }
}

function onboardingReadyCopy(connector: OnboardingConnectorId) {
  switch (connector) {
    case "notion":
      return "Your local workspace is ready. Agents can open this folder, edit Markdown, and leave changes for Review Center. Open the app to review changes, manage sync, and turn on Live Mode when you want file saves to update Notion and new Notion changes to appear locally.";
    case "google-docs":
      return "Your Google Docs workspace is ready as local files. Agents can edit docs in Markdown and leave changes for Review Center before anything is pushed back.";
    case "google-calendar":
      return "Your Google Calendar source is ready as local files. Agents can review events and prepare new event drafts before anything is created remotely.";
    case "gmail":
      return "Your Gmail source is ready as local files. Agents can search mailbox content and prepare reviewed draft work without leaving the filesystem.";
    case "granola":
      return "Your Granola meetings are ready as local read-only files. Agents can search summaries and transcripts with normal file tools, while Locality keeps the remote notes protected from edits.";
    case "linear":
      return "Your Linear issues are ready as local files. Agents can edit issue descriptions and supported fields in Markdown, then leave changes for Review Center before anything is pushed back.";
    case "slack":
      return "Your Slack conversations are ready as local read-only files. Agents can search recent accessible channels, private channels, DMs, group DMs, and users with normal file tools.";
  }
}

function onboardingPromptHint(connector: OnboardingConnectorId) {
  return connector === "granola" || connector === "slack"
    ? "Ask an agent to use the mounted read-only files."
    : "Claude and Codex are now set up to use Locality.";
}

function sampleAgentGuidanceReport(mountPath: string): AgentGuidanceInstallReport {
  return {
    ok: true,
    command: "install_agent_guidance",
    prompt: suggestedAgentPrompt(mountPath),
    targets: [
      {
        agent: "Claude Code / Claude Desktop / Claude Cowork",
        status: "installed",
        path: "~/.claude/skills/locality/SKILL.md",
        detail: "Installed the Locality skill for Claude local agents.",
      },
      {
        agent: "Codex",
        status: "installed",
        path: "~/.codex/skills/locality/SKILL.md",
        detail: "Installed the Locality skill for Codex.",
      },
      {
        agent: "Warp",
        status: "installed",
        path: "~/.agents/skills/locality/SKILL.md",
        detail: "Installed the Locality skill for Warp.",
      },
    ],
  };
}

function isTauriRuntime() {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

function routeForcesMainApp(route: string) {
  return route === "#app";
}

function routeForcesOnboarding(route: string) {
  return route === "#onboarding" || route === "#onboarding-ready";
}

function previewRouteStartsOnboarding(route: string) {
  return !isTauriRuntime() && (route === "" || route === "#");
}

function snapshotNeedsOnboarding(snapshot: DesktopSnapshot) {
  return snapshot.needsOnboarding || connectionMissing(snapshot) || mountMissing(snapshot);
}

function routeShouldShowOnboarding(route: string, snapshot: DesktopSnapshot) {
  if (route === "#tray" || routeForcesMainApp(route)) {
    return false;
  }
  return routeForcesOnboarding(route) || previewRouteStartsOnboarding(route) || snapshotNeedsOnboarding(snapshot);
}

async function callCommand<T>(command: string, args?: Record<string, unknown>, fallback?: T) {
  if (!isTauriRuntime()) {
    if (fallback === undefined) {
      throw new Error(`Tauri command unavailable: ${command}`);
    }
    return fallback;
  }

  return invoke<T>(command, args);
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

function liveModeTooltip(enabled: boolean) {
  return enabled
    ? "Live Mode is watching safe local edits, pushing them to Notion, and pulling remote Notion changes when no review is needed. It pauses when a change needs review."
    : "Turn on Live Mode to keep this Notion source in sync while you work. Locality still pauses for conflicts, large changes, or anything that needs review.";
}

function trayLiveModeLabel(liveMode: MountLiveMode, busy: boolean) {
  if (busy || liveMode.state === "syncing") {
    return "Syncing";
  }
  if (liveMode.state === "error") {
    return "Needs attention";
  }
  if (!liveMode.enabled) {
    return "Off";
  }
  if (liveMode.reviewCount > 0) {
    return `${liveMode.reviewCount} need review`;
  }
  if (liveMode.coveredCount > 0) {
    return `${liveMode.coveredCount} safe pending`;
  }
  return "On";
}

function sourceSyncModeLabel(liveMode: MountLiveMode, active: boolean) {
  if (!active) {
    return "Managed when active";
  }
  if (!liveMode.enabled) {
    return "Review mode";
  }
  if (liveMode.state === "syncing") {
    return "Live Mode syncing";
  }
  if (liveMode.state === "error") {
    return "Live Mode needs attention";
  }
  return "Live Mode";
}

function reviewQueueCounts(changes: PendingChange[]) {
  const problems = changes.filter(isProblemReviewChange).length;
  return {
    total: changes.length,
    approvals: changes.length - problems,
    problems,
  };
}

function reviewFilterLabel(filter: ReviewFilter) {
  if (filter === "approvals") {
    return "Approvals";
  }
  if (filter === "problems") {
    return "Problems";
  }
  return "All";
}

function changeMatchesReviewFilter(change: PendingChange, filter: ReviewFilter) {
  if (filter === "all") {
    return true;
  }
  const isProblem = isProblemReviewChange(change);
  return filter === "problems" ? isProblem : !isProblem;
}

function isProblemReviewChange(change: PendingChange) {
  return change.state === "conflict" || change.state === "blocked";
}

function fileStatusFilterLabel(filter: FileStatusFilter) {
  if (filter === "review") {
    return "Needs review";
  }
  if (filter === "conflict") {
    return "Conflicts";
  }
  if (filter === "synced") {
    return "Synced";
  }
  return "All";
}

function itemMatchesFileStatusFilter(item: LocatedItem, filter: FileStatusFilter) {
  if (filter === "all") {
    return true;
  }
  if (filter === "review") {
    return item.state === "pending_changes" || item.state === "remote_update_available";
  }
  if (filter === "conflict") {
    return item.state === "conflict";
  }
  return item.state === "ready";
}

function onboardingStepMeta(step: OnboardingStep) {
  if (step === 2) {
    return "Optional guide";
  }
  if (step === 3) {
    return "2 of 4";
  }
  if (step === 4) {
    return "3 of 4";
  }
  if (step === 5) {
    return "4 of 4";
  }
  return "1 of 4";
}

function useMountLiveModeController(
  snapshot: DesktopSnapshot,
  onRefresh: () => Promise<void>,
) {
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");
  const refreshRef = useRef(onRefresh);
  const enabled = snapshot.liveMode.enabled;
  const state = snapshot.liveMode.state;

  useEffect(() => {
    refreshRef.current = onRefresh;
  }, [onRefresh]);

  async function toggle() {
    setBusy(true);
    setMessage("");
    try {
      const report = await callCommand<ActionReport>(
        "set_mount_live_mode",
        { change: { enabled: !enabled } },
        {
          ok: true,
          message: enabled ? "Live Mode is off for this folder." : "Live Mode is on for this folder.",
        },
      );
      setMessage(report.message);
    } catch (error) {
      setMessage(errorMessage(error));
    } finally {
      await refreshRef.current().catch(() => undefined);
      setBusy(false);
    }
  }

  return {
    liveModeEnabled: enabled,
    liveModeBusy: busy || state === "syncing",
    liveModeState: state,
    liveModeMessage: message || snapshot.liveMode.reason || "",
    toggleLiveMode: toggle,
  };
}

function useNotionSearchResults(query: string, enabled = true) {
  const [results, setResults] = useState<LocatedItem[]>([]);
  const [searching, setSearching] = useState(false);

  useEffect(() => {
    const trimmed = query.trim();
    if (!enabled || trimmed.length < 2) {
      setResults([]);
      setSearching(false);
      return;
    }

    let cancelled = false;
    setSearching(true);
    const timer = window.setTimeout(() => {
      void callCommand<LocatedItem[]>(
        "search_notion_pages",
        { query: trimmed },
        sampleSearchResults.filter((item) =>
          `${item.title} ${item.localPath}`.toLowerCase().includes(trimmed.toLowerCase()),
        ),
      )
        .then((items) => {
          if (!cancelled) {
            setResults(items);
          }
        })
        .catch(() => {
          if (!cancelled) {
            setResults([]);
          }
        })
        .finally(() => {
          if (!cancelled) {
            setSearching(false);
          }
        });
    }, 160);

    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [enabled, query]);

  return { results, searching };
}

function initialAppTheme(): AppTheme {
  try {
    const saved = window.localStorage.getItem(APP_THEME_STORAGE_KEY);
    return saved === "light" || saved === "dark" || saved === "system" ? saved : "system";
  } catch {
    return "system";
  }
}

function resolvedAppTheme(theme: AppTheme): "light" | "dark" {
  if (theme !== "system") {
    return theme;
  }
  return window.matchMedia?.("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

function applyAppTheme(theme: AppTheme) {
  const resolved = resolvedAppTheme(theme);
  document.documentElement.dataset.theme = resolved;
  document.documentElement.dataset.themePreference = theme;
  document.documentElement.style.colorScheme = resolved;
}

export default function App() {
  const initialRoute = window.location.hash;
  const [snapshot, setSnapshot] = useState<DesktopSnapshot>(() =>
    isTauriRuntime() ? loadingSnapshot : sampleSnapshot,
  );
  const [snapshotLoaded, setSnapshotLoaded] = useState(() => !isTauriRuntime());
  const [view, setView] = useState<AppView>("home");
  const [reviewInitialFilter, setReviewInitialFilter] = useState<ReviewFilter>("all");
  const [theme, setTheme] = useState<AppTheme>(() => initialAppTheme());
  const [route, setRoute] = useState(initialRoute);
  const [showOnboarding, setShowOnboarding] = useState(() =>
    routeForcesOnboarding(initialRoute) || previewRouteStartsOnboarding(initialRoute),
  );
  const [onboardingKey, setOnboardingKey] = useState(0);
  const [onboardingInitialStep, setOnboardingInitialStep] = useState<OnboardingStep>(() =>
    initialRoute === "#onboarding-ready" ? 5 : 1,
  );
  const [updateStatus, setUpdateStatus] = useState<UpdateStatus>(emptyUpdateStatus);
  const updateStatusRef = useRef<UpdateStatus>(updateStatus);
  const updateOperationRef = useRef<Promise<void> | null>(null);
  const refreshSnapshotPromise = useRef<Promise<void> | null>(null);
  const refreshSnapshotQueued = useRef(false);

  useEffect(() => {
    updateStatusRef.current = updateStatus;
  }, [updateStatus]);

  useEffect(() => {
    applyAppTheme(theme);
    try {
      window.localStorage.setItem(APP_THEME_STORAGE_KEY, theme);
    } catch {
      // Keep the in-memory theme even when storage is unavailable.
    }

    if (theme !== "system") {
      return undefined;
    }

    const media = window.matchMedia?.("(prefers-color-scheme: dark)");
    if (!media) {
      return undefined;
    }
    const syncSystemTheme = () => applyAppTheme("system");
    media.addEventListener("change", syncSystemTheme);
    return () => media.removeEventListener("change", syncSystemTheme);
  }, [theme]);

  async function loadDesktopSnapshot() {
    const nextSnapshot = await callCommand<DesktopSnapshot>("desktop_snapshot", undefined, sampleSnapshot);
    setSnapshot(nextSnapshot);
    setSnapshotLoaded(true);
    return nextSnapshot;
  }

  async function refreshSnapshot() {
    if (refreshSnapshotPromise.current) {
      refreshSnapshotQueued.current = true;
      return refreshSnapshotPromise.current;
    }

    const run = async () => {
      do {
        refreshSnapshotQueued.current = false;
        await loadDesktopSnapshot();
      } while (refreshSnapshotQueued.current);
    };

    const promise = run().finally(() => {
      refreshSnapshotPromise.current = null;
    });
    refreshSnapshotPromise.current = promise;
    return promise;
  }

  async function checkForAppUpdate(options: AppUpdateCheckOptions = {}) {
    if (updateOperationRef.current) {
      return updateOperationRef.current;
    }

    const operation = (async () => {
      if (appStoreDistribution) {
        if (!options.silent) {
          setUpdateStatus({
            state: "current",
            message: "Updates are managed by the Mac App Store.",
            update: null,
          });
        }
        return;
      }

      if (!isTauriRuntime()) {
        if (!options.silent) {
          setUpdateStatus({
            state: "error",
            message: "Updates are available in the packaged app.",
            update: null,
          });
        }
        return;
      }

      if (!options.silent) {
        setUpdateStatus({ state: "checking", message: "Checking for updates.", update: null });
      }

      try {
        const update = await check();
        if (!update) {
          if (!options.silent) {
            setUpdateStatus({ state: "current", message: "Locality is up to date.", update: null });
          }
          return;
        }

        setUpdateStatus(availableUpdateStatus(update));
        if (options.autoDownload) {
          await downloadAppUpdate(update, {
            installWhenHidden: options.installWhenHidden ?? false,
          });
        }
      } catch (error) {
        if (!options.silent || updateStatusRef.current.state !== "idle") {
          setUpdateStatus({ state: "error", message: updateErrorMessage(error), update: null });
        }
      }
    })();

    updateOperationRef.current = operation;
    try {
      await operation;
    } finally {
      if (updateOperationRef.current === operation) {
        updateOperationRef.current = null;
      }
    }
  }

  async function downloadAppUpdate(
    update: Update,
    options: { installWhenHidden?: boolean } = {},
  ) {
    let progress: UpdateStatus["progress"] = undefined;
    setUpdateStatus({
      state: "downloading",
      message: updateDownloadMessage(update.version, progress),
      update,
      version: update.version,
      progress,
    });

    await update.download((event) => {
      progress = nextUpdateDownloadProgress(progress, event);
      setUpdateStatus({
        state: "downloading",
        message:
          event.event === "Finished"
            ? downloadedUpdateMessage(update.version)
            : updateDownloadMessage(update.version, progress),
        update,
        version: update.version,
        progress,
      });
    });

    setUpdateStatus({
      state: "downloaded",
      message: downloadedUpdateMessage(update.version),
      update,
      version: update.version,
      progress,
    });

    if (options.installWhenHidden && (await autoInstallUpdateAllowed())) {
      await installDownloadedAppUpdate(update);
    }
  }

  async function autoInstallUpdateAllowed() {
    if (await mainWindowVisible()) {
      return false;
    }
    try {
      const queue = await callCommand<DebugQueueStatus>("debug_notion_queue_status");
      return queueIdleForAutoUpdateInstall(queue);
    } catch (error) {
      console.warn(`Skipping automatic update install: ${errorMessage(error)}`);
      return false;
    }
  }

  async function mainWindowVisible() {
    try {
      return await getCurrentWindow().isVisible();
    } catch {
      return true;
    }
  }

  async function installDownloadedAppUpdate(update: Update) {
    setUpdateStatus({
      state: "installing",
      message: installingUpdateMessage(update.version),
      update,
      version: update.version,
    });
    const relaunchFallback = await scheduleUpdateRelaunchFallback();
    if (!relaunchFallback.ok) {
      console.warn(relaunchFallback.message);
    }
    await update.install();
    setUpdateStatus({
      state: "installing",
      message: "Restarting Locality to finish the update.",
      update: null,
      version: update.version,
    });
    await relaunch();
  }

  async function scheduleUpdateRelaunchFallback(): Promise<ActionReport> {
    try {
      return await callCommand<ActionReport>("schedule_update_relaunch", undefined, {
        ok: false,
        message: "Relaunch fallback is only available in the packaged app.",
      });
    } catch (error) {
      return {
        ok: false,
        message: `Could not schedule relaunch fallback: ${errorMessage(error)}`,
      };
    }
  }

  async function installAppUpdate() {
    if (appStoreDistribution) {
      setUpdateStatus({
        state: "current",
        message: "Updates are managed by the Mac App Store.",
        update: null,
      });
      return;
    }

    if (!isTauriRuntime()) {
      setUpdateStatus({
        state: "error",
        message: "Updates are available in the packaged app.",
        update: null,
      });
      return;
    }

    if (updateOperationRef.current) {
      await updateOperationRef.current;
    }

    const operation = (async () => {
      try {
        const current = updateStatusRef.current;
        const update = current.update ?? (await check());
        if (!update) {
          setUpdateStatus({ state: "current", message: "Locality is up to date.", update: null });
          return;
        }

        if (current.state !== "downloaded" || current.update !== update) {
          await downloadAppUpdate(update);
        }
        await installDownloadedAppUpdate(update);
      } catch (error) {
        setUpdateStatus({ state: "error", message: updateErrorMessage(error), update: null });
      }
    })();

    updateOperationRef.current = operation;
    try {
      await operation;
    } finally {
      if (updateOperationRef.current === operation) {
        updateOperationRef.current = null;
      }
    }
  }

  useEffect(() => {
    let cancelled = false;

    void (async () => {
      let installReview: InstallStateReview | null = null;
      if (isTauriRuntime()) {
        installReview = await callCommand<InstallStateReview>(
          "install_state_review",
          undefined,
          {
            shouldPrompt: false,
            stateExists: true,
            sqliteExists: true,
            previousBuildId: null,
            currentBuildId: "unknown",
          },
        ).catch(() => null);
        await callCommand<ActionReport>("acknowledge_install_state").catch(() => undefined);
        if (!appStoreDistribution) {
          await callCommand<ActionReport>("ensure_terminal_cli_available").catch(() => undefined);
        }
        await callCommand<ActionReport>("ensure_runtime_ready").catch(() => undefined);
      }
      await loadDesktopSnapshot();
      if (!cancelled && installReview?.shouldPrompt && window.location.hash !== "#tray") {
        setOnboardingInitialStep(1);
        setOnboardingKey((key) => key + 1);
        setShowOnboarding(true);
      }
    })().catch(() => {
      setSnapshot(isTauriRuntime() ? snapshotLoadFailed : sampleSnapshot);
      setSnapshotLoaded(true);
    });

    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    if (!isTauriRuntime() || appStoreDistribution) {
      return undefined;
    }

    const timer = window.setTimeout(() => {
      void checkForAppUpdate({
        silent: true,
        autoDownload: true,
        installWhenHidden: true,
      });
    }, 1500);

    return () => window.clearTimeout(timer);
  }, []);

  useEffect(() => {
    const handleHashChange = () => setRoute(window.location.hash);
    window.addEventListener("hashchange", handleHashChange);
    return () => window.removeEventListener("hashchange", handleHashChange);
  }, []);

  useEffect(() => {
    if (!snapshotLoaded || route === "#tray") {
      return;
    }

    if (route === "#onboarding-ready") {
      setOnboardingInitialStep(5);
      setShowOnboarding(true);
      return;
    }

    if (routeShouldShowOnboarding(route, snapshot)) {
      setOnboardingInitialStep(1);
      setShowOnboarding(true);
      return;
    }

    if (showOnboarding) {
      return;
    }

    setShowOnboarding(false);
  }, [
    route,
    snapshot.connection.status,
    snapshot.mount.status,
    snapshot.needsOnboarding,
    snapshotLoaded,
  ]);

  useEffect(() => {
    const handleOpenView = (event: Event) => {
      const nextView = normalizeAppView((event as CustomEvent<string>).detail);
      if (!nextView) {
        return;
      }
      setShowOnboarding(false);
      setView(nextView);
    };

    window.addEventListener("loc-open-view", handleOpenView);
    return () => window.removeEventListener("loc-open-view", handleOpenView);
  }, []);

  useEffect(() => {
    const refresh = () => {
      void refreshSnapshot().catch(() => undefined);
    };

    window.addEventListener("loc-refresh-snapshot", refresh);
    return () => {
      window.removeEventListener("loc-refresh-snapshot", refresh);
    };
  }, []);

  useEffect(() => {
    if (!isTauriRuntime()) {
      return undefined;
    }

    const refreshVisibleSnapshot = () => {
      if (document.visibilityState !== "hidden") {
        void refreshSnapshot().catch(() => undefined);
      }
    };

    const interval = window.setInterval(refreshVisibleSnapshot, 10000);
    window.addEventListener("focus", refreshVisibleSnapshot);
    document.addEventListener("visibilitychange", refreshVisibleSnapshot);

    return () => {
      window.clearInterval(interval);
      window.removeEventListener("focus", refreshVisibleSnapshot);
      document.removeEventListener("visibilitychange", refreshVisibleSnapshot);
    };
  }, []);

  useEffect(() => {
    document.body.dataset.surface = route === "#tray" ? "tray" : "app";
  }, [route]);

  if (route === "#tray") {
    return <TrayPopover snapshot={snapshot} onRefresh={refreshSnapshot} />;
  }

  const shouldRenderOnboarding =
    showOnboarding || (snapshotLoaded && routeShouldShowOnboarding(route, snapshot));

  if (shouldRenderOnboarding) {
    return (
      <Onboarding
        key={onboardingKey}
        snapshot={snapshot}
        snapshotLoaded={snapshotLoaded}
        initialStep={onboardingInitialStep}
        onComplete={() => {
          void refreshSnapshot().catch(() => undefined);
          setShowOnboarding(false);
          setView("home");
        }}
      />
    );
  }

  if (isTauriRuntime() && !snapshotLoaded && !routeForcesMainApp(route)) {
    return <SetupLoading />;
  }

  return (
            <MainShell
              snapshot={snapshot}
              view={view}
              onViewChange={setView}
              reviewInitialFilter={reviewInitialFilter}
              onOpenReview={(filter = "all") => {
                setReviewInitialFilter(filter);
                setView("pending");
              }}
              theme={theme}
              onThemeChange={setTheme}
              onRefresh={refreshSnapshot}
              updateStatus={updateStatus}
      onCheckForUpdate={checkForAppUpdate}
      onInstallUpdate={installAppUpdate}
      appStoreDistribution={appStoreDistribution}
      onResetComplete={() => {
        setOnboardingInitialStep(1);
        setOnboardingKey((key) => key + 1);
        setView("home");
        setShowOnboarding(true);
      }}
    />
  );
}

function SetupLoading() {
  return (
    <main className="setup-shell">
      <section className="setup-window">
        <WindowChrome title="Locality Setup" meta="Checking" />
        <SetupContent mark={<BrandTile />}>
          <div>
            <div className="sync-note">
              <Loader2 className="spin" />
              Checking setup
            </div>
            <h1>Checking your Locality setup</h1>
            <p>Locality is checking your Notion connection and local folder.</p>
          </div>
        </SetupContent>
      </section>
    </main>
  );
}

function Onboarding({
  snapshot,
  snapshotLoaded,
  initialStep,
  onComplete,
}: {
  snapshot: DesktopSnapshot;
  snapshotLoaded: boolean;
  initialStep: OnboardingStep;
  onComplete: () => void;
}) {
  const [step, setStep] = useState<OnboardingStep>(initialStep);
  const [oauthReady, setOauthReady] = useState(false);
  const [oauthInFlight, setOauthInFlight] = useState(false);
  const [oauthError, setOauthError] = useState("");
  const [loginUrl, setLoginUrl] = useState("");
  const [loginCopyMessage, setLoginCopyMessage] = useState("");
  const [selectedOnboardingConnector, setSelectedOnboardingConnector] = useState<OnboardingConnectorId>(() =>
    onboardingConnectorFromSnapshot(snapshot),
  );
  const [connectedOnboardingConnector, setConnectedOnboardingConnector] = useState<OnboardingConnectorId | null>(() => {
    const connector = onboardingConnectorFromSnapshot(snapshot);
    return connectionReady(snapshot) && !mountMissing(snapshot) ? connector : null;
  });
  const [granolaApiKey, setGranolaApiKey] = useState("");
  const [linearApiKey, setLinearApiKey] = useState("");
  const [googleDocsWorkspaceFolder, setGoogleDocsWorkspaceFolder] = useState("Locality");
  const [connectorConnecting, setConnectorConnecting] = useState(false);
  const [connectedWorkspace, setConnectedWorkspace] = useState(snapshot.connection.workspaceName);
  const [mountPath, setMountPath] = useState(snapshot.mount.localPath);
  const [mountPathDirty, setMountPathDirty] = useState(false);
  const [locateUrl, setLocateUrl] = useState("");
  const [locatedItem, setLocatedItem] = useState<LocatedItem | null>(null);
  const [locateState, setLocateState] = useState<LocateState>("idle");
  const [locateError, setLocateError] = useState("");
  const [optionalGuideReturnStep, setOptionalGuideReturnStep] = useState<OnboardingStep | null>(null);
  const [mountOnboarding, setMountOnboarding] = useState<WorkspaceMountOnboardingReport | null>(null);
  const [mounting, setMounting] = useState(false);
  const [fileProviderEnablement, setFileProviderEnablement] = useState<FileProviderEnablementReport | null>(null);
  const [finderHelpOpen, setFinderHelpOpen] = useState(false);
  const [finderRevealError, setFinderRevealError] = useState("");
  const [agentGuidanceReport, setAgentGuidanceReport] = useState<AgentGuidanceInstallReport | null>(null);
  const [agentGuidanceState, setAgentGuidanceState] = useState<"idle" | "installing" | "ready" | "error">("idle");
  const mountStartRequestedRef = useRef(false);
  const finderRevealRequestedRef = useRef(false);
  const snapshotConnectionConnector = isOnboardingConnector(snapshot.connection.connector)
    ? snapshot.connection.connector
    : null;
  const snapshotMountConnector = isOnboardingConnector(snapshot.mount.connector)
    ? snapshot.mount.connector
    : null;
  const connectionReadyNow = selectedOnboardingConnector === "notion"
    ? oauthReady || (connectionReady(snapshot) && snapshotConnectionConnector === "notion")
    : connectedOnboardingConnector === selectedOnboardingConnector ||
      (
        connectionReady(snapshot) &&
        snapshotConnectionConnector === selectedOnboardingConnector &&
        snapshotMountConnector === selectedOnboardingConnector &&
        !mountMissing(snapshot)
      );
  const selectedSourceName = sourceDisplayName(selectedOnboardingConnector);
  const selectedConnectorBusy = oauthInFlight || connectorConnecting;
  const selectedApiKey = selectedOnboardingConnector === "linear" ? linearApiKey : granolaApiKey;

  async function installAgentGuidance(path: string) {
    setAgentGuidanceState("installing");
    try {
      const report = await callCommand<AgentGuidanceInstallReport>(
        "install_agent_guidance",
        { mountPath: path },
        sampleAgentGuidanceReport(path),
      );
      setAgentGuidanceReport(report);
      setAgentGuidanceState(report.ok ? "ready" : "error");
    } catch (error) {
      setAgentGuidanceReport({
        ok: false,
        command: "install_agent_guidance",
        prompt: suggestedAgentPrompt(path),
        targets: [
          {
            agent: "Agent instructions",
            status: "failed",
            path: null,
            detail: errorMessage(error),
          },
        ],
      });
      setAgentGuidanceState("error");
    }
  }

  useEffect(() => {
    setConnectedWorkspace(snapshot.connection.workspaceName);
  }, [snapshot.connection.workspaceName]);

  useEffect(() => {
    const connector = onboardingConnectorFromSnapshot(snapshot);
    if (!connectionReady(snapshot)) {
      return;
    }
    if (connector === "notion") {
      setOauthReady(true);
    }
    if (snapshot.mount.connector === connector && !mountMissing(snapshot)) {
      setConnectedOnboardingConnector(connector);
      if (connector !== "notion") {
        setSelectedOnboardingConnector(connector);
      }
    }
  }, [snapshot.connection.connector, snapshot.connection.status, snapshot.mount.connector, snapshot.mount.status]);

  useEffect(() => {
    if (!mountPathDirty) {
      setMountPath(snapshot.mount.localPath);
    }
  }, [mountPathDirty, snapshot.mount.localPath]);

  useEffect(() => {
    if (selectedOnboardingConnector !== "notion" || step !== 3 || !oauthInFlight || oauthReady) {
      return;
    }

    let cancelled = false;
    async function refreshLoginUrl() {
      const url = await callCommand<string | null>("notion_login_link", undefined, null).catch(() => null);
      if (!cancelled && url) {
        setLoginUrl(url);
      }
    }

    void refreshLoginUrl();
    const interval = window.setInterval(() => void refreshLoginUrl(), 700);
    return () => {
      cancelled = true;
      window.clearInterval(interval);
    };
  }, [oauthInFlight, oauthReady, selectedOnboardingConnector, step]);

  useEffect(() => {
    if (!oauthInFlight) {
      setLoginUrl("");
    }
  }, [oauthInFlight]);

  useEffect(() => {
    if (
      !snapshotLoaded ||
      window.location.hash === "#onboarding" ||
      window.location.hash === "#onboarding-ready" ||
      connectionMissing(snapshot)
    ) {
      return;
    }

    const connector = onboardingConnectorFromSnapshot(snapshot);
    setSelectedOnboardingConnector(connector);
    if (connector === "notion") {
      setOauthReady(true);
    } else if (!mountMissing(snapshot)) {
      setConnectedOnboardingConnector(connector);
    }
    setStep((current) => {
      if (mountMissing(snapshot)) {
        if (connectorSkipsMountStep(connector)) {
          return current < 3 ? 3 : current;
        }
        return current < 4 ? 4 : current;
      }
      return current < 5 ? 5 : current;
    });
  }, [snapshot.connection.connector, snapshot.connection.status, snapshot.mount.connector, snapshot.mount.status, snapshotLoaded]);

  useEffect(() => {
    if (
      selectedOnboardingConnector !== "notion" ||
      !shouldAutoCreateMount({
        step,
        connectionReady: connectionReadyNow,
        mountMissing: mountMissing(snapshot),
        mounting,
        hasMountError: mountOnboarding !== null,
        mountPath,
        startRequested: mountStartRequestedRef.current,
      })
    ) {
      return;
    }
    void runMountOnboarding("start");
  }, [connectionReadyNow, mountOnboarding, mountPath, mounting, selectedOnboardingConnector, snapshot.mount.status, step]);

  useEffect(() => {
    const enablementActive =
      step === 4 &&
      (mountOnboarding?.state === "needs_finder_enable" ||
        mountOnboarding?.state === "waiting_for_cloudstorage_root");
    if (!enablementActive) {
      finderRevealRequestedRef.current = false;
      setFileProviderEnablement(null);
      setFinderHelpOpen(false);
      setFinderRevealError("");
      return;
    }

    let completionTimer: number | null = null;
    const poller = createFileProviderEnablementPoller({
      probe: () =>
        callCommand<FileProviderEnablementReport>(
          "file_provider_enablement_status",
          undefined,
          {
            state: "ready",
            message: "Locality is enabled in Finder.",
            path: mountPath,
          },
        ),
      onReport: (report) => {
        setFileProviderEnablement(report);
        if (report.state === "unavailable") {
          setMountOnboarding(failedMountOnboardingReport(report.message));
        }
      },
      onReady: (report) => {
        setFileProviderEnablement(report);
        completionTimer = window.setTimeout(() => {
          void runMountOnboarding("start");
        }, 350);
      },
    });
    const updateVisibility = () => {
      poller.setVisible(document.visibilityState !== "hidden");
    };

    if (
      mountOnboarding?.state === "needs_finder_enable" &&
      !finderRevealRequestedRef.current
    ) {
      finderRevealRequestedRef.current = true;
      void revealFileProviderEnablement();
    }
    document.addEventListener("visibilitychange", updateVisibility);
    updateVisibility();
    poller.start();
    return () => {
      poller.stop();
      document.removeEventListener("visibilitychange", updateVisibility);
      if (completionTimer !== null) {
        window.clearTimeout(completionTimer);
      }
    };
  }, [mountOnboarding?.state, step]);

  useEffect(() => {
    if (
      selectedOnboardingConnector !== "notion" ||
      step !== 5 ||
      mountMissing(snapshot) ||
      agentGuidanceState !== "idle"
    ) {
      return;
    }
    void installAgentGuidance(mountPath);
  }, [agentGuidanceState, mountPath, selectedOnboardingConnector, snapshot.mount.status, step]);

  async function readLoginUrl() {
    return callCommand<string | null>("notion_login_link", undefined, null).catch(() => null);
  }

  async function waitForLoginUrl(connectPromise?: Promise<unknown>) {
    for (let attempt = 0; attempt < 12; attempt += 1) {
      const url = await readLoginUrl();
      if (url) {
        return url;
      }
      if (connectPromise) {
        const state = await Promise.race([
          connectPromise.then(() => "done"),
          new Promise<"waiting">((resolve) => {
            window.setTimeout(() => resolve("waiting"), 120);
          }),
        ]);
        if (state === "done") {
          break;
        }
      } else {
        await new Promise<void>((resolve) => {
          window.setTimeout(resolve, 120);
        });
      }
    }
    return null;
  }

  async function runConnectFlow({
    openBrowser,
    onLoginUrlReady,
  }: {
    openBrowser: boolean;
    onLoginUrlReady?: (url: string) => void | Promise<void>;
  }) {
    setOauthError("");
    setLoginUrl("");
    setLoginCopyMessage("");
    setOauthReady(false);
    setOauthInFlight(true);
    setStep(3);

    const connectPromise = callCommand<ActionReport>(
      openBrowser ? "connect_notion" : "connect_notion_without_browser",
      undefined,
      { ok: true, message: "Connected demo workspace." },
    ).then(
      (report) => ({ ok: true as const, report }),
      (error) => ({ ok: false as const, error }),
    );

    try {
      if (onLoginUrlReady) {
        const url = await waitForLoginUrl(connectPromise);
        if (url) {
          setLoginUrl(url);
          await onLoginUrlReady(url);
        }
      }
      const result = await connectPromise;
      if (!result.ok) {
        throw result.error;
      }
      const report = result.report;
      if (!report.ok) {
        setOauthError(report.message);
        return;
      }
      const nextSnapshot = await callCommand<DesktopSnapshot>(
        "desktop_snapshot",
        undefined,
        sampleSnapshot,
      );
      setConnectedWorkspace(nextSnapshot.connection.workspaceName);
      setOauthReady(true);
    } catch (error) {
      setOauthError(errorMessage(error));
    } finally {
      setOauthInFlight(false);
    }
  }

  async function startConnect() {
    await runConnectFlow({ openBrowser: true });
  }

  async function createOnboardingConnectorMount(connector: Exclude<OnboardingConnectorId, "notion">) {
    return callCommand<ActionReport>(
      "create_desktop_mount",
      {
        request: {
          connector,
          path: sourceDefaultPath(snapshot, connector),
          mountId: sourceMountId(connector),
          connectionId: null,
          readOnly: connector === "granola" || connector === "slack",
          notionRootPage: null,
          googleDocsWorkspaceFolder: connector === "google-docs"
            ? googleDocsWorkspaceFolder.trim() || "Locality"
            : null,
        },
      },
      { ok: true, message: `Mounted demo ${sourceDisplayName(connector)} source.` },
    );
  }

  async function connectOAuthOnboarding(connector: "google-docs" | "google-calendar" | "gmail" | "slack") {
    if (selectedConnectorBusy) {
      return;
    }
    if (connector === "google-docs" && !googleDocsWorkspaceFolder.trim()) {
      setOauthError("Enter a Google Drive folder name, URL, or ID.");
      return;
    }

    setOauthError("");
    setLoginCopyMessage("");
    setMountOnboarding(null);
    setOauthInFlight(true);
    setStep(3);
    try {
      const command = oauthConnectCommand(connector);
      const connectReport = await callCommand<ActionReport>(
        command,
        undefined,
        { ok: true, message: `Connected demo ${sourceDisplayName(connector)} account.` },
      );
      if (!connectReport.ok) {
        setOauthError(connectReport.message);
        return;
      }

      const mountReport = await createOnboardingConnectorMount(connector);
      if (!mountReport.ok) {
        setOauthError(mountReport.message);
        return;
      }

      const nextSnapshot = await callCommand<DesktopSnapshot>(
        "desktop_snapshot",
        undefined,
        sampleSnapshot,
      );
      const sourceMount = nextSnapshot.mounts.find((mount) => mount.connector === connector)
        ?? (nextSnapshot.mount.connector === connector ? nextSnapshot.mount : null);
      setMountPathDirty(false);
      setMountPath(sourceMount?.localPath || sourceDefaultPath(nextSnapshot, connector));
      setConnectedWorkspace(sourceDisplayName(connector));
      setConnectedOnboardingConnector(connector);
      const cliReady = await ensureCliAvailable();
      if (!cliReady) {
        setOauthError(
          `${sourceDisplayName(connector)} is connected, but Locality could not prepare the terminal command. Open Settings to repair Locality, then open the app.`,
        );
        return;
      }
      setStep(5);
    } catch (error) {
      setOauthError(errorMessage(error));
    } finally {
      setOauthInFlight(false);
    }
  }

  async function connectSelectedOnboardingConnector() {
    switch (selectedOnboardingConnector) {
      case "notion":
        await startConnect();
        return;
      case "google-docs":
      case "google-calendar":
      case "gmail":
      case "slack":
        await connectOAuthOnboarding(selectedOnboardingConnector);
        return;
      case "granola":
        await connectGranolaOnboarding();
        return;
      case "linear":
        await connectLinearOnboarding();
    }
  }

  function selectOnboardingConnector(connector: OnboardingConnectorId) {
    if (selectedConnectorBusy || mounting) {
      return;
    }
    setSelectedOnboardingConnector(connector);
    setOauthError("");
    setLoginCopyMessage("");
    setMountOnboarding(null);
  }

  async function connectGranolaOnboarding() {
    await connectApiKeyOnboarding("granola", granolaApiKey);
  }

  async function connectLinearOnboarding() {
    await connectApiKeyOnboarding("linear", linearApiKey);
  }

  async function connectApiKeyOnboarding(connector: "granola" | "linear", apiKey: string) {
    const sourceName = sourceDisplayName(connector);
    if (connectorConnecting || !apiKey.trim()) {
      if (!apiKey.trim()) {
        setOauthError(`Enter a ${sourceName} API key.`);
      }
      return;
    }

    setOauthError("");
    setLoginCopyMessage("");
    setMountOnboarding(null);
    setConnectorConnecting(true);
    setStep(3);
    try {
      const report = await callCommand<ActionReport>(
        connector === "linear" ? "connect_linear" : "connect_granola",
        { apiKey },
        { ok: true, message: `Connected demo ${sourceName} source.` },
      );
      if (!report.ok) {
        setOauthError(report.message);
        return;
      }

      const nextSnapshot = await callCommand<DesktopSnapshot>(
        "desktop_snapshot",
        undefined,
        sampleSnapshot,
      );
      const sourceMount = nextSnapshot.mounts.find((mount) => mount.connector === connector)
        ?? (nextSnapshot.mount.connector === connector ? nextSnapshot.mount : null);
      const nextMountPath = sourceMount?.localPath || sourceDefaultPath(nextSnapshot, connector);
      setMountPathDirty(false);
      setMountPath(nextMountPath);
      setConnectedWorkspace(sourceName);
      setConnectedOnboardingConnector(connector);
      const cliReady = await ensureCliAvailable();
      if (!cliReady) {
        setOauthError(
          `${sourceName} is connected, but Locality could not prepare the terminal command. Open Settings to repair Locality, then open the app.`,
        );
        return;
      }
      setStep(5);
    } catch (error) {
      setOauthError(errorMessage(error));
    } finally {
      setConnectorConnecting(false);
    }
  }

  async function copyLoginLink() {
    setOauthError("");
    setLoginCopyMessage("");
    const mode = loginLinkFlowMode({
      connectionReady: connectionReadyNow,
      oauthInFlight,
      loginUrl,
    });

    if (mode === "start-without-browser") {
      await runConnectFlow({
        openBrowser: false,
        onLoginUrlReady: async (url) => {
          copyText(url);
          setLoginCopyMessage("Copied login link.");
        },
      });
      return;
    }

    const url = loginUrl || (await waitForLoginUrl());
    if (!url) {
      setOauthError("The Notion login link is still being prepared. Try again in a moment.");
      return;
    }

    setLoginUrl(url);
    copyText(url);
    setLoginCopyMessage("Copied login link.");
  }

  async function runMountOnboarding(action: "start" | "allow_in_macos" | "check_again") {
    if (mountStartRequestedRef.current || mounting) {
      return;
    }

    mountStartRequestedRef.current = true;
    setMounting(true);
    try {
      const report = await callCommand<WorkspaceMountOnboardingReport>(
        "run_workspace_mount_onboarding",
        { request: { path: mountPath, action } },
        {
          state: "created",
          message: "Created demo mount.",
          primaryAction: "retry_setup",
          launchStrategy: "none",
        },
      );
      setMountOnboarding(report);
      if (report.state !== "created") {
        return;
      }
      const cliReady = await ensureCliAvailable();
      if (!cliReady) {
        return;
      }
      const nextSnapshot = await callCommand<DesktopSnapshot>(
        "desktop_snapshot",
        undefined,
        sampleSnapshot,
      );
      setMountPathDirty(false);
      setMountPath(nextSnapshot.mount.localPath);
      await installAgentGuidance(nextSnapshot.mount.localPath);
      setMountOnboarding(null);
      setStep(5);
    } catch (error) {
      setMountOnboarding(failedMountOnboardingReport(errorMessage(error)));
    } finally {
      mountStartRequestedRef.current = false;
      setMounting(false);
    }
  }

  async function revealFileProviderEnablement() {
    setFinderRevealError("");
    try {
      const report = await callCommand<ActionReport>(
        "reveal_file_provider_enablement",
        undefined,
        { ok: true, message: "Opened Locality in Finder." },
      );
      if (!report.ok) {
        setFinderRevealError(report.message);
      }
    } catch (error) {
      setFinderRevealError(errorMessage(error));
    }
  }

  async function ensureCliAvailable() {
    if (appStoreDistribution) {
      return true;
    }

    const report = await callCommand<ActionReport>(
      "ensure_terminal_cli_available",
      undefined,
      { ok: true, message: "Locality terminal command is ready." },
    );
    if (!report.ok) {
      setMountOnboarding(failedMountOnboardingReport(report.message));
      return false;
    }
    setMountOnboarding(null);
    return true;
  }

  async function chooseFolder() {
    if (mountStartRequestedRef.current || mounting) {
      return;
    }

    try {
      const selected = await callCommand<string | null>(
        "choose_mount_folder",
        { current: mountPath },
        null,
      );
      if (selected) {
        setMountPathDirty(true);
        setMountPath(selected.replace(/\/$/, ""));
      }
    } catch (error) {
      setMountOnboarding(failedMountOnboardingReport(errorMessage(error)));
    }
  }

  async function openMountFolder() {
    const report = await callCommand<ActionReport>(
      "open_path",
      { path: mountPath },
      { ok: true, message: "Opened demo folder." },
    );
    if (!report.ok) {
      setMountOnboarding(failedMountOnboardingReport(report.message));
      return;
    }
    setMountOnboarding(null);
  }

  function finishOnboarding() {
    onComplete();
  }

  function openOptionalGuide(returnStep: OnboardingStep) {
    setOptionalGuideReturnStep(returnStep);
    setStep(2);
  }

  function closeOptionalGuide() {
    const nextStep = optionalGuideReturnStep ?? 3;
    setOptionalGuideReturnStep(null);
    setStep(nextStep);
  }

  function continueFromOptionalGuide() {
    if (optionalGuideReturnStep === 5) {
      setOptionalGuideReturnStep(null);
      finishOnboarding();
      return;
    }
    setOptionalGuideReturnStep(null);
    if (connectionReadyNow) {
      setStep(connectorSkipsMountStep(selectedOnboardingConnector) ? 5 : 4);
      return;
    }
    setStep(3);
  }

  async function locatePage() {
    if (!locateUrl.trim()) {
      return;
    }

    setLocateState("preparing");
    setLocateError("");
    try {
      const item = await callCommand<LocatedItem>(
        "locate_notion_page",
        { url: locateUrl },
        {
          title: "Roadmap 2026",
          kind: "Page",
          localPath: "~/Library/CloudStorage/Locality/notion/Engineering/Roadmap 2026/page.md",
          state: "ready",
        },
      );
      setLocatedItem(item);
      setLocateState("ready");
    } catch (error) {
      setLocateError(errorMessage(error));
      setLocateState("error");
      setLocatedItem(null);
    }
  }

  const workspaceLabel = connectedWorkspace || snapshot.connection.workspaceName || "Your workspace";
  const finalPrompt = selectedOnboardingConnector === "notion" && agentGuidanceReport?.prompt
    ? agentGuidanceReport.prompt
    : suggestedAgentPrompt(mountPath, selectedOnboardingConnector);
  const mountSetupError =
    mountOnboarding?.state === "failed"
      ? classifyMountSetupError(mountOnboarding.message)
      : null;
  const showRecoveryChooser = mountRecoveryEnabled(mountSetupError);
  const fileProviderGuideVisible =
    mountOnboarding?.state === "needs_finder_enable" ||
    mountOnboarding?.state === "waiting_for_cloudstorage_root";
  const displayedFileProviderEnablement = fileProviderEnablement ??
    (mountOnboarding?.state === "waiting_for_cloudstorage_root"
      ? {
          state: "waiting_for_root" as const,
          message: "Finishing the Locality folder setup.",
          path: mountPath,
        }
      : {
          state: "needs_finder_enable" as const,
          message: "In Finder, click Enable for Locality.",
          path: mountPath,
        });

  return (
    <main className="setup-shell">
      <section className="setup-window">
        <WindowChrome title="Locality Setup" meta={onboardingStepMeta(step)} />
        {step === 1 && (
          <SetupContent variant="hero" side={<ProductLoopDemo />}>
            <div>
              <div className="eyebrow">Meet Locality</div>
              <h1>Turn work apps into agent-ready files.</h1>
              <p>
                Locality turns tools like Notion into a local folder. Agents edit Markdown you can
                inspect, while Locality keeps the connected app in sync after review.
              </p>
            </div>
            <div className="button-row">
              <PrimaryButton onClick={() => setStep(3)}>Get Started</PrimaryButton>
              <SecondaryButton onClick={() => openOptionalGuide(1)}>How agents use it</SecondaryButton>
            </div>
            <div className="onboarding-pill-row">
              <span>Finder-native files</span>
              <span>Markdown edits</span>
              <span>Review before sync</span>
            </div>
          </SetupContent>
        )}

        {step === 2 && (
          <SetupContent side={<AgentWorkspaceDemo />}>
            <div>
              <div className="eyebrow">Optional agent guide</div>
              <h1>Agents work in files you can see.</h1>
              <p>
                Each connected source appears as a local folder. Pages and docs become page.md files.
                Locality writes guidance files so agents understand what to edit, when to stop, and
                how Review Center protects the remote app.
              </p>
            </div>
            <div className="button-row">
              <PrimaryButton onClick={continueFromOptionalGuide}>
                {optionalGuideReturnStep === 5 ? "Open Locality" : "Continue Setup"}
              </PrimaryButton>
              {optionalGuideReturnStep && (
                <SecondaryButton onClick={closeOptionalGuide}>Back</SecondaryButton>
              )}
            </div>
          </SetupContent>
        )}

        {step === 3 && (
          <SetupContent
            side={
              <ConnectorOptions
                selected={selectedOnboardingConnector}
                connectedConnector={connectionReadyNow ? selectedOnboardingConnector : null}
                busy={selectedConnectorBusy}
                onSelect={selectOnboardingConnector}
              />
            }
          >
            <div>
              <div className="eyebrow">Connect source</div>
              {(selectedConnectorBusy || connectionReadyNow) && (
                <div className={`sync-note ${connectionReadyNow ? "connected" : ""}`}>
                  {connectionReadyNow ? <Check /> : <Loader2 className="spin-icon" />}
                  {connectionReadyNow ? `${selectedSourceName} connected` : `Waiting for ${selectedSourceName}`}
                </div>
              )}
              <h1>{onboardingConnectorTitle(selectedOnboardingConnector, connectionReadyNow, selectedConnectorBusy)}</h1>
              <p>
                {onboardingConnectorDescription(
                  selectedOnboardingConnector,
                  connectionReadyNow,
                  selectedConnectorBusy,
                  workspaceLabel,
                )}
              </p>
            </div>
            {connectorUsesOAuth(selectedOnboardingConnector) && oauthInFlight && !connectionReadyNow && (
              <ProgressList
                items={selectedOnboardingConnector === "notion"
                  ? [
                      { label: "Browser opened", state: oauthError ? "idle" : "done" },
                      { label: "Select workspace and pages", state: "active" },
                      { label: "Approve access", state: "idle" },
                    ]
                  : [
                      { label: "Browser opened", state: oauthError ? "idle" : "done" },
                      { label: `Approve ${selectedSourceName} access`, state: "active" },
                      { label: "Create local folder", state: "idle" },
                    ]}
              />
            )}
            {sourceRequiresApiKey(selectedOnboardingConnector) && !connectionReadyNow && (
              <label className="source-inline-field onboarding-source-field">
                <span>{selectedSourceName} API key</span>
                <input
                  type="password"
                  autoComplete="off"
                  value={selectedApiKey}
                  placeholder="Paste API key"
                  disabled={connectorConnecting}
                  onChange={(event) => {
                    if (selectedOnboardingConnector === "linear") {
                      setLinearApiKey(event.target.value);
                    } else {
                      setGranolaApiKey(event.target.value);
                    }
                  }}
                />
              </label>
            )}
            {selectedOnboardingConnector === "google-docs" && !connectionReadyNow && (
              <label className="source-inline-field onboarding-source-field">
                <span>Drive folder</span>
                <input
                  value={googleDocsWorkspaceFolder}
                  placeholder="Folder name, URL, or ID"
                  disabled={oauthInFlight}
                  onChange={(event) => setGoogleDocsWorkspaceFolder(event.target.value)}
                />
              </label>
            )}
            <div className="button-row">
              <PrimaryButton
                busy={selectedConnectorBusy && !connectionReadyNow}
                disabled={
                  !connectionReadyNow &&
                  (
                    (sourceRequiresApiKey(selectedOnboardingConnector) && !selectedApiKey.trim()) ||
                    (selectedOnboardingConnector === "google-docs" && !googleDocsWorkspaceFolder.trim())
                  )
                }
                onClick={
                  connectionReadyNow
                    ? () => setStep(connectorSkipsMountStep(selectedOnboardingConnector) ? 5 : 4)
                    : () => void connectSelectedOnboardingConnector()
                }
              >
                {connectionReadyNow
                  ? "Continue"
                  : selectedConnectorBusy
                    ? `Connecting ${selectedSourceName}`
                    : `Connect ${selectedSourceName}`}
              </PrimaryButton>
              {selectedOnboardingConnector === "notion" && (
                <SecondaryButton
                  disabled={copyLoginLinkDisabled({
                    connectionReady: connectionReadyNow,
                    oauthInFlight,
                  })}
                  onClick={() => void copyLoginLink()}
                >
                  Copy login link
                </SecondaryButton>
              )}
            </div>
            <div className="onboarding-pill-row">
              {onboardingConnectorPills(selectedOnboardingConnector).map((label) => (
                <span key={label}>{label}</span>
              ))}
            </div>
            {loginCopyMessage && <p className="quiet-note inline-note">{loginCopyMessage}</p>}
            {oauthError && <p className="field-error">{oauthError}</p>}
          </SetupContent>
        )}

        {step === 4 && (
          <SetupContent
            mark={<BrandTile variant={mountOnboarding?.state === "failed" ? "folder" : "progress"} />}
            variant="wide"
          >
            <div>
              <div className="eyebrow">Local folder</div>
              <h1>
                {fileProviderGuideVisible
                  ? fileProviderEnablementHeadline(displayedFileProviderEnablement)
                  : mountOnboardingHeadline(mountOnboarding)}
              </h1>
              <p>
                {fileProviderGuideVisible
                  ? displayedFileProviderEnablement.state === "needs_finder_enable"
                    ? "Finder is open to the Locality location. Click Enable there; this screen will continue automatically."
                    : displayedFileProviderEnablement.message
                  : mountOnboarding?.message ??
                    `Locality is creating your ${selectedSourceName} folder under the default CloudStorage root and verifying that files appear locally.`}
              </p>
            </div>
            {fileProviderGuideVisible && (
              <FinderEnableGuide
                waitingForRoot={displayedFileProviderEnablement.state === "waiting_for_root"}
              />
            )}
            <div className={`sync-note${displayedFileProviderEnablement.state === "ready" ? " connected" : ""}`}>
              {mounting ? (
                <Loader2 className="spin-icon" />
              ) : mountOnboarding?.state === "failed" ? (
                <AlertTriangle />
              ) : fileProviderGuideVisible ? (
                <Loader2 className="spin-icon" />
              ) : (
                <FolderOpen />
              )}
              {fileProviderGuideVisible
                ? fileProviderEnablementStatusLabel(displayedFileProviderEnablement)
                : mounting
                ? "Checking File Provider approval"
                : mountOnboarding?.message ?? `Creating folder and preparing ${selectedSourceName} files`}
            </div>
            <div className="path-field ready-path-field">
              <span>{mountPath}</span>
            </div>
            {fileProviderGuideVisible ? (
              <>
                <div className="button-row">
                  <PrimaryButton onClick={() => void revealFileProviderEnablement()}>
                    Reopen Finder
                  </PrimaryButton>
                  <SecondaryButton onClick={() => setFinderHelpOpen((open) => !open)}>
                    Having trouble?
                  </SecondaryButton>
                </div>
                {finderHelpOpen && (
                  <div className="finder-enable-help">
                    <strong>Look under Locations in the Finder sidebar.</strong>
                    <span>
                      Select Locality and click Enable. If Locality is missing, reopen Finder first,
                      then verify Locality under File Providers in System Settings.
                    </span>
                  </div>
                )}
                {finderRevealError && <p className="field-error">{finderRevealError}</p>}
              </>
            ) : showRecoveryChooser ? (
              <div className="button-row">
                <PrimaryButton
                  busy={mounting}
                  disabled={!mountPath.trim()}
                  onClick={() => void runMountOnboarding(mountOnboardingNextAction(mountOnboarding))}
                >
                  {mountOnboardingPrimaryLabel(mountOnboarding, mounting)}
                </PrimaryButton>
                <SecondaryButton disabled={mounting} onClick={() => void chooseFolder()}>
                  Choose Folder
                </SecondaryButton>
              </div>
            ) : (
              <PrimaryButton
                busy={mounting}
                disabled={!mountPath.trim()}
                onClick={() => void runMountOnboarding(mountOnboardingNextAction(mountOnboarding))}
              >
                {mountOnboardingPrimaryLabel(mountOnboarding, mounting)}
              </PrimaryButton>
            )}
            {!fileProviderGuideVisible && mountOnboardingNeedsInstructions(mountOnboarding) && (
              <p className="quiet-note">{mountOnboardingInstructions(mountOnboarding)}</p>
            )}
            {!fileProviderGuideVisible && mountOnboardingSupplementaryNote(mountOnboarding) && (
              <p className="quiet-note">{mountOnboardingSupplementaryNote(mountOnboarding)}</p>
            )}
            <p className="quiet-note">
              Locality uses the default CloudStorage location so Finder and your agents see the
              same source folder automatically.
            </p>
          </SetupContent>
        )}

        {step === 5 && (
          <SetupContent mark={<BrandTile variant="ready" />} variant="final">
            <div>
              <h1>Locality is ready!</h1>
              <p>{onboardingReadyCopy(selectedOnboardingConnector)}</p>
            </div>
            {mountOnboarding && <p className="field-error">{mountOnboarding.message}</p>}
            <div className="final-actions">
              <PrimaryButton onClick={finishOnboarding}>
                Open Locality
              </PrimaryButton>
              <SecondaryButton onClick={() => openOptionalGuide(5)}>
                View Optional Guide
              </SecondaryButton>
            </div>
            <div className="folder-inline final-folder-card">
              <div className="ready-head">
                <div>
                  <strong>Folder</strong>
                  <p>Your {selectedSourceName} files are mounted here.</p>
                </div>
                <span className="onboarding-pill">Mounted</span>
              </div>
              <div className="path-field ready-path-field">
                <span>{mountPath}</span>
                <SecondaryButton onClick={() => void openMountFolder()}>
                  Open Folder
                </SecondaryButton>
              </div>
            </div>
            <div className="agent-demo compact-agent-demo">
              <div className="agent-demo-header">
                <div>
                  <strong>Try this agent prompt</strong>
                  <p>{onboardingPromptHint(selectedOnboardingConnector)}</p>
                </div>
                <SecondaryButton
                  onClick={() => copyText(finalPrompt)}
                >
                  Copy
                </SecondaryButton>
              </div>
              <div className="agent-demo-command">{finalPrompt}</div>
            </div>
          </SetupContent>
        )}
      </section>
    </main>
  );
}

function AgentGuidanceSummary({
  report,
  state,
}: {
  report: AgentGuidanceInstallReport | null;
  state: "idle" | "installing" | "ready" | "error";
}) {
  const installedAgents = compactAgentNames(report?.targets.filter((target) => target.status === "installed") || []);
  const fallbackTargets = report?.targets.filter((target) => target.status === "available").slice(0, 2) || [];
  const failed = report?.targets.some((target) => target.status === "failed") || state === "error";
  const title =
    state === "installing"
      ? "Preparing agents"
      : failed
        ? "Agent skills need attention"
        : report
          ? "Agents can use Locality"
          : "Preparing agents";

  return (
    <div className={`agent-guidance-card ${failed ? "warning" : ""}`}>
      <div className="agent-demo-title">
        {state === "installing" ? <Loader2 className="spin-icon" /> : failed ? <AlertTriangle /> : <Bot />}
        <span>{title}</span>
      </div>
      {state === "installing" && <p>Installing the Locality skill for local agents.</p>}
      {state !== "installing" && installedAgents.length > 0 && (
        <p>
          Now your agents know how to use <code>loc</code> to view and edit Notion. Installed for{" "}
          <strong>{formatList(installedAgents)}</strong>.
        </p>
      )}
      {state !== "installing" && installedAgents.length === 0 && fallbackTargets.length > 0 && (
        <p>{fallbackTargets[0].detail}</p>
      )}
      {state !== "installing" && installedAgents.length === 0 && fallbackTargets.length === 0 && !failed && (
        <p>Locality is preparing local agent instructions for this source.</p>
      )}
      {failed && report?.targets.find((target) => target.status === "failed")?.detail && (
        <p>{report.targets.find((target) => target.status === "failed")?.detail}</p>
      )}
    </div>
  );
}

function compactAgentNames(targets: AgentGuidanceTarget[]) {
  const names = targets.map((target) => {
    if (target.agent.includes("Claude")) return "Claude";
    if (target.agent.includes("Copilot")) return "Copilot";
    if (target.agent.includes("AGENTS.md")) return "AGENTS.md";
    return target.agent;
  });
  return Array.from(new Set(names));
}

function formatList(items: string[]) {
  if (items.length <= 2) {
    return items.join(" and ");
  }
  return `${items.slice(0, -1).join(", ")} and ${items[items.length - 1]}`;
}

function MainShell({
  snapshot,
  view,
  onViewChange,
  reviewInitialFilter,
  onOpenReview,
  theme,
  onThemeChange,
  onRefresh,
  updateStatus,
  onCheckForUpdate,
  onInstallUpdate,
  appStoreDistribution,
  onResetComplete,
}: {
  snapshot: DesktopSnapshot;
  view: AppView;
  onViewChange: (view: AppView) => void;
  reviewInitialFilter: ReviewFilter;
  onOpenReview: (filter?: ReviewFilter) => void;
  theme: AppTheme;
  onThemeChange: (theme: AppTheme) => void;
  onRefresh: () => Promise<void>;
  updateStatus: UpdateStatus;
  onCheckForUpdate: (options?: AppUpdateCheckOptions) => Promise<void>;
  onInstallUpdate: () => Promise<void>;
  appStoreDistribution: boolean;
  onResetComplete: () => void;
}) {
  const meta = chromeStatusLabel(snapshot);
  const statusTitle = healthDescription(snapshot.health.state, snapshot.health.attentionCount);
  const statusTarget = chromeStatusTarget(snapshot);
  const [selectedMountId, setSelectedMountId] = useState<string | null>(null);
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false);
  const mountTableRows = useMemo(
    () => mountRows(snapshot.mounts, snapshot.mount, snapshot.activeMountId),
    [snapshot.activeMountId, snapshot.mount, snapshot.mounts],
  );
  const selectedMount = selectedMountRow(mountTableRows, selectedMountId);

  useEffect(() => {
    const nextSelectedMountId = selectedMountIdAfterViewChange(selectedMountId, view);
    if (nextSelectedMountId !== selectedMountId) {
      setSelectedMountId(nextSelectedMountId);
    }
  }, [selectedMountId, view]);

  useEffect(() => {
    if (selectedMountId && !selectedMount) {
      setSelectedMountId(null);
    }
  }, [selectedMount, selectedMountId]);

  useEffect(() => {
    const clearSelectionForMountOpen = (event: Event) => {
      const nextView = (event as CustomEvent<string>).detail;
      const nextSelectedMountId = selectedMountIdAfterOpenViewEvent(selectedMountId, nextView);
      if (nextSelectedMountId !== selectedMountId) {
        setSelectedMountId(nextSelectedMountId);
      }
    };

    window.addEventListener("loc-open-view", clearSelectionForMountOpen);
    return () => window.removeEventListener("loc-open-view", clearSelectionForMountOpen);
  }, [selectedMountId]);

  function openMountsView() {
    setSelectedMountId(null);
    onViewChange("mount");
  }

  function openReviewCenter(filter: ReviewFilter = "all") {
    setSelectedMountId(null);
    onOpenReview(filter);
  }

  function openStatusTarget() {
    if (statusTarget) {
      if (statusTarget === "pending") {
        openReviewCenter();
        return;
      }
      onViewChange(statusTarget);
      return;
    }
    openMountsView();
  }

  return (
    <main className="app-frame">
      <WindowChrome
        title="Locality"
        meta={meta}
        metaTitle={statusTitle}
        onMetaClick={statusTarget ? openStatusTarget : undefined}
      />
      <div className={`app-shell ${sidebarCollapsed ? "sidebar-collapsed" : ""}`}>
        <aside className="sidebar">
          <div className="sidebar-brand">
            <span className="sidebar-brand-mark">
              <LocalityLogo surface="light" />
              <strong>Locality</strong>
            </span>
            <button
              className="sidebar-collapse-button has-tooltip"
              type="button"
              data-tooltip={sidebarCollapsed ? "Expand sidebar" : "Collapse sidebar"}
              aria-label={sidebarCollapsed ? "Expand sidebar" : "Collapse sidebar"}
              aria-pressed={sidebarCollapsed}
              onClick={() => setSidebarCollapsed((collapsed) => !collapsed)}
            >
              {sidebarCollapsed ? <PanelLeftOpen /> : <PanelLeftClose />}
            </button>
          </div>
          <nav>
            <SidebarButton active={view === "home"} icon={<Home />} onClick={() => onViewChange("home")}>
              {PRODUCT_TERMS.home}
            </SidebarButton>
            <SidebarButton active={view === "mount"} icon={<FolderOpen />} onClick={openMountsView}>
              {PRODUCT_TERMS.sources}
            </SidebarButton>
            <SidebarButton
              active={view === "pending" || view === "review"}
              icon={<ListChecks />}
              onClick={() => openReviewCenter()}
            >
              {PRODUCT_TERMS.reviewCenter}
            </SidebarButton>
            <SidebarButton
              active={view === "settings"}
              icon={<Settings />}
              onClick={() => onViewChange("settings")}
            >
              {PRODUCT_TERMS.settings}
            </SidebarButton>
          </nav>
          <SidebarUpdateNotice
            status={updateStatus}
            onInstall={onInstallUpdate}
          />
          <div className="sidebar-status">
            <button
              className="status-button"
              title={statusTitle}
              onClick={openStatusTarget}
            >
              <StatusPill tone={healthTone(snapshot.health.state)} title={statusTitle}>
                {sidebarStatusLabel(snapshot)}
              </StatusPill>
            </button>
          </div>
        </aside>

        <section className="content">
          {view === "home" && (
            <HomeView
              snapshot={snapshot}
              onMount={openMountsView}
              onFiles={() => onViewChange("files")}
              onReview={openReviewCenter}
              onRefresh={onRefresh}
            />
          )}
          {view === "mount" && selectedMount && (
            <MountDetailView
              snapshot={snapshot}
              mount={selectedMount.mount}
              onHome={() => onViewChange("home")}
              onMounts={() => setSelectedMountId(null)}
              onRefresh={onRefresh}
              onReview={() => openReviewCenter()}
            />
          )}
          {view === "files" && (
            <FilesView
              snapshot={snapshot}
              onHome={() => onViewChange("home")}
              onRefresh={onRefresh}
              onReview={() => openReviewCenter()}
            />
          )}
          {view === "mount" && !selectedMount && (
            <MountsView
              snapshot={snapshot}
              rows={mountTableRows}
              onHome={() => onViewChange("home")}
              onRefresh={onRefresh}
              onSelectMount={(mountId: string) => setSelectedMountId(mountId)}
            />
          )}
          {view === "pending" && (
            <PendingView
              snapshot={snapshot}
              onHome={() => onViewChange("home")}
              onReview={() => onViewChange("review")}
              onRefresh={onRefresh}
              initialFilter={reviewInitialFilter}
            />
          )}
          {view === "review" && (
            <ReviewView
              snapshot={snapshot}
              onHome={() => onViewChange("home")}
              onPending={() => openReviewCenter()}
              onRefresh={onRefresh}
              onDone={() => onViewChange("activity")}
            />
          )}
          {view === "activity" && <ActivityView snapshot={snapshot} onHome={() => onViewChange("home")} />}
          {view === "settings" && (
            <SettingsView
              snapshot={snapshot}
              onHome={() => onViewChange("home")}
              onRefresh={onRefresh}
              updateStatus={updateStatus}
              onCheckForUpdate={onCheckForUpdate}
              onInstallUpdate={onInstallUpdate}
              appStoreDistribution={appStoreDistribution}
              theme={theme}
              onThemeChange={onThemeChange}
              onActivity={() => onViewChange("activity")}
              onSources={openMountsView}
              onResetComplete={onResetComplete}
            />
          )}
        </section>
      </div>
    </main>
  );
}

function SidebarUpdateNotice({
  status,
  onInstall,
}: {
  status: UpdateStatus;
  onInstall: () => Promise<void>;
}) {
  if (!updateNoticeVisible(status)) {
    return null;
  }

  const disabled = updateInstallActionDisabled(status);
  const title = updateSidebarTitle(status);
  const subtitle = updateSidebarSubtitle(status);
  return (
    <button
      type="button"
      className="sidebar-update"
      disabled={disabled}
      aria-label={status.version ? `${title}, version ${status.version}` : title}
      onClick={() => void onInstall()}
    >
      <span className="sidebar-update-copy">
        <strong>{title}</strong>
        <small>{subtitle}</small>
      </span>
      <span className="sidebar-update-arrow" aria-hidden="true">
        <ChevronRight />
      </span>
    </button>
  );
}

function HomeView({
  snapshot,
  onMount,
  onFiles,
  onReview,
  onRefresh,
}: {
  snapshot: DesktopSnapshot;
  onMount: () => void;
  onFiles: () => void;
  onReview: (filter?: ReviewFilter) => void;
  onRefresh: () => Promise<void>;
}) {
  const [url, setUrl] = useState("");
  const [locateState, setLocateState] = useState<LocateState>("idle");
  const [locateError, setLocateError] = useState("");
  const [locatedItem, setLocatedItem] = useState<LocatedItem | null>(null);
  const [actionError, setActionError] = useState("");
  const {
    liveModeEnabled,
    liveModeBusy,
    liveModeState,
    liveModeMessage,
    toggleLiveMode,
  } = useMountLiveModeController(snapshot, onRefresh);
  const hasPendingChanges = snapshot.pendingChanges.length > 0;
  const homeReviewCounts = reviewQueueCounts(snapshot.pendingChanges);

  async function connectNotion() {
    setActionError("");
    const report = await callCommand<ActionReport>(
      "connect_notion",
      undefined,
      { ok: true, message: "Connected demo workspace." },
    );
    if (!report.ok) {
      setActionError(report.message);
      return;
    }
    await onRefresh();
  }

  async function createMount() {
    setActionError("");
    const report = await callCommand<ActionReport>(
      "create_workspace_mount",
      { path: snapshot.mount.localPath },
      { ok: true, message: "Created demo mount." },
    );
    if (!report.ok) {
      setActionError(report.message);
      return;
    }
    await onRefresh();
  }

  async function openWorkspaceFolder(path: string) {
    setActionError("");
    const report = await callCommand<ActionReport>(
      "open_path",
      { path },
      { ok: true, message: "Opened demo folder." },
    );
    if (!report.ok) {
      setActionError(report.message);
    }
  }

  async function locatePage() {
    if (!url.trim()) {
      return;
    }
    setLocateState("preparing");
    setLocateError("");
    try {
      const item = await callCommand<LocatedItem>(
        "locate_notion_page",
        { url },
        {
          title: "Roadmap 2026",
          kind: "Page",
          localPath: "~/Library/CloudStorage/Locality/notion/Engineering/Roadmap 2026/page.md",
          state: "ready",
        },
      );
      setLocatedItem(item);
      setLocateState("ready");
    } catch (error) {
      setLocateError(errorMessage(error));
      setLocateState("error");
      setLocatedItem(null);
    }
  }

  return (
    <div className="view-stack">
      <ViewHeader eyebrow={PRODUCT_TERMS.home} title="Notion workspace">
        <StatusPill
          tone={healthTone(snapshot.health.state)}
          title={healthDescription(snapshot.health.state, snapshot.health.attentionCount)}
        >
          {healthLabel(snapshot.health.state)}
        </StatusPill>
      </ViewHeader>

      {connectionMissing(snapshot) ? (
        <section className="empty-action-panel">
          <BrandTile variant="notion">N</BrandTile>
          <div>
            <h2>Connect your Notion workspace</h2>
            <p>Locality needs access before it can create local files for agents.</p>
          </div>
          <PrimaryButton icon={<ChevronRight />} onClick={() => void connectNotion()}>
            Connect Notion
          </PrimaryButton>
          {actionError && <p className="field-error">{actionError}</p>}
        </section>
      ) : mountMissing(snapshot) ? (
        <section className="empty-action-panel">
          <BrandTile variant="folder" />
          <div>
            <h2>Create your Notion folder</h2>
            <p>Use the default notion folder under the shared Locality CloudStorage root.</p>
          </div>
          <PrimaryButton
            icon={<FolderOpen />}
            onClick={() => void createMount()}
          >
            Create Notion Folder
          </PrimaryButton>
          {actionError && <p className="field-error">{actionError}</p>}
        </section>
      ) : (
        <>
          <section className="home-stat-grid" aria-label="Workspace summary">
            <button type="button" className="home-stat" onClick={onMount}>
              <span>Sources</span>
              <strong>{snapshot.mounts.length || 1}</strong>
            </button>
            <button type="button" className="home-stat" onClick={() => onReview()}>
              <span>Awaiting review</span>
              <strong className={homeReviewCounts.total > 0 ? "warn" : ""}>{homeReviewCounts.total}</strong>
            </button>
            <button type="button" className="home-stat" onClick={() => onReview("problems")}>
              <span>Problems</span>
              <strong className={homeReviewCounts.problems > 0 ? "danger" : ""}>
                {homeReviewCounts.problems > 0 ? homeReviewCounts.problems : "0"}
              </strong>
            </button>
          </section>
          <section className="workspace-card">
            <div className="workspace-summary">
              <p className="label">{PRODUCT_TERMS.connectedSource}</p>
              <h2>{snapshot.mount.workspaceName}</h2>
              <p className="path-line">{snapshot.mount.localPath}</p>
            </div>
            <div className="workspace-actions">
              <button
                className={`live-mode-control has-tooltip ${liveModeEnabled ? "active" : ""}`}
                aria-pressed={liveModeEnabled}
                aria-label={`${liveModeEnabled ? "Turn off" : "Turn on"} Live Mode`}
                data-tooltip={liveModeTooltip(liveModeEnabled)}
                title={liveModeTooltip(liveModeEnabled)}
                onClick={toggleLiveMode}
              >
                <span className="live-mode-copy">
                  {liveModeBusy ? <span className="live-mode-spinner" aria-hidden="true" /> : <Zap />}
                  <span>Live Mode</span>
                </span>
                <span className={`toggle ${liveModeEnabled ? "enabled" : ""}`} aria-hidden="true">
                  <i />
                </span>
              </button>
              <SecondaryButton icon={<FolderOpen />} onClick={() => void openWorkspaceFolder(snapshot.mount.localPath)}>
                Open Folder
              </SecondaryButton>
              <SecondaryButton icon={<ChevronRight />} onClick={onFiles}>
                Files
              </SecondaryButton>
              <SecondaryButton icon={<FolderOpen />} onClick={onMount}>
                View Sources
              </SecondaryButton>
            </div>
          </section>
          {actionError && <p className="field-error">{actionError}</p>}
          {liveModeMessage && (
            <p className={liveModeState === "error" ? "field-error" : "quiet-note inline-note"}>
              {liveModeMessage}
            </p>
          )}

          <section className="panel locate-panel">
            <LocateBox
              label="Open a Notion page"
              value={url}
              onChange={(next) => {
                setUrl(next);
                setLocateState("idle");
                setLocatedItem(null);
              }}
              onSubmit={locatePage}
              onSelect={(item) => {
                setLocatedItem(item);
                setLocateState("ready");
                setLocateError("");
                setUrl(item.title);
              }}
              state={locateState}
              error={locateError}
            />
            {locatedItem && <LocatedPath item={locatedItem} />}
          </section>
          <RecentFilesPanel items={snapshot.recentFiles} onOpenFiles={onFiles} compact />
        </>
      )}

      {hasPendingChanges ? (
        <section className="attention-panel">
          <div>
            <p className="label">{PRODUCT_TERMS.reviewCenter}</p>
            <h2>{snapshot.pendingChanges.length} files need review.</h2>
          </div>
          <PrimaryButton icon={<ListChecks />} onClick={onReview}>
            Open Review Center
          </PrimaryButton>
        </section>
      ) : (
        <section className="panel muted-panel">
          <Check />
          <div>
            <h2>No review needed</h2>
            <p>Safe local edits can sync automatically. Anything risky will appear in Review Center.</p>
          </div>
        </section>
      )}

    </div>
  );
}

function MountsView({
  snapshot,
  rows,
  onHome,
  onRefresh,
  onSelectMount,
}: {
  snapshot: DesktopSnapshot;
  rows: MountRow[];
  onHome: () => void;
  onRefresh: () => Promise<void>;
  onSelectMount: (mountId: string) => void;
}) {
  const [actionError, setActionError] = useState("");
  const [actionMessage, setActionMessage] = useState("");
  const [creating, setCreating] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [backupMountId, setBackupMountId] = useState<string | null>(null);
  const [sourceDialogOpen, setSourceDialogOpen] = useState(false);
  const [sourceDialogState, setSourceDialogState] = useState<SourceSetupState>("idle");
  const [sourceDialogConnector, setSourceDialogConnector] = useState<SourceConnectorId | null>(null);
  const [sourceDialogMessage, setSourceDialogMessage] = useState("");
  const [sourceFileProviderEnablement, setSourceFileProviderEnablement] = useState<FileProviderEnablementReport | null>(null);
  const [pendingMountRetry, setPendingMountRetry] = useState<{
    connector: SourceConnectorId;
    googleDocsWorkspaceFolder?: string;
  } | null>(null);
  const sourceFinderRevealRequestedRef = useRef(false);
  const sourceSetupBusy = sourceSetupIsBusy(sourceDialogState);
  const readyToMountSources = connectedSourcesReadyToMount(snapshot);
  const hasVisibleSources = rows.length > 0 || readyToMountSources.length > 0;

  useEffect(() => {
    if (!pendingMountRetry) {
      sourceFinderRevealRequestedRef.current = false;
      return;
    }

    let completionTimer: number | null = null;
    const poller = createFileProviderEnablementPoller({
      probe: () =>
        callCommand<FileProviderEnablementReport>(
          "file_provider_enablement_status",
          undefined,
          {
            state: "ready",
            message: "Locality is enabled in Finder.",
            path: sourceDefaultPath(snapshot, pendingMountRetry.connector),
          },
        ),
      onReport: (report) => {
        setSourceFileProviderEnablement(report);
        if (report.state === "unavailable") {
          setSourceDialogMessage(report.message);
          setSourceDialogState("error");
        }
      },
      onReady: (report) => {
        setSourceFileProviderEnablement(report);
        completionTimer = window.setTimeout(async () => {
          const mountReport = await createConnectorMount(
            pendingMountRetry.connector,
            pendingMountRetry.googleDocsWorkspaceFolder,
          );
          const outcome = sourceMountRetryOutcome(mountReport);
          if (outcome.kind === "retry") {
            return;
          }
          setPendingMountRetry(null);
          setSourceFileProviderEnablement(null);
          setSourceDialogMessage(outcome.message);
          setSourceDialogState(outcome.kind);
        }, 350);
      },
    });
    const updateVisibility = () => {
      poller.setVisible(document.visibilityState !== "hidden");
    };

    if (!sourceFinderRevealRequestedRef.current) {
      sourceFinderRevealRequestedRef.current = true;
      void revealSourceFileProviderEnablement();
    }
    document.addEventListener("visibilitychange", updateVisibility);
    updateVisibility();
    poller.start();
    return () => {
      poller.stop();
      document.removeEventListener("visibilitychange", updateVisibility);
      if (completionTimer !== null) {
        window.clearTimeout(completionTimer);
      }
    };
  }, [pendingMountRetry]);

  function openAddSourceDialog() {
    setActionError("");
    setActionMessage("");
    if (!sourceSetupBusy) {
      setSourceDialogMessage("");
      setSourceDialogState("idle");
      setSourceDialogConnector(null);
    }
    setSourceDialogOpen(true);
  }

  function beginSourceFileProviderRecovery(
    connector: SourceConnectorId,
    googleDocsWorkspaceFolder: string | undefined,
  ) {
    setActionError("");
    setSourceDialogOpen(true);
    setSourceDialogConnector(connector);
    setSourceDialogState("creating");
    setSourceDialogMessage("");
    setSourceFileProviderEnablement({
      state: "needs_finder_enable",
      message: "In Finder, click Enable for Locality.",
      path: sourceDefaultPath(snapshot, connector),
    });
    setPendingMountRetry({ connector, googleDocsWorkspaceFolder });
  }

  async function revealSourceFileProviderEnablement() {
    const report = await callCommand<ActionReport>(
      "reveal_file_provider_enablement",
      undefined,
      { ok: true, message: "Opened Locality in Finder." },
    ).catch((error) => ({ ok: false, message: errorMessage(error) }));
    if (!report.ok) {
      setSourceDialogMessage(report.message);
    }
  }

  async function createConnectorMount(
    connector: SourceConnectorId,
    googleDocsWorkspaceFolder?: string,
  ): Promise<ActionReport> {
    if (creating) {
      return { ok: false, message: "Source setup is already running." };
    }
    setActionError("");
    setActionMessage("");
    setCreating(true);
    try {
      const report = await callCommand<ActionReport>(
        connector === "notion" ? "create_workspace_mount" : "create_desktop_mount",
        connector === "notion"
          ? { path: sourceDefaultPath(snapshot, "notion") }
          : {
              request: {
                connector,
                path: sourceDefaultPath(snapshot, connector),
                mountId: sourceMountId(connector),
                connectionId: null,
                readOnly: connector === "granola" || connector === "slack",
                notionRootPage: null,
                googleDocsWorkspaceFolder: connector === "google-docs"
                  ? googleDocsWorkspaceFolder?.trim() || "Locality"
                  : null,
              },
            },
        { ok: true, message: "Created demo mount." },
      );
      if (!report.ok) {
        if (classifyMountSetupError(report.message).kind === "file-provider-disabled") {
          beginSourceFileProviderRecovery(connector, googleDocsWorkspaceFolder);
          return report;
        }
        setActionError(report.message);
        return report;
      }
      await onRefresh();
      return report;
    } catch (error) {
      const message = errorMessage(error);
      if (classifyMountSetupError(message).kind === "file-provider-disabled") {
        beginSourceFileProviderRecovery(connector, googleDocsWorkspaceFolder);
        return { ok: false, message };
      }
      setActionError(message);
      return { ok: false, message };
    } finally {
      setCreating(false);
    }
  }

  async function createMount(): Promise<ActionReport> {
    return createConnectorMount("notion");
  }

  async function connectNotionSource(): Promise<ActionReport> {
    setActionError("");
    setActionMessage("");
    const report = await callCommand<ActionReport>(
      "connect_notion",
      undefined,
      { ok: true, message: "Connected demo workspace." },
    );
    if (!report.ok) {
      setActionError(report.message);
      return report;
    }
    await onRefresh();
    return report;
  }

  async function changeNotionSourceAccess(): Promise<ActionReport> {
    setActionError("");
    setActionMessage("");
    const report = await callCommand<ActionReport>(
      "change_notion_access",
      undefined,
      { ok: true, message: "Changed demo Notion access." },
    );
    if (!report.ok) {
      setActionError(report.message);
      return report;
    }
    await onRefresh();
    return report;
  }

  async function connectConnectorSource(connector: SourceConnectorId): Promise<ActionReport> {
    if (connector === "notion") {
      return connectNotionSource();
    }
    if (sourceRequiresApiKey(connector)) {
      return { ok: false, message: `${sourceDisplayName(connector)} requires an API key.` };
    }

    const command = oauthConnectCommand(connector);
    setActionError("");
    setActionMessage("");
    const report = await callCommand<ActionReport>(
      command,
      undefined,
      { ok: true, message: `Connected demo ${sourceDisplayName(connector)} account.` },
    );
    if (!report.ok) {
      setActionError(report.message);
      return report;
    }
    await onRefresh();
    return report;
  }

  async function runSourceDialogAction(
    connector: SourceConnectorId,
    options?: { googleDocsWorkspaceFolder?: string },
  ) {
    if (sourceSetupBusy) {
      return;
    }

    setSourceDialogMessage("");
    setSourceDialogConnector(connector);
    const connectorReady = sourceConnectionReady(snapshot, connector);
    const connectorHasMount = sourceMounted(snapshot, connector);
    const nextState = !connectorReady
      ? "connecting"
      : !connectorHasMount
        ? "creating"
        : connector === "notion"
          ? "changing"
          : "success";
    setSourceDialogState(nextState);
    try {
      if (connectorReady && !connectorHasMount) {
        const mountReport = await createConnectorMount(connector, options?.googleDocsWorkspaceFolder);
        if (classifyMountSetupError(mountReport.message).kind === "file-provider-disabled") {
          return;
        }
        setSourceDialogMessage(mountReport.message);
        setSourceDialogState(mountReport.ok ? "success" : "error");
        return;
      }

      const report = connector === "notion"
        ? connectorReady
          ? await changeNotionSourceAccess()
          : await connectNotionSource()
        : await connectConnectorSource(connector);
      if (!report.ok || connector === "notion") {
        setSourceDialogMessage(report.message);
        setSourceDialogState(report.ok ? "success" : "error");
        return;
      }

      setSourceDialogState("creating");
      const mountReport = await createConnectorMount(connector, options?.googleDocsWorkspaceFolder);
      if (classifyMountSetupError(mountReport.message).kind === "file-provider-disabled") {
        return;
      }
      const message = mountReport.ok
        ? `${report.message} ${mountReport.message}`
        : `${report.message} ${mountReport.message}`;
      setSourceDialogMessage(message);
      setSourceDialogState(mountReport.ok ? "success" : "error");
      if (mountReport.ok) {
        await onRefresh();
      }
    } catch (error) {
      setSourceDialogMessage(errorMessage(error));
      setSourceDialogState("error");
    }
  }

  async function connectApiKeySource(connector: "granola" | "linear", apiKey: string) {
    if (sourceSetupBusy) {
      return;
    }
    const sourceName = sourceDisplayName(connector);
    setSourceDialogMessage("");
    setActionMessage("");
    setSourceDialogConnector(connector);
    setSourceDialogState("connecting");
    try {
      const report = await callCommand<ActionReport>(
        connector === "linear" ? "connect_linear" : "connect_granola",
        { apiKey },
        { ok: true, message: `Connected demo ${sourceName} source.` },
      );
      setSourceDialogMessage(report.message);
      setSourceDialogState(report.ok ? "success" : "error");
      if (!report.ok) {
        setActionError(report.message);
      }
      if (report.ok) {
        await onRefresh();
      }
    } catch (error) {
      const message = errorMessage(error);
      setSourceDialogMessage(message);
      setActionError(message);
      setSourceDialogState("error");
    }
  }

  async function refreshMounts() {
    if (refreshing) {
      return;
    }
    setActionError("");
    setActionMessage("");
    setRefreshing(true);
    try {
      await onRefresh();
    } catch (error) {
      setActionError(errorMessage(error));
    } finally {
      setRefreshing(false);
    }
  }

  async function openMountFolder(path: string) {
    setActionError("");
    setActionMessage("");
    const report = await callCommand<ActionReport>(
      "open_path",
      { path },
      { ok: true, message: "Opened demo folder." },
    );
    if (!report.ok) {
      setActionError(report.message);
    }
  }

  async function exportSourceBackup(mountId: string) {
    if (backupMountId) {
      return;
    }
    setActionError("");
    setActionMessage("");
    setBackupMountId(mountId);
    try {
      const report = await callCommand<ActionReport>(
        "export_source_backup",
        { mountId },
        { ok: true, message: `Exported demo backup for ${mountId}.` },
      );
      if (!report.ok) {
        setActionError(report.message);
        return;
      }
      setActionMessage(report.message);
      await onRefresh().catch(() => undefined);
    } catch (error) {
      setActionError(errorMessage(error));
    } finally {
      setBackupMountId(null);
    }
  }

  return (
    <div className="view-stack mounts-view">
      <Breadcrumbs items={[{ label: PRODUCT_TERMS.home, onClick: onHome }, { label: PRODUCT_TERMS.sources }]} />
      <ViewHeader title={PRODUCT_TERMS.sources}>
        <SecondaryButton
          compact
          busy={sourceSetupBusy}
          icon={<Plus />}
          onClick={openAddSourceDialog}
        >
          Add Source
        </SecondaryButton>
        <SecondaryButton
          compact
          busy={refreshing}
          icon={<RefreshCw />}
          onClick={() => void refreshMounts()}
        >
          Refresh
        </SecondaryButton>
      </ViewHeader>

      {!hasVisibleSources ? (
        <section className="empty-action-panel">
          <BrandTile variant="folder" />
          <div>
            <h2>Add a Notion source</h2>
            <p>Connect a source once, then use its local folder from Files or your editor.</p>
          </div>
          <PrimaryButton
            busy={creating}
            icon={<FolderOpen />}
            onClick={openAddSourceDialog}
          >
            Add Notion Source
          </PrimaryButton>
        </section>
      ) : (
        <>
          <p className="view-copy">
            {rows.length} mounted {rows.length === 1 ? "source" : "sources"}
            {readyToMountSources.length > 0
              ? ` · ${readyToMountSources.length} ready to mount`
              : ""}.
          </p>
          {readyToMountSources.length > 0 && (
            <section className="source-ready-strip" aria-label="Sources ready to mount">
              {readyToMountSources.map((connector) => (
                <article className="source-ready-card" key={connector}>
                  <ConnectorIcon connector={connector} />
                  <div>
                    <strong>{sourceDisplayName(connector)}</strong>
                    <span>Connected. Create the local folder to use it from Files and agents.</span>
                  </div>
                  <StatusPill tone="warn" title="Connection is ready but no local folder exists yet.">
                    Folder needed
                  </StatusPill>
                  <SecondaryButton
                    compact
                    busy={creating}
                    icon={<FolderOpen />}
                    onClick={() => void createConnectorMount(connector)}
                  >
                    Create Folder
                  </SecondaryButton>
                </article>
              ))}
            </section>
          )}
          <section className="mounts-grid" aria-label="Connected sources">
            {rows.map((row) => (
              <article className={`mount-card ${row.active ? "active" : ""}`} key={row.id}>
                <div className="mount-card-top">
                  <button className="mount-card-title" type="button" onClick={() => onSelectMount(row.id)}>
                    <span className="mount-card-icon">
                      <FolderOpen />
                    </span>
                    <span>
                      <strong>{row.title}</strong>
                      <span>{row.subtitle}</span>
                    </span>
                  </button>
                  <StatusPill tone={row.tone} title={row.status}>
                    <span className="mount-status-text">{row.status}</span>
                  </StatusPill>
                </div>
                <div className="mount-card-path">
                  <code title={row.localPath}>{row.displayPath}</code>
                  <button
                    className="icon-button has-tooltip"
                    data-tooltip="Copy path"
                    type="button"
                    onClick={() => {
                      setActionError("");
                      copyText(row.localPath);
                    }}
                  >
                    <Copy />
                  </button>
                  <button
                    className="icon-button has-tooltip"
                    data-tooltip="Open folder"
                    type="button"
                    onClick={() => void openMountFolder(row.localPath)}
                  >
                    <FolderOpen />
                  </button>
                </div>
                <div className="mount-card-meta">
                  {row.active && <span className="primary">Primary</span>}
                  {row.active && <span>{sourceSyncModeLabel(snapshot.liveMode, row.active)}</span>}
                  <span>{row.projection}</span>
                  <span>{row.access}</span>
                  <span>{row.content}</span>
                </div>
                <div className="mount-card-footer">
                  <span>{row.mount.mountId}</span>
                  <button
                    className="mount-details-button"
                    type="button"
                    disabled={backupMountId !== null}
                    onClick={() => void exportSourceBackup(row.id)}
                  >
                    {backupMountId === row.id ? "Exporting" : "Backup"}
                    {backupMountId === row.id ? <Loader2 className="spin-icon" /> : <Download />}
                  </button>
                  <button className="mount-details-button" type="button" onClick={() => onSelectMount(row.id)}>
                    Details
                    <ChevronRight />
                  </button>
                </div>
              </article>
            ))}
          </section>
        </>
      )}
      {actionError && <p className="field-error">{actionError}</p>}
      {actionMessage && <p className="quiet-note inline-note">{actionMessage}</p>}
      {sourceDialogOpen && (
        <AddSourceDialog
          snapshot={snapshot}
          state={sourceDialogState}
          activeConnector={sourceDialogConnector}
          message={sourceDialogMessage}
          fileProviderEnablement={sourceFileProviderEnablement}
          onAction={(connector, options) => void runSourceDialogAction(connector, options)}
          onApiKeyAction={(connector, apiKey) => void connectApiKeySource(connector, apiKey)}
          onReopenFinder={() => void revealSourceFileProviderEnablement()}
          onClose={() => {
            setSourceDialogOpen(false);
          }}
        />
      )}
    </div>
  );
}

function AddSourceDialog({
  snapshot,
  state,
  activeConnector,
  message,
  fileProviderEnablement,
  onAction,
  onApiKeyAction,
  onReopenFinder,
  onClose,
}: {
  snapshot: DesktopSnapshot;
  state: SourceSetupState;
  activeConnector: SourceConnectorId | null;
  message: string;
  fileProviderEnablement: FileProviderEnablementReport | null;
  onAction: (connector: SourceConnectorId, options?: { googleDocsWorkspaceFolder?: string }) => void;
  onApiKeyAction: (connector: "granola" | "linear", apiKey: string) => void;
  onReopenFinder: () => void;
  onClose: () => void;
}) {
  const [query, setQuery] = useState("");
  const [viewMode, setViewMode] = useState<SourceListViewMode>("list");
  const [googleDocsWorkspaceFolder, setGoogleDocsWorkspaceFolder] = useState("Locality");
  const [granolaApiKey, setGranolaApiKey] = useState("");
  const [linearApiKey, setLinearApiKey] = useState("");
  const busy = sourceSetupIsBusy(state);
  const connectors: ConnectorOption[] = [
    {
      id: "notion",
      name: "Notion",
      description: "Pages and databases as folders with page.md files.",
      status: sourceConnectorStatus(snapshot, "notion"),
      keywords: ["notion", "wiki", "pages", "database", "docs"],
      mounted: sourceMounted(snapshot, "notion"),
    },
    {
      id: "google-docs",
      name: "Google Docs",
      description: "Docs and Drive folders through the same local file workflow.",
      status: sourceConnectorStatus(snapshot, "google-docs"),
      keywords: ["google", "docs", "gdocs", "drive", "documents"],
      mounted: sourceMounted(snapshot, "google-docs"),
    },
    {
      id: "google-calendar",
      name: "Google Calendar",
      description: "Primary calendar events as files, new events from reviewed drafts.",
      status: sourceConnectorStatus(snapshot, "google-calendar"),
      keywords: ["google", "calendar", "gcal", "events", "meet"],
      mounted: sourceMounted(snapshot, "google-calendar"),
    },
    {
      id: "gmail",
      name: "Gmail",
      description: "Inbox and sent as readable files, drafts as reviewed outbound mail.",
      status: sourceConnectorStatus(snapshot, "gmail"),
      keywords: ["gmail", "mail", "email", "inbox", "drafts"],
      mounted: sourceMounted(snapshot, "gmail"),
    },
    {
      id: "granola",
      name: "Granola",
      description: "Meeting summaries and raw transcripts as read-only files.",
      status: sourceConnectorStatus(snapshot, "granola"),
      keywords: ["granola", "meetings", "notes", "transcripts", "summaries"],
      mounted: sourceMounted(snapshot, "granola"),
    },
    {
      id: "linear",
      name: "Linear",
      description: "Issues and teams as editable Markdown files.",
      status: sourceConnectorStatus(snapshot, "linear"),
      keywords: ["linear", "issues", "tickets", "projects", "teams"],
      mounted: sourceMounted(snapshot, "linear"),
    },
    {
      id: "slack",
      name: "Slack",
      description: "Recent accessible conversations as read-only Markdown.",
      status: sourceConnectorStatus(snapshot, "slack"),
      keywords: ["slack", "channels", "private channels", "dms", "group dms", "users"],
      mounted: sourceMounted(snapshot, "slack"),
    },
  ];
  const normalizedQuery = query.trim().toLowerCase();
  const visibleConnectors = normalizedQuery
    ? connectors.filter((connector) =>
        [connector.name, connector.description, connector.status, ...connector.keywords]
          .join(" ")
          .toLowerCase()
          .includes(normalizedQuery),
      )
    : connectors;

  return (
    <div className="modal-backdrop" role="presentation">
      <section className="source-modal" role="dialog" aria-modal="true" aria-labelledby="add-source-title">
        <div className="destructive-modal-header">
          <div>
            <p className="label">Add source</p>
            <h2 id="add-source-title">Connect a workspace</h2>
            <p>Choose the system Locality should expose as local files.</p>
          </div>
          <button className="icon-button has-tooltip" data-tooltip="Close" onClick={onClose}>
            <X />
          </button>
        </div>

        {fileProviderEnablement ? (
          <div className="source-file-provider-recovery">
            <div>
              <p className="label">Finder access</p>
              <h3>{fileProviderEnablementHeadline(fileProviderEnablement)}</h3>
              <p>
                {fileProviderEnablement.state === "needs_finder_enable"
                  ? "Click Enable in the Locality Finder window. Source setup will continue automatically."
                  : fileProviderEnablement.message}
              </p>
            </div>
            <FinderEnableGuide
              waitingForRoot={fileProviderEnablement.state === "waiting_for_root"}
            />
            <div className="sync-note">
              {fileProviderEnablement.state === "ready" ? <Check /> : <Loader2 className="spin-icon" />}
              {fileProviderEnablementStatusLabel(fileProviderEnablement)}
            </div>
            <SecondaryButton icon={<FolderOpen />} onClick={onReopenFinder}>
              Reopen Finder
            </SecondaryButton>
          </div>
        ) : (
          <>
        <div className="source-toolbar">
          <label className="source-search-row">
            <Search />
            <input
              autoFocus
              value={query}
              placeholder="Search sources"
              onChange={(event) => setQuery(event.target.value)}
            />
          </label>
          <div className="source-view-toggle" aria-label="Source view">
            <button
              type="button"
              className={viewMode === "list" ? "active" : ""}
              aria-pressed={viewMode === "list"}
              onClick={() => setViewMode("list")}
            >
              <List />
              <span>List</span>
            </button>
            <button
              type="button"
              className={viewMode === "tiles" ? "active" : ""}
              aria-pressed={viewMode === "tiles"}
              onClick={() => setViewMode("tiles")}
            >
              <LayoutGrid />
              <span>Tiles</span>
            </button>
          </div>
        </div>

        <div className="source-list-scroll">
          <div className={`connector-choice-grid ${viewMode}`}>
            {visibleConnectors.map((connector) => {
              const connectorBusy = sourceSetupIsActiveConnector(state, activeConnector, connector.id);
              const apiKeyConnector = sourceRequiresApiKey(connector.id) ? connector.id : null;
              const connected = sourceConnectionReady(snapshot, connector.id);
              const needsConnection = !connected;
              const needsFolder = connected && !connector.mounted;
              const connectionDetails = sourceConnectionDetails(snapshot, connector.id);
              const mountDetails = sourceMountDetails(snapshot, connector.id);
              const disabled = busy || (connector.id !== "notion" && connector.mounted);
              const displayedStatus = connectorBusy
                ? sourceSetupProgressLabel(state, connector.mounted)
                : connector.status;
              const actionLabel = sourceActionLabel(connector.id, {
                needsConnection,
                needsFolder,
                mounted: connector.mounted,
              });
              return (
                <article
                  className={`connector-choice-card ${connector.mounted ? "mounted" : "active"}`}
                  aria-disabled={disabled}
                  key={connector.id}
                >
                  <div className="connector-choice-heading">
                    <ConnectorIcon connector={connector.id} />
                    <div>
                      <h3>{connector.name}</h3>
                      <p>{connector.description}</p>
                    </div>
                    <StatusPill
                      tone={
                        connectorBusy
                          ? "warn"
                          : connector.mounted || (connector.id === "notion" && !needsConnection && !needsFolder)
                          ? "ready"
                          : "warn"
                      }
                      title={displayedStatus}
                    >
                      {displayedStatus}
                    </StatusPill>
                  </div>
                  <div className="connector-choice-facts">
                    {connector.id === "notion" ? (
                      <>
                        <SettingRow title="Workspace" value={connectionDetails?.workspaceName || "Not connected"} />
                        <SettingRow title="Local folder" value={sourceDefaultPath(snapshot, connector.id)} />
                        <SettingRow title="Access" value={mountDetails?.accessScope || (connected ? "Ready to mount" : "Not requested")} />
                      </>
                    ) : connector.id === "google-docs" ? (
                      <>
                        <SettingRow title="Workspace folder" value={googleDocsWorkspaceFolder || "Locality"} />
                        <SettingRow title="Local folder" value={sourceDefaultPath(snapshot, connector.id)} />
                      </>
                    ) : connector.id === "google-calendar" ? (
                      <>
                        <SettingRow title="Calendar" value="Primary calendar" />
                        <SettingRow title="Local folder" value={sourceDefaultPath(snapshot, connector.id)} />
                      </>
                    ) : connector.id === "granola" ? (
                      <>
                        <SettingRow title="Content" value="Summaries and transcripts" />
                        <SettingRow title="Local folder" value={sourceDefaultPath(snapshot, connector.id)} />
                      </>
                    ) : connector.id === "linear" ? (
                      <>
                        <SettingRow title="Content" value="Issues by team" />
                        <SettingRow title="Local folder" value={sourceDefaultPath(snapshot, connector.id)} />
                        <SettingRow title="Access" value="Issue edits" />
                      </>
                    ) : connector.id === "slack" ? (
                      <>
                        <SettingRow title="Content" value="Channels, DMs, group DMs, users" />
                        <SettingRow title="Local folder" value={sourceDefaultPath(snapshot, connector.id)} />
                      </>
                    ) : (
                      <>
                        <SettingRow title="Mailboxes" value="Inbox, Sent, Draft" />
                        <SettingRow title="Local folder" value={sourceDefaultPath(snapshot, connector.id)} />
                      </>
                    )}
                  </div>
                  {connector.id === "google-docs" && !connector.mounted && (
                    <label className="source-inline-field">
                      <span>Drive folder</span>
                      <input
                        value={googleDocsWorkspaceFolder}
                        placeholder="Folder name, URL, or ID"
                        onChange={(event) => setGoogleDocsWorkspaceFolder(event.target.value)}
                      />
                    </label>
                  )}
                  {apiKeyConnector && !connector.mounted && needsConnection && (
                    <>
                      <label className="source-inline-field">
                        <span>{connector.name} API key</span>
                        <input
                          type="password"
                          autoComplete="off"
                          value={apiKeyConnector === "linear" ? linearApiKey : granolaApiKey}
                          placeholder="Paste API key"
                          disabled={busy}
                          onChange={(event) => {
                            if (apiKeyConnector === "linear") {
                              setLinearApiKey(event.target.value);
                            } else {
                              setGranolaApiKey(event.target.value);
                            }
                          }}
                        />
                      </label>
                      <p className="quiet-note">
                        {apiKeyConnector === "linear"
                          ? "Create a key in Linear Settings > API > Personal API keys."
                          : "Create a key in Granola Settings > Connectors > API keys. Business or Enterprise is required."}
                      </p>
                    </>
                  )}
                  {connector.mounted && connector.id !== "notion" && !connectorBusy ? (
                    <SecondaryButton compact disabled icon={<Check />}>
                      Mounted
                    </SecondaryButton>
                  ) : apiKeyConnector ? (
                    <PrimaryButton
                      compact
                      busy={connectorBusy}
                      disabled={
                        disabled ||
                        (needsConnection && !(apiKeyConnector === "linear" ? linearApiKey : granolaApiKey).trim())
                      }
                      icon={needsConnection ? <ShieldCheck /> : <FolderOpen />}
                      onClick={() => {
                        if (needsConnection) {
                          onApiKeyAction(
                            apiKeyConnector,
                            apiKeyConnector === "linear" ? linearApiKey : granolaApiKey,
                          );
                        } else {
                          onAction(connector.id);
                        }
                      }}
                    >
                      {connectorBusy ? sourceSetupProgressLabel(state, connector.mounted) : actionLabel}
                    </PrimaryButton>
                  ) : (
                    <PrimaryButton
                      compact
                      busy={connectorBusy}
                      disabled={disabled || (connector.id === "google-docs" && !googleDocsWorkspaceFolder.trim())}
                      icon={sourceActionIcon(connector.id, needsConnection)}
                      onClick={() => onAction(connector.id, { googleDocsWorkspaceFolder })}
                    >
                      {connectorBusy ? sourceSetupProgressLabel(state, connector.mounted) : actionLabel}
                    </PrimaryButton>
                  )}
                </article>
              );
            })}
            {visibleConnectors.length === 0 && (
              <div className="settings-empty-state">No source matched that search.</div>
            )}
          </div>
        </div>
          </>
        )}

        {busy && <p className="quiet-note inline-note">Setup continues if you close this window.</p>}
        {message && <p className={state === "error" ? "field-error" : "quiet-note inline-note"}>{message}</p>}
      </section>
    </div>
  );
}

function sourceDisplayName(connector: SourceConnectorId) {
  switch (connector) {
    case "notion":
      return "Notion";
    case "google-docs":
      return "Google Docs";
    case "google-calendar":
      return "Google Calendar";
    case "gmail":
      return "Gmail";
    case "granola":
      return "Granola";
    case "linear":
      return "Linear";
    case "slack":
      return "Slack";
  }
}

function sourceMountId(connector: SourceConnectorId) {
  switch (connector) {
    case "notion":
      return "notion-main";
    case "google-docs":
      return "google-docs-main";
    case "google-calendar":
      return "google-calendar-main";
    case "gmail":
      return "gmail-main";
    case "granola":
      return "granola-main";
    case "linear":
      return "linear-main";
    case "slack":
      return "slack-main";
  }
}

function sourceConnectionDetails(snapshot: DesktopSnapshot, connector: SourceConnectorId): ConnectionSummary | null {
  const connections = snapshot.connections?.length ? snapshot.connections : [snapshot.connection];
  return (
    connections.find((connection) => connection.connector === connector && sourceConnectionStatusReady(connection.status)) ??
    connections.find((connection) => connection.connector === connector) ??
    null
  );
}

function sourceMountDetails(snapshot: DesktopSnapshot, connector: SourceConnectorId): MountSummary | null {
  return (
    snapshot.mounts.find((mount) => mount.connector === connector) ??
    (snapshot.mount.connector === connector && snapshot.mount.status !== "not_mounted" ? snapshot.mount : null)
  );
}

function sourceConnectorStatus(snapshot: DesktopSnapshot, connector: SourceConnectorId) {
  if (sourceMounted(snapshot, connector)) {
    return "Mounted";
  }
  if (sourceConnectionReady(snapshot, connector)) {
    return "Folder needed";
  }
  if (sourceRequiresApiKey(connector)) {
    return "API key required";
  }
  return "Ready to connect";
}

function sourceConnectionStatusReady(status: string) {
  const normalized = status.trim().toLowerCase();
  return normalized === "active" || normalized === "ready";
}

function sourceDefaultPath(snapshot: DesktopSnapshot, connector: SourceConnectorId) {
  const existing = snapshot.mounts.find((mount) => mount.connector === connector)?.localPath;
  if (existing?.trim()) {
    return existing;
  }
  if (
    connector === "notion" &&
    snapshot.mount.connector === "notion" &&
    snapshot.mount.status !== "not_mounted" &&
    snapshot.mount.localPath.trim()
  ) {
    return snapshot.mount.localPath;
  }
  switch (connector) {
    case "notion":
      return "~/Library/CloudStorage/Locality/notion";
    case "google-docs":
      return "~/Library/CloudStorage/Locality/google-docs-main";
    case "google-calendar":
      return "~/Library/CloudStorage/Locality/google-calendar-main";
    case "gmail":
      return "~/Library/CloudStorage/Locality/gmail-main";
    case "granola":
      return "~/Library/CloudStorage/Locality/granola";
    case "linear":
      return "~/Library/CloudStorage/Locality/linear";
    case "slack":
      return "~/Library/CloudStorage/Locality/slack";
  }
}

function sourceActionLabel(
  connector: SourceConnectorId,
  state: { needsConnection: boolean; needsFolder: boolean; mounted: boolean },
) {
  if (connector !== "notion") {
    if (state.mounted) {
      return "Mounted";
    }
    return state.needsFolder ? "Create Folder" : "Connect & Mount";
  }
  if (state.needsConnection) {
    return "Connect Notion";
  }
  if (state.needsFolder) {
    return "Create Local Folder";
  }
  return "Change Notion Access";
}

function sourceActionIcon(connector: SourceConnectorId, needsConnection: boolean) {
  if (connector === "notion" && needsConnection) {
    return <ShieldCheck />;
  }
  return <FolderOpen />;
}

function oauthConnectCommand(connector: "google-docs" | "google-calendar" | "gmail" | "slack") {
  switch (connector) {
    case "google-docs":
      return "connect_google_docs";
    case "google-calendar":
      return "connect_google_calendar";
    case "gmail":
      return "connect_gmail";
    case "slack":
      return "connect_slack";
  }
}

function ConnectorIcon({ connector }: { connector: SourceConnectorId }) {
  return (
    <span className={`connector-icon ${connector}`} aria-hidden="true">
      <img src={CONNECTOR_ICON_URLS[connector]} alt="" draggable="false" />
    </span>
  );
}

function FilesView({
  snapshot,
  onHome,
  onRefresh,
  onReview,
}: {
  snapshot: DesktopSnapshot;
  onHome: () => void;
  onRefresh: () => Promise<void>;
  onReview: () => void;
}) {
  const [query, setQuery] = useState("");
  const [statusFilter, setStatusFilter] = useState<FileStatusFilter>("all");
  const { results, searching } = useNotionSearchResults(query, !mountMissing(snapshot));
  const searchActive = query.trim().length >= 2;
  const browseItems = searchActive ? results : snapshot.recentFiles;
  const visibleItems = browseItems.filter((item) => itemMatchesFileStatusFilter(item, statusFilter));

  return (
    <div className="view-stack">
      <ViewHeader title="Files">
        <Breadcrumbs
          items={[
            { label: PRODUCT_TERMS.home, onClick: onHome },
            { label: PRODUCT_TERMS.files },
          ]}
        />
      </ViewHeader>

      {!mountMissing(snapshot) && (
        <CurrentWorkspacePanel snapshot={snapshot} onRefresh={onRefresh} onReview={onReview} />
      )}

      <section className="panel discovery-panel">
        <div className="discovery-heading">
          <div>
            <p className="label">Current access</p>
            <h2>Browse current files</h2>
            <p>Results come from connected sources available in this Locality session.</p>
          </div>
          <div className="file-filter-bar" role="tablist" aria-label="File status filters">
            {(["all", "review", "conflict", "synced"] as const).map((filter) => (
              <button
                key={filter}
                className={statusFilter === filter ? "active" : ""}
                role="tab"
                aria-selected={statusFilter === filter}
                onClick={() => setStatusFilter(filter)}
              >
                {fileStatusFilterLabel(filter)}
              </button>
            ))}
          </div>
        </div>
        <div className="locate-row">
          <Search />
          <input
            value={query}
            placeholder="Search current Notion files"
            onChange={(event) => setQuery(event.target.value)}
          />
        </div>
        <div className="file-discovery-list" aria-busy={searching ? "true" : "false"}>
          {visibleItems.length ? (
            visibleItems.map((item) => <FileDiscoveryRow key={`${item.kind}-${item.localPath}`} item={item} />)
          ) : (
            <EmptyDiscoveryState
              text={
                searching
                  ? "Searching current files..."
                  : searchActive
                    ? "No current files matched."
                    : statusFilter === "all"
                      ? "No current files are available yet."
                      : `No ${fileStatusFilterLabel(statusFilter).toLowerCase()} files are available.`
              }
            />
          )}
        </div>
        {!searchActive && snapshot.recentFiles.length > 0 && (
          <p className="quiet-note inline-note">
            Showing recent files. Use search to look across current access.
          </p>
        )}
      </section>

      {snapshot.recentFiles.length > 0 && searchActive && (
        <RecentFilesPanel items={snapshot.recentFiles} />
      )}
    </div>
  );
}

function CurrentWorkspacePanel({
  snapshot,
  onRefresh,
  onReview,
}: {
  snapshot: DesktopSnapshot;
  onRefresh: () => Promise<void>;
  onReview: () => void;
}) {
  const hasPendingChanges = snapshot.pendingChanges.length > 0;
  const [actionError, setActionError] = useState("");
  const [accessMessage, setAccessMessage] = useState("");
  const [accessState, setAccessState] = useState<"idle" | "changing" | "success" | "error">("idle");
  const [pullMessage, setPullMessage] = useState("");
  const [pullState, setPullState] = useState<"idle" | "pulling" | "success" | "error">("idle");
  const accountLabel = snapshot.connection.accountLabel.trim();
  const showAccount = accountLabel.length > 0 && accountLabel !== snapshot.connection.workspaceName;
  const fileProgressLabel = mountFileIndexProgressLabel(snapshot.mount);

  async function openFolder() {
    setActionError("");
    const report = await callCommand<ActionReport>(
      "open_path",
      { path: snapshot.mount.localPath },
      { ok: true, message: "Opened demo folder." },
    );
    if (!report.ok) {
      setActionError(report.message);
    }
  }

  async function openVsCode() {
    setActionError("");
    const report = await callCommand<ActionReport>(
      "open_in_vs_code",
      { path: snapshot.mount.localPath },
      { ok: true, message: "Opened demo folder in VS Code." },
    );
    if (!report.ok) {
      setActionError(report.message);
    }
  }

  async function changeNotionAccess() {
    if (accessState === "changing") {
      return;
    }

    setAccessMessage("");
    setAccessState("changing");
    const report = await callCommand<ActionReport>(
      "change_notion_access",
      undefined,
      { ok: true, message: "Changed demo Notion access." },
    );
    if (!report.ok) {
      setAccessMessage(report.message);
      setAccessState("error");
      return;
    }
    setAccessMessage(report.message);
    setAccessState("success");
    await onRefresh().catch(() => undefined);
  }

  async function pullChanges() {
    if (pullState === "pulling") {
      return;
    }

    setActionError("");
    setPullMessage("");
    setPullState("pulling");

    try {
      const report = await callCommand<ActionReport>("pull_notion_file", {
        path: snapshot.mount.localPath,
      });
      setPullMessage(report.message);
      setPullState(report.ok ? "success" : "error");
      if (report.ok) {
        void onRefresh().catch(() => undefined);
      }
    } catch (error) {
      setPullMessage(errorMessage(error));
      setPullState("error");
    }
  }

  return (
    <section className="panel workspace-detail-panel">
      <div className="discovery-heading">
        <div>
          <p className="label">{PRODUCT_TERMS.localWorkspace}</p>
          <h2>{snapshot.mount.workspaceName}</h2>
          <p>{snapshot.mount.connectorName} · {snapshot.mount.accessScope}</p>
        </div>
        <StatusPill
          tone={healthTone(snapshot.health.state)}
          title={healthDescription(snapshot.health.state, snapshot.health.attentionCount)}
        >
          {healthLabel(snapshot.health.state)}
        </StatusPill>
      </div>

      <div className="workspace-path-row">
        <FolderOpen />
        <code title={snapshot.mount.localPath}>{compactPath(snapshot.mount.localPath, 76)}</code>
        <button className="icon-button has-tooltip" data-tooltip="Copy path" onClick={() => copyText(snapshot.mount.localPath)}>
          <Copy />
        </button>
        <button className="icon-button has-tooltip" data-tooltip="Reveal in Finder" onClick={() => void openFolder()}>
          <FolderOpen />
        </button>
        <button className="icon-button has-tooltip" data-tooltip="Open in VS Code" onClick={() => void openVsCode()}>
          <Code2 />
        </button>
      </div>

      <div className="workspace-action-row">
        <SecondaryButton
          compact
          disabled={connectionMissing(snapshot) || accessState === "changing"}
          icon={accessState === "changing" ? <Loader2 className="spin-icon" /> : <ShieldCheck />}
          onClick={() => void changeNotionAccess()}
        >
          {accessState === "changing" ? "Waiting for Notion" : "Change Access"}
        </SecondaryButton>
        <SecondaryButton
          compact
          disabled={!snapshot.mount.localPath.trim() || accessState === "changing" || pullState === "pulling"}
          icon={pullState === "pulling" ? <Loader2 className="spin-icon" /> : <RefreshCw />}
          onClick={() => void pullChanges()}
        >
          {pullState === "pulling" ? "Pulling" : "Pull Latest"}
        </SecondaryButton>
        {hasPendingChanges && (
          <PrimaryButton compact icon={<ListChecks />} onClick={onReview}>
            Review
          </PrimaryButton>
        )}
      </div>

      {actionError && <p className="field-error">{actionError}</p>}
      {accessMessage && (
        <p className={accessState === "error" ? "field-error" : "quiet-note inline-note"}>
          {accessMessage}
        </p>
      )}
      {pullMessage && (
        <p className={pullState === "error" ? "field-error" : "quiet-note inline-note"}>{pullMessage}</p>
      )}

      <div className="workspace-facts">
        <span>Permission: {snapshot.mount.readOnly ? "Read only" : "Edit enabled"}</span>
        <span>Projection: {snapshot.mount.projection}</span>
        <span>{fileProgressLabel ?? `Indexed: ${mountEntityCountLabel(snapshot.mount)}`}</span>
        {showAccount && <span>Account: {accountLabel}</span>}
      </div>

      <details className="workspace-diagnostics">
        <summary>Diagnostics</summary>
        <div className="workspace-facts">
          <span>Connection: {snapshot.connection.status}</span>
          <span>Status: {snapshot.mount.status}</span>
          <span>Connector: {snapshot.mount.connector}</span>
          <span>Review items: {snapshot.pendingChanges.length}</span>
        </div>
      </details>
    </section>
  );
}

function MountDetailView({
  snapshot,
  mount,
  onHome,
  onMounts,
  onRefresh,
  onReview,
}: {
  snapshot: DesktopSnapshot;
  mount: MountSummary;
  onHome: () => void;
  onMounts: () => void;
  onRefresh: () => Promise<void>;
  onReview: () => void;
}) {
  const hasPendingChanges = mount.pendingChangeCount > 0;
  const isActiveMount = snapshot.activeMountId === mount.mountId;
  const showNotionAccessAction = mount.connector === "notion" && isActiveMount;
  const showNotionPullAction = mount.connector === "notion";
  const [actionError, setActionError] = useState("");
  const [accessMessage, setAccessMessage] = useState("");
  const [accessState, setAccessState] = useState<"idle" | "changing" | "success" | "error">("idle");
  const [pullMessage, setPullMessage] = useState("");
  const [pullState, setPullState] = useState<"idle" | "pulling" | "success" | "error">("idle");
  const [backupMessage, setBackupMessage] = useState("");
  const [backupState, setBackupState] = useState<"idle" | "exporting" | "success" | "error">("idle");
  const [sourceAction, setSourceAction] = useState<SourceDestructiveAction | null>(null);
  const [sourceConfirmation, setSourceConfirmation] = useState("");
  const [sourceActionBusy, setSourceActionBusy] = useState(false);
  const [sourceActionMessage, setSourceActionMessage] = useState("");
  const accountLabel = isActiveMount ? snapshot.connection.accountLabel.trim() : "";
  const showAccount = accountLabel.length > 0 && accountLabel !== mount.workspaceName;
  const providerState = mount.provider?.state ?? "Not registered";
  const providerMessage = mount.provider?.message ?? providerState;
  const {
    liveModeEnabled,
    liveModeBusy,
    liveModeState,
    liveModeMessage,
    toggleLiveMode,
  } = useMountLiveModeController(snapshot, onRefresh);
  const liveModeAppliesToSource = isActiveMount && mount.connector === "notion";
  const sourceSyncMode = sourceSyncModeLabel(snapshot.liveMode, liveModeAppliesToSource);
  const fileProgressValue = mountFileIndexProgressValue(mount);

  async function openFolder() {
    setActionError("");
    setBackupMessage("");
    const report = await callCommand<ActionReport>(
      "open_path",
      { path: mount.localPath },
      { ok: true, message: "Opened demo folder." },
    );
    if (!report.ok) {
      setActionError(report.message);
    }
  }

  async function openVsCode() {
    setActionError("");
    setBackupMessage("");
    const report = await callCommand<ActionReport>(
      "open_in_vs_code",
      { path: mount.localPath },
      { ok: true, message: "Opened demo folder in VS Code." },
    );
    if (!report.ok) {
      setActionError(report.message);
    }
  }

  async function changeNotionAccess() {
    if (accessState === "changing" || !showNotionAccessAction) {
      return;
    }

    setAccessMessage("");
    setBackupMessage("");
    setAccessState("changing");
    const report = await callCommand<ActionReport>(
      "change_notion_access",
      undefined,
      { ok: true, message: "Changed demo Notion access." },
    );
    if (!report.ok) {
      setAccessMessage(report.message);
      setAccessState("error");
      return;
    }
    setAccessMessage(report.message);
    setAccessState("success");
    await onRefresh().catch(() => undefined);
  }

  async function pullChanges() {
    if (pullState === "pulling" || !showNotionPullAction) {
      return;
    }

    setActionError("");
    setPullMessage("");
    setBackupMessage("");
    setPullState("pulling");

    try {
      const report = await callCommand<ActionReport>("pull_notion_file", {
        path: mount.localPath,
      });
      setPullMessage(report.message);
      setPullState(report.ok ? "success" : "error");
      if (report.ok) {
        void onRefresh().catch(() => undefined);
      }
    } catch (error) {
      setPullMessage(errorMessage(error));
      setPullState("error");
    }
  }

  async function exportBackup() {
    if (backupState === "exporting") {
      return;
    }

    setActionError("");
    setBackupMessage("");
    setBackupState("exporting");
    try {
      const report = await callCommand<ActionReport>(
        "export_source_backup",
        { mountId: mount.mountId },
        { ok: true, message: `Exported demo backup for ${mount.mountId}.` },
      );
      setBackupMessage(report.message);
      setBackupState(report.ok ? "success" : "error");
      if (report.ok) {
        await onRefresh().catch(() => undefined);
      }
    } catch (error) {
      setBackupMessage(errorMessage(error));
      setBackupState("error");
    }
  }

  function requestSourceAction(action: SourceDestructiveAction) {
    setSourceActionMessage("");
    setSourceConfirmation("");
    setSourceAction(action);
  }

  function cancelSourceAction() {
    if (sourceActionBusy) {
      return;
    }
    setSourceAction(null);
    setSourceConfirmation("");
  }

  async function confirmSourceAction() {
    if (!sourceAction || sourceActionBusy) {
      return;
    }
    setSourceActionMessage("");
    setSourceActionBusy(true);
    try {
      const command = sourceAction === "reset" ? "reset_source_state" : "disconnect_source";
      const report = await callCommand<ActionReport>(
        command,
        { mountId: mount.mountId, confirmation: sourceConfirmation },
        {
          ok: true,
          message: sourceAction === "reset"
            ? `Reset demo source ${mount.mountId}.`
            : `Disconnected demo source ${mount.mountId}.`,
        },
      );
      setSourceActionMessage(report.message);
      if (report.ok) {
        setSourceAction(null);
        setSourceConfirmation("");
        await onRefresh().catch(() => undefined);
      }
    } catch (error) {
      setSourceActionMessage(errorMessage(error));
    } finally {
      setSourceActionBusy(false);
    }
  }

  return (
    <div className="view-stack">
      <Breadcrumbs
        items={[
          { label: PRODUCT_TERMS.home, onClick: onHome },
          { label: PRODUCT_TERMS.sources, onClick: onMounts },
          { label: mount.mountId },
        ]}
      />
      <ViewHeader title={mount.workspaceName}>
        <StatusPill tone={mountStatusTone(mount)} title={mountStatusLabel(mount)}>
          {mountStatusLabel(mount)}
        </StatusPill>
      </ViewHeader>

      <section className="mount-hero">
        <div className="mount-hero-icon">
          <FolderOpen />
        </div>
        <div>
          <p className="label">{mount.connectorName} source</p>
          <h2 title={mount.localPath}>{compactPath(mount.localPath, 78)}</h2>
          <p>
            Locality exposes this connected source as local files at the registered folder.
          </p>
        </div>
        <div className="mount-actions">
          <PrimaryButton icon={<FolderOpen />} onClick={() => void openFolder()}>
            Open Folder
          </PrimaryButton>
          <SecondaryButton compact icon={<Copy />} onClick={() => copyText(mount.localPath)}>
            Copy Path
          </SecondaryButton>
          <SecondaryButton compact icon={<Code2 />} onClick={() => void openVsCode()}>
            Open in VS Code
          </SecondaryButton>
          <SecondaryButton
            compact
            busy={backupState === "exporting"}
            disabled={!mount.localPath.trim()}
            icon={<Download />}
            onClick={() => void exportBackup()}
          >
            {backupState === "exporting" ? "Exporting" : "Export Backup"}
          </SecondaryButton>
          {showNotionAccessAction && (
            <SecondaryButton
              compact
              disabled={connectionMissing(snapshot) || accessState === "changing"}
              icon={accessState === "changing" ? <Loader2 className="spin-icon" /> : <ShieldCheck />}
              onClick={() => void changeNotionAccess()}
            >
              {accessState === "changing" ? "Waiting for Notion" : "Change Notion Access"}
            </SecondaryButton>
          )}
          {showNotionPullAction && (
            <SecondaryButton
              compact
              disabled={!mount.localPath.trim() || accessState === "changing" || pullState === "pulling"}
              icon={pullState === "pulling" ? <Loader2 className="spin-icon" /> : <RefreshCw />}
              onClick={() => void pullChanges()}
            >
              {pullState === "pulling" ? "Syncing source" : "Sync source"}
            </SecondaryButton>
          )}
        </div>
      </section>
      {actionError && <p className="field-error">{actionError}</p>}
      {accessMessage && (
        <p className={accessState === "error" ? "field-error" : "quiet-note inline-note"}>
          {accessMessage}
        </p>
      )}
      {pullMessage && (
        <p className={pullState === "error" ? "field-error" : "quiet-note inline-note"}>{pullMessage}</p>
      )}
      {backupMessage && (
        <p className={backupState === "error" ? "field-error" : "quiet-note inline-note"}>
          {backupMessage}
        </p>
      )}

      <section className="detail-grid">
        <div className="panel">
          <PanelTitle title={`${mount.connectorName} Access`} />
          <SettingRow title="Workspace" value={mount.workspaceName} />
          {showAccount && <SettingRow title="Account" value={accountLabel} />}
          {mount.connectionId && <SettingRow title="Connection" value={mount.connectionId} />}
          <SettingRow title="Selected access" value={mount.accessScope} />
          {mount.notionUrl && (
            <SettingRow title="Mounted root" value="Open in Notion" href={mount.notionUrl} />
          )}
          <SettingRow title="Permission" value={mountAccessLabel(mount)} />
        </div>

        <div className="panel">
          <PanelTitle title="Local Files" />
          <SettingRow title="Location" value={mount.localPath} />
          <SettingRow title="Projection" value={mount.projection} />
          <SettingRow title="Mounted content" value={`${mount.entityCount} items`} />
          {fileProgressValue && <SettingRow title="Files indexed" value={fileProgressValue} />}
          <SettingRow title="Root exists" value={mount.rootExists ? "Yes" : "No"} />
        </div>
      </section>

      <section className="panel source-sync-panel">
        <div className="source-sync-heading">
          <div>
            <p className="label">Sync for this source</p>
            <h2>{sourceSyncMode}</h2>
            <p>
              Clean remote pulls and safe local pushes can run in the background. Conflicts,
              destructive changes, and large plans still go to Review Center.
            </p>
          </div>
          <StatusPill
            tone={liveModeAppliesToSource && liveModeState === "error" ? "danger" : liveModeEnabled ? "ready" : "warn"}
            title={liveModeAppliesToSource ? liveModeTooltip(liveModeEnabled) : "Only the active source can change Live Mode from this view."}
          >
            {liveModeAppliesToSource ? (liveModeEnabled ? "Live" : "Review") : "Inactive"}
          </StatusPill>
        </div>
        <div className="source-sync-grid">
          <SettingRow title="Clean remote pulls" value={liveModeAppliesToSource && liveModeEnabled ? "Automatic when safe" : "Manual sync"} />
          <SettingRow title="Safe local pushes" value={liveModeAppliesToSource && liveModeEnabled ? "Live Mode" : "Review first"} />
          <SettingRow title="Review rules" value="Conflict, destructive, large plan" />
          <SettingRow title="Last run" value={snapshot.liveMode.lastRunAt ?? "Not available"} />
        </div>
        <div className="source-sync-actions">
          <button
            className={`live-mode-control has-tooltip ${liveModeEnabled && liveModeAppliesToSource ? "active" : ""}`}
            aria-pressed={liveModeEnabled && liveModeAppliesToSource}
            aria-label={`${liveModeEnabled ? "Turn off" : "Turn on"} Live Mode for this source`}
            data-tooltip={liveModeAppliesToSource ? liveModeTooltip(liveModeEnabled) : "Open the active source to change Live Mode"}
            title={liveModeAppliesToSource ? liveModeTooltip(liveModeEnabled) : "Open the active source to change Live Mode"}
            disabled={!liveModeAppliesToSource || liveModeBusy}
            onClick={() => void toggleLiveMode()}
          >
            <span className="live-mode-copy">
              {liveModeBusy ? <span className="live-mode-spinner" aria-hidden="true" /> : <Zap />}
              <span>{liveModeEnabled && liveModeAppliesToSource ? "Live Mode On" : "Live Mode Off"}</span>
            </span>
            <span className={`toggle ${liveModeEnabled && liveModeAppliesToSource ? "enabled" : ""}`} aria-hidden="true">
              <i />
            </span>
          </button>
          {showNotionPullAction && (
            <SecondaryButton
              compact
              disabled={!mount.localPath.trim() || accessState === "changing" || pullState === "pulling"}
              icon={pullState === "pulling" ? <Loader2 className="spin-icon" /> : <RefreshCw />}
              onClick={() => void pullChanges()}
            >
              {pullState === "pulling" ? "Syncing" : "Sync Now"}
            </SecondaryButton>
          )}
          <SecondaryButton compact disabled>
            Use Global Default
          </SecondaryButton>
        </div>
        {liveModeMessage && liveModeAppliesToSource && (
          <p className={liveModeState === "error" ? "field-error" : "quiet-note inline-note"}>
            {liveModeMessage}
          </p>
        )}
      </section>

      <section className="safety-strip">
        <ShieldCheck />
        <div>
          <h2>Review catches work that needs approval</h2>
          <p>
            Safe Live Mode work can sync automatically. This source currently has {mount.pendingChangeCount}
            {mount.pendingChangeCount === 1 ? " item" : " items"} waiting for review.
          </p>
        </div>
        {isActiveMount && hasPendingChanges && (
          <PrimaryButton compact icon={<ListChecks />} onClick={onReview}>
            Review
          </PrimaryButton>
        )}
      </section>

      <details className="advanced-panel">
        <summary>Advanced diagnostics</summary>
        <div className="settings-grid compact-settings">
          <div className="panel">
            <SettingRow title="Mount id" value={mount.mountId} />
            <SettingRow title="Connector" value={mount.connector} />
            <SettingRow title="Remote root" value={mount.remoteRootId ?? "Workspace"} />
          </div>
          <div className="panel">
            <SettingRow title="Mount status" value={mount.status} />
            <SettingRow title="Provider" value={providerMessage} />
            <SettingRow title="Primary mount" value={isActiveMount ? "Yes" : "No"} />
          </div>
        </div>
      </details>

      <details className="danger-accordion">
        <summary>
          <span>
            <AlertTriangle />
            Danger Zone
          </span>
        </summary>
        <div className="danger-body">
          <div>
            <h3>Source-scoped destructive actions</h3>
            <p>
              Reset rebuilds this source from remote data. Disconnect revokes its saved connection
              while keeping the registered local folder available for reconnection.
            </p>
            {sourceActionMessage && <p className="quiet-note inline-note">{sourceActionMessage}</p>}
          </div>
          <div className="danger-actions">
            <SecondaryButton
              compact
              icon={sourceActionBusy && sourceAction === "reset" ? <Loader2 className="spin-icon" /> : <RotateCcw />}
              disabled={sourceActionBusy}
              onClick={() => requestSourceAction("reset")}
            >
              {sourceActionBusy && sourceAction === "reset" ? "Resetting" : "Reset Source State"}
            </SecondaryButton>
            <SecondaryButton
              compact
              icon={sourceActionBusy && sourceAction === "disconnect" ? <Loader2 className="spin-icon" /> : <Trash2 />}
              disabled={sourceActionBusy}
              onClick={() => requestSourceAction("disconnect")}
            >
              {sourceActionBusy && sourceAction === "disconnect" ? "Disconnecting" : "Disconnect Source"}
            </SecondaryButton>
          </div>
        </div>
      </details>
      {sourceAction && (
        <DestructiveSourceDialog
          action={sourceAction}
          mount={mount}
          value={sourceConfirmation}
          busy={sourceActionBusy}
          message={sourceActionMessage}
          onChange={setSourceConfirmation}
          onCancel={cancelSourceAction}
          onConfirm={() => void confirmSourceAction()}
        />
      )}
    </div>
  );
}

function DestructiveSourceDialog({
  action,
  mount,
  value,
  busy,
  message,
  onChange,
  onCancel,
  onConfirm,
}: {
  action: SourceDestructiveAction;
  mount: MountSummary;
  value: string;
  busy: boolean;
  message: string;
  onChange: (value: string) => void;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const isReset = action === "reset";
  const requiredValue = sourceDestructiveConfirmation(action, mount.mountId);
  const confirmed = sourceDestructiveConfirmationMatches(action, mount.mountId, value);
  const title = isReset ? `Reset ${mount.connectorName} source state` : `Disconnect ${mount.connectorName} source`;
  const actionLabel = isReset ? "Reset source state" : "Disconnect source";
  const description = isReset
    ? `This clears Locality's cached state for ${mount.mountId} and rebuilds it from ${mount.connectorName}.`
    : `This revokes the saved connection used by ${mount.mountId}.`;
  const consequences = isReset
    ? [
        "The remote source is not changed.",
        "Pending local changes are preserved in Locality's recovery folder before reset.",
        "The source folder is repopulated from remote data.",
      ]
    : [
        "The remote source is not changed.",
        "Cached local files and the mount registration are kept.",
        "Any other mount using the same saved connection will also need reconnection.",
      ];

  return (
    <TypedDestructiveDialog
      titleId="destructive-source-title"
      title={title}
      description={description}
      consequences={consequences}
      requiredValue={requiredValue}
      actionLabel={actionLabel}
      value={value}
      confirmed={confirmed}
      busy={busy}
      message={message}
      onChange={onChange}
      onCancel={onCancel}
      onConfirm={onConfirm}
    />
  );
}

function TypedDestructiveDialog({
  titleId,
  title,
  description,
  consequences,
  requiredValue,
  actionLabel,
  value,
  confirmed,
  busy,
  message = "",
  onChange,
  onCancel,
  onConfirm,
}: {
  titleId: string;
  title: string;
  description: string;
  consequences: string[];
  requiredValue: string;
  actionLabel: string;
  value: string;
  confirmed: boolean;
  busy: boolean;
  message?: string;
  onChange: (value: string) => void;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <div className="modal-backdrop" role="presentation">
      <section className="destructive-modal" role="dialog" aria-modal="true" aria-labelledby={titleId}>
        <div className="destructive-modal-header">
          <div>
            <h2 id={titleId}>{title}</h2>
            <p>{description}</p>
          </div>
          <button className="icon-button has-tooltip" data-tooltip="Cancel" disabled={busy} onClick={onCancel}>
            <X />
          </button>
        </div>
        <ul className="destructive-list">
          {consequences.map((item) => (
            <li key={item}>{item}</li>
          ))}
        </ul>
        <label className="destructive-input-label">
          <span>
            Type <strong>{requiredValue}</strong> to confirm
          </span>
          <input
            autoFocus
            value={value}
            disabled={busy}
            onChange={(event) => onChange(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Escape") {
                onCancel();
              }
              if (event.key === "Enter" && confirmed && !busy) {
                onConfirm();
              }
            }}
          />
        </label>
        {message && <p className="field-error">{message}</p>}
        <div className="modal-actions">
          <SecondaryButton disabled={busy} onClick={onCancel}>
            Cancel
          </SecondaryButton>
          <button className="destructive-action-button" disabled={!confirmed || busy} onClick={onConfirm}>
            {busy ? "Working..." : actionLabel}
          </button>
        </div>
      </section>
    </div>
  );
}

function RecentFilesPanel({
  items,
  onOpenFiles,
  compact = false,
}: {
  items: LocatedItem[];
  onOpenFiles?: () => void;
  compact?: boolean;
}) {
  const visibleItems = compact ? items.slice(0, 3) : items;

  return (
    <section className="panel discovery-panel">
      <div className="discovery-heading">
        <div>
          <p className="label">Recent files</p>
          <h2>{items.length ? "Recently opened or changed" : "No recent files yet"}</h2>
          <p>{items.length ? "Files from the active workspace that were opened, changed, or need review." : "Open or edit Locality files and they will appear here."}</p>
        </div>
        {onOpenFiles && (
          <SecondaryButton compact icon={<ChevronRight />} onClick={onOpenFiles}>
            View Files
          </SecondaryButton>
        )}
      </div>
      {visibleItems.length ? (
        <div className="file-discovery-list">
          {visibleItems.map((item) => (
            <FileDiscoveryRow key={`${item.kind}-${item.localPath}`} item={item} />
          ))}
        </div>
      ) : (
        <EmptyDiscoveryState text="No active files have been opened or changed yet." />
      )}
    </section>
  );
}

function FileDiscoveryRow({ item }: { item: LocatedItem }) {
  const [error, setError] = useState("");
  const stateIcon =
    item.state === "conflict" ? (
      <AlertTriangle />
    ) : item.state === "pending_changes" || item.state === "remote_update_available" ? (
      <Clock3 />
    ) : (
      <Check />
    );

  async function reveal() {
    setError("");
    try {
      const report = await callCommand<ActionReport>("reveal_path", { path: item.localPath }, { ok: true, message: "" });
      if (!report.ok) {
        setError(report.message);
      }
    } catch (caught) {
      setError(errorMessage(caught));
    }
  }

  return (
    <div className={`file-discovery-row ${item.state}`}>
      <div className="file-state">{stateIcon}</div>
      <div>
        <strong>{item.title}</strong>
        <code title={item.localPath}>{compactPath(item.localPath, 68)}</code>
        <span>{item.kind} · {locatedStateLabel(item.state)}</span>
        {error && <p className="field-error">{error}</p>}
      </div>
      <div className="file-discovery-actions">
        <button className="icon-button has-tooltip" data-tooltip="Copy path" onClick={() => copyText(item.localPath)}>
          <Copy />
        </button>
        <SecondaryButton compact icon={<FolderOpen />} onClick={() => void reveal()}>
          Reveal
        </SecondaryButton>
      </div>
    </div>
  );
}

function EmptyDiscoveryState({ text }: { text: string }) {
  return <p className="discovery-empty">{text}</p>;
}

function PendingView({
  snapshot,
  onHome,
  onReview,
  onRefresh,
  initialFilter,
}: {
  snapshot: DesktopSnapshot;
  onHome: () => void;
  onReview: () => void;
  onRefresh: () => Promise<void>;
  initialFilter: ReviewFilter;
}) {
  const hasPendingChanges = snapshot.pendingChanges.length > 0;
  const [filter, setFilter] = useState<ReviewFilter>(initialFilter);
  const [pushState, setPushState] = useState<"idle" | "pushing" | "success" | "error">("idle");
  const [pushMessage, setPushMessage] = useState("");
  const reviewCounts = reviewQueueCounts(snapshot.pendingChanges);
  const visibleChanges = snapshot.pendingChanges.filter((change) => changeMatchesReviewFilter(change, filter));

  useEffect(() => {
    setFilter(initialFilter);
  }, [initialFilter]);

  async function pushAll() {
    if (!hasPendingChanges || pushState === "pushing") {
      return;
    }

    setPushState("pushing");
    setPushMessage("");
    try {
      const report = await callCommand<ActionReport>(
        "push_to_notion",
        { confirmDangerous: false },
        {
          ok: true,
          message: "Pushed changes to Notion.",
        },
      );
      if (!report.ok) {
        setPushState("error");
        setPushMessage(report.message);
        return;
      }
      setPushState("success");
      setPushMessage(report.message || "Pushed changes to Notion.");
      await onRefresh().catch(() => undefined);
    } catch (error) {
      setPushState("error");
      setPushMessage(errorMessage(error));
    }
  }

  const isPushing = pushState === "pushing";

  return (
    <div className="view-stack">
      <Breadcrumbs items={[{ label: PRODUCT_TERMS.home, onClick: onHome }, { label: PRODUCT_TERMS.reviewCenter }]} />
      <ViewHeader title={PRODUCT_TERMS.reviewCenter}>
        <div className="button-row">
          <PrimaryButton
            disabled={!hasPendingChanges || isPushing}
            icon={isPushing ? <Loader2 className="spin-icon" /> : <ShieldCheck />}
            onClick={() => void pushAll()}
          >
            {isPushing ? "Pushing..." : "Push Safe Changes"}
          </PrimaryButton>
          <SecondaryButton disabled={!hasPendingChanges || isPushing} icon={<ListChecks />} onClick={onReview}>
            Review Push
          </SecondaryButton>
        </div>
      </ViewHeader>
      {pushMessage && (
        <p className={pushState === "error" ? "field-error" : "success-note inline-note"}>
          {pushMessage}
        </p>
      )}
      {hasPendingChanges ? (
        <>
          <section className="panel review-overview-panel">
            <div className="review-mode-copy">
              <p className="label">{snapshot.liveMode.enabled ? "Live Mode on" : "Review mode"}</p>
              <h2>{reviewCounts.total} items need attention</h2>
              <p>
                Safe edits can sync automatically. Review Center keeps approvals, conflicts, failed syncs,
                and policy-paused files in one place.
              </p>
            </div>
            <div className="review-counts">
              <Metric label="Approvals" value={reviewCounts.approvals} />
              <Metric label="Problems" value={reviewCounts.problems} />
              <Metric label="Live tracked" value={snapshot.liveMode.coveredCount} />
            </div>
          </section>
          <div className="review-filter-bar" role="tablist" aria-label="Review Center filters">
            {(["all", "approvals", "problems"] as const).map((nextFilter) => (
              <button
                key={nextFilter}
                className={`review-filter-button ${filter === nextFilter ? "active" : ""}`}
                role="tab"
                aria-selected={filter === nextFilter}
                onClick={() => setFilter(nextFilter)}
              >
                {reviewFilterLabel(nextFilter)}
                <span>
                  {nextFilter === "all"
                    ? reviewCounts.total
                    : nextFilter === "approvals"
                      ? reviewCounts.approvals
                      : reviewCounts.problems}
                </span>
              </button>
            ))}
          </div>
          {visibleChanges.length > 0 ? (
            <FileChangeList
              changes={visibleChanges}
              mountPath={snapshot.mount.localPath}
              onReview={onReview}
              onRefresh={onRefresh}
            />
          ) : (
            <section className="panel muted-panel review-empty-filter">
              <Check />
              <div>
                <h2>No {reviewFilterLabel(filter).toLowerCase()} in this queue</h2>
                <p>Switch filters to see other review items.</p>
              </div>
            </section>
          )}
        </>
      ) : (
        <section className="panel muted-panel">
          <Check />
          <div>
            <h2>No review needed</h2>
            <p>Safe Live Mode work can sync automatically. Items that need approval will appear here.</p>
          </div>
        </section>
      )}
    </div>
  );
}

function ReviewView({
  snapshot,
  onHome,
  onPending,
  onRefresh,
  onDone,
}: {
  snapshot: DesktopSnapshot;
  onHome: () => void;
  onPending: () => void;
  onRefresh: () => Promise<void>;
  onDone: () => void;
}) {
  const [plan, setPlan] = useState<PushPlan>(samplePushPlan);
  const [complete, setComplete] = useState(false);
  const [pushState, setPushState] = useState<"idle" | "pushing" | "success" | "error">("idle");
  const [pushMessage, setPushMessage] = useState("");
  const [destructiveAccepted, setDestructiveAccepted] = useState(false);

  useEffect(() => {
    void callCommand<PushPlan>("review_push_plan", undefined, samplePushPlan)
      .then(setPlan)
      .catch(() => setPlan(samplePushPlan));
  }, []);

  useEffect(() => {
    if (pushState !== "success") {
      return undefined;
    }

    const timer = window.setTimeout(() => setComplete(true), 1200);
    return () => window.clearTimeout(timer);
  }, [pushState]);

  async function push() {
    if (pushState === "pushing" || pushState === "success") {
      return;
    }

    setPushState("pushing");
    setPushMessage("");
    try {
      const report = await callCommand<ActionReport>(
        "push_to_notion",
        { confirmDangerous: true },
        {
          ok: true,
          message: "Pushed changes to Notion.",
        },
      );
      if (!report.ok) {
        setPushState("error");
        setPushMessage(report.message);
        return;
      }
      await onRefresh().catch(() => undefined);
      setPushMessage(report.message || "Pushed changes to Notion.");
      setPushState("success");
    } catch (error) {
      setPushState("error");
      setPushMessage(errorMessage(error));
    }
  }

  if (complete) {
    const updatedCount = plan.pagesUpdated + plan.databaseRowsUpdated;
    const fileLabel = updatedCount === 1 ? "file" : "files";
    return (
      <div className="center-result">
        <BrandTile variant="ready" />
        <h1>Pushed to Notion</h1>
        <p>{updatedCount} {fileLabel} updated successfully.</p>
        <PrimaryButton onClick={onDone}>Done</PrimaryButton>
      </div>
    );
  }

  const isPushing = pushState === "pushing";
  const pushSucceeded = pushState === "success";
  const needsDeletionApproval = plan.pagesDeleted > 0;
  const pushBlockedByDeletion = needsDeletionApproval && !destructiveAccepted;

  return (
    <div className="view-stack">
      <Breadcrumbs
        items={[
          { label: PRODUCT_TERMS.home, onClick: onHome },
          { label: PRODUCT_TERMS.reviewCenter, onClick: onPending },
          { label: PRODUCT_TERMS.pushApproval },
        ]}
      />
      <ViewHeader title={PRODUCT_TERMS.pushApproval}>
        <StatusPill
          tone={pushState === "error" ? "danger" : isPushing ? "warn" : "ready"}
          title={isPushing ? "Locality is writing the approved local changes to Notion." : "This push is ready for review."}
        >
          {pushState === "error" ? "Needs attention" : isPushing ? "Pushing" : pushSucceeded ? "Pushed" : "Ready"}
        </StatusPill>
      </ViewHeader>
      <h2 className="section-title">{plan.title}</h2>
      <p className="view-copy">{plan.summary}</p>
      {isPushing && (
        <p className="quiet-note inline-note">
          Writing changes to Notion. You can keep reviewing this window while Locality finishes.
        </p>
      )}
      {pushSucceeded && (
        <p className="success-note inline-note">
          {pushMessage || "Pushed changes to Notion."}
        </p>
      )}
      {pushState === "error" && pushMessage && <p className="field-error">{pushMessage}</p>}

      <section className="summary-grid">
        <Metric label="Pages updated" value={plan.pagesUpdated} />
        <Metric label="Database rows updated" value={plan.databaseRowsUpdated} />
        <Metric label="Pages deleted" value={plan.pagesDeleted} />
      </section>
      {needsDeletionApproval && (
        <label className="destructive-check">
          <input
            type="checkbox"
            checked={destructiveAccepted}
            disabled={isPushing || pushSucceeded}
            onChange={(event) => setDestructiveAccepted(event.target.checked)}
          />
          <span>I understand {plan.pagesDeleted} Notion {plan.pagesDeleted === 1 ? "page" : "pages"} will be deleted.</span>
        </label>
      )}

      <FileChangeList
        changes={plan.files}
        mountPath={snapshot.mount.localPath}
        confirmDangerous
        onRefresh={onRefresh}
      />

      <div className="footer-actions">
        <PrimaryButton
          disabled={!plan.canPush || isPushing || pushSucceeded || pushBlockedByDeletion}
          icon={isPushing ? <Loader2 className="spin-icon" /> : pushSucceeded ? <Check /> : <ShieldCheck />}
          onClick={push}
        >
          {isPushing ? "Pushing..." : pushSucceeded ? "Pushed" : "Approve and Push"}
        </PrimaryButton>
        <SecondaryButton disabled={isPushing || pushSucceeded} onClick={onPending}>Cancel</SecondaryButton>
      </div>
    </div>
  );
}

function ActivityView({ snapshot, onHome }: { snapshot: DesktopSnapshot; onHome: () => void }) {
  const [tab, setTab] = useState<"recent" | "debug">("recent");
  const grouped = useMemo(() => {
    return snapshot.activity.reduce<Record<string, ActivityItem[]>>((acc, item) => {
      const label = activityGroupLabel(item);
      acc[label] = [...(acc[label] ?? []), item];
      return acc;
    }, {});
  }, [snapshot.activity]);

  return (
    <div className="view-stack">
      <Breadcrumbs items={[{ label: PRODUCT_TERMS.home, onClick: onHome }, { label: PRODUCT_TERMS.activity }]} />
      <ViewHeader title={PRODUCT_TERMS.activity}>
        <div className="activity-tabs" role="tablist" aria-label="Activity sections">
          <button className={tab === "recent" ? "active" : ""} onClick={() => setTab("recent")} role="tab">
            Recent
          </button>
          <button className={tab === "debug" ? "active" : ""} onClick={() => setTab("debug")} role="tab">
            Queue Debug
          </button>
        </div>
      </ViewHeader>
      {tab === "recent" ? (
        Object.entries(grouped).map(([when, items]) => (
          <section className="activity-group" key={when}>
            <p className="label">{when}</p>
            {items.map((item) => (
              <article className="activity-item" key={`${when}-${item.kind}-${item.title}-${item.occurredAt ?? item.when}`}>
                <span className="activity-time" title={activityFullTimeLabel(item)}>
                  <Clock3 />
                  <span>{activityTimeLabel(item)}</span>
                </span>
                <div>
                  <h3>{item.title}</h3>
                  <p>{item.detail}</p>
                </div>
              </article>
            ))}
          </section>
        ))
      ) : (
        <DebugQueueView />
      )}
    </div>
  );
}

function DebugQueueView() {
  const [status, setStatus] = useState<DebugQueueStatus | null>(() =>
    isTauriRuntime() ? null : sampleDebugQueueStatus,
  );
  const [loading, setLoading] = useState(() => isTauriRuntime());
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;

    async function refresh() {
      try {
        const next = await callCommand<DebugQueueStatus>(
          "debug_notion_queue_status",
          undefined,
          sampleDebugQueueStatus,
        );
        if (!cancelled) {
          setStatus(next);
          setError(null);
          setLoading(false);
        }
      } catch (error) {
        if (!cancelled) {
          setError(errorMessage(error));
          setLoading(false);
        }
      }
    }

    void refresh();
    const timer = window.setInterval(() => void refresh(), 1000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, []);

  return (
    <section className="debug-queue-panel">
      <div className="debug-queue-heading">
        <div>
          <p className="label">Debug only</p>
          <h2>Notion request queue</h2>
          <p>Runtime queue snapshot. This tab polls only while it is open.</p>
        </div>
        <div className="debug-queue-meta">
          {loading ? <Loader2 className="spin-icon" /> : <RefreshCw />}
          <span>{status ? `Updated ${debugTimestampLabel(status.generatedAtUnixMs)}` : "Waiting"}</span>
        </div>
      </div>

      {error && <p className="debug-queue-error">{error}</p>}
      {status && (
        <>
          <div className="debug-queue-summary">
            <Metric label="Active" value={status.active.length} />
            <Metric label="Scheduler" value={status.schedulerMode} />
            <Metric label="Active poll" value={formatDuration(status.activeIntervalMs)} />
            <Metric label="Cold poll" value={formatDuration(status.coldIntervalMs)} />
          </div>
          <DebugLiveModeSection liveMode={status.liveMode} />
          <DebugActiveJobs active={status.active} />
          <div className="debug-queue-sections">
            {status.sections.map((section) => (
              <DebugQueueSectionView section={section} key={section.name} />
            ))}
          </div>
        </>
      )}
    </section>
  );
}

function DebugLiveModeSection({ liveMode }: { liveMode: DebugLiveModeStatus }) {
  const meta = [
    liveMode.mountId ? `mount ${liveMode.mountId}` : null,
    liveMode.enabled ? "enabled" : "off",
    liveMode.lastRunAt ? `last run ${debugTimestampValueLabel(liveMode.lastRunAt)}` : null,
  ].filter(Boolean) as string[];

  return (
    <section className="debug-queue-section debug-live-mode-section">
      <div className="debug-queue-section-header">
        <div>
          <h3>Live Mode tracked files</h3>
          <p>{[liveMode.label, ...meta].join(" · ")}</p>
          {liveMode.reason && <p className="debug-live-mode-reason">{liveMode.reason}</p>}
        </div>
      </div>
      {liveMode.trackedFiles.length ? (
        liveMode.trackedFiles.map((file) => (
          <DebugQueueRow
            key={`${file.remoteId}-${file.path}`}
            title={file.title || file.path}
            detail={file.path}
            meta={[
              file.status,
              file.activeForPolling ? "active polling" : null,
              file.remoteCheckDue ? "check due" : null,
              file.pollingReason ? `poll ${file.pollingReason}` : null,
              `sync ${file.syncState}`,
              `hydration ${file.hydration}`,
              file.freshnessTier ? `tier ${file.freshnessTier}` : null,
              file.remoteHintPending ? "remote hint" : null,
              file.autoSaveState ? `auto ${file.autoSaveState}` : null,
              file.lastCheckedAt ? `checked ${debugTimestampValueLabel(file.lastCheckedAt)}` : null,
              file.lastOpenedAt ? `opened ${debugTimestampValueLabel(file.lastOpenedAt)}` : null,
              file.lastLocalChangeAt ? `local ${debugTimestampValueLabel(file.lastLocalChangeAt)}` : null,
              ...file.issueCodes,
            ].filter(Boolean) as string[]}
          />
        ))
      ) : (
        <p className="debug-queue-empty">No files are currently tracked by Live Mode.</p>
      )}
    </section>
  );
}

function DebugActiveJobs({ active }: { active: DebugQueueActive[] }) {
  return (
    <section className="debug-queue-section">
      <div className="debug-queue-section-header">
        <div>
          <h3>Currently executing</h3>
          <p>{active.length ? `${active.length} active request${active.length === 1 ? "" : "s"}` : "No active request"}</p>
        </div>
      </div>
      {active.length ? (
        active.map((item) => (
          <DebugQueueRow
            key={`${item.kind}-${item.target ?? ""}-${item.startedAtUnixMs}`}
            title={item.kind}
            detail={item.target || "No target"}
            meta={[`elapsed ${formatDuration(item.elapsedMs)}`, `started ${debugTimestampLabel(item.startedAtUnixMs)}`]}
          />
        ))
      ) : (
        <p className="debug-queue-empty">The daemon is idle.</p>
      )}
    </section>
  );
}

function DebugQueueSectionView({ section }: { section: DebugQueueSection }) {
  const meta = [
    `${section.total} total`,
    section.ready === null || section.ready === undefined ? null : `${section.ready} ready`,
    section.deferred === null || section.deferred === undefined ? null : `${section.deferred} deferred`,
  ].filter(Boolean) as string[];

  return (
    <section className="debug-queue-section">
      <div className="debug-queue-section-header">
        <div>
          <h3>{section.label}</h3>
          <p>{meta.join(" · ")}</p>
        </div>
      </div>
      {section.items.length ? (
        section.items.map((item, index) => (
          <DebugQueueRow
            key={`${section.name}-${item.kind}-${item.target ?? item.remoteId ?? index}`}
            title={item.kind}
            detail={item.target || item.path || item.remoteId || "No target"}
            meta={[item.priority, item.reason, item.nextEligibleAt ? `eligible ${item.nextEligibleAt}` : null].filter(Boolean) as string[]}
          />
        ))
      ) : (
        <p className="debug-queue-empty">No queued requests.</p>
      )}
    </section>
  );
}

function DebugQueueRow({ title, detail, meta }: { title: string; detail: string; meta: string[] }) {
  return (
    <article className="debug-queue-row">
      <div>
        <strong>{title}</strong>
        <p>{detail}</p>
      </div>
      {meta.length > 0 && (
        <div className="debug-queue-tags">
          {meta.map((item) => (
            <span key={item}>{item}</span>
          ))}
        </div>
      )}
    </article>
  );
}

function debugTimestampLabel(unixMs: number) {
  if (!Number.isFinite(unixMs) || unixMs <= 0) {
    return "unknown";
  }
  return new Intl.DateTimeFormat(undefined, {
    hour: "numeric",
    minute: "2-digit",
    second: "2-digit",
  }).format(new Date(unixMs));
}

function debugTimestampValueLabel(value: string) {
  const unixMs = Number(value.startsWith("unix_ms:") ? value.slice("unix_ms:".length) : value);
  if (Number.isFinite(unixMs) && unixMs > 0) {
    return debugTimestampLabel(unixMs);
  }
  return value;
}

function formatDuration(ms: number) {
  if (!Number.isFinite(ms) || ms < 0) {
    return "unknown";
  }
  if (ms < 1000) {
    return `${Math.round(ms)}ms`;
  }
  if (ms < 60_000) {
    const seconds = ms / 1000;
    return `${seconds < 10 ? seconds.toFixed(1) : Math.round(seconds)}s`;
  }
  const minutes = Math.floor(ms / 60_000);
  const seconds = Math.round((ms % 60_000) / 1000);
  return seconds > 0 ? `${minutes}m ${seconds}s` : `${minutes}m`;
}

function activityGroupLabel(item: ActivityItem) {
  const date = parseActivityDate(item.occurredAt);
  if (!date) {
    return item.when;
  }
  if (sameCalendarDay(date, new Date())) {
    return "Today";
  }
  const yesterday = new Date();
  yesterday.setDate(yesterday.getDate() - 1);
  if (sameCalendarDay(date, yesterday)) {
    return "Yesterday";
  }
  return new Intl.DateTimeFormat(undefined, {
    month: "short",
    day: "numeric",
    year: date.getFullYear() === new Date().getFullYear() ? undefined : "numeric",
  }).format(date);
}

function activityTimeLabel(item: ActivityItem) {
  const date = parseActivityDate(item.occurredAt);
  if (!date) {
    return item.when;
  }
  return new Intl.DateTimeFormat(undefined, {
    hour: "numeric",
    minute: "2-digit",
  }).format(date);
}

function activityFullTimeLabel(item: ActivityItem) {
  const date = parseActivityDate(item.occurredAt);
  if (!date) {
    return item.when;
  }
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(date);
}

function parseActivityDate(value?: string | null) {
  if (!value) {
    return null;
  }
  const millis = value.startsWith("unix_ms:") ? Number(value.slice("unix_ms:".length)) : Number(value);
  const date = Number.isFinite(millis)
    ? new Date(value.length <= 10 ? millis * 1000 : millis)
    : new Date(value);
  return Number.isNaN(date.getTime()) ? null : date;
}

function sameCalendarDay(left: Date, right: Date) {
  return (
    left.getFullYear() === right.getFullYear() &&
    left.getMonth() === right.getMonth() &&
    left.getDate() === right.getDate()
  );
}

function SettingsView({
  snapshot,
  onHome,
  onRefresh,
  updateStatus,
  onCheckForUpdate,
  onInstallUpdate,
  appStoreDistribution,
  theme,
  onThemeChange,
  onActivity,
  onSources,
  onResetComplete,
}: {
  snapshot: DesktopSnapshot;
  onHome: () => void;
  onRefresh: () => Promise<void>;
  updateStatus: UpdateStatus;
  onCheckForUpdate: (options?: AppUpdateCheckOptions) => Promise<void>;
  onInstallUpdate: () => Promise<void>;
  appStoreDistribution: boolean;
  theme: AppTheme;
  onThemeChange: (theme: AppTheme) => void;
  onActivity: () => void;
  onSources: () => void;
  onResetComplete: () => void;
}) {
  const [diagnosticMessage, setDiagnosticMessage] = useState("");
  const [settingsMessage, setSettingsMessage] = useState("");
  const [resetMessage, setResetMessage] = useState("");
  const [agentMessage, setAgentMessage] = useState("");
  const [installingAgents, setInstallingAgents] = useState(false);
  const [resettingState, setResettingState] = useState(false);
  const [preparingUninstall, setPreparingUninstall] = useState(false);
  const [destructiveAction, setDestructiveAction] = useState<DestructiveSettingsAction | null>(null);
  const [destructiveConfirmation, setDestructiveConfirmation] = useState("");
  const [settingsSection, setSettingsSection] = useState<SettingsSection>("general");
  const [busySetting, setBusySetting] = useState("");
  const [localSettings, setLocalSettings] = useState(snapshot.settings);
  const daemonStopped = snapshot.health.state === "stopped";
  const runtimeStopped = snapshot.health.state === "runtime_stopped";
  const runtimeNeedsRepair = daemonStopped || runtimeStopped;
  const checkingForUpdate = updateStatus.state === "checking";
  const installActionDisabled = updateInstallActionDisabled(updateStatus);
  const updateAvailable = updateNoticeVisible(updateStatus);
  const updateChannelLabel = appStoreDistribution ? "Mac App Store" : "GitHub Releases";
  const updateStatusValue = updateStatusLabel(updateStatus, appStoreDistribution);

  useEffect(() => {
    setLocalSettings(snapshot.settings);
  }, [snapshot.settings.launchAtLogin, snapshot.settings.showMenuBar]);

  async function repairRuntime() {
    if (!runtimeNeedsRepair) {
      return;
    }
    setDiagnosticMessage("");
    const report = await callCommand<ActionReport>(
      "ensure_runtime_ready",
      undefined,
      { ok: true, message: "Locality runtime is running." },
    );
    setDiagnosticMessage(report.message);
    await onRefresh().catch(() => undefined);
  }

  function copyDiagnostics() {
    const summary = [
      `Health: ${healthLabel(snapshot.health.state)}`,
      `Locality process: ${daemonStopped ? "Stopped" : "Running"}`,
      snapshot.mount.provider ? `Provider: ${providerStatusLabel(snapshot.mount.provider)}` : null,
      "State folder: ~/.loc",
      "Logs folder: ~/.loc/logs",
      `Projection: ${snapshot.mount.projection}`,
      `Connection: ${snapshot.connection.status}`,
      `Mount: ${snapshot.mount.status}`,
      `Pending changes: ${snapshot.pendingChanges.length}`,
    ].filter(Boolean).join("\n");
    copyText(summary);
    setDiagnosticMessage("Copied diagnostics summary.");
  }

  async function openLogsFolder() {
    setDiagnosticMessage("");
    const report = await callCommand<ActionReport>(
      "open_logs_folder",
      undefined,
      { ok: true, message: "Opened logs folder." },
    );
    setDiagnosticMessage(report.message);
  }

  async function updateDesktopSetting(key: "launch_at_login" | "show_menu_bar", enabled: boolean) {
    setBusySetting(key);
    setSettingsMessage("");
    const previous = localSettings;
    setLocalSettings({
      ...localSettings,
      launchAtLogin: key === "launch_at_login" ? enabled : localSettings.launchAtLogin,
      showMenuBar: key === "show_menu_bar" ? enabled : localSettings.showMenuBar,
    });
    try {
      const report = await callCommand<ActionReport>(
        "set_desktop_setting",
        { change: { key, enabled } },
        { ok: true, message: "Updated setting." },
      );
      if (!report.ok) {
        setLocalSettings(previous);
      }
      setSettingsMessage(report.message);
      await onRefresh().catch(() => undefined);
    } catch (error) {
      setLocalSettings(previous);
      setSettingsMessage(errorMessage(error));
    } finally {
      setBusySetting("");
    }
  }

  async function resetLocalState() {
    setResetMessage("");
    setResettingState(true);
    try {
      const report = await callCommand<ActionReport>(
        "reset_locality_state",
        undefined,
        { ok: true, message: "Locality local state was reset." },
      );
      setResetMessage(report.message);
      if (report.ok) {
        setDestructiveAction(null);
        setDestructiveConfirmation("");
        await callCommand<ActionReport>(
          "quit_completely",
          undefined,
          { ok: true, message: "Locality is quitting." },
        );
      }
    } catch (error) {
      setResetMessage(errorMessage(error));
    } finally {
      setResettingState(false);
    }
  }

  async function prepareUninstall() {
    setResetMessage("");
    setPreparingUninstall(true);
    try {
      const report = await callCommand<ActionReport>(
        "prepare_locality_uninstall",
        undefined,
        { ok: true, message: "Locality is ready to uninstall." },
      );
      setResetMessage(report.message);
      if (report.ok) {
        setDestructiveAction(null);
        setDestructiveConfirmation("");
        await onRefresh().catch(() => undefined);
        onResetComplete();
      }
    } catch (error) {
      setResetMessage(errorMessage(error));
    } finally {
      setPreparingUninstall(false);
    }
  }

  function requestDestructiveAction(action: DestructiveSettingsAction) {
    setResetMessage("");
    setDestructiveConfirmation("");
    setDestructiveAction(action);
  }

  function cancelDestructiveAction() {
    if (resettingState || preparingUninstall) {
      return;
    }
    setDestructiveAction(null);
    setDestructiveConfirmation("");
  }

  function confirmDestructiveAction() {
    if (destructiveAction === "reset") {
      void resetLocalState();
      return;
    }
    if (destructiveAction === "uninstall") {
      void prepareUninstall();
    }
  }

  async function installAgentInstructions() {
    setAgentMessage("");
    setInstallingAgents(true);
    try {
      const report = await callCommand<AgentGuidanceInstallReport>(
        "install_agent_guidance",
        { mountPath: snapshot.mount.localPath },
        sampleAgentGuidanceReport(snapshot.mount.localPath),
      );
      const installed = report.targets.filter((target) => target.status === "installed").length;
      const failed = report.targets.filter((target) => target.status === "failed").length;
      setAgentMessage(
        failed > 0
          ? `Installed ${installed} agent instruction target(s); ${failed} failed.`
          : `Installed ${installed} agent instruction target(s).`,
      );
    } catch (error) {
      setAgentMessage(errorMessage(error));
    } finally {
      setInstallingAgents(false);
    }
  }

  const settingsSections: Array<{ id: SettingsSection; label: string; description: string }> = [
    { id: "general", label: "General", description: "Startup and desktop behavior" },
    { id: "sources", label: "Sources", description: "Connected workspaces and local folders" },
    { id: "sync", label: "Sync", description: "Live Mode and review policy" },
    { id: "activity", label: "Activity", description: "Recent events and debug queue" },
    { id: "agents", label: "Agents", description: "Local agent instructions" },
    { id: "advanced", label: "Advanced", description: "Diagnostics, reset, and uninstall" },
    { id: "about", label: "About", description: "Updates and distribution channel" },
  ];
  const activeSettingsSection = settingsSections.find((section) => section.id === settingsSection) ?? settingsSections[0];
  const activeSourceCount = snapshot.mounts.length || (mountMissing(snapshot) ? 0 : 1);
  const recentActivity = snapshot.activity.slice(0, 4);
  const sourceStatus = connectionMissing(snapshot)
    ? "Not connected"
    : mountMissing(snapshot)
      ? "Folder needed"
      : mountStatusLabel(snapshot.mount);
  const sourceLocalPath = snapshot.mount.localPath.trim() || "Not created yet";
  const liveModeStatus = sourceSyncModeLabel(snapshot.liveMode, !mountMissing(snapshot));

  return (
    <div className="view-stack">
      <Breadcrumbs items={[{ label: PRODUCT_TERMS.home, onClick: onHome }, { label: PRODUCT_TERMS.settings }]} />
      <ViewHeader eyebrow={PRODUCT_TERMS.settings} title={activeSettingsSection.label}>
        <StatusPill tone={healthTone(snapshot.health.state)} title={healthDescription(snapshot.health.state, snapshot.health.attentionCount)}>
          {healthLabel(snapshot.health.state)}
        </StatusPill>
      </ViewHeader>

      <section className="settings-shell">
        <nav className="settings-nav" aria-label="Settings sections">
          {settingsSections.map((section) => (
            <button
              key={section.id}
              className={settingsSection === section.id ? "active" : ""}
              type="button"
              onClick={() => setSettingsSection(section.id)}
            >
              <span>{section.label}</span>
              <small>{section.description}</small>
            </button>
          ))}
        </nav>

        <div className="settings-content-panel">
          {settingsSection === "general" && (
            <section className="panel settings-section-panel">
              <PanelTitle title="Desktop behavior" />
              <ThemeRow value={theme} onChange={onThemeChange} />
              <ToggleRow
                title="Launch Locality at login"
                enabled={localSettings.launchAtLogin}
                busy={busySetting === "launch_at_login"}
                onToggle={(enabled) => void updateDesktopSetting("launch_at_login", enabled)}
              />
              <ToggleRow
                title="Show Locality in the menu bar"
                enabled={localSettings.showMenuBar}
                busy={busySetting === "show_menu_bar"}
                onToggle={(enabled) => void updateDesktopSetting("show_menu_bar", enabled)}
              />
              <SettingRow title="Default Notion folder" value="~/Library/CloudStorage/Locality/notion" />
              {settingsMessage && <p className="quiet-note inline-note">{settingsMessage}</p>}
            </section>
          )}

          {settingsSection === "sources" && (
            <section className="panel settings-section-panel">
              <PanelTitle title="Connected sources" />
              <SettingRow title="Sources" value={`${activeSourceCount} registered`} />
              <SettingRow title="Primary source" value={snapshot.mount.workspaceName} />
              <SettingRow title="Connector" value={snapshot.mount.connectorName} />
              <SettingRow title="Status" value={sourceStatus} />
              <SettingRow title="Local folder" value={sourceLocalPath} />
              <SettingRow title="Access" value={snapshot.mount.accessScope} />
              <div className="button-row">
                <PrimaryButton compact icon={<FolderOpen />} onClick={onSources}>
                  Manage Sources
                </PrimaryButton>
                <SecondaryButton compact icon={<RefreshCw />} onClick={() => void onRefresh()}>
                  Refresh
                </SecondaryButton>
              </div>
            </section>
          )}

          {settingsSection === "sync" && (
            <section className="panel settings-section-panel">
              <PanelTitle title="Sync and review policy" />
              <SettingRow title="Live Mode" value={liveModeStatus} />
              <SettingRow title="Local edits" value="Review when needed" />
              <SettingRow title="Push confirmation" value="Required for large or risky changes" />
              <SettingRow title="Remote drift" value="Pause and ask for review" />
              <SettingRow title="Pending changes" value={`${snapshot.liveMode.pendingCount} tracked`} />
              <SettingRow title="Needs review" value={`${snapshot.liveMode.reviewCount} item(s)`} />
            </section>
          )}

          {settingsSection === "activity" && (
            <section className="panel settings-section-panel">
              <PanelTitle title="Activity" />
              <SettingRow title="Recent events" value={`${snapshot.activity.length} recorded`} />
              <SettingRow title="Debug queue" value="Available in Activity" />
              <div className="settings-activity-list">
                {recentActivity.length ? (
                  recentActivity.map((item, index) => (
                    <button
                      key={`${item.title}-${item.when}-${index}`}
                      className="settings-activity-row"
                      type="button"
                      onClick={onActivity}
                    >
                      <Clock3 />
                      <span>
                        <strong>{item.title}</strong>
                        <small>{item.detail}</small>
                      </span>
                      <em>{item.when}</em>
                    </button>
                  ))
                ) : (
                  <div className="settings-empty-state">No activity recorded yet.</div>
                )}
              </div>
              <SecondaryButton compact icon={<Clock3 />} onClick={onActivity}>
                Open Activity Log
              </SecondaryButton>
            </section>
          )}

          {settingsSection === "agents" && (
            <section className="panel settings-section-panel">
              <PanelTitle title="Agent instructions" />
              <SettingRow title="Local agents" value="Claude, Codex, Warp, Cursor, Gemini, Cline/Roo" />
              <SettingRow title="Notion guidance" value="Installed under /Locality/notion" />
              <SettingRow title="Behavior" value="Edit mounted Markdown directly, review when needed" />
              <SecondaryButton
                compact
                icon={installingAgents ? <Loader2 className="spin-icon" /> : <Bot />}
                disabled={installingAgents}
                onClick={() => void installAgentInstructions()}
              >
                {installingAgents ? "Installing" : "Install Agent Skills"}
              </SecondaryButton>
              {agentMessage && <p className="quiet-note inline-note">{agentMessage}</p>}
            </section>
          )}

          {settingsSection === "advanced" && (
            <>
              <section className="panel settings-section-panel">
                <PanelTitle title="Diagnostics" />
                <SettingRow title="Locality process" value={daemonStopped ? "Stopped" : "Running"} />
                {snapshot.mount.provider && (
                  <SettingRow title="Provider" value={providerStatusLabel(snapshot.mount.provider)} />
                )}
                <SettingRow title="State folder" value="~/.loc" />
                <SettingRow title="Logs folder" value="~/.loc/logs" />
                <SettingRow title="Projection" value={snapshot.mount.projection} />
                <div className="button-row">
                  <SecondaryButton compact onClick={copyDiagnostics}>
                    Copy Summary
                  </SecondaryButton>
                  <SecondaryButton compact icon={<FolderOpen />} onClick={() => void openLogsFolder()}>
                    Open Logs
                  </SecondaryButton>
                  <SecondaryButton compact disabled={!runtimeNeedsRepair} onClick={() => void repairRuntime()}>
                    {runtimeNeedsRepair ? "Start Locality" : "Repair Locality"}
                  </SecondaryButton>
                </div>
                {diagnosticMessage && <p className="quiet-note inline-note">{diagnosticMessage}</p>}
              </section>

              <details className="danger-accordion settings-danger-accordion">
                <summary>
                  <span>
                    <AlertTriangle />
                    Danger Zone
                  </span>
                  <small>Reset and uninstall tools</small>
                </summary>
                <div className="settings-danger-body">
                  <SettingRow title="Local database" value="~/.loc/state.sqlite3" />
                  <SettingRow title="Reset behavior" value="Preserve local files" />
                  <div className="button-row">
                    <SecondaryButton
                      compact
                      icon={resettingState ? <Loader2 className="spin-icon" /> : <RotateCcw />}
                      disabled={resettingState || preparingUninstall}
                      onClick={() => requestDestructiveAction("reset")}
                    >
                      {resettingState ? "Resetting" : "Reset Local State"}
                    </SecondaryButton>
                    <SecondaryButton
                      compact
                      icon={preparingUninstall ? <Loader2 className="spin-icon" /> : <Trash2 />}
                      disabled={resettingState || preparingUninstall}
                      onClick={() => requestDestructiveAction("uninstall")}
                    >
                      {preparingUninstall ? "Preparing" : "Prepare for Uninstall"}
                    </SecondaryButton>
                  </div>
                  {resetMessage && <p className="quiet-note inline-note">{resetMessage}</p>}
                </div>
              </details>

              <section className="panel settings-section-panel">
                <PanelTitle title="Quit options" />
                <button className="option-row" onClick={() => void callCommand("hide_menubar", undefined, { ok: true })}>
                  <EyeOff />
                  <span>Don't Show in Menubar</span>
                  <ChevronRight />
                </button>
                <button className="option-row danger" onClick={() => void callCommand("quit_completely", undefined, { ok: true })}>
                  <Power />
                  <span>Quit Completely</span>
                  <ChevronRight />
                </button>
              </section>
            </>
          )}

          {settingsSection === "about" && (
            <section className="panel settings-section-panel">
              <PanelTitle title="Updates" />
              <SettingRow title="Channel" value={updateChannelLabel} />
              <SettingRow title="Status" value={updateStatusValue} />
              {updateStatus.version && <SettingRow title="Available version" value={updateStatus.version} />}
              {!appStoreDistribution && (
                <div className="button-row">
                  <SecondaryButton
                    compact
                    icon={checkingForUpdate ? <Loader2 className="spin-icon" /> : <RefreshCw />}
                    disabled={checkingForUpdate || updateAvailable}
                    onClick={() => void onCheckForUpdate()}
                  >
                    {checkingForUpdate ? "Checking" : "Check"}
                  </SecondaryButton>
                  <PrimaryButton
                    compact
                    icon={
                      updateStatus.state === "downloading" || updateStatus.state === "installing"
                        ? <Loader2 className="spin-icon" />
                        : <Download />
                    }
                    disabled={!updateAvailable || installActionDisabled}
                    onClick={() => void onInstallUpdate()}
                  >
                    {updateInstallActionLabel(updateStatus)}
                  </PrimaryButton>
                </div>
              )}
            </section>
          )}
        </div>
      </section>
      {destructiveAction && (
        <DestructiveSettingsDialog
          action={destructiveAction}
          value={destructiveConfirmation}
          busy={resettingState || preparingUninstall}
          onChange={setDestructiveConfirmation}
          onCancel={cancelDestructiveAction}
          onConfirm={confirmDestructiveAction}
        />
      )}
    </div>
  );
}

function DestructiveSettingsDialog({
  action,
  value,
  busy,
  onChange,
  onCancel,
  onConfirm,
}: {
  action: DestructiveSettingsAction;
  value: string;
  busy: boolean;
  onChange: (value: string) => void;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const isReset = action === "reset";
  const requiredValue = isReset ? "RESET" : "UNINSTALL";
  const confirmed = value.trim().toUpperCase() === requiredValue;
  const title = isReset ? "Reset local state" : "Prepare for uninstall";
  const actionLabel = isReset ? "Reset local state" : "Prepare for uninstall";
  const description = isReset
    ? "This removes Locality's local database, credentials, sync metadata, and mount registration on this machine."
    : "This stops Locality, removes local agent integrations and MCP config, and clears Locality local state on this machine.";
  const consequences = isReset
    ? [
        "Your files in the Locality folder are kept.",
        "Your Notion workspace is not changed.",
        "You will need to reconnect and re-verify the local folder.",
      ]
    : [
        "Your files in the Locality folder are kept.",
        "Your Notion workspace is not changed.",
        "Locality opens onboarding again after cleanup.",
      ];

  return (
    <TypedDestructiveDialog
      titleId="destructive-settings-title"
      title={title}
      description={description}
      consequences={consequences}
      requiredValue={requiredValue}
      actionLabel={actionLabel}
      value={value}
      confirmed={confirmed}
      busy={busy}
      onChange={onChange}
      onCancel={onCancel}
      onConfirm={onConfirm}
    />
  );
}

function TrayPopover({
  snapshot,
  onRefresh,
}: {
  snapshot: DesktopSnapshot;
  onRefresh: () => Promise<void>;
}) {
  const [url, setUrl] = useState("");
  const [locateState, setLocateState] = useState<LocateState>("idle");
  const [locateError, setLocateError] = useState("");
  const [locatedItem, setLocatedItem] = useState<LocatedItem | null>(null);
  const [quitOptionsOpen, setQuitOptionsOpen] = useState(false);
  const {
    liveModeEnabled,
    liveModeBusy,
    liveModeState,
    liveModeMessage,
    toggleLiveMode,
  } = useMountLiveModeController(snapshot, onRefresh);
  const quitOptionsRef = useRef<HTMLDivElement | null>(null);
  const { results: searchResults, searching } = useNotionSearchResults(url);
  const visibleChanges = snapshot.pendingChanges.slice(0, 3);
  const visibleSearchResults = locateState === "ready" ? [] : searchResults.slice(0, 3);
  const trayReviewCounts = reviewQueueCounts(snapshot.pendingChanges);
  const trayAccountLabel = snapshot.connection.accountLabel || snapshot.connection.workspaceName || "Locality";

  useEffect(() => {
    if (!quitOptionsOpen) {
      return undefined;
    }

    const closeOnOutsideClick = (event: PointerEvent) => {
      if (!quitOptionsRef.current?.contains(event.target as Node)) {
        setQuitOptionsOpen(false);
      }
    };
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setQuitOptionsOpen(false);
      }
    };

    document.addEventListener("pointerdown", closeOnOutsideClick, true);
    document.addEventListener("keydown", closeOnEscape);
    return () => {
      document.removeEventListener("pointerdown", closeOnOutsideClick, true);
      document.removeEventListener("keydown", closeOnEscape);
    };
  }, [quitOptionsOpen]);

  async function locatePage() {
    if (!url.trim()) {
      return;
    }

    setLocateState("preparing");
    setLocateError("");
    try {
      const item = await callCommand<LocatedItem>(
        "locate_notion_page",
        { url },
        {
          title: "Roadmap 2026",
          kind: "Page",
          localPath: "~/Library/CloudStorage/Locality/notion/Engineering/Roadmap 2026/page.md",
          state: "ready",
        },
      );
      setLocatedItem(item);
      setLocateState("ready");
    } catch (error) {
      setLocateError(errorMessage(error));
      setLocatedItem(null);
      setLocateState("error");
    }
  }

  function selectSearchResult(item: LocatedItem) {
    setLocatedItem(item);
    setLocateState("ready");
    setLocateError("");
    setUrl(item.title);
  }

  function openMain(view?: AppView) {
    void callCommand("show_main_window", { view }, { ok: true });
  }

  return (
    <main className="tray-popover">
      <header className="tray-header">
        <div className="tray-title">
          <LocalityLogo surface="light" state={healthIconState(snapshot.health.state)} />
          <strong>Locality</strong>
        </div>
        <StatusPill
          tone={healthTone(snapshot.health.state)}
          title={healthDescription(snapshot.health.state, snapshot.health.attentionCount)}
        >
          {healthLabel(snapshot.health.state)}
        </StatusPill>
      </header>

      <section className="tray-section tray-search-section">
        <div className="tray-locate-row">
          <Search />
          <input
            value={url}
            placeholder="Locate a page or file"
            onChange={(event) => {
              setUrl(event.target.value);
              setLocateState("idle");
              setLocateError("");
              setLocatedItem(null);
            }}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                locatePage();
              }
            }}
          />
          <button disabled={!url.trim() || locateState === "preparing"} onClick={locatePage}>
            {locateState === "preparing" ? "..." : "Open"}
          </button>
        </div>
        {locateState === "error" && <p className="field-error">{locateError || "Paste a Notion URL or search title."}</p>}
        {visibleSearchResults.length > 0 && (
          <div className="tray-search-results" aria-busy={searching ? "true" : "false"}>
            {visibleSearchResults.map((item) => (
              <button type="button" key={`${item.kind}-${item.localPath}`} onClick={() => selectSearchResult(item)}>
                <strong>{item.title}</strong>
                <small>{item.localPath}</small>
                <span className={`search-state ${item.state}`}>{locatedStateLabel(item.state)}</span>
              </button>
            ))}
          </div>
        )}
        {locatedItem && (
          <div className="tray-result">
            <strong>{locatedItem.title}</strong>
            <code>{locatedItem.localPath}</code>
            <button onClick={() => copyText(locatedItem.localPath)}>Copy Path</button>
            <button onClick={() => void callCommand("reveal_path", { path: locatedItem.localPath }, { ok: true })}>
              Reveal
            </button>
          </div>
        )}
      </section>

      <section className="tray-status-row">
        <button className="tray-status-copy" type="button" onClick={() => openMain("pending")}>
          <StatusPill
            tone={trayReviewCounts.total > 0 ? "warn" : healthTone(snapshot.health.state)}
            title={trayReviewCounts.total > 0 ? "Local edits need review." : healthDescription(snapshot.health.state, snapshot.health.attentionCount)}
          >
            {trayReviewCounts.total > 0 ? `${trayReviewCounts.total} need review` : "All changes reviewed"}
          </StatusPill>
        </button>
        <button
          className="tray-live-inline"
          aria-pressed={liveModeEnabled}
          aria-label={`${liveModeEnabled ? "Turn off" : "Turn on"} Live Mode`}
          disabled={mountMissing(snapshot) || connectionMissing(snapshot)}
          onClick={() => void toggleLiveMode()}
        >
          <span>{liveModeBusy ? "Syncing" : "Live Mode"}</span>
          <span className={`toggle ${liveModeEnabled ? "enabled" : ""}`} aria-hidden="true">
            <i />
          </span>
        </button>
      </section>
      {liveModeMessage && (
        <p className={liveModeState === "error" ? "field-error" : "quiet-note inline-note"}>
          {liveModeMessage}
        </p>
      )}

      <section className="tray-section tray-list-section">
        <div className="tray-section-heading">
          <span>{PRODUCT_TERMS.reviewCenter} ({trayReviewCounts.total})</span>
          <button onClick={() => openMain("pending")}>{trayReviewCounts.total > 0 ? "See all" : "Open"}</button>
        </div>
        {visibleChanges.length > 0 ? (
          visibleChanges.slice(0, 2).map((change) => (
            <button
              className="tray-list-row"
              key={change.localPath}
              onClick={() =>
                void callCommand("open_path", { path: joinMountPath(snapshot.mount.localPath, change.localPath) }, { ok: true })
              }
            >
              <ConnectorIcon connector="notion" />
              <span>
                <strong>{change.title}</strong>
                <small>{change.summary}</small>
              </span>
              <em>Review</em>
            </button>
          ))
        ) : (
          <div className="tray-empty-row">No changes waiting for review.</div>
        )}
      </section>

      {snapshot.recentFiles.length > 0 && (
        <section className="tray-section tray-list-section tray-recent-files">
          <div className="tray-section-heading">
            <span>Recent</span>
            <button onClick={() => openMain("files")}>View</button>
          </div>
          {snapshot.recentFiles.slice(0, 2).map((item) => (
            <button
              className="tray-list-row"
              type="button"
              key={`${item.kind}-${item.localPath}`}
              onClick={() => void callCommand("reveal_path", { path: item.localPath }, { ok: true })}
            >
              <ConnectorIcon connector="notion" />
              <span>
                <strong>{item.title}</strong>
                <small>{item.localPath}</small>
              </span>
              <span className={`search-state ${item.state}`}>{locatedStateLabel(item.state)}</span>
            </button>
          ))}
        </section>
      )}

      <section className="tray-controls-row">
        <button disabled title="Pause presets need backend support">Pause syncing</button>
        <button onClick={() => void callCommand("open_path", { path: snapshot.mount.localPath }, { ok: true })}>
          Open folder
        </button>
        <button onClick={() => openMain("activity")}>Activity</button>
      </section>

      <footer className="tray-footer">
        <span className="tray-account">{trayAccountLabel}</span>
        <button onClick={() => openMain("settings")}>Settings</button>
        <div className="tray-quit-options" ref={quitOptionsRef}>
          <button onClick={() => setQuitOptionsOpen((open) => !open)}>Quit</button>
          {quitOptionsOpen && (
            <div className="tray-quit-menu">
              <button onClick={() => void callCommand("hide_menubar", undefined, { ok: true })}>
                Don't Show in Menubar
              </button>
              <button className="danger" onClick={() => void callCommand("quit_completely", undefined, { ok: true })}>
                Quit Completely
              </button>
            </div>
          )}
        </div>
      </footer>
    </main>
  );
}

type FileActionStatus = {
  state: "working" | "success" | "error";
  message: string;
  action?: FileAction | "live_mode";
};

type FileDetailStatus = {
  state: "loading" | "ready" | "error";
  report?: FileDetailReport;
  message: string;
};

type FileEditorStatus = {
  state: "loading" | "ready" | "saving" | "error";
  contents: string;
  savedContents: string;
  message: string;
  hasConflictMarkers: boolean;
};

type MarkdownEditorView = {
  state: { doc: { toString: () => string } };
  dispatch: (transaction: { changes: { from: number; to: number; insert: string } }) => void;
  destroy: () => void;
};

function isRemoteDeletedChange(change: PendingChange) {
  return change.issueCodes.some(
    (code) => code === "remote_deleted" || code === "remote_deleted_with_local_pending",
  );
}

type FileAction = "diff" | "push" | "resolve" | "check" | "reset" | "draft";

function fileActionWorkingMessage(action: FileAction, remoteDeleted: boolean) {
  switch (action) {
    case "diff":
      return "Checking diff...";
    case "push":
      return "Pushing this file...";
    case "check":
      return "Checking Notion...";
    case "draft":
      return "Saving local draft...";
    case "reset":
      return remoteDeleted ? "Removing local copy..." : "Resetting to remote...";
    case "resolve":
      return "Pulling latest...";
  }
}

function fileActionCommand(action: FileAction) {
  switch (action) {
    case "diff":
      return "diff_notion_file";
    case "push":
      return "push_notion_file";
    case "check":
      return "check_notion_file";
    case "draft":
      return "keep_notion_file_as_draft";
    case "reset":
      return "reset_notion_file_to_remote";
    case "resolve":
      return "pull_notion_file";
  }
}

function FileChangeList({
  changes,
  mountPath,
  confirmDangerous = false,
  onReview,
  onRefresh,
}: {
  changes: PendingChange[];
  mountPath: string;
  confirmDangerous?: boolean;
  onReview?: () => void;
  onRefresh?: () => Promise<void>;
}) {
  const [actions, setActions] = useState<Record<string, FileActionStatus>>({});
  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  const [details, setDetails] = useState<Record<string, FileDetailStatus>>({});
  const [editors, setEditors] = useState<Record<string, FileEditorStatus>>({});
  const [liveModeOverrides, setLiveModeOverrides] = useState<Record<string, PendingChange["liveMode"]>>({});

  useEffect(() => {
    setLiveModeOverrides((current) => {
      let changed = false;
      const next = { ...current };
      const activePaths = new Set(changes.map((change) => change.localPath));
      for (const path of Object.keys(next)) {
        if (!activePaths.has(path)) {
          delete next[path];
          changed = true;
        }
      }
      for (const change of changes) {
        const override = next[change.localPath];
        if (
          override &&
          override.enabled === change.liveMode.enabled &&
          override.state === change.liveMode.state &&
          override.label === change.liveMode.label
        ) {
          delete next[change.localPath];
          changed = true;
        }
      }
      return changed ? next : current;
    });
  }, [changes]);

  async function loadFileDetails(change: PendingChange) {
    setDetails((current) => ({
      ...current,
      [change.localPath]: { state: "loading", message: "Reading local file..." },
    }));
    setEditors((current) => ({
      ...current,
      [change.localPath]: {
        state: "loading",
        contents: "",
        savedContents: "",
        message: "Loading editor...",
        hasConflictMarkers: false,
      },
    }));

    try {
      const path = joinMountPath(mountPath, change.localPath);
      const [report, editor] = await Promise.all([
        callCommand<FileDetailReport>("inspect_notion_file", { path }),
        callCommand<FileEditorReport>("read_notion_file", { path }),
      ]);
      setEditors((current) => ({
        ...current,
        [change.localPath]: {
          state: editor.ok ? "ready" : "error",
          contents: editor.contents,
          savedContents: editor.contents,
          message: editor.message,
          hasConflictMarkers: editor.hasConflictMarkers,
        },
      }));
      setDetails((current) => ({
        ...current,
        [change.localPath]: {
          state: report.ok ? "ready" : "error",
          report,
          message: report.message,
        },
      }));
    } catch (error) {
      setDetails((current) => ({
        ...current,
        [change.localPath]: { state: "error", message: errorMessage(error) },
      }));
      setEditors((current) => ({
        ...current,
        [change.localPath]: {
          state: "error",
          contents: "",
          savedContents: "",
          message: errorMessage(error),
          hasConflictMarkers: false,
        },
      }));
    }
  }

  async function toggleDetails(change: PendingChange) {
    if (selectedPath === change.localPath) {
      setSelectedPath(null);
      return;
    }

    setSelectedPath(change.localPath);
    await loadFileDetails(change);
  }

  async function saveEditor(change: PendingChange) {
    const editor = editors[change.localPath];
    if (!editor || editor.state === "loading" || editor.state === "saving") {
      return;
    }
    setEditors((current) => ({
      ...current,
      [change.localPath]: { ...editor, state: "saving", message: "Saving local Markdown..." },
    }));

    try {
      const report = await callCommand<ActionReport>("save_notion_file", {
        path: joinMountPath(mountPath, change.localPath),
        contents: editor.contents,
      });
      setEditors((current) => ({
        ...current,
        [change.localPath]: {
          ...editor,
          state: report.ok ? "ready" : "error",
          savedContents: report.ok ? editor.contents : editor.savedContents,
          message: report.message,
          hasConflictMarkers: hasConflictMarkers(editor.contents),
        },
      }));
      if (report.ok) {
        await onRefresh?.().catch(() => undefined);
      }
    } catch (error) {
      setEditors((current) => ({
        ...current,
        [change.localPath]: { ...editor, state: "error", message: errorMessage(error) },
      }));
    }
  }

  async function runFileAction(change: PendingChange, action: FileAction) {
    const remoteDeleted = isRemoteDeletedChange(change);
    const path = joinMountPath(mountPath, change.localPath);
    const workingMessage = fileActionWorkingMessage(action, remoteDeleted);
    setActions((current) => ({
      ...current,
      [change.localPath]: { state: "working", message: workingMessage, action },
    }));

    try {
      const command = fileActionCommand(action);
      const args =
        action === "push"
          ? { path, confirmDangerous }
          : {
              path,
            };
      const report = await callCommand<ActionReport>(command, args);
      setActions((current) => ({
        ...current,
        [change.localPath]: {
          state: report.ok ? "success" : "error",
          message: report.message,
          action,
        },
      }));
      if (report.ok && (action === "resolve" || action === "reset") && selectedPath === change.localPath) {
        await loadFileDetails(change);
      }
      if (report.ok && action !== "diff") {
        await onRefresh?.().catch(() => undefined);
      }
    } catch (error) {
      setActions((current) => ({
        ...current,
        [change.localPath]: { state: "error", message: errorMessage(error), action },
      }));
    }
  }

  async function toggleFileLiveMode(change: PendingChange, enabled: boolean) {
    const path = joinMountPath(mountPath, change.localPath);
    const optimisticState: PendingChange["liveMode"] = {
      ...change.liveMode,
      enabled,
      state: enabled ? "active" : "off",
      label: enabled ? "Live Mode on" : "Live Mode off",
      reason: null,
    };
    setLiveModeOverrides((current) => ({
      ...current,
      [change.localPath]: optimisticState,
    }));
    setActions((current) => ({
      ...current,
      [change.localPath]: {
        state: "working",
        message: enabled ? "Turning on Live Mode..." : "Turning off Live Mode...",
        action: "live_mode",
      },
    }));

    try {
      const report = await callCommand<ActionReport>("set_live_mode_for_file", {
        change: { path, enabled },
      });
      setActions((current) => ({
        ...current,
        [change.localPath]: {
          state: report.ok ? "success" : "error",
          message: report.message,
          action: "live_mode",
        },
      }));
      if (!report.ok) {
        setLiveModeOverrides((current) => ({
          ...current,
          [change.localPath]: change.liveMode,
        }));
      }
      await onRefresh?.().catch(() => undefined);
    } catch (error) {
      setLiveModeOverrides((current) => ({
        ...current,
        [change.localPath]: change.liveMode,
      }));
      setActions((current) => ({
        ...current,
        [change.localPath]: { state: "error", message: errorMessage(error), action: "live_mode" },
      }));
    }
  }

  return (
    <section className="file-list">
      {changes.map((change) => {
        const action = actions[change.localPath];
        const detail = details[change.localPath];
        const editor = editors[change.localPath];
        const isWorking = action?.state === "working";
        const isPushingFile = isWorking && action?.action === "push";
        const isSaving = editor?.state === "saving";
        const hasUnsavedEditorChanges = editor !== undefined && editor.contents !== editor.savedContents;
        const shouldReviewBeforePush = Boolean(!confirmDangerous && change.state === "needs_review" && onReview);
        const actionNeedsReview = Boolean(action?.state === "error" && pushNeedsReview(action.message) && onReview);
        const isSelected = selectedPath === change.localPath;
        const liveMode = liveModeOverrides[change.localPath] ?? change.liveMode;
        const remoteDeleted = isRemoteDeletedChange(change);
        return (
          <article className={`file-row ${change.state} ${isSelected ? "expanded" : ""}`} key={change.localPath}>
            <div className="file-state">
              {change.state === "needs_review" || change.state === "blocked" || change.state === "conflict" ? (
                <AlertTriangle />
              ) : (
                <Check />
              )}
            </div>
            <div
              className="file-row-content"
              role="button"
              tabIndex={0}
              onClick={() => void toggleDetails(change)}
              onKeyDown={(event) => {
                if (event.key === "Enter" || event.key === " ") {
                  event.preventDefault();
                  void toggleDetails(change);
                }
              }}
            >
              <h3>{change.title}</h3>
              <p>{change.localPath}</p>
              <span>{change.summary}</span>
              {action && (
                <div className={`file-action-message ${action.state}`}>
                  {action.state === "working" && <Loader2 className="spin-icon" />}
                  <span>{action.message}</span>
                  {actionNeedsReview && (
                    <button
                      className="inline-review-button"
                      type="button"
                      onClick={(event) => {
                        event.stopPropagation();
                        onReview?.();
                      }}
                    >
                      Review Push
                    </button>
                  )}
                </div>
              )}
            </div>
            <div className="file-row-actions">
              <div className={`file-live-mode ${liveMode.state}`}>
                <span title={liveMode.reason || liveMode.label}>
                  <Zap />
                  {liveMode.label}
                </span>
                <button
                  className={`toggle ${liveMode.enabled ? "enabled" : ""}`}
                  type="button"
                  disabled={isWorking}
                  aria-label={`${liveMode.enabled ? "Turn off" : "Turn on"} Live Mode for ${change.title}`}
                  onClick={() => void toggleFileLiveMode(change, !liveMode.enabled)}
                >
                  <i />
                </button>
              </div>
              <div className="file-utility-actions">
                <IconButton
                  label="Show diff"
                  disabled={isWorking}
                  icon={<Search />}
                  onClick={() => void runFileAction(change, "diff")}
                />
                <IconButton
                  label={remoteDeleted ? "Check again" : "Pull latest"}
                  disabled={isWorking}
                  icon={<RefreshCw />}
                  onClick={() => void runFileAction(change, remoteDeleted ? "check" : "resolve")}
                />
                <IconButton
                  label={remoteDeleted ? "Remove local copy" : "Reset to remote"}
                  disabled={isWorking}
                  icon={remoteDeleted ? <Trash2 /> : <RotateCcw />}
                  onClick={() => void runFileAction(change, "reset")}
                />
                <IconButton
                  label={remoteDeleted ? "Open local copy" : "Open file"}
                  disabled={isWorking}
                  icon={<FolderOpen />}
                  onClick={() =>
                    void callCommand("open_path", { path: joinMountPath(mountPath, change.localPath) }, { ok: true })
                  }
                />
              </div>
              <PrimaryButton
                compact
                icon={
                  remoteDeleted ? (
                    <FolderOpen />
                  ) : isPushingFile ? (
                    <Loader2 className="spin-icon" />
                  ) : shouldReviewBeforePush ? (
                    <ListChecks />
                  ) : (
                    <ShieldCheck />
                  )
                }
                disabled={isWorking}
                onClick={() => {
                  if (remoteDeleted) {
                    void runFileAction(change, "draft");
                    return;
                  }
                  if (shouldReviewBeforePush) {
                    onReview?.();
                    return;
                  }
                  void runFileAction(change, "push");
                }}
              >
                {remoteDeleted ? "Keep Draft" : isPushingFile ? "Pushing..." : shouldReviewBeforePush ? "Review" : "Push"}
              </PrimaryButton>
            </div>
            {isSelected && (
              <div className="file-detail-panel">
                <div className="file-detail-heading">
                  <div className="file-detail-copy">
                    <strong>{editor?.hasConflictMarkers ? "Conflict markers found" : "Local Markdown editor"}</strong>
                    <span>{editor?.message || detail?.message || "Reading local file..."}</span>
                  </div>
                  <SecondaryButton compact icon={<ChevronUp />} onClick={() => setSelectedPath(null)}>
                    Collapse
                  </SecondaryButton>
                </div>
                {editor?.state === "loading" && (
                  <div className="editor-loading">
                    <Loader2 className="spin-icon" />
                    Loading editor...
                  </div>
                )}
                {editor && editor.state !== "loading" && (
                  <>
                    {editor.hasConflictMarkers && (
                      <div className="editor-warning">
                        Resolve the marker block in the editor, then save before pushing.
                      </div>
                    )}
                    <MarkdownEditor
                      value={editor.contents}
                      onChange={(contents) =>
                        setEditors((current) => ({
                          ...current,
                          [change.localPath]: {
                            ...editor,
                            state: "ready",
                            contents,
                            message:
                              contents === editor.savedContents
                                ? "No unsaved editor changes."
                                : "Unsaved local editor changes.",
                            hasConflictMarkers: hasConflictMarkers(contents),
                          },
                        }))
                      }
                    />
                    <div className="editor-actions">
                      <SecondaryButton
                        compact
                        disabled={isSaving || !hasUnsavedEditorChanges}
                        onClick={() => void saveEditor(change)}
                      >
                        {isSaving ? "Saving..." : "Save Local"}
                      </SecondaryButton>
                      <PrimaryButton
                        compact
                        disabled={isSaving || hasUnsavedEditorChanges || isWorking}
                        icon={isPushingFile ? <Loader2 className="spin-icon" /> : undefined}
                        onClick={() => {
                          if (shouldReviewBeforePush) {
                            onReview?.();
                            return;
                          }
                          void runFileAction(change, "push");
                        }}
                      >
                        {isPushingFile ? "Pushing..." : shouldReviewBeforePush ? "Review Saved" : "Push Saved"}
                      </PrimaryButton>
                    </div>
                  </>
                )}
                {detail?.report?.conflictPreview && !editor?.hasConflictMarkers && (
                  <pre>{detail.report.conflictPreview}</pre>
                )}
              </div>
            )}
          </article>
        );
      })}
    </section>
  );
}

function MarkdownEditor({ value, onChange }: { value: string; onChange: (value: string) => void }) {
  const hostRef = useRef<HTMLDivElement | null>(null);
  const viewRef = useRef<MarkdownEditorView | null>(null);
  const onChangeRef = useRef(onChange);

  useEffect(() => {
    onChangeRef.current = onChange;
  }, [onChange]);

  useEffect(() => {
    const host = hostRef.current;
    if (!host) {
      return undefined;
    }
    const editorHost = host;

    let cancelled = false;

    async function loadEditor() {
      const [
        commands,
        markdownModule,
        languageModule,
        searchModule,
        stateModule,
        viewModule,
      ] = await Promise.all([
        import("@codemirror/commands"),
        import("@codemirror/lang-markdown"),
        import("@codemirror/language"),
        import("@codemirror/search"),
        import("@codemirror/state"),
        import("@codemirror/view"),
      ]);
      if (cancelled) {
        return;
      }

      const view = new viewModule.EditorView({
        parent: editorHost,
        state: stateModule.EditorState.create({
          doc: value,
          extensions: [
            viewModule.lineNumbers(),
            viewModule.drawSelection(),
            viewModule.highlightActiveLine(),
            commands.history(),
            markdownModule.markdown(),
            languageModule.syntaxHighlighting(languageModule.defaultHighlightStyle),
            searchModule.highlightSelectionMatches(),
            viewModule.keymap.of([
              commands.indentWithTab,
              ...commands.defaultKeymap,
              ...commands.historyKeymap,
              ...searchModule.searchKeymap,
            ]),
            viewModule.EditorView.lineWrapping,
            viewModule.EditorView.updateListener.of((update) => {
              if (update.docChanged) {
                onChangeRef.current(update.state.doc.toString());
              }
            }),
            viewModule.EditorView.theme({
              "&": {
                minHeight: "320px",
                fontSize: "13px",
              },
              ".cm-content": {
                fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
                padding: "12px",
              },
              ".cm-gutters": {
                backgroundColor: "#f7f9fb",
                color: "#8a96a6",
                borderRight: "1px solid #dfe6eb",
              },
              ".cm-activeLine": {
                backgroundColor: "rgba(49, 120, 198, 0.07)",
              },
              ".cm-activeLineGutter": {
                backgroundColor: "rgba(49, 120, 198, 0.08)",
              },
              "&.cm-focused": {
                outline: "none",
              },
            }),
          ],
        }),
      });
      viewRef.current = view;
    }

    void loadEditor();

    return () => {
      cancelled = true;
      viewRef.current?.destroy();
      viewRef.current = null;
    };
  }, []);

  useEffect(() => {
    const view = viewRef.current;
    if (!view) {
      return;
    }
    const current = view.state.doc.toString();
    if (current !== value) {
      view.dispatch({
        changes: { from: 0, to: current.length, insert: value },
      });
    }
  }, [value]);

  return <div className="markdown-editor" ref={hostRef} />;
}

function LocateBox({
  label,
  value,
  onChange,
  onSubmit,
  onSelect,
  state,
  error,
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  onSubmit: () => void;
  onSelect?: (item: LocatedItem) => void;
  state: LocateState;
  error?: string;
}) {
  const { results, searching } = useNotionSearchResults(value);

  return (
    <div className="locate-box">
      <label>{label}</label>
      <div className="locate-row">
        <Search />
        <input
          value={value}
          placeholder="Paste a Notion URL or search by title/path"
          onChange={(event) => onChange(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter") {
              onSubmit();
            }
          }}
        />
        <PrimaryButton compact disabled={!value.trim() || state === "preparing"} onClick={onSubmit}>
          {state === "preparing" ? "Preparing" : "Open Page"}
        </PrimaryButton>
      </div>
      {state !== "ready" && results.length > 0 && (
        <SearchResultList
          items={results}
          searching={searching}
          onSelect={(item) => {
            onSelect?.(item);
          }}
        />
      )}
      {state === "error" && <p className="field-error">{error || "Paste a Notion URL or search title/path."}</p>}
    </div>
  );
}

function SearchResultList({
  items,
  searching,
  onSelect,
}: {
  items: LocatedItem[];
  searching?: boolean;
  onSelect: (item: LocatedItem) => void;
}) {
  return (
    <div className="search-results" aria-busy={searching ? "true" : "false"}>
      {items.map((item) => (
        <button type="button" key={`${item.kind}-${item.localPath}`} onClick={() => onSelect(item)}>
          <div>
            <strong>{item.title}</strong>
            <code>{item.localPath}</code>
          </div>
          <span className={`search-state ${item.state}`}>{locatedStateLabel(item.state)}</span>
        </button>
      ))}
    </div>
  );
}

function LocatedPath({ item }: { item: LocatedItem }) {
  const [revealing, setRevealing] = useState(false);
  const [revealError, setRevealError] = useState("");

  async function revealLocatedPath() {
    setRevealing(true);
    setRevealError("");
    try {
      const report = await callCommand<ActionReport>("reveal_path", { path: item.localPath }, { ok: true, message: "" });
      if (!report.ok) {
        setRevealError(report.message);
      }
    } catch (error) {
      setRevealError(errorMessage(error));
    } finally {
      setRevealing(false);
    }
  }

  return (
    <div className="located-path">
      <div>
        <p className="label">{item.kind}</p>
        <h3>{item.title}</h3>
        <code>{item.localPath}</code>
      </div>
      <div className="button-row">
        <SecondaryButton compact icon={<Copy />} onClick={() => copyText(item.localPath)}>
          Copy Path
        </SecondaryButton>
        <SecondaryButton compact busy={revealing} icon={<FolderOpen />} onClick={() => void revealLocatedPath()}>
          {revealing ? "Preparing..." : "Reveal in Finder"}
        </SecondaryButton>
      </div>
      {revealError && <p className="field-error">{revealError}</p>}
    </div>
  );
}

function AgentPrompt() {
  return (
    <div className="agent-prompt">
      <Clipboard />
      <div>
        <span>Try this with an agent</span>
        <p>"Edit this Notion file and make the launch plan clearer."</p>
      </div>
    </div>
  );
}

function Breadcrumbs({ items }: { items: { label: string; onClick?: () => void }[] }) {
  return (
    <nav className="breadcrumbs" aria-label="Breadcrumb">
      {items.map((item, index) => (
        <span key={`${item.label}-${index}`}>
          {item.onClick ? (
            <button type="button" onClick={item.onClick}>
              {item.label}
            </button>
          ) : (
            <strong>{item.label}</strong>
          )}
        </span>
      ))}
    </nav>
  );
}

function ViewHeader({
  eyebrow,
  title,
  children,
}: {
  eyebrow?: string;
  title: string;
  children?: React.ReactNode;
}) {
  return (
    <header className="view-header">
      <div>
        {eyebrow && <p className="eyebrow">{eyebrow}</p>}
        <h1>{title}</h1>
      </div>
      {children}
    </header>
  );
}

function ProductLoopDemo() {
  const [videoAvailable, setVideoAvailable] = useState(Boolean(onboardingDemoVideoUrl));

  if (onboardingDemoVideoUrl && videoAvailable) {
    return (
      <div className="onboarding-video-demo">
        <video
          aria-label="Locality product demo"
          autoPlay
          loop
          muted
          playsInline
          preload="auto"
          onError={() => setVideoAvailable(false)}
        >
          <source src={onboardingDemoVideoUrl} type="video/mp4" />
        </video>
      </div>
    );
  }

  return (
    <div className="onboarding-product-demo">
      <div className="demo-tile-grid">
        <div className="demo-tile">
          <div>
            <strong>Notion</strong>
            <span>Connected</span>
          </div>
          <p>Launch Plan</p>
        </div>
        <div className="demo-tile">
          <div>
            <strong>Local Markdown</strong>
            <span>Editable</span>
          </div>
          <code>Locality/notion/Launch Plan/page.md</code>
        </div>
        <div className="demo-tile">
          <div>
            <strong>Needs review</strong>
            <span>Safe</span>
          </div>
          <p>Edited intro paragraph</p>
          <p>Updated launch checklist</p>
        </div>
        <div className="demo-tile">
          <div>
            <strong>Notion</strong>
            <span>Updated</span>
          </div>
          <p>Launch Plan reflects the approved Markdown edits.</p>
        </div>
      </div>
    </div>
  );
}

function AgentWorkspaceDemo() {
  return (
    <div className="agent-workspace-demo">
      <div className="demo-card-header">
        <span>Locality folder</span>
        <strong>Visible</strong>
      </div>
      <div className="agent-surface-demo">
        <div className="folder-pane-demo">
          <span className="active">Locality</span>
          <span>notion</span>
          <span>google-docs</span>
          <span>linear</span>
          <pre>{`notion/
  AGENTS.md
  CLAUDE.md
  Engineering/
    Roadmap/
      page.md
  Launch Plan/
    page.md`}</pre>
        </div>
        <div className="markdown-pane-demo">
          <div>
            <strong>Launch Plan/page.md</strong>
            <span>Edited</span>
          </div>
          <pre>{`# Launch Plan

Owner: Growth
Status: Ready

## Launch checklist
- Finalize onboarding
- Review pricing page
- Publish announcement

loc: notion-page`}</pre>
        </div>
      </div>
      <div className="review-strip">
        <strong>3 local edits ready to sync</strong>
        <span>Review before updating Notion</span>
      </div>
    </div>
  );
}

function ConnectorOptions({
  selected,
  connectedConnector,
  busy,
  onSelect,
}: {
  selected: OnboardingConnectorId;
  connectedConnector: OnboardingConnectorId | null;
  busy: boolean;
  onSelect: (connector: OnboardingConnectorId) => void;
}) {
  return (
    <div className="connector-options">
      <button
        type="button"
        className={`connector-option available selectable ${selected === "notion" ? "selected" : ""}`}
        disabled={busy}
        onClick={() => onSelect("notion")}
      >
        <ConnectorIcon connector="notion" />
        <div>
          <strong>Notion</strong>
          <small>Pages, databases, properties, and Markdown edits.</small>
        </div>
        <span>{connectedConnector === "notion" ? "Connected" : "OAuth"}</span>
      </button>
      <button
        type="button"
        className={`connector-option available selectable ${selected === "google-docs" ? "selected" : ""}`}
        disabled={busy}
        onClick={() => onSelect("google-docs")}
      >
        <ConnectorIcon connector="google-docs" />
        <div>
          <strong>Google Docs</strong>
          <small>Docs and Drive folders through the same local model.</small>
        </div>
        <span>{connectedConnector === "google-docs" ? "Connected" : "OAuth"}</span>
      </button>
      <button
        type="button"
        className={`connector-option available selectable ${selected === "google-calendar" ? "selected" : ""}`}
        disabled={busy}
        onClick={() => onSelect("google-calendar")}
      >
        <ConnectorIcon connector="google-calendar" />
        <div>
          <strong>Google Calendar</strong>
          <small>Primary calendar events and reviewed event drafts.</small>
        </div>
        <span>{connectedConnector === "google-calendar" ? "Connected" : "OAuth"}</span>
      </button>
      <button
        type="button"
        className={`connector-option available selectable ${selected === "gmail" ? "selected" : ""}`}
        disabled={busy}
        onClick={() => onSelect("gmail")}
      >
        <ConnectorIcon connector="gmail" />
        <div>
          <strong>Gmail</strong>
          <small>Inbox and sent as files, drafts as reviewed outbound mail.</small>
        </div>
        <span>{connectedConnector === "gmail" ? "Connected" : "OAuth"}</span>
      </button>
      <button
        type="button"
        className={`connector-option available selectable ${selected === "granola" ? "selected" : ""}`}
        disabled={busy}
        onClick={() => onSelect("granola")}
      >
        <ConnectorIcon connector="granola" />
        <div>
          <strong>Granola</strong>
          <small>Meeting summaries and transcripts as read-only files.</small>
        </div>
        <span>{connectedConnector === "granola" ? "Connected" : "API key"}</span>
      </button>
      <button
        type="button"
        className={`connector-option available selectable ${selected === "linear" ? "selected" : ""}`}
        disabled={busy}
        onClick={() => onSelect("linear")}
      >
        <ConnectorIcon connector="linear" />
        <div>
          <strong>Linear</strong>
          <small>Issues and teams as editable Markdown files.</small>
        </div>
        <span>{connectedConnector === "linear" ? "Connected" : "API key"}</span>
      </button>
      <button
        type="button"
        className={`connector-option available selectable ${selected === "slack" ? "selected" : ""}`}
        disabled={busy}
        onClick={() => onSelect("slack")}
      >
        <ConnectorIcon connector="slack" />
        <div>
          <strong>Slack</strong>
          <small>Channels, DMs, group DMs, and users as read-only files.</small>
        </div>
        <span>{connectedConnector === "slack" ? "Connected" : "OAuth"}</span>
      </button>
    </div>
  );
}

function WindowChrome({
  title,
  meta,
  metaTitle,
  onMetaClick,
}: {
  title: string;
  meta?: string;
  metaTitle?: string;
  onMetaClick?: () => void;
}) {
  const showWindowControls = isWindowsRuntime();

  return (
    <div
      className={`window-chrome ${showWindowControls ? "windows-chrome" : ""}`}
      onMouseDown={handleChromeMouseDown}
    >
      <div className="native-traffic-space" aria-hidden="true" />
      <div className="window-title" data-tauri-drag-region>{title}</div>
      <div
        className="window-chrome-actions"
        data-tauri-drag-region={(!onMetaClick && !showWindowControls) || undefined}
      >
        {onMetaClick ? (
          <button className="window-meta-button" title={metaTitle} onClick={onMetaClick}>
            {meta}
          </button>
        ) : (
          <span title={metaTitle}>{meta}</span>
        )}
        {showWindowControls && <WindowsWindowControls />}
      </div>
    </div>
  );
}

function WindowsWindowControls() {
  return (
    <div className="window-controls" aria-label="Window controls">
      <button
        className="window-control-button"
        type="button"
        aria-label="Minimize"
        title="Minimize"
        onClick={() => void getCurrentWindow().minimize()}
      >
        <Minus />
      </button>
      <button
        className="window-control-button"
        type="button"
        aria-label="Maximize or restore"
        title="Maximize or restore"
        onClick={() => void getCurrentWindow().toggleMaximize()}
      >
        <Square />
      </button>
      <button
        className="window-control-button close"
        type="button"
        aria-label="Close"
        title="Close"
        onClick={() => void getCurrentWindow().hide()}
      >
        <X />
      </button>
    </div>
  );
}

function handleChromeMouseDown(event: React.MouseEvent<HTMLDivElement>) {
  if (event.button !== 0 || !isTauriRuntime()) {
    return;
  }

  const target = event.target;
  if (target instanceof Element && target.closest("button")) {
    return;
  }

  event.preventDefault();
  void getCurrentWindow().startDragging();
}

function isWindowsRuntime() {
  return typeof navigator !== "undefined" && /^Win/i.test(navigator.platform);
}

function SetupContent({
  mark,
  children,
  variant,
  side,
}: {
  mark?: React.ReactNode;
  children: React.ReactNode;
  variant?: "final" | "hero" | "wide";
  side?: React.ReactNode;
}) {
  if (side) {
    return (
      <div className="setup-scrollport">
        <div className={`setup-content split-setup ${variant === "hero" ? "hero-setup" : ""}`}>
          <div className="setup-copy">
            {mark ? mark : null}
            {children}
          </div>
          <aside className="setup-side">{side}</aside>
        </div>
      </div>
    );
  }

  return (
    <div className="setup-scrollport">
      <div
        className={`setup-content ${variant === "final" ? "final-setup" : ""} ${
          variant === "wide" ? "wide-setup" : ""
        }`}
      >
        {mark ? mark : null}
        {children}
      </div>
    </div>
  );
}

function FinderEnableGuide({ waitingForRoot }: { waitingForRoot: boolean }) {
  if (waitingForRoot) {
    return (
      <div className="finder-enable-guide complete" role="status">
        <Check />
        <span>
          <strong>Finder access enabled</strong>
          <small>macOS is creating the Locality folder.</small>
        </span>
      </div>
    );
  }

  return (
    <div className="finder-enable-guide">
      <div className="finder-enable-illustration" aria-hidden="true">
        <div className="finder-enable-toolbar">
          <i />
          <i />
          <i />
          <span>Finder</span>
        </div>
        <div className="finder-enable-sidebar">
          <small>Locations</small>
          <span className="finder-enable-location">
            <FolderOpen />
            <strong>Locality</strong>
          </span>
        </div>
        <div className="finder-enable-content">
          <p>
            <strong>&quot;Locality&quot; is not enabled.</strong> To access Locality, click Enable.
          </p>
          <span className="finder-enable-control">Enable</span>
          <div className="finder-enable-placeholders">
            <i />
            <i />
            <i />
          </div>
        </div>
      </div>
    </div>
  );
}

function BrandTile({
  children,
  variant,
}: {
  children?: React.ReactNode;
  variant?: "notion" | "folder" | "progress" | "ready";
}) {
  return (
    <div className={`brand-tile ${variant ?? ""}`}>
      {variant === "folder" && <FolderOpen />}
      {variant === "progress" && <Loader2 />}
      {variant === "ready" && <Check />}
      {!variant && (children ? <span className="brand-word">{children}</span> : <LocalityLogo surface="light" />)}
      {variant === "notion" && children}
    </div>
  );
}

function ProgressList({ items }: { items: { label: string; state: "done" | "active" | "idle" }[] }) {
  return (
    <ol className="progress-list">
      {items.map((item) => (
        <li className={item.state} key={item.label}>
          <span>{item.state === "done" ? <Check /> : null}</span>
          {item.label}
        </li>
      ))}
    </ol>
  );
}

function SidebarButton({
  active,
  icon,
  children,
  onClick,
}: {
  active: boolean;
  icon: React.ReactNode;
  children: React.ReactNode;
  onClick: () => void;
}) {
  const label = typeof children === "string" ? children : undefined;
  return (
    <button className={`sidebar-link ${active ? "active" : ""}`} title={label} aria-label={label} onClick={onClick}>
      {icon}
      <span>{children}</span>
    </button>
  );
}

function PrimaryButton({
  children,
  icon,
  compact,
  busy,
  disabled,
  onClick,
}: {
  children: React.ReactNode;
  icon?: React.ReactNode;
  compact?: boolean;
  busy?: boolean;
  disabled?: boolean;
  onClick?: () => void;
}) {
  return (
    <button className={`primary-button ${compact ? "compact" : ""}`} disabled={disabled || busy} onClick={onClick} aria-busy={busy ? "true" : "false"}>
      {busy ? <Loader2 className="spin-icon" /> : icon}
      <span>{children}</span>
    </button>
  );
}

function SecondaryButton({
  children,
  icon,
  compact,
  busy,
  disabled,
  onClick,
}: {
  children: React.ReactNode;
  icon?: React.ReactNode;
  compact?: boolean;
  busy?: boolean;
  disabled?: boolean;
  onClick?: () => void;
}) {
  return (
    <button className={`secondary-button ${compact ? "compact" : ""}`} disabled={disabled || busy} onClick={onClick} aria-busy={busy ? "true" : "false"}>
      {busy ? <Loader2 className="spin-icon" /> : icon}
      <span>{children}</span>
    </button>
  );
}

function IconButton({
  label,
  icon,
  disabled,
  onClick,
}: {
  label: string;
  icon: React.ReactNode;
  disabled?: boolean;
  onClick?: () => void;
}) {
  return (
    <button className="icon-button has-tooltip" type="button" disabled={disabled} onClick={onClick} aria-label={label} title={label} data-tooltip={label}>
      {icon}
    </button>
  );
}

function TextButton({
  children,
  disabled,
  onClick,
}: {
  children: React.ReactNode;
  disabled?: boolean;
  onClick?: () => void;
}) {
  return (
    <button className="text-button" disabled={disabled} onClick={onClick}>
      {children}
    </button>
  );
}

function StatusPill({
  children,
  tone,
  title,
}: {
  children: React.ReactNode;
  tone: "ready" | "warn" | "danger";
  title?: string;
}) {
  return (
    <span
      className={`status-pill ${tone} ${title ? "has-tooltip" : ""}`}
      aria-label={title}
      data-tooltip={title}
      tabIndex={title ? 0 : undefined}
    >
      {children}
    </span>
  );
}

function LocalityLogo({
  surface,
  state = "default",
}: {
  surface: "light" | "dark";
  state?: "default" | "review" | "reconnect";
}) {
  const logoUrl = surface === "dark" ? localityShortLightUrl : localityShortDarkUrl;

  return (
    <span className={`locality-logo ${state}`} aria-hidden="true">
      <img src={logoUrl} alt="" draggable="false" />
      {state !== "default" && <i />}
    </span>
  );
}

function PanelTitle({ title }: { title: string }) {
  return <h3 className="panel-title">{title}</h3>;
}

function SettingRow({ title, value, href }: { title: string; value: string; href?: string }) {
  return (
    <div className="setting-row">
      <span>{title}</span>
      <strong>
        {href ? (
          <a className="setting-link" href={href} target="_blank" rel="noreferrer">
            {value}
          </a>
        ) : (
          value
        )}
      </strong>
    </div>
  );
}

function ToggleRow({
  title,
  enabled,
  busy,
  onToggle,
}: {
  title: string;
  enabled: boolean;
  busy?: boolean;
  onToggle?: (enabled: boolean) => void;
}) {
  return (
    <div className="setting-row">
      <span>{title}</span>
      <button
        className={`toggle ${enabled ? "enabled" : ""}`}
        aria-label={title}
        aria-pressed={enabled}
        disabled={busy}
        onClick={() => onToggle?.(!enabled)}
      >
        <i />
      </button>
    </div>
  );
}

function ThemeRow({
  value,
  onChange,
}: {
  value: AppTheme;
  onChange: (theme: AppTheme) => void;
}) {
  const options: Array<{ value: AppTheme; label: string; icon: React.ReactNode }> = [
    { value: "system", label: "System", icon: <Monitor /> },
    { value: "light", label: "Light", icon: <Sun /> },
    { value: "dark", label: "Dark", icon: <Moon /> },
  ];

  return (
    <div className="setting-row theme-setting-row">
      <span>Appearance</span>
      <div className="theme-segmented" role="radiogroup" aria-label="Appearance">
        {options.map((option) => (
          <button
            key={option.value}
            type="button"
            className={value === option.value ? "active" : ""}
            role="radio"
            aria-checked={value === option.value}
            onClick={() => onChange(option.value)}
          >
            {option.icon}
            <span>{option.label}</span>
          </button>
        ))}
      </div>
    </div>
  );
}

function PathRow({ path }: { path: string }) {
  return (
    <div className="path-row">
      <code>{path}</code>
      <SecondaryButton compact>Move</SecondaryButton>
    </div>
  );
}

function Metric({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <article className="metric">
      <strong>{value}</strong>
      <span>{label}</span>
    </article>
  );
}

function mountMissing(snapshot: DesktopSnapshot) {
  return snapshot.mount.status === "not_mounted";
}

function normalizeAppView(value: string): AppView | null {
  if (value === "mount") {
    return "files";
  }
  return isAppView(value) ? value : null;
}

function isAppView(value: string): value is AppView {
  return value === "home" || value === "files" || value === "pending" || value === "review" || value === "activity" || value === "settings";
}

function chromeStatusLabel(snapshot: DesktopSnapshot) {
  if (snapshot.health.state === "ready") {
    return "Ready";
  }
  if (snapshot.health.state === "needs_review") {
    return PRODUCT_TERMS.reviewCenter;
  }
  return healthLabel(snapshot.health.state);
}

function sidebarStatusLabel(snapshot: DesktopSnapshot) {
  if (snapshot.health.state === "ready") {
    const mountedSources = snapshot.mounts.filter((mount) => mount.status !== "not_mounted");
    if (mountedSources.length > 1) {
      return "Sources Ready";
    }
    if (mountedSources.length === 1) {
      return `${sourceDisplayNameFromConnector(mountedSources[0].connector)} Ready`;
    }
    const readyToMountSources = connectedSourcesReadyToMount(snapshot);
    if (readyToMountSources.length > 1) {
      return "Sources Connected";
    }
    if (readyToMountSources.length === 1) {
      return `${sourceDisplayName(readyToMountSources[0])} Connected`;
    }
    return "Ready";
  }
  if (snapshot.health.state === "needs_review") {
    return PRODUCT_TERMS.reviewNeeded;
  }
  return healthLabel(snapshot.health.state);
}

function sourceDisplayNameFromConnector(connector: string) {
  return isSourceConnectorId(connector)
    ? sourceDisplayName(connector)
    : connector
        .split(/[-_\s]+/)
        .filter(Boolean)
        .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
        .join(" ") || "Source";
}

function chromeStatusTarget(snapshot: DesktopSnapshot): AppView | null {
  if (snapshot.health.state === "needs_review") {
    return "pending";
  }
  if (
    snapshot.health.state === "stopped" ||
    snapshot.health.state === "runtime_stopped" ||
    snapshot.health.state === "reconnect_needed"
  ) {
    return "settings";
  }
  return null;
}

function healthLabel(state: string) {
  if (state === "needs_review") {
    return "Needs Review";
  }
  if (state === "reconnect_needed") {
    return "Reconnect Needed";
  }
  if (state === "stopped") {
    return "Stopped";
  }
  if (state === "runtime_stopped") {
    return "Runtime Needs Repair";
  }
  if (state === "checking_freshness") {
    return "Checking";
  }
  return "Ready";
}

function healthDescription(state: string, attentionCount: number) {
  if (state === "needs_review") {
    return `${attentionCount} local change${attentionCount === 1 ? "" : "s"} waiting for review or push.`;
  }
  if (state === "reconnect_needed") {
    return "Notion needs to be reconnected before Locality can sync this workspace.";
  }
  if (state === "stopped") {
    return "The Locality daemon is stopped. Background sync, hydration, and Live Mode are paused; direct actions can still run from the app.";
  }
  if (state === "runtime_stopped") {
    return "The filesystem provider is stopped or unregistered. Use Repair Locality in Settings to restore online-only file access.";
  }
  if (state === "checking_freshness") {
    return "Locality is checking the local mount and Notion freshness state.";
  }
  return "Notion is connected, the local workspace is ready, and remote writes remain explicit.";
}

function healthTone(state: string): "ready" | "warn" | "danger" {
  if (state === "reconnect_needed" || state === "stopped" || state === "runtime_stopped") {
    return "danger";
  }
  if (state === "needs_review") {
    return "warn";
  }
  return "ready";
}

function healthIconState(state: string): "default" | "review" | "reconnect" {
  if (state === "reconnect_needed" || state === "stopped" || state === "runtime_stopped") {
    return "reconnect";
  }
  if (state === "needs_review") {
    return "review";
  }
  return "default";
}

function locatedStateLabel(state: LocatedItem["state"]) {
  if (state === "online_only") {
    return "Online Only";
  }
  if (state === "pending_changes") {
    return PRODUCT_TERMS.reviewNeeded;
  }
  if (state === "conflict") {
    return "Conflict";
  }
  if (state === "remote_update_available") {
    return "Remote Update";
  }
  if (state === "preparing") {
    return "Preparing";
  }
  if (state === "no_access") {
    return "No Access";
  }
  if (state === "not_found") {
    return "Not Found";
  }
  return "Ready";
}

function providerStatusLabel(provider: ProviderRuntimeSummary) {
  const state = provider.state === "running" ? "Running" : provider.state === "stopped" ? "Stopped" : "Error";
  const parts = [state];
  if (provider.pid) {
    parts.push(`pid ${provider.pid}`);
  }
  if (provider.registered === false) {
    parts.push("not registered");
  }
  if (provider.stalePidFile) {
    parts.push("stale pid");
  }
  return parts.join(" - ");
}

function joinMountPath(mountPath: string, relativePath: string) {
  if (relativePath.startsWith("/") || relativePath.startsWith("~/")) {
    return relativePath;
  }

  return `${mountPath.replace(/\/$/, "")}/${relativePath}`;
}

function hasConflictMarkers(contents: string) {
  return /^\s*<<<<<<<.*$/m.test(contents) && /^\s*=======\s*$/m.test(contents) && /^\s*>>>>>>>.*$/m.test(contents);
}

function pushNeedsReview(message: string) {
  return message.includes("Open Review Push") || message.includes("needs review");
}

function copyText(value: string) {
  void navigator.clipboard?.writeText(value);
}
