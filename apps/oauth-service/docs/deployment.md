# Deployment

## Recommended: Cloudflare Workers

The broker is a small stateless HTTP service. Cloudflare Workers is the best
initial fit because it runs TypeScript directly, supports environment secrets,
and does not require operating a server.

Production setup:

```sh
npm install
wrangler secret put LOCALITY_BROKER_SESSION_SECRET
wrangler secret put LOCALITY_REFRESH_HANDLE_KEY
wrangler secret put LOCALITY_NOTION_CLIENT_ID
wrangler secret put LOCALITY_NOTION_CLIENT_SECRET
wrangler secret put LOCALITY_GOOGLE_CLIENT_ID
wrangler secret put LOCALITY_GOOGLE_CLIENT_SECRET
wrangler secret put LOCALITY_SLACK_CLIENT_ID
wrangler secret put LOCALITY_SLACK_CLIENT_SECRET
```

Register the exact hosted broker callbacks with each provider before deploying
matching `LOCALITY_<CONNECTOR>_HOSTED_CALLBACK_URI` values:

```text
https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback
https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/google-docs/callback
https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/google-calendar/callback
https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/gmail/callback
https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/slack/callback
```

The Google OAuth client must include the Google Docs, Google Calendar, and Gmail
hosted callback URIs. Slack must include the Slack hosted callback URI. Notion
must include the Notion hosted callback URI. Each hosted variable must exactly
match the reachable Worker callback URL registered with the provider. That can
be the `workers.dev` URL above or a custom domain such as
`https://auth.locality.dev/v1/oauth/notion/callback`.

Keep the local loopback callbacks in the broker allowlists for the CLI handoff:

```text
http://localhost:8757/oauth/notion/callback
http://127.0.0.1:8757/oauth/notion/callback
http://localhost:8757/oauth/google-docs/callback
http://127.0.0.1:8757/oauth/google-docs/callback
http://localhost:8757/oauth/google-calendar/callback
http://127.0.0.1:8757/oauth/google-calendar/callback
http://localhost:8757/oauth/gmail/callback
http://127.0.0.1:8757/oauth/gmail/callback
http://localhost:8757/oauth/slack/callback
http://127.0.0.1:8757/oauth/slack/callback
```

Deploy after secrets and provider callback URLs are in place:

```sh
wrangler deploy
```

Use a stable production URL such as:

```text
https://auth.locality.dev
```

The Locality client should have:

```text
LOCALITY_AUTH_BROKER_URL=https://auth.locality.dev
LOCALITY_NOTION_OAUTH_CLIENT_ID=<public client id>
```

The client ID may also be fetched from `/v1/oauth/notion/start`,
`/v1/oauth/google-docs/start`, `/v1/oauth/google-calendar/start`, or
`/v1/oauth/gmail/start`, or `/v1/oauth/slack/start`; keeping it in the Locality
binary is fine because it is not confidential. The three Google start endpoints
return the same shared Google OAuth client ID.

Optional broker environment overrides for connector local testing:

```text
LOCALITY_GOOGLE_CALENDAR_REDIRECT_URIS=http://localhost:8757/oauth/google-calendar/callback,http://127.0.0.1:8757/oauth/google-calendar/callback
LOCALITY_GOOGLE_CALENDAR_HOSTED_CALLBACK_URI=https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/google-calendar/callback
LOCALITY_GOOGLE_CALENDAR_AUTH_BASE_URL=https://accounts.google.com
LOCALITY_GOOGLE_CALENDAR_API_BASE_URL=https://oauth2.googleapis.com
LOCALITY_GMAIL_REDIRECT_URIS=http://localhost:8757/oauth/gmail/callback,http://127.0.0.1:8757/oauth/gmail/callback
LOCALITY_GMAIL_HOSTED_CALLBACK_URI=https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/gmail/callback
LOCALITY_GMAIL_AUTH_BASE_URL=https://accounts.google.com
LOCALITY_GMAIL_API_BASE_URL=https://oauth2.googleapis.com
LOCALITY_SLACK_REDIRECT_URIS=http://localhost:8757/oauth/slack/callback,http://127.0.0.1:8757/oauth/slack/callback
LOCALITY_SLACK_HOSTED_CALLBACK_URI=https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/slack/callback
LOCALITY_SLACK_AUTH_BASE_URL=https://slack.com
LOCALITY_SLACK_API_BASE_URL=https://slack.com/api
```

## GitHub Actions CD

Cloudflare Workers does not use a Vercel/Mintlify-style GitHub App for this
monorepo path. Deployments run through
[`.github/workflows/oauth-service-deploy.yml`](../../../.github/workflows/oauth-service-deploy.yml):

- triggers on pushes to `main` that touch `apps/oauth-service/**`
- also supports manual `workflow_dispatch`
- runs `npm run check`, then `wrangler deploy` via `cloudflare/wrangler-action`

One-time GitHub setup:

1. Create a Cloudflare API token with **Edit Cloudflare Workers** scope for the
   target account.
2. Add repository or environment secrets:
   - `CLOUDFLARE_API_TOKEN`
   - `CLOUDFLARE_ACCOUNT_ID`
3. Create the GitHub Environment named `oauth-broker` (optional protection
   rules / required reviewers are recommended for production).

Provider OAuth secrets stay in Cloudflare Workers secrets via
`wrangler secret put`. Do not copy Notion/Google client secrets into GitHub
Actions unless a workflow is explicitly managing secret rotation.

Until those GitHub secrets exist, the deploy job will fail at authentication
even if checks pass. Manual `wrangler deploy` from a machine that already has
Worker secrets configured remains valid for bootstrap and recovery.

## Alternatives

Cloudflare Workers Builds (dashboard Git integration) can also deploy from
GitHub, but for this monorepo the Actions workflow is simpler because the Worker
root is `apps/oauth-service/`.

Vercel Functions are also viable and have a simple GitHub integration. Choose
Vercel if Locality already has a Vercel web property and we want one hosting surface.

Fly.io is a better fit if this grows into a long-running relay, needs regional
process control, or starts using local stateful services.
