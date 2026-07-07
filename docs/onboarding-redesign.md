# Locality Onboarding Redesign Draft

Status: discovery and wireframe draft, not implementation.

## Goal

Redesign first-run onboarding so a new user understands Locality before they
hit app-specific setup. The flow should still complete the required setup:
app authorization, macOS File Provider permission, local folder selection, mount
creation, and agent guidance install.

## External Research Takeaways

- Permission-heavy products work best when they explain the permission before
  the OS or browser prompt appears. The prompt should feel expected, not like an
  interruption.
- The strongest setup flows prove the workflow quickly. They do not teach every
  feature; they get the user to one successful action, then let product surfaces
  continue the education.
- Agent products need a mental model before a control surface. Users need to
  know what the agent can see, what it can edit, and where approval happens.
- Local integration setup should avoid config-heavy language. If Locality
  updates agent skills, MCP config, or guidance files, the UI should frame that
  as "agents can now use this folder," not as a technical installation task.
- The Lexaro website style is most useful here as a structure: strong headline,
  concrete workflow demo, proof/benefit cards, and a focused CTA. For the
  desktop app, keep the density native and utility-oriented.

## Product Facts To Preserve

- Locality turns work apps like Notion into local Markdown files.
- The current first-run path uses Notion as the first connector, but the product
  should be framed around work apps generally.
- On macOS, the visible root is under CloudStorage, with source folders beneath
  it, for example:

```text
~/Library/CloudStorage/Locality/
  notion/
    AGENTS.md
    CLAUDE.md
    Engineering/
      Roadmap 2026/
        page.md
```

- Opening or reading files hydrates remote content on demand.
- Agents edit Markdown directly.
- Local edits remain pending for review by default.
- Live Mode can keep safe changes moving in the background, but it still pauses
  for conflicts, large changes, and review-required plans.
- Locality installs agent guidance for detected local agents and MCP fallback
  where supported.

## Recommended V1 Flow

Keep the setup as a short 5-screen wizard. Avoid an extra marketing tour, but
make each required setup step teach one product idea.

### 1. Meet Locality

Purpose: explain what Locality is before asking for Notion auth.

Primary copy:

```text
Turn work apps into agent-ready files.

Locality mounts tools like Notion as a local folder. Agents edit Markdown you
can inspect, while Locality keeps the connected app in sync after review.
```

Primary action: `Set Up Locality`

Secondary surface: a compact autoplaying product video:

```text
Notion page -> Locality/notion/page.md -> Pending review -> Notion updated
```

This should feel like native product media, not an embedded player. The app
loads the video from `VITE_LOCALITY_ONBOARDING_DEMO_VIDEO_URL`, which should be
set to the hosted Azure asset URL. The installer must not package the media
file. The source demo can be transcoded with:

```bash
cd apps/desktop
npm run transcode:onboarding-demo -- /tmp/herolocality.m4v /tmp/herolocality-onboarding.mp4
```

Upload the generated MP4 to Azure Blob Storage, then set
`VITE_LOCALITY_ONBOARDING_DEMO_VIDEO_URL` to:

```text
https://locvid0707134548.blob.core.windows.net/assets/onboarding/herolocality-onboarding-v1.mp4
```

The output is H.264 Main profile MP4, 30 fps, 960 px wide, no audio, and
fast-start for streaming. The current Azure resource group is
`locality-assets-rg`, storage account `locvid0707134548`, and container
`assets`.

Azure Front Door Standard was also provisioned as `locality-video-afd`, endpoint
`locality-video-0707134548`, with a default hostname:

```text
https://locality-video-0707134548-cba8dpgpangmcger.z01.azurefd.net/assets/onboarding/herolocality-onboarding-v1.mp4
```

As of setup, the Blob URL returns `200` with `Content-Type: video/mp4`, while
the Front Door URL returns an edge `404` with `x-cache: CONFIG_NOCACHE`.
Use the Blob URL until the Front Door route deploys or is recreated.

Support pills:

```text
Finder-native files
Markdown edits
Review before sync
```

Use the native file manager name for the platform, such as Finder on macOS and
File Explorer on Windows.

Design notes:

- Show the macOS File Provider permission as an expected setup item if it appears
  on this screen.
- Do not make the headline Notion-specific.
- Use a dark product-demo card on the right, matching the website's high-contrast
  demo panels.
- Avoid numbered steps on the welcome screen. Use product objects: app content,
  local file, pending review, and the updated app content after approval.

### 2. How Agents Use It

Purpose: teach the folder and review model before the user chooses a connector.

Primary copy:

```text
Agents work in files you can see.

Each app appears as a folder. Pages and docs become `page.md` files that stay
in sync with the connected app. Locality adds `AGENTS.md` and `CLAUDE.md` so
agents know how to work in the folder safely.
```

Primary action: `Continue`

Demo modules:

- folder tree with `notion/`, future `google-docs/`, and `linear/` examples;
- Markdown preview with editable body and protected `loc:` identity metadata;
- small review panel: `3 local edits ready to sync` and
  `Review before updating Notion`.

Demo plan:

Screen 2 should teach the working model, not repeat the product promise from
screen 1. The right-side product surface should show what users and agents
actually touch after setup:

1. Finder-style folder:

