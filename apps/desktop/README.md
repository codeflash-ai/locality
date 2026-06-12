# AFS Desktop

Tauri desktop shell for AFS onboarding, workspace controls, pending-change
review, and settings.

## Development

```sh
npm install
npm run dev
```

Open the Vite preview at `http://127.0.0.1:1420/`.

Useful preview routes:

- `http://127.0.0.1:1420/` starts at first-run onboarding.
- `http://127.0.0.1:1420/#app` starts at the main app shell.
- `http://127.0.0.1:1420/#tray` starts at the tray popover surface.

The Rust side is under `src-tauri` and can be checked from the repo root:

```sh
cargo check -p afs-desktop
```

## Current Scope

This app implements the first desktop UI pass from `docs/desktop-app.md` and
`docs/desktop-ui-screens.md`. The Tauri shell now reads local AFS state,
starts the Notion broker OAuth flow, exposes the main daily-use screens, and
opens a tray popover window. Remaining product gaps, especially workspace-level
Notion mount creation and multi-file push orchestration, are tracked in
`docs/deviations.md`.
