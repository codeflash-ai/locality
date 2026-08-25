# Hosted Google Docs Picker Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the Desktop loopback Google Picker with a stateless hosted HTTPS Picker that returns selected Google Doc IDs through a secure Locality deep link.

**Architecture:** The OAuth broker creates encrypted, expiring browser and completion capabilities rather than persisting a Picker session. Desktop keeps an in-memory redemption secret, opens the hosted broker page, receives an opaque `locality://` completion deep link, and redeems it through the broker before invoking the existing mount creation flow.

**Tech Stack:** Cloudflare Workers, Hono, Web Crypto, Vitest, Rust, Tauri 2 deep-link plugin, React/Vitest.

**Spec:** `docs/superpowers/specs/2026-08-25-hosted-google-docs-picker-design.md`

## Global Constraints

- Retain only Google Docs `documents` and `drive.file` scopes; do not implement Drive enumeration or metadata access.
- Keep the OAuth broker stateless: do not add Durable Objects, KV, D1, or another persistence layer.
- Use the same Google Cloud project for the broker OAuth client, Picker API key, and numeric Picker app ID.
- Never persist or log Google access tokens, refresh handles, Picker API keys, document IDs, opaque browser capabilities, or completion capabilities.
- Broker capabilities expire in ten minutes; completion capabilities expire in five minutes.
- Desktop retains the random redemption secret and consumed capability IDs only in process memory.
- The hosted page uses `Cache-Control: no-store` and `Referrer-Policy: no-referrer`.

---

## File structure

- `apps/oauth-service/src/security/picker-capabilities.ts`: Capability payload definitions plus authenticated encryption, expiry, and redemption-secret verification.
- `apps/oauth-service/src/picker/google-docs.ts`: Hosted Picker page rendering and Google Docs Picker session/selection handlers.
- `apps/oauth-service/src/app.ts`: Wires the four picker routes into the existing Hono broker.
- `apps/oauth-service/src/types.ts`: Broker environment variables for Picker key and app ID.
- `apps/oauth-service/test/google-docs-picker.test.ts`: Endpoint and capability tests with mocked Google token refresh.
- `apps/oauth-service/wrangler.toml`, `.dev.vars.example`, README/deployment/security docs: broker configuration and hosted-flow contract.
- `apps/desktop/src-tauri/Cargo.toml`, `tauri.conf.json`, `src/main.rs`: deep-link plugin setup, broker client, pending-selection correlation, and removal of loopback TCP page code.
- `apps/desktop/src/google-docs-picker.test.ts`: Desktop command and deep-link behavior tests.

### Task 1: Add stateless Picker capabilities to the OAuth broker

**Files:**
- Create: `apps/oauth-service/src/security/picker-capabilities.ts`
- Modify: `apps/oauth-service/src/types.ts`
- Test: `apps/oauth-service/test/google-docs-picker.test.ts`

**Interfaces:**
- Produces `createPickerBrowserCapability(input: PickerBrowserCapabilityInput, secret: string): Promise<string>`.
- Produces `readPickerBrowserCapability(capability: string, secret: string, now?: number): Promise<PickerBrowserCapability>`.
- Produces `createPickerCompletionCapability(input: PickerCompletionCapabilityInput, secret: string): Promise<string>`.
- Produces `redeemPickerCompletionCapability(capability: string, redemptionSecret: string, secret: string, now?: number): Promise<string[]>`.
- Produces `sha256Base64Url(value: string): Promise<string>` for the Desktop redemption-secret binding.
- Adds `LOCALITY_GOOGLE_PICKER_DEVELOPER_KEY` and `LOCALITY_GOOGLE_PICKER_PROJECT_NUMBER` to `BrokerEnv`.

- [ ] **Step 1: Write failing capability tests**

```ts
it("redeems only the completion bound to the Desktop redemption secret", async () => {
  const completion = await createPickerCompletionCapability(
    {
      version: 1,
      connector: "google-docs",
      expires_at: 1_800_000_300,
      capability_id: "cap-1",
      redemption_secret_hash: await sha256Base64Url("desktop-secret"),
      document_ids: ["doc-a", "doc-b"]
    },
    env.LOCALITY_BROKER_SESSION_SECRET
  );

  await expect(
    redeemPickerCompletionCapability(completion, "wrong-secret", env.LOCALITY_BROKER_SESSION_SECRET, 1_800_000_000)
  ).rejects.toMatchObject({ code: "picker_redemption_denied" });
  await expect(
    redeemPickerCompletionCapability(completion, "desktop-secret", env.LOCALITY_BROKER_SESSION_SECRET, 1_800_000_000)
  ).resolves.toEqual(["doc-a", "doc-b"]);
});
```

- [ ] **Step 2: Run the test and verify it fails**

