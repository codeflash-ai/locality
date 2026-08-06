# Gmail Remote Draft Edit And Send Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` for task dispatch, or `superpowers:executing-plans` for inline execution. Execute this plan task-by-task and update each checkbox as it completes.

**Goal:** Gmail drafts projected under `draft/` can be viewed from the remote account, edited locally, pushed back as Gmail drafts, or moved to `outbox/` and pushed to send the updated draft.

**Architecture:** Treat Gmail drafts as stable Gmail draft resources, not as message-only projections. Draft entities use Locality remote IDs of the form `gmail-draft:<draft_id>`, render both `gmail.draft_id` and the current contained `gmail.message_id`, and route draft updates through `users.drafts.update`. Moving a remote draft file from `draft/` to `outbox/` updates that draft with the local state, sends it with `users.drafts.send`, reconciles the sent message under `sent/`, and retires the old draft entity.

**Tech Stack:** Rust workspace, `locality-gmail`, `localityd`, `locality-core`, Gmail REST draft resource, existing Locality push journal, projection, virtual mutation, and reconcile pipeline.

---

## Current Baseline

- `draft/` already exists and is populated by listing Gmail messages with the `DRAFT` label.
- Existing draft entries currently use the contained Gmail message ID as the entity remote ID.
- Creating a new Markdown file under `draft/` creates a Gmail draft.
- Creating a new Markdown file under `outbox/` sends a new Gmail message.
- `GmailApi` already has `create_draft`, `send_message`, and `send_draft`, but it does not expose `list_drafts`, `get_draft_full`, or `update_draft`.
- `GmailConnector::supported_push_operations()` currently returns only `CreateEntity`.
- `source_move_decision_for_parent_path()` currently rejects all Gmail moves.
- The generic push reconciler expects `MoveEntity` to move the same remote ID to the destination parent. Draft-to-outbox send does not fit that shape because the operation consumes a draft and creates a sent message.

## Target User Semantics

- `draft/` shows remote Gmail drafts.
- Editing a direct child Markdown file in `draft/` and pushing updates the Gmail draft.
- Creating a new direct child Markdown file in `draft/` creates a new unsent Gmail draft.
- Creating a new direct child Markdown file in `outbox/` sends a new message immediately on push.
- Moving a remote draft file directly from `draft/` to `outbox/` and pushing sends the existing Gmail draft after first updating it from the local Markdown state.
- `outbox/` remains local-only staging. It does not enumerate remote children.
- Attachments in outbound Gmail documents remain unsupported and must be rejected before push.
- Inbox and sent messages remain read-only.

## Files To Change

- `crates/locality-gmail/src/dto.rs`
- `crates/locality-gmail/src/client.rs`
- `crates/locality-gmail/src/render.rs`
- `crates/locality-gmail/src/connector.rs`
- `crates/localityd/src/source.rs`
- `crates/localityd/src/gmail.rs`
- `crates/localityd/src/push.rs`
- `crates/localityd/tests/source_descriptor.rs`
- `crates/localityd/tests/push_preparation.rs`
- `crates/localityd/tests/push_execution.rs`
- `tests/live_gmail_vfs_roundtrip.sh`
- `docs/gmail-connector.md`
- `docs/cli.md`
- `docs/daemon.md`
- `docs/agent-guidance.md`
- `apps/desktop/src-tauri/src/agent_guidance.rs`

## Implementation Steps

### Task 1: Add Gmail Draft API DTOs And Client Methods

- [ ] Add failing unit coverage for draft API request paths in `crates/locality-gmail/src/client.rs`.
  - Add a test server case for `GET /users/me/drafts?maxResults=100`.
  - Add a test server case for `GET /users/me/drafts/<draft_id>?format=full`.
  - Add a test server case for `PUT /users/me/drafts/<draft_id>`.
  - Keep the existing create draft and send tests passing.

- [ ] Add DTOs to `crates/locality-gmail/src/dto.rs`:

```rust
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmailDraftList {
    #[serde(default)]
    pub drafts: Vec<GmailDraftRef>,
    pub next_page_token: Option<String>,
    pub result_size_estimate: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmailDraftRef {
    pub id: String,
    pub message: GmailMessage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailDraftUpdateRequest {
    pub message: GmailRawMessage,
}
```

