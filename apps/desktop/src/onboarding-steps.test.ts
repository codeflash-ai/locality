import { describe, expect, it } from "vitest";

import {
  onboardingProgressSegments,
  onboardingProgressStep,
  onboardingStepMeta,
  type OnboardingStep,
} from "./onboarding-steps";

describe("onboarding visible progress", () => {
  it("maps internal onboarding screens onto the four visible progress steps", () => {
    const cases: Array<[OnboardingStep, number]> = [
      [1, 1],
      [2, 1],
      [3, 2],
      [4, 3],
      [5, 4],
    ];

    for (const [step, visibleStep] of cases) {
      expect(onboardingProgressStep(step)).toBe(visibleStep);
      expect(onboardingStepMeta(step)).toBe(`${visibleStep}/4`);
    }
  });

  it("uses the return step when the optional guide is open", () => {
    expect(onboardingProgressStep(2, 1)).toBe(1);
    expect(onboardingProgressStep(2, 3)).toBe(2);
    expect(onboardingProgressStep(2, 5)).toBe(4);
    expect(onboardingStepMeta(2, 5)).toBe("4/4");
  });

  it("marks completed rail segments for the current visible step", () => {
    expect(onboardingProgressSegments(1)).toEqual([
      { step: 1, state: "complete" },
      { step: 2, state: "pending" },
      { step: 3, state: "pending" },
      { step: 4, state: "pending" },
    ]);

    expect(onboardingProgressSegments(4)).toEqual([
      { step: 1, state: "complete" },
      { step: 2, state: "complete" },
      { step: 3, state: "complete" },
      { step: 4, state: "complete" },
    ]);
  });
});
