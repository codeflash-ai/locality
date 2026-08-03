import { readFileSync } from "node:fs";

import { describe, expect, it } from "vitest";

const appSource = readFileSync(new URL("./App.tsx", import.meta.url), "utf8");
const tauriSource = readFileSync(new URL("../src-tauri/src/main.rs", import.meta.url), "utf8");
const cliSource = readFileSync(new URL("../../../crates/loc-cli/src/commands.rs", import.meta.url), "utf8");

function sourceSlice(source, startNeedle, endNeedle) {
  const start = source.indexOf(startNeedle);
  expect(start, `missing ${startNeedle}`).toBeGreaterThanOrEqual(0);
  const end = source.indexOf(endNeedle, start);
  expect(end, `missing ${endNeedle}`).toBeGreaterThan(start);
  return source.slice(start, end);
}

describe("reset onboarding state", () => {
  it("stores onboarding completion in resettable desktop state", () => {
    expect(appSource).toContain("onboardingCompleted: boolean;");
    expect(appSource).toContain(
      "const effectiveOnboardingCompleted = isTauriRuntime() ? snapshot.onboardingCompleted : onboardingCompleted;",
    );
    expect(appSource).toContain('callCommand<ActionReport>("complete_onboarding"');
    expect(appSource).toContain(
      "routeShouldShowOnboarding(route, snapshot, effectiveOnboardingCompleted)",
    );

    const snapshotStruct = sourceSlice(
      tauriSource,
      "struct DesktopSnapshot {",
      "#[derive(Clone, Serialize)]\n#[serde(rename_all = \"camelCase\")]\nstruct AppHealth",
    );
    expect(snapshotStruct).toContain("onboarding_completed: bool,");

    const settingsStruct = sourceSlice(
      tauriSource,
      "struct DesktopSettings {",
      "impl Default for DesktopSettings",
    );
    expect(settingsStruct).toContain("#[serde(default)]");
    expect(settingsStruct).toContain("onboarding_completed: bool,");

    const snapshotLoader = sourceSlice(
      tauriSource,
      "fn load_desktop_snapshot_from_store(",
      "fn degraded_snapshot(message: String) -> DesktopSnapshot",
    );
    expect(snapshotLoader).toContain("let settings = desktop_settings();");
    expect(snapshotLoader).toContain("onboarding_completed: settings.onboarding_completed,");
    expect(snapshotLoader).toContain("settings,");

    expect(tauriSource).toContain("async fn complete_onboarding");
    expect(tauriSource).toContain("settings.onboarding_completed = true;");
    expect(tauriSource).toContain("complete_onboarding,");
  });

  it("clears the desktop onboarding completion marker after a successful reset", () => {
    expect(appSource).toContain("function clearOnboardingCompleted()");
    expect(appSource).toContain("window.localStorage.removeItem(ONBOARDING_COMPLETED_STORAGE_KEY);");

    const appResetComplete = sourceSlice(
      appSource,
      "onResetComplete={() => {",
      "function SetupLoading()",
    );
    expect(appResetComplete).toContain("clearOnboardingCompleted();");
    expect(appResetComplete).toContain("setOnboardingCompleted(false);");

    const resetLocalState = sourceSlice(
      appSource,
      "async function resetLocalState()",
      "async function prepareUninstall()",
    );
    expect(resetLocalState).toContain("onResetComplete();");
  });

  it("tells users that reset opens onboarding again", () => {
    const destructiveDialog = sourceSlice(
      appSource,
      "function DestructiveSettingsDialog({",
      "function TrayPopover({",
    );
    expect(destructiveDialog).toContain("Locality opens onboarding again after cleanup.");
  });

  it("removes macOS WebKit desktop data when reset clears support state", () => {
    const desktopReset = sourceSlice(
      tauriSource,
      "fn remove_desktop_support_state()",
      "#[cfg(target_os = \"macos\")]\nfn remove_path_if_exists",
    );
    expect(desktopReset).toContain('home.join("Library/WebKit/ai.codeflash.locality")');

    const cliReset = sourceSlice(
      cliSource,
      "fn remove_desktop_support_state_for_reset()",
      "#[cfg(target_os = \"macos\")]\nfn remove_path_if_exists",
    );
    expect(cliReset).toContain('home.join("Library/WebKit/ai.codeflash.locality")');
  });
});
