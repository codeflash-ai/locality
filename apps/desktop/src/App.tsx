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
  state: "ready" | "preparing" | "no_access" | "not_found";
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
    localPath: "~/Documents/AFS/Notion",
    projection: "macOS File Provider",
    readOnly: false,
    status: "ready",
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

export default function App() {
  const [snapshot, setSnapshot] = useState<DesktopSnapshot>(sampleSnapshot);
  const [view, setView] = useState<AppView>("home");
  const route = window.location.hash;
  const [showOnboarding, setShowOnboarding] = useState(() => route !== "#app" && route !== "#tray");

  async function refreshSnapshot() {
    const nextSnapshot = await callCommand<DesktopSnapshot>("desktop_snapshot", undefined, sampleSnapshot);
    setSnapshot(nextSnapshot);
  }

  useEffect(() => {
    void (async () => {
      if (isTauriRuntime()) {
        await callCommand<ActionReport>("ensure_runtime_ready").catch(() => undefined);
      }
      await refreshSnapshot();
    })().catch(() => setSnapshot(sampleSnapshot));
  }, []);

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
    document.body.dataset.surface = route === "#tray" ? "tray" : "app";
  }, [route]);

  if (route === "#tray") {
    return <TrayPopover snapshot={snapshot} />;
  }

  if (showOnboarding) {
    return (
      <Onboarding
        snapshot={snapshot}
        onComplete={() => {
          void refreshSnapshot().catch(() => undefined);
          setShowOnboarding(false);
          setView("home");
        }}
      />
    );
  }

  return <MainShell snapshot={snapshot} view={view} onViewChange={setView} onRefresh={refreshSnapshot} />;
}

