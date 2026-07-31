export type OnboardingStep = 1 | 2 | 3 | 4 | 5;
export type OnboardingProgressStep = 1 | 2 | 3 | 4;
export type OnboardingProgressSegmentState = "complete" | "pending";

export type OnboardingProgressSegment = {
  step: OnboardingProgressStep;
  state: OnboardingProgressSegmentState;
};

export function onboardingProgressStep(
  step: OnboardingStep,
  optionalGuideReturnStep: OnboardingStep | null = null,
): OnboardingProgressStep {
  const resolvedStep = step === 2 ? optionalGuideReturnStep ?? 1 : step;

  if (resolvedStep >= 5) {
    return 4;
  }
  if (resolvedStep === 4) {
    return 3;
  }
  if (resolvedStep === 3) {
    return 2;
  }
  return 1;
}

export function onboardingStepMeta(
  step: OnboardingStep,
  optionalGuideReturnStep: OnboardingStep | null = null,
) {
  return `${onboardingProgressStep(step, optionalGuideReturnStep)}/4`;
}

export function onboardingProgressSegments(
  currentStep: OnboardingProgressStep,
): OnboardingProgressSegment[] {
  return ([1, 2, 3, 4] as OnboardingProgressStep[]).map((step) => ({
    step,
    state: step <= currentStep ? "complete" : "pending",
  }));
}
