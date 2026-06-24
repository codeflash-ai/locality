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
  Power,
  RefreshCw,
  RotateCcw,
  Search,
  Settings,
  ShieldCheck,
  Sparkles,
  Square,
  Zap,
  X,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

const distributionChannel = (import.meta.env.VITE_AFS_DISTRIBUTION_CHANNEL || "direct").toLowerCase();
const appStoreDistribution = distributionChannel === "mas";

type AppView = "home" | "mount" | "pending" | "review" | "activity" | "settings";
type LocateState = "idle" | "preparing" | "ready" | "error";
type OnboardingStep = 1 | 2 | 3 | 4;

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
  mount: {
    connector: string;
    workspaceName: string;
    localPath: string;
    notionUrl?: string | null;
    accessScope: string;
    projection: string;
    readOnly: boolean;
    status: string;
    provider?: ProviderRuntimeSummary | null;
  };
  settings: {
    launchAtLogin: boolean;
    showMenuBar: boolean;
  };
  pendingChanges: PendingChange[];
  activity: ActivityItem[];
  suggestions: ConnectorSuggestion[];
};

type PendingChange = {
  title: string;
  localPath: string;
  summary: string;
  state: "safe" | "needs_review" | "conflict" | "blocked";
  autoSave: {
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
  undoAvailable: boolean;
};

type ConnectorSuggestion = {
  connector: string;
  description: string;
  state: string;
};

type ProviderRuntimeSummary = {
  state: string;
  message: string;
  daemonRunning: boolean;
  registered?: boolean | null;
  pid?: number | null;
  stalePidFile: boolean;
};

type LocatedItem = {
  title: string;
  kind: string;
  localPath: string;
  state: "ready" | "online_only" | "pending_changes" | "conflict" | "preparing" | "no_access" | "not_found";
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
  mount: {
    connector: "notion",
    workspaceName: "CodeFlash",
    localPath: "~/Library/CloudStorage/AFS/notion",
    notionUrl: "https://www.notion.so/37b3ac0ebb88802cbcf4d53c9cfc4972",
    accessScope: "Initial Idea",
    projection: "macOS File Provider",
    readOnly: false,
    status: "ready",
    provider: null,
  },
  settings: {
    launchAtLogin: true,
    showMenuBar: true,
  },
  pendingChanges: [
    {
      title: "Roadmap 2026",
      localPath: "Engineering/Roadmap 2026/page.md",
      summary: "2 text edits",
      state: "safe",
      autoSave: { enabled: false, state: "off", label: "Auto-save off" },
    },
    {
      title: "Launch Plan",
      localPath: "Marketing/Launch Plan/page.md",
      summary: "needs review: large deletion",
      state: "needs_review",
      autoSave: { enabled: false, state: "off", label: "Auto-save off" },
    },
    {
      title: "Customer Notes",
      localPath: "Sales/Customer Notes/page.md",
      summary: "1 property edit",
      state: "safe",
      autoSave: { enabled: true, state: "active", label: "Auto-save on" },
    },
  ],
  activity: [
    {
      title: "Pushed Roadmap 2026 to Notion",
      detail: "2 block edits",
      when: "Today",
      occurredAt: "unix_ms:1782033300000",
      kind: "push",
      undoAvailable: true,
    },
    {
      title: "Located Launch Plan",
      detail: "Prepared local path for an agent",
      when: "Today",
      occurredAt: "unix_ms:1782028800000",
      kind: "locate",
      undoAvailable: false,
    },
    {
      title: "Connected Notion workspace CodeFlash",
      detail: "Credentials stored in the OS credential store",
      when: "Earlier",
      occurredAt: "unix_ms:1781942400000",
      kind: "connect",
      undoAvailable: false,
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
    localPath: "~/Library/CloudStorage/AFS/notion/Engineering/Roadmap 2026/page.md",
    state: "ready",
  },
  {
    title: "Launch Plan",
    kind: "Page",
    localPath: "~/Library/CloudStorage/AFS/notion/Marketing/Launch Plan/page.md",
    state: "online_only",
  },
];

function suggestedAgentPrompt(mountPath: string) {
  return `Use AFS to edit my Notion workspace. Open the Notion files under ${mountPath}, make the requested edits directly in Markdown, and leave the changes pending for AFS review.`;
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
        path: "~/.claude/skills/afs/SKILL.md",
        detail: "Installed the AFS skill for Claude local agents.",
      },
      {
        agent: "Codex",
        status: "installed",
        path: "~/.codex/skills/afs/SKILL.md",
        detail: "Installed the AFS skill for Codex.",
      },
      {
        agent: "Warp",
        status: "installed",
        path: "~/.agents/skills/afs/SKILL.md",
        detail: "Installed the AFS skill for Warp.",
      },
    ],
  };
}

