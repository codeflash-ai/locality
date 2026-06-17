import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  AlertTriangle,
  Check,
  ChevronRight,
  Clipboard,
  Clock3,
  Copy,
  EyeOff,
  FolderOpen,
  History,
  Home,
  ListChecks,
  Loader2,
  Power,
  RotateCcw,
  Search,
  Settings,
  ShieldCheck,
  Sparkles,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";

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
    projection: string;
    readOnly: boolean;
    status: string;
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
};

type ActivityItem = {
  title: string;
  detail: string;
  when: string;
  kind: string;
  undoAvailable: boolean;
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

type InstallStateReview = {
  shouldPrompt: boolean;
  stateExists: boolean;
  sqliteExists: boolean;
  previousBuildId?: string | null;
  currentBuildId: string;
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
    projection: "macOS File Provider",
    readOnly: false,
    status: "ready",
  },
  settings: {
    launchAtLogin: true,
    showMenuBar: true,
  },
  pendingChanges: [
    {
      title: "Roadmap 2026",
      localPath: "Engineering/Roadmap 2026 ~a3f2.md",
      summary: "2 text edits",
      state: "safe",
    },
    {
      title: "Launch Plan",
      localPath: "Marketing/Launch Plan ~8841.md",
      summary: "needs review: large deletion",
      state: "needs_review",
    },
    {
      title: "Customer Notes",
      localPath: "Sales/Customer Notes ~6b91.md",
      summary: "1 property edit",
      state: "safe",
    },
  ],
  activity: [
    {
      title: "Pushed Roadmap 2026 to Notion",
      detail: "2 block edits",
      when: "Today",
      kind: "push",
      undoAvailable: true,
    },
    {
      title: "Located Launch Plan",
      detail: "Prepared local path for an agent",
      when: "Today",
      kind: "locate",
      undoAvailable: false,
    },
    {
      title: "Connected Notion workspace CodeFlash",
      detail: "Credentials stored in the OS credential store",
      when: "Earlier",
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
    localPath: "~/Library/CloudStorage/AFS/notion/Engineering/Roadmap 2026 ~a3f2.md",
    state: "ready",
  },
  {
    title: "Launch Plan",
    kind: "Page",
    localPath: "~/Library/CloudStorage/AFS/notion/Marketing/Launch Plan ~8841.md",
    state: "online_only",
  },
];

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
  const [showOnboarding, setShowOnboarding] = useState(() => route !== "#app" && route !== "#tray");
  const [installReview, setInstallReview] = useState<InstallStateReview | null>(null);
  const [onboardingKey, setOnboardingKey] = useState(0);
  const [onboardingInitialStep, setOnboardingInitialStep] = useState<1 | 4>(() =>
    route === "#onboarding-ready" ? 4 : 1,
  );
  const setupIsComplete = setupComplete(snapshot);

  async function refreshSnapshot() {
    const nextSnapshot = await callCommand<DesktopSnapshot>("desktop_snapshot", undefined, sampleSnapshot);
    setSnapshot(nextSnapshot);
    setSnapshotLoaded(true);
  }

  useEffect(() => {
    void (async () => {
      if (isTauriRuntime()) {
        const review = await callCommand<InstallStateReview>("install_state_review");
        if (review.shouldPrompt) {
          setInstallReview(review);
          setSnapshotLoaded(true);
          return;
        }
        await callCommand<ActionReport>("acknowledge_install_state").catch(() => undefined);
        await callCommand<ActionReport>("ensure_runtime_ready").catch(() => undefined);
      }
      await refreshSnapshot();
    })().catch(() => {
      setSnapshot(sampleSnapshot);
      setSnapshotLoaded(true);
    });
  }, []);

  useEffect(() => {
    if (!snapshotLoaded || route === "#app" || route === "#tray" || route === "#onboarding-ready") {
      return;
    }
    if (setupIsComplete) {
      setShowOnboarding(false);
    }
  }, [route, setupIsComplete, snapshotLoaded]);

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
    document.body.dataset.surface = route === "#tray" ? "tray" : "app";
  }, [route]);

  if (route === "#tray") {
    return <TrayPopover snapshot={snapshot} />;
  }

  if (installReview?.shouldPrompt) {
    return (
      <StateResetPrompt
        review={installReview}
        onReset={async () => {
          const report = await callCommand<ActionReport>("reset_local_afs_state", undefined, {
            ok: true,
            message: "AFS local state was reset.",
          });
          if (!report.ok) {
            throw new Error(report.message);
          }
          setSnapshotLoaded(false);
          setOnboardingInitialStep(1);
          setOnboardingKey((key) => key + 1);
          await refreshSnapshot();
          setInstallReview(null);
          setView("home");
          setShowOnboarding(true);
        }}
        onKeep={async () => {
          const report = await callCommand<ActionReport>("acknowledge_install_state", undefined, {
            ok: true,
            message: "AFS install state recorded.",
          });
          if (!report.ok) {
            throw new Error(report.message);
          }
          setInstallReview(null);
          await callCommand<ActionReport>("ensure_runtime_ready").catch(() => undefined);
          await refreshSnapshot();
        }}
      />
    );
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
      onResetComplete={() => {
        setOnboardingInitialStep(1);
        setOnboardingKey((key) => key + 1);
        setView("home");
        setShowOnboarding(true);
      }}
    />
  );
}

