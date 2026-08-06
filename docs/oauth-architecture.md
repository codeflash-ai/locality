# OAuth Architecture

Locality has two OAuth hosts:

- Local desktop OAuth: `loc`, the public OAuth broker, localhost completion, and
  the local credential store.
- Hosted admin OAuth: `locality-internal`, admin intents, backend provider
  callbacks, Postgres finalization, and managed secret storage.

The shared layer is `locality-auth-core`. It owns connector IDs, callback paths,
and scope profiles. It does not own token storage, tenant authorization, hosted
source finalization, or background job scheduling.

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

## Hosted Flow

Hosted connector OAuth stays in `locality-internal`. It creates hash-only admin
intents, receives provider callbacks on the backend, writes credentials to
managed secret storage, and stores only opaque credential references in
Postgres.

Hosted connectors may consume `locality-auth-core` for IDs and scope profiles,
but hosted availability, tenant binding, finalization, grants, and jobs remain
private runtime responsibilities.
