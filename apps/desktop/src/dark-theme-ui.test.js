import { readFileSync } from "node:fs";

import { describe, expect, it } from "vitest";

const appSource = readFileSync(new URL("./App.tsx", import.meta.url), "utf8");
const styles = readFileSync(new URL("./styles.css", import.meta.url), "utf8");

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function cssBlock(selector) {
  const match = styles.match(new RegExp(`(^|\\n)${escapeRegExp(selector)}\\s*\\{`));
  const start = match?.index ?? -1;
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

function expectTokenizedColors(block, properties) {
  for (const property of properties) {
    const match = block.match(new RegExp(`${property}:\\s*([^;]+);`));
    expect(match, `missing ${property}`).not.toBeNull();
    expect(match?.[1].trim(), `${property} should use a semantic token`).toMatch(/^var\(--/);
  }
}

function expectNoRawSurfaceColors(block) {
  expect(block).not.toMatch(/background:\s*(?:#|rgba?\(|linear-gradient)/);
  expect(block).not.toMatch(/border(?:-color)?:\s*(?:#|rgba?\()/);
  expect(block).not.toMatch(/color:\s*#[0-9a-fA-F]+/);
}

describe("desktop dark theme UI", () => {
  it("keeps the semantic theme blocks in one designer-facing section", () => {
    expect(styles).toMatch(/Designer-facing theme tokens live here/);
    expect(styles).toMatch(/:root\s*\{[\s\S]*?--app-frame-bg:[\s\S]*?--status-danger-text:[\s\S]*?--focus-ring:/s);
    expect(styles).toMatch(
      /:root\[data-theme="dark"\]\s*\{[\s\S]*?--app-frame-bg:\s*#1c1d1c;[\s\S]*?--status-danger-text:[\s\S]*?--focus-ring:/s,
    );
  });

  it("uses neutral shell tokens for the desktop frame, chrome, and sidebar", () => {
    expect(styles).toMatch(/\.app-frame,\s*\.setup-window\s*\{[\s\S]*?background:\s*var\(--app-frame-bg\);/s);
    expect(styles).toMatch(/\.window-chrome\s*\{[\s\S]*?background:\s*var\(--chrome-bg\);[\s\S]*?color:\s*var\(--muted\);/s);
    expect(styles).toMatch(/\.window-title\s*\{[\s\S]*?color:\s*var\(--ink\);/s);
    expect(styles).toMatch(/\.sidebar\s*\{[\s\S]*?border-right:\s*1px solid var\(--sidebar-border\);[\s\S]*?background:\s*var\(--sidebar-bg\);/s);
    expect(styles).toMatch(/\.sidebar-link\.active,\s*\.sidebar-link:hover\s*\{[\s\S]*?background:\s*var\(--nav-active-bg\);[\s\S]*?color:\s*var\(--nav-active-text\);/s);
  });

  it("keeps the desktop chrome title hooked and theme-neutral", () => {
    expect(appSource).toMatch(/className="window-title"/);
    expect(styles).toMatch(/\.window-title\s*\{[\s\S]*?text-overflow:\s*ellipsis;[\s\S]*?white-space:\s*nowrap;/s);
    expect(styles).not.toMatch(/:root\[data-theme="dark"\] \.window-title\s*\{[\s\S]*?color:\s*#[0-9a-fA-F]+;/s);
  });

  it("keeps sidebar status tooltips inside the app window", () => {
    expect(styles).toMatch(
      /\.sidebar-status \.status-pill\.has-tooltip:hover::after,[\s\S]*?bottom:\s*calc\(100% \+ 10px\);/s,
    );
    expect(styles).toMatch(
      /:root\[data-theme="dark"\] \.status-pill\.has-tooltip:hover::after,[\s\S]*?background:\s*var\(--tooltip-bg\);/s,
    );
  });

  it("defines dark surfaces for home stats, Live Mode, and tray popover through tokens", () => {
    expect(styles).toMatch(/\.home-stat\s*\{[\s\S]*?background:\s*var\(--surface\);/s);
    expect(styles).toMatch(/\.live-mode-control\s*\{[\s\S]*?background:\s*var\(--live-control-bg\);/s);
    expect(styles).toMatch(/:root\[data-theme="dark"\] \.tray-popover\s*\{[\s\S]*?background:\s*var\(--paper\);/s);
    expect(styles).not.toMatch(/:root\[data-theme="dark"\] \.tray-live-mode-control,[\s\S]*?\.file-row\.expanded/s);
  });

  it("keeps Live Mode dark control surfaces tokenized", () => {
    expect(styles).not.toMatch(
      /:root\[data-theme="dark"\] \.live-mode-control\.active,[\s\S]*?background:\s*linear-gradient/s,
    );

    const base = cssBlock(':root[data-theme="dark"] .live-mode-control');
    const hover = cssBlock(':root[data-theme="dark"] .live-mode-control:hover:not(:disabled)');
    const active = cssBlock(':root[data-theme="dark"] .live-mode-control.active');

    expectTokenizedColors(base, ["border-color", "background", "color"]);
    expect(base).toContain("background: var(--live-control-bg);");
    expect(hover).toContain("border-color: var(--control-border-hover);");
    expect(hover).toContain("background: var(--live-control-hover-bg);");
    expect(hover).toContain("color: var(--accent);");
    expect(hover).toContain("box-shadow: var(--surface-shadow);");
    expect(active).toContain("border-color: var(--control-border-hover);");
    expect(active).toContain("background: var(--live-control-active-bg);");
    expect(active).toContain("color: var(--accent);");
    expect(active).toContain("box-shadow: var(--surface-shadow);");
    expectNoRawSurfaceColors(base);
    expectNoRawSurfaceColors(hover);
    expectNoRawSurfaceColors(active);
  });

  it("keeps home stat dark surfaces tokenized", () => {
    const stat = cssBlock(':root[data-theme="dark"] .home-stat');
    const hover = cssBlock(':root[data-theme="dark"] button.home-stat:hover');
    const label = cssBlock(':root[data-theme="dark"] .home-stat span');
    const value = cssBlock(':root[data-theme="dark"] .home-stat strong');
    const warn = cssBlock(':root[data-theme="dark"] .home-stat strong.warn');
    const danger = cssBlock(':root[data-theme="dark"] .home-stat strong.danger');

    expectTokenizedColors(stat, ["border-color", "background", "color"]);
    expect(stat).toContain("box-shadow: var(--surface-shadow);");
    expect(hover).toContain("border-color: var(--control-border-hover);");
    expect(hover).toContain("background: var(--surface-hover);");
    expect(label).toContain("color: var(--muted);");
    expect(value).toContain("color: var(--ink);");
    expect(warn).toContain("color: var(--status-warn-text);");
    expect(danger).toContain("color: var(--status-danger-text);");
    for (const block of [stat, hover, label, value, warn, danger]) {
      expectNoRawSurfaceColors(block);
    }
  });

  it("keeps status pill colors semantic across themes", () => {
    expect(cssBlock(".status-pill.ready")).toContain("background: var(--status-ready-bg);");
    expect(cssBlock(".status-pill.ready")).toContain("color: var(--status-ready-text);");
    expect(cssBlock(".status-pill.warn")).toContain("background: var(--status-warn-bg);");
    expect(cssBlock(".status-pill.warn")).toContain("color: var(--status-warn-text);");
    expect(cssBlock(".status-pill.danger")).toContain("background: var(--status-danger-bg);");
    expect(cssBlock(".status-pill.danger")).toContain("color: var(--status-danger-text);");
    expect(cssBlock(".sidebar-collapsed .status-pill.ready")).toContain("background: var(--status-ready-text);");
    expect(cssBlock(".sidebar-collapsed .status-pill.warn")).toContain("background: var(--status-warn-text);");
    expect(cssBlock(".sidebar-collapsed .status-pill.danger")).toContain("background: var(--status-danger-text);");

    const darkReady = cssBlock(':root[data-theme="dark"] .status-pill.ready');
    const darkWarn = cssBlock(':root[data-theme="dark"] .status-pill.warn');
    const darkDanger = cssBlock(':root[data-theme="dark"] .status-pill.danger');

    expect(darkReady).toContain("background: var(--status-ready-bg);");
    expect(darkReady).toContain("color: var(--status-ready-text);");
    expect(darkWarn).toContain("background: var(--status-warn-bg);");
    expect(darkWarn).toContain("color: var(--status-warn-text);");
    expect(darkDanger).toContain("background: var(--status-danger-bg);");
    expect(darkDanger).toContain("color: var(--status-danger-text);");
    for (const block of [darkReady, darkWarn, darkDanger]) {
      expectNoRawSurfaceColors(block);
    }
  });

  it("keeps Live Mode tooltips on semantic tooltip tokens", () => {
    const tooltip = cssBlock(".live-mode-control.has-tooltip:hover::after,\n.live-mode-control.has-tooltip:focus-visible::after");
    const darkTooltip = cssBlock(
      ':root[data-theme="dark"] .live-mode-control.has-tooltip:hover::after,\n:root[data-theme="dark"] .live-mode-control.has-tooltip:focus-visible::after',
    );

    expect(tooltip).toContain("border: 1px solid var(--line);");
    expect(tooltip).toContain("background: var(--tooltip-bg);");
    expect(tooltip).toContain("box-shadow: var(--surface-shadow);");
    expect(tooltip).toContain("color: var(--tooltip-text);");
    expect(darkTooltip).toContain("border-color: var(--line);");
    expect(darkTooltip).toContain("background: var(--tooltip-bg);");
    expect(darkTooltip).toContain("box-shadow: var(--modal-shadow);");
    expect(darkTooltip).toContain("color: var(--tooltip-text);");
    expectNoRawSurfaceColors(tooltip);
    expectNoRawSurfaceColors(darkTooltip);
  });

  it("uses a dark safety wrapper on the source detail review prompt", () => {
    expect(appSource).toMatch(/className="safety-strip"[\s\S]*?Review catches work that needs approval/);
    expect(styles).toMatch(
      /:root\[data-theme="dark"\] \.safety-strip\s*\{[\s\S]*?border-color:\s*var\(--control-border-hover\);[\s\S]*?background:\s*var\(--control-selected-bg\);/s,
    );
  });

  it("keeps disabled mount detail buttons from taking hover colors", () => {
    expect(styles).toMatch(/\.mount-details-button:hover:not\(:disabled\)\s*\{/);
    expect(styles).toMatch(
      /:root\[data-theme="dark"\] \.mount-details-button:hover:not\(:disabled\),/,
    );
  });

  it("uses readable control tokens for Sources page actions", () => {
    expect(styles).toMatch(/\.secondary-button\s*\{[\s\S]*?background:\s*var\(--control-bg\);[\s\S]*?color:\s*var\(--control-text\);/s);
    expect(styles).toMatch(/\.mount-details-button\s*\{[\s\S]*?background:\s*var\(--control-bg\);[\s\S]*?color:\s*var\(--control-text\);/s);
    expect(styles).toMatch(/\.source-view-toggle button\.active\s*\{[\s\S]*?background:\s*var\(--control-selected-bg\);/s);
    expect(styles).toMatch(
      /:root\[data-theme="dark"\] \.secondary-button:disabled,[\s\S]*?background:\s*var\(--control-disabled-bg\);[\s\S]*?color:\s*var\(--control-disabled-text\);/s,
    );
  });
});
