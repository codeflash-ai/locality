import { configError, upstreamError } from "../http/errors";
import type { BrokerEnv } from "../types";

export const SLACK_OAUTH_SCOPES = [
  "channels:read",
  "channels:history",
  "groups:read",
  "groups:history",
  "im:read",
  "im:history",
  "mpim:read",
  "mpim:history",
  "users:read",
  "team:read",
  "files:read",
  "channels:join"
];

export interface SlackTokenResponse {
  ok: boolean;
  error?: string;
  access_token: string;
  token_type?: string;
  scope?: string;
  refresh_token?: string;
  expires_in?: number;
  bot_user_id?: string;
  team?: {
    id?: string;
    name?: string;
  };
  enterprise?: {
    id?: string;
    name?: string;
  };
}

export function slackAuthorizeUrl(env: BrokerEnv, redirectUri: string, state: string): string {
  const url = new URL(`${slackAuthBaseUrl(env)}/oauth/v2/authorize`);
  url.searchParams.set("client_id", slackClientId(env));
  url.searchParams.set("scope", SLACK_OAUTH_SCOPES.join(","));
  url.searchParams.set("redirect_uri", redirectUri);
  url.searchParams.set("state", state);
  return url.toString();
}

export async function exchangeSlackCode(
  env: BrokerEnv,
  code: string,
  redirectUri: string,
  fetcher: typeof fetch = fetch
): Promise<SlackTokenResponse> {
  return slackTokenRequest(
    env,
    {
      grant_type: "authorization_code",
      code,
      redirect_uri: redirectUri
    },
    fetcher
  );
}

export async function refreshSlackToken(
  env: BrokerEnv,
  refreshToken: string,
  fetcher: typeof fetch = fetch
): Promise<SlackTokenResponse> {
  return slackTokenRequest(
    env,
    {
      grant_type: "refresh_token",
      refresh_token: refreshToken
    },
    fetcher
  );
}

async function slackTokenRequest(
  env: BrokerEnv,
  body: Record<string, string>,
  fetcher: typeof fetch
): Promise<SlackTokenResponse> {
  const params = new URLSearchParams({
    client_id: slackClientId(env),
    client_secret: slackClientSecret(env),
    ...body
  });
  const response = await fetcher(`${slackApiBaseUrl(env)}/oauth.v2.access`, {
    method: "POST",
    headers: {
      "Content-Type": "application/x-www-form-urlencoded"
    },
    body: params.toString()
  });
  if (!response.ok) {
    throw upstreamError(`Slack OAuth returned HTTP ${response.status}`);
  }
  const token = (await response.json()) as SlackTokenResponse;
  if (!token.ok) {
    throw upstreamError("Slack OAuth failed");
  }
  return token;
}

function slackClientId(env: BrokerEnv): string {
  if (!env.LOCALITY_SLACK_CLIENT_ID) {
    throw configError("LOCALITY_SLACK_CLIENT_ID must be configured");
  }
  return env.LOCALITY_SLACK_CLIENT_ID;
}

function slackClientSecret(env: BrokerEnv): string {
  if (!env.LOCALITY_SLACK_CLIENT_SECRET) {
    throw configError("LOCALITY_SLACK_CLIENT_SECRET must be configured");
  }
  return env.LOCALITY_SLACK_CLIENT_SECRET;
}

function slackAuthBaseUrl(env: BrokerEnv): string {
  return (env.LOCALITY_SLACK_AUTH_BASE_URL ?? "https://slack.com").replace(/\/+$/, "");
}

function slackApiBaseUrl(env: BrokerEnv): string {
  return (env.LOCALITY_SLACK_API_BASE_URL ?? "https://slack.com/api").replace(/\/+$/, "");
}
