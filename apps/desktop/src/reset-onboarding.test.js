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
