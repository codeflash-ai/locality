import { existsSync, readFileSync } from "node:fs";

import { describe, expect, it } from "vitest";

const appSource = readFileSync(new URL("./App.tsx", import.meta.url), "utf8");
const styles = readFileSync(new URL("./styles.css", import.meta.url), "utf8");
const darkLogoPath = new URL("./assets/brand/locality-short-dark.svg", import.meta.url);
const lightLogoPath = new URL("./assets/brand/locality-short-light.svg", import.meta.url);

describe("Locality logo surfaces", () => {
  it("tracks the short dark and light logo assets for UI imports", () => {
    expect(existsSync(darkLogoPath)).toBe(true);
    expect(existsSync(lightLogoPath)).toBe(true);
  });

  it("uses LocalityLogo instead of the legacy ApertureIcon component", () => {
    expect(appSource).toMatch(/import localityShortDarkUrl from "\.\/assets\/brand\/locality-short-dark\.svg";/);
    expect(appSource).toMatch(/import localityShortLightUrl from "\.\/assets\/brand\/locality-short-light\.svg";/);
    expect(appSource).toMatch(/function LocalityLogo\(/);
    expect(appSource).not.toMatch(/ApertureIcon/);
  });

  it("chooses dark logos for light surfaces and maps dark surfaces to the light asset", () => {
    expect(appSource).toMatch(/<LocalityLogo surface="light" \/>/);
    expect(appSource).toMatch(
      /const logoUrl = surface === "dark" \? localityShortLightUrl : localityShortDarkUrl;/,
    );
  });

  it("sizes the short logo inside compact app and tray marks", () => {
    expect(styles).toMatch(/\.locality-logo\s*\{[\s\S]*?width:\s*34px;[\s\S]*?height:\s*34px;/s);
    expect(styles).toMatch(/\.brand-tile \.locality-logo\s*\{[\s\S]*?width:\s*38px;[\s\S]*?height:\s*40px;/s);
    expect(styles).toMatch(/\.tray-title \.locality-logo\s*\{[\s\S]*?width:\s*28px;[\s\S]*?height:\s*30px;/s);
  });
});
