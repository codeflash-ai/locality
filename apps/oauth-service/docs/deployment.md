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
wrangler deploy
```

Configure the Notion OAuth integration with the exact localhost callback used by
Locality:

```text
http://localhost:8757/oauth/notion/callback
http://127.0.0.1:8757/oauth/notion/callback
```

Configure one Google OAuth client with the exact localhost callbacks used by
Locality for both Google Docs and Gmail:

```text
http://localhost:8757/oauth/google-docs/callback
http://127.0.0.1:8757/oauth/google-docs/callback
http://localhost:8757/oauth/gmail/callback
http://127.0.0.1:8757/oauth/gmail/callback
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
`/v1/oauth/google-docs/start`, or `/v1/oauth/gmail/start`; keeping it in the
Locality binary is fine because it is not confidential. The two Google start
endpoints return the same shared Google OAuth client ID.

Optional broker environment overrides for Gmail local testing:

```text
LOCALITY_GMAIL_REDIRECT_URIS=http://localhost:8757/oauth/gmail/callback,http://127.0.0.1:8757/oauth/gmail/callback
LOCALITY_GMAIL_AUTH_BASE_URL=https://accounts.google.com
LOCALITY_GMAIL_API_BASE_URL=https://oauth2.googleapis.com
```

## GitHub Actions

Use Cloudflare's deploy action with a Cloudflare API token stored as a GitHub
secret. Provider OAuth secrets should stay in Cloudflare Workers secrets, not in
GitHub Actions secrets, unless the deployment workflow explicitly manages them.

## Alternatives

Vercel Functions are also viable and have a simple GitHub integration. Choose
Vercel if Locality already has a Vercel web property and we want one hosting surface.

Fly.io is a better fit if this grows into a long-running relay, needs regional
process control, or starts using local stateful services.
