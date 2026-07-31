import { readFileSync } from "node:fs";

import { describe, expect, it } from "vitest";

const styles = readFileSync(new URL("./styles.css", import.meta.url), "utf8");

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function cssBlock(selector) {
  const matches = [...styles.matchAll(new RegExp(`(^|\\n)${escapeRegExp(selector)}\\s*\\{`, "g"))];
  const match = matches.at(-1);
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

function expectNoRawSurfaceColors(block) {
  expect(block).not.toMatch(/(?:background|border(?:-color)?|color|box-shadow):[^;]*(?:#|rgba?\(|linear-gradient)/);
}

function expectCssBlock(selector, declarations) {
  const block = cssBlock(selector);
  for (const declaration of declarations) {
    expect(block).toContain(declaration);
  }
  return block;
}

describe("desktop secondary surface theme contract", () => {
  it("keeps onboarding cards, paths, demos, and permission callouts tokenized", () => {
    expect(cssBlock(".sync-note")).toContain("border: 1px solid var(--control-border);");
    expect(cssBlock(".sync-note")).toContain("background: var(--surface-muted);");
    expect(cssBlock(".sync-note")).toContain("color: var(--muted);");
    expect(cssBlock(".sync-note.connected")).toContain("border-color: var(--status-ready-bg);");
    expect(cssBlock(".sync-note.connected")).toContain("background: var(--status-ready-bg);");
    expect(cssBlock(".sync-note.connected")).toContain("color: var(--status-ready-text);");
    expect(cssBlock(".sync-note.warning")).toContain("border-color: var(--status-warn-bg);");
    expect(cssBlock(".sync-note.warning")).toContain("background: var(--status-warn-bg);");
    expect(cssBlock(".sync-note.warning")).toContain("color: var(--status-warn-text);");
    expect(cssBlock(".sync-note svg")).toContain("color: currentColor;");
    expect(cssBlock(".onboarding-pill-row span,\n.onboarding-pill,\n.connector-option > span,\n.demo-tile span,\n.review-strip span")).toContain("border: 1px solid var(--chip-border);");
    expect(cssBlock(".onboarding-pill-row span,\n.onboarding-pill,\n.connector-option > span,\n.demo-tile span,\n.review-strip span")).toContain("background: var(--chip-bg);");
    expect(cssBlock(".onboarding-pill-row span,\n.onboarding-pill,\n.connector-option > span,\n.demo-tile span,\n.review-strip span")).toContain("color: var(--chip-text);");
    expectCssBlock(".onboarding-product-demo", [
      "background: var(--code-bg);",
      "color: var(--ink);",
      "box-shadow: var(--modal-shadow);",
    ]);
    expectCssBlock(".onboarding-video-demo", [
      "border: 1px solid var(--line);",
      "background: var(--code-bg);",
      "box-shadow: var(--modal-shadow);",
    ]);
    expectCssBlock(".demo-tile", ["border: 1px solid var(--line);", "background: var(--surface-muted);"]);
    expectCssBlock(".demo-tile p,\n.demo-tile code", ["color: var(--muted);"]);
    expectCssBlock(".agent-workspace-demo", ["background: var(--surface);", "box-shadow: var(--surface-shadow);"]);
    expectCssBlock(".agent-surface-demo", ["background: var(--surface-raised);"]);
    expectCssBlock(".path-field", ["background: var(--field-bg);"]);
    expectCssBlock(".agent-demo", ["background: var(--surface-active);"]);
    expectCssBlock(".agent-demo-command", ["background: var(--code-bg);"]);
    expectCssBlock(".connector-option.selectable:hover:not(:disabled),\n.connector-option.selectable.selected", [
      "border-color: var(--control-border-hover);",
      "background: var(--control-selected-bg);",
      "box-shadow: var(--surface-shadow);",
    ]);
    expectCssBlock(".agent-guidance-card", [
      "border: 1px solid var(--control-border-hover);",
      "background: var(--control-selected-bg);",
      "box-shadow: var(--surface-shadow);",
    ]);
    expectCssBlock(".agent-guidance-card.warning", [
      "border-color: var(--status-warn-bg);",
      "background: var(--status-warn-bg);",
    ]);
    expectCssBlock(".setup-permission-callout", ["background: var(--status-warn-bg);"]);
    expect(cssBlock(".brand-tile")).toContain("border: 1px solid var(--control-border-hover);");
    expect(cssBlock(".brand-tile")).toContain("background: var(--control-selected-bg);");
    expect(cssBlock(".brand-tile")).toContain("color: var(--control-selected-text);");
    expect(cssBlock(".brand-tile")).toContain("box-shadow: var(--surface-shadow);");
    expect(cssBlock(".brand-tile.notion")).toContain("border-color: var(--line);");
    expect(cssBlock(".brand-tile.notion")).toContain("background: var(--code-bg);");
    expect(cssBlock(".brand-tile.notion")).toContain("color: var(--ink);");
    expect(cssBlock(".brand-tile.notion")).toContain("box-shadow: var(--surface-shadow);");
    expect(cssBlock(".progress-list span")).toContain("border: 1px solid var(--line);");
    expect(cssBlock(".progress-list span")).toContain("color: var(--muted);");
    expect(cssBlock(".progress-list li.done span,\n.progress-list li.active span")).toContain("border-color: var(--accent);");
    expect(cssBlock(".progress-list li.done span,\n.progress-list li.active span")).toContain("background: var(--accent);");
    expect(cssBlock(".progress-list li.done span,\n.progress-list li.active span")).toContain("color: var(--accent-ink);");
    expect(cssBlock(".progress-list li.active span")).toContain("background: transparent;");

    expectNoRawSurfaceColors(cssBlock(".onboarding-product-demo"));
    expectNoRawSurfaceColors(cssBlock(".onboarding-video-demo"));
    expectNoRawSurfaceColors(cssBlock(".brand-tile"));
    expectNoRawSurfaceColors(cssBlock(".brand-tile.notion"));
    expectNoRawSurfaceColors(cssBlock(".progress-list span"));
    expectNoRawSurfaceColors(cssBlock(".progress-list li.done span,\n.progress-list li.active span"));
    expectNoRawSurfaceColors(cssBlock(".progress-list li.active span"));
    expectNoRawSurfaceColors(cssBlock(".sync-note"));
    expectNoRawSurfaceColors(cssBlock(".sync-note.connected"));
    expectNoRawSurfaceColors(cssBlock(".sync-note.warning"));
    expectNoRawSurfaceColors(cssBlock(".sync-note svg"));
    expectNoRawSurfaceColors(cssBlock(".onboarding-pill-row span,\n.onboarding-pill,\n.connector-option > span,\n.demo-tile span,\n.review-strip span"));
    expectNoRawSurfaceColors(cssBlock(".demo-tile"));
    expectNoRawSurfaceColors(cssBlock(".demo-tile p,\n.demo-tile code"));
    expectNoRawSurfaceColors(cssBlock(".connector-option.selectable:hover:not(:disabled),\n.connector-option.selectable.selected"));
    expectNoRawSurfaceColors(cssBlock(".agent-guidance-card"));
    expectNoRawSurfaceColors(cssBlock(".agent-guidance-card.warning"));
  });

  it("keeps Finder enable guide and final ready surfaces on semantic tokens", () => {
    expectCssBlock(".finder-enable-illustration", [
      "position: relative;",
      "display: grid;",
      "grid-template-columns: minmax(118px, 0.34fr) minmax(0, 1fr);",
      "grid-template-rows: 34px minmax(0, 1fr);",
      "width: 100%;",
      "overflow: hidden;",
      "aspect-ratio: 16 / 7;",
      "border: 1px solid var(--line);",
      "background: var(--surface-raised);",
      "color: var(--ink);",
      "box-shadow: var(--surface-shadow);",
    ]);
    expectCssBlock(".finder-enable-toolbar", ["background: var(--surface-muted);"]);
    expectCssBlock(".finder-enable-sidebar", ["background: var(--wash);"]);
    expectCssBlock(".finder-enable-control", [
      "border: 1px solid var(--control-border-hover);",
      "background: var(--field-bg);",
      "box-shadow: 0 0 0 4px var(--focus-ring);",
      "color: var(--accent);",
    ]);
    expectCssBlock("@keyframes finder-enable-pulse", ["box-shadow: 0 0 0 7px var(--focus-ring);"]);
    expectCssBlock('[data-theme="dark"] .finder-enable-illustration', [
      "border-color: var(--line);",
      "background: var(--surface-raised);",
      "box-shadow: var(--surface-shadow);",
      "color: var(--ink);",
    ]);
    expectCssBlock('[data-theme="dark"] .finder-enable-toolbar', [
      "border-color: var(--line);",
      "background: var(--surface-muted);",
    ]);
    expectCssBlock('[data-theme="dark"] .finder-enable-sidebar', [
      "border-color: var(--line);",
      "background: var(--wash);",
    ]);
    expectCssBlock('[data-theme="dark"] .finder-enable-control', [
      "border-color: var(--control-border-hover);",
      "background: var(--field-bg);",
      "color: var(--accent);",
    ]);
    expectCssBlock(".ready-folder", ["background: var(--surface-active);"]);
    expectCssBlock(".folder-inline", ["background: var(--surface-raised);"]);

    expectNoRawSurfaceColors(cssBlock(".finder-enable-illustration"));
    expectNoRawSurfaceColors(cssBlock(".finder-enable-control"));
    expectNoRawSurfaceColors(cssBlock('[data-theme="dark"] .finder-enable-illustration'));
    expectNoRawSurfaceColors(cssBlock('[data-theme="dark"] .finder-enable-toolbar'));
    expectNoRawSurfaceColors(cssBlock('[data-theme="dark"] .finder-enable-sidebar'));
    expectNoRawSurfaceColors(cssBlock('[data-theme="dark"] .finder-enable-control'));
  });

  it("keeps setup status strips and onboarding badges tokenized", () => {
    expect(cssBlock(".review-strip")).toContain("border: 1px solid var(--status-warn-bg);");
    expect(cssBlock(".review-strip")).toContain("background: var(--status-warn-bg);");
    expect(cssBlock(".review-strip")).toContain("color: var(--status-warn-text);");
    expect(cssBlock(".review-strip span")).toContain("color: var(--status-warn-text);");

    expectNoRawSurfaceColors(cssBlock(".review-strip"));
    expectNoRawSurfaceColors(cssBlock(".review-strip span"));
  });

  it("keeps destructive and source modals tokenized in both themes", () => {
    expectCssBlock(".modal-backdrop", ["background: var(--modal-backdrop);"]);
    expectCssBlock(".destructive-modal", [
      "border: 1px solid var(--status-danger-bg);",
      "background: var(--paper);",
      "box-shadow: var(--modal-shadow);",
    ]);
    expectCssBlock(".source-modal", ["background: var(--paper);", "box-shadow: var(--modal-shadow);"]);
    expect(cssBlock(".source-search-row input")).toContain("background: transparent;");
    expect(cssBlock(".source-search-row input")).toContain("color: var(--ink);");
    expect(cssBlock(".source-inline-field input")).toContain("background: var(--field-bg);");
    expect(cssBlock(".source-inline-field input")).toContain("color: var(--ink);");
    expect(cssBlock(".source-inline-field input:focus")).toContain("border-color: var(--control-border-hover);");
    expect(cssBlock(".source-inline-field input:focus")).toContain("box-shadow: 0 0 0 3px var(--focus-ring);");
    expectCssBlock(".destructive-input-label input", [
      "border: 1px solid var(--status-danger-bg);",
      "background: var(--field-bg);",
    ]);
    expectCssBlock(".destructive-input-label input:focus", [
      "border-color: var(--status-danger-text);",
      "box-shadow: 0 0 0 3px var(--focus-ring);",
    ]);
    expectCssBlock(".destructive-action-button", [
      "border: 1px solid var(--status-danger-text);",
      "background: var(--status-danger-text);",
      "color: var(--accent-ink);",
    ]);
    expectCssBlock(".destructive-action-button:disabled", [
      "border-color: var(--control-border);",
      "background: var(--control-disabled-bg);",
      "color: var(--control-disabled-text);",
    ]);
    expect(
      cssBlock(
        ':root[data-theme="dark"] .source-ready-card span,\n:root[data-theme="dark"] .connector-choice-card.active p,\n:root[data-theme="dark"] .connector-option small,\n:root[data-theme="dark"] .file-row.expanded span',
      ),
    ).toContain("color: var(--muted);");

    expectNoRawSurfaceColors(cssBlock(".destructive-modal"));
    expectNoRawSurfaceColors(cssBlock(".source-search-row input"));
    expectNoRawSurfaceColors(cssBlock(".source-inline-field input"));
    expectNoRawSurfaceColors(cssBlock(".source-inline-field input:focus"));
    expectNoRawSurfaceColors(cssBlock(".destructive-input-label input"));
    expectNoRawSurfaceColors(cssBlock(".destructive-input-label input:focus"));
    expectNoRawSurfaceColors(cssBlock(".destructive-action-button"));
    expectNoRawSurfaceColors(cssBlock(".destructive-action-button:disabled"));
    expectNoRawSurfaceColors(
      cssBlock(
        ':root[data-theme="dark"] .source-ready-card span,\n:root[data-theme="dark"] .connector-choice-card.active p,\n:root[data-theme="dark"] .connector-option small,\n:root[data-theme="dark"] .file-row.expanded span',
      ),
    );
  });

  it("keeps tray popover controls and list rows tokenized", () => {
    expectCssBlock(".tray-popover", ["background: var(--paper);", "box-shadow: var(--modal-shadow);"]);
    expectCssBlock(".tray-live-mode-control", ["background: var(--live-control-bg);"]);
    expect(cssBlock(".tray-live-mode-control:hover:not(:disabled)")).toContain("box-shadow: var(--surface-shadow);");
    expectCssBlock(".tray-locate-row", ["background: var(--field-bg);"]);
    expectCssBlock(".tray-locate-row button", [
      "background: var(--primary-button-bg);",
      "color: var(--primary-button-text);",
    ]);
    expectCssBlock(".tray-result", ["background: var(--surface-raised);"]);
    expectCssBlock(".tray-change-list button", ["background: var(--surface-muted);"]);
    expectCssBlock(".tray-quit-menu", ["background: var(--surface-raised);"]);
    expectCssBlock(".tray-quit-menu button:hover:not(:disabled)", ["background: var(--surface-hover);"]);
    expect(cssBlock(".tray-review-summary")).toContain("border-color: var(--status-warn-bg);");
    expect(cssBlock(".tray-review-summary")).toContain("background: var(--status-warn-bg);");
    expect(cssBlock(".tray-review-summary")).toContain("color: var(--status-warn-text);");
    expectCssBlock(
      ':root[data-theme="dark"] .tray-search-results button:hover,\n:root[data-theme="dark"] .tray-change-list button:hover:not(:disabled),\n:root[data-theme="dark"] .tray-quit-menu button:hover:not(:disabled)',
      ["border-color: var(--control-border-hover);", "background: var(--surface-hover);"],
    );
    expectCssBlock(':root[data-theme="dark"] .tray-locate-row button', [
      "background: var(--primary-button-bg);",
      "color: var(--primary-button-text);",
    ]);
    expectCssBlock(':root[data-theme="dark"] .tray-locate-row button:disabled', [
      "background: var(--control-disabled-bg);",
      "color: var(--control-disabled-text);",
    ]);
    expect(cssBlock(':root[data-theme="dark"] .search-state')).toContain("background: var(--status-ready-bg);");
    expect(cssBlock(':root[data-theme="dark"] .search-state')).toContain("color: var(--status-ready-text);");
    expect(cssBlock(':root[data-theme="dark"] .search-state.online_only,\n:root[data-theme="dark"] .search-state.preparing,\n:root[data-theme="dark"] .search-state.remote_update_available')).toContain("background: var(--control-selected-bg);");
    expect(cssBlock(':root[data-theme="dark"] .search-state.online_only,\n:root[data-theme="dark"] .search-state.preparing,\n:root[data-theme="dark"] .search-state.remote_update_available')).toContain("color: var(--control-selected-text);");
    expect(cssBlock(':root[data-theme="dark"] .search-state.pending_changes')).toContain("background: var(--status-warn-bg);");
    expect(cssBlock(':root[data-theme="dark"] .search-state.pending_changes')).toContain("color: var(--status-warn-text);");
    expect(cssBlock(':root[data-theme="dark"] .search-state.conflict,\n:root[data-theme="dark"] .search-state.no_access,\n:root[data-theme="dark"] .search-state.not_found')).toContain("background: var(--status-danger-bg);");
    expect(cssBlock(':root[data-theme="dark"] .search-state.conflict,\n:root[data-theme="dark"] .search-state.no_access,\n:root[data-theme="dark"] .search-state.not_found')).toContain("color: var(--status-danger-text);");
    expect(cssBlock(':root[data-theme="dark"] .tray-section-heading button,\n:root[data-theme="dark"] .tray-result button,\n:root[data-theme="dark"] .tray-suggestion button,\n:root[data-theme="dark"] .tray-controls-row button,\n:root[data-theme="dark"] .tray-footer button,\n:root[data-theme="dark"] .tray-list-row em')).toContain("color: var(--accent);");
    expect(cssBlock(".toggle")).toContain("background: var(--switch-track-bg);");
    expect(cssBlock(".toggle")).toContain("box-shadow: inset 0 0 0 1px var(--switch-track-border);");
    expect(cssBlock(".toggle i")).toContain("background: var(--switch-thumb-bg);");
    expect(cssBlock(".toggle i")).toContain("box-shadow: var(--switch-thumb-shadow);");
    expect(cssBlock(".toggle.enabled")).toContain("background: var(--switch-track-active-bg);");
    expect(cssBlock(".toggle.enabled i")).toContain("background: var(--switch-thumb-active-bg);");
    expect(cssBlock(':root[data-theme="dark"] .toggle')).toContain("background: var(--switch-track-bg);");
    expect(cssBlock(':root[data-theme="dark"] .toggle')).toContain("box-shadow: inset 0 0 0 1px var(--switch-track-border);");
    expect(cssBlock(':root[data-theme="dark"] .toggle i')).toContain("background: var(--switch-thumb-bg);");
    expect(cssBlock(':root[data-theme="dark"] .toggle i')).toContain("box-shadow: var(--switch-thumb-shadow);");
    expect(cssBlock(':root[data-theme="dark"] .toggle.enabled')).toContain("background: var(--switch-track-active-bg);");
    expect(cssBlock(':root[data-theme="dark"] .toggle.enabled')).toContain("box-shadow: inset 0 0 0 1px var(--switch-track-active-bg);");
    expect(cssBlock(':root[data-theme="dark"] .toggle.enabled i')).toContain("background: var(--switch-thumb-active-bg);");

    expectNoRawSurfaceColors(cssBlock(".tray-live-mode-control:hover:not(:disabled)"));
    expectNoRawSurfaceColors(cssBlock(".tray-review-summary"));
    expectNoRawSurfaceColors(cssBlock(".toggle"));
    expectNoRawSurfaceColors(cssBlock(".toggle i"));
    expectNoRawSurfaceColors(cssBlock(".toggle.enabled"));
    expectNoRawSurfaceColors(cssBlock(".toggle.enabled i"));
    expectNoRawSurfaceColors(cssBlock(':root[data-theme="dark"] .toggle'));
    expectNoRawSurfaceColors(cssBlock(':root[data-theme="dark"] .toggle i'));
    expectNoRawSurfaceColors(cssBlock(':root[data-theme="dark"] .toggle.enabled'));
    expectNoRawSurfaceColors(cssBlock(':root[data-theme="dark"] .toggle.enabled i'));
    expectNoRawSurfaceColors(cssBlock(".tray-quit-menu button:hover:not(:disabled)"));
    expectNoRawSurfaceColors(
      cssBlock(
        ':root[data-theme="dark"] .tray-search-results button:hover,\n:root[data-theme="dark"] .tray-change-list button:hover:not(:disabled),\n:root[data-theme="dark"] .tray-quit-menu button:hover:not(:disabled)',
      ),
    );
    expectNoRawSurfaceColors(cssBlock(':root[data-theme="dark"] .tray-locate-row button'));
    expectNoRawSurfaceColors(cssBlock(':root[data-theme="dark"] .tray-locate-row button:disabled'));
    expectNoRawSurfaceColors(cssBlock(':root[data-theme="dark"] .search-state'));
    expectNoRawSurfaceColors(
      cssBlock(
        ':root[data-theme="dark"] .search-state.online_only,\n:root[data-theme="dark"] .search-state.preparing,\n:root[data-theme="dark"] .search-state.remote_update_available',
      ),
    );
    expectNoRawSurfaceColors(cssBlock(':root[data-theme="dark"] .search-state.pending_changes'));
    expectNoRawSurfaceColors(
      cssBlock(
        ':root[data-theme="dark"] .search-state.conflict,\n:root[data-theme="dark"] .search-state.no_access,\n:root[data-theme="dark"] .search-state.not_found',
      ),
    );
    expectNoRawSurfaceColors(
      cssBlock(
        ':root[data-theme="dark"] .tray-section-heading button,\n:root[data-theme="dark"] .tray-result button,\n:root[data-theme="dark"] .tray-suggestion button,\n:root[data-theme="dark"] .tray-controls-row button,\n:root[data-theme="dark"] .tray-footer button,\n:root[data-theme="dark"] .tray-list-row em',
      ),
    );
  });
});
