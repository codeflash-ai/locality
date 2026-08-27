# OAuth Architecture

Locality has two OAuth hosts:

- Local desktop OAuth: `loc`, the public OAuth broker, localhost completion, and
  the local credential store.
- Hosted admin OAuth: `locality-internal`, admin intents, backend provider
  callbacks, Postgres finalization, and managed secret storage.

The shared layer is `locality-auth-core`. It owns connector IDs, callback paths,
authority modes, and scope profiles. It does not own token storage, tenant
authorization, hosted source finalization, or background job scheduling.

## Public Authority Contract

`locality-auth-core` exposes the public connector authority vocabulary:
`local_direct` and `hosted_managed`. `local_direct` means the local Locality
host resolves credentials and calls the provider directly. `hosted_managed`
means a hosted/admin runtime owns managed credential and provider access.

Desktop-originated OAuth uses the `LocalBrokered` host mode and maps to
`local_direct`, even when the public OAuth broker helps complete the provider
authorization. Hosted/admin OAuth uses the `HostedAdmin` host mode and maps to
`hosted_managed`; it must not be silently treated as local direct authority.

## Public Brokered Desktop Flow

Provider applications redirect to the broker over HTTPS:

```text
provider -> https://<broker>/v1/oauth/<connector>/callback
```

The broker verifies signed state and redirects back to the localhost completion
URI:

```text
broker -> http://localhost:8757/oauth/<connector>/callback
```

`loc` then exchanges the code through the broker. The broker sends the provider
token request using the HTTPS provider callback URI, not the localhost
completion URI.

Slack access tokens use rotating, single-use refresh tokens. The public broker
coordinates each opaque Slack refresh handle with a Durable Object and briefly
persists the encrypted successful response. This makes client retries safe when
the provider accepted a refresh but the network dropped the response.

## Hosted Flow

Hosted connector OAuth stays in `locality-internal`. It creates hash-only admin
intents, receives provider callbacks on the backend, writes credentials to
managed secret storage, and stores only opaque credential references in
Postgres.

Hosted connectors may consume `locality-auth-core` for IDs and scope profiles,
but hosted availability, tenant binding, finalization, grants, and jobs remain
private runtime responsibilities.

## Backend Image Boundary

The public `apps/oauth-service` broker is the desktop/local-direct OAuth helper.
It completes provider authorization through localhost and must not be deployed
as the hosted credential authority for enterprise/admin sources.

Hosted/admin OAuth is a backend process mode in `locality-internal`. That mode
owns tenant-bound intents, provider callbacks, managed secret references,
Postgres finalization, and worker handoff. Keeping the boundary explicit lets
the public repo share connector identity and callback vocabulary without
shipping hosted tenant authorization, Secrets Manager policy, or admin
credential lifecycle code.
