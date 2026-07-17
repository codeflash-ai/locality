export type FileProviderEnablementState =
  | "not_registered"
  | "needs_finder_enable"
  | "waiting_for_root"
  | "ready"
  | "unavailable";

export type FileProviderEnablementReport = {
  state: FileProviderEnablementState;
  message: string;
  path: string | null;
};

type PollerOptions = {
  probe: () => Promise<FileProviderEnablementReport>;
  onReport: (report: FileProviderEnablementReport) => void;
  onReady: (report: FileProviderEnablementReport) => void;
};

export type FileProviderEnablementPoller = {
  start: () => void;
  stop: () => void;
  setVisible: (visible: boolean) => void;
};

export function createFileProviderEnablementPoller(
  options: PollerOptions,
): FileProviderEnablementPoller {
  let running = false;
  let visible = true;
  let inFlight = false;
  let transientFailures = 0;
  let generation = 0;
  let timer: ReturnType<typeof setTimeout> | null = null;

  function clearTimer() {
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
  }

  function schedule(delay: number) {
    clearTimer();
    if (!running || !visible || inFlight) {
      return;
    }
    timer = setTimeout(() => {
      timer = null;
      void poll();
    }, delay);
  }

  async function poll() {
    if (!running || !visible || inFlight) {
      return;
    }
    const pollGeneration = generation;
    inFlight = true;
    try {
      const next = await options.probe();
      if (!running || generation !== pollGeneration) {
        return;
      }
      transientFailures = 0;
      options.onReport(next);
      if (next.state === "ready") {
        running = false;
        options.onReady(next);
        return;
      }
      if (next.state === "unavailable") {
        running = false;
        return;
      }
      schedule(1_000);
    } catch {
      if (!running || generation !== pollGeneration) {
        return;
      }
      transientFailures += 1;
      schedule(Math.min(1_000 * 2 ** (transientFailures - 1), 5_000));
    } finally {
      inFlight = false;
      if (running && visible && timer === null) {
        const delay = transientFailures === 0
          ? 1_000
          : Math.min(1_000 * 2 ** (transientFailures - 1), 5_000);
        schedule(delay);
      }
    }
  }

  return {
    start: () => {
      if (running) {
        return;
      }
      generation += 1;
      running = true;
      schedule(0);
    },
    stop: () => {
      generation += 1;
      running = false;
      clearTimer();
    },
    setVisible: (nextVisible) => {
      if (visible === nextVisible) {
        return;
      }
      visible = nextVisible;
      if (!visible) {
        clearTimer();
      } else if (running) {
        schedule(0);
      }
    },
  };
}

export function fileProviderEnablementHeadline(report: FileProviderEnablementReport): string {
  switch (report.state) {
    case "needs_finder_enable":
      return "Enable Locality in Finder";
    case "waiting_for_root":
      return "Finishing folder setup";
    case "unavailable":
      return "Folder setup needs attention";
    case "ready":
      return "Locality is enabled";
    default:
      return "Preparing Locality in Finder";
  }
}

export function fileProviderEnablementStatusLabel(report: FileProviderEnablementReport): string {
  switch (report.state) {
    case "needs_finder_enable":
      return "Waiting for macOS";
    case "waiting_for_root":
      return "Finishing folder setup";
    case "unavailable":
      return "Setup needs attention";
    case "ready":
      return "Locality enabled";
    default:
      return "Preparing Finder location";
  }
}
