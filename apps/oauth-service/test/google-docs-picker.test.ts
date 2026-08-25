import { describe, expect, it } from "vitest";
import {
  createPickerBrowserCapability,
  createPickerCompletionCapability,
  readPickerBrowserCapability,
  redeemPickerCompletionCapability,
  sha256Base64Url
} from "../src/security/picker-capabilities";

const brokerSecret = "test-picker-capability-secret-with-enough-entropy";

describe("Google Docs Picker capabilities", () => {
  it("redeems selected IDs only with the Desktop redemption secret", async () => {
    const completion = await createPickerCompletionCapability(
      {
        version: 1,
        connector: "google-docs",
        expires_at: 1_800_000_300,
        capability_id: "capability-1",
        redemption_secret_hash: await sha256Base64Url("desktop-redemption-secret"),
        document_ids: ["doc-b", "doc-a", "doc-b"]
      },
      brokerSecret
    );

    await expect(
      redeemPickerCompletionCapability(completion, "another-desktop-secret", brokerSecret, 1_800_000_000)
    ).rejects.toMatchObject({ code: "picker_redemption_denied" });

    await expect(
      redeemPickerCompletionCapability(completion, "desktop-redemption-secret", brokerSecret, 1_800_000_000)
    ).resolves.toEqual(["doc-a", "doc-b"]);
  });

  it("rejects an expired browser capability before it can refresh Google access", async () => {
    const browserCapability = await createPickerBrowserCapability(
      {
        version: 1,
        connector: "google-docs",
        expires_at: 1_800_000_000,
        capability_id: "capability-2",
        refresh_token_handle: "locrh_v1.opaque-refresh-handle",
        redemption_secret_hash: await sha256Base64Url("desktop-redemption-secret")
      },
      brokerSecret
    );

    await expect(readPickerBrowserCapability(browserCapability, brokerSecret, 1_800_000_001)).rejects.toMatchObject({
      code: "invalid_picker_capability"
    });
  });
});
