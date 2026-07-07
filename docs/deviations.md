# Deviations From `plan.md`

`plan.md` is authoritative. Any intentional implementation deviation must be documented here before it becomes part of the codebase.

## Active Deviations

None.

## Temporary Implementation Gaps

- Desktop UI command handlers now read the local Locality store, daemon health,
  status reports, mounts, connections, journals, and the Notion OAuth broker.
  They still fall back to sample data when the local store cannot be opened so
  the UI remains reviewable before a first real connection.
- Desktop onboarding still uses a timer to advance from the Notion OAuth helper
  screen after starting the real broker flow in-process. The production app
  should replace this timer with real connection-state events from the OAuth
  callback and store refresh.
- The native Tauri tray now opens an initial rich popover window using the
  confirmed clean Aperture icon, refreshes its snapshot when opened, switches to
  amber/red badged icons only for review or reconnect states, and keeps the
  native menu for secondary actions. Exact native popover positioning polish
  remains to be implemented.
- Desktop push review currently pushes the first pending file through the shared
  Rust push path after the user approves the review. A multi-file review/apply
  loop should batch or sequence all selected pending changes.
- Desktop bundling now targets macOS `.app` and `.dmg`, Linux `.deb`/`.rpm`,
  and Linux AppImage updater artifacts with the required sidecars staged before
  Tauri bundling. App Store submission, public APT/DNF repository activation,
  and production icon sets remain distribution milestones.
- Toggle blocks currently render as anchored directives with their summary in the `title` attribute. This preserves identity and child content, but it is not yet the clean nested-list or `<details>` round-trip targeted by `plan.md`.
- Layout-rich blocks such as columns, tabs, synced blocks, AI/custom blocks, and meeting notes are directive-backed until the diff/apply layer can preserve their nesting and source-specific semantics safely.
- Database row creation currently validates writable property names and types against the live Notion data source during apply. The `plan.md` target is local `_schema.yaml` validation during the parse/validate stage; that schema-backed preflight remains the next property-validation milestone.

## Open Design Questions Carried From `plan.md`

- Hydration aggressiveness remains configurable. The code defaults to the 90-day policy and no eager-under-size threshold.
- `_view.csv` remains read-only unless the plan is updated.
- Journals now store core shadow preimages and apply effects for undo planning; native connector preimages remain undecided.
- `loc` remains the working title.
