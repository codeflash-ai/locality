# File Provider Onboarding Read-Only Validation Design

## Goal

Allow onboarding to recreate the `notion-main` mount when macOS already exposes
its File Provider mount-point directory with provider-managed read-only POSIX
permissions.

## Problem

The desktop mount path validator treats every existing mount path like an
ordinary local directory. macOS File Provider mount points can legitimately
appear as `dr-x------` even though Locality can manage their contents through
the File Provider protocol. The validator therefore returns `Selected folder is
read-only` before the onboarding command can recreate the missing mount record.
Finder can still show previously hydrated provider content while Locality's
durable state and daemon report no mounts, leaving onboarding stuck at `Folder
setup needs attention.`

## Design

Keep the existing generic path safety checks for ordinary local projections,
including rejection of paths inside Locality's state directory and rejection of
read-only existing folders or parents.

For the macOS File Provider projection:

1. Resolve and normalize the requested path as today.
2. Reject paths outside the registered Locality CloudStorage root as today.
3. Do not use the visible File Provider item's POSIX write bit as a setup
   prerequisite. The provider owns that directory and File Provider capability
   checks, registration, activation, and runtime startup remain the authority
   for whether it is usable.

The exception is limited to a path strictly below a recognized Locality File
Provider root. It does not weaken validation for plain-file mounts or arbitrary
read-only directories.

## Alternatives Considered

- Reset the File Provider domain before onboarding. This removes the immediate
  stale replica but is destructive recovery and can discard useful local state.
- Adopt any visible CloudStorage directory as a mount. The visible replica does
  not contain enough durable identity or connection state to make that safe.
- Recommended: make validation projection-aware. This fixes the incorrect
  assumption at its source while retaining existing provider activation checks.

## Testing

- Add a focused macOS regression test proving an existing read-only directory
  beneath the Locality File Provider root passes desktop mount validation.
- Keep or add coverage proving an ordinary read-only directory is still
  rejected by generic mount validation.
- Run the desktop Rust tests covering mount validation and onboarding reports,
  followed by the relevant desktop frontend tests and formatting checks.

## Scope

This change does not reset domains, migrate or reconstruct missing mount rows,
change hydration behavior, or automatically remove older installed File
Provider plug-ins. Once validation succeeds, the existing mount workflow
recreates state and activates the registered provider through the normal path.
