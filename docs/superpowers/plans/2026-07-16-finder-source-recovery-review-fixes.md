# Finder Source Recovery Review Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate the four reviewed Finder recovery and prompt-test installer regressions without changing the intended onboarding experience.

**Architecture:** Keep source recovery owned by `MountsView` independently of dialog visibility, express automatic retry outcomes with a small pure source-setup helper, and invalidate stopped poller generations before they can publish asynchronous results. Resolve installer inputs with explicit source options ahead of discovered build artifacts.

**Tech Stack:** React 18, TypeScript, Vitest, Bash, macOS File Provider test harness

---

## File Structure

- `apps/desktop/src/file-provider-enablement.ts`: generation-safe readiness poller.
- `apps/desktop/src/file-provider-enablement.test.ts`: asynchronous stop and restart regressions.
- `apps/desktop/src/source-setup.ts`: pure classification of automatic mount retry outcomes.
- `apps/desktop/src/source-setup.test.ts`: success, repeated-disablement, and terminal-error outcome tests.
- `apps/desktop/src/App.tsx`: dialog visibility and recovery lifecycle integration.
- `apps/desktop/src/file-provider-enablement-ui.test.js`: exact Add Source close-handler contract.
- `scripts/install-macos-prompt-test-app.sh`: explicit input and discovered artifact precedence.
- `scripts/install-macos-prompt-test-app.test.sh`: isolated dry-run source-selection regression.

### Task 1: Drop Results From Stopped Poller Generations

**Files:**
- Modify: `apps/desktop/src/file-provider-enablement.test.ts`
- Modify: `apps/desktop/src/file-provider-enablement.ts`

- [ ] **Step 1: Add the failing late-result tests**

Add these tests inside `describe("File Provider enablement polling", ...)`:

```ts
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
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```bash
cd apps/desktop
npm test -- --run src/file-provider-enablement.test.ts
```

Expected: the new tests fail because the first stopped run still calls
`onReport`, and its ready result still calls `onReady` after restart.

- [ ] **Step 3: Invalidate and check poller generations**

In `createFileProviderEnablementPoller`, add `let generation = 0;` beside the
other lifecycle fields. Capture it before awaiting the probe and reject stale
results in both resolution paths:

```ts
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
```

Advance the generation for each new run and every stop:

```ts
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
```

- [ ] **Step 4: Run the focused tests and verify GREEN**

Run:

```bash
cd apps/desktop
npm test -- --run src/file-provider-enablement.test.ts
```

Expected: all File Provider enablement tests pass, including both late-result
regressions.

- [ ] **Step 5: Commit the poller fix**

```bash
git add apps/desktop/src/file-provider-enablement.ts apps/desktop/src/file-provider-enablement.test.ts
git commit -m "fix: discard stopped File Provider probes"
```

### Task 2: Make Source Recovery Close And Retry Outcomes Terminally Consistent

**Files:**
- Modify: `apps/desktop/src/source-setup.test.ts`
- Modify: `apps/desktop/src/source-setup.ts`
- Modify: `apps/desktop/src/file-provider-enablement-ui.test.js`
- Modify: `apps/desktop/src/App.tsx`

- [ ] **Step 1: Add failing automatic-retry outcome tests**

Import `sourceMountRetryOutcome` in `source-setup.test.ts`, then add:

```ts
describe("source File Provider mount retry", () => {
  it("completes a successful automatic mount retry", () => {
    expect(sourceMountRetryOutcome({ ok: true, message: "Mounted Notion." })).toEqual({
      kind: "success",
      message: "Mounted Notion.",
    });
  });

  it("continues recovery when File Provider is still disabled", () => {
    expect(sourceMountRetryOutcome({
      ok: false,
      message: "The Locality File Provider is registered but not enabled.",
    })).toEqual({ kind: "retry" });
  });

  it("turns another automatic mount failure into a visible dialog error", () => {
    expect(sourceMountRetryOutcome({
      ok: false,
      message: "Could not load the top-level Notion folder.",
    })).toEqual({
      kind: "error",
      message: "Could not load the top-level Notion folder.",
    });
  });
});
```

- [ ] **Step 2: Tighten the Add Source close-handler contract**

Replace the broad later-source recovery assertion in
`file-provider-enablement-ui.test.js` with a full handler assertion:

```js
  it("keeps later source recovery running when its dialog closes", () => {
    const addSourceDialog = app.match(
      /<AddSourceDialog[\s\S]*?fileProviderEnablement=\{sourceFileProviderEnablement\}[\s\S]*?onClose=\{\(\) => \{([\s\S]*?)\}\}\s*\/>/,
    );

    expect(addSourceDialog?.[1].trim()).toBe("setSourceDialogOpen(false);");
  });
