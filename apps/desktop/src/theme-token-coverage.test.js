import { readFileSync } from "node:fs";

import { describe, expect, it } from "vitest";

const styles = readFileSync(new URL("./styles.css", import.meta.url), "utf8");

function cssBlock(selector) {
  const start = styles.indexOf(`${selector} {`);
  expect(start, `missing ${selector} block`).toBeGreaterThanOrEqual(0);

  const open = styles.indexOf("{", start);
  let depth = 0;
  for (let index = open; index < styles.length; index += 1) {
    const char = styles[index];
    if (char === "{") {
      depth += 1;
    }
    if (char === "}") {
      depth -= 1;
      if (depth === 0) {
        return styles.slice(open + 1, index);
      }
    }
  }

  throw new Error(`unterminated ${selector} block`);
}

function tokenValue(block, token) {
  const match = block.match(new RegExp(`${token}:\\s*([^;]+);`));
  expect(match, `missing ${token}`).not.toBeNull();
  return match?.[1].trim();
}

const roleTokens = [
  "--accent",
  "--accent-strong",
  "--accent-deep",
  "--accent-soft",
  "--accent-ink",
  "--app-bg",
  "--app-frame-bg",
  "--chrome-bg",
  "--sidebar-bg",
  "--sidebar-border",
  "--content-bg",
  "--nav-hover-bg",
  "--nav-active-bg",
  "--nav-active-text",
  "--surface",
  "--surface-hover",
  "--surface-active",
  "--surface-strong",
  "--surface-muted",
  "--surface-raised",
  "--surface-shadow",
  "--field-bg",
  "--field-bg-hover",
  "--control-bg",
  "--control-bg-hover",
  "--control-text",
  "--control-muted",
  "--control-border",
  "--control-border-hover",
  "--control-disabled-bg",
  "--control-disabled-text",
  "--control-selected-bg",
  "--control-selected-text",
  "--primary-button-bg",
  "--primary-button-hover-bg",
  "--primary-button-text",
  "--live-control-bg",
  "--live-control-hover-bg",
  "--live-control-active-bg",
  "--switch-track-bg",
  "--switch-track-border",
  "--switch-track-active-bg",
  "--switch-thumb-bg",
  "--switch-thumb-active-bg",
  "--switch-thumb-shadow",
  "--tooltip-bg",
  "--tooltip-text",
  "--modal-backdrop",
  "--status-ready-bg",
  "--status-ready-text",
  "--status-warn-bg",
  "--status-warn-text",
  "--status-danger-bg",
  "--status-danger-text",
  "--code-bg",
  "--focus-ring",
  "--chip-bg",
  "--chip-text",
  "--chip-border",
  "--onboarding-progress-bg",
  "--onboarding-ambient-bg",
  "--onboarding-demo-bg",
  "--onboarding-demo-panel-bg",
  "--onboarding-demo-line",
  "--onboarding-demo-text",
  "--onboarding-demo-muted",
  "--onboarding-demo-chip-bg",
  "--onboarding-demo-active-bg",
  "--onboarding-demo-active-text",
  "--onboarding-card-bg",
  "--onboarding-card-selected-bg",
  "--onboarding-ready-mark-bg",
];

describe("desktop theme tokens", () => {
  it("defines the same semantic role tokens for light and dark themes", () => {
    const light = cssBlock(":root");
    const dark = cssBlock(':root[data-theme="dark"]');

    for (const token of roleTokens) {
      expect(light).toContain(`${token}:`);
      expect(dark).toContain(`${token}:`);
    }
  });

  it("anchors dark mode to the charcoal and emerald reference palette", () => {
    const dark = cssBlock(':root[data-theme="dark"]');

    expect(tokenValue(dark, "--canvas")).toBe("#1c1d1c");
    expect(tokenValue(dark, "--paper")).toBe("#242625");
    expect(tokenValue(dark, "--wash")).toBe("#202221");
    expect(tokenValue(dark, "--ink")).toBe("#f4f5f0");
    expect(tokenValue(dark, "--accent")).toBe("#00d77c");
    expect(tokenValue(dark, "--app-frame-bg")).toBe("#1c1d1c");
    expect(tokenValue(dark, "--chrome-bg")).toBe("#1b1c1b");
    expect(tokenValue(dark, "--sidebar-bg")).toBe("#1c1d1c");
  });

  it("keeps app frame background tokens free of decorative gradients", () => {
    const light = cssBlock(":root");
    const dark = cssBlock(':root[data-theme="dark"]');
    const frameBlock = cssBlock(".app-frame,\n.setup-window");

    expect(tokenValue(light, "--app-frame-bg")).not.toMatch(/gradient/i);
    expect(tokenValue(dark, "--app-frame-bg")).not.toMatch(/gradient/i);
    expect(frameBlock).toContain("background: var(--app-frame-bg);");
    expect(frameBlock).not.toContain("radial-gradient");
    expect(styles).not.toMatch(/:root\[data-theme="dark"\] \.app-frame,[\s\S]*?radial-gradient/);
  });
});
