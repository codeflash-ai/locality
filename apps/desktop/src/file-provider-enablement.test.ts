import { afterEach, describe, expect, it, vi } from "vitest";
import {
  createFileProviderEnablementPoller,
  fileProviderEnablementHeadline,
  fileProviderEnablementStatusLabel,
  type FileProviderEnablementReport,
} from "./file-provider-enablement";

function report(
  state: FileProviderEnablementReport["state"],
): FileProviderEnablementReport {
  return { state, message: state, path: null };
}

afterEach(() => {
  vi.useRealTimers();
});

describe("File Provider enablement polling", () => {
  it("polls immediately and completes exactly once when the domain becomes ready", async () => {
    vi.useFakeTimers();
    const states = [report("needs_finder_enable"), report("ready")];
    const seen: string[] = [];
    let completions = 0;
    const poller = createFileProviderEnablementPoller({
      probe: async () => states.shift() ?? report("ready"),
      onReport: (next) => seen.push(next.state),
      onReady: () => {
        completions += 1;
      },
    });

    poller.start();
    await vi.advanceTimersByTimeAsync(0);
    expect(seen).toEqual(["needs_finder_enable"]);

    await vi.advanceTimersByTimeAsync(1_000);
    await vi.advanceTimersByTimeAsync(5_000);
    expect(seen).toEqual(["needs_finder_enable", "ready"]);
    expect(completions).toBe(1);
  });

  it("does not overlap probes while a previous request is pending", async () => {
    vi.useFakeTimers();
    let resolveProbe: ((value: FileProviderEnablementReport) => void) | undefined;
    let calls = 0;
    const poller = createFileProviderEnablementPoller({
      probe: () => {
        calls += 1;
        return new Promise((resolve) => {
          resolveProbe = resolve;
        });
      },
      onReport: () => undefined,
      onReady: () => undefined,
    });

    poller.start();
    await vi.advanceTimersByTimeAsync(10_000);
    expect(calls).toBe(1);

    resolveProbe?.(report("needs_finder_enable"));
    await vi.advanceTimersByTimeAsync(0);
    await vi.advanceTimersByTimeAsync(1_000);
    expect(calls).toBe(2);
  });

  it("drops a non-ready report that resolves after the poller stops", async () => {
    vi.useFakeTimers();
    let resolveProbe: ((value: FileProviderEnablementReport) => void) | undefined;
    const seen: string[] = [];
    const poller = createFileProviderEnablementPoller({
      probe: () => new Promise((resolve) => {
        resolveProbe = resolve;
      }),
      onReport: (next) => seen.push(next.state),
      onReady: () => undefined,
    });

    poller.start();
    await vi.advanceTimersByTimeAsync(0);
    poller.stop();
    resolveProbe?.(report("needs_finder_enable"));
    await vi.advanceTimersByTimeAsync(0);

    expect(seen).toEqual([]);
  });

  it("drops a ready result from an older run after stop and restart", async () => {
    vi.useFakeTimers();
    let resolveFirstProbe: ((value: FileProviderEnablementReport) => void) | undefined;
    let calls = 0;
    const seen: string[] = [];
    let completions = 0;
    const poller = createFileProviderEnablementPoller({
      probe: () => {
        calls += 1;
        if (calls === 1) {
          return new Promise((resolve) => {
            resolveFirstProbe = resolve;
          });
        }
        return Promise.resolve(report("needs_finder_enable"));
      },
      onReport: (next) => seen.push(next.state),
      onReady: () => {
        completions += 1;
      },
    });

    poller.start();
    await vi.advanceTimersByTimeAsync(0);
    poller.stop();
    poller.start();
    resolveFirstProbe?.(report("ready"));
    await vi.advanceTimersByTimeAsync(0);
    await vi.advanceTimersByTimeAsync(1_000);

    expect(seen).toEqual(["needs_finder_enable"]);
    expect(completions).toBe(0);
    expect(calls).toBe(2);
  });

  it("pauses while hidden and probes immediately when visible again", async () => {
    vi.useFakeTimers();
    let calls = 0;
    const poller = createFileProviderEnablementPoller({
      probe: async () => {
        calls += 1;
        return report("needs_finder_enable");
      },
      onReport: () => undefined,
      onReady: () => undefined,
    });

    poller.start();
    await vi.advanceTimersByTimeAsync(0);
    poller.setVisible(false);
    await vi.advanceTimersByTimeAsync(10_000);
    expect(calls).toBe(1);

    poller.setVisible(true);
    await vi.advanceTimersByTimeAsync(0);
    expect(calls).toBe(2);
  });

  it("backs transient failures off to five seconds", async () => {
    vi.useFakeTimers();
    let calls = 0;
    const poller = createFileProviderEnablementPoller({
      probe: async () => {
        calls += 1;
        throw new Error("temporary helper failure");
      },
      onReport: () => undefined,
      onReady: () => undefined,
    });

    poller.start();
    await vi.advanceTimersByTimeAsync(0);
    await vi.advanceTimersByTimeAsync(1_000);
    await vi.advanceTimersByTimeAsync(2_000);
    await vi.advanceTimersByTimeAsync(4_000);
    await vi.advanceTimersByTimeAsync(5_000);
    expect(calls).toBe(5);
  });
});

describe("File Provider enablement copy", () => {
  it("keeps the Finder action and automatic waiting state explicit", () => {
    expect(fileProviderEnablementHeadline(report("needs_finder_enable"))).toBe(
      "Enable Locality in Finder",
    );
    expect(fileProviderEnablementStatusLabel(report("needs_finder_enable"))).toBe(
      "Waiting for macOS",
    );
    expect(fileProviderEnablementHeadline(report("waiting_for_root"))).toBe(
      "Finishing folder setup",
    );
  });
});