- [ ] Extend `GmailApi` in `crates/locality-gmail/src/client.rs`:

```rust
fn list_drafts(
    &self,
    max_results: u32,
    page_token: Option<&str>,
    query: Option<&str>,
) -> LocalityResult<GmailDraftList>;
fn get_draft_full(&self, draft_id: &str) -> LocalityResult<GmailDraft>;
fn update_draft(
    &self,
    draft_id: &str,
    request: GmailDraftUpdateRequest,
) -> LocalityResult<GmailDraft>;
```

- [ ] Add `put_json_with_context` beside `post_json_with_context`:

```rust
fn put_json_with_context<T, B>(&self, path: &str, body: &B, context: &str) -> LocalityResult<T>
where
    T: DeserializeOwned,
    B: Serialize + ?Sized,
{
    decode_response(
        self.client
            .put(format!("{}{}", self.base_url, path))
            .bearer_auth(&self.access_token)
            .json(body)
            .send(),
        context,
    )
}
```

- [ ] Implement the new HTTP methods:

```rust
fn list_drafts(
    &self,
    max_results: u32,
    page_token: Option<&str>,
    search_query: Option<&str>,
) -> LocalityResult<GmailDraftList> {
    let mut params = vec![("maxResults".to_string(), max_results.to_string())];
    if let Some(page_token) = page_token {
        params.push(("pageToken".to_string(), page_token.to_string()));
    }
    if let Some(search_query) = search_query {
        params.push(("q".to_string(), search_query.to_string()));
    }
    self.get_json("/users/me/drafts", params)
}

fn get_draft_full(&self, draft_id: &str) -> LocalityResult<GmailDraft> {
    let draft_id = percent_encode_path_segment(draft_id);
    self.get_json(
        &format!("/users/me/drafts/{draft_id}"),
        vec![("format".to_string(), "full".to_string())],
    )
}

fn update_draft(
    &self,
    draft_id: &str,
    request: GmailDraftUpdateRequest,
) -> LocalityResult<GmailDraft> {
    let draft_id = percent_encode_path_segment(draft_id);
    self.put_json_with_context(
        &format!("/users/me/drafts/{draft_id}"),
        &request,
        "gmail draft update",
    )
}
```

- [ ] Update all fake `GmailApi` implementations in tests to implement the new trait methods.

- [ ] Run:

```bash
cargo test -p locality-gmail client
```

Expected result:

```text
test result: ok
```

### Task 2: Project Remote Drafts With Stable Draft IDs

- [ ] Add remote ID helpers in `crates/locality-gmail/src/connector.rs`:

```rust
const DRAFT_REMOTE_PREFIX: &str = "gmail-draft:";

fn draft_remote_id(draft_id: &str) -> RemoteId {
    RemoteId::new(format!("{DRAFT_REMOTE_PREFIX}{draft_id}"))
}

fn parse_draft_remote_id(remote_id: &RemoteId) -> Option<&str> {
    remote_id.as_str().strip_prefix(DRAFT_REMOTE_PREFIX)
}
```