function isTauriRuntime() {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
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

function liveModeReportNeedsRefresh(message: string) {
  return message.includes("synced") || message.includes("pulled") || message.includes("paused");
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
  const [snapshot, setSnapshot] = useState<DesktopSnapshot>(sampleSnapshot);
  const [snapshotLoaded, setSnapshotLoaded] = useState(() => !isTauriRuntime());
  const [view, setView] = useState<AppView>("home");
  const route = window.location.hash;
  const [showOnboarding, setShowOnboarding] = useState(() => route === "#onboarding-ready");
  const [onboardingKey, setOnboardingKey] = useState(0);
  const [onboardingInitialStep, setOnboardingInitialStep] = useState<1 | 4>(() =>
    route === "#onboarding-ready" ? 4 : 1,
  );
  const [updateStatus, setUpdateStatus] = useState<UpdateStatus>(emptyUpdateStatus);
  const refreshSnapshotPromise = useRef<Promise<void> | null>(null);
  const refreshSnapshotQueued = useRef(false);

  async function refreshSnapshot() {
    if (refreshSnapshotPromise.current) {
      refreshSnapshotQueued.current = true;
      return refreshSnapshotPromise.current;
    }

    const run = async () => {
      do {
        refreshSnapshotQueued.current = false;
        const nextSnapshot = await callCommand<DesktopSnapshot>("desktop_snapshot", undefined, sampleSnapshot);
        setSnapshot(nextSnapshot);
        setSnapshotLoaded(true);
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
          setUpdateStatus({ state: "current", message: "AFS is up to date.", update: null });
        }
        return;
      }

      setUpdateStatus({
        state: "available",
        message: `AFS ${update.version} is ready to install.`,
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
      message: current.version ? `Installing AFS ${current.version}.` : "Installing update.",
    }));

    try {
      const update = updateStatus.update ?? (await check());
      if (!update) {
        setUpdateStatus({ state: "current", message: "AFS is up to date.", update: null });
        return;
      }
      await update.downloadAndInstall();
      setUpdateStatus({
        state: "installing",
        message: "Restarting AFS to finish the update.",
        update: null,
        version: update.version,
      });
      await relaunch();
    } catch (error) {
      setUpdateStatus({ state: "error", message: updaterErrorMessage(error), update: null });
    }
  }

  useEffect(() => {
    void (async () => {
      if (isTauriRuntime()) {
        await callCommand<ActionReport>("acknowledge_install_state").catch(() => undefined);
        if (!appStoreDistribution) {
          await callCommand<ActionReport>("ensure_terminal_cli_available").catch(() => undefined);
        }
        await callCommand<ActionReport>("ensure_runtime_ready").catch(() => undefined);
      }
      await refreshSnapshot();
    })().catch(() => {
      setSnapshot(sampleSnapshot);
      setSnapshotLoaded(true);
    });
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
    if (!snapshotLoaded || route === "#tray") {
      return;
    }

    if (route === "#onboarding-ready") {
      setShowOnboarding(true);
      return;
    }

    setShowOnboarding(false);
  }, [route, snapshotLoaded]);

  useEffect(() => {
    const handleOpenView = (event: Event) => {
      const nextView = (event as CustomEvent<string>).detail;
      if (!isAppView(nextView)) {
        return;
      }
      setShowOnboarding(false);
      setView(nextView);
    };

    window.addEventListener("afs-open-view", handleOpenView);
    return () => window.removeEventListener("afs-open-view", handleOpenView);
  }, []);

  useEffect(() => {
    const refresh = () => {
      void refreshSnapshot().catch(() => undefined);
    };

    window.addEventListener("afs-refresh-snapshot", refresh);
    return () => {
      window.removeEventListener("afs-refresh-snapshot", refresh);
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
    return <TrayPopover snapshot={snapshot} />;
  }

  if (showOnboarding) {
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
  const [mountError, setMountError] = useState("");
  const [agentGuidanceReport, setAgentGuidanceReport] = useState<AgentGuidanceInstallReport | null>(null);
  const [agentGuidanceState, setAgentGuidanceState] = useState<"idle" | "installing" | "ready" | "error">("idle");

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
    if (step !== 2 || !oauthInFlight || oauthReady) {
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
    if (!snapshotLoaded || window.location.hash === "#onboarding-ready" || connectionMissing(snapshot)) {
      return;
    }

    setOauthReady(true);
    setStep((current) => {
      if (mountMissing(snapshot)) {
        return current < 3 ? 3 : current;
      }
      return current < 4 ? 4 : current;
    });
  }, [snapshot.connection.status, snapshot.mount.status, snapshotLoaded]);

  useEffect(() => {
    if (step !== 4 || mountMissing(snapshot) || agentGuidanceState !== "idle") {
      return;
    }
    void installAgentGuidance(mountPath);
  }, [agentGuidanceState, mountPath, snapshot.mount.status, step]);

  async function startConnect() {
    setOauthError("");
    setLoginUrl("");
    setLoginCopyMessage("");
    setOauthReady(false);
    setOauthInFlight(true);
    setStep(2);
    try {
      const report = await callCommand<ActionReport>(
        "connect_notion",
        undefined,
        { ok: true, message: "Connected demo workspace." },
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
      setConnectedWorkspace(nextSnapshot.connection.workspaceName);
      setOauthReady(true);
    } catch (error) {
      setOauthError(errorMessage(error));
    } finally {
      setOauthInFlight(false);
    }
  }

  async function copyLoginLink() {
    setOauthError("");
    setLoginCopyMessage("");
    const url =
      loginUrl ||
      (await callCommand<string | null>("notion_login_link", undefined, null).catch(() => null));
    if (!url) {
      setOauthError("The Notion login link is still being prepared. Try again in a moment.");
      return;
    }
    setLoginUrl(url);
    copyText(url);
    setLoginCopyMessage("Copied login link.");
  }

  async function startMount() {
    setMountError("");
    const report = await callCommand<ActionReport>(
      "create_workspace_mount",
      { path: mountPath },
      { ok: true, message: "Created demo mount." },
    );
    if (!report.ok) {
      setMountError(report.message);
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
    setStep(4);
  }

  async function ensureCliAvailable() {
    if (appStoreDistribution) {
      return true;
    }

    const report = await callCommand<ActionReport>(
      "ensure_terminal_cli_available",
      undefined,
      { ok: true, message: "AFS terminal command is ready." },
    );
    if (!report.ok) {
      setMountError(report.message);
      return false;
    }
    setMountError("");
    return true;
  }

  async function chooseFolder() {
    setMountError("");
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
      setMountError(errorMessage(error));
    }
  }

  async function openFolderAndFinish() {
    setMountError("");
    const report = await callCommand<ActionReport>(
      "open_path",
      { path: mountPath },
      { ok: true, message: "Opened demo folder." },
    );
    if (!report.ok) {
      setMountError(report.message);
      return;
    }
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
          localPath: "~/Library/CloudStorage/AFS/notion/Engineering/Roadmap 2026/page.md",
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
    <main className="setup-shell">
      <section className="setup-window">
        <WindowChrome title="AFS Setup" meta={`${step} of 4`} />
        {step === 1 && (
          <SetupContent mark={<BrandTile>AFS</BrandTile>}>
            <div>
              <h1>Let your agents edit Notion as local files.</h1>
              <p>
                Mount your Notion workspace in CloudStorage. Agents edit local
                files, then AFS syncs reviewed changes back to Notion.
              </p>
            </div>
            <PrimaryButton onClick={startConnect}>Connect Notion</PrimaryButton>
            <p className="quiet-note">Local edits stay pending until you review and push.</p>
          </SetupContent>
        )}

        {step === 2 && (
          <SetupContent
            mark={
              <BrandTile variant={oauthReady ? "ready" : "notion"}>
                {oauthReady ? undefined : "N"}
              </BrandTile>
            }
          >
            <div>
              <div className={`sync-note ${oauthReady ? "connected" : ""}`}>
                {oauthReady ? <Check /> : <Loader2 className={oauthInFlight ? "spin" : ""} />}
                {oauthReady ? "Notion connected" : "Waiting for Notion"}
              </div>
              <h1>{oauthReady ? "Your Notion workspace is connected" : "Finish connecting in Notion"}</h1>
              <p>
                {oauthReady
                  ? `${
                      connectedWorkspace || "Your workspace"
                    } is ready. Next, choose where AFS should place the local folder.`
                  : "A browser window is open. Choose your workspace, pick the pages AFS can use, then approve access."}
              </p>
            </div>
            <ProgressList
              items={[
                { label: "Browser opened", state: oauthError ? "idle" : "done" },
                { label: "Select workspace and pages", state: oauthReady ? "done" : "active" },
                { label: "Approve access", state: oauthReady ? "done" : "idle" },
              ]}
            />
            <PrimaryButton disabled={!oauthReady} onClick={() => setStep(3)}>
              {oauthReady ? "Continue to folder setup" : oauthInFlight ? "Waiting for Notion" : "Continue"}
            </PrimaryButton>
            <TextButton disabled={!oauthInFlight && !loginUrl} onClick={() => void copyLoginLink()}>
              Copy login link
            </TextButton>
            {loginCopyMessage && <p className="quiet-note">{loginCopyMessage}</p>}
            {oauthError && <p className="field-error">{oauthError}</p>}
            <p className="quiet-note">Credentials are stored securely in the OS credential store.</p>
          </SetupContent>
        )}

        {step === 3 && (
          <SetupContent mark={<BrandTile variant="folder" />}>
            <div>
              <h1>Where should your Notion files appear?</h1>
              <p>
                AFS keeps every source under one CloudStorage root. Notion will appear as the
                live folder Finder and agents open.
              </p>
            </div>
            <div className="path-field">
              <input
                value={mountPath}
                onChange={(event) => {
                  setMountPathDirty(true);
                  setMountPath(event.target.value);
                }}
              />
              <SecondaryButton compact onClick={chooseFolder}>
                Choose
              </SecondaryButton>
            </div>
            <PrimaryButton disabled={!mountPath.trim()} onClick={startMount}>
              Continue
            </PrimaryButton>
            {mountError && <p className="field-error">{mountError}</p>}
            <p className="quiet-note">
              The Notion folder will include AGENTS.md and CLAUDE.md to help your agents edit
              files natively.
            </p>
          </SetupContent>
        )}

        {step === 4 && (
          <SetupContent mark={<BrandTile variant="ready" />} variant="final">
            <div>
              <h1>AFS is ready</h1>
              <p>
                Your Notion folder is mounted. AFS will keep syncing the workspace quietly in the
                background while agents edit local Markdown.
              </p>
            </div>
            <div className="ready-folder">
              <FolderOpen />
              <div>
                <span>Notion folder</span>
                <code>{mountPath}</code>
              </div>
              <SecondaryButton compact icon={<Copy />} onClick={() => copyText(mountPath)}>
                Copy
              </SecondaryButton>
            </div>
            <div className="final-actions">
              <PrimaryButton icon={<FolderOpen />} onClick={openFolderAndFinish}>
                Open Notion Folder
              </PrimaryButton>
            </div>
            <LocateBox
              label="Open a Notion page"
              value={locateUrl}
              onChange={(next) => {
                setLocateUrl(next);
                setLocateState("idle");
                setLocatedItem(null);
              }}
              onSubmit={locatePage}
              onSelect={(item) => {
                setLocatedItem(item);
                setLocateState("ready");
                setLocateError("");
                setLocateUrl(item.title);
              }}
              state={locateState}
              error={locateError}
            />
            {locatedItem && <LocatedPath item={locatedItem} />}
            <div className="agent-demo compact-agent-demo">
              <div className="agent-demo-title">
                <Clipboard />
                <span>Try this with an agent</span>
              </div>
              <div className="agent-prompt-row">
                <div className="agent-demo-command">
                  {agentGuidanceReport?.prompt ||
                    `In ${mountPath}, find the Q4 launch plan and make it sharper for leadership review. Keep the edits ready for AFS review.`}
                </div>
                <SecondaryButton
                  compact
                  icon={<Copy />}
                  onClick={() =>
                    copyText(
                      agentGuidanceReport?.prompt ||
                        `In ${mountPath}, find the Q4 launch plan and make it sharper for leadership review. Keep the edits ready for AFS review.`,
                    )
                  }
                >
                  Copy
                </SecondaryButton>
              </div>
            </div>
            <AgentGuidanceSummary report={agentGuidanceReport} state={agentGuidanceState} />
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
          ? "Agents can use AFS"
          : "Preparing agents";

  return (
    <div className={`agent-guidance-card ${failed ? "warning" : ""}`}>
      <div className="agent-demo-title">
        {state === "installing" ? <Loader2 className="spin-icon" /> : failed ? <AlertTriangle /> : <Bot />}
        <span>{title}</span>
      </div>
      {state === "installing" && <p>Installing the AFS skill for local agents.</p>}
      {state !== "installing" && installedAgents.length > 0 && (
        <p>
          Now your agents know how to use <code>afs</code> to view and edit Notion. Installed for{" "}
          <strong>{formatList(installedAgents)}</strong>.
        </p>
      )}
      {state !== "installing" && installedAgents.length === 0 && fallbackTargets.length > 0 && (
        <p>{fallbackTargets[0].detail}</p>
      )}
      {state !== "installing" && installedAgents.length === 0 && fallbackTargets.length === 0 && !failed && (
        <p>AFS is preparing local agent instructions for this Notion folder.</p>
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

  return (
    <main className="app-frame">
      <WindowChrome
        title="AFS"
        meta={meta}
        metaTitle={statusTitle}
        onMetaClick={statusTarget ? () => onViewChange(statusTarget) : undefined}
      />
      <div className="app-shell">
        <aside className="sidebar">
          <div className="sidebar-brand">
            <ApertureIcon />
            <strong>AFS</strong>
          </div>
          <nav>
            <SidebarButton active={view === "home"} icon={<Home />} onClick={() => onViewChange("home")}>
              Home
            </SidebarButton>
            <SidebarButton active={view === "mount"} icon={<FolderOpen />} onClick={() => onViewChange("mount")}>
              Mount
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
              onClick={() => onViewChange(statusTarget ?? "mount")}
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
              onMount={() => onViewChange("mount")}
              onReview={() => onViewChange("pending")}
              onRefresh={onRefresh}
            />
          )}
          {view === "mount" && (
            <MountDetailView
              snapshot={snapshot}
              onHome={() => onViewChange("home")}
              onRefresh={onRefresh}
              onReview={() => onViewChange("pending")}
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
        <strong>{status.version ? `AFS ${status.version} available` : "AFS update available"}</strong>
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
  onReview,
  onRefresh,
}: {
  snapshot: DesktopSnapshot;
  onMount: () => void;
  onReview: () => void;
  onRefresh: () => Promise<void>;
}) {
  const [url, setUrl] = useState("");
  const [locateState, setLocateState] = useState<LocateState>("idle");
  const [locateError, setLocateError] = useState("");
  const [locatedItem, setLocatedItem] = useState<LocatedItem | null>(null);
  const [actionError, setActionError] = useState("");
  const [liveModeEnabled, setLiveModeEnabled] = useState(false);
  const [liveModeBusy, setLiveModeBusy] = useState(false);
  const [liveModeState, setLiveModeState] = useState<"idle" | "active" | "error">("idle");
  const [liveModeMessage, setLiveModeMessage] = useState("");
  const liveModeInFlight = useRef(false);
  const liveModeSnapshot = useRef(snapshot);
  const refreshHomeSnapshot = useRef(onRefresh);
  const hasPendingChanges = snapshot.pendingChanges.length > 0;

  useEffect(() => {
    liveModeSnapshot.current = snapshot;
  }, [snapshot]);

  useEffect(() => {
    refreshHomeSnapshot.current = onRefresh;
  }, [onRefresh]);

  useEffect(() => {
    if (!liveModeEnabled) {
      setLiveModeBusy(false);
      return undefined;
    }

    let cancelled = false;
    const runTick = async () => {
      if (cancelled || liveModeInFlight.current) {
        return;
      }

      const currentSnapshot = liveModeSnapshot.current;
      if (connectionMissing(currentSnapshot) || mountMissing(currentSnapshot)) {
        setLiveModeEnabled(false);
        setLiveModeState("error");
        setLiveModeMessage("Live Mode needs a connected Notion folder.");
        return;
      }

      liveModeInFlight.current = true;
      setLiveModeBusy(true);
      try {
        const report = await callCommand<ActionReport>(
          "live_mode_tick",
          undefined,
          { ok: true, message: "Live Mode checked for changes." },
        );
        if (cancelled) {
          return;
        }
        if (!report.ok) {
          setLiveModeEnabled(false);
          setLiveModeState("error");
          setLiveModeMessage(report.message);
          return;
        }
        setLiveModeState("active");
        if (report.message) {
          setLiveModeMessage((current) => (current === report.message ? current : report.message));
        }
        if (liveModeReportNeedsRefresh(report.message)) {
          await refreshHomeSnapshot.current().catch(() => undefined);
        }
      } catch (error) {
        if (!cancelled) {
          setLiveModeEnabled(false);
          setLiveModeState("error");
          setLiveModeMessage(errorMessage(error));
        }
      } finally {
        liveModeInFlight.current = false;
        if (!cancelled) {
          setLiveModeBusy(false);
        }
      }
    };

    void runTick();
    const interval = window.setInterval(runTick, 500);
    return () => {
      cancelled = true;
      window.clearInterval(interval);
    };
  }, [liveModeEnabled]);

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

  function toggleLiveMode() {
    setActionError("");
    setLiveModeMessage("");
    setLiveModeState(liveModeEnabled ? "idle" : "active");
    setLiveModeEnabled((enabled) => !enabled);
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
          localPath: "~/Library/CloudStorage/AFS/notion/Engineering/Roadmap 2026/page.md",
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
            <p>AFS needs access before it can create local files for agents.</p>
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
            <p>Use the default source folder under the shared AFS CloudStorage root.</p>
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
            <div>
              <p className="label">Connected workspace</p>
              <h2>{snapshot.mount.workspaceName}</h2>
              <p className="path-line">{snapshot.mount.localPath}</p>
            </div>
            <div className="button-row">
              <button
                className={`live-mode-button ${liveModeEnabled ? "active" : ""}`}
                aria-pressed={liveModeEnabled}
                onClick={toggleLiveMode}
              >
                {liveModeBusy ? <Loader2 className="spin-icon" /> : <Zap />}
                <span>Live Mode</span>
              </button>
              <SecondaryButton icon={<FolderOpen />} onClick={() => void openWorkspaceFolder(snapshot.mount.localPath)}>
                Open Folder
              </SecondaryButton>
              <SecondaryButton icon={<ChevronRight />} onClick={onMount}>
                Mount Detail
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
          <p>{snapshot.suggestions[0]?.description ?? "Mount more workspaces as local files."}</p>
        </div>
        <SecondaryButton compact disabled>
          Coming Soon
        </SecondaryButton>
      </section>
    </div>
  );
}

function MountDetailView({
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
  const hasPendingChanges = snapshot.pendingChanges.length > 0;
  const [actionError, setActionError] = useState("");
  const [accessMessage, setAccessMessage] = useState("");
  const [accessState, setAccessState] = useState<"idle" | "changing" | "success" | "error">("idle");
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

  return (
    <div className="view-stack">
      <Breadcrumbs items={[{ label: "Home", onClick: onHome }, { label: "Mount" }]} />
      <ViewHeader title={snapshot.mount.workspaceName}>
        <StatusPill
          tone={healthTone(snapshot.health.state)}
          title={healthDescription(snapshot.health.state, snapshot.health.attentionCount)}
        >
          {healthLabel(snapshot.health.state)}
        </StatusPill>
      </ViewHeader>

      <section className="mount-hero">
        <div className="mount-hero-icon">
          <FolderOpen />
        </div>
        <div>
          <p className="label">Notion folder</p>
          <h2>{snapshot.mount.localPath}</h2>
          <p>
            AFS follows your Notion workspace hierarchy here, starting with the pages and databases
            your connection can access.
          </p>
        </div>
        <div className="mount-actions">
          <PrimaryButton icon={<FolderOpen />} onClick={() => void openFolder()}>
            Open Folder
          </PrimaryButton>
          <SecondaryButton compact icon={<Copy />} onClick={() => copyText(snapshot.mount.localPath)}>
            Copy Path
          </SecondaryButton>
          <SecondaryButton compact icon={<Code2 />} onClick={() => void openVsCode()}>
            Open in VS Code
          </SecondaryButton>
          <SecondaryButton
            compact
            disabled={connectionMissing(snapshot) || accessState === "changing"}
            icon={accessState === "changing" ? <Loader2 className="spin-icon" /> : <ShieldCheck />}
            onClick={() => void changeNotionAccess()}
          >
            {accessState === "changing" ? "Waiting for Notion" : "Change Notion Access"}
          </SecondaryButton>
        </div>
      </section>
      {actionError && <p className="field-error">{actionError}</p>}
      {accessMessage && (
        <p className={accessState === "error" ? "field-error" : "quiet-note inline-note"}>
          {accessMessage}
        </p>
      )}

      <section className="detail-grid">
        <div className="panel">
          <PanelTitle title="Notion Access" />
          <SettingRow title="Workspace" value={snapshot.connection.workspaceName} />
          {showAccount && <SettingRow title="Account" value={accountLabel} />}
          <SettingRow title="Selected access" value={snapshot.mount.accessScope} />
          {snapshot.mount.notionUrl && (
            <SettingRow title="Mounted root" value="Open in Notion" href={snapshot.mount.notionUrl} />
          )}
          <SettingRow title="Permission" value={snapshot.mount.readOnly ? "Read only" : "Edit enabled"} />
        </div>

        <div className="panel">
          <PanelTitle title="Local Files" />
          <SettingRow title="Location" value={snapshot.mount.localPath} />
          <SettingRow title="Projection" value={snapshot.mount.projection} />
          <SettingRow title="Mounted content" value="Workspace hierarchy" />
          <SettingRow title="Agent guidance" value="AGENTS.md and CLAUDE.md" />
        </div>
      </section>

      <section className="safety-strip">
        <ShieldCheck />
        <div>
          <h2>Edits stay pending until reviewed</h2>
          <p>
            Local changes are staged first. Push review shows what will update in Notion before
            remote writes happen.
          </p>
        </div>
        {hasPendingChanges && (
          <PrimaryButton compact icon={<ListChecks />} onClick={onReview}>
            Review
          </PrimaryButton>
        )}
      </section>

      <details className="advanced-panel">
        <summary>Advanced diagnostics</summary>
        <div className="settings-grid compact-settings">
          <div className="panel">
            <SettingRow title="AFS process" value={snapshot.health.state === "stopped" ? "Stopped" : "Running"} />
            <SettingRow title="State folder" value="~/.afs" />
            <SettingRow title="Connector" value={snapshot.mount.connector} />
          </div>
          <div className="panel">
            <SettingRow title="Connection status" value={snapshot.connection.status} />
            <SettingRow title="Mount status" value={snapshot.mount.status} />
            <SettingRow title="Pending changes" value={String(snapshot.pendingChanges.length)} />
          </div>
        </div>
      </details>
    </div>
  );
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
          title={isPushing ? "AFS is writing the approved local changes to Notion." : "This push is ready for review."}
        >
          {pushState === "error" ? "Needs Attention" : isPushing ? "Pushing" : pushSucceeded ? "Pushed" : "Safe"}
        </StatusPill>
      </ViewHeader>
      <p className="view-copy">{plan.summary}</p>
      {isPushing && (
        <p className="quiet-note inline-note">
          Writing changes to Notion. You can keep reviewing this window while AFS finishes.
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
      <ViewHeader title="Recent activity" />
      {Object.entries(grouped).map(([when, items]) => (
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
              {item.undoAvailable && (
                <SecondaryButton compact icon={<RotateCcw />}>
                  Undo Push
                </SecondaryButton>
              )}
            </article>
          ))}
        </section>
      ))}
    </div>
  );
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
      { ok: true, message: "AFS runtime is running." },
    );
    setDiagnosticMessage(report.message);
    await onRefresh().catch(() => undefined);
  }

  function copyDiagnostics() {
    const summary = [
      `AFS process: ${daemonStopped ? "Stopped" : "Running"}`,
      snapshot.mount.provider ? `Provider: ${providerStatusLabel(snapshot.mount.provider)}` : null,
      "State folder: ~/.afs",
      `Projection: ${snapshot.mount.projection}`,
      `Connection: ${snapshot.connection.status}`,
      `Mount: ${snapshot.mount.status}`,
      `Pending changes: ${snapshot.pendingChanges.length}`,
    ].filter(Boolean).join("\n");
    copyText(summary);
    setDiagnosticMessage("Copied diagnostics summary.");
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
      "Reset local AFS state? This clears AFS metadata, cache, mount registration, and connector credentials. It does not delete your local files.",
    );
    if (!confirmed) {
      return;
    }

    setResetMessage("");
    setResettingState(true);
    try {
      const report = await callCommand<ActionReport>(
        "reset_local_afs_state",
        undefined,
        { ok: true, message: "AFS local state was reset." },
      );
      setResetMessage(report.message);
      if (report.ok) {
        await onRefresh().catch(() => undefined);
        onResetComplete();
      }
    } catch (error) {
      setResetMessage(errorMessage(error));
    } finally {
      setResettingState(false);
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
      <ViewHeader title="AFS controls" />

      <section className="settings-grid">
        <div className="panel">
          <PanelTitle title="Startup" />
          <ToggleRow
            title="Launch AFS at login"
            enabled={localSettings.launchAtLogin}
            busy={busySetting === "launch_at_login"}
            onToggle={(enabled) => void updateDesktopSetting("launch_at_login", enabled)}
          />
          <ToggleRow
            title="Show AFS in the menu bar"
            enabled={localSettings.showMenuBar}
            busy={busySetting === "show_menu_bar"}
            onToggle={(enabled) => void updateDesktopSetting("show_menu_bar", enabled)}
          />
          <SettingRow title="Default folder" value="~/Library/CloudStorage/AFS" />
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
          <SettingRow title="Notion guidance" value="Installed under /AFS/notion" />
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
          <SettingRow title="AFS process" value={daemonStopped ? "Stopped" : "Running"} />
          {snapshot.mount.provider && (
            <SettingRow title="Provider" value={providerStatusLabel(snapshot.mount.provider)} />
          )}
          <SettingRow title="State folder" value="~/.afs" />
          <SettingRow title="Projection" value={snapshot.mount.projection} />
          <div className="button-row">
            <SecondaryButton compact onClick={copyDiagnostics}>
              Copy Summary
            </SecondaryButton>
            <SecondaryButton compact disabled={!runtimeNeedsRepair} onClick={() => void repairRuntime()}>
              {runtimeNeedsRepair ? "Start AFS" : "Repair AFS"}
            </SecondaryButton>
          </div>
          {diagnosticMessage && <p className="quiet-note inline-note">{diagnosticMessage}</p>}
        </div>

        <div className="panel">
          <PanelTitle title="Developer" />
          <SettingRow title="Local database" value="~/.afs/state.sqlite3" />
          <SettingRow title="Reset behavior" value="Preserve local files" />
          <SecondaryButton
            compact
            icon={resettingState ? <Loader2 className="spin-icon" /> : <RotateCcw />}
            disabled={resettingState}
            onClick={() => void resetLocalState()}
          >
            {resettingState ? "Resetting" : "Reset Local State"}
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

function TrayPopover({ snapshot }: { snapshot: DesktopSnapshot }) {
  const [url, setUrl] = useState("");
  const [locateState, setLocateState] = useState<LocateState>("idle");
  const [locateError, setLocateError] = useState("");
  const [locatedItem, setLocatedItem] = useState<LocatedItem | null>(null);
  const [quitOptionsOpen, setQuitOptionsOpen] = useState(false);
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
          localPath: "~/Library/CloudStorage/AFS/notion/Engineering/Roadmap 2026/page.md",
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
          <strong>AFS</strong>
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
  const [autoSaveOverrides, setAutoSaveOverrides] = useState<Record<string, PendingChange["autoSave"]>>({});

  useEffect(() => {
    setAutoSaveOverrides((current) => {
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
          override.enabled === change.autoSave.enabled &&
          override.state === change.autoSave.state &&
          override.label === change.autoSave.label
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

  async function runFileAction(change: PendingChange, action: "diff" | "push" | "resolve") {
    const path = joinMountPath(mountPath, change.localPath);
    const workingMessage =
      action === "diff" ? "Checking diff..." : action === "push" ? "Pushing this file..." : "Pulling latest...";
    setActions((current) => ({
      ...current,
      [change.localPath]: { state: "working", message: workingMessage },
    }));

    try {
      const command =
        action === "diff" ? "diff_notion_file" : action === "push" ? "push_notion_file" : "pull_notion_file";
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
        },
      }));
      if (report.ok && action === "resolve" && selectedPath === change.localPath) {
        await loadFileDetails(change);
      }
      if (report.ok && action !== "diff") {
        await onRefresh?.().catch(() => undefined);
      }
    } catch (error) {
      setActions((current) => ({
        ...current,
        [change.localPath]: { state: "error", message: errorMessage(error) },
      }));
    }
  }

  async function toggleAutoSave(change: PendingChange, enabled: boolean) {
    const path = joinMountPath(mountPath, change.localPath);
    const optimisticState: PendingChange["autoSave"] = {
      ...change.autoSave,
      enabled,
      state: enabled ? "active" : "off",
      label: enabled ? "Auto-save on" : "Auto-save off",
      reason: null,
    };
    setAutoSaveOverrides((current) => ({
      ...current,
      [change.localPath]: optimisticState,
    }));
    setActions((current) => ({
      ...current,
      [change.localPath]: { state: "working", message: enabled ? "Turning on auto-save..." : "Turning off auto-save..." },
    }));

    try {
      const report = await callCommand<ActionReport>("set_auto_save_for_file", {
        change: { path, enabled },
      });
      setActions((current) => ({
        ...current,
        [change.localPath]: {
          state: report.ok ? "success" : "error",
          message: report.message,
        },
      }));
      if (!report.ok) {
        setAutoSaveOverrides((current) => ({
          ...current,
          [change.localPath]: change.autoSave,
        }));
      }
      await onRefresh?.().catch(() => undefined);
    } catch (error) {
      setAutoSaveOverrides((current) => ({
        ...current,
        [change.localPath]: change.autoSave,
      }));
      setActions((current) => ({
        ...current,
        [change.localPath]: { state: "error", message: errorMessage(error) },
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
        const isSaving = editor?.state === "saving";
        const hasUnsavedEditorChanges = editor !== undefined && editor.contents !== editor.savedContents;
        const shouldReviewBeforePush = Boolean(!confirmDangerous && change.state === "needs_review" && onReview);
        const actionNeedsReview = Boolean(action?.state === "error" && pushNeedsReview(action.message) && onReview);
        const isSelected = selectedPath === change.localPath;
        const autoSave = autoSaveOverrides[change.localPath] ?? change.autoSave;
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
              <div className={`auto-save-control ${autoSave.state}`}>
                <span title={autoSave.reason || autoSave.label}>{autoSave.label}</span>
                <button
                  className={`toggle ${autoSave.enabled ? "enabled" : ""}`}
                  type="button"
                  disabled={isWorking}
                  aria-label={`${autoSave.enabled ? "Turn off" : "Turn on"} auto-save for ${change.title}`}
                  onClick={() => void toggleAutoSave(change, !autoSave.enabled)}
                >
                  <i />
                </button>
              </div>
              <SecondaryButton compact disabled={isWorking} icon={<Search />} onClick={() => void runFileAction(change, "diff")}>
                Diff
              </SecondaryButton>
              <SecondaryButton compact disabled={isWorking} icon={<RefreshCw />} onClick={() => void runFileAction(change, "resolve")}>
                Resolve
              </SecondaryButton>
              <PrimaryButton
                compact
                icon={shouldReviewBeforePush ? <ListChecks /> : <ShieldCheck />}
                disabled={isWorking}
                onClick={() => {
                  if (shouldReviewBeforePush) {
                    onReview?.();
                    return;
                  }
                  void runFileAction(change, "push");
                }}
              >
                {shouldReviewBeforePush ? "Review" : "Push"}
              </PrimaryButton>
              <SecondaryButton
                compact
                disabled={isWorking}
                icon={<FolderOpen />}
                onClick={() =>
                  void callCommand("open_path", { path: joinMountPath(mountPath, change.localPath) }, { ok: true })
                }
              >
                Open
              </SecondaryButton>
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
                        onClick={() => {
                          if (shouldReviewBeforePush) {
                            onReview?.();
                            return;
                          }
                          void runFileAction(change, "push");
                        }}
                      >
                        {shouldReviewBeforePush ? "Review Saved" : "Push Saved"}
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
        <SecondaryButton compact icon={<FolderOpen />} onClick={() => void callCommand("reveal_path", { path: item.localPath }, { ok: true })}>
          Reveal in Finder
        </SecondaryButton>
      </div>
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
        onClick={() => void getCurrentWindow().close()}
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
}: {
  mark: React.ReactNode;
  children: React.ReactNode;
  variant?: "final";
}) {
  return (
    <div className={`setup-content ${variant === "final" ? "final-setup" : ""}`}>
      {mark}
      {children}
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
      {!variant && children}
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
  disabled,
  onClick,
}: {
  children: React.ReactNode;
  icon?: React.ReactNode;
  compact?: boolean;
  disabled?: boolean;
  onClick?: () => void;
}) {
  return (
    <button className={`primary-button ${compact ? "compact" : ""}`} disabled={disabled} onClick={onClick}>
      {icon}
      <span>{children}</span>
    </button>
  );
}

function SecondaryButton({
  children,
  icon,
  compact,
  disabled,
  onClick,
}: {
  children: React.ReactNode;
  icon?: React.ReactNode;
  compact?: boolean;
  disabled?: boolean;
  onClick?: () => void;
}) {
  return (
    <button className={`secondary-button ${compact ? "compact" : ""}`} disabled={disabled} onClick={onClick}>
      {icon}
      <span>{children}</span>
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

function Metric({ label, value }: { label: string; value: number }) {
  return (
    <article className="metric">
      <strong>{value}</strong>
      <span>{label}</span>
    </article>
  );
}

function connectionMissing(snapshot: DesktopSnapshot) {
  return snapshot.connection.status === "missing";
}

function mountMissing(snapshot: DesktopSnapshot) {
  return snapshot.mount.status === "not_mounted";
}

function isAppView(value: string): value is AppView {
  return value === "home" || value === "mount" || value === "pending" || value === "review" || value === "activity" || value === "settings";
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
    return "Notion needs to be reconnected before AFS can sync this workspace.";
  }
  if (state === "stopped") {
    return "The AFS daemon is stopped. Background sync, hydration, and auto-save are paused; direct actions can still run from the app.";
  }
  if (state === "runtime_stopped") {
    return "The filesystem provider is stopped or unregistered. Use Repair AFS in Settings to restore online-only file access.";
  }
  if (state === "checking_freshness") {
    return "AFS is checking the local mount and Notion freshness state.";
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