```

Keep a separate integration-contract assertion that the component receives the
recovery report:

```js
  it("passes later source recovery state into the guided dialog", () => {
    expect(app).toContain("fileProviderEnablement={sourceFileProviderEnablement}");
  });
```

- [ ] **Step 3: Run the focused tests and verify RED**

Run:

```bash
cd apps/desktop
npm test -- --run src/source-setup.test.ts src/file-provider-enablement-ui.test.js
```

Expected: `sourceMountRetryOutcome` is missing, and the close-handler assertion
reports the existing pending-retry and readiness-state cancellation calls.

- [ ] **Step 4: Implement the pure retry outcome**

Add this import and helper to `source-setup.ts`:

```ts
import { classifyMountSetupError } from "./onboarding-errors";

export type SourceMountRetryOutcome =
  | { kind: "retry" }
  | { kind: "success" | "error"; message: string };

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
```

- [ ] **Step 5: Apply the retry outcome in `MountsView`**

Import `sourceMountRetryOutcome` from `./source-setup`. Replace the success-only
completion branch inside the delayed automatic retry with:

```ts
          const outcome = sourceMountRetryOutcome(mountReport);
          if (outcome.kind === "retry") {
            return;
          }
          setPendingMountRetry(null);
          setSourceFileProviderEnablement(null);
          setSourceDialogMessage(outcome.message);
          setSourceDialogState(outcome.kind);
```

This relies on `createConnectorMount` to have already called
`beginSourceFileProviderRecovery` for the repeated disabled-provider result.
For success and terminal error, it clears recovery and makes the result visible.

- [ ] **Step 6: Make dialog close visibility-only**

Replace the Add Source dialog close callback with:

```tsx
          onClose={() => {
            setSourceDialogOpen(false);
          }}
