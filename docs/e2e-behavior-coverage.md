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
```

Run:

```sh
cargo test -p afs-notion --test live_integrity -- --ignored --test-threads=1
cargo test -p afs-cli --test e2e_push_workflow live_ -- --ignored --test-threads=1
```

GitHub Actions runs these from `.github/workflows/notion-live-e2e.yml` on
`main` when `NOTION_TOKEN` and `AFS_NOTION_LIVE_PARENT_PAGE` are configured in
the `notion-live-e2e` environment.

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
| E2E-002 | New install/onboarding can reset stale local beta state, then proceed through connection and mount setup. | Gap | Desktop command/unit coverage plus manual DMG testing. | Needs a desktop automation harness or scripted signed-app smoke on macOS. |
| E2E-003 | Workspace mount is created under the macOS CloudStorage AFS root, with source folders below `AFS/notion` and no Documents symlink dependency. | Gap | Mount path validation tests in `apps/desktop/src-tauri/src/main.rs`. | Real File Provider mount behavior is manual-only on macOS. |
| E2E-004 | A mounted workspace can expose top-level source directories and page/database entries without materializing every page body. | Covered live for virtual filesystem core | `architecture_behavior::local_virtual_mount_supports_browse_open_edit_review_push_round_trip`; `tests/linux_fuse_smoke.sh`. | Live Notion now exercises the lazy virtual filesystem path directly; kernel FUSE/File Provider registration remains local/manual. |
| E2E-005 | Opening/listing a nested page directory lazily discovers immediate children only. | Covered live for virtual filesystem core | `architecture_behavior::local_virtual_mount_supports_browse_open_edit_review_push_round_trip`. | Kernel FUSE/File Provider shell/Finder path still needs platform e2e. |
| E2E-006 | Opening a page file hydrates full content into canonical Markdown. | Covered live | Local pull and architecture behavior tests. | Covered through plain-file pull; real File Provider open hydration remains manual/macOS. |
| E2E-007 | Pasting a Notion URL locates/prioritizes the matching local file path and reveal selects the Markdown file. | Covered live for locate and hydration priority | Search/locate helpers and desktop command code are covered locally in pieces. | Desktop Finder reveal/select assertion still needs UI automation. |
| E2E-008 | Page Markdown read supports rich Notion blocks and keeps unsupported/read-only blocks visible without lossy edits. | Covered live | Broad renderer/unit coverage. | Covered for representative block types, not every future Notion block variant. |
| E2E-009 | A read/no-op push of a rich page does not mutate Notion JSON. | Covered live | Local no-op push tests. | Covered by live cyclic diverse page test. |
| E2E-010 | Image/media references render in Markdown and images can be downloaded into a local media directory for agent inspection. | Covered live | Media render/unit coverage. | Live test covers image download; broader media pruning/cache lifecycle is not covered. |
| E2E-011 | Editing a local page marks it pending in status before push. | Covered live | Local status and architecture behavior tests. | Live coverage uses CLI status; desktop/tray pending display remains manual. |
| E2E-012 | Diff/review planning reports intended changes before applying remote writes. | Covered live | `mount_pull_mid_page_insert_push_and_status_clean`; push/diff tests. | Desktop review UI is not e2e automated. |
| E2E-013 | Pushing a simple page edit writes to Notion, fetches back, and status returns clean. | Covered live | Local push workflow tests. | Covered. |
| E2E-014 | Supported rich block edits push and verify: paragraph, headings 1-4, bullet, number, todo, quote, callout, code, divider, equation, table, bookmark, embed, media URL/caption. | Covered live | Notion apply/render tests. | Covered for representative edits; not every color/layout option. |
| E2E-015 | Typed rich text round-trips for links, annotations, inline equations, page/database/date/user mentions. | Covered live | Notion renderer/apply tests. | Live write requires explicit IDs for page/database/user mentions. |
| E2E-016 | Database directories expose `_schema.yaml` and row Markdown files. | Covered live | Pull and schema tests. | Covered through plain-file live mounted workflow, not live File Provider. |
| E2E-017 | Database row property validation rejects invalid options before writing. | Covered live | Schema validation tests. | Live coverage is connector-level for invalid option validation, not the desktop UI path. |
| E2E-018 | Existing database rows can edit body and supported properties: title, rich text, number, select, status, multi-select, checkbox, date, URL, email, phone, external files, people IDs, relation IDs. | Covered live | Notion apply tests. | Covered for the representative property set. |
| E2E-019 | Creating a new Markdown file under a database directory creates a Notion row and verifies the rendered row. | Covered live | Local virtual create tests. | Covered through live mounted workflow. |
| E2E-020 | Local creates/renames/deletes in virtual mounts surface as pending virtual mutations. | Local only | `tests/linux_fuse_smoke.sh`; virtual filesystem tests. | Needs live Notion File Provider/FUSE create/rename/delete e2e. |
| E2E-021 | Push preflight detects remote drift before writing and blocks or requires review instead of overwriting. | Covered live | `crates/afs-cli/tests/push.rs`, `crates/afs-core/tests/push_executor.rs`, Notion concurrency unit tests. | Covered with scratch page local+remote drift; desktop review UI still separate. |
| E2E-022 | Pull of a dirty local file never overwrites local pending edits and does not insert conflict markers unless remote content diverged. | Local only | `crates/afs-cli/tests/pull.rs`; `crates/afs-cli/tests/live_workspace_mount.rs` manual/destructive. | Needs non-destructive live Notion dirty-pull/drift test. |
| E2E-023 | Remote observation uses cheap metadata and does not hydrate full block bodies. | Local only | `crates/afs-notion/tests/observe.rs`; freshness tests. | Needs live Notion observation API smoke that asserts metadata-only behavior indirectly. |
| E2E-024 | Freshness scheduling keeps active/pending paths hot, uses bounded work, and avoids full-workspace scans. | Local only | `crates/afsd/tests/runtime.rs`, `scheduler.rs`, `scheduled_pull.rs`, `hydration_queue.rs`. | Needs long-running live workspace test with bounded API-call assertions. |
| E2E-025 | Clean inactive files can auto-fast-forward when remote changes, while active or locally pending files are protected. | Covered live for hydration executor behavior | `crates/afsd/tests/runtime.rs`; `crates/afsd/tests/hydration_executor.rs`; `crates/afs-core/tests/explain.rs`. | Daemon scheduler trigger remains local-only; live test covers the remote fast-forward apply/skip behavior. |
| E2E-026 | `afs inspect` explains remote-only vs local+remote divergence without mutating local or remote state. | Local only | `crates/afs-cli/tests/inspect.rs`; `crates/afs-core/tests/explain.rs`. | Needs live Notion inspect e2e. |
| E2E-027 | Push success journals the operation and enables future history/undo surfaces where possible. | Partial live | Local journal/history/undo tests. | Live pushes journal and clean status; live remote undo is not covered. |
| E2E-028 | Desktop push runs asynchronously, does not freeze the UI, and briefly confirms success. | Manual only | Desktop command wiring and prior manual test. | Needs UI automation around a test backend or live Notion page. |
| E2E-029 | Tray icon appears, reflects ready/pending/error state, and idle desktop CPU remains near zero. | Manual only | Tray icon unit tests and manual CPU profile. | Needs desktop runtime smoke/perf test. |
| E2E-030 | Packaged DMG includes signed `afsd`, File Provider extension, CLI helper, tray app, and passes notarization. | Manual/publish covered | `make publish` signing/notarization validation. | Not currently a CI e2e because it requires Apple credentials. |

## Live Notion Test Coverage Map

| Test | Kind | Behaviors covered |
|---|---|---|
| `crates/afs-notion/tests/live_integrity.rs::live_page_read_edit_write_verify_integrity_with_media_download` | Live connector | Fetch/render rich page content, media download, update supported blocks, append block, fetch/verify rendered Notion content. Covers E2E-006, E2E-008, E2E-010, E2E-014. |
| `crates/afs-notion/tests/live_integrity.rs::live_database_row_property_create_edit_verify_integrity` | Live connector | Create live database, get schema, validate row frontmatter, create row, update supported properties, fetch/verify row. Covers E2E-017, E2E-018. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_scratch_page_mount_edit_push_verifies_notion` | Live mounted workflow, plain files | Create scratch page, mount, pull, edit Markdown, diff, dirty status, push, clean status, fetch/verify Notion. Covers E2E-006, E2E-011, E2E-012, E2E-013. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_lazy_virtual_mount_enumerates_children_and_hydrates_on_open` | Live virtual filesystem core | Create scratch page tree, enumerate virtual source root and page directory without content materialization, then hydrate on open and verify Markdown. Covers E2E-004, E2E-005, E2E-006. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_drift_preflight_blocks_push_before_overwriting_remote` | Live mounted workflow, plain files | Pull scratch page, edit locally, mutate the same page remotely, assert push blocks before overwrite, fetch/verify remote edit remains. Covers E2E-021. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_remote_fast_forward_updates_clean_file_and_preserves_pending_file` | Live virtual filesystem core | Hydrate a clean virtual file, mutate remote, fast-forward local content; then add local pending edit and assert remote fast-forward skips without overwrite. Covers E2E-025. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_locate_notion_url_returns_markdown_path_and_can_prioritize_hydration` | Live virtual filesystem core | Index scratch page metadata, locate by Notion URL, assert Markdown file path and online-only state, then hydrate located page. Covers E2E-007. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_cyclic_diverse_page_read_noop_preserves_notion` | Live mounted workflow, plain files | Create page with diverse blocks and references, render expected Markdown, no-op push, verify Notion block JSON unchanged. Covers E2E-008, E2E-009, E2E-015 read paths. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_cyclic_supported_block_edits_push_and_verify_notion` | Live mounted workflow, plain files | Edit supported rich blocks and typed mentions through Markdown, push, fetch/verify Notion render. Covers E2E-014, E2E-015. |
| `crates/afs-cli/tests/e2e_push_workflow.rs::live_cyclic_database_rows_mount_edit_create_and_verify_notion` | Live mounted workflow, plain files | Create database, pull schema, hydrate row, no-op preserves row JSON, edit row properties/body, create row from new Markdown file, fetch/verify Notion. Covers E2E-016, E2E-018, E2E-019. |
| `crates/afs-cli/tests/live_workspace_mount.rs::live_workspace_pull_edit_pull_push_regression` | Manual/destructive live workspace | Pull workspace/file, edit mounted file, dirty pull skip, push, pull, verify content. Covers parts of E2E-011, E2E-013, E2E-022, but is ignored and uses a pre-existing workspace. |
| `crates/afs-notion/tests/fetch_render.rs::live_fetch_and_render_page_from_environment` | Manual live fetch/render | Fetch/render one configured page. Legacy smoke; lower coverage than the scratch live tests. |

