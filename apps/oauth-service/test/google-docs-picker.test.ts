import { describe, expect, it, vi } from "vitest";
import app from "../src/app";
import { encryptJsonHandle } from "../src/security/crypto";
import {
  createPickerBrowserCapability,
  createPickerCompletionCapability,
  readPickerBrowserCapability,
  redeemPickerCompletionCapability,
  sha256Base64Url
} from "../src/security/picker-capabilities";
import type { BrokerEnv } from "../src/types";

const brokerSecret = "test-picker-capability-secret-with-enough-entropy";
const env: BrokerEnv = {
  LOCALITY_BROKER_SESSION_SECRET: brokerSecret,
  LOCALITY_REFRESH_HANDLE_KEY: "test-refresh-handle-key-with-enough-entropy",
  LOCALITY_TOKEN_MODE: "handle",
  LOCALITY_BROKER_PUBLIC_BASE_URL: "https://oauth.locality.test",
  LOCALITY_NOTION_CLIENT_ID: "notion-client-id",
  LOCALITY_NOTION_CLIENT_SECRET: "notion-client-secret",
  LOCALITY_GOOGLE_CLIENT_ID: "123456789-client.apps.googleusercontent.com",
  LOCALITY_GOOGLE_CLIENT_SECRET: "google-client-secret",
  LOCALITY_GOOGLE_PICKER_DEVELOPER_KEY: "picker-api-key",
  LOCALITY_GOOGLE_PICKER_PROJECT_NUMBER: "123456789",
  LOCALITY_SLACK_CLIENT_ID: "slack-client-id",
  LOCALITY_SLACK_CLIENT_SECRET: "slack-client-secret"
};

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

  it("creates a hosted browser URL without returning Google credentials", async () => {
    const refreshTokenHandle = await encryptJsonHandle(
      { v: 1, connector: "google-docs", refresh_token: "provider-refresh-token", issued_at: 1_800_000_000 },
      env.LOCALITY_REFRESH_HANDLE_KEY!
    );
    const response = await app.request(
      "/v1/google-docs/picker/sessions",
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          refresh_token_handle: refreshTokenHandle,
          redemption_secret_hash: await sha256Base64Url("desktop-redemption-secret")
        })
      },
      env
    );

    expect(response.status).toBe(201);
    await expect(response.json()).resolves.toEqual({
      browser_url: expect.stringMatching(/^https:\/\/oauth\.locality\.test\/v1\/google-docs\/picker\/locpicker_v1\./),
      expires_in: 600
    });
  });

  it("redirects selected IDs to Locality without disclosing them in the URL", async () => {
    const capability = await createPickerBrowserCapability(
      {
        version: 1,
        connector: "google-docs",
        expires_at: Math.floor(Date.now() / 1000) + 600,
        capability_id: "capability-3",
        refresh_token_handle: "locrh_v1.opaque-refresh-handle",
        redemption_secret_hash: await sha256Base64Url("desktop-redemption-secret")
      },
      brokerSecret
    );
    const response = await app.request(
      `/v1/google-docs/picker/${capability}/selection`,
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ document_ids: ["doc-b", "doc-a"] })
      },
      env
    );

    expect(response.status).toBe(303);
    const location = response.headers.get("location") ?? "";
    expect(location).toMatch(/^locality:\/\/google-docs-picker\?completion=locpicker_v1\./);
    expect(location).not.toContain("doc-a");
    expect(location).not.toContain("doc-b");
  });

  it("returns selected IDs only after Desktop redeems the completion secret", async () => {
    const completion = await createPickerCompletionCapability(
      {
        version: 1,
        connector: "google-docs",
        expires_at: Math.floor(Date.now() / 1000) + 300,
        capability_id: "capability-4",
        redemption_secret_hash: await sha256Base64Url("desktop-redemption-secret"),
        document_ids: ["doc-b", "doc-a"]
      },
      brokerSecret
    );
    const response = await app.request(
      "/v1/google-docs/picker/redeem",
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ completion, redemption_secret: "desktop-redemption-secret" })
      },
      env
    );

    expect(response.status).toBe(200);
    await expect(response.json()).resolves.toEqual({ document_ids: ["doc-a", "doc-b"] });
  });

  it("serves a no-store hosted Picker page without exposing the refresh handle", async () => {
    const originalFetch = globalThis.fetch;
    globalThis.fetch = vi.fn(async () =>
      Response.json({ access_token: "short-lived-access-token", token_type: "Bearer", expires_in: 3600 })
    ) as unknown as typeof fetch;
    try {
      const refreshTokenHandle = await encryptJsonHandle(
        { v: 1, connector: "google-docs", refresh_token: "provider-refresh-token", issued_at: 1_800_000_000 },
        env.LOCALITY_REFRESH_HANDLE_KEY!
      );
      const capability = await createPickerBrowserCapability(
        {
          version: 1,
          connector: "google-docs",
          expires_at: Math.floor(Date.now() / 1000) + 600,
          capability_id: "capability-5",
          refresh_token_handle: refreshTokenHandle,
          redemption_secret_hash: await sha256Base64Url("desktop-redemption-secret")
        },
        brokerSecret
      );
      const response = await app.request(`/v1/google-docs/picker/${capability}`, { method: "GET" }, env);

      expect(response.status).toBe(200);
      expect(response.headers.get("cache-control")).toBe("no-store");
      expect(response.headers.get("referrer-policy")).toBe("no-referrer");
      const page = await response.text();
      expect(page).toContain("short-lived-access-token");
      expect(page).not.toContain("opaque-refresh-handle");
      expect(page).toContain("setDeveloperKey");
      expect(page).toContain("setOAuthToken");
    } finally {
      globalThis.fetch = originalFetch;
    }
  });
});
