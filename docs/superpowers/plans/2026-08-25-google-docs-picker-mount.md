# Google Docs Picker Mount Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans task-by-task. Steps use checkbox syntax for tracking.

**Goal:** Replace the Google Docs Drive-folder connector with a flat mount of documents selected through Google Picker, with no restricted Drive metadata scope.

**Architecture:** OAuth retains Docs and drive.file access while removing Drive metadata scopes. Versioned GoogleDocsMountSettings owns selected IDs. The connector uses Docs API only; Desktop receives a short-lived Picker configuration and persists only IDs.

**Tech Stack:** Rust, TypeScript React/Vitest, Cloudflare Worker OAuth broker, Google Docs API, Google Picker.

---

### Task 1: Narrow the Google Docs OAuth contract

**Files:**
- Modify: crates/locality-auth-core/src/oauth.rs
- Modify: apps/oauth-service/src/oauth/google-docs.ts
- Modify: apps/oauth-service/test/app.test.ts
- Modify: connectors/registry.json
- Modify: connectors/oauth-verification/google-docs.json
- Test: crates/locality-auth-core/src/oauth.rs

- [ ] **Step 1: Write the failing exact scope test**

~~~
assert_eq!(GOOGLE_DOCS_REQUIRED_API_SCOPES, [
    "https://www.googleapis.com/auth/documents",
    "https://www.googleapis.com/auth/drive.file",
]);
assert!(validate_google_oauth_scopes(
    OAuthConnector::GoogleDocs,
    &["https://www.googleapis.com/auth/documents".into(),
      "https://www.googleapis.com/auth/drive.metadata.readonly".into()],
).is_err());
~~~

- [ ] **Step 2: Run it to verify failure**

Run: cargo test -p locality-auth-core google_docs --lib && pnpm --dir apps/oauth-service test -- --run app.test.ts

Expected: FAIL because drive.metadata.readonly remains in the broker URL and profile.

- [ ] **Step 3: Implement the minimum scope set**

~~~
pub const GOOGLE_DOCS_REQUIRED_API_SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/documents",
    "https://www.googleapis.com/auth/drive.file",
];
pub const GOOGLE_DOCS_LOCAL_BROKER_SCOPES: &[&str] = &[
    "openid", "email", "profile",
    "https://www.googleapis.com/auth/documents",
    "https://www.googleapis.com/auth/drive.file",
];
~~~

Apply the same list to the Worker, registry, verification fixture, and exact URL assertions. Reject the removed metadata scope on exchange and refresh.

- [ ] **Step 4: Run the focused tests**

Run: cargo test -p locality-auth-core google_docs --lib && pnpm --dir apps/oauth-service test -- --run app.test.ts

Expected: PASS; no runtime OAuth declaration contains drive.metadata.readonly.

- [ ] **Step 5: Commit**

~~~
git add crates/locality-auth-core/src/oauth.rs apps/oauth-service/src/oauth/google-docs.ts apps/oauth-service/test/app.test.ts connectors/registry.json connectors/oauth-verification/google-docs.json
git commit -m "feat: narrow Google Docs OAuth scopes"
~~~

### Task 2: Add durable selected-document settings and legacy safety

**Files:**
- Create: crates/locality-google-docs/src/settings.rs
- Modify: crates/locality-google-docs/src/lib.rs
- Modify: crates/localityd/src/google_docs.rs
- Test: crates/locality-google-docs/src/settings.rs
- Test: crates/localityd/tests/source_descriptor.rs

- [ ] **Step 1: Write failing settings and legacy-mount tests**

