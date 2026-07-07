# Clean-Start Fresh Install Reset Design

## Problem

`scripts/clean-start.sh` and `docs/daemon.md` describe `clean-start` as a way to
return a macOS machine to a clean Locality install state for manual testing.
That promise is currently incomplete.

Today the script:

- stops Locality processes and agents;
- resets File Provider domains through `locality-file-providerctl` when
  available;
- removes `~/.loc`, mount roots, support files, and keychain credentials; and
- deletes `/Applications/Locality.app`.

Today the script does not fully remove Locality-owned install residue that can
make the next install behave like a reinstall instead of a first install:

- the development install path `~/Applications/Locality.app` is not removed by
  default;
- Locality-specific File Provider persistence under
  `~/Library/Application Support/FileProvider/ai.codeflash.locality.Locality.FileProvider`
  is not removed; and
- fallback CloudStorage cleanup is too narrow when `state.sqlite3` is already
  gone, so legacy roots such as `~/Library/CloudStorage/Locality-Locality` can
  survive.

Those leftovers matter because macOS File Provider approval and registration are
not derived only from the app bundle. If Locality-owned File Provider state
survives, a reinstall can skip the expected first-install approval path and
produce misleading onboarding results.

## Goal

After `scripts/clean-start.sh --yes`, the machine should behave like a
first-time Locality install for Locality-owned artifacts.

Concretely, the next Locality install should not inherit:

- a previously installed Locality app bundle in either standard install
  location;
- Locality-specific File Provider domain persistence;
- Locality CloudStorage roots or other safe Locality mount roots;
- Locality launch agents, support files, caches, or `~/.loc` state; or
- Locality connection credentials, unless `--keep-credentials` is set.

## Non-Goals

- Do not reinstall Locality or leave the system in a
  `registered but not enabled` state.
- Do not remove non-Locality File Provider state, generic macOS caches, or
  unrelated extensions.
- Do not change desktop onboarding logic directly.
- Do not promise a byte-for-byte pristine macOS account; the contract is
  scoped to Locality-owned artifacts that affect first-install behavior.
- Do not preserve old `--app-path` override behavior if that would leave the
  machine looking previously installed.

## Decision

Expand `scripts/clean-start.sh` itself to become the canonical
fresh-install-style reset for Locality.

This keeps one operator command with one meaning instead of introducing a
second, stronger reset path.

### App Bundle Targeting

`clean-start` should always consider both standard Locality app bundle
locations:

- `/Applications/Locality.app`
- `~/Applications/Locality.app`

`--app-path PATH` should add an extra Locality app bundle target instead of
replacing the standard paths. The command's contract is to remove all standard
Locality install locations plus any explicitly supplied extra install path.

## Cleanup Contract

`clean-start` should keep its current top-level sequence, but broaden the
artifact set it removes:

1. Stop Locality processes and launch agents.
2. Reset Locality File Provider domains through `locality-file-providerctl`
   when available.
3. Remove safe Locality mount roots.
4. Remove Locality connection credentials unless `--keep-credentials` is set.
5. Unregister and delete Locality app bundles from all targeted install paths.
6. Remove Locality support files, caches, containers, and Locality-specific
   File Provider persistence.

This ordering matters. File Provider domain reset and process shutdown must
happen before deleting app bundles or File Provider persistence so the system
does not immediately recreate Locality-owned files during cleanup.

## Mount Root Cleanup

The fallback mount-root cleanup should cover the Locality roots that can remain
after state is already missing.

Required behavior:

- keep reading explicit mount roots from `state.sqlite3` when present;
- continue to allow only safe, Locality-owned roots to be removed;
- broaden the hardcoded/fallback CloudStorage targets to cover both
  `~/Library/CloudStorage/Locality` and `~/Library/CloudStorage/Locality-*`,
  including legacy aliases such as `Locality-Locality`; and
- keep existing safe temp and Documents roots.

The safety boundary remains path-based. `clean-start` should never remove
arbitrary `~/Library/CloudStorage` folders that do not match Locality's naming
scheme.

## App Bundle Cleanup

For every targeted Locality app bundle path that exists:

- unregister the bundle from PlugInKit before deletion;
- unregister the bundle from LaunchServices when `lsregister` is available,
  because the development installer registers `~/Applications/Locality.app`
  there explicitly; and
- remove the bundle directory.

This cleanup should be per-path and deduplicated so the same app location is
not processed twice.

## File Provider Persistence Cleanup

`clean-start` should delete Locality-specific File Provider persistence at:

```text
~/Library/Application Support/FileProvider/ai.codeflash.locality.Locality.FileProvider
```

This path is Locality-owned extension state and is the primary residue that can
make the next install look previously approved or previously registered.

Important constraints:

- remove only the Locality-specific directory, not the whole
  `~/Library/Application Support/FileProvider` tree;
- do not edit `Domains.plist` in place as part of `clean-start`; full removal is
  the correct first-install reset behavior; and
- perform this cleanup after Locality processes stop and domains reset.

## Docs And CLI Help

Update `clean-start` user-facing descriptions so they match the stronger
contract.

Required updates:

- `scripts/clean-start.sh` usage text should describe the command as resetting
  Locality to a fresh-install testing state;
- `--app-path PATH` should be described as an additional installed app bundle to
  remove;
- `docs/daemon.md` should explicitly mention both standard app locations and the
  Locality-specific File Provider persistence directory; and
- the `Makefile` help text for `clean-start` should reflect
  fresh-install-style manual testing.

## Testing

Automated tests should stay narrow and CI-safe.

Recommended approach:

- extract or structure the cleanup-target enumeration so it can be tested
  without invoking macOS-only commands;
- add a focused shell regression test that verifies:
  - both standard app bundle paths are part of the cleanup target set;
  - `--app-path` adds an extra path instead of replacing the standard ones;
  - the Locality File Provider persistence directory is part of the support
    cleanup target set; and
  - fallback mount-root targets include the Locality CloudStorage alias pattern
    needed to remove `Locality-Locality`-style roots.

Manual macOS verification remains required for the system-owned parts of the
flow.

## Manual Verification

1. Install the Locality dev bundle to `~/Applications/Locality.app`.
2. If relevant for the release path being tested, also install a Locality app
   bundle to `/Applications/Locality.app`.
3. Register and open the Locality File Provider domain so Locality-specific
   File Provider persistence and CloudStorage roots exist.
4. Run `scripts/clean-start.sh --yes --keep-credentials`.
5. Confirm that neither `/Applications/Locality.app` nor
   `~/Applications/Locality.app` remains.
6. Confirm that
   `~/Library/Application Support/FileProvider/ai.codeflash.locality.Locality.FileProvider`
   is gone.
7. Confirm that surviving Locality CloudStorage roots such as
   `~/Library/CloudStorage/Locality` or `~/Library/CloudStorage/Locality-Locality`
   are gone.
8. Confirm that `pluginkit -m -v -i ai.codeflash.locality.Locality.FileProvider`
   no longer reports an installed Locality File Provider from a deleted app
   bundle.
9. Reinstall Locality and verify the next setup behaves like a first install for
   Locality, including requiring the normal approval path again.

## Risks

- Changing `--app-path` from override semantics to additive semantics is a small
  CLI contract change. The docs and help text must make that explicit.
- CloudStorage cleanup must remain tightly scoped to Locality-named roots to
  avoid broad filesystem deletion.
- If Locality processes or File Provider domains are still active during
  cleanup, macOS may recreate Locality-owned extension files after they are
  deleted. The reset order should prevent that.

## Compatibility

No SQLite schema or desktop IPC change is required. This is a macOS operator
script and documentation contract change.
