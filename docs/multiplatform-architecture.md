# Multiplatform Architecture

Locality should treat operating systems as presentation and host-integration
surfaces, not as sync-engine variants. The product promise is the same on every
platform:

- connected sources appear as normal local files and folders;
- online-only content is browseable before it is hydrated;
- first read can hydrate the file without exposing placeholder bytes;
- local edits remain reviewable and explicit until `loc push`;
- the daemon remains the single owner of durable state, scheduling, connector
  calls, hydration, dirty-state transitions, journals, and push execution.

The existing core split already points in the right direction. This plan keeps
`locality-core`, connector crates, `locality-store`, and the daemon runtime mostly intact,
then adds explicit platform interfaces around the places where OS behavior is
currently implicit.

## Design Goals

1. Preserve the connector-agnostic sync model.
   `locality-core` must not learn about File Provider, FUSE, Cloud Files, services,
   named pipes, launchd, systemd, or Windows paths.

2. Make platform adapters peers.
   macOS File Provider, Linux FUSE, and Windows Cloud Files should all talk to
   the same daemon virtual filesystem API. None should own sync policy.

3. Make plain files a supported fallback, not the product architecture.
   Plain-file mounts are useful for tests, recovery, unsupported systems. Product-grade online-only mounts need OS-native
   virtualization.

4. Keep path semantics explicit.
   Locality entity paths are logical slash-separated source paths. Host paths are
   OS-native filesystem paths. Reports should deliberately choose one instead
   of leaking `Path::display()` everywhere.

5. Keep the daemon transport replaceable.
   Unix sockets are good for macOS/Linux CLI usage. Windows needs named pipes or
   authenticated localhost TCP. Sandboxed platform extensions may need TCP even
   on Unix. The daemon protocol should not care which transport carried it.

6. Make installation reversible.
   Every platform registration path needs a matching unregister/reset path that
   removes OS integration without deleting source-owned user data.

## Target Module Shape

```text
locality-core
  Pure sync model, planning, validation, conflicts, journals, hydration policy.

locality-connector
  Connector trait and connector-neutral apply/fetch contracts.

locality-notion
  Notion fetch/render/apply implementation.

locality-store
  SQLite state, credential-store trait, mount records, entity records.

localityd
  Daemon runtime, job queue, scheduler, hydration/push/pull execution,
  virtual filesystem service, daemon protocol handlers.

locality-platform
  Small cross-platform host abstraction crate:
    paths
    credentials
    ipc transport
    process manager
    opener
    projection registration

loc-cli
  User/agent command surface. Calls locality-platform and localityd protocol clients
  instead of hard-coding OS behavior.

platform/macos
  File Provider extension and helper.

platform/linux
  FUSE helper and systemd user-unit integration.

platform/windows
  Cloud Files provider/helper, Windows service/login startup, installer helpers.

apps/desktop
  Tauri UI and platform packaging. Uses locality-platform for host integration.
```

`locality-platform` can begin as modules inside `localityd`/`loc-cli` if that keeps the
first change smaller, but the product architecture should converge on a crate
with narrow traits. The important constraint is that platform decisions are made
in one layer, not scattered through CLI, daemon, desktop, and tests.

## Platform Interfaces

### Host Paths

Create a host path service that centralizes platform defaults and path display.

```rust
pub trait HostPaths {
    fn state_root(&self) -> PathBuf;
    fn logs_dir(&self) -> PathBuf;
    fn default_mount_root(&self) -> PathBuf;
    fn user_home(&self) -> Option<PathBuf>;
}

pub enum ReportPath<'a> {
    Logical(&'a Path),
    Host(&'a Path),
}
```

Rules:

- `LOCALITY_STATE_DIR` continues to override state root for tests/dev.
- macOS/Linux can keep `~/.loc` for compatibility.
- Windows should use `%LOCALAPPDATA%\Locality` or `%LOCALAPPDATA%\Locality` for
  durable app state, with `%USERPROFILE%` only as a fallback.
- Entity paths and virtual mutation paths should be rendered with `/` for JSON
  and agent-facing reports.
- Absolute host paths should use native OS separators.
- Shell suggestions should come from a platform-aware command renderer, not
  ad-hoc string interpolation.

