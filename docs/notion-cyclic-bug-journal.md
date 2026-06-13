# Notion Cyclic Test Bug Journal

This journal tracks bugs found while exercising live Notion cyclic tests against
the disposable AFS e2e workspace. Each entry should include the live behavior,
the local symptom, and the fix made in the PR.

## 2026-06-13

### `link_to_page` Rendered As An AFS Directive

- **Found by:** `live_cyclic_diverse_page_read_noop_preserves_notion`.
- **Symptom:** A Notion `link_to_page` block rendered as
  `::afs{id=... type=link_to_page ...}`. That made a normal page link look like
  connector internals instead of a Markdown link that agents can follow.
- **Fix:** Render `link_to_page` blocks as ordinary Markdown links to Notion
  object URLs. Malformed link blocks still fall back to directives so corrupted
  native payloads are not silently flattened.
- **Verification:** Fixture tests now expect `[Linked page](https://www.notion.so/...)`
  and `[Linked database](https://www.notion.so/...)`. The live cyclic read test
  asserts no `type=link_to_page` directive appears for valid links.

### Full Same-Shape Page Edits Planned Archive/Recreate

- **Found by:** `live_cyclic_supported_block_edits_push_and_verify_notion`.
- **Symptom:** Editing every supported block in a page caused the diff engine to
  mark all original blocks for archive and all edited blocks for append. The
  push was blocked as a dangerous plan instead of producing block updates.
- **Cause:** Residual alignment degraded whenever more than one edited block and
  more than one shadow block were unmatched, even if their Markdown block kinds
  matched in order.
- **Fix:** Residual alignment now pairs blocks by order when the unmatched
  edited and shadow sequences have the same Markdown kind sequence. Mixed-kind
  residual sequences remain explicitly degraded.
- **Verification:** Added `residual_alignment_updates_same_kind_sequence_without_archive_recreate`
  and kept an ambiguous mixed-kind degradation test.

### UUID-Shaped External Links Could Be Parsed As Page Mentions

- **Found by:** pre-PR code review of the Notion URL write parser.
- **Symptom:** The new page-mention parser accepted any Markdown link whose URL
  ended with 32 hexadecimal characters. An unrelated external link could
  therefore be converted into a Notion page mention.
- **Fix:** Page mention writes now accept legacy `afs://` links and URLs on
  Notion hosts only (`www.notion.so`, `notion.so`, and `app.notion.com`).
  Slugged and hyphenated Notion page IDs are still accepted.
- **Verification:** The rich text apply test now includes an external URL with a
  UUID-shaped path and verifies it remains a normal linked text span.
