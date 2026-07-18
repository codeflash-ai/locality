import { configError, upstreamError } from "../http/errors";
import type { BrokerEnv } from "../types";

export const SLACK_OAUTH_SCOPES = ["channels:read", "channels:history", "users:read"];

export interface SlackTokenResponse {
  ok?: boolean;
  error?: string;
  access_token: string;
  token_type?: string;
  refresh_token?: string;
  expires_in?: number;
  scope?: string;
  bot_user_id?: string;
  team?: {
    id?: string;
    name?: string;
  };
  enterprise?: {
    id?: string;
    name?: string;
  };
  authed_user?: {
    id?: string;
    scope?: string;
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

export function slackTokenScopes(token: SlackTokenResponse): string[] {
  const scopes = new Set<string>();
  addSlackScopes(scopes, token.scope);
  addSlackScopes(scopes, token.authed_user?.scope);
  return Array.from(scopes).sort();
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
  const response = await fetcher(`${slackApiBaseUrl(env)}/api/oauth.v2.access`, {
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
  if (token.ok === false) {
    throw upstreamError(`Slack OAuth returned error ${token.error ?? "unknown_error"}`);
  }
  return token;
}

function addSlackScopes(scopes: Set<string>, value: string | undefined) {
  for (const scope of value?.split(/[,\s]+/) ?? []) {
    if (scope.trim()) {
      scopes.add(scope.trim());
    }
  }
}

function slackClientId(env: BrokerEnv): string {
  return requireEnv(env.LOCALITY_SLACK_CLIENT_ID, "LOCALITY_SLACK_CLIENT_ID");
}

function slackClientSecret(env: BrokerEnv): string {
  return requireEnv(env.LOCALITY_SLACK_CLIENT_SECRET, "LOCALITY_SLACK_CLIENT_SECRET");
}

function slackAuthBaseUrl(env: BrokerEnv): string {
  return (env.LOCALITY_SLACK_AUTH_BASE_URL ?? "https://slack.com").replace(/\/+$/, "");
}

function slackApiBaseUrl(env: BrokerEnv): string {
  return (env.LOCALITY_SLACK_API_BASE_URL ?? "https://slack.com").replace(/\/+$/, "");
}

function requireEnv(value: string | undefined, name: string): string {
  if (!value) {
    throw configError(`${name} is required`);
  }
  return value;
}
