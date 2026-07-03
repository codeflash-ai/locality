import { readFileSync } from "node:fs";

import { describe, expect, it } from "vitest";

const styles = readFileSync(new URL("./styles.css", import.meta.url), "utf8");

describe("onboarding layout styles", () => {
  it("keeps the onboarding window chrome fixed while the content scrolls", () => {
    expect(styles).toMatch(/\.setup-window\s*\{\s*display:\s*grid;\s*grid-template-rows:\s*auto minmax\(0, 1fr\);\s*\}/s);
    expect(styles).toMatch(
      /\.setup-window > \.setup-content\s*\{\s*min-height:\s*0;\s*overflow-y:\s*auto;\s*overflow-x:\s*hidden;\s*overscroll-behavior:\s*contain;\s*scrollbar-gutter:\s*stable;\s*\}/s,
    );
  });

  it("allows the ready-screen prompt copy to wrap when the mount path is long", () => {
    expect(styles).toMatch(/\.agent-demo-command\s*\{[\s\S]*?overflow-wrap:\s*anywhere;/s);
  });
});