Run: `npm --prefix apps/oauth-service test -- google-docs-picker.test.ts`

Expected: FAIL because `picker-capabilities.ts` does not exist.

- [ ] **Step 3: Implement encrypted capability helpers**

Use a versioned AES-GCM envelope derived from `LOCALITY_BROKER_SESSION_SECRET`; use a distinct `locpicker_v1` prefix so Picker capabilities cannot be confused with refresh handles. Define these exact payloads:

```ts
export interface PickerBrowserCapability {
  version: 1;
  connector: "google-docs";
  expires_at: number;
  capability_id: string;
  refresh_token_handle: string;
  redemption_secret_hash: string;
}

export interface PickerCompletionCapability {
  version: 1;
  connector: "google-docs";
  expires_at: number;
  capability_id: string;
  redemption_secret_hash: string;
  document_ids: string[];
}
```

Reject malformed versions, expired capabilities, non-Google-Docs connectors, and a redemption-secret hash mismatch with stable `HttpError` codes. Canonicalize document IDs before capability creation: trim, reject empty values, deduplicate, and sort.

- [ ] **Step 4: Run focused capability tests**

Run: `npm --prefix apps/oauth-service test -- google-docs-picker.test.ts`

Expected: PASS, including expiry, malformed envelope, wrong secret, and canonical ID cases.

- [ ] **Step 5: Commit**

```bash
git add apps/oauth-service/src/security/picker-capabilities.ts apps/oauth-service/src/types.ts apps/oauth-service/test/google-docs-picker.test.ts
git commit -m "feat(auth): add stateless Google Picker capabilities"
```

### Task 2: Serve the hosted Picker and broker redemption endpoints

**Files:**
- Create: `apps/oauth-service/src/picker/google-docs.ts`
- Modify: `apps/oauth-service/src/app.ts`
- Modify: `apps/oauth-service/test/google-docs-picker.test.ts`

**Interfaces:**
- Consumes the Task 1 capability helpers and `refreshGoogleDocsToken`.
- Produces routes:
  - `POST /v1/google-docs/picker/sessions`
  - `GET /v1/google-docs/picker/:capability`
  - `POST /v1/google-docs/picker/:capability/selection`
  - `POST /v1/google-docs/picker/redeem`

- [ ] **Step 1: Write failing broker route tests**

```ts
it("returns only a hosted browser URL when creating a Picker session", async () => {
  const validHandle = await encryptJsonHandle(
    { v: 1, connector: "google-docs", refresh_token: "refresh-token", issued_at: 1_800_000_000 },
    env.LOCALITY_REFRESH_HANDLE_KEY
  );
  const response = await app.request(
    "/v1/google-docs/picker/sessions",
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        refresh_token_handle: validHandle,
        redemption_secret_hash: await sha256Base64Url("desktop-secret")
      })
    },
    env
  );

  expect(response.status).toBe(201);
  await expect(response.json()).resolves.toEqual({
    browser_url: expect.stringMatching(/^https:\/\/oauth\.locality\.test\/v1\/google-docs\/picker\//),
    expires_in: 600
  });
});

it("redirects selection to a Locality completion URL without exposing IDs", async () => {
  const browserCapability = await createPickerBrowserCapability(
    {
      version: 1,
      connector: "google-docs",
      expires_at: 1_800_000_600,
      capability_id: "cap-1",
      refresh_token_handle: "locrh_v1.example",
      redemption_secret_hash: await sha256Base64Url("desktop-secret")
    },
    env.LOCALITY_BROKER_SESSION_SECRET
  );
  const response = await app.request(
    `/v1/google-docs/picker/${browserCapability}/selection`,
    { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ document_ids: ["doc-b", "doc-a"] }) },
    env
  );

  expect(response.status).toBe(303);
  expect(response.headers.get("location")).toMatch(/^locality:\/\/google-docs-picker\?completion=locpicker_v1\./);
  expect(response.headers.get("location")).not.toContain("doc-a");
});
```

- [ ] **Step 2: Run the route test and verify it fails**

Run: `npm --prefix apps/oauth-service test -- google-docs-picker.test.ts`

Expected: FAIL with route-not-found responses.

- [ ] **Step 3: Implement the four routes and hosted page**

Session creation must require a valid opaque Google Docs refresh handle, decrypt it only with `resolveRefreshToken`, and return a browser URL built from `LOCALITY_BROKER_PUBLIC_BASE_URL`.

The hosted GET route refreshes the access token server-side and serves one static HTML document. It must configure Google Picker with:

