# Locality

Locality mounts systems of record as real Markdown files that agents and editors can read, grep, and edit locally. Reads are implicit through the daemon. Writes are explicit by default through `loc push`, which validates, plans, journals, and applies changes back to the source with connector-specific APIs.

This repository contains the Rust workspace for the `plan.md` design and the first functional slices of the core sync engine, CLI, store, daemon hydration loop, and Notion connector.

## Install on macOS

Locality publishes an Apple Silicon macOS build through Homebrew:

```sh
brew tap codeflash-ai/tap
brew install --cask loc
```

To update an existing Homebrew install:

```sh
brew update
brew upgrade --cask loc
```

The public Homebrew build currently requires an Apple Silicon Mac running macOS 14 Sonoma or newer.

## Install on Linux

Linux packages are published through distro package repositories. On Debian or Ubuntu:

```sh
curl -fsSL https://codeflash-ai.github.io/locality/apt/codeflash-loc.asc | sudo gpg --dearmor -o /usr/share/keyrings/codeflash-loc.gpg && echo "deb [signed-by=/usr/share/keyrings/codeflash-loc.gpg] https://codeflash-ai.github.io/locality/apt stable main" | sudo tee /etc/apt/sources.list.d/loc.list >/dev/null && sudo apt update && sudo apt install loc
```

On Fedora, RHEL, or compatible distributions:

```sh
sudo curl -fsSL -o /etc/yum.repos.d/loc.repo https://codeflash-ai.github.io/locality/rpm/loc.repo && sudo dnf install loc
```

Linux packages require `fuse3` and `systemd`; the package metadata declares both dependencies.
APT/DNF installs update through the system package manager. For Tauri-managed
self-update, use the AppImage channel:

```sh
mkdir -p ~/.local/bin && curl -L -o ~/.local/bin/Locality.AppImage https://github.com/codeflash-ai/locality/releases/latest/download/Locality-release-linux-x86_64.AppImage && chmod +x ~/.local/bin/Locality.AppImage
```

## Development

The root `Makefile` is the easiest way to run common project tasks:

```sh
make help
```

For a fresh checkout, install the desktop dependencies once:

```sh
make setup
```

Common targets:

| Target | What it does |
| --- | --- |
| `make build` | Builds the Rust workspace and desktop frontend. |
| `make check` | Runs Rust `cargo check` plus the desktop TypeScript/Vite build. |
| `make test` | Runs the default Rust workspace test suite. |
| `make ci` | Runs the same checks as GitHub Actions: Rust formatting and workspace tests. |
| `make lint` | Runs Rust formatting checks and clippy with warnings denied. |
| `make fmt` | Formats all Rust code. |
| `make dev-desktop` | Starts the desktop Vite dev server at `http://127.0.0.1:1420/`. |
| `make dev-tauri` | Builds fresh debug desktop sidecars, then starts the Tauri desktop app in development mode. |
| `make build-tauri` | Builds the packaged Tauri desktop app. |
| `make install-macos-file-provider` | Installs/registers the local macOS File Provider development bundle. |
| `make run-cli ARGS='status --json'` | Runs the `loc` CLI with custom arguments. |
| `make clean` | Removes Rust and desktop build outputs. |

## Workspace layout

- `crates/loc-cli`: `loc` command surface for humans and agents.
- `crates/localityd`: per-user daemon supervising mounts, virtual filesystem projection requests, watchers, hydration, pull, and push orchestration.
- `platform/linux/locality-fuse`: Linux FUSE projection helper for `linux_fuse` mounts.
- `crates/locality-core`: connector-agnostic sync engine, three-tree model, diff, planning, conflicts, hydration state, validation, and journal abstractions.
- `crates/locality-connector`: connector SDK trait for enumerate, fetch, render, parse, and apply.
- `crates/locality-notion`: first-party Notion connector with live page/block reads, database row projection, schema rendering, narrow block writes, and supported page-property writes.
- `crates/locality-store`: state-store abstraction and SQLite implementation.
- `templates/mount/AGENTS.md`: generated mount guidance template for coding agents.
- `docs/`: design notes split by implementation surface.

## Current status

The implementation is still early, but the main module boundaries are now exercised end to end: mount writes concise agent guidance, mount and pull can project a Notion root page into files, database rows appear as page stubs with property frontmatter, selected pages can hydrate, info explains local source context, status reports local dirty/stub/conflict state, simple block and supported property edits can push with journaling and reconciliation, daemon hydration requests and virtual filesystem metadata/write requests have tested execution paths, Linux FUSE has an initial mount helper, and `loc daemon start|stop|status` can run `localityd` as a user-managed background process.