## Current Live Coverage Summary

| Area | Live status | Notes |
|---|---|---|
| Notion page read/write integrity | Strong | Scratch pages are created, edited, pushed, fetched, and verified. |
| Rich block coverage | Strong for supported block types | Representative supported blocks are read and edited live. Unsupported/layout blocks are read/no-op protected. |
| Database schema and row writes | Strong | Schema, validation, property update, body update, and row create are covered. |
| Media download | Covered for images | Live test downloads image assets locally. Video/file/PDF/audio are rendered as links, not downloaded. |
| Mounted workflow | Good for core, partial for platform kernels | Live tests cover plain-file mounted workflow and virtual filesystem lazy paths. Online-only kernel File Provider/FUSE registration against live Notion is not covered. |
| Desktop onboarding/tray/review UI | Manual only | Important product surface, but not currently covered by automated live e2e. |
| Freshness/drift/auto-fast-forward | Partial live | Live tests cover drift preflight and fast-forward apply/skip behavior. Scheduler trigger policy remains local-only. |
| OAuth broker | Local only | Secret separation is tested; real OAuth UX/broker round-trip is manual. |
| Packaging/notarization | Manual/publish covered | `make publish` validates signing, stapling, and DMG integrity outside CI. |

## Remaining Recommended Live E2E Additions

Implemented in `crates/afs-cli/tests/e2e_push_workflow.rs`: live lazy virtual
mount, drift preflight, remote fast-forward, and URL locate coverage.

1. **Kernel live mount e2e**: run live Notion lazy enumeration through real
   Linux FUSE in CI and signed macOS File Provider locally or on a dedicated Mac.
2. **Daemon scheduler live fast-forward e2e**: start `afsd`, create a remote
   change, wait for bounded observation/scheduler work, and assert the daemon
   queues and applies/skips fast-forward correctly.
3. **Desktop app smoke e2e**: run the Tauri app against a disposable state dir
   and fake/live backend, verify onboarding reset, pending changes refresh,
   non-blocking push, success confirmation, tray state, and low idle CPU.
4. **OAuth broker smoke**: use a test Notion integration and broker deployment
   to verify the local client receives/stores only the broker refresh handle.
