import { readFileSync } from "node:fs";

import { describe, expect, it } from "vitest";

const appSource = readFileSync(new URL("./App.tsx", import.meta.url), "utf8");
const styles = readFileSync(new URL("./styles.css", import.meta.url), "utf8");

describe("dark theme UI contrast", () => {
  it("uses a stable title hook for the desktop chrome", () => {
    expect(appSource).toMatch(/className="window-title"/);
    expect(styles).toMatch(/:root\[data-theme="dark"\] \.window-chrome\s*\{/);
    expect(styles).toMatch(/:root\[data-theme="dark"\] \.window-title\s*\{[\s\S]*?color:\s*#f1f8f5;/s);
  });

  it("keeps sidebar status tooltips inside the app window", () => {
    expect(styles).toMatch(
      /\.sidebar-status \.status-pill\.has-tooltip:hover::after,[\s\S]*?bottom:\s*calc\(100% \+ 10px\);/s,
    );
    expect(styles).toMatch(
      /:root\[data-theme="dark"\] \.status-pill\.has-tooltip:hover::after,[\s\S]*?background:\s*#223035;/s,
    );
  });

  it("defines dark surfaces for home stats, Live Mode, and tray popover", () => {
    expect(styles).toMatch(/:root\[data-theme="dark"\] \.home-stat\s*\{/);
    expect(styles).toMatch(/:root\[data-theme="dark"\] \.live-mode-control\s*\{/);
    expect(styles).toMatch(/:root\[data-theme="dark"\] \.tray-popover\s*\{/);
    expect(styles).not.toMatch(/:root\[data-theme="dark"\] \.tray-live-mode-control,[\s\S]*?\.file-row\.expanded/s);
  });

  it("keeps disabled mount detail buttons from taking hover colors", () => {
    expect(styles).toMatch(/\.mount-details-button:hover:not\(:disabled\)\s*\{/);
    expect(styles).toMatch(
      /:root\[data-theme="dark"\] \.mount-details-button:hover:not\(:disabled\),/,
    );
  });
});
