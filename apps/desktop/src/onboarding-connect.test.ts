import { describe, expect, it } from "vitest";

import { copyLoginLinkDisabled, loginLinkFlowMode } from "./onboarding-connect";

describe("loginLinkFlowMode", () => {
  it("starts the no-browser OAuth flow when the user copies the login link before connecting", () => {
    expect(
      loginLinkFlowMode({
        connectionReady: false,
        oauthInFlight: false,
        loginUrl: "",
      }),
    ).toBe("start-without-browser");
  });

  it("reuses the existing login link while OAuth is already in flight", () => {
    expect(
      loginLinkFlowMode({
        connectionReady: false,
        oauthInFlight: true,
        loginUrl: "",
      }),
    ).toBe("copy-existing-link");
    expect(
      loginLinkFlowMode({
        connectionReady: false,
        oauthInFlight: true,
        loginUrl: "https://api.notion.com/v1/oauth/authorize?state=abc",
      }),
    ).toBe("copy-existing-link");
  });

  it("does not start a fresh OAuth flow after the workspace is already connected", () => {
    expect(
      loginLinkFlowMode({
        connectionReady: true,
        oauthInFlight: false,
        loginUrl: "",
      }),
    ).toBe("copy-existing-link");
  });
});

describe("copyLoginLinkDisabled", () => {
  it("keeps the copy login link action enabled before the user starts OAuth", () => {
    expect(
      copyLoginLinkDisabled({
        connectionReady: false,
        oauthInFlight: false,
      }),
    ).toBe(false);
  });

  it("disables the copy login link action after the workspace is already connected", () => {
    expect(
      copyLoginLinkDisabled({
        connectionReady: true,
        oauthInFlight: false,
      }),
    ).toBe(true);
  });
});
