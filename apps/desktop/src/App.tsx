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
  History,
  Home,
  ListChecks,
  Loader2,
  Minus,
  Plus,
  Power,
  RefreshCw,
  RotateCcw,
  Search,
  Settings,
  ShieldCheck,
  Sparkles,
  Square,
  Trash2,
  Zap,
  X,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import {
  compactPath,
  mountEntityCountLabel,
  mountAccessLabel,
  mountRows,
  mountStatusLabel,
  mountStatusTone,
  selectedMountIdAfterOpenViewEvent,
  selectedMountIdAfterViewChange,
  selectedMountRow,
  type MountRow,
  type MountSummary,
  type ProviderRuntimeSummary,
} from "./mounts";
import { connectionMissing, connectionReady } from "./connection-state";
import { copyLoginLinkDisabled, loginLinkFlowMode } from "./onboarding-connect";
import { mountRecoveryEnabled, shouldAutoCreateMount } from "./onboarding-flow";
import { classifyMountSetupError } from "./onboarding-errors";
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

const distributionChannel = (import.meta.env.VITE_LOCALITY_DISTRIBUTION_CHANNEL || "direct").toLowerCase();
const appStoreDistribution = distributionChannel === "mas";

type AppView = "home" | "files" | "mount" | "pending" | "review" | "activity" | "settings";
type LocateState = "idle" | "preparing" | "ready" | "error";
type OnboardingStep = 1 | 2 | 3 | 4 | 5;

