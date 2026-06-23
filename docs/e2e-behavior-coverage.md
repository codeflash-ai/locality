# E2E Behavior Coverage

Generated from the current design and test tree.

This report tracks user-visible, connector-backed behavior. It intentionally
separates:

- **Live Notion e2e**: runs against the real Notion API and creates scratch
  Notion content.
- **Local behavior e2e**: exercises the AFS architecture with fake connectors,
  local state, or seeded filesystem mounts.
- **Manual macOS coverage**: requires a signed app, File Provider approval, and
  a user session. GitHub-hosted macOS runners compile this code but do not
  register a real File Provider domain.

## How To Run Live Notion E2E

Required environment:

```sh
export NOTION_TOKEN=...
export AFS_NOTION_LIVE_PARENT_PAGE=...
```

Optional:

```sh
export AFS_NOTION_LIVE_DIR=/tmp/afs-notion-live
export AFS_WINDOWS_CLOUD_FILES_LIVE=1
```

Run:

```sh
cargo test -p afs-notion --test live_integrity -- --ignored --test-threads=1
cargo test -p afs-cli --test e2e_push_workflow live_ -- --ignored --test-threads=1
pwsh ./tests/windows_cloud_files_live.ps1
```

GitHub Actions runs these from `.github/workflows/notion-live-e2e.yml` on
`main` when `NOTION_TOKEN` and `AFS_NOTION_LIVE_PARENT_PAGE` are configured in
the `notion-live-e2e` environment. The Windows job runs on `windows-latest` and
registers a real Cloud Files sync root against disposable scratch Notion pages.
During that live Windows run, `afs doctor --json` must pass against the same
state directory after the daemon and Cloud Files provider are running.

## Expected Behavior Coverage

Coverage labels:

- **Covered live**: exercised against real Notion and verified after API reads.
- **Partial live**: real Notion is involved, but not through the full product
  surface or not for the whole behavior.
- **Local only**: covered with fake connectors, seeded state, FUSE smoke, or
  unit/integration tests, but not against real Notion.
- **Manual only**: expected product behavior exists but is only manually tested.
- **Gap**: important behavior not yet covered by e2e tests.

