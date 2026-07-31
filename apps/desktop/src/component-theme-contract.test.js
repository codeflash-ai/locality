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

function expectToken(block, property, token) {
  expect(block).toContain(`${property}: var(${token});`);
}

function expectNoRawColorValues(block) {
  expect(block).not.toMatch(/(?:background|border(?:-color)?|color):\s*(?:#|rgba?\(|linear-gradient)/);
}

function expectNoRawShadowValues(block) {
  for (const match of block.matchAll(/box-shadow:\s*([^;]+);/g)) {
    expect(match[1], "box-shadow should include a semantic token").toContain("var(--");
    expect(match[1], "box-shadow should not use raw colors").not.toMatch(/#|rgba?\(/);
  }
}

describe("desktop component theme contract", () => {
  it("uses semantic tokens for primary, secondary, Live Mode, and icon controls", () => {
    const primaryButton = cssBlock(".primary-button");
    const primaryButtonHover = cssBlock(".primary-button:hover:not(:disabled)");
    const secondaryButton = cssBlock(".secondary-button");
    const secondaryButtonHover = cssBlock(".secondary-button:hover:not(:disabled)");
    const liveModeControl = cssBlock(".live-mode-control");
    const liveModeButton = cssBlock(".live-mode-button");
    const liveModeButtonHover = cssBlock(".live-mode-button:hover:not(:disabled)");
    const liveModeButtonActive = cssBlock(".live-mode-button.active");
    const iconButton = cssBlock(".icon-button");

    expectToken(primaryButton, "background", "--primary-button-bg");
    expectToken(primaryButton, "color", "--primary-button-text");
    expectToken(primaryButton, "box-shadow", "--surface-shadow");
    expectToken(primaryButtonHover, "background", "--primary-button-hover-bg");
    expectToken(primaryButtonHover, "box-shadow", "--surface-shadow");
    expectToken(secondaryButton, "background", "--control-bg");
    expectToken(secondaryButton, "color", "--control-text");
    expectToken(secondaryButtonHover, "background", "--control-bg-hover");
    expectToken(secondaryButtonHover, "box-shadow", "--surface-shadow");
    expectToken(liveModeControl, "background", "--live-control-bg");
    expectToken(liveModeControl, "color", "--accent");
    expectToken(liveModeButton, "background", "--live-control-bg");
    expectToken(liveModeButton, "color", "--accent");
    expectToken(liveModeButtonHover, "background", "--live-control-hover-bg");
    expectToken(liveModeButtonHover, "box-shadow", "--surface-shadow");
    expectToken(liveModeButtonActive, "background", "--primary-button-bg");
    expectToken(liveModeButtonActive, "color", "--primary-button-text");
    expectToken(liveModeButtonActive, "box-shadow", "--surface-shadow");
    expectToken(iconButton, "background", "--control-bg");
    expectToken(iconButton, "color", "--control-text");

    for (const block of [
      primaryButton,
      primaryButtonHover,
      secondaryButton,
      secondaryButtonHover,
      liveModeControl,
      liveModeButton,
      liveModeButtonHover,
      liveModeButtonActive,
      iconButton,
    ]) {
      expectNoRawColorValues(block);
    }

    for (const block of [
      primaryButton,
      primaryButtonHover,
      secondaryButtonHover,
      liveModeButtonHover,
      liveModeButtonActive,
    ]) {
      expectNoRawShadowValues(block);
    }
  });

  it("uses shared surface tokens for cards, panels, metrics, and rows", () => {
    const sharedSurface = cssBlock(
      ".workspace-card,\n.panel,\n.attention-panel,\n.empty-action-panel,\n.mount-hero,\n.safety-strip,\n.file-list,\n.activity-group,\n.summary-grid,\n.advanced-panel,\n.located-path",
    );
    const homeStat = cssBlock(".home-stat");
    const homeStatHover = cssBlock("button.home-stat:hover");
    const metric = cssBlock(".metric");
    const activityItem = cssBlock(".activity-item");

    expectToken(sharedSurface, "background", "--surface");
    expectToken(sharedSurface, "box-shadow", "--surface-shadow");
    expectToken(homeStat, "background", "--surface");
    expectToken(homeStat, "box-shadow", "--surface-shadow");
    expectToken(homeStatHover, "background", "--surface-hover");
    expectToken(metric, "background", "--surface-muted");
    expectToken(activityItem, "background", "--surface-muted");
  });

  it("uses status role tokens instead of dark-only status colors", () => {
    expect(styles).toMatch(/\.status-pill\.ready\s*\{[\s\S]*?background:\s*var\(--status-ready-bg\);[\s\S]*?color:\s*var\(--status-ready-text\);/s);
    expect(styles).toMatch(/\.status-pill\.warn\s*\{[\s\S]*?background:\s*var\(--status-warn-bg\);[\s\S]*?color:\s*var\(--status-warn-text\);/s);
    expect(styles).toMatch(/\.status-pill\.danger\s*\{[\s\S]*?background:\s*var\(--status-danger-bg\);[\s\S]*?color:\s*var\(--status-danger-text\);/s);
    expect(styles).not.toMatch(/:root\[data-theme="dark"\][^{]*\.status-pill\.(ready|warn|danger)\s*\{/);
  });

  it("keeps review, file, settings, and editor surfaces tokenized", () => {
    expect(styles).toMatch(/\.review-filter-bar\s*\{[\s\S]*?background:\s*var\(--field-bg\);/s);
    expect(styles).toMatch(/\.file-filter-bar button\s*\{[\s\S]*?background:\s*var\(--control-bg\);/s);
    expect(styles).toMatch(/\.file-detail-panel\s*\{[\s\S]*?background:\s*var\(--surface-muted\);/s);
    expect(styles).toMatch(/\.file-detail-panel pre\s*\{[\s\S]*?background:\s*var\(--code-bg\);/s);
    expect(styles).toMatch(/\.markdown-editor\s*\{[\s\S]*?background:\s*var\(--code-bg\);/s);
    expect(styles).toMatch(/\.settings-nav\s*\{[\s\S]*?background:\s*var\(--surface\);/s);
    expect(styles).toMatch(/\.theme-segmented\s*\{[\s\S]*?background:\s*var\(--field-bg\);/s);
  });

  it("keeps task-scoped status, review, and settings overrides tokenized", () => {
    const safetyStrip = cssBlock(".safety-strip");
    const darkMetric = cssBlock(':root[data-theme="dark"] .review-counts .metric,\n:root[data-theme="dark"] .metric');
    const darkMetricText = cssBlock(':root[data-theme="dark"] .metric strong');
    const darkReviewFilter = cssBlock(
      ':root[data-theme="dark"] .review-filter-button.active,\n:root[data-theme="dark"] .review-filter-button:hover',
    );
    const darkReviewFilterCount = cssBlock(':root[data-theme="dark"] .review-filter-button span');
    const darkReviewOverview = cssBlock(':root[data-theme="dark"] .review-overview-panel');
    const fileFilterState = cssBlock(".file-filter-bar button.active,\n.file-filter-bar button:hover");
    const settingsActivity = cssBlock(".settings-activity-row");
    const settingsActivityHover = cssBlock(".settings-activity-row:hover");
    const settingsNavState = cssBlock(".settings-nav button:hover,\n.settings-nav button.active");
    const darkSettingsNavSmall = cssBlock(
      ':root[data-theme="dark"] .settings-nav button:hover small,\n:root[data-theme="dark"] .settings-nav button.active small',
    );
    const darkSharedActive = cssBlock(
      ':root[data-theme="dark"] .source-view-toggle button.active,\n:root[data-theme="dark"] .file-filter-bar button.active,\n:root[data-theme="dark"] .file-filter-bar button:hover,\n:root[data-theme="dark"] .activity-tabs button.active,\n:root[data-theme="dark"] .settings-nav button:hover,\n:root[data-theme="dark"] .settings-nav button.active,\n:root[data-theme="dark"] .theme-segmented button.active,\n:root[data-theme="dark"] .option-row:hover',
    );
    const darkSourceCards = cssBlock(
      ':root[data-theme="dark"] .source-ready-card,\n:root[data-theme="dark"] .connector-choice-card.active,\n:root[data-theme="dark"] .mount-card.active',
    );
    const primaryButton = cssBlock(".primary-button");
    const primaryButtonHover = cssBlock(".primary-button:hover:not(:disabled)");
    const secondaryButtonHover = cssBlock(".secondary-button:hover:not(:disabled)");
    const liveModeButtonHover = cssBlock(".live-mode-button:hover:not(:disabled)");
    const liveModeButtonActive = cssBlock(".live-mode-button.active");
    const liveModeHover = cssBlock(".live-mode-control:hover:not(:disabled)");
    const liveModeActive = cssBlock(".live-mode-control.active");
    const themeSegmentedActive = cssBlock(".theme-segmented button.active");

    expectToken(safetyStrip, "border-color", "--control-border-hover");
    expectToken(safetyStrip, "background", "--control-selected-bg");
    expectToken(darkMetric, "border-color", "--line");
    expectToken(darkMetric, "background", "--surface-muted");
    expectToken(darkMetricText, "color", "--ink");
    expectToken(darkReviewFilter, "background", "--control-selected-bg");
    expectToken(darkReviewFilter, "color", "--control-selected-text");
    expectToken(darkReviewFilterCount, "background", "--surface-raised");
    expectToken(darkReviewOverview, "background", "--surface");
    expectToken(fileFilterState, "border-color", "--control-border-hover");
    expectToken(fileFilterState, "background", "--control-selected-bg");
    expectToken(fileFilterState, "color", "--control-selected-text");
    expect(settingsActivity).toContain("border: 1px solid var(--line);");
    expectToken(settingsActivity, "background", "--surface-raised");
    expectToken(settingsActivity, "color", "--ink");
    expectToken(settingsActivityHover, "border-color", "--control-border-hover");
    expectToken(settingsActivityHover, "background", "--surface-hover");
    expectToken(settingsNavState, "border-color", "--control-border-hover");
    expectToken(settingsNavState, "background", "--control-selected-bg");
    expectToken(settingsNavState, "color", "--control-selected-text");
    expectToken(darkSettingsNavSmall, "color", "--control-muted");
    expectToken(darkSharedActive, "box-shadow", "--surface-shadow");
    expectToken(darkSourceCards, "border-color", "--control-border-hover");
    expectToken(darkSourceCards, "background", "--surface-muted");
    expectToken(darkSourceCards, "color", "--ink");
    expectToken(primaryButton, "box-shadow", "--surface-shadow");
    expectToken(primaryButtonHover, "box-shadow", "--surface-shadow");
    expectToken(secondaryButtonHover, "box-shadow", "--surface-shadow");
    expectToken(liveModeButtonHover, "box-shadow", "--surface-shadow");
    expectToken(liveModeButtonActive, "box-shadow", "--surface-shadow");
    expectToken(liveModeHover, "box-shadow", "--surface-shadow");
    expectToken(liveModeActive, "box-shadow", "--surface-shadow");
    expectToken(themeSegmentedActive, "background", "--control-selected-bg");
    expectToken(themeSegmentedActive, "color", "--control-selected-text");
    expect(themeSegmentedActive).toContain("box-shadow: inset 0 0 0 1px var(--control-border-hover);");

    for (const block of [
      safetyStrip,
      darkMetric,
      darkMetricText,
      darkReviewFilter,
      darkReviewFilterCount,
      darkReviewOverview,
      fileFilterState,
      settingsActivity,
      settingsActivityHover,
      settingsNavState,
      darkSettingsNavSmall,
      darkSourceCards,
      themeSegmentedActive,
    ]) {
      expectNoRawColorValues(block);
    }

    for (const block of [
      primaryButton,
      primaryButtonHover,
      secondaryButtonHover,
      liveModeButtonHover,
      liveModeButtonActive,
      liveModeHover,
      liveModeActive,
      themeSegmentedActive,
    ]) {
      expectNoRawColorValues(block);
      expectNoRawShadowValues(block);
    }

    expectNoRawShadowValues(darkSharedActive);
  });
});