```js
new google.picker.PickerBuilder()
  .setDeveloperKey(configuration.developerKey)
  .setAppId(configuration.projectNumber)
  .setOAuthToken(configuration.accessToken)
  .setOrigin(window.location.origin)
  .addView(new google.picker.DocsView(google.picker.ViewId.DOCUMENTS)
    .setIncludeFolders(false)
    .setSelectFolderEnabled(false)
    .setMimeTypes("application/vnd.google-apps.document"))
  .enableFeature(google.picker.Feature.MULTISELECT_ENABLED)
  .setCallback(onPickerResponse)
  .build();
```

`onPickerResponse` posts IDs once to the selection route. On `PICKED`, it must accept only non-empty strings from `doc.id`; on error it must replace the page body with visible recovery guidance. The POST response must be a 303 redirect to the Locality deep link.

The redeem route must require both `completion` and `redemption_secret`, return `{"document_ids":[...]}`, and must not return the capability identifier or any token material.

- [ ] **Step 4: Run focused broker tests**

Run: `npm --prefix apps/oauth-service test -- google-docs-picker.test.ts`

Expected: PASS for capability expiry, invalid handle, hosted-page no-store headers, selection redirect, wrong secret, and redemption.

- [ ] **Step 5: Commit**

```bash
git add apps/oauth-service/src/picker/google-docs.ts apps/oauth-service/src/app.ts apps/oauth-service/test/google-docs-picker.test.ts
git commit -m "feat(auth): host stateless Google Docs Picker"
```

### Task 3: Register and consume the Locality deep link in Desktop

**Files:**
- Modify: `apps/desktop/src-tauri/Cargo.toml`
- Modify: `apps/desktop/src-tauri/tauri.conf.json`
- Modify: `apps/desktop/src-tauri/src/main.rs`
- Test: `apps/desktop/src-tauri/src/main.rs`

**Interfaces:**
- Consumes broker response `{ browser_url: string; expires_in: number }` and redemption response `{ document_ids: string[] }`.
- Produces `choose_google_docs_in_browser() -> Result<Vec<String>, String>` backed by deep-link completion instead of TCP.
- Registers only `locality://google-docs-picker?completion=<capability>`.

- [ ] **Step 1: Write failing Desktop tests**

```rust
#[test]
fn google_docs_picker_deep_link_rejects_wrong_host_and_missing_completion() {
    assert!(parse_google_docs_picker_deep_link("locality://other?completion=value").is_err());
    assert!(parse_google_docs_picker_deep_link("locality://google-docs-picker").is_err());
}

#[test]
fn google_docs_picker_deep_link_accepts_an_opaque_completion() {
    assert_eq!(
        parse_google_docs_picker_deep_link("locality://google-docs-picker?completion=locpicker_v1.value")
            .unwrap(),
        "locpicker_v1.value"
    );
}
```

- [ ] **Step 2: Run the tests and verify they fail**

Run: `cargo test -p locality-desktop --bin locality-desktop google_docs_picker_deep_link`

Expected: FAIL because the parser and deep-link plugin are absent.

- [ ] **Step 3: Add deep-link plugin and pending-request coordination**

Add `tauri-plugin-deep-link` at the compatible Tauri 2 version, initialize it in the app builder, and declare `locality` as the allowed deep-link scheme in `tauri.conf.json` for every packaged platform.

Replace the `TcpListener`, HTTP parser, browser-page generator, and picker configuration command with:

```rust
struct PendingGoogleDocsPicker {
    capability_id: String,
    redemption_secret: String,
    completion: std::sync::mpsc::Receiver<String>,
}
```

Create the broker session with the OS-stored `refresh_token_handle`, retain its redemption secret only in this struct, open `browser_url`, await a matching deep-link completion for ten minutes, and redeem it. Store consumed capability IDs in an in-memory `HashSet` before mount creation. Do not add the redemption secret, completion capability, Google access token, API key, or document IDs to `DesktopSnapshot`, logs, or SQLite.

Use a strict parser: scheme `locality`, host `google-docs-picker`, no path, exactly one non-empty `completion` query parameter, no fragments.

- [ ] **Step 4: Run focused Desktop tests**

Run: `cargo test -p locality-desktop --bin locality-desktop google_docs_picker`

Expected: PASS for URL parsing, duplicate completion rejection, timeout, broker request shape, and canonical IDs forwarded to mount setup.

- [ ] **Step 5: Commit**

```bash
git add apps/desktop/src-tauri/Cargo.toml apps/desktop/src-tauri/tauri.conf.json apps/desktop/src-tauri/src/main.rs
git commit -m "feat(desktop): redeem hosted Google Picker selections"
```

### Task 4: Route Desktop setup through the hosted Picker and remove loopback behavior

**Files:**
- Modify: `apps/desktop/src/App.tsx`
- Modify: `apps/desktop/src/google-docs-picker.test.ts`
- Modify: `docs/google-docs-connector.md`
- Modify: `docs/cli.md`