function Onboarding({
  snapshot,
  onComplete,
}: {
  snapshot: DesktopSnapshot;
  onComplete: () => void;
}) {
  const [step, setStep] = useState(() => (window.location.hash === "#onboarding-ready" ? 4 : 1));
  const [oauthReady, setOauthReady] = useState(false);
  const [oauthInFlight, setOauthInFlight] = useState(false);
  const [oauthError, setOauthError] = useState("");
  const [mountPath, setMountPath] = useState(snapshot.mount.localPath);
  const [locateUrl, setLocateUrl] = useState("");
  const [locatedItem, setLocatedItem] = useState<LocatedItem | null>(null);
  const [locateState, setLocateState] = useState<LocateState>("idle");
  const [mountError, setMountError] = useState("");

  async function startConnect() {
    setOauthError("");
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
      setOauthReady(true);
    } catch (error) {
      setOauthError(errorMessage(error));
    } finally {
      setOauthInFlight(false);
    }
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
    try {
      const item = await callCommand<LocatedItem>(
        "locate_notion_page",
        { url: locateUrl },
        {
          title: "Roadmap 2026",
          kind: "Page",
          localPath: "~/Documents/AFS/Notion/Engineering/Roadmap 2026 ~a3f2.md",
          state: "ready",
        },
      );
      setLocatedItem(item);
      setLocateState("ready");
    } catch {
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
                Mount your Notion workspace in Documents. Agents edit local
                files, then AFS syncs reviewed changes back to Notion.
              </p>
            </div>
            <PrimaryButton onClick={startConnect}>Connect Notion</PrimaryButton>
            <p className="quiet-note">Local edits stay pending until you review and push.</p>
          </SetupContent>
        )}

        {step === 2 && (
          <SetupContent mark={<BrandTile variant="notion">N</BrandTile>}>
            <div>
              <h1>Finish connecting in Notion</h1>
              <p>
                A browser window is open. Choose your workspace, pick the pages
                AFS can use, then approve access.
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
              {oauthReady ? "Continue" : oauthInFlight ? "Waiting for Notion" : "Continue"}
            </PrimaryButton>
            <TextButton disabled={oauthInFlight} onClick={() => void startConnect()}>
              Open browser again
            </TextButton>
            {oauthError && <p className="field-error">{oauthError}</p>}
            <p className="quiet-note">Credentials are stored securely in the OS credential store.</p>
          </SetupContent>
        )}

        {step === 3 && (
          <SetupContent mark={<BrandTile variant="folder" />}>
            <div>
              <h1>Where should your Notion files appear?</h1>
              <p>AFS keeps the folder visible in Documents and organized under its own directory.</p>
            </div>
            <div className="path-field">
              <input value={mountPath} onChange={(event) => setMountPath(event.target.value)} />
              <SecondaryButton compact onClick={chooseFolder}>
                Choose
              </SecondaryButton>
            </div>
            <PrimaryButton disabled={!mountPath.trim()} onClick={startMount}>
              Continue
            </PrimaryButton>
            {mountError && <p className="field-error">{mountError}</p>}
            <p className="quiet-note">
              This folder will include AGENTS.md and CLAUDE.md to help your agents edit files
              natively.
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
                Your Notion folder is in Documents. AFS will keep syncing the workspace quietly in
                the background.
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
            <LocateBox
              label="Open a Notion page"
              value={locateUrl}
              onChange={setLocateUrl}
              onSubmit={locatePage}
              state={locateState}
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
}: {
  snapshot: DesktopSnapshot;
  view: AppView;
  onViewChange: (view: AppView) => void;
  onRefresh: () => Promise<void>;
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
          {view === "mount" && <MountDetailView snapshot={snapshot} onReview={() => onViewChange("pending")} />}
          {view === "pending" && <PendingView snapshot={snapshot} onReview={() => onViewChange("review")} />}
          {view === "review" && <ReviewView snapshot={snapshot} onDone={() => onViewChange("activity")} />}
          {view === "activity" && <ActivityView snapshot={snapshot} />}
          {view === "settings" && <SettingsView snapshot={snapshot} onRefresh={onRefresh} />}
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
    try {
      const item = await callCommand<LocatedItem>(
        "locate_notion_page",
        { url },
        {
          title: "Roadmap 2026",
          kind: "Page",
          localPath: "~/Documents/AFS/Notion/Engineering/Roadmap 2026 ~a3f2.md",
          state: "ready",
        },
      );
      setLocatedItem(item);
      setLocateState("ready");
    } catch {
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
            <p>Use the default visible location under Documents, or choose a different folder.</p>
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
              onChange={setUrl}
              onSubmit={locatePage}
              state={locateState}
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
        <SecondaryButton compact>Coming Soon</SecondaryButton>
      </section>
    </div>
  );
}

function MountDetailView({ snapshot, onReview }: { snapshot: DesktopSnapshot; onReview: () => void }) {
  const hasPendingChanges = snapshot.pendingChanges.length > 0;
  const [actionError, setActionError] = useState("");

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
        </div>
      </section>
      {actionError && <p className="field-error">{actionError}</p>}

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

function ReviewView({ snapshot, onDone }: { snapshot: DesktopSnapshot; onDone: () => void }) {
  const [plan, setPlan] = useState<PushPlan>(samplePushPlan);
  const [complete, setComplete] = useState(false);

  useEffect(() => {
    void callCommand<PushPlan>("review_push_plan", undefined, samplePushPlan)
      .then(setPlan)
      .catch(() => setPlan(samplePushPlan));
  }, []);

  async function push() {
    await callCommand("push_to_notion", undefined, { ok: true });
    setComplete(true);
  }

  if (complete) {
    return (
      <div className="center-result">
        <BrandTile variant="ready" />
        <h1>Pushed to Notion</h1>
        <p>3 files updated successfully.</p>
        <PrimaryButton onClick={onDone}>Done</PrimaryButton>
      </div>
    );
  }

  return (
    <div className="view-stack">
      <ViewHeader eyebrow="Review Push" title={plan.title}>
        <StatusPill tone="ready">Safe</StatusPill>
      </ViewHeader>
      <p className="view-copy">{plan.summary}</p>

      <section className="summary-grid">
        <Metric label="Pages updated" value={plan.pagesUpdated} />
        <Metric label="Database rows updated" value={plan.databaseRowsUpdated} />
        <Metric label="Pages deleted" value={plan.pagesDeleted} />
      </section>

      <FileChangeList changes={plan.files} mountPath={snapshot.mount.localPath} />

      <div className="footer-actions">
        <PrimaryButton disabled={!plan.canPush} icon={<ShieldCheck />} onClick={push}>
          Push to Notion
        </PrimaryButton>
        <SecondaryButton>Cancel</SecondaryButton>
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
}: {
  snapshot: DesktopSnapshot;
  onRefresh: () => Promise<void>;
}) {
  const [diagnosticMessage, setDiagnosticMessage] = useState("");
  const daemonStopped = snapshot.health.state === "stopped";

  async function repairRuntime() {
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

  return (
    <div className="view-stack">
      <ViewHeader eyebrow="Settings" title="AFS controls" />

      <section className="settings-grid">
        <div className="panel">
          <PanelTitle title="Startup" />
          <ToggleRow title="Launch AFS at login" enabled />
          <ToggleRow title="Show AFS in the menu bar" enabled />
          <SettingRow title="Default folder" value="~/Documents/AFS" />
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
            <SecondaryButton compact onClick={() => void repairRuntime()}>
              {daemonStopped ? "Start AFS" : "Repair AFS"}
            </SecondaryButton>
          </div>
          {diagnosticMessage && <p className="quiet-note inline-note">{diagnosticMessage}</p>}
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
  const [locatedItem, setLocatedItem] = useState<LocatedItem | null>(null);
  const visibleChanges = snapshot.pendingChanges.slice(0, 3);

  async function locatePage() {
    if (!url.trim()) {
      return;
    }

    setLocateState("preparing");
    try {
      const item = await callCommand<LocatedItem>(
        "locate_notion_page",
        { url },
        {
          title: "Roadmap 2026",
          kind: "Page",
          localPath: "~/Documents/AFS/Notion/Engineering/Roadmap 2026 ~a3f2.md",
          state: "ready",
        },
      );
      setLocatedItem(item);
      setLocateState("ready");
    } catch {
      setLocatedItem(null);
      setLocateState("error");
    }
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
            placeholder="Paste Notion URL"
            onChange={(event) => setUrl(event.target.value)}
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
        {locateState === "error" && <p className="field-error">Paste a Notion page or database URL.</p>}
        {locatedItem && (
          <div className="tray-result">
            <strong>{locatedItem.title}</strong>
            <code>{locatedItem.localPath}</code>
            <button onClick={() => copyText(locatedItem.localPath)}>Copy Path</button>
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
        <button onClick={() => openMain("settings")}>Connect Linear</button>
      </section>

      <footer className="tray-footer">
        <button onClick={() => openMain("settings")}>Settings</button>
        <details>
          <summary>Quit Options</summary>
          <button onClick={() => void callCommand("hide_menubar", undefined, { ok: true })}>
            Don't Show in Menubar
          </button>
          <button className="danger" onClick={() => void callCommand("quit_completely", undefined, { ok: true })}>
            Quit Completely
          </button>
        </details>
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
  state,
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  onSubmit: () => void;
  state: LocateState;
}) {
  return (
    <div className="locate-box">
      <label>{label}</label>
      <div className="locate-row">
        <Search />
        <input
          value={value}
          placeholder="Paste a Notion URL to get the local file path"
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
      {state === "error" && <p className="field-error">Paste a Notion page or database URL.</p>}
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
        <SecondaryButton compact icon={<FolderOpen />} onClick={() => void callCommand("open_path", { path: item.localPath }, { ok: true })}>
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
      <div className="traffic">
        <button aria-label="Close window" className="traffic-dot close" onClick={() => void windowAction("hide")} />
        <button aria-label="Minimize window" className="traffic-dot minimize" onClick={() => void windowAction("minimize")} />
        <button aria-label="Toggle fullscreen" className="traffic-dot zoom" onClick={() => void windowAction("toggleMaximize")} />
      </div>
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

async function windowAction(action: "hide" | "minimize" | "toggleMaximize") {
  if (!isTauriRuntime()) {
    return;
  }

  const currentWindow = getCurrentWindow();
  if (action === "hide") {
    await currentWindow.hide();
  } else if (action === "minimize") {
    await currentWindow.minimize();
  } else {
    await currentWindow.toggleMaximize();
  }
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

function ToggleRow({ title, enabled }: { title: string; enabled: boolean }) {
  return (
    <div className="setting-row">
      <span>{title}</span>
      <button className={`toggle ${enabled ? "enabled" : ""}`} aria-label={title}>
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

function joinMountPath(mountPath: string, relativePath: string) {
  if (relativePath.startsWith("/") || relativePath.startsWith("~/")) {
    return relativePath;
  }

  return `${mountPath.replace(/\/$/, "")}/${relativePath}`;
}

function copyText(value: string) {
  void navigator.clipboard?.writeText(value);
}
