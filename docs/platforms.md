# Platform Guides

Locality keeps the sync model shared and lets each OS own only the native projection,
process, installer, and credential surfaces.

Start with the shared model:

- [Multiplatform architecture](multiplatform-architecture.md): platform
  boundaries, projection contracts, daemon ownership, and phase plan.
- [CLI surface](cli.md): shared user and agent commands, including
  `loc doctor`.
- [Daemon](daemon.md): process and IPC behavior.

Platform-specific guides:

- [macOS distribution](macos-distribution.md): DMG, File Provider extension,
  signing, notarization, Homebrew, and updater artifacts.
- [Linux distribution](linux-distribution.md): `.deb`, `.rpm`, FUSE helper,
  systemd user units, repository publishing, and AppImage updater artifacts.
- [Linux FUSE](linux-fuse.md): virtual projection runtime behavior and smoke
  testing.
- [Windows distribution](windows-distribution.md): NSIS packaging, signed
  sidecars, Cloud Files helper, GitHub release workflow, and live e2e checks.