This directly addresses the Windows test failures around `\` vs `/`, quoted
paths, and CRLF-sensitive fixture expectations.

### Credentials

Keep the existing `CredentialStore` trait but provide platform implementations:

```rust
pub trait CredentialStore: Send + Sync {
    fn put(&self, secret_ref: &str, secret: &str) -> CredentialResult<()>;
    fn get(&self, secret_ref: &str) -> CredentialResult<String>;
    fn delete(&self, secret_ref: &str) -> CredentialResult<()>;
}
```

Target implementations:

- macOS: Keychain, preserving the current behavior.
- Windows: Windows Credential Manager, with DPAPI as an acceptable internal
  mechanism if Credential Manager proves awkward for service/desktop sharing.
- Linux: libsecret where available; file credential store as an explicit dev/CI
  fallback.

File credentials should no longer be the default on Windows because they do not
match the product promise that credentials are stored in the OS credential
store.

### Daemon IPC

Separate daemon protocol from daemon transport.

```rust
pub trait DaemonTransport {
    fn send(&self, request: &DaemonRequest, timeout: Duration)
        -> Result<DaemonResponse, DaemonClientError>;
}

pub enum DaemonEndpoint {
    UnixSocket(PathBuf),
    WindowsNamedPipe(String),
    LocalTcp(SocketAddr),
}
```

Target defaults:

- macOS/Linux CLI: Unix socket first, localhost TCP fallback when configured.
- macOS File Provider: localhost TCP by default, matching the current Swift
  extension behavior; Unix sockets remain a development override.
- Linux FUSE: Unix socket or localhost TCP, selected by host config.
- Windows CLI/desktop/provider: Windows named pipe or authenticated localhost
  TCP.

The daemon JSON request/response protocol can remain as-is. `DaemonRequest`
already contains the right platform-neutral commands: pull, push, status,
virtual item lookup, children listing, materialize, commit write, create, rename,
and trash.

### Process Management

Move `loc daemon start|stop|status|reload|restart` process details behind a
manager interface.

```rust
pub trait DaemonProcessManager {
    fn start(&self, options: StartOptions) -> Result<StartReport, StartError>;
    fn stop(&self, state_root: &Path) -> Result<StopReport, StopError>;
    fn status(&self, state_root: &Path) -> ProcessStatus;
}
```

Target managers:

- macOS: LaunchAgent for installed app; detached session for development.
- Linux: systemd user service for installed app; detached session for
  development.
- Windows: Windows Service for installed product, or scheduled task/login item
  for per-user beta; detached child for development.

The CLI should report the same logical state regardless of manager:
`running`, `stopped`, `manager`, `state_root`, `endpoint`, `logs`.

### Projection Adapters

The daemon already has the correct platform-neutral boundary in
`localityd::virtual_fs`. Formalize it as a service contract used by all virtual
filesystem adapters.

```rust
pub trait VirtualProjectionClient {
    fn item(&self, mount_id: &MountId, identifier: &str) -> Result<VirtualFsItem>;
    fn children(&self, mount_id: &MountId, container: &str) -> Result<Vec<VirtualFsItem>>;
    fn materialize(&self, mount_id: &MountId, identifier: &str)
        -> Result<MaterializedFile>;
    fn commit_write(&self, mount_id: &MountId, identifier: &str, bytes: Vec<u8>)
        -> Result<WriteReport>;
    fn create_file(&self, mount_id: &MountId, parent: &str, filename: &str)
        -> Result<VirtualMutationReport>;
    fn rename(&self, mount_id: &MountId, identifier: &str, new_parent: &str, name: &str)
        -> Result<VirtualMutationReport>;
    fn trash(&self, mount_id: &MountId, identifier: &str)
        -> Result<VirtualMutationReport>;
}
```

Adapters must follow two rules:

- They do not mutate SQLite directly.
- They do not call connectors directly.

All slow source work is routed through the daemon runtime queue. This preserves
the existing single-owner mutation model.

## Platform Projection Strategy

### macOS: File Provider

Keep the current Swift File Provider extension and helper. Treat it as the
macOS projection adapter over `VirtualProjectionClient`.

Responsibilities:

- Register/unregister File Provider domains.
- Map Finder enumeration to `children`.
- Map fetch/open to `materialize`.
- Copy daemon-materialized bytes into File Provider's transfer location.
- Signal changes after daemon metadata refreshes.

Non-responsibilities:

- No connector calls.
- No SQLite writes.
- No push planning.

### Linux: FUSE

Keep `locality-fuse` under `platform/linux/locality-fuse`, with Linux-only dependency
selection.

Responsibilities:

- Mount one shared `linux_fuse` root containing mount-point folders.
- Serve `lookup`, `getattr`, and `readdir` from daemon metadata.
- Serve `open/read` through `materialize`.
- Stage `write/flush` bytes and call `commit_write`.
- Route create/rename/unlink to daemon virtual mutations.

Packaging:

- systemd user service remains the product-grade lifecycle manager.
- `loc file-provider register` repairs/restarts the shared-root unit.

### Windows: Cloud Files

Windows should use Cloud Files as the primary product architecture for
online-only mounts. Cloud Files is the closest native equivalent to OneDrive:
sync-root registration, placeholder files/directories, hydration callbacks, and
Explorer integration.

Responsibilities:

- Register/unregister an Locality sync root.
- Create placeholder directories/files from daemon metadata.
- On fetch/hydration callback, call daemon `materialize`, then supply bytes to
  Windows.
- On local modification/close, call daemon `commit_write` and mark the local
  item dirty in daemon state.
- Map local create/rename/delete notifications to daemon virtual mutations.
- Start and supervise the shared-root provider runtime from the desktop app.
- Surface sync/provider status in a Windows-native way where possible.

Implementation note: Cloud Files native callbacks cover close, rename, delete,
placeholder enumeration, and data fetch. Local creates are observed with a
sync-root filesystem watcher, converted to placeholders after the daemon records
the virtual mutation, and then tracked through the same placeholder identity path
as existing cloud items.

Remote reconciliation changes durable entity state before updating an existing
Cloud Files namespace entry. Undo follows that order and only relocates or removes
a materialized replica that still matches its previous synced shadow. Before the
filesystem operation, the daemon atomically records short-lived, one-shot
acknowledgements keyed by mount, access root, normalized provider identity, exact
relative path, and callback channel. Cloud Filter and watcher notifications each
consume their own acknowledgement only when durable entity state also matches;
ordinary, malformed, expired, or unmatched events use the normal mutation path.

Current implementation direction: Windows uses the Rust `locality-cloud-files.exe`
helper. The CLI exposes `loc file-provider start|stop|status|restart` as the
shared lifecycle surface for the provider runtime; the desktop app should call
the same platform layer when a mount is activated/opened or when the app
launches with existing Cloud Files mounts, and can build restart supervision on
top of the same provider PID/log metadata.

Open design questions:

- Whether the provider talks to `localityd` over named pipe or authenticated TCP.
- Whether the installed daemon is a per-user Windows Service, scheduled task, or
  desktop-managed background process.

### Windows: ProjFS as Secondary Option

dont implement it

Projected File System is a useful fallback or developer adapter if Cloud Files
integration proves too heavy, but it should not be the default product target.
ProjFS projects a hierarchy from a backing store, which matches metadata
projection, but Cloud Files better matches online-only sync-root behavior and
Explorer cloud-file expectations.

### WinFsp / Third-Party FUSE

dont implement it.
Avoid making WinFsp the primary product path. It adds a driver dependency and
does not give the same native cloud-file UX. It may be useful for development or
enterprise-controlled deployments, but it should not define the architecture.

## Mount Model

Extend `ProjectionMode` without changing existing meanings:

```rust
pub enum ProjectionMode {
    PlainFiles,
    MacosFileProvider,
    LinuxFuse,
    WindowsCloudFiles,
}
```

Platform defaults:

- macOS desktop: `MacosFileProvider`
- Linux desktop: `LinuxFuse`
- Windows desktop: `WindowsCloudFiles`
- CLI fallback on unsupported systems: `PlainFiles`

CLI validation should be table-driven:

```rust
pub struct PlatformCapabilities {
    pub default_projection: ProjectionMode,
    pub supported_projections: Vec<ProjectionMode>,
    pub supports_daemon_service: bool,
    pub supports_secure_os_credentials: bool,
}
```

This removes the need for scattered `cfg(target_os)` logic in command parsing.

## Desktop Packaging

The desktop app should use the same platform layer as the CLI:

- discover sidecar binaries through `locality-platform`;
- start/stop daemon through `DaemonProcessManager`;
- register virtual projection through `ProjectionRegistrar`;
- open mount roots through a platform `Opener`;
- install terminal command through a platform `CliInstaller`.

Packaging targets:

- macOS: signed/notarized DMG with File Provider extension.
- Linux: deb/rpm with `loc`, `localityd`, `locality-fuse`, systemd integration.
- Windows: signed MSIX/MSI/NSIS package with `loc.exe`, `localityd.exe`, Cloud Files
  helper/provider, icon resources, and uninstall cleanup.

The current desktop build needs Windows-specific assets and bundle config:

- `icons/icon.ico`;
- Windows bundle target;
- Windows `prepare-bundle` staging script;
- sidecar discovery that checks `.exe` names.

## Testing Strategy

### Always-On CI

Run on every PR:

- Linux: full workspace including FUSE plus FUSE smoke when `/dev/fuse` exists.
- macOS: full workspace plus File Provider compile/package checks.
- Windows: all non-FUSE crates, desktop Rust compile once Windows assets exist,
  and path/credential/daemon transport tests.

Windows CI command after gating FUSE:

```text
cargo test -p locality-core -p locality-connector -p locality-store -p locality-notion -p localityd -p loc-cli --all-targets
```

### Platform Contract Tests

Add reusable tests for every projection adapter using a fake daemon:

- list root children;
- list nested page/database children;
- materialize online-only page on read;
- write hydrated page and mark dirty;
- create draft page;
- rename pending draft;
- delete pending draft;
- block path traversal;
- survive daemon unavailable errors cleanly.

The same contract should run against:

- in-memory fake adapter;
- macOS helper in manual/signed environments;
- Linux FUSE smoke;
- Windows Cloud Files smoke.

Implemented baseline: `crates/loc-cli/tests/projection_contract.rs` runs the
shared daemon virtual-filesystem contract for `macos-file-provider`,
`linux-fuse`, and `windows-cloud-files` projection modes below the kernel
adapters. It covers metadata-only browse, nested child refresh, hydration,
provider write/dirty transition, create, rename, and delete semantics. Real
kernel/provider smoke tests remain platform-specific layers above that shared
contract.

### Path Contract Tests

Add explicit path tests for:

- logical path render uses `/` on all platforms;
- host path render uses native separators;
- JSON reports are stable;
- command suggestions are copy/pasteable in PowerShell, cmd, bash, and zsh;
- CRLF Markdown frontmatter parses the same as LF.

## Migration Plan

### Phase 1: Windows CLI / Plain Files

Goal: Windows users can connect, mount plain files, pull, inspect, diff, push,
search, restore, and run tests without virtual projection.

Work:

- Gate or remove `locality-fuse` from Windows workspace builds.
- Add Windows host paths.
- Add Windows credential store.
- Add IPC transport abstraction and make daemon run on Windows over named pipe
  or TCP.
- Fix path rendering and CRLF tests.
- Add Windows CI for non-FUSE packages.

### Phase 2: Desktop and Service

Goal: Windows desktop app can install, start, stop, and manage Locality, using
plain-file mounts while Cloud Files is built.

Work:

- Add Windows Tauri bundle config and icon assets.
- Stage `loc.exe` and `localityd.exe`.
- Implement Windows daemon process manager.
- Implement terminal command install/update for Windows.
- Implement installer reset/uninstall cleanup.

### Phase 3: Windows Cloud Files Projection

Goal: Windows has native online-only mounts with Explorer integration.

Work:

- Add `WindowsCloudFiles` projection mode.
- Build provider/helper under `platform/windows`.
- Implement sync-root registration.
- Implement placeholder enumeration from daemon metadata.
- Implement hydration callback through daemon `materialize`.
- Implement write/create/rename/delete callback mapping to virtual mutations.
- Wire CLI lifecycle management and diagnostics for the Windows provider
  runtime.
- Wire desktop lifecycle supervision through the same provider lifecycle layer.
- Add Windows Cloud Files smoke tests on a suitable runner.

### Phase 4: Cross-Platform Product Hardening

Goal: macOS, Linux, and Windows share one conceptual product with platform-native
install and recovery paths.

Work:

- Unified diagnostics: implemented as top-level `loc doctor`, a read-only
  command that inspects daemon state, SQLite compatibility, connections,
  credentials, mounts, and platform provider lifecycle without mutating local
  state.
- Unified reset: out of scope for this phase of implementation; keep existing
  explicit reset surfaces until a cross-platform reset contract is designed.
- Shared projection contract test suite: implemented for the daemon virtual
  filesystem contract and wired into local e2e CI.
- Signed release artifacts for all platforms: macOS and Linux release workflows
  already exist; Windows now has a signed NSIS release workflow and updater
  manifest path.
- Documentation split by platform, with one shared model section: see
  `docs/platforms.md`, `docs/macos-distribution.md`,
  `docs/linux-distribution.md`, and `docs/windows-distribution.md`.

## Non-Goals

- Do not put platform-specific code in `locality-core`.
- Do not make connectors know which OS is running.
- Do not let projection adapters bypass daemon state ownership.
- Do not make Windows depend on Linux FUSE semantics.
- Do not treat slash/backslash differences as cosmetic; path rendering is part
  of the product API for agents.

## Immediate Codebase Implications

Current Windows research found these concrete issues:

- `localityd` and `loc-cli` compile on Windows, but daemon IPC and foreground serving
  are Unix-gated.
- `locality-fuse` fails on Windows through its `fuse3` dependency and should become
  Linux-only.
- Tauri Windows build needs `icon.ico` and Windows bundle configuration.
- Tests expose inconsistent logical path rendering and CRLF parsing.
- Desktop and CLI still use `HOME` in several state-root and install paths.

Those are not reasons to fork the architecture. They are signs that the existing
platform boundary needs to become explicit.