~~~
let settings = GoogleDocsMountSettings::from_document_ids(["doc-b", "doc-a", "doc-b"])?;
assert_eq!(settings.document_ids(), ["doc-a", "doc-b"]);
assert_eq!(settings.to_json()?,
    r#"{"google_docs":{"version":2,"document_ids":["doc-a","doc-b"]}}"#);

let legacy = MountConfig::new(MountId::new("docs"), "google-docs", "/tmp/docs")
    .with_remote_root_id(RemoteId::new("old-drive-folder"));
assert!(resolve_google_docs_connector_for_mount(&store, credentials, &legacy)
    .unwrap_err().to_string().contains("select Google Docs"));
~~~

- [ ] **Step 2: Run it to verify failure**

Run: cargo test -p locality-google-docs settings --lib && cargo test -p localityd source_descriptor --test source_descriptor

Expected: FAIL because no versioned selection format or legacy-mount outcome exists.

- [ ] **Step 3: Implement strict versioned selection settings**

~~~
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleDocsMountSettings {
    pub google_docs: GoogleDocsSelection,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleDocsSelection {
    pub version: u32,
    pub document_ids: Vec<String>,
}
~~~

Accept only version 2 and non-empty unique opaque IDs sorted deterministically. Missing selection or a legacy remote_root_id must produce an actionable migration-required error before remote calls and retain projections, shadows, journals, and pending work.

- [ ] **Step 4: Run the focused tests**

Run: cargo test -p locality-google-docs settings --lib && cargo test -p localityd source_descriptor --test source_descriptor

Expected: PASS; valid IDs resolve and old folder mounts pause safely.

- [ ] **Step 5: Commit**

~~~
git add crates/locality-google-docs/src/settings.rs crates/locality-google-docs/src/lib.rs crates/localityd/src/google_docs.rs crates/localityd/tests/source_descriptor.rs
git commit -m "feat: persist Google Docs picker selections"
~~~

### Task 3: Implement Docs-only flat projection and mutation boundary

**Files:**
- Modify: crates/locality-google-docs/src/client.rs
- Modify: crates/locality-google-docs/src/docs_dto.rs
- Modify: crates/locality-google-docs/src/render.rs
- Modify: crates/locality-google-docs/src/connector.rs
- Test: crates/locality-google-docs/src/connector.rs
- Test: crates/loc-cli/tests/e2e_push_workflow.rs

- [ ] **Step 1: Write failing behavior tests**

~~~
let connector = GoogleDocsConnector::with_documents(config, docs_api);
assert_eq!(connector.enumerate(request)?.len(), 2);
assert!(connector.apply(move_plan).is_err());
assert_eq!(fake_docs.create_calls(), 0);
~~~

Cover selected IDs only, root-level page paths, Docs title/body/revision, Docs create, body batch updates, and failure-before-call for rename, move, archive, folder creation, or non-root creation.

- [ ] **Step 2: Run tests to verify failure**

Run: cargo test -p locality-google-docs connector --lib && cargo test -p loc-cli google_docs --test e2e_push_workflow

Expected: FAIL because the connector requires a workspace folder and the Drive fake receives calls.

- [ ] **Step 3: Replace the connector Drive boundary**

~~~
pub trait GoogleDocsApi: std::fmt::Debug + Send + Sync {
    fn get_document(&self, document_id: &str) -> LocalityResult<GoogleDocument>;
    fn create_document(&self, title: &str) -> LocalityResult<GoogleDocument>;
    fn batch_update_document(&self, document_id: &str,
        request: BatchUpdateDocumentRequest) -> LocalityResult<GoogleDocument>;
}
~~~

Remove GoogleDriveApi, Drive DTOs, Drive native bundles, and combined Drive versions from the connector. Build tree entries, observations, frontmatter/search metadata, and versions from Docs ID/title/revision. Enumerate persisted IDs as slugified-title/page.md. Create root documents via POST /v1/documents. Allow body block operations only.

- [ ] **Step 4: Run Docs-only tests**

Run: cargo test -p locality-google-docs --lib && cargo test -p loc-cli google_docs --test e2e_push_workflow

Expected: PASS; selected pages are direct mount-root children and the Drive fake has no calls.

- [ ] **Step 5: Commit**

~~~
git add crates/locality-google-docs/src/client.rs crates/locality-google-docs/src/docs_dto.rs crates/locality-google-docs/src/render.rs crates/locality-google-docs/src/connector.rs crates/loc-cli/tests/e2e_push_workflow.rs
git commit -m "feat: mount selected Google Docs without Drive metadata"
~~~

### Task 4: Replace CLI and daemon folder behavior

**Files:**
- Modify: crates/loc-cli/src/commands.rs
- Modify: crates/loc-cli/tests/mount.rs
- Modify: crates/localityd/src/google_docs.rs
- Modify: crates/localityd/src/source.rs
- Modify: crates/localityd/src/push.rs
- Test: crates/localityd/tests/push_preparation.rs

- [ ] **Step 1: Write failing mount and guard tests**

~~~
assert!(run_loc(["mount", "google-docs", "/tmp/docs",
    "--document", "doc-a", "--document", "doc-b"]).is_ok());
assert!(run_loc(["mount", "google-docs", "/tmp/docs",
    "--workspace-folder", "old"]).is_err());
~~~

- [ ] **Step 2: Run tests to verify failure**

Run: cargo test -p loc-cli mount --test mount && cargo test -p localityd google_docs --test push_preparation

Expected: FAIL because workspace-folder is required and Drive operations remain available.

- [ ] **Step 3: Implement explicit document selection**

~~~
struct MountGoogleDocsArgs {
    path: String,
    #[arg(long = "document", value_name = "id-or-url", required = true)]
    documents: Vec<String>,
}
~~~

Normalize URLs, reject invalid/duplicate IDs, set remote_root_id to None, and persist canonical settings. Remove folder resolution. Update guidance and push guards to reject renames, moves, archives, and folders before mutation.

- [ ] **Step 4: Run CLI and daemon tests**

Run: cargo test -p loc-cli mount --test mount && cargo test -p localityd google_docs --test push_preparation

Expected: PASS; new mounts store IDs only and legacy mounts remain recoverable.

- [ ] **Step 5: Commit**

~~~
git add crates/loc-cli/src/commands.rs crates/loc-cli/tests/mount.rs crates/localityd/src/google_docs.rs crates/localityd/src/source.rs crates/localityd/src/push.rs crates/localityd/tests/push_preparation.rs
git commit -m "feat: configure Google Docs mounts by selection"
~~~

### Task 5: Add desktop multi-select Google Picker setup

**Files:**
- Modify: apps/desktop/src-tauri/src/main.rs
- Modify: apps/desktop/src/App.tsx
- Modify: apps/desktop/src/mounts.test.ts
- Create: apps/desktop/src/App.test.tsx if needed
- Test: apps/desktop/src-tauri/src/main.rs

- [ ] **Step 1: Write failing Picker UI/request tests**

~~~
expect(createDesktopMountRequest({
  connector: "google-docs", documentIds: ["doc-b", "doc-a"]
})).toMatchObject({ googleDocsDocumentIds: ["doc-a", "doc-b"] });
expect(screen.getByText("Choose Google Docs")).toBeVisible();
expect(screen.queryByText("Drive folder")).toBeNull();
~~~

- [ ] **Step 2: Run them to verify failure**

Run: pnpm --dir apps/desktop test -- --run mounts.test.ts App.test.tsx && cargo test -p locality-desktop google_docs_picker --lib

Expected: FAIL because the request and UI only implement a workspace folder.

- [ ] **Step 3: Implement Picker handoff and selection persistence**

Add googleDocsDocumentIds to CreateDesktopMountRequest. Add a Tauri command that returns developerKey, clientId, and the active connection token only for the local Picker session; read LOCALITY_GOOGLE_PICKER_DEVELOPER_KEY and error clearly when absent. Do not put the token into snapshots, logs, or durable state.

Load Picker once, configure Docs-only multi-select, and submit document IDs on PICKED. Replace every Drive-folder label, input, retry payload, guard, and success copy with a Choose Google Docs action and selection count. Encode selection through Rust settings before saving.

- [ ] **Step 4: Run desktop tests**

Run: pnpm --dir apps/desktop test -- --run mounts.test.ts App.test.tsx && cargo test -p locality-desktop google_docs_picker --lib

Expected: PASS; no setup copy mentions Drive folders and missing Picker configuration is actionable.

- [ ] **Step 5: Commit**

~~~
git add apps/desktop/src-tauri/src/main.rs apps/desktop/src/App.tsx apps/desktop/src/mounts.test.ts apps/desktop/src/App.test.tsx
git commit -m "feat: select Google Docs with Picker"
~~~

### Task 6: Update docs and verify

**Files:**
- Modify: docs/google-docs-connector.md
- Modify: docs-site/connectors/google-docs.mdx
- Modify: docs-site/cli-reference.mdx
- Modify: README.md
- Modify: tests/live_google_docs_vfs_roundtrip.sh

- [ ] **Step 1: Add exact documentation checks**

~~~
rg -q 'Google Picker' docs/google-docs-connector.md
rg -q -- '--document <id-or-url>' docs-site/cli-reference.mdx
! rg -q 'drive.metadata.readonly' connectors/registry.json
~~~

- [ ] **Step 2: Run them to confirm stale content**

Run: rg -n 'workspace folder|Drive metadata|drive.metadata.readonly' docs/google-docs-connector.md docs-site/connectors/google-docs.mdx docs-site/cli-reference.mdx README.md

Expected: stale Drive-folder references are reported.

- [ ] **Step 3: Document the contract**

Replace folder setup/hierarchy examples with selected Docs at mount root. Document scopes, Picker selection/reconfiguration, supported Docs-only operations, legacy migration, and LOCALITY_GOOGLE_PICKER_DEVELOPER_KEY. Update live testing to use explicit selected scratch Doc IDs without Drive listing/cleanup.

- [ ] **Step 4: Run complete verification**

Run: cargo test -p locality-auth-core -p locality-google-docs -p localityd -p loc-cli && pnpm --dir apps/oauth-service test && pnpm --dir apps/desktop test -- --run && git diff --check

Expected: PASS; all targeted broker, connector, daemon, CLI, and desktop tests are green with no restricted Drive metadata scope.

- [ ] **Step 5: Commit**

~~~
git add docs/google-docs-connector.md docs-site/connectors/google-docs.mdx docs-site/cli-reference.mdx README.md tests/live_google_docs_vfs_roundtrip.sh
git commit -m "docs: describe picker-based Google Docs mounts"
~~~

## Self-review

- Task 1 removes the restricted scope from runtime and verification declarations.
- Tasks 2 and 3 make selected IDs the only discovery input and eliminate connector Drive metadata.
- Task 4 preserves legacy durable state and pending work.
- Task 5 implements the approved Picker UX with session-only token exposure.
- Task 6 updates docs and runs targeted verification.