| ID | Expected e2e behavior | Current live Notion coverage | Local/CI coverage | Current gap |
|---|---|---|---|---|
| E2E-001 | OAuth broker connection stores a refresh handle/metadata without shipping or persisting a Notion client secret locally. | Gap | `crates/afs-cli/tests/connect.rs` covers broker secret separation with a fake broker. | Add a broker-backed live auth smoke, likely outside normal CI unless using a test OAuth integration. |
| E2E-002 | New install/onboarding can reset stale local beta state, install the terminal-visible `afs` command, then proceed through connection and mount setup. | Gap | Desktop command/unit coverage plus manual DMG testing. | Needs a desktop automation harness or scripted signed-app smoke on macOS. |
| E2E-003 | Workspace mount is created under the macOS CloudStorage AFS root, with source folders below `AFS/notion` and no Documents symlink dependency. | Gap | Mount path validation tests in `apps/desktop/src-tauri/src/main.rs`. | Real File Provider mount behavior is manual-only on macOS. |
| E2E-004 | A mounted workspace can expose top-level source directories and page/database entries without materializing every page body. | Covered live for virtual filesystem core | `architecture_behavior::local_virtual_mount_supports_browse_open_edit_review_push_round_trip`; `tests/linux_fuse_smoke.sh`. | Live Notion now exercises the lazy virtual filesystem path directly; kernel FUSE/File Provider registration remains local/manual. |
| E2E-005 | Opening/listing a nested page directory lazily discovers immediate children only. | Covered live for virtual filesystem core | `architecture_behavior::local_virtual_mount_supports_browse_open_edit_review_push_round_trip`. | Kernel FUSE/File Provider shell/Finder path still needs platform e2e. |
| E2E-006 | Opening `page.md` hydrates full page content into canonical Markdown. | Covered live | Local pull and architecture behavior tests. | Covered through plain-file pull; real File Provider open hydration remains manual/macOS. |
| E2E-007 | Pasting a Notion URL locates/prioritizes the matching local `page.md` path and reveal selects the Markdown file. | Covered live for locate and hydration priority | Search/locate helpers and desktop command code are covered locally in pieces. | Desktop Finder reveal/select assertion still needs UI automation. |
| E2E-008 | Page Markdown read supports rich Notion blocks and keeps unsupported/read-only blocks visible without lossy edits. | Covered live | Broad renderer/unit coverage. | Covered for representative block types, not every future Notion block variant. |
| E2E-009 | A read/no-op push of a rich page does not mutate Notion JSON. | Covered live | Local no-op push tests. | Covered by live cyclic diverse page test. |
| E2E-010 | File-like media references render in Markdown and downloadable media can be materialized into a local media directory for agent inspection. | Covered live | Media render/unit coverage. | Live tests cover media download and local file-like upload/reconcile; broader media pruning/cache lifecycle is not covered. |
| E2E-011 | Editing a local page marks it pending in status before push. | Covered live | Local status and architecture behavior tests. | Live coverage uses CLI status; desktop/tray pending display remains manual. |
| E2E-012 | Diff/review planning reports intended changes before applying remote writes. | Covered live | `mount_pull_mid_page_insert_push_and_status_clean`; push/diff tests. | Desktop review UI is not e2e automated. |
| E2E-013 | Pushing a simple page edit writes to Notion, fetches back, and status returns clean. | Covered live | Local push workflow tests. | Covered. |
| E2E-014 | Supported rich block edits push and verify: paragraph, headings 1-4, bullet, number, todo, quote, callout, code, divider, equation, table, bookmark, embed, media URL/caption, block-type replacement, safe directive block moves, read-only `link_to_page` line moves, unsafe `link_to_page` retarget blocking, unsafe `link_preview` edit/move/delete blocking, unsafe table shape blocking, and unsafe child-page link edit/move/delete blocking. | Covered live | Notion apply/render tests; `crates/afs-cli/tests/e2e_push_workflow.rs::live_block_type_replace_pushes_and_reconciles_notion`; `crates/afs-cli/tests/e2e_push_workflow.rs::live_directive_block_move_pushes_and_reconciles_notion`; `crates/afs-cli/tests/e2e_push_workflow.rs::live_link_to_page_line_move_preserves_notion_block_type`; `crates/afs-cli/tests/e2e_push_workflow.rs::live_link_to_page_retarget_blocks_before_journaled_apply`; `crates/afs-cli/tests/e2e_push_workflow.rs::live_table_width_change_blocks_before_journaled_apply`; `crates/afs-cli/tests/e2e_push_workflow.rs::live_child_page_link_move_blocks_before_journaled_apply`; `crates/afs-cli/tests/e2e_push_workflow.rs::live_child_page_link_delete_blocks_before_journaled_apply`. | Covered for representative edits, replacement, a childless directive move, a rendered `link_to_page` move, `link_to_page` retarget guardrails, local `link_preview` edit/move/delete guardrails, table width/header-mode pre-apply guardrails, and child-page link edit/move/delete guardrails; not every color/layout option. |
| E2E-015 | Typed rich text round-trips for links, annotations, inline equations, page/database/date/user mentions. | Covered live | Notion renderer/apply tests. | Live write requires explicit IDs for page/database/user mentions. |
| E2E-016 | Database directories expose `_schema.yaml` and row Markdown files. | Covered live | Pull and schema tests. | Covered through plain-file live mounted workflow, not live File Provider. |
| E2E-017 | Database row property validation rejects invalid options before writing. | Covered live | Schema validation tests. | Live coverage is connector-level for invalid option validation, not the desktop UI path. |
| E2E-018 | Existing database rows can edit body and supported properties: title, rich text, number, select, status, multi-select, checkbox, date, URL, email, phone, external files, people IDs, relation IDs. | Covered live | Notion apply tests. | Covered for the representative property set. |
| E2E-019 | Creating a new Markdown file under a database directory creates a Notion row and verifies the rendered row. | Covered live | Local virtual create tests. | Covered through live mounted workflow. |
| E2E-020 | Local creates/renames/deletes in virtual mounts surface as pending virtual mutations. | Covered live for virtual create/rename/delete core and Windows Cloud Files | `crates/afs-cli/tests/projection_contract.rs`; `tests/linux_fuse_smoke.sh`; virtual filesystem tests; `crates/afs-cli/tests/e2e_push_workflow.rs::live_page_directory_create_pushes_child_page_and_refreshes_parent`; `crates/afs-cli/tests/e2e_push_workflow.rs::live_virtual_page_directory_rename_updates_remote_title_and_reconciles`; `crates/afs-cli/tests/e2e_push_workflow.rs::live_virtual_page_directory_delete_archives_remote_child_page`. | Linux FUSE and macOS File Provider live create/rename/delete still need provider-backed Notion coverage. |
| E2E-021 | Push preflight detects remote drift before writing and blocks or requires review instead of overwriting. | Covered live | `crates/afs-cli/tests/push.rs`, `crates/afs-core/tests/push_executor.rs`, Notion concurrency unit tests. | Covered with scratch page local+remote drift; desktop review UI still separate. |
| E2E-022 | Pull of a dirty local file never overwrites local pending edits and does not insert conflict markers unless remote content diverged. | Covered live | `crates/afs-cli/tests/pull.rs`; `crates/afs-cli/tests/e2e_push_workflow.rs::live_dirty_pull_conflict_can_be_resolved_and_pushed`; `crates/afs-cli/tests/live_workspace_mount.rs` manual/destructive. | Live scratch coverage now verifies conflict marker creation, manual resolution, push, and cleanup. |
| E2E-023 | Remote observation uses cheap metadata and does not hydrate full block bodies. | Covered live | `crates/afs-notion/tests/observe.rs::live_notion_observe_page_reads_metadata_without_hydrating_blocks`; freshness tests. | Live coverage uses a counting API wrapper against real Notion and asserts zero block-children calls. |
| E2E-024 | Freshness scheduling keeps active/pending paths hot, uses bounded work, and avoids full-workspace scans. | Partial live | `crates/afsd/tests/runtime.rs`, `scheduler.rs`, `scheduled_pull.rs`, `hydration_queue.rs`; `crates/afs-cli/tests/e2e_push_workflow.rs::live_scheduled_pull_queues_and_applies_remote_fast_forward`. | Live scratch coverage verifies scheduled enumeration queues and applies remote fast-forward; long-running bounded API-call assertions remain local-only. |
| E2E-025 | Clean inactive files can auto-fast-forward when remote changes, while active or locally pending files are protected. | Covered live for hydration executor behavior | `crates/afsd/tests/runtime.rs`; `crates/afsd/tests/hydration_executor.rs`; `crates/afs-core/tests/explain.rs`. | Daemon scheduler trigger remains local-only; live test covers the remote fast-forward apply/skip behavior. |
| E2E-026 | `afs inspect` explains remote-only vs local+remote divergence without mutating local or remote state. | Covered live | `crates/afs-cli/tests/inspect.rs`; `crates/afs-cli/tests/e2e_push_workflow.rs::live_inspect_explains_remote_and_local_drift_without_mutating`; `crates/afs-core/tests/explain.rs`. | Covered through live scratch page inspection; desktop review UI remains separate. |
| E2E-027 | Push success journals the operation and enables future history/undo surfaces where possible. | Covered live | `crates/afs-cli/tests/history.rs`; `crates/afs-cli/tests/e2e_push_workflow.rs::live_push_log_and_undo_restores_remote_content`; `crates/afs-cli/tests/e2e_push_workflow.rs::live_directive_block_move_undo_restores_remote_order`. | Covered for a reversible block update and a Notion copy+archive directive move undo; broader undo variants remain local-only. |
| E2E-028 | Desktop push runs asynchronously, does not freeze the UI, and briefly confirms success. | Manual only | Desktop command wiring and prior manual test. | Needs UI automation around a test backend or live Notion page. |
| E2E-029 | Tray icon appears, reflects ready/pending/error state, and idle desktop CPU remains near zero. | Manual only | Tray icon unit tests and manual CPU profile. | Needs desktop runtime smoke/perf test. |
| E2E-030 | Packaged DMG includes signed `afs`, `afsd`, File Provider extension, CLI helper, tray app, and passes notarization. | Manual/publish covered | `make publish` signing/notarization validation. | Not currently a CI e2e because it requires Apple credentials. |

