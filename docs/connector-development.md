# Connector Development

Locality first-party connectors are compiled Rust crates. The public connector
manifest is language-neutral discovery metadata; it is not a plugin ABI and it
does not grant credential, network, filesystem, or push authority. Trusted host
code remains responsible for auth resolution, write policy, validation,
concurrency checks, and operation execution.

## Add-a-connector checklist

Complete every item in one change. CI compares these surfaces so a connector
crate cannot ship by itself.

1. Add `crates/locality-<id>` to the workspace. Use a stable lowercase
   kebab-case connector ID and depend on `locality-connector`.
2. Implement `Connector::kind`, `capabilities`,
   `supported_push_operations`, enumeration, fetch, render, and only the write
   methods the provider actually supports. Do not advertise portable or batch
   behavior until its full path is reachable and tested.
3. Add the connector to `connectors/registry.json` and keep it valid against
   `connectors/registry.schema.json`. Record the exact runtime ID/version,
   default profile and connection IDs, auth kinds/scopes/actions, mount
   defaults/settings schema, descriptive capabilities/operations, projection
   policy, crate path, icon, and docs slug.
4. Add the connection/profile creation path. Credentials go into the credential
   store behind a `secret_ref`; never place credentials, executable commands,
   broker sessions, or bearer values in the manifest or mount settings.
5. Add one `SourceRegistration` in `crates/localityd/src/source.rs`, paired to
   the manifest ID. Add the daemon resolver, source descriptor, frontmatter
   validators, read/write/create/move decisions, hydration adapter, and
   reconciliation hooks that apply to the source.
6. Add CLI connect and mount routing without changing existing commands. Keep
   provider-specific mount settings in the provider crate and serialize the
   default represented by the manifest.
7. Add the desktop source ID, setup/auth classification, display metadata, and
   `apps/desktop/src/assets/connectors/<id>.svg` icon. Add OAuth-service routing
   only when the connector actually uses the hosted OAuth broker.
8. Add `docs/<id>-connector.md`, public
   `docs-site/connectors/<docs_slug>.mdx`, docs navigation, README support, and
   any provider-specific security or live-test instructions.
9. Add the direct fixture layout below and use
   `locality_connector::conformance` for identity, capability/operation, safe
   path, read-only, redaction, and fixture checks.
10. Run the contract, provider, daemon, CLI, docs, formatting, and workspace
    commands listed below. Verify live behavior only with a dedicated scratch
    account and explicit live-test credentials.

## Required direct fixture layout

New provider crates must start with this credential-free layout:

```text
crates/locality-<id>/fixtures/direct-v1/
  .gitattributes
  tree-paths.txt
  native-<case>.json
  <case>.md
  settings-default.json
  auth-scopes.json       # OAuth connectors
  auth-kind.txt          # token/API-key connectors
```

`tree-paths.txt` is the canonical ordered projection. Each
`native-<case>.json` has a matching exact rendered Markdown fixture. Settings
must contain no credentials. OAuth scope fixtures contain scope names only;
token/API-key fixtures contain only the auth-kind enum. Add more versioned
directories instead of silently changing an incompatible fixture contract.

Use `FixtureLayout` and `check_fixture_layout` to enforce the files relevant to
the connector. Existing connectors can adopt this layout incrementally; the
Slack direct-v1 fixtures are the first applied example.

## Host hooks and boundaries

The provider crate owns API DTOs/client behavior, quota/retry classification,
native fetch, canonical rendering/parsing, and provider operation lowering.
`locality-connector` owns reusable protocol, manifest, network, and conformance
types. The daemon owns runtime registration, credential resolution, source
descriptors, scheduling, path-level write decisions, hydration, and reconcile.
The CLI and desktop own setup presentation; they do not bypass daemon or
connector checks.

The manifest describes those surfaces for discovery and drift testing. Hosts
must not generate security behavior from JSON. In particular:

- a listed action or operation never authorizes a remote call;
- a writable mount still passes code-owned path policy, parsing, validation,
  guardrails, approval, concurrency preflight, and connector apply;
- read-only connectors reject edit, create, move, delete, push, undo, and
  autosave paths in trusted code even if a mount record is malformed;
- credentials and refresh handles stay behind `secret_ref` and every auth or
  client `Debug` implementation redacts them;
- docs/icon identifiers are safe relative identifiers resolved below fixed
  repository roots, never arbitrary paths or URLs.

## Direct and hosted implementations

Direct connectors, portable connector/projection contracts, clients, and the
manifest remain in this public repository. Direct mode resolves a local
connection and calls the provider from the Locality host.

Hosted service orchestration, PostgreSQL persistence, AWS integration, and
OpenTofu stay in the private repository. A hosted adapter may consume an exact
public connector revision, but the public crates must not depend on private
hosted code. Do not use this manifest branch to enable currently unreachable
portable/batch paths or introduce a dynamic ABI/plugin loader.

## Minimal read-only example

The connector advertises no push operations and fails closed on apply. Real
implementations still provide enumerate/fetch/render methods omitted here for
brevity:

```rust
use std::collections::BTreeSet;
use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, Connector, ConnectorCapabilities,
    ConnectorKind,
};
use locality_core::{LocalityError, LocalityResult};
use locality_core::planner::PushOperationKind;

struct ExampleConnector;

impl Connector for ExampleConnector {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind("example")
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities::read_only()
    }

    fn supported_push_operations(&self) -> BTreeSet<PushOperationKind> {
        BTreeSet::new()
    }

    // enumerate, fetch, render, and parse omitted

    fn apply(&self, _: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        Err(LocalityError::Unsupported("example connector is read-only"))
    }
}
```

Its manifest uses `"read_only": true`, read-only or empty profile actions, and
an empty `push_operations` array. The daemon must also return read-only
decisions for write, create, and move paths. Conformance tests then compare
`kind()`, capabilities, operations, descriptor defaults, and host rejection to
the manifest without making a provider request.

## Test commands

```sh
cargo fmt --all -- --check
cargo test -p locality-connector --all-targets
cargo test -p localityd --test connector_manifest
cargo test -p localityd --test source_descriptor
cargo test -p locality-<id> --all-targets
cargo test -p loc-cli --all-targets
cargo test --workspace --all-targets
jq empty connectors/registry.json connectors/registry.schema.json docs-site/docs.json
make docs-validate
make docs-broken-links
```

Run ignored live tests separately and never make them a prerequisite for the
credential-free conformance suite.