- [ ] Extend `GmailNativeBundle` in `crates/locality-gmail/src/render.rs`:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GmailNativeBundle {
    pub mailbox: String,
    pub draft_id: Option<String>,
    pub message: GmailMessage,
}
```

- [ ] Update existing `GmailNativeBundle` construction sites to pass `draft_id: None` for inbox and sent messages.

- [ ] Render `gmail.draft_id` for drafts only:

```rust
if let Some(draft_id) = &bundle.draft_id {
    gmail.insert(
        Value::String("draft_id".to_string()),
        Value::String(draft_id.clone()),
    );
}
```

- [ ] Add `list_draft_entries` in `crates/locality-gmail/src/connector.rs` that uses `GmailApi::list_drafts` instead of `list_messages("DRAFT")`:

```rust
fn list_draft_entries(
    api: &dyn GmailApi,
    settings: &GmailMountSettings,
    mount_id: &MountId,
    parent_path: &Path,
) -> LocalityResult<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    let mut page_token: Option<String> = None;
    loop {
        let list = api.list_drafts(
            GMAIL_PAGE_SIZE,
            page_token.as_deref(),
            gmail_recent_query(settings),
        )?;
        for draft in list.drafts {
            entries.push(draft_entry(mount_id, parent_path, draft.id, draft.message)?);
        }
        match list.next_page_token {
            Some(next) => page_token = Some(next),
            None => break,
        }
    }
    Ok(entries)
}
```

- [ ] Add `draft_entry` beside `message_entry`:

```rust
fn draft_entry(
    mount_id: &MountId,
    parent_path: &Path,
    draft_id: String,
    message: GmailMessage,
) -> LocalityResult<TreeEntry> {
    let version = remote_version(&message);
    let name = message_filename(&message, "draft");
    Ok(TreeEntry::page(
        mount_id.clone(),
        draft_remote_id(&draft_id),
        parent_path.join(name),
        version,
    ))
}
```

- [ ] Replace only the `DRAFT_FOLDER_ID` enumeration paths to call `list_draft_entries`.
  - `INBOX` and `SENT` continue using `list_label_entries`.
  - Thread view still lists inbox and sent as threads, but `draft/` uses draft resources.

- [ ] Update `observe()`:
  - If `parse_draft_remote_id(remote_id)` returns a draft ID, call `get_draft_full(draft_id)`.
  - Build the path from `draft_entry`.
  - Return a `RemoteObservation` for the stable `gmail-draft:<draft_id>` remote ID.

- [ ] Update `fetch()`:
  - If `parse_draft_remote_id(remote_id)` returns a draft ID, call `get_draft_full(draft_id)`.
  - Return a native entity with `GmailNativeBundle { mailbox: "draft".to_string(), draft_id: Some(draft.id), message: draft.message }`.

- [ ] Keep backward compatibility for old state whose entity remote ID is a Gmail message ID with label `DRAFT`.
  - In `observe()` and `fetch()`, if the remote ID is not prefixed with `gmail-draft:`, keep the existing message-based path.
  - Old projected draft entries may be refreshed into the new draft ID on the next full enumerate or pull.

- [ ] Update tests in `crates/locality-gmail/src/connector.rs`:
  - `enumerate_projects_four_folders_and_recent_inbox_sent_draft_messages` should assert draft entries have remote IDs like `gmail-draft:draft-1`.
  - `list_children_for_draft_folder_returns_remote_drafts` should assert `list_drafts` was called and message listing for `DRAFT` was not called.
  - Add `fetch_remote_draft_uses_draft_resource_and_renders_draft_id`.
  - Add `observe_remote_draft_uses_draft_resource`.
  - Add `fetch_legacy_draft_message_remote_id_still_works`.

- [ ] Run:

```bash
cargo test -p locality-gmail connector::tests::enumerate_projects_four_folders_and_recent_inbox_sent_draft_messages
cargo test -p locality-gmail connector::tests::list_children_for_draft_folder_returns_remote_drafts
cargo test -p locality-gmail connector::tests::fetch_remote_draft_uses_draft_resource_and_renders_draft_id
cargo test -p locality-gmail connector::tests::observe_remote_draft_uses_draft_resource
```

Expected result for each command:

```text
test result: ok
```

### Task 3: Allow And Validate Draft Updates And Draft-To-Outbox Moves

- [ ] Update `GmailConnector::capabilities()` in `crates/locality-gmail/src/connector.rs`:

```rust
supports_entity_body_updates: true,
```

- [ ] Update `GmailConnector::supported_push_operations()`:

```rust
[
    PushOperationKind::CreateEntity,
    PushOperationKind::UpdateProperties,
    PushOperationKind::UpdateEntityBody,
    PushOperationKind::MoveEntity,
]
.into_iter()
.collect()
```

- [ ] Update `source_move_decision_for_parent_path()` in `crates/localityd/src/source.rs` so Gmail only allows moves into the direct `outbox/` folder:

```rust
"gmail" => {
    if relative_path.components().count() == 1 && relative_path == Path::new("outbox") {
        SourceWriteDecision::writable()
    } else {
        SourceWriteDecision::read_only(
            "Gmail only supports moving an existing draft directly into outbox/ to send it",
        )
    }
}
```

- [ ] Keep direct writes under `draft/` and `outbox/` allowed. Do not allow nested outbound files.

- [ ] Add a shared outbound validator in `crates/localityd/src/gmail.rs`:

```rust
fn validate_gmail_outbound_document(
    document: &CanonicalDocument,
    issues: &mut Vec<ValidationIssue>,
) {
    let gmail = document
        .frontmatter
        .get("gmail")
        .and_then(|value| value.as_mapping());
    let attachments = gmail
        .and_then(|gmail| gmail.get(Value::String("attachments".to_string())))
        .and_then(|value| value.as_sequence())
        .map(|items| !items.is_empty())
        .unwrap_or(false);
    if attachments {
        issues.push(ValidationIssue::error(
            "gmail_attachments_not_supported",
            "Gmail outbound messages with attachments are not supported yet",
        ));
    }
    if document
        .frontmatter
        .get("subject")
        .and_then(|value| value.as_str())
        .map(|subject| subject.trim().is_empty())
        .unwrap_or(true)
    {
        issues.push(ValidationIssue::error(
            "gmail_subject_required",
            "Gmail outbound messages require a subject",
        ));
    }
    let has_recipient = document
        .frontmatter
        .get("to")
        .and_then(|value| value.as_sequence())
        .map(|items| {
            items.iter().any(|item| {
                item.as_str()
                    .map(|recipient| !recipient.trim().is_empty())
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    if !has_recipient {
        issues.push(ValidationIssue::error(
            "gmail_recipient_required",
            "Gmail outbound messages require at least one recipient in `to`",
        ));
    }
}
```

- [ ] Call `validate_gmail_outbound_document` from both create and changed validation paths for direct children of `draft/` and `outbox/`.

- [ ] Keep `inbox/` and `sent/` changed items blocked.

- [ ] Add tests in `crates/localityd/tests/source_descriptor.rs`:
  - `gmail_move_policy_allows_draft_to_direct_outbox_parent`
  - `gmail_move_policy_rejects_nested_outbox_parent`
  - `local_gmail_validator_allows_valid_changed_draft`
  - `local_gmail_validator_rejects_changed_draft_without_to`
  - `local_gmail_validator_rejects_changed_draft_without_subject`
  - `local_gmail_validator_rejects_changed_draft_with_attachments`
  - `local_gmail_validator_allows_valid_changed_outbox`
  - `local_gmail_validator_blocks_changed_inbox_and_sent_items` remains green.

- [ ] Run:

```bash
cargo test -p localityd --test source_descriptor gmail_move_policy
cargo test -p localityd --test source_descriptor local_gmail_validator
```

Expected result:

```text
test result: ok
```

### Task 4: Prepare Push Plans For Draft Update And Draft Send

- [ ] Add a push preparation test for editing an existing remote draft in `crates/localityd/tests/push_preparation.rs`.
  - Mount a Gmail draft entity with remote ID `gmail-draft:draft-1`.
  - Hydrate it under `draft/Original.md`.
  - Edit subject, `to`, and body.
  - Prepare push for that file.
  - Assert the plan contains `UpdateProperties` and `UpdateEntityBody`.
  - Assert the plan does not contain `CreateEntity`.
  - Assert guardrails are not dangerous.

- [ ] Add a push preparation test for moving a draft into outbox with edits.
  - Mount a Gmail draft entity with remote ID `gmail-draft:draft-1`.
  - Hydrate it under `draft/Original.md`.
  - Move it to `outbox/Send Now.md` through virtual mutation setup.
  - Edit body content in the moved file.
  - Prepare push for `outbox/Send Now.md`.
  - Assert operation order:

```text
MoveEntity
UpdateProperties
UpdateEntityBody
```

- [ ] Assert the `MoveEntity` has:
  - `entity_id == RemoteId::new("gmail-draft:draft-1")`
  - `new_parent_id == RemoteId::new("gmail-folder:outbox")`
  - `projected_path == "outbox/Send Now.md"`

- [ ] If the preparation test produces only `MoveEntity` without content updates, inspect `lower_move_document_operations()` in `crates/localityd/src/push.rs` before changing it. The expected behavior is already used by generic move tests and should be preserved.

- [ ] Run:

```bash
cargo test -p localityd --test push_preparation gmail_draft
```

Expected result:

```text
test result: ok
```

### Task 5: Apply Draft Update And Draft-To-Outbox Send In The Gmail Connector

- [ ] Add connector tests in `crates/locality-gmail/src/connector.rs` before implementation:
  - `apply_updates_remote_gmail_draft`
  - `apply_sends_remote_gmail_draft_moved_to_outbox`
  - `apply_rejects_move_of_non_draft_gmail_entity`
  - `apply_rejects_gmail_draft_move_to_non_outbox_parent`
  - `apply_rejects_draft_update_with_attachments`

- [ ] Extend the fake Gmail API in connector tests:
  - Store draft list responses.
  - Store full draft responses by draft ID.
  - Record `updated_drafts: Vec<(String, String)>`.
  - Record `sent_drafts: Vec<String>`.
  - Return updated drafts with a changed contained message ID to prove identity remains `gmail-draft:<draft_id>`.

- [ ] Add a connector-side mutation accumulator:

```rust
#[derive(Clone, Debug, Default)]
struct DraftApplyMutation {
    draft_remote_id: RemoteId,
    draft_id: String,
    projected_path: Option<PathBuf>,
    move_to_outbox: bool,
    title: Option<String>,
    properties: BTreeMap<String, PropertyValue>,
    body: Option<String>,
    operation_index: usize,
    operation_id: Option<PushOperationId>,
}
```

- [ ] In `apply()`, scan `request.plan.operations` once:
  - Keep existing `CreateEntity` behavior for new files under `draft/` and `outbox/`.
  - Collect `UpdateProperties` and `UpdateEntityBody` for remote IDs where `parse_draft_remote_id(entity_id)` succeeds.
  - Collect `MoveEntity` only when the entity ID is a draft remote ID and the new parent ID is `OUTBOX_FOLDER_ID`.
  - Reject `MoveEntity` for non-draft Gmail entities.
  - Reject draft moves to any parent other than `OUTBOX_FOLDER_ID`.

- [ ] For each collected draft mutation:
  - Load the current draft through `api.get_draft_full(draft_id)`.
  - Render the current draft into a `GmailDraftDocument`.
  - Apply `UpdateProperties` values over the rendered document.
  - Apply `UpdateEntityBody` over the rendered document body.
  - If `MoveEntity.new_title` exists and no explicit subject property exists, use the move title as the subject fallback.
  - Parse and validate the resulting outbound document with the same rules used for create.
  - Build MIME with no new Locality-generated `Message-ID` for remote draft updates.

- [ ] Add helper:

```rust
fn update_gmail_draft_from_document(
    api: &dyn GmailApi,
    draft_id: &str,
    draft: &GmailDraftDocument,
) -> LocalityResult<GmailDraft> {
    let raw = raw_message_base64url(&build_draft_mime_with_message_id(draft, None)?);
    api.update_draft(
        draft_id,
        crate::dto::GmailDraftUpdateRequest {
            message: GmailRawMessage { raw },
        },
    )
}
```

- [ ] For normal draft updates:
  - Call `update_gmail_draft_from_document`.
  - Add `draft_remote_id(draft_id)` to `changed_remote_ids`.
  - Do not create, archive, or move any entity.

- [ ] For draft-to-outbox sends:
  - Call `update_gmail_draft_from_document`.
  - Call `api.send_draft(GmailDraftSendRequest { id: draft_id.to_string() })`.
  - Add the sent message ID to `changed_remote_ids`.
  - Return both effects:

```rust
JournalApplyEffect::ArchivedEntity {
    operation_id,
    operation_index,
    entity_id: draft_remote_id(draft_id),
}
JournalApplyEffect::CreatedEntity {
    operation_id,
    operation_index,
    parent_id: RemoteId::new(SENT_FOLDER_ID),
    entity_id: RemoteId::new(sent.id),
}
```

- [ ] Guard idempotency for draft sends.
  - Extend `block_ambiguous_gmail_send_replay` in `crates/localityd/src/push.rs` to also block pending Gmail `MoveEntity` operations whose destination parent is `gmail-folder:outbox`.
  - In connector apply, if `send_draft` fails after `update_draft` succeeds, return an error that leaves the journal unresolved and forces review instead of retrying silently.

- [ ] Run:

```bash
cargo test -p locality-gmail connector::tests::apply_updates_remote_gmail_draft
cargo test -p locality-gmail connector::tests::apply_sends_remote_gmail_draft_moved_to_outbox
cargo test -p locality-gmail connector::tests::apply_rejects_move_of_non_draft_gmail_entity
cargo test -p locality-gmail connector::tests::apply_rejects_gmail_draft_move_to_non_outbox_parent
```

Expected result for each command:

```text
test result: ok
```

### Task 6: Reconcile Draft Update And Draft Send Correctly In The Daemon

- [ ] Add a daemon push execution test in `crates/localityd/tests/push_execution.rs` for draft update:
  - Create a mounted Gmail draft entity under `draft/Remote Draft.md`.
  - Edit recipient, subject, and body.
  - Execute push.
  - Assert fake Gmail API recorded `update_draft("draft-1", raw)`.
  - Assert no send happened.
  - Assert the projected draft remains under `draft/`.
  - Assert the entity remote ID remains `gmail-draft:draft-1`.
  - Assert shadow body and frontmatter match the updated local file.

- [ ] Add a daemon push execution test for moving a draft to outbox:
  - Create a mounted Gmail draft entity under `draft/Remote Draft.md`.
  - Move it to `outbox/Send Remote Draft.md` through the visible projection or virtual mutation helper used by adjacent tests.
  - Edit the moved file body before push.
  - Execute push.
  - Assert fake Gmail API recorded one `update_draft("draft-1", raw)` before one `send_draft("draft-1")`.
  - Assert the final projected file is under `sent/`.
  - Assert no file remains under `draft/Remote Draft.md`.
  - Assert no file remains under `outbox/Send Remote Draft.md`.
  - Assert the final sent entity remote ID is the sent message ID returned by fake Gmail.

- [ ] Add a Gmail-specific reconcile predicate in `crates/localityd/src/push.rs`:

```rust
fn is_gmail_draft_send_move(operation: &PushOperation) -> bool {
    match operation {
        PushOperation::MoveEntity {
            entity_id,
            new_parent_id,
            ..
        } => {
            entity_id.as_str().starts_with("gmail-draft:")
                && new_parent_id.as_str() == "gmail-folder:outbox"
        }
        _ => false,
    }
}
```

- [ ] In the generic `MoveEntity` reconcile branch:
  - If `is_gmail_draft_send_move(operation)` is true and the apply effects contain a `CreatedEntity` for the same operation index with parent `gmail-folder:sent`, skip the same-entity move invariant.
  - Require an `ArchivedEntity` effect for the moved draft remote ID.
  - Remove or mark archived the old draft entity through the same local cleanup path used by other archive effects.

- [ ] Extend the `CreatedEntity` reconcile branch:
  - Accept a matching original operation of either `CreateEntity` or Gmail draft-send `MoveEntity`.
  - For Gmail draft-send `MoveEntity`, use the `CreatedEntity` effect to fetch and render the sent message.
  - Save the sent entity under `sent/` using `created_entity_reconcile_path_from_rendered`.
  - Clear the virtual move mutation after the sent entity is saved.

- [ ] Keep existing non-Gmail move reconciliation unchanged.

- [ ] Run:

```bash
cargo test -p localityd --test push_execution daemon_push_reconciles_gmail_draft_update
cargo test -p localityd --test push_execution daemon_push_reconciles_gmail_draft_move_to_outbox_send
cargo test -p localityd --test push_execution daemon_push_reconciles_gmail_draft_create_to_draft_folder
cargo test -p localityd --test push_execution daemon_push_reconciles_gmail_send_create_to_sent_folder
```

Expected result for each command:

```text
test result: ok
```

### Task 7: Update User And Agent Guidance

- [ ] Update `docs/gmail-connector.md`:
  - Explain that `draft/` contains remote Gmail drafts and local draft creates.
  - Explain that editing a remote draft and pushing updates the Gmail draft.
  - Explain that moving a remote draft from `draft/` to `outbox/` and pushing sends the updated draft.
  - Keep `outbox/` described as local-only send staging.
  - State that outbound attachments remain unsupported.

- [ ] Update `docs/agent-guidance.md`:
  - Tell agents to leave messages in `draft/` when the user asks to draft or revise.
  - Tell agents to use `outbox/` only when the user explicitly asks to send now.
  - Tell agents that moving an existing draft into `outbox/` sends that draft after applying local edits.
  - Tell agents to inspect with `loc status` and `loc diff` before pushing if Live Mode is paused, conflicted, or review-needed.

- [ ] Update `docs/cli.md` and `docs/daemon.md` where Gmail write flows are described.

- [ ] Update `apps/desktop/src-tauri/src/agent_guidance.rs` with the same draft versus outbox distinction.

- [ ] Add exact-output assertions in any tests that snapshot generated guidance. If no snapshot test exists, add or update the closest source descriptor guidance test in `crates/localityd/tests/source_descriptor.rs`.

- [ ] Run:

```bash
cargo test -p localityd --test source_descriptor gmail
```

Expected result:

```text
test result: ok
```

### Task 8: Extend Live Gmail Roundtrip Coverage

- [ ] Update `tests/live_gmail_vfs_roundtrip.sh` with a gated remote draft edit/send scenario.
  - Use the existing OAuth and Gmail helpers already in the script.
  - Create a draft through the Gmail drafts API or through Locality `draft/`.
  - Pull or wait until the draft appears under local `draft/`.
  - Edit subject and body in the local Markdown file.
  - Push the draft file.
  - Verify through Gmail drafts API that the draft content changed.
  - Move the local draft file to `outbox/`.
  - Push the moved outbox file.
  - Verify through Gmail API that a sent message exists with the updated subject and body.
  - Clean up scratch draft or sent artifacts when the Gmail API allows safe cleanup.

- [ ] Keep the live test opt-in through its existing environment requirements. Do not make live Gmail API tests part of default CI unless the repo already does that.

- [ ] Run the live test only when credentials are available:

```bash
tests/live_gmail_vfs_roundtrip.sh
```

Expected result:

```text
PASS live Gmail VFS roundtrip
```

### Task 9: Full Verification

- [ ] Format the workspace:

```bash
cargo fmt --all --check
```

Expected result:

```text
command exits 0 with no output
```

- [ ] Run focused Gmail connector tests:

```bash
cargo test -p locality-gmail
```

Expected result:

```text
test result: ok
```

- [ ] Run focused daemon tests:

```bash
cargo test -p localityd --test source_descriptor
cargo test -p localityd --test push_preparation
cargo test -p localityd --test push_execution
```

Expected result for each command:

```text
test result: ok
```

- [ ] Run the full workspace test set if time permits:

```bash
cargo test --workspace
```

Expected result:

```text
test result: ok
```

- [ ] Inspect changed files:

```bash
git diff --stat
git diff -- crates/locality-gmail/src crates/localityd/src docs apps tests
```

Expected result:

```text
Only Gmail draft edit/send code, tests, live test coverage, and documentation changed.
```

## Design Checks

- [ ] Remote draft identity is the Gmail draft ID, not the contained message ID.
- [ ] Rendered draft frontmatter includes `gmail.draft_id` and current `gmail.message_id`.
- [ ] Updating a draft preserves Locality entity identity even if Gmail returns a new contained message ID.
- [ ] Moving a draft to `outbox/` sends the existing draft and reconciles the resulting sent message under `sent/`.
- [ ] `outbox/` remains empty on remote enumeration.
- [ ] Inbox and sent remain read-only.
- [ ] Attachments remain rejected for outbound Gmail push.
- [ ] Generic non-Gmail move reconciliation remains unchanged.
- [ ] Agent guidance clearly distinguishes `draft/` from `outbox/`.