function StateResetPrompt({
  review,
  onReset,
  onKeep,
}: {
  review: InstallStateReview;
  onReset: () => Promise<void>;
  onKeep: () => Promise<void>;
}) {
  const [state, setState] = useState<"idle" | "resetting" | "keeping" | "error">("idle");
  const [message, setMessage] = useState("");

  async function run(action: "reset" | "keep") {
    setState(action === "reset" ? "resetting" : "keeping");
    setMessage("");
    try {
      await (action === "reset" ? onReset() : onKeep());
    } catch (error) {
      setState("error");
      setMessage(errorMessage(error));
    }
  }

  return (
    <main className="setup-shell">
      <section className="setup-window">
        <WindowChrome title="AFS Setup" meta="State Check" />
        <SetupContent mark={<BrandTile variant="folder" />}>
          <div>
            <div className="sync-note warning">
              <AlertTriangle />
              Previous install found
            </div>
            <h1>Start this beta with clean AFS state?</h1>
            <p>
              AFS found an existing local database from an earlier build. During the beta, resetting
              avoids mount and schema drift. Your local files and folders are left in place.
            </p>
          </div>
          <div className="state-reset-card">
            <SettingRow title="Local database" value={review.sqliteExists ? "~/.afs/state.sqlite3" : "Not found"} />
            <SettingRow title="Previous build" value={review.previousBuildId ?? "Unknown"} />
            <SettingRow title="Current build" value={review.currentBuildId} />
          </div>
          <div className="button-row">
            <PrimaryButton
              icon={state === "resetting" ? <Loader2 className="spin-icon" /> : <RotateCcw />}
              disabled={state === "resetting" || state === "keeping"}
              onClick={() => void run("reset")}
            >
              {state === "resetting" ? "Resetting" : "Reset AFS State"}
            </PrimaryButton>
            <SecondaryButton
              disabled={state === "resetting" || state === "keeping"}
              onClick={() => void run("keep")}
            >
              Keep Existing State
            </SecondaryButton>
          </div>
          <p className="quiet-note">
            Reset clears AFS metadata, cache, mount registration, and connector credentials. It does
            not delete documents outside AFS state.
          </p>
          {state === "error" && <p className="field-error">{message}</p>}
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
  const [mountError, setMountError] = useState("");

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
    const nextSnapshot = await callCommand<DesktopSnapshot>(
      "desktop_snapshot",
      undefined,
      sampleSnapshot,
    );
    setMountPathDirty(false);
    setMountPath(nextSnapshot.mount.localPath);
    setStep(4);
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
          localPath: "~/Library/CloudStorage/AFS/notion/Engineering/Roadmap 2026 ~a3f2.md",
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
              <div className="sync-note">
                <Sparkles />
                Setup complete
              </div>
              <h1>You’re ready to use AFS</h1>
              <p>
                Your Notion folder is ready. AFS will keep syncing the workspace quietly in the
                background.
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
            <PrimaryButton icon={<FolderOpen />} onClick={openFolderAndFinish}>
              Open Notion Folder
            </PrimaryButton>
            <p className="quiet-note">
              If Finder asks to enable the AFS File Provider, click Enable once. macOS requires
              this approval before showing the Notion files.
            </p>
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
                  Find the Q4 launch plan and make it sharper for leadership review.
                </div>
                <SecondaryButton
                  compact
                  icon={<Copy />}
                  onClick={() =>
                    copyText(
                      `In ${mountPath}, find the Q4 launch plan and make it sharper for leadership review. Keep the edits ready for AFS review.`,
                    )
                  }
                >
                  Copy
                </SecondaryButton>
              </div>
            </div>
          </SetupContent>
        )}
      </section>
    </main>
  );
}

