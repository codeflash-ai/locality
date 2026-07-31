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
});
