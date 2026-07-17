import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

const app = readFileSync(new URL("./App.tsx", import.meta.url), "utf8");
const styles = readFileSync(new URL("./styles.css", import.meta.url), "utf8");

describe("Finder enablement checkpoint", () => {
  it("renders the Finder cue and recovery actions", () => {
    expect(app).toContain('className="finder-enable-guide"');
    expect(app).toContain('className="finder-enable-control"');
    expect(app).toContain('&quot;Locality&quot; is not enabled.');
    expect(app).toContain("Reopen Finder");
    expect(app).toContain("Having trouble?");
  });

  it("keeps later source recovery running when its dialog closes", () => {
    const addSourceDialog = app.match(
      /<AddSourceDialog[\s\S]*?fileProviderEnablement=\{sourceFileProviderEnablement\}[\s\S]*?onClose=\{\(\) => \{([\s\S]*?)\}\}\s*\/>/,
    );

    expect(addSourceDialog?.[1].trim()).toBe("setSourceDialogOpen(false);");
  });

  it("passes later source recovery state into the guided dialog", () => {
    expect(app).toContain("fileProviderEnablement={sourceFileProviderEnablement}");
  });

  it("keeps the Finder crop stable and theme-aware", () => {
    expect(styles).toMatch(/\.finder-enable-illustration\s*\{[\s\S]*?aspect-ratio:\s*16 \/ 7;/s);
    expect(styles).toMatch(/\.finder-enable-control\s*\{[\s\S]*?min-width:\s*62px;/s);
    expect(styles).toContain('[data-theme="dark"] .finder-enable-illustration');
  });

  it("disables the enable-control highlight for reduced motion", () => {
    expect(styles).toMatch(
      /@media \(prefers-reduced-motion: reduce\)[\s\S]*?\.finder-enable-control\s*\{[\s\S]*?animation:\s*none;/s,
    );
  });
});
