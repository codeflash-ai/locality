import { describe, expect, it } from "vitest";
import { pushNeedsReview, shouldOpenReviewAfterPush, shouldReviewBeforePush } from "./review-push";

describe("review push routing", () => {
  it("routes a card to approval after a normal push is rejected for review", () => {
    const action = {
      state: "error" as const,
      message:
        "This push needs review because it may move, archive, or touch a large amount of Notion content. Open Review Push to approve it.",
    };

    expect(pushNeedsReview(action.message)).toBe(true);
    expect(
      shouldReviewBeforePush({
        confirmDangerous: false,
        changeState: "pending_changes",
        action,
        canOpenReview: true,
      }),
    ).toBe(true);
    expect(
      shouldOpenReviewAfterPush({
        action: "push",
        reportOk: false,
        message: action.message,
        canOpenReview: true,
      }),
    ).toBe(true);
  });

  it("allows the reviewed approval screen to execute the confirmed push", () => {
    expect(
      shouldReviewBeforePush({
        confirmDangerous: true,
        changeState: "needs_review",
        action: {
          state: "error",
          message: "This push needs review. Open Review Push to approve it.",
        },
        canOpenReview: true,
      }),
    ).toBe(false);
  });
});
