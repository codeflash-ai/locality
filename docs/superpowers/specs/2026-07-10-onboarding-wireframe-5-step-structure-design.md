# Desktop Wireframe Onboarding 5-Step Structure Design

## Goal

Update `docs/wireframes/index.html` so its onboarding flow follows the current
desktop app's 5-step structure while keeping the wireframe deck's existing
visual language and multi-source framing.

## Source Of Truth

The current desktop app onboarding flow in `apps/desktop/src/App.tsx` is the
behavioral source of truth for step order and screen responsibilities:

1. Meet Locality
2. How agents use it
3. Connect app
4. Local folder
5. Ready

This wireframe update should mirror that rhythm and screen ownership, but it
does not need to copy the app's layout or copy exactly.

## Approved Constraints

- Keep onboarding as a 5-step workflow.
- Keep the wireframe's multi-source framing instead of switching to a
  Notion-only story.
- Keep a dedicated "How agents use it" educational screen as step 2.
- Limit this pass to structure only.
- Preserve the existing wireframe deck styling and non-onboarding screens.

## Problem

`docs/wireframes/index.html` currently presents onboarding as a 4-step flow:

1. Welcome
2. Connect a source
3. Local folder + sync mode
4. Ready

That structure no longer matches the shipped desktop app. The mismatch causes
two problems:

- the wireframe skips the app's dedicated education step;
- the folder/setup screen is overloaded with unrelated choices such as sync
  mode, instead of acting as the app-like setup/progress step.

## Design

### Step 1: Welcome / Meet Locality

Purpose: introduce the product thesis before any setup mechanics.

Responsibilities:

- explain that Locality turns connected apps into local, agent-editable files;
- keep the promise of safe review before sync;
- offer one primary action to begin setup;
- keep a lower-emphasis recovery path for reinstall or repair scenarios.

This remains a product-intro screen. It should not ask the user to connect a
source or make setup decisions yet.

### Step 2: How Agents Use It

Purpose: teach the filesystem contract before the connection step.

Responsibilities:

- show that agents work in visible folders and files;
- explain that app content becomes local Markdown-like files;
- show that Locality can install guidance such as `AGENTS.md` and `CLAUDE.md`;
- reinforce that edits are reviewed before remote sync.

This is a dedicated educational screen with one clear `Continue` action. It is
not a source-selection or setup-progress screen.

### Step 3: Connect A Source

Purpose: authorize the first source while preserving the multi-source product
framing.

Responsibilities:

- keep source cards visible so Locality still reads as a multi-source product;
- make one active source path obvious;
- show inline connect states on this step only:
  - pre-connect
  - waiting for browser auth
  - connected
  - retry/error
- expose the fallback login-link copy action here.

This screen owns source connection state. Folder creation or setup progress does
not belong here.

### Step 4: Local Folder

Purpose: own the local folder creation and setup progress phase.

Responsibilities:

- show the CloudStorage path where the source will appear;
- show setup progress or blocked states such as approval required, waiting, or
  retry needed;
- keep the primary action tied to setup state;
- allow a recovery action like folder selection only when needed.

The current sync-mode choice should be removed from onboarding in this pass. The
desktop app's onboarding rhythm treats this screen as setup/progress, not as a
policy-selection screen.

### Step 5: Ready

Purpose: hand off into normal product use after setup is complete.

Responsibilities:

- confirm that the folder is mounted and usable;
- keep the mounted path visible;
- explain that agents can work here now;
- keep any prompt/example secondary to the completion handoff;
- keep the primary action focused on entering the product or opening the mounted
  folder context.

This screen should not act like a final setup checklist. It appears only after
the local folder/setup step is complete.

## Wireframe Mapping

The existing onboarding sections in `docs/wireframes/index.html` should be
remapped like this:

| Current section | New role |
| --- | --- |
| `ob1` | Step 1: Welcome / Meet Locality |
| new section | Step 2: How agents use it |
| current `ob2` | Step 3: Connect a source |
| current `ob3` | Step 4: Local folder |
| current `ob4` | Step 5: Ready |

## Interaction Rules

- Every onboarding screen should show `step X / 5`.
- Every onboarding window chrome should use a 5-dot step indicator.
- The onboarding flow remains linear:
  - Welcome
  - How agents use it
  - Connect a source
  - Local folder
  - Ready
- Back navigation should exist only where it already makes sense in the
  wireframe deck after the intro.
- Step 3 owns source-connection state.
- Step 4 owns setup-progress and setup-recovery state.
- Step 5 appears only after setup is complete.
- Multi-source framing stays visual and product-level, not structural. The flow
  still progresses through one primary source-connection path.

## Scope

### In Scope

- onboarding nav updates for steps `1.1` through `1.5`;
- adding a new onboarding section for "How agents use it";
- renumbering and relabeling existing onboarding screens to match the 5-step
  sequence;
- updating onboarding progress text and step indicators from 4 steps to 5;
- removing the sync-mode choice from the onboarding folder step.

### Out Of Scope

- redesigning the deck's overall visual system;
- updating non-onboarding screens;
- making the wireframe pixel-match the desktop app;
- converting the onboarding story to a Notion-only flow;
- implementing actual app behavior changes in `apps/desktop`.

## Validation

After implementation, validate by reloading the locally served wireframe deck
and checking:

1. the deck exposes 5 onboarding screens in order;
2. each onboarding screen shows the correct `step X / 5` label;
3. each onboarding window uses a 5-dot indicator;
4. the second onboarding screen is the dedicated "How agents use it" education
   step;
5. the local-folder step no longer contains the sync-mode choice;
6. the connect step still presents Locality as a multi-source product.