## Live Notion Test Coverage Map

| Test | Kind | Behaviors covered |
|---|---|---|
| `crates/afs-notion/tests/live_integrity.rs::live_page_read_edit_write_verify_integrity_with_media_download` | Live connector | Fetch/render rich page content, download image/video/file/PDF/audio assets into local media files, update supported blocks, append block, fetch/verify rendered Notion content. Covers E2E-006, E2E-008, E2E-010, E2E-014. |
| `crates/afs-notion/tests/live_integrity.rs::live_database_row_property_create_edit_verify_integrity` | Live connector | Create live database, get schema, validate row frontmatter, create row, update supported properties, fetch/verify row. Covers E2E-017, E2E-018. |
| `crates/afs-notion/tests/observe.rs::live_notion_observe_page_reads_metadata_without_hydrating_blocks` | Live connector | Create scratch page with a child block, observe page metadata, update title metadata, observe changed metadata/title, and assert the live connector made zero block-children calls. Covers E2E-023. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_scratch_page_mount_edit_push_verifies_notion` | Live mounted workflow, plain files | Create scratch page, mount, pull, edit Markdown, diff, dirty status, push, clean status, fetch/verify Notion. Covers E2E-006, E2E-011, E2E-012, E2E-013. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_block_type_replace_pushes_and_reconciles_notion` | Live mounted workflow, plain files | Replace a paragraph with a bullet in Markdown, verify the `replace_block` plan, push to Notion, assert create+archive apply effects, verify the remote block type and local clean reconciliation. Covers the replacement path for E2E-014. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_directive_block_move_pushes_and_reconciles_notion` | Live mounted workflow, plain files | Move an unchanged `table_of_contents` directive line, verify the `move_block` plan, push through Notion's copy+archive apply path, and assert the reconciled local file is clean with the refreshed directive before the paragraph. Covers the directive move path for E2E-014. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_link_to_page_line_move_preserves_notion_block_type` | Live mounted workflow, plain files | Move a rendered `link_to_page` Markdown line, verify the planner emits append+archive, push through the preservation path, and assert Notion still has a `link_to_page` block targeting the original page. Covers the read-only link move path for E2E-014. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_link_to_page_retarget_blocks_before_journaled_apply` | Live mounted workflow, plain files | Retarget a rendered `link_to_page` Markdown line, assert diff/push report structured validation instead of creating a journal, and verify the live Notion source blocks are unchanged. Covers the unsafe `link_to_page` retarget guardrail for E2E-014. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_table_width_change_blocks_before_journaled_apply` | Live mounted workflow, plain files | Change a rendered table's column count, assert diff/push report structured validation instead of creating a journal, and verify the live Notion source blocks are unchanged. Covers the unsafe table shape guardrail for E2E-014. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_child_page_link_move_blocks_before_journaled_apply` | Live mounted workflow, plain files | Move a rendered child-page Markdown link, assert diff/push report structured validation instead of creating a journal, and verify the live Notion parent blocks are unchanged. Covers the unsafe child-page link move guardrail for E2E-014. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_child_page_link_delete_blocks_before_journaled_apply` | Live mounted workflow, plain files | Delete a rendered child-page Markdown link, assert diff/push report structured validation instead of creating a journal, verify the live Notion parent blocks are unchanged, and verify the child page remains unarchived. Covers the unsafe child-page link delete guardrail for E2E-014. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_lazy_virtual_mount_enumerates_children_and_hydrates_on_open` | Live virtual filesystem core | Create scratch page tree, enumerate virtual source root and page directory without content materialization, then hydrate on open and verify Markdown. Covers E2E-004, E2E-005, E2E-006. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_drift_preflight_blocks_push_before_overwriting_remote` | Live mounted workflow, plain files | Pull scratch page, edit locally, mutate the same page remotely, assert push blocks before overwrite, fetch/verify remote edit remains. Covers E2E-021. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_dirty_pull_conflict_can_be_resolved_and_pushed` | Live mounted workflow, plain files | Pull scratch page, edit locally, mutate the same block remotely, pull conflict markers, resolve the Markdown, push, fetch/verify resolved Notion content. Covers E2E-022 and conflict recovery. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_virtual_page_directory_delete_archives_remote_child_page` | Live virtual filesystem core | Hydrate a child page, record a virtual page-directory delete, verify the pending archive status and `archive_entity` plan, push to Notion, and verify the child page is archived and the local mutation is reconciled. Covers the virtual delete/archive path for E2E-020. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_virtual_page_directory_rename_updates_remote_title_and_reconciles` | Live virtual filesystem core | Hydrate a child page, rename its virtual page directory, verify a pending `Rename` mutation, push the title change to Notion, and verify the remote title plus local clean reconciliation. Covers the virtual rename path for E2E-020. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_remote_fast_forward_updates_clean_file_and_preserves_pending_file` | Live virtual filesystem core | Hydrate a clean virtual file, mutate remote, fast-forward local content; then add local pending edit and assert remote fast-forward skips without overwrite. Covers E2E-025. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_scheduled_pull_queues_and_applies_remote_fast_forward` | Live mounted workflow, scheduled pull | Run scheduled pull enumeration against a scratch parent/child tree, hydrate the child, mutate it remotely, run scheduled pull again, verify a `RemoteFastForward` hydration request is queued, drain it, and verify local Markdown updates. Covers live scheduler trigger behavior for E2E-024 and E2E-025. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_inspect_explains_remote_and_local_drift_without_mutating` | Live mounted workflow, plain files | Pull scratch page, inspect all-synced state, mutate remote and inspect safe fast-forward, add local drift and inspect review-needed state, while verifying local file content is unchanged by inspect. Covers E2E-026. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_push_log_and_undo_restores_remote_content` | Live mounted workflow, plain files | Push a scratch page edit, verify journal/log preimage and apply-effect metadata, run connector-backed undo, verify Notion remote content is restored and journal status becomes reverted. Covers E2E-027. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_directive_block_move_undo_restores_remote_order` | Live mounted workflow, plain files | Move a `table_of_contents` directive through Notion's copy+archive move path, undo the reconciled journal, and verify the remote Markdown order is restored with a single directive block. Covers copy+archive move undo for E2E-027. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_page_directory_create_pushes_child_page_and_refreshes_parent` | Live mounted workflow, plain files | Create a new local child page directory with `page.md`, push it to Notion, verify the child page body, and verify the parent local/remote Markdown refreshes with the child-page link. Covers the create side of E2E-020. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_locate_notion_url_returns_markdown_path_and_can_prioritize_hydration` | Live virtual filesystem core | Index scratch page metadata, locate by Notion URL, assert Markdown file path and online-only state, then hydrate located page. Covers E2E-007. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_cyclic_diverse_page_read_noop_preserves_notion` | Live mounted workflow, plain files | Create page with diverse blocks and references, render expected Markdown, no-op push, verify Notion block JSON unchanged. Covers E2E-008, E2E-009, E2E-015 read paths. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_cyclic_supported_block_edits_push_and_verify_notion` | Live mounted workflow, plain files | Edit supported rich blocks and typed mentions through Markdown, push, fetch/verify Notion render. Covers E2E-014, E2E-015. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_local_image_media_edit_uploads_and_reconciles_bytes` | Live mounted workflow, plain files | Overwrite a downloaded local image file, edit its Markdown caption, push the upload to Notion, and verify the reconciled local image bytes remain local. Covers image upload behavior for E2E-010 and E2E-014. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_local_file_like_media_appends_upload_and_reconcile_local_links` | Live mounted workflow, plain files | Append local video, PDF, audio, and HTML links from `.afs/media`, push uploads to Notion, and verify reconciled Markdown uses local media links with non-empty materialized files. Covers file-like media upload behavior for E2E-010 and E2E-014. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_cyclic_database_rows_mount_edit_create_and_verify_notion` | Live mounted workflow, plain files | Create database, pull schema, hydrate row, no-op preserves row JSON, edit row properties/body, create row from new Markdown file, fetch/verify Notion. Covers E2E-016, E2E-018, E2E-019. |
| `tests/windows_cloud_files_live.ps1` | Live Windows Cloud Files provider | Registers a real Cloud Files sync root, lazily enumerates a scratch Notion page, hydrates `page.md`, edits and pushes it, creates/renames/deletes a pending draft, creates a child page directory through the mount, pushes it to Notion, then deletes and pushes the child archive. Covers Windows provider paths for E2E-004, E2E-006, E2E-011, E2E-013, and E2E-020. |
| `crates/afs-cli/tests/projection_contract.rs` | Local shared projection contract | Runs the same daemon virtual-filesystem browse, hydrate, write, create, rename, and delete contract against macOS File Provider, Linux FUSE, and Windows Cloud Files projection modes below the OS adapters. Covers the shared semantics behind E2E-004, E2E-005, E2E-006, E2E-011, and E2E-020. |
| `crates/afs-cli/tests/doctor.rs` | Local diagnostics contract | Verifies `afs doctor` does not initialize missing state and reports mount, connection, profile, and credential findings with recovery commands. |
| `crates/afs-cli/tests/live_workspace_mount.rs::live_workspace_pull_edit_pull_push_regression` | Manual/destructive live workspace | Pull workspace/file, edit mounted file, dirty pull skip, push, pull, verify content. Covers parts of E2E-011, E2E-013, E2E-022, but is ignored and uses a pre-existing workspace. |
| `crates/afs-notion/tests/fetch_render.rs::live_fetch_and_render_page_from_environment` | Manual live fetch/render | Fetch/render one configured page. Legacy smoke; lower coverage than the scratch live tests. |

## Current Live Coverage Summary

| Area | Live status | Notes |
|---|---|---|
| Notion page read/write integrity | Strong | Scratch pages are created, edited, pushed, fetched, and verified. |
| Rich block coverage | Strong for supported block types | Representative supported blocks are read and edited live. Unsupported/layout blocks are read/no-op protected. |
| Database schema and row writes | Strong | Schema, validation, property update, body update, and row create are covered. |
| Media download/upload | Covered for common file-like media | Live tests download image, video, file, PDF, and audio assets locally; upload edited image bytes; and append local video, PDF, audio, and HTML uploads while verifying local links after reconciliation. Broader media pruning/cache lifecycle is not covered. |
| Mounted workflow | Good for core, partial for platform kernels | Live tests cover plain-file mounted workflow, virtual filesystem lazy paths, and Windows Cloud Files registration against live Notion. Linux FUSE live Notion and macOS File Provider live Notion remain open. |
| Desktop onboarding/tray/review UI | Manual only | Important product surface, but not currently covered by automated live e2e. |
| Freshness/drift/auto-fast-forward | Partial live | Live tests cover drift preflight, dirty-pull conflict recovery, fast-forward apply/skip behavior, and scheduled pull queuing/applying a remote fast-forward. Long-running scheduler budget assertions remain local-only. |
| OAuth broker | Local only | Secret separation is tested; real OAuth UX/broker round-trip is manual. |
| Packaging/notarization | Manual/publish covered | `make publish` validates signing, stapling, and DMG integrity outside CI. |

## Remaining Recommended Live E2E Additions

Implemented in `crates/afs-cli/tests/e2e_push_workflow.rs`: live lazy virtual
mount, virtual page rename/delete, drift preflight, remote fast-forward, and URL
locate coverage.

1. **Linux/macOS kernel live mount e2e**: run live Notion lazy enumeration and
   create/rename/delete through real Linux FUSE in CI and signed macOS File
   Provider locally or on a dedicated Mac.
2. **Daemon scheduler budget e2e**: run a long-lived live workspace scheduler
   test and assert bounded API work over time.
3. **Desktop app smoke e2e**: run the Tauri app against a disposable state dir
   and fake/live backend, verify onboarding reset, pending changes refresh,
   non-blocking push, success confirmation, tray state, and low idle CPU.
4. **OAuth broker smoke**: use a test Notion integration and broker deployment
   to verify the local client receives/stores only the broker refresh handle.
