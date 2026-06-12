# AFS Desktop

Tauri desktop shell for AFS onboarding, workspace controls, pending-change
review, and settings.

## Development

Install dependencies once from the repo root:

```sh
make setup
```

Start the Tauri desktop app in development mode:

```sh
make dev-tauri
```

Equivalent direct command:

```sh
npm --prefix apps/desktop run tauri -- dev
```

For the frontend-only Vite preview:

```sh
make dev-desktop
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

## macOS Packaging

Build local `.app` and `.dmg` artifacts from the repo root:

```sh
make build-tauri
```

The build stages the macOS File Provider extension before Tauri bundles the
app. See `docs/macos-distribution.md` for signing, notarization, and Homebrew
cask notes.

## Current Scope

This app implements the first desktop UI pass from `docs/desktop-app.md` and
`docs/desktop-ui-screens.md`. The Tauri shell now reads local AFS state,
starts the Notion broker OAuth flow, exposes the main daily-use screens, and
opens a tray popover window. Remaining product gaps, especially workspace-level
Notion mount creation and multi-file push orchestration, are tracked in
`docs/deviations.md`.
