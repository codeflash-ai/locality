# `afs-core` Design

`afs-core` is the connector-agnostic correctness layer. It should stay free of Notion API calls, SQLite details, file watching, daemon lifecycle, and CLI formatting.

## Design Rules

- `plan.md` is authoritative.
- Core APIs should be deterministic and easy to property-test.
- Remote IDs are canonical. Paths are projections.
- Sync direction is derived from explicit remote/local/synced state.
- Validation and guardrail failures should be structured enough for agents to repair.
- Connector-specific rendering and schema rules should plug into the core rather than live inside it.

## Modules

| Module | Role |
| --- | --- |
| `canonical` | Markdown/frontmatter envelope parsing, stub detection, directive extraction, and stable rendering. |
| `model` | Mount IDs, remote IDs, entity fingerprints, hydration states, canonical documents, canonical blocks, and transient connector-rendered stub frontmatter. |
| `sync` | Three-tree classification and block-collision classification. |
| `conflict` | Conflict summaries, resolutions, and block change sets. |
| `hydration` | Hydration policy and request types. |
| `validation` | Structured validation reports and directive integrity checks. |
| `planner` | Connector-neutral push plans, plan summaries, and guardrail policy. |
| `push` | Explicit push pipeline request/output types, validation/diff/guardrail orchestration, journaled execution hooks, and guardrail evaluation. |
| `pull` | Polling/relay pull scheduler configuration. |
| `shadow` | Shadow document snapshots, Markdown block segmentation, stable block hashes, and source spans. |
| `diff` | Initial block-aware push planner over shadow snapshots and edited canonical documents. |
| `journal` | Push journal entry and status contracts. |
| `undo` | Connector-neutral reverse-plan derivation from journaled preimage snapshots. |
| `error` | Core error categories. |

## First Invariants Implemented

- Hydration states only move through legal transitions from the plan's ladder.
- Three-tree classification uses actual tree entries, not caller-supplied booleans.
- Remote-only changes pull when local is clean.
- Local-only changes push.
- Local and remote changes conflict unless block changes are explicitly disjoint.
- Remote deletion deletes the local projection only when the local file is clean.
- Directive lines may move unchanged or be removed as a delete signal, but edits and invented directive anchors fail validation.
- Push guardrails require confirmation when archives exceed the threshold or the plan touches more than the configured mount percentage.
- `TreeEntry.stub_frontmatter` is transient enumeration data. It lets connectors write complete stubs with source metadata, such as Notion database row properties, while the durable entity store keeps only identity, path, hydration, hashes, and remote timestamps.

## Canonical Document Layer

The canonical parser is intentionally shallow:

- It requires a YAML frontmatter envelope at the start of every canonical file.
- It parses the `afs` identity block, title, and arbitrary connector properties.
- It detects the exact stub marker from `plan.md`.
- It extracts `::afs{...}` directive lines and their line numbers.
- It renders the original frontmatter and body back to stable Markdown.

It does not parse all Markdown into blocks yet. For now, it only materializes directive blocks because directive integrity is a universal AgentFS rule. Full block segmentation belongs to the future block diff engine.

## Shadow And Diff Layer

The shadow layer stores the synced body text plus a block tree:

- each shadow block has a remote block ID, kind, source span, stable content hash, and rendered text;
- directive blocks get their remote ID from the visible directive line;
- clean Markdown blocks get their remote IDs from connector-rendered shadow metadata;
- table blocks can carry row-level remote IDs as shadow metadata for future table-aware apply;
- stable hashes use a deterministic in-process hash, not randomized runtime hashing.

The first planner is deliberately conservative:

- exact block hashes align first;
- directive IDs are anchored and validated before planning;
- residual unmatched native blocks align by order for simple edits;
- ambiguous residual alignment adds an explicit degradation note to the plan;
- directive edits fail validation instead of becoming lossy updates;
- directive moves are represented as block moves.

This is not the final Notion-grade diff engine from `plan.md`; it is the first correct contract surface. Later exact/structural/residual passes can improve the internals while preserving the same `ShadowDocument -> PushPlan` boundary.

## Push Pipeline And Execution Contract

The push pipeline composes the core primitives into the decision surface used by `afs diff` and `afs push`:

- read-only mounts stop before validation or planning;
- frontmatter identity and directive syntax validate before diffing;
- directive integrity errors from diff planning are surfaced as validation issues with file/line context;
- no-op plans return `Noop`;
- normal non-empty plans return `ConfirmPlan` unless `assume_yes` is set;
- dangerous plans return `ConfirmDangerousPlan` unless `confirm_dangerous` is set;
- confirmed dangerous plans still preserve the guardrail reasons in the output.

The pipeline itself still does not apply remote operations. It returns the next required action so CLI/daemon code can decide whether to ask for confirmation, execute the plan, or stop for fixes.

The push execution layer starts only from `ProceedToApply`. It is connector-neutral and requires host-supplied hooks for:

- remote concurrency checks immediately before apply;
- connector-specific remote apply;
- post-apply read-back and reconciliation.

Execution prepares the journal before any remote mutation, moves status through `Prepared`, `Applying`, `Applied`, and `Reconciled`, and marks `Failed` on concurrency, apply, or reconcile errors. Non-approved pipeline actions return `NotReady` without touching the journal or connector hooks.

Each approved operation also receives a deterministic `PushOperationId` derived from the push ID, operation index, operation kind, and target remote ID. Connectors return operation-level `JournalApplyEffect` values after apply. Those effects record durable facts such as updated blocks, archived blocks, and created remote block/entity IDs so resume and undo do not have to infer what happened from the remote alone.

## Undo Contract

Journal entries now include shadow preimages for affected entities. The undo planner uses those preimages to derive reverse operations without guessing:

- block updates reverse to the previous block text;
- block moves reverse to the previous sibling position;
- archived blocks reverse to a restore operation with original content and position;
- appends reverse to archiving the created block when apply journaled the created block ID;
- created entities reverse to archiving the created entity when apply journaled the created entity ID;
- property updates and archived entities are reported as unsupported until apply journals property/entity preimages.

Undo plans are marked `Complete`, `Partial`, or `Blocked`. A complete plan can now be handed to a connector reverse-apply hook; the Notion connector still returns `NotImplemented` until its API implementation exists.