function MainShell({
  snapshot,
  view,
  onViewChange,
  onRefresh,
  onResetComplete,
}: {
  snapshot: DesktopSnapshot;
  view: AppView;
  onViewChange: (view: AppView) => void;
  onRefresh: () => Promise<void>;
  onResetComplete: () => void;
}) {
  return (
    <main className="app-frame">
      <WindowChrome title="AFS" meta={snapshot.health.attentionCount > 0 ? "Pending Changes" : "Ready"} />
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
            <StatusPill tone={snapshot.health.attentionCount > 0 ? "warn" : "ready"}>
              {snapshot.health.attentionCount > 0 ? "Pending Changes" : "Notion Ready"}
            </StatusPill>
          </div>
        </aside>

        <section className="content">
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
              onRefresh={onRefresh}
              onReview={() => onViewChange("pending")}
            />
          )}
          {view === "pending" && <PendingView snapshot={snapshot} onReview={() => onViewChange("review")} />}
          {view === "review" && (
            <ReviewView
              snapshot={snapshot}
              onRefresh={onRefresh}
              onDone={() => onViewChange("activity")}
            />
          )}
          {view === "activity" && <ActivityView snapshot={snapshot} />}
          {view === "settings" && (
            <SettingsView
              snapshot={snapshot}
              onRefresh={onRefresh}
              onResetComplete={onResetComplete}
            />
          )}
        </section>
      </div>
    </main>
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
          localPath: "~/Library/CloudStorage/AFS/notion/Engineering/Roadmap 2026 ~a3f2.md",
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
        <StatusPill tone={healthTone(snapshot.health.state)}>{healthLabel(snapshot.health.state)}</StatusPill>
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
              <SecondaryButton icon={<FolderOpen />} onClick={() => void openWorkspaceFolder(snapshot.mount.localPath)}>
                Open Folder
              </SecondaryButton>
              <SecondaryButton icon={<ChevronRight />} onClick={onMount}>
                Mount Detail
              </SecondaryButton>
            </div>
          </section>
          {actionError && <p className="field-error">{actionError}</p>}

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
      <ViewHeader eyebrow="Mount" title={snapshot.mount.workspaceName}>
        <StatusPill tone={healthTone(snapshot.health.state)}>{healthLabel(snapshot.health.state)}</StatusPill>
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
          <PanelTitle title="Workspace" />
          <SettingRow title="Source" value="Notion" />
          <SettingRow title="Workspace" value={snapshot.connection.workspaceName} />
          <SettingRow title="Account" value={snapshot.connection.accountLabel || "Connected"} />
          <SettingRow title="Access" value={snapshot.mount.readOnly ? "Read Only" : "Edit enabled"} />
        </div>

        <div className="panel">
          <PanelTitle title="Local Files" />
          <SettingRow title="Location" value={snapshot.mount.localPath} />
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