**Interfaces:**
- Consumes `choose_google_docs_in_browser` from Task 3 with the unchanged return type `Promise<string[]>`.
- Produces user-facing setup copy that accurately describes hosted Picker and deep-link return behavior.

- [ ] **Step 1: Write failing UI contract tests**

```ts
it("uses the hosted-picker desktop command", () => {
  expect(googleDocsPickerCommand()).toBe("choose_google_docs_in_browser");
  expect(googleDocsSelectionNeededForMount("google-docs", [])).toBe(true);
});
```

Add a source setup test that an empty or expired hosted selection leaves the source dialog idle with the broker error, rather than attempting `create_desktop_mount`.

- [ ] **Step 2: Run UI tests and verify they fail for the new error behavior**

Run: `apps/desktop/node_modules/.bin/vitest run --root apps/desktop google-docs-picker.test.ts source-setup.test.ts`

Expected: FAIL until expired-selection errors are surfaced by the setup flow.

- [ ] **Step 3: Implement minimal UI error handling and documentation**

Keep all existing call sites on `chooseGoogleDocs`; do not expose API keys, OAuth tokens, completion capabilities, or raw document IDs in UI messages. Update docs to say that Desktop opens a hosted HTTPS Picker page, which returns to Locality through a short-lived deep link. Remove statements that describe a loopback HTTP listener or Desktop Picker configuration environment variables.

- [ ] **Step 4: Run Desktop UI verification**

Run: `apps/desktop/node_modules/.bin/vitest run --root apps/desktop google-docs-picker.test.ts source-setup.test.ts`

Run: `apps/desktop/node_modules/.bin/tsc --noEmit -p apps/desktop/tsconfig.json`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add apps/desktop/src/App.tsx apps/desktop/src/google-docs-picker.test.ts docs/google-docs-connector.md docs/cli.md
git commit -m "docs: describe hosted Google Docs Picker flow"
```

### Task 5: Verify broker configuration and full integration

**Files:**
- Modify: `apps/oauth-service/wrangler.toml`
- Modify: `apps/oauth-service/.dev.vars.example`
- Modify: `apps/oauth-service/README.md`
- Modify: `apps/oauth-service/docs/deployment.md`
- Modify: `apps/oauth-service/docs/security.md`
- Test: `apps/oauth-service/test/google-docs-picker.test.ts`

**Interfaces:**
- Requires `LOCALITY_GOOGLE_PICKER_DEVELOPER_KEY` and `LOCALITY_GOOGLE_PICKER_PROJECT_NUMBER` on the broker.
- Requires the project number to match the numeric prefix of `LOCALITY_GOOGLE_CLIENT_ID`.

- [ ] **Step 1: Write failing configuration validation tests**

```ts
it("rejects a Picker project number that does not match the OAuth client project", async () => {
  const response = await app.request("/v1/google-docs/picker/sessions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ refresh_token_handle: "locrh_v1.example", redemption_secret_hash: "hash" })
  }, {
    ...env,
    LOCALITY_GOOGLE_CLIENT_ID: "123456789-client.apps.googleusercontent.com",
    LOCALITY_GOOGLE_PICKER_PROJECT_NUMBER: "987654321"
  });

  expect(response.status).toBe(500);
  await expect(response.json()).resolves.toMatchObject({ error: { code: "picker_project_mismatch" } });
});
```

- [ ] **Step 2: Run configuration test and verify it fails**

Run: `npm --prefix apps/oauth-service test -- google-docs-picker.test.ts`

Expected: FAIL until Picker configuration validation exists.

- [ ] **Step 3: Implement deployment validation and docs**

Require a non-empty Picker API key, numeric project number, and an OAuth client ID whose numeric project prefix matches the configured Picker project number. Document that the key must be a browser key for the broker HTTPS origin and has the Google Picker API enabled. Do not accept Desktop environment overrides for these values.

- [ ] **Step 4: Run full relevant verification**

Run: `npm --prefix apps/oauth-service run check`

Run: `cargo fmt --all -- --check`

Run: `cargo test -p locality-desktop --bin locality-desktop google_docs_picker`

Run: `apps/desktop/node_modules/.bin/vitest run --root apps/desktop google-docs-picker.test.ts source-setup.test.ts`

Run: `apps/desktop/node_modules/.bin/tsc --noEmit -p apps/desktop/tsconfig.json`

Run: `git diff --check`

Expected: all commands exit 0.

- [ ] **Step 5: Commit**

```bash
git add apps/oauth-service/wrangler.toml apps/oauth-service/.dev.vars.example apps/oauth-service/README.md apps/oauth-service/docs/deployment.md apps/oauth-service/docs/security.md apps/oauth-service/test/google-docs-picker.test.ts
git commit -m "docs(auth): configure hosted Google Picker"
```
