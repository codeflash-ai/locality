export type ReviewablePushAction = {
  state: "working" | "success" | "error";
  message: string;
};

export function pushNeedsReview(message: string) {
  const normalized = message.toLocaleLowerCase();
  return normalized.includes("open review push") || normalized.includes("needs review");
}

export function shouldReviewBeforePush({
  confirmDangerous,
  changeState,
  action,
  canOpenReview,
}: {
  confirmDangerous: boolean;
  changeState: string;
  action?: ReviewablePushAction;
  canOpenReview: boolean;
}) {
  if (confirmDangerous || !canOpenReview) {
    return false;
  }

  return changeState === "needs_review" || Boolean(action?.state === "error" && pushNeedsReview(action.message));
}

export function shouldOpenReviewAfterPush({
  action,
  reportOk,
  message,
  canOpenReview,
}: {
  action: string;
  reportOk: boolean;
  message: string;
  canOpenReview: boolean;
}) {
  return action === "push" && !reportOk && canOpenReview && pushNeedsReview(message);
}