function PendingView({ snapshot, onReview }: { snapshot: DesktopSnapshot; onReview: () => void }) {
  const hasPendingChanges = snapshot.pendingChanges.length > 0;

  return (
    <div className="view-stack">
      <ViewHeader eyebrow="Pending" title="Pending Changes">
        <PrimaryButton disabled={!hasPendingChanges} icon={<ListChecks />} onClick={onReview}>
          Review Push
        </PrimaryButton>
      </ViewHeader>
      {hasPendingChanges ? (
        <>
          <p className="view-copy">{snapshot.pendingChanges.length} files have pending changes.</p>
          <FileChangeList changes={snapshot.pendingChanges} mountPath={snapshot.mount.localPath} />
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
  onRefresh,
  onDone,
}: {
  snapshot: DesktopSnapshot;
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
      const report = await callCommand<ActionReport>("push_to_notion", undefined, {
        ok: true,
        message: "Pushed changes to Notion.",
      });
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
      <ViewHeader eyebrow="Review Push" title={plan.title}>
        <StatusPill tone={pushState === "error" ? "danger" : isPushing ? "warn" : "ready"}>
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

      <FileChangeList changes={plan.files} mountPath={snapshot.mount.localPath} />

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

function ActivityView({ snapshot }: { snapshot: DesktopSnapshot }) {
  const grouped = useMemo(() => {
    return snapshot.activity.reduce<Record<string, ActivityItem[]>>((acc, item) => {
      acc[item.when] = [...(acc[item.when] ?? []), item];
      return acc;
    }, {});
  }, [snapshot.activity]);

  return (
    <div className="view-stack">
      <ViewHeader eyebrow="Activity" title="Recent activity" />
      {Object.entries(grouped).map(([when, items]) => (
        <section className="activity-group" key={when}>
          <p className="label">{when}</p>
          {items.map((item) => (
            <article className="activity-item" key={`${when}-${item.title}`}>
              <Clock3 />
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

function SettingsView({
  snapshot,
  onRefresh,
  onResetComplete,
}: {
  snapshot: DesktopSnapshot;
  onRefresh: () => Promise<void>;
  onResetComplete: () => void;
}) {
  const [diagnosticMessage, setDiagnosticMessage] = useState("");
  const [settingsMessage, setSettingsMessage] = useState("");
  const [resetMessage, setResetMessage] = useState("");
  const [resettingState, setResettingState] = useState(false);
  const [busySetting, setBusySetting] = useState("");
  const [localSettings, setLocalSettings] = useState(snapshot.settings);
  const daemonStopped = snapshot.health.state === "stopped";

  useEffect(() => {
    setLocalSettings(snapshot.settings);
  }, [snapshot.settings.launchAtLogin, snapshot.settings.showMenuBar]);

  async function repairRuntime() {
    if (!daemonStopped) {
      return;
    }
    setDiagnosticMessage("");
    const report = await callCommand<ActionReport>(
      "ensure_runtime_ready",
      undefined,
      { ok: true, message: "AFS daemon is running." },
    );
    setDiagnosticMessage(report.message);
    await onRefresh().catch(() => undefined);
  }

  function copyDiagnostics() {
    const summary = [
      `AFS process: ${daemonStopped ? "Stopped" : "Running"}`,
      "State folder: ~/.afs",
      `Projection: ${snapshot.mount.projection}`,
      `Connection: ${snapshot.connection.status}`,
      `Mount: ${snapshot.mount.status}`,
      `Pending changes: ${snapshot.pendingChanges.length}`,
    ].join("\n");
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

  return (
    <div className="view-stack">
      <ViewHeader eyebrow="Settings" title="AFS controls" />

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
          <PanelTitle title="Diagnostics" />
          <SettingRow title="AFS process" value={daemonStopped ? "Stopped" : "Running"} />
          <SettingRow title="State folder" value="~/.afs" />
          <SettingRow title="Projection" value={snapshot.mount.projection} />
          <div className="button-row">
            <SecondaryButton compact onClick={copyDiagnostics}>
              Copy Summary
            </SecondaryButton>
            <SecondaryButton compact disabled={!daemonStopped} onClick={() => void repairRuntime()}>
              {daemonStopped ? "Start AFS" : "Repair AFS"}
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
  const { results: searchResults, searching } = useNotionSearchResults(url);
  const visibleChanges = snapshot.pendingChanges.slice(0, 3);
  const visibleSearchResults = locateState === "ready" ? [] : searchResults.slice(0, 3);

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
          localPath: "~/Library/CloudStorage/AFS/notion/Engineering/Roadmap 2026 ~a3f2.md",
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
        <StatusPill tone={healthTone(snapshot.health.state)}>{healthLabel(snapshot.health.state)}</StatusPill>
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
        <div className="tray-suggestion-copy">
          <strong>Connect {snapshot.suggestions[0]?.connector ?? "Linear"}</strong>
          <span>{snapshot.suggestions[0]?.description ?? "Mount more workspaces as local files."}</span>
        </div>
        <button disabled>Coming Soon</button>
      </section>

      <footer className="tray-footer">
        <button onClick={() => openMain("settings")}>Settings</button>
        <div className="tray-quit-options">
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

function FileChangeList({ changes, mountPath }: { changes: PendingChange[]; mountPath: string }) {
  return (
    <section className="file-list">
      {changes.map((change) => (
        <article className={`file-row ${change.state}`} key={change.localPath}>
          <div className="file-state">
            {change.state === "needs_review" ? <AlertTriangle /> : <Check />}
          </div>
          <div>
            <h3>{change.title}</h3>
            <p>{change.localPath}</p>
            <span>{change.summary}</span>
          </div>
          <SecondaryButton
            compact
            onClick={() => void callCommand("open_path", { path: joinMountPath(mountPath, change.localPath) }, { ok: true })}
          >
            Open
          </SecondaryButton>
        </article>
      ))}
    </section>
  );
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

function ViewHeader({
  eyebrow,
  title,
  children,
}: {
  eyebrow: string;
  title: string;
  children?: React.ReactNode;
}) {
  return (
    <header className="view-header">
      <div>
        <p className="eyebrow">{eyebrow}</p>
        <h1>{title}</h1>
      </div>
      {children}
    </header>
  );
}

function WindowChrome({ title, meta }: { title: string; meta?: string }) {
  return (
    <div className="window-chrome" onMouseDown={handleChromeMouseDown}>
      <div className="native-traffic-space" aria-hidden="true" />
      <div data-tauri-drag-region>{title}</div>
      <div data-tauri-drag-region>{meta}</div>
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

function StatusPill({ children, tone }: { children: React.ReactNode; tone: "ready" | "warn" | "danger" }) {
  return <span className={`status-pill ${tone}`}>{children}</span>;
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

function SettingRow({ title, value }: { title: string; value: string }) {
  return (
    <div className="setting-row">
      <span>{title}</span>
      <strong>{value}</strong>
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

function setupComplete(snapshot: DesktopSnapshot) {
  return !connectionMissing(snapshot) && !mountMissing(snapshot);
}

function isAppView(value: string): value is AppView {
  return value === "home" || value === "mount" || value === "pending" || value === "review" || value === "activity" || value === "settings";
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
  if (state === "checking_freshness") {
    return "Checking";
  }
  return "Ready";
}

function healthTone(state: string): "ready" | "warn" | "danger" {
  if (state === "reconnect_needed" || state === "stopped") {
    return "danger";
  }
  if (state === "needs_review") {
    return "warn";
  }
  return "ready";
}

function healthIconState(state: string): "default" | "review" | "reconnect" {
  if (state === "reconnect_needed" || state === "stopped") {
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

function joinMountPath(mountPath: string, relativePath: string) {
  if (relativePath.startsWith("/") || relativePath.startsWith("~/")) {
    return relativePath;
  }

  return `${mountPath.replace(/\/$/, "")}/${relativePath}`;
}

function copyText(value: string) {
  void navigator.clipboard?.writeText(value);
}
