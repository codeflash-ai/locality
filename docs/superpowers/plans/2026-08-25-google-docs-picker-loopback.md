# Google Docs Picker Loopback Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Google Docs selection work from packaged Locality desktop builds by hosting Picker on a one-use loopback HTTP page.

**Architecture:** The Rust desktop process binds an ephemeral `127.0.0.1` listener, generates a random session token, and opens the tokenized page in the system browser. The page loads Google Picker with the active local OAuth token and posts chosen IDs to a token-bound endpoint; the command awaits those IDs and the existing React flows create or reconfigure the mount.

**Tech Stack:** Rust std TCP networking, `getrandom`, Tauri shell opening, React/TypeScript, Vitest, Rust unit tests.

---

### Task 1: Loopback Picker session

**Files:**

- Modify: `apps/desktop/src-tauri/Cargo.toml`
- Modify: `apps/desktop/src-tauri/src/main.rs`
- Test: `apps/desktop/src-tauri/src/main.rs`

- [ ] Write a failing Rust test that a Picker submission with the wrong session token is rejected.
- [ ] Implement an ephemeral `127.0.0.1` listener, a random one-use token, a tokenized GET Picker page, and a tokenized POST result endpoint.
- [ ] Open the tokenized URL in the system browser, validate a JSON list of non-empty Docs IDs, and time out safely if no result arrives.
- [ ] Run `cargo test -p locality-desktop --bin locality-desktop google_docs_picker` and commit.

### Task 2: Route desktop selection through loopback

**Files:**

- Modify: `apps/desktop/src/App.tsx`
- Modify: `apps/desktop/src/google-docs-picker.test.ts`

- [ ] Write a failing Vitest assertion for `choose_google_docs_in_browser`.
- [ ] Replace the in-webview Picker loader with the command while preserving existing mount persistence.
- [ ] Run `apps/desktop/node_modules/.bin/vitest run --root apps/desktop google-docs-picker.test.ts mounts.test.ts` and `apps/desktop/node_modules/.bin/tsc --noEmit -p apps/desktop/tsconfig.json`, then commit.

### Task 3: Document and verify the packaged flow

**Files:**

- Modify: `docs/google-docs-connector.md`
- Modify: `docs/cli.md`

- [ ] State that Desktop opens Picker in the default browser because Google requires an HTTP(S) Picker origin and that the short-lived loopback session does not persist OAuth tokens.
- [ ] Run `cargo fmt --all -- --check`, focused Rust and desktop tests, `git diff --check`, commit, and push.
