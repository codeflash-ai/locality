import { readFileSync } from "node:fs";

import { describe, expect, it } from "vitest";

const appSource = readFileSync(new URL("./App.tsx", import.meta.url), "utf8");
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

function expectDeclarations(selector, declarations) {
  const block = cssBlock(selector);
  for (const declaration of declarations) {
    expect(block).toContain(declaration);
  }
  return block;
}

function expectNoRawSurfaceColors(block) {
  expect(block).not.toMatch(/(?:background|border(?:-color)?|color|box-shadow):[^;]*(?:#|rgba?\(|linear-gradient|radial-gradient)/);
}

describe("onboarding redesign structure", () => {
  it("renders the four segment progress rail directly under the onboarding chrome", () => {
    expect(appSource).toContain('import { onboardingProgressSegments, onboardingProgressStep, onboardingStepMeta, type OnboardingStep } from "./onboarding-steps";');
    expect(appSource).toContain("function OnboardingFrame({");
    expect(appSource).toContain('<section className="setup-window onboarding-window">');
    expect(appSource).toContain('<WindowChrome title="Locality" meta={onboardingStepMeta(step, optionalGuideReturnStep)} />');
    expect(appSource).toContain("<OnboardingProgressRail currentStep={currentStep} />");
    expect(appSource).toContain('className="setup-progress-rail"');
    expect(appSource).toContain("onboardingProgressSegments(currentStep).map");

    expectDeclarations(".onboarding-window", [
      "position: relative;",
      "isolation: isolate;",
      "grid-template-rows: auto auto minmax(0, 1fr);",
    ]);
    expectDeclarations(".setup-progress-rail", [
      "display: grid;",
      "grid-template-columns: repeat(4, minmax(0, 1fr));",
      "background: transparent;",
    ]);
    expectDeclarations(".setup-progress-rail span", [
      "background: var(--onboarding-progress-bg);",
    ]);
    expectDeclarations(".setup-progress-rail span.complete", [
      "background: var(--accent);",
    ]);
  });

  it("uses the reference step one copy, actions, chips, and static editor mock", () => {
    expect(appSource).toContain('<div className="eyebrow"><span />Meet Locality</div>');
    expect(appSource).toContain("<h1>Turn work apps into agent-ready files.</h1>");
    expect(appSource).toContain('<PrimaryButton icon={<ChevronRight />} onClick={() => setStep(3)}>Get Started</PrimaryButton>');
    expect(appSource).toContain('<SecondaryButton icon={<ChevronRight />} onClick={() => openOptionalGuide(1)}>How agents use it</SecondaryButton>');
    expect(appSource).toContain('<FolderOpen />');
    expect(appSource).toContain("Finder-native files");
    expect(appSource).toContain('<Code2 />');
    expect(appSource).toContain("Markdown edits");
    expect(appSource).toContain('<Check />');
    expect(appSource).toContain("Review before sync");
    expect(appSource).toContain('aria-label="Local Markdown preview"');
    expect(appSource).toContain("Release Notes - v2.4");
    expect(appSource).not.toContain("onboardingDemoVideoUrl");

    expectDeclarations(".onboarding-editor-demo", [
      "background: var(--onboarding-demo-bg);",
      "color: var(--onboarding-demo-text);",
      "box-shadow: var(--modal-shadow);",
    ]);
    expectDeclarations(".editor-demo-toolbar span", [
      "background: var(--onboarding-demo-chip-bg);",
      "color: var(--onboarding-demo-muted);",
    ]);
    expectDeclarations(".editor-demo-sidebar", [
      "border-right: 1px solid var(--onboarding-demo-line);",
      "background: var(--onboarding-demo-panel-bg);",
    ]);
    expectDeclarations(".editor-demo-sidebar small", [
      "color: var(--onboarding-demo-muted);",
    ]);
    expectDeclarations(".editor-demo-line", [
      "color: var(--onboarding-demo-muted);",
    ]);
    expectDeclarations(".editor-demo-line.active", [
      "background: var(--onboarding-demo-active-bg);",
      "color: var(--onboarding-demo-active-text);",
    ]);
    expectDeclarations(".editor-demo-document pre", [
      "color: var(--onboarding-demo-text);",
    ]);

    expectNoRawSurfaceColors(cssBlock(".onboarding-editor-demo"));
    expectNoRawSurfaceColors(cssBlock(".editor-demo-toolbar span"));
    expectNoRawSurfaceColors(cssBlock(".editor-demo-sidebar"));
    expectNoRawSurfaceColors(cssBlock(".editor-demo-sidebar small"));
    expectNoRawSurfaceColors(cssBlock(".editor-demo-line"));
    expectNoRawSurfaceColors(cssBlock(".editor-demo-line.active"));
    expectNoRawSurfaceColors(cssBlock(".editor-demo-document pre"));
  });

  it("renders the reference source list as five vertical cards", () => {
    expect(appSource).toContain("type OnboardingConnectorCard = {");
    expect(appSource).toContain("const onboardingConnectorCards: OnboardingConnectorCard[] = [");
    expect(appSource).toContain('connector: "notion",');
    expect(appSource).toContain('connector: "google-docs",');
    expect(appSource).toContain('connector: "google-calendar",');
    expect(appSource).toContain('connector: "gmail",');
    expect(appSource).toContain('connector: "granola",');
    expect(appSource).not.toContain('connector: "linear", title: "Linear"');
    expect(appSource).not.toContain('connector: "slack", title: "Slack"');
    expect(appSource).toContain('className="connector-options onboarding-source-list"');
    expect(appSource).toContain('className="connector-card-accessory"');
    expect(appSource).toContain('<ChevronRight />');
    expect(appSource).toContain('<PrimaryButton icon={<ConnectorIcon connector={selectedOnboardingConnector} />}');
    expect(appSource).toContain('<SecondaryButton icon={<Clipboard />}');

    expectDeclarations(".onboarding-source-list", [
      "gap: 10px;",
    ]);
    expectDeclarations(".connector-option", [
      "grid-template-columns: 38px minmax(0, 1fr) auto;",
    ]);
    expectDeclarations(".connector-option.available", [
      "background: var(--onboarding-card-bg);",
    ]);
    expectDeclarations(".connector-option.selectable:hover:not(:disabled),\n.connector-option.selectable.selected", [
      "background: var(--onboarding-card-selected-bg);",
    ]);
    expectDeclarations(".connector-card-accessory", [
      "background: transparent;",
      "color: var(--muted);",
    ]);
    expectDeclarations(".connector-option.connected .connector-card-accessory", [
      "border: 1px solid var(--chip-border);",
      "background: var(--chip-bg);",
      "color: var(--chip-text);",
    ]);

    expectNoRawSurfaceColors(cssBlock(".connector-option.available"));
    expectNoRawSurfaceColors(cssBlock(".connector-option.selectable:hover:not(:disabled),\n.connector-option.selectable.selected"));
    expectNoRawSurfaceColors(cssBlock(".connector-card-accessory"));
    expectNoRawSurfaceColors(cssBlock(".connector-option.connected .connector-card-accessory"));
  });
});