```

Do not clear `pendingMountRetry`, `sourceFileProviderEnablement`,
`sourceDialogState`, or the active connector in this callback.

- [ ] **Step 7: Run the focused tests and verify GREEN**

Run:

```bash
cd apps/desktop
npm test -- --run src/source-setup.test.ts src/file-provider-enablement-ui.test.js
```

Expected: all source retry outcome and exact close-handler tests pass.

- [ ] **Step 8: Commit the source recovery fix**

```bash
git add apps/desktop/src/App.tsx apps/desktop/src/source-setup.ts apps/desktop/src/source-setup.test.ts apps/desktop/src/file-provider-enablement-ui.test.js
git commit -m "fix: keep Finder source recovery consistent"
```

### Task 3: Honor An Explicit Prompt-Test DMG

**Files:**
- Modify: `scripts/install-macos-prompt-test-app.test.sh`
- Modify: `scripts/install-macos-prompt-test-app.sh`

- [ ] **Step 1: Add an isolated source-precedence regression test**

Add this test before `main()` in `install-macos-prompt-test-app.test.sh`:

```bash
test_dry_run_prefers_explicit_dmg_over_built_app() {
  local tmp isolated_root isolated_script built_app dmg app output
  tmp="$(mktemp -d)"
  trap '[[ -z "${tmp:-}" ]] || rm -rf "${tmp}"' RETURN
  isolated_root="${tmp}/repo"
  isolated_script="${isolated_root}/scripts/install-macos-prompt-test-app.sh"
  built_app="${isolated_root}/target/release/bundle/macos/Locality.app"
  dmg="${tmp}/explicit.dmg"
  app="${tmp}/Applications/Locality Prompt Test.app"
  mkdir -p "$(dirname "${isolated_script}")" "${built_app}"
  cp "${SCRIPT}" "${isolated_script}"
  touch "${dmg}"

  output="$(
    LOCALITY_PROMPT_TEST_TIMESTAMP=20260714130007 \
      "${isolated_script}" \
        --dry-run \
        --dmg "${dmg}" \
        --app-path "${app}" \
        --signing-identity "Developer ID Application: Test (TEAMID)" \
        --no-launch
  )"

  assert_contains "${output}" "+ hdiutil attach ${dmg}"
  assert_not_contains "${output}" "source app: ${built_app}"
}
```

Add `test_dry_run_prefers_explicit_dmg_over_built_app` to `main()`.

- [ ] **Step 2: Run the installer helper test and verify RED**

Run:

```bash
bash scripts/install-macos-prompt-test-app.test.sh
```

Expected: the new test fails with a missing `hdiutil attach` line because the
isolated built app is selected before the explicit DMG.

- [ ] **Step 3: Put explicit DMG selection before built-app discovery**

Restructure `resolve_source_app()` after the explicit `SOURCE_APP` branch:

```bash
  if [[ -z "${DMG}" ]]; then
    local bundle_app="${ROOT}/target/release/bundle/macos/Locality.app"
    if [[ -d "${bundle_app}" ]]; then
      SOURCE_APP="${bundle_app}"
      validate_source_and_target_paths
      return 0
    fi

    DMG="$(default_dmg)" || fail "missing DMG under ${DEFAULT_DMG_DIR}. Run make build-tauri first or pass --dmg PATH."
    DMG="$(expand_tilde "${DMG}")"
  fi
  [[ -f "${DMG}" ]] || fail "missing DMG: ${DMG}. Run make build-tauri first or pass --dmg PATH."
```

Keep the existing DMG mount and `Locality.app` validation code unchanged after
this block. Explicit `SOURCE_APP` remains the first branch, while a non-empty
explicit `DMG` now bypasses built-app discovery.

- [ ] **Step 4: Run the installer helper test and verify GREEN**

Run:

```bash
bash scripts/install-macos-prompt-test-app.test.sh
```

Expected: `install-macos-prompt-test-app helper tests passed`.

- [ ] **Step 5: Commit the installer fix**

```bash
git add scripts/install-macos-prompt-test-app.sh scripts/install-macos-prompt-test-app.test.sh
git commit -m "fix: honor explicit prompt-test DMG"
```

### Task 4: Verify The Review Fixes Together

**Files:**
- Verify: `apps/desktop/src/App.tsx`
- Verify: `apps/desktop/src/file-provider-enablement.ts`
- Verify: `apps/desktop/src/source-setup.ts`
- Verify: `scripts/install-macos-prompt-test-app.sh`

- [ ] **Step 1: Run all desktop frontend tests**

```bash
cd apps/desktop
npm test -- --run
```

Expected: the complete Vitest suite passes with zero failures.

- [ ] **Step 2: Run the production frontend build**

```bash
cd apps/desktop
npm run build
```

Expected: TypeScript compilation and the Vite production build exit zero.

- [ ] **Step 3: Run the installer helper through its Make target**

```bash
make test-macos-prompt-test-app-installer
```

Expected: `install-macos-prompt-test-app helper tests passed`.

- [ ] **Step 4: Check formatting and the final patch**

```bash
cargo fmt --all -- --check
git diff --check HEAD~3..HEAD
git status --short
```

Expected: formatting and diff checks exit zero. The worktree has no uncommitted
changes, and the three implementation commits contain only the planned files.

- [ ] **Step 5: Re-read the four review requirements**

Confirm directly from the final code and tests:

1. Closing the source dialog no longer cancels recovery or leaves a canceled
   `creating` state.
2. Every automatic mount report reaches success, renewed provider recovery, or
   a visible terminal error.
3. A stopped or superseded poll generation cannot call `onReport` or `onReady`.
4. Explicit `--dmg` selection bypasses an existing default built app.

No additional commit is needed unless verification uncovers a defect; any such
defect must restart the relevant red-green task before completion is claimed.
