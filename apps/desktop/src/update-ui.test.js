import { readFileSync } from "node:fs";

import { describe, expect, it } from "vitest";

const appSource = readFileSync(new URL("./App.tsx", import.meta.url), "utf8");
const styles = readFileSync(new URL("./styles.css", import.meta.url), "utf8");

describe("update notification UI", () => {
  it("keeps update prompts in the sidebar instead of a content banner", () => {
    expect(appSource).not.toMatch(/<UpdateBanner\b/);
    expect(appSource).not.toMatch(/function UpdateBanner\b/);
    expect(styles).not.toMatch(/\.update-banner\b/);
  });

  it("uses a compact sidebar update tile without secondary details controls", () => {
    expect(styles).toMatch(/\.sidebar-update\s*\{[\s\S]*?display:\s*flex;/s);
    expect(styles).toMatch(/\.sidebar-update\s*\{[\s\S]*?width:\s*calc\(100% - 8px\);/s);
    expect(styles).toMatch(/\.sidebar-update \+ \.sidebar-status\s*\{[\s\S]*?margin-top:\s*0;/s);
    expect(appSource).not.toMatch(/sidebar-update-icon/);
    expect(styles).not.toMatch(/\.sidebar-update-title\b/);
    expect(styles).not.toMatch(/\.sidebar-update-action\b/);
    expect(styles).not.toMatch(/\.sidebar-update-icon\b/);
    expect(styles).not.toMatch(
      /\.sidebar-update-copy strong,\s*\.sidebar-update-copy small\s*\{[^}]*text-overflow:\s*ellipsis;/s,
    );
    expect(styles).toMatch(
      /\.sidebar-update-copy strong,\s*\.sidebar-update-copy small\s*\{[^}]*white-space:\s*normal;/s,
    );
  });

  it("schedules a native relaunch fallback before installing an update", () => {
    expect(appSource).toMatch(/callCommand<ActionReport>\("schedule_update_relaunch"/);
    expect(appSource).toMatch(
      /const relaunchFallback = await scheduleUpdateRelaunchFallback\(\);[\s\S]*?await update\.install\(\);/s,
    );
  });
});
