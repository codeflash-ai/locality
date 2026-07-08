import { readFileSync } from "node:fs";

import { describe, expect, it } from "vitest";

const styles = readFileSync(new URL("./styles.css", import.meta.url), "utf8");

describe("onboarding layout styles", () => {
  it("keeps the onboarding window chrome fixed while the content scrolls", () => {
    expect(styles).toMatch(/\.setup-window\s*\{\s*display:\s*grid;\s*grid-template-rows:\s*auto minmax\(0, 1fr\);\s*\}/s);
    expect(styles).toMatch(
      /\.setup-window > \.setup-scrollport\s*\{\s*min-height:\s*0;\s*overflow-y:\s*auto;\s*overflow-x:\s*hidden;\s*overscroll-behavior:\s*contain;\s*scrollbar-gutter:\s*stable;\s*\}/s,
    );
  });

  it("keeps short onboarding steps centered without clipping tall ones above scrollTop zero", () => {
    expect(styles).toMatch(/\.setup-scrollport > \.setup-content\s*\{\s*min-height:\s*100%;\s*\}/s);
  });

  it("allows the ready-screen prompt copy to wrap when the mount path is long", () => {
    expect(styles).toMatch(/\.agent-demo-command\s*\{[\s\S]*?overflow-wrap:\s*anywhere;/s);
  });

  it("keeps the onboarding demo video in a stable native-looking frame", () => {
    expect(styles).toMatch(/\.onboarding-video-demo\s*\{[\s\S]*?aspect-ratio:\s*143 \/ 90;/s);
    expect(styles).toMatch(/\.onboarding-video-demo video\s*\{[\s\S]*?object-fit:\s*cover;/s);
  });

  it("keeps the first onboarding screen from overlapping at the native default width", () => {
    expect(styles).toMatch(/\.setup-content\.hero-setup\s*\{[\s\S]*?grid-template-columns:\s*minmax\(0,\s*390px\) minmax\(390px,\s*520px\);/s);
    expect(styles).toMatch(/\.setup-content\.hero-setup\s*\{[\s\S]*?justify-content:\s*end;/s);
    expect(styles).toMatch(/\.hero-setup \.setup-side\s*\{[\s\S]*?max-width:\s*520px;/s);
    expect(styles).toMatch(
      /@media \(min-width:\s*1120px\)\s*\{[\s\S]*?\.setup-content\.hero-setup\s*\{[\s\S]*?grid-template-columns:\s*minmax\(0,\s*390px\) minmax\(560px,\s*clamp\(560px,\s*50vw,\s*720px\)\);/s,
    );
    expect(styles).toMatch(/@media \(min-width:\s*1120px\)\s*\{[\s\S]*?\.setup-content\.hero-setup\s*\{[\s\S]*?justify-content:\s*center;/s);
  });

  it("keeps connector onboarding cards from overlapping the copy column", () => {
    expect(styles).toMatch(
      /\.setup-content\.split-setup\s*\{[\s\S]*?grid-template-columns:\s*minmax\(0,\s*0\.95fr\) minmax\(340px,\s*0\.85fr\);[\s\S]*?gap:\s*40px;/s,
    );
    expect(styles).toMatch(/\.setup-copy\s*\{[\s\S]*?width:\s*100%;/s);
    expect(styles).toMatch(
      /\.setup-content\.split-setup \.setup-copy h1,\s*\.setup-content\.split-setup \.setup-copy p\s*\{[\s\S]*?max-width:\s*100%;/s,
    );
    expect(styles).toMatch(/\.onboarding-pill-row\s*\{[\s\S]*?flex-wrap:\s*wrap;/s);
  });
});