```text
Locality/
  notion/
    AGENTS.md
    CLAUDE.md
    Launch Plan/
      page.md
```

Use `notion/` as the active concrete example. Keep future apps absent or very
muted so the demo feels real rather than hypothetical.

2. Selected Markdown file:

```markdown
# Launch Plan

## Launch checklist
- Finalize onboarding
- Review pricing page
- Publish announcement

loc: notion-page
```

Add a small `Edited` badge so it is clear this is live content agents and humans
can change.

3. Review/sync state:

```text
3 local edits ready to sync
Review before updating Notion
```

The demo should communicate: Locality gives agents a normal folder. Agents and
humans edit Markdown files. Locality keeps those files synced with the app, and
remote updates happen after review.

Design notes:

- This replaces vague benefit copy with the actual operating model.
- Keep this fast. No decisions on this screen.

### 3. Connect First Source

Purpose: keep Notion setup intact while making the product connector-neutral.

Primary copy:

```text
Start with Notion.

Connect the workspace you want agents to help with. More sources use the same
local folder model as they become available.
```

Primary action: `Connect Notion`

Connector cards:

- `Notion` - available now.
- `Google Docs` - coming next or beta, depending on release readiness.
- `Linear` - planned.

OAuth waiting state:

```text
Finish connecting in Notion.

A browser window is open. Choose the workspace and pages Locality can access,
then approve.
```

Design notes:

- The browser/OAuth state is part of this screen, not a separate conceptual
  product step.
- Show secure storage and scoped access as quiet proof points.
- For macOS onboarding, use `Credentials in Keychain`. For cross-platform copy,
  use `OS-secured credentials`. Avoid generic `Secure OS storage` because the
  mounted files are plain local files; only app credentials are OS-protected.
- Add a quiet proof point that the local machine talks directly to the connected
  app and workspace content is not routed through a backend.

### 4. Create Local Folder

Purpose: keep the CloudStorage root clear without asking the user to choose a
path during onboarding.

Primary copy:

```text
Creating your local folder.

Locality creates your connected apps under one CloudStorage root. Agents and
Finder will use this folder automatically.
```

Default path:

```text
~/Library/CloudStorage/Locality/notion
```

Entry action: advance here automatically after Notion is connected.

Mounting state:

```text
Creating folder...
Preparing Notion files and agent guidance.
```

Design notes:

- Use the default CloudStorage mount root. Do not prompt for a folder choice in
  onboarding unless the automatic mount creation fails and needs recovery.
- Keep retry affordances disabled/busy while mounting so setup cannot start
  twice.
- Do not show a folder-layout preview here; screen 2 already teaches the file
  structure. Keep this step focused on the mount busy state.
- If macOS asks for File Provider access here instead of screen 1, the screen
  should say "macOS will ask Locality to enable the local file bridge."

### 5. Ready And First Agent Task

Purpose: finish with action, not a passive success state.

Primary copy:

```text
Locality is ready!

Your Notion files are now mounted locally. Agents can open this folder, edit
Markdown, and leave changes for Locality review. Open the app to review changes,
manage sync, and turn on Live Mode when you want file saves to update Notion and
new Notion changes to appear locally.
```

Primary action: `Open Locality`

Secondary actions:

- `Copy agent prompt`
- `Open Locality Folder`

Demo prompt:

```text
Use Locality to edit my Notion workspace. Open the files under
~/Library/CloudStorage/Locality/notion, make the requested edits directly in
Markdown, and leave changes pending for Locality review.
```

Design notes:

- This is the transition into the full app. It should feel like completion, with
  a little more warmth than the setup steps, but no low-quality confetti.
- Use a single-column completion layout. Put the folder path, `Open Locality
  Folder`, and copyable prompt below the success body so long content cannot
  collide in a narrow app window.
- Include a copyable prompt that is useful immediately for Claude, Codex, or
  another agent.
- Show which agents were prepared, but do not over-index on technical targets.
- Do not put Live Mode as a primary onboarding action here. Mention it as part
  of the full app, and surface the actual control on the main home screen.
- Remove the "Open a page" section from onboarding.

## Key Copy Shifts

| Current | Proposed |
| --- | --- |
| Let your agents edit Notion as local files. | Turn work apps into agent-ready files. |
| Where should your Notion files appear? | Creating your local folder. |
| The Notion folder will include AGENTS.md and CLAUDE.md. | Locality adds agent guidance so tools know what is safe to edit. |
| Open Notion Folder | Open Locality Folder |
| Agents can use Locality | Agents are ready to use this folder |

## Interaction States To Design

- File Provider permission not granted.
- Notion OAuth denied or timed out.
- Browser failed to open, with copy login link fallback.
- Automatic folder creation failed.
- Mount creation in progress.
- Agent guidance install partial failure.
- Ready state with Live Mode off.
- Ready state with Live Mode on.

## Open Questions

- Should Google Docs appear as "Beta" in onboarding now, or should V1 keep it as
  a non-clickable future connector?
- Should the final screen default to `Open Locality Folder` or `Copy agent
  prompt` as the strongest first action?
- Should Live Mode be offered during onboarding, or only after the user sees the
  ready screen?
- Do we want a short "safe by design" proof card, or is the review model enough?

## Wireframe Artifact

Static review wireframes live at:

```text
docs/onboarding-redesign-wireframes.html
```