type DesktopSnapshot = {
  health: {
    state: string;
    attentionCount: number;
  };
  connection: {
    connector: string;
    workspaceName: string;
    accountLabel: string;
    status: string;
  };
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

type UpdateStatus = {
  state: "idle" | "checking" | "available" | "installing" | "current" | "error";
  message: string;
  update: Update | null;
  version?: string;
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
    status: "ready",
  },
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
      state: "planned",
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

function suggestedAgentPrompt(mountPath: string) {
  return `Use Locality to edit my Notion workspace. Open the files under ${mountPath}, make the requested edits directly in Markdown, and leave changes pending for Locality review.`;
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
    : "Turn on Live Mode to keep this Notion mount point in sync while you work. Locality still pauses for conflicts, large changes, or anything that needs review.";
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

function emptyUpdateStatus(): UpdateStatus {
  return { state: "idle", message: "", update: null };
}

function updaterErrorMessage(error: unknown) {
  const message = errorMessage(error);
  const lower = message.toLowerCase();
  if (lower.includes("updater") && (lower.includes("config") || lower.includes("endpoint"))) {
    return "Updates are not configured for this build.";
  }
  return message;
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

export default function App() {
  const initialRoute = window.location.hash;
  const [snapshot, setSnapshot] = useState<DesktopSnapshot>(() =>
    isTauriRuntime() ? loadingSnapshot : sampleSnapshot,
  );
  const [snapshotLoaded, setSnapshotLoaded] = useState(() => !isTauriRuntime());
  const [view, setView] = useState<AppView>("home");
  const [route, setRoute] = useState(initialRoute);
  const [showOnboarding, setShowOnboarding] = useState(() =>
    routeForcesOnboarding(initialRoute) || previewRouteStartsOnboarding(initialRoute),
  );
  const [onboardingKey, setOnboardingKey] = useState(0);
  const [onboardingInitialStep, setOnboardingInitialStep] = useState<OnboardingStep>(() =>
    initialRoute === "#onboarding-ready" ? 5 : 1,
  );
  const [updateStatus, setUpdateStatus] = useState<UpdateStatus>(emptyUpdateStatus);
  const refreshSnapshotPromise = useRef<Promise<void> | null>(null);
  const refreshSnapshotQueued = useRef(false);

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

  async function checkForAppUpdate(options: { silent?: boolean } = {}) {
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

      setUpdateStatus({
        state: "available",
        message: `Locality ${update.version} is ready to install.`,
        update,
        version: update.version,
      });
    } catch (error) {
      if (!options.silent) {
        setUpdateStatus({ state: "error", message: updaterErrorMessage(error), update: null });
      }
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

    setUpdateStatus((current) => ({
      ...current,
      state: "installing",
      message: current.version ? `Installing Locality ${current.version}.` : "Installing update.",
    }));

    try {
      const update = updateStatus.update ?? (await check());
      if (!update) {
        setUpdateStatus({ state: "current", message: "Locality is up to date.", update: null });
        return;
      }
      await update.downloadAndInstall();
      setUpdateStatus({
        state: "installing",
        message: "Restarting Locality to finish the update.",
        update: null,
        version: update.version,
      });
      await relaunch();
    } catch (error) {
      setUpdateStatus({ state: "error", message: updaterErrorMessage(error), update: null });
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
      void checkForAppUpdate({ silent: true });
    }, 5000);

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
      onRefresh={refreshSnapshot}
      updateStatus={updateStatus}
      onCheckForUpdate={checkForAppUpdate}
      onInstallUpdate={installAppUpdate}
      onDismissUpdate={() => setUpdateStatus(emptyUpdateStatus())}
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
            <p>Locality is checking your Notion connection and mount point.</p>
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
  const [connectedWorkspace, setConnectedWorkspace] = useState(snapshot.connection.workspaceName);
  const [mountPath, setMountPath] = useState(snapshot.mount.localPath);
  const [mountPathDirty, setMountPathDirty] = useState(false);
  const [locateUrl, setLocateUrl] = useState("");
  const [locatedItem, setLocatedItem] = useState<LocatedItem | null>(null);
  const [locateState, setLocateState] = useState<LocateState>("idle");
  const [locateError, setLocateError] = useState("");
  const [mountOnboarding, setMountOnboarding] = useState<WorkspaceMountOnboardingReport | null>(null);
  const [mounting, setMounting] = useState(false);
  const [agentGuidanceReport, setAgentGuidanceReport] = useState<AgentGuidanceInstallReport | null>(null);
  const [agentGuidanceState, setAgentGuidanceState] = useState<"idle" | "installing" | "ready" | "error">("idle");
  const mountStartRequestedRef = useRef(false);
  const connectionReadyNow = oauthReady || connectionReady(snapshot);

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
    if (!mountPathDirty) {
      setMountPath(snapshot.mount.localPath);
    }
  }, [mountPathDirty, snapshot.mount.localPath]);

  useEffect(() => {
    if (step !== 3 || !oauthInFlight || oauthReady) {
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
  }, [oauthInFlight, oauthReady, step]);

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

    setOauthReady(true);
    setStep((current) => {
      if (mountMissing(snapshot)) {
        return current < 4 ? 4 : current;
      }
      return current < 5 ? 5 : current;
    });
  }, [snapshot.connection.status, snapshot.mount.status, snapshotLoaded]);

  useEffect(() => {
    if (
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
  }, [connectionReadyNow, mountOnboarding, mountPath, mounting, snapshot.mount.status, step]);

  useEffect(() => {
    if (step !== 5 || mountMissing(snapshot) || agentGuidanceState !== "idle") {
      return;
    }
    void installAgentGuidance(mountPath);
  }, [agentGuidanceState, mountPath, snapshot.mount.status, step]);

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
  const finalPrompt = agentGuidanceReport?.prompt || suggestedAgentPrompt(mountPath);
  const mountSetupError =
    mountOnboarding?.state === "failed"
      ? classifyMountSetupError(mountOnboarding.message)
      : null;
  const showRecoveryChooser = mountRecoveryEnabled(mountSetupError);

  return (
    <main className="setup-shell">
      <section className="setup-window">
        <WindowChrome title="Locality Setup" meta={`${step} of 5`} />
        {step === 1 && (
          <SetupContent side={<ProductLoopDemo />}>
            <div>
              <div className="eyebrow">Meet Locality</div>
              <h1>Your work apps, as local files for agents.</h1>
              <p>
                Locality gives agents like Claude and Codex a safe local folder for tools like
                Notion. Agents and humans can edit Markdown side by side, and you review what
                changes before it updates the content in the connected app.
              </p>
            </div>
            <PrimaryButton onClick={() => setStep(2)}>Set Up Locality</PrimaryButton>
            <div className="onboarding-pill-row">
              <span>Works in Finder</span>
              <span>Agents edit Markdown</span>
              <span>Review before sync</span>
            </div>
          </SetupContent>
        )}

        {step === 2 && (
          <SetupContent side={<AgentWorkspaceDemo />}>
            <div>
              <div className="eyebrow">How agents use it</div>
              <h1>Agents work in files you can see.</h1>
              <p>
                Each app appears as a folder. Pages and docs become page.md files that stay in sync
                with the connected app. Locality adds AGENTS.md and CLAUDE.md so agents know how to
                work in the folder safely.
              </p>
            </div>
            <PrimaryButton onClick={() => setStep(3)}>Continue</PrimaryButton>
          </SetupContent>
        )}

        {step === 3 && (
          <SetupContent
            side={<ConnectorOptions connected={connectionReadyNow} />}
          >
            <div>
              <div className="eyebrow">Connect app</div>
              {(oauthInFlight || connectionReadyNow) && (
                <div className={`sync-note ${connectionReadyNow ? "connected" : ""}`}>
                  {connectionReadyNow ? <Check /> : <Loader2 className="spin-icon" />}
                  {connectionReadyNow ? "Notion connected" : "Waiting for Notion"}
                </div>
              )}
              <h1>
                {connectionReadyNow
                  ? "Your Notion workspace is connected"
                  : oauthInFlight
                    ? "Finish connecting in Notion."
                    : "Start with Notion."}
              </h1>
              <p>
                {connectionReadyNow
                  ? `${workspaceLabel} is ready. Locality will now create the Notion folder under CloudStorage and prepare agent guidance.`
                  : oauthInFlight
                    ? "A browser window is open. Choose the workspace and pages Locality can access, then approve."
                    : "Connect the workspace you want agents to help with. Your machine talks directly to Notion, and app credentials are protected by macOS Keychain."}
              </p>
            </div>
            {oauthInFlight && !connectionReadyNow && (
              <ProgressList
                items={[
                  { label: "Browser opened", state: oauthError ? "idle" : "done" },
                  { label: "Select workspace and pages", state: "active" },
                  { label: "Approve access", state: "idle" },
                ]}
              />
            )}
            <div className="button-row">
              <PrimaryButton
                busy={oauthInFlight && !connectionReadyNow}
                onClick={connectionReadyNow ? () => setStep(4) : startConnect}
              >
                {connectionReadyNow ? "Continue" : oauthInFlight ? "Waiting for Notion" : "Connect Notion"}
              </PrimaryButton>
              <SecondaryButton
                disabled={copyLoginLinkDisabled({
                  connectionReady: connectionReadyNow,
                  oauthInFlight,
                })}
                onClick={() => void copyLoginLink()}
              >
                Copy login link
              </SecondaryButton>
            </div>
            <div className="onboarding-pill-row">
              <span>Scoped access</span>
              <span>Credentials in Keychain</span>
              <span>Direct app connection</span>
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
              <h1>{mountOnboardingHeadline(mountOnboarding)}</h1>
              <p>
                {mountOnboarding?.message ??
                  "Locality is creating your Notion folder under the default CloudStorage root and preparing agent guidance."}
              </p>
            </div>
            <div className="sync-note">
              {mounting ? (
                <Loader2 className="spin-icon" />
              ) : mountOnboarding?.state === "failed" ? (
                <AlertTriangle />
              ) : (
                <FolderOpen />
              )}
              {mounting
                ? "Checking File Provider approval"
                : mountOnboarding?.message ?? "Creating folder and preparing Notion files"}
            </div>
            <div className="path-field ready-path-field">
              <span>{mountPath}</span>
            </div>
            {showRecoveryChooser ? (
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
            {mountOnboardingNeedsInstructions(mountOnboarding) && (
              <p className="quiet-note">{mountOnboardingInstructions(mountOnboarding)}</p>
            )}
            {mountOnboardingSupplementaryNote(mountOnboarding) && (
              <p className="quiet-note">{mountOnboardingSupplementaryNote(mountOnboarding)}</p>
            )}
            <p className="quiet-note">
              Locality uses the default CloudStorage location so Finder and your agents see the
              same Notion folder automatically.
            </p>
          </SetupContent>
        )}

        {step === 5 && (
          <SetupContent mark={<BrandTile variant="ready" />} variant="final">
            <div>
              <h1>Locality is ready!</h1>
              <p>
                Your Notion mount point is ready. Agents can open this folder, edit
                Markdown, and leave changes for Locality review. Open the app to review changes,
                manage sync, and turn on Live Mode when you want file saves to update Notion and
                new Notion changes to appear locally.
              </p>
            </div>
            {mountOnboarding && <p className="field-error">{mountOnboarding.message}</p>}
            <div className="final-actions">
              <PrimaryButton onClick={finishOnboarding}>
                Open Locality
              </PrimaryButton>
            </div>
            <div className="folder-inline final-folder-card">
              <div className="ready-head">
                <div>
                  <strong>Folder</strong>
                  <p>Your Notion files are mounted here.</p>
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
                  <p>Claude, Codex are now setup to use Locality.</p>
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
        <p>Locality is preparing local agent instructions for this Notion mount point.</p>
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
  onRefresh,
  updateStatus,
  onCheckForUpdate,
  onInstallUpdate,
  onDismissUpdate,
  appStoreDistribution,
  onResetComplete,
}: {
  snapshot: DesktopSnapshot;
  view: AppView;
  onViewChange: (view: AppView) => void;
  onRefresh: () => Promise<void>;
  updateStatus: UpdateStatus;
  onCheckForUpdate: (options?: { silent?: boolean }) => Promise<void>;
  onInstallUpdate: () => Promise<void>;
  onDismissUpdate: () => void;
  appStoreDistribution: boolean;
  onResetComplete: () => void;
}) {
  const meta = chromeStatusLabel(snapshot);
  const statusTitle = healthDescription(snapshot.health.state, snapshot.health.attentionCount);
  const statusTarget = chromeStatusTarget(snapshot);
  const [selectedMountId, setSelectedMountId] = useState<string | null>(null);
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

  function openStatusTarget() {
    if (statusTarget) {
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
        onMetaClick={statusTarget ? () => onViewChange(statusTarget) : undefined}
      />
      <div className="app-shell">
        <aside className="sidebar">
          <div className="sidebar-brand">
            <ApertureIcon />
            <strong>Locality</strong>
          </div>
          <nav>
            <SidebarButton active={view === "home"} icon={<Home />} onClick={() => onViewChange("home")}>
              Home
            </SidebarButton>
            <SidebarButton active={view === "files"} icon={<Search />} onClick={() => onViewChange("files")}>
              Files
            </SidebarButton>
            <SidebarButton active={view === "mount"} icon={<FolderOpen />} onClick={openMountsView}>
              Mounts
            </SidebarButton>
            <SidebarButton
              active={view === "pending" || view === "review"}
              icon={<ListChecks />}
              onClick={() => onViewChange("pending")}
            >
              Pending
            </SidebarButton>
            <SidebarButton
              active={view === "activity"}
              icon={<History />}
              onClick={() => onViewChange("activity")}
            >
              Activity
            </SidebarButton>
            <SidebarButton
              active={view === "settings"}
              icon={<Settings />}
              onClick={() => onViewChange("settings")}
            >
              Settings
            </SidebarButton>
          </nav>
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
          <UpdateBanner
            status={updateStatus}
            onInstall={onInstallUpdate}
            onDismiss={onDismissUpdate}
            onSettings={() => onViewChange("settings")}
          />
          {view === "home" && (
            <HomeView
              snapshot={snapshot}
              onMount={openMountsView}
              onFiles={() => onViewChange("files")}
              onReview={() => onViewChange("pending")}
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
              onReview={() => onViewChange("pending")}
            />
          )}
          {view === "files" && (
            <FilesView
              snapshot={snapshot}
              onHome={() => onViewChange("home")}
              onRefresh={onRefresh}
              onReview={() => onViewChange("pending")}
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
            />
          )}
          {view === "review" && (
            <ReviewView
              snapshot={snapshot}
              onHome={() => onViewChange("home")}
              onPending={() => onViewChange("pending")}
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
              onResetComplete={onResetComplete}
            />
          )}
        </section>
      </div>
    </main>
  );
}

function UpdateBanner({
  status,
  onInstall,
  onDismiss,
  onSettings,
}: {
  status: UpdateStatus;
  onInstall: () => Promise<void>;
  onDismiss: () => void;
  onSettings: () => void;
}) {
  if (status.state !== "available" && status.state !== "installing") {
    return null;
  }

  const installing = status.state === "installing";
  return (
    <div className="update-banner">
      <div>
        <strong>{status.version ? `Locality ${status.version} available` : "Locality update available"}</strong>
        <p>{status.message}</p>
      </div>
      <div className="update-banner-actions">
        <SecondaryButton compact onClick={onSettings}>
          Settings
        </SecondaryButton>
        <PrimaryButton
          compact
          icon={installing ? <Loader2 className="spin-icon" /> : <Download />}
          disabled={installing}
          onClick={() => void onInstall()}
        >
          {installing ? "Installing" : "Install"}
        </PrimaryButton>
        <button className="update-dismiss" aria-label="Dismiss update notice" onClick={onDismiss}>
          Dismiss
        </button>
      </div>
    </div>
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
  onReview: () => void;
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
      <ViewHeader eyebrow="Home" title="Notion workspace">
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
          <section className="workspace-card">
            <div className="workspace-summary">
              <p className="label">Connected workspace</p>
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
                View Mounts
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
            <p className="label">Pending Changes</p>
            <h2>{snapshot.pendingChanges.length} files have pending changes.</h2>
          </div>
          <PrimaryButton icon={<ListChecks />} onClick={onReview}>
            Review Pending Changes
          </PrimaryButton>
        </section>
      ) : (
        <section className="panel muted-panel">
          <Check />
          <div>
            <h2>No pending changes</h2>
            <p>Local edits will appear here before they update Notion.</p>
          </div>
        </section>
      )}

      <section className="suggestion-card">
        <Sparkles />
        <div>
          <p className="label">Suggestion</p>
          <h3>Connect {snapshot.suggestions[0]?.connector ?? "Linear"}</h3>
          <p>{snapshot.suggestions[0]?.description ?? "Connect more workspaces as local files."}</p>
        </div>
        <SecondaryButton compact disabled>
          Coming Soon
        </SecondaryButton>
      </section>
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
  const [creating, setCreating] = useState(false);
  const [refreshing, setRefreshing] = useState(false);

  async function createMount() {
    if (creating) {
      return;
    }
    setActionError("");
    setCreating(true);
    try {
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
    } catch (error) {
      setActionError(errorMessage(error));
    } finally {
      setCreating(false);
    }
  }

  async function refreshMounts() {
    if (refreshing) {
      return;
    }
    setActionError("");
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
    const report = await callCommand<ActionReport>(
      "open_path",
      { path },
      { ok: true, message: "Opened demo folder." },
    );
    if (!report.ok) {
      setActionError(report.message);
    }
  }

  return (
    <div className="view-stack mounts-view">
      <Breadcrumbs items={[{ label: "Home", onClick: onHome }, { label: "Mounts" }]} />
      <ViewHeader title="Mounted folders">
        <SecondaryButton
          compact
          busy={creating}
          disabled={connectionMissing(snapshot)}
          icon={<Plus />}
          onClick={() => void createMount()}
        >
          Add Mount
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

      {rows.length === 0 ? (
        <section className="empty-action-panel">
          <BrandTile variant="folder" />
          <div>
            <h2>Add a Notion mounted folder</h2>
            <p>Use the default Notion folder under the shared Locality location.</p>
          </div>
          <PrimaryButton
            busy={creating}
            disabled={connectionMissing(snapshot)}
            icon={<FolderOpen />}
            onClick={() => void createMount()}
          >
            Add Notion Mount
          </PrimaryButton>
        </section>
      ) : (
        <>
          <p className="view-copy">
            {rows.length} mounted {rows.length === 1 ? "folder" : "folders"} registered for this Locality state.
          </p>
          <section className="mounts-grid" aria-label="Registered mounted folders">
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
                  <span>{row.projection}</span>
                  <span>{row.access}</span>
                  <span>{row.content}</span>
                </div>
                <div className="mount-card-footer">
                  <span>{row.mount.mountId}</span>
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
    </div>
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
  const { results, searching } = useNotionSearchResults(query, !mountMissing(snapshot));

  return (
    <div className="view-stack">
      <ViewHeader title="Files">
        <Breadcrumbs
          items={[
            { label: "Home", onClick: onHome },
            { label: "Files" },
          ]}
        />
      </ViewHeader>

      {!mountMissing(snapshot) && (
        <CurrentWorkspacePanel snapshot={snapshot} onRefresh={onRefresh} onReview={onReview} />
      )}

      <section className="panel discovery-panel">
        <div>
          <p className="label">Current access</p>
          <h2>Search current files</h2>
          <p>Results come from active workspaces and active connections only.</p>
        </div>
        <div className="locate-row">
          <Search />
          <input
            value={query}
            placeholder="Search current Notion files"
            onChange={(event) => setQuery(event.target.value)}
          />
        </div>
        {query.trim().length >= 2 && (
          <div className="file-discovery-list" aria-busy={searching ? "true" : "false"}>
            {results.length ? (
              results.map((item) => <FileDiscoveryRow key={`${item.kind}-${item.localPath}`} item={item} />)
            ) : (
              <EmptyDiscoveryState text={searching ? "Searching current files..." : "No current files matched."} />
            )}
          </div>
        )}
      </section>

      <RecentFilesPanel items={snapshot.recentFiles} />
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
          <p className="label">Current workspace</p>
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
            Review Pending
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
        <span>Indexed: {mountEntityCountLabel(snapshot.mount)}</span>
        {showAccount && <span>Account: {accountLabel}</span>}
      </div>

      <details className="workspace-diagnostics">
        <summary>Diagnostics</summary>
        <div className="workspace-facts">
          <span>Connection: {snapshot.connection.status}</span>
          <span>Status: {snapshot.mount.status}</span>
          <span>Connector: {snapshot.mount.connector}</span>
          <span>Pending: {snapshot.pendingChanges.length}</span>
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
  const accountLabel = isActiveMount ? snapshot.connection.accountLabel.trim() : "";
  const showAccount = accountLabel.length > 0 && accountLabel !== mount.workspaceName;
  const providerState = mount.provider?.state ?? "Not registered";
  const providerMessage = mount.provider?.message ?? providerState;

  async function openFolder() {
    setActionError("");
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

  return (
    <div className="view-stack">
      <Breadcrumbs
        items={[
          { label: "Home", onClick: onHome },
          { label: "Mounts", onClick: onMounts },
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
          <p className="label">{mount.connectorName} mounted folder</p>
          <h2 title={mount.localPath}>{compactPath(mount.localPath, 78)}</h2>
          <p>
            Locality exposes this connected workspace as local files at the registered mount point.
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
              {pullState === "pulling" ? "Pulling changes" : "Pull changes"}
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
          <SettingRow title="Root exists" value={mount.rootExists ? "Yes" : "No"} />
        </div>
      </section>

      <section className="safety-strip">
        <ShieldCheck />
        <div>
          <h2>Edits stay pending until reviewed</h2>
          <p>
            Local changes are staged first. This mount currently has {mount.pendingChangeCount} pending
            {mount.pendingChangeCount === 1 ? " change" : " changes"}.
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
}: {
  snapshot: DesktopSnapshot;
  onHome: () => void;
  onReview: () => void;
  onRefresh: () => Promise<void>;
}) {
  const hasPendingChanges = snapshot.pendingChanges.length > 0;
  const [pushState, setPushState] = useState<"idle" | "pushing" | "success" | "error">("idle");
  const [pushMessage, setPushMessage] = useState("");

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
      <Breadcrumbs items={[{ label: "Home", onClick: onHome }, { label: "Pending" }]} />
      <ViewHeader title="Pending Changes">
        <div className="button-row">
          <SecondaryButton
            disabled={!hasPendingChanges || isPushing}
            icon={isPushing ? <Loader2 className="spin-icon" /> : <ShieldCheck />}
            onClick={() => void pushAll()}
          >
            {isPushing ? "Pushing..." : "Push All"}
          </SecondaryButton>
          <PrimaryButton disabled={!hasPendingChanges || isPushing} icon={<ListChecks />} onClick={onReview}>
            Review Push
          </PrimaryButton>
        </div>
      </ViewHeader>
      {pushMessage && (
        <p className={pushState === "error" ? "field-error" : "success-note inline-note"}>
          {pushMessage}
        </p>
      )}
      {hasPendingChanges ? (
        <>
          <p className="view-copy">{snapshot.pendingChanges.length} files have pending changes.</p>
          <FileChangeList
            changes={snapshot.pendingChanges}
            mountPath={snapshot.mount.localPath}
            onReview={onReview}
            onRefresh={onRefresh}
          />
        </>
      ) : (
        <section className="panel muted-panel">
          <Check />
          <div>
            <h2>No pending changes</h2>
            <p>Local edits will appear here before they update Notion.</p>
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

  return (
    <div className="view-stack">
      <Breadcrumbs items={[{ label: "Home", onClick: onHome }, { label: "Pending", onClick: onPending }, { label: "Review" }]} />
      <ViewHeader title={plan.title}>
        <StatusPill
          tone={pushState === "error" ? "danger" : isPushing ? "warn" : "ready"}
          title={isPushing ? "Locality is writing the approved local changes to Notion." : "This push is ready for review."}
        >
          {pushState === "error" ? "Needs Attention" : isPushing ? "Pushing" : pushSucceeded ? "Pushed" : "Safe"}
        </StatusPill>
      </ViewHeader>
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

      <FileChangeList
        changes={plan.files}
        mountPath={snapshot.mount.localPath}
        confirmDangerous
        onRefresh={onRefresh}
      />

      <div className="footer-actions">
        <PrimaryButton
          disabled={!plan.canPush || isPushing || pushSucceeded}
          icon={isPushing ? <Loader2 className="spin-icon" /> : pushSucceeded ? <Check /> : <ShieldCheck />}
          onClick={push}
        >
          {isPushing ? "Pushing..." : pushSucceeded ? "Pushed" : "Push to Notion"}
        </PrimaryButton>
        <SecondaryButton disabled={isPushing || pushSucceeded}>Cancel</SecondaryButton>
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
      <Breadcrumbs items={[{ label: "Home", onClick: onHome }, { label: "Activity" }]} />
      <ViewHeader title="Activity">
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
  onResetComplete,
}: {
  snapshot: DesktopSnapshot;
  onHome: () => void;
  onRefresh: () => Promise<void>;
  updateStatus: UpdateStatus;
  onCheckForUpdate: (options?: { silent?: boolean }) => Promise<void>;
  onInstallUpdate: () => Promise<void>;
  appStoreDistribution: boolean;
  onResetComplete: () => void;
}) {
  const [diagnosticMessage, setDiagnosticMessage] = useState("");
  const [settingsMessage, setSettingsMessage] = useState("");
  const [resetMessage, setResetMessage] = useState("");
  const [agentMessage, setAgentMessage] = useState("");
  const [installingAgents, setInstallingAgents] = useState(false);
  const [resettingState, setResettingState] = useState(false);
  const [preparingUninstall, setPreparingUninstall] = useState(false);
  const [busySetting, setBusySetting] = useState("");
  const [localSettings, setLocalSettings] = useState(snapshot.settings);
  const daemonStopped = snapshot.health.state === "stopped";
  const runtimeStopped = snapshot.health.state === "runtime_stopped";
  const runtimeNeedsRepair = daemonStopped || runtimeStopped;
  const checkingForUpdate = updateStatus.state === "checking";
  const installingUpdate = updateStatus.state === "installing";
  const updateAvailable = updateStatus.state === "available" || updateStatus.state === "installing";
  const updateChannelLabel = appStoreDistribution ? "Mac App Store" : "GitHub Releases";
  const updateStatusLabel = appStoreDistribution
    ? "Managed by the App Store"
    : updateStatus.message || "Ready";

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
      `Locality process: ${daemonStopped ? "Stopped" : "Running"}`,
      snapshot.mount.provider ? `Provider: ${providerStatusLabel(snapshot.mount.provider)}` : null,
      "State folder: ~/.loc",
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
    const confirmed = window.confirm(
      "Reset local Locality state? This clears Locality metadata, cache, mount registration, and connector credentials. It does not delete your local files.",
    );
    if (!confirmed) {
      return;
    }

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
        window.alert(report.message);
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
    const confirmed = window.confirm(
      "Prepare Locality for uninstall? This stops Locality, removes Locality agent integrations and MCP config, clears Locality local state, and leaves your local files in place.",
    );
    if (!confirmed) {
      return;
    }

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
        await onRefresh().catch(() => undefined);
        onResetComplete();
      }
    } catch (error) {
      setResetMessage(errorMessage(error));
    } finally {
      setPreparingUninstall(false);
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

  return (
    <div className="view-stack">
      <Breadcrumbs items={[{ label: "Home", onClick: onHome }, { label: "Settings" }]} />
      <ViewHeader title="Locality controls" />

      <section className="settings-grid">
        <div className="panel">
          <PanelTitle title="Startup" />
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
          <SettingRow title="Default Notion mount point" value="~/Library/CloudStorage/Locality/notion" />
          {settingsMessage && <p className="quiet-note inline-note">{settingsMessage}</p>}
        </div>

        <div className="panel">
          <PanelTitle title="Safety" />
          <SettingRow title="Local edits" value="Pending until reviewed" />
          <SettingRow title="Push confirmation" value="Require for large changes" />
          <SettingRow title="Default new mount mode" value="Edit enabled" />
        </div>

        <div className="panel">
          <PanelTitle title="Updates" />
          <SettingRow title="Channel" value={updateChannelLabel} />
          <SettingRow
            title="Status"
            value={updateStatusLabel}
          />
          {updateStatus.version && <SettingRow title="Available version" value={updateStatus.version} />}
          {!appStoreDistribution && (
            <div className="button-row">
              <SecondaryButton
                compact
                icon={checkingForUpdate ? <Loader2 className="spin-icon" /> : <RefreshCw />}
                disabled={checkingForUpdate || installingUpdate}
                onClick={() => void onCheckForUpdate()}
              >
                {checkingForUpdate ? "Checking" : "Check"}
              </SecondaryButton>
              <PrimaryButton
                compact
                icon={installingUpdate ? <Loader2 className="spin-icon" /> : <Download />}
                disabled={!updateAvailable || checkingForUpdate || installingUpdate}
                onClick={() => void onInstallUpdate()}
              >
                {installingUpdate ? "Installing" : "Install"}
              </PrimaryButton>
            </div>
          )}
        </div>

        <div className="panel">
          <PanelTitle title="Agent Instructions" />
          <SettingRow title="Local agents" value="Claude, Codex, Warp, Cursor, Gemini, Cline/Roo" />
          <SettingRow title="Notion guidance" value="Installed under /Locality/notion" />
          <SecondaryButton
            compact
            icon={installingAgents ? <Loader2 className="spin-icon" /> : <Bot />}
            disabled={installingAgents}
            onClick={() => void installAgentInstructions()}
          >
            {installingAgents ? "Installing" : "Install Agent Skills"}
          </SecondaryButton>
          {agentMessage && <p className="quiet-note inline-note">{agentMessage}</p>}
        </div>

        <div className="panel">
          <PanelTitle title="Diagnostics" />
          <SettingRow title="Locality process" value={daemonStopped ? "Stopped" : "Running"} />
          {snapshot.mount.provider && (
            <SettingRow title="Provider" value={providerStatusLabel(snapshot.mount.provider)} />
          )}
          <SettingRow title="State folder" value="~/.loc" />
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
        </div>

        <div className="panel">
          <PanelTitle title="Developer" />
          <SettingRow title="Local database" value="~/.loc/state.sqlite3" />
          <SettingRow title="Reset behavior" value="Preserve local files" />
          <SecondaryButton
            compact
            icon={resettingState ? <Loader2 className="spin-icon" /> : <RotateCcw />}
            disabled={resettingState || preparingUninstall}
            onClick={() => void resetLocalState()}
          >
            {resettingState ? "Resetting" : "Reset Local State"}
          </SecondaryButton>
          <SecondaryButton
            compact
            icon={preparingUninstall ? <Loader2 className="spin-icon" /> : <Trash2 />}
            disabled={resettingState || preparingUninstall}
            onClick={() => void prepareUninstall()}
          >
            {preparingUninstall ? "Preparing" : "Prepare for Uninstall"}
          </SecondaryButton>
          {resetMessage && <p className="quiet-note inline-note">{resetMessage}</p>}
        </div>

        <div className="panel">
          <PanelTitle title="Quit Options" />
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
        </div>
      </section>
    </div>
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
          <ApertureIcon state={healthIconState(snapshot.health.state)} />
          <strong>Locality</strong>
        </div>
        <StatusPill
          tone={healthTone(snapshot.health.state)}
          title={healthDescription(snapshot.health.state, snapshot.health.attentionCount)}
        >
          {healthLabel(snapshot.health.state)}
        </StatusPill>
      </header>

      <section className="tray-section tray-workspace">
        <p className="label">Notion</p>
        <h2>{snapshot.mount.workspaceName}</h2>
        <button className="path-button" onClick={() => copyText(snapshot.mount.localPath)}>
          {snapshot.mount.localPath}
        </button>
        <PrimaryButton
          compact
          icon={<FolderOpen />}
          onClick={() => void callCommand("open_path", { path: snapshot.mount.localPath }, { ok: true })}
        >
          Open Notion Folder
        </PrimaryButton>
      </section>

      <section className="tray-section tray-live-mode">
        <button
          className={`tray-live-mode-control ${liveModeEnabled ? "active" : ""}`}
          aria-pressed={liveModeEnabled}
          aria-label={`${liveModeEnabled ? "Turn off" : "Turn on"} Live Mode`}
          disabled={mountMissing(snapshot) || connectionMissing(snapshot)}
          onClick={() => void toggleLiveMode()}
        >
          <span className="tray-live-mode-copy">
            {liveModeBusy ? <span className="live-mode-spinner" aria-hidden="true" /> : <Zap />}
            <span>
              <strong>Live Mode</strong>
              <small>{trayLiveModeLabel(snapshot.liveMode, liveModeBusy)}</small>
            </span>
          </span>
          <span className={`toggle ${liveModeEnabled ? "enabled" : ""}`} aria-hidden="true">
            <i />
          </span>
        </button>
        {liveModeMessage && (
          <p className={liveModeState === "error" ? "field-error" : "quiet-note inline-note"}>
            {liveModeMessage}
          </p>
        )}
      </section>

      <section className="tray-section">
        <div className="tray-locate-label">Open a Notion page</div>
        <div className="tray-locate-row">
          <Search />
          <input
            value={url}
            placeholder="Paste URL or search title"
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

      {snapshot.recentFiles.length > 0 && (
        <section className="tray-section tray-recent-files">
          <div className="tray-section-heading">
            <span>Recent Files</span>
            <button onClick={() => openMain("files")}>View</button>
          </div>
          {snapshot.recentFiles.slice(0, 3).map((item) => (
            <button
              type="button"
              key={`${item.kind}-${item.localPath}`}
              onClick={() => void callCommand("reveal_path", { path: item.localPath }, { ok: true })}
            >
              <strong>{item.title}</strong>
              <small>{item.localPath}</small>
            </button>
          ))}
        </section>
      )}

      <button className="tray-row-button" onClick={() => openMain("pending")}>
        <span>Pending Changes</span>
        <strong>{snapshot.pendingChanges.length}</strong>
      </button>

      {visibleChanges.length > 0 && (
        <section className="tray-change-list">
          {visibleChanges.map((change) => (
            <button
              key={change.localPath}
              onClick={() =>
                void callCommand("open_path", { path: joinMountPath(snapshot.mount.localPath, change.localPath) }, { ok: true })
              }
            >
              <span>{change.title}</span>
              <small>{change.summary}</small>
            </button>
          ))}
        </section>
      )}

      <section className="tray-section tray-suggestion">
        <p className="label">Suggestion</p>
        <div className="tray-suggestion-row">
          <strong>Connect {snapshot.suggestions[0]?.connector ?? "Linear"}</strong>
          <button disabled>Coming Soon</button>
        </div>
      </section>

      <footer className="tray-footer">
        <button onClick={() => openMain("settings")}>Settings</button>
        <div className="tray-quit-options" ref={quitOptionsRef}>
          <button onClick={() => setQuitOptionsOpen((open) => !open)}>Quit Options</button>
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
  return (
    <div className="onboarding-product-demo">
      <div className="demo-card-header">
        <span>Product demo</span>
        <strong>Ready</strong>
      </div>
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
            <strong>Pending review</strong>
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

function ConnectorOptions({ connected }: { connected: boolean }) {
  return (
    <div className="connector-options">
      <div className="connector-option available">
        <div>
          <strong>Notion</strong>
          <small>Pages, databases, properties, and Markdown edits.</small>
        </div>
        <span>{connected ? "Connected" : "Available"}</span>
      </div>
      <div className="connector-option">
        <div>
          <strong>Google Docs</strong>
          <small>Docs and Drive folders through the same local model.</small>
        </div>
        <span>Next</span>
      </div>
      <div className="connector-option">
        <div>
          <strong>Linear</strong>
          <small>Issues and projects as agent-editable files.</small>
        </div>
        <span>Planned</span>
      </div>
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
      <div data-tauri-drag-region>{title}</div>
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
  variant?: "final" | "wide";
  side?: React.ReactNode;
}) {
  if (side) {
    return (
      <div className="setup-scrollport">
        <div className="setup-content split-setup">
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
      {!variant && (children ? <span className="brand-word">{children}</span> : <ApertureIcon />)}
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
  return (
    <button className={`sidebar-link ${active ? "active" : ""}`} onClick={onClick}>
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

function ApertureIcon({ state = "default" }: { state?: "default" | "review" | "reconnect" }) {
  return (
    <span className={`aperture-icon ${state}`}>
      <svg aria-hidden="true" viewBox="0 0 28 18">
        <path d="M7 14.4 4.5 9 7 3.6" />
        <path d="M21 3.6 23.5 9 21 14.4" />
        <path d="M9.5 5.7h9" />
        <path d="M9.5 12.3h9" />
        <path d="M12 9h4" />
      </svg>
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
    return "Pending Changes";
  }
  return healthLabel(snapshot.health.state);
}

function sidebarStatusLabel(snapshot: DesktopSnapshot) {
  if (snapshot.health.state === "ready") {
    return "Notion Ready";
  }
  if (snapshot.health.state === "needs_review") {
    return "Pending Changes";
  }
  return healthLabel(snapshot.health.state);
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
  return "Notion is connected, the mount is ready, and remote writes remain explicit.";
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
    return "Pending";
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
