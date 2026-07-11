# Locality Desktop

Tauri desktop shell for Locality onboarding, workspace controls, pending-change
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

Tauri dev mode builds fresh debug `loc` and `localityd` sidecars before launching
the app. This keeps the desktop build ID and the daemon build ID aligned when
you switch commits or rebuild from source.

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
cargo check -p locality-desktop
```

## macOS Packaging

Build local `.app` and `.dmg` artifacts from the repo root:

```sh
make build-tauri
```

The build stages the macOS File Provider extension before Tauri bundles the
app, packages the `loc` CLI and `localityd` sidecar, and post-processes the DMG with
a dedicated installer disk icon. See `docs/macos-distribution.md` for signing,
notarization, terminal command setup, and Homebrew cask notes.

## Windows Packaging

Build a local NSIS installer from the repo root on Windows:

```sh
make build-tauri-windows
```

For a Windows on Arm installer:

```sh
rustup target add aarch64-pc-windows-msvc
make build-tauri-windows-arm64
```

The build stages `loc.exe`, `localityd.exe`, and `locality-cloud-files.exe` under
`src-tauri/windows` before Tauri bundles the app. The installer copies those
sidecars next to the desktop executable so the app can start the packaged
daemon, locate the packaged CLI, and call the Windows Cloud Files registration
and provider runtime helper.
At runtime, the Windows desktop app starts the `locality-cloud-files.exe run`
provider for existing Cloud Files mounts and restarts supervised provider
children if they exit.
On uninstall, the NSIS hook removes the sidecars, the per-user Windows login
item, and Locality-managed terminal command shims.
See `docs/windows-distribution.md` for release signing, updater artifacts, and
the GitHub release workflow.

## Current Scope

This app implements the first desktop UI pass from `docs/desktop-app.md` and
`docs/desktop-ui-screens.md`. The Tauri shell now reads local Locality state,
starts the Notion broker OAuth flow, exposes the main daily-use screens, and
opens a tray popover window. Remaining product gaps, especially workspace-level
Notion mount creation and multi-file push orchestration, are tracked in
`docs/deviations.md`.
