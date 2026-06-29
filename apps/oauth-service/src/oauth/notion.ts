import { configError, upstreamError } from "../http/errors";
import type { BrokerEnv } from "../types";

export interface NotionTokenResponse {
  access_token: string;
  token_type?: string;
  refresh_token?: string;
  expires_in?: number;
  bot_id?: string;
  workspace_id?: string;
  workspace_name?: string;
  workspace_icon?: string;
  owner?: unknown;
  duplicated_template_id?: string;
}

export function notionAuthorizeUrl(env: BrokerEnv, redirectUri: string, state: string): string {
  const url = new URL(`${notionAuthBaseUrl(env)}/v1/oauth/authorize`);
  url.searchParams.set("client_id", requireEnv(env.LOCALITY_NOTION_CLIENT_ID, "LOCALITY_NOTION_CLIENT_ID"));
  url.searchParams.set("response_type", "code");
  url.searchParams.set("owner", "user");
  url.searchParams.set("redirect_uri", redirectUri);
  url.searchParams.set("state", state);
  return url.toString();
}

export async function exchangeNotionCode(
  env: BrokerEnv,
  code: string,
  redirectUri: string,
  fetcher: typeof fetch = fetch
): Promise<NotionTokenResponse> {
  return notionTokenRequest(
    env,
    {
      grant_type: "authorization_code",
      code,
      redirect_uri: redirectUri
    },
    fetcher
  );
}

export async function refreshNotionToken(
  env: BrokerEnv,
  refreshToken: string,
  fetcher: typeof fetch = fetch
): Promise<NotionTokenResponse> {
  return notionTokenRequest(
    env,
    {
      grant_type: "refresh_token",
      refresh_token: refreshToken
    },
    fetcher
  );
}

async function notionTokenRequest(
  env: BrokerEnv,
  body: Record<string, string>,
  fetcher: typeof fetch
): Promise<NotionTokenResponse> {
  const clientId = requireEnv(env.LOCALITY_NOTION_CLIENT_ID, "LOCALITY_NOTION_CLIENT_ID");
  const clientSecret = requireEnv(env.LOCALITY_NOTION_CLIENT_SECRET, "LOCALITY_NOTION_CLIENT_SECRET");
  const response = await fetcher(`${notionApiBaseUrl(env)}/v1/oauth/token`, {
    method: "POST",
    headers: {
      Authorization: `Basic ${btoa(`${clientId}:${clientSecret}`)}`,
      "Content-Type": "application/json",
      "Notion-Version": env.LOCALITY_NOTION_VERSION ?? "2022-06-28"
    },
    body: JSON.stringify(body)
  });
  if (!response.ok) {
    throw upstreamError(`Notion OAuth returned HTTP ${response.status}`);
  }
  return response.json() as Promise<NotionTokenResponse>;
}

function notionAuthBaseUrl(env: BrokerEnv): string {
  return (env.LOCALITY_NOTION_AUTH_BASE_URL ?? "https://api.notion.com").replace(/\/+$/, "");
}

function notionApiBaseUrl(env: BrokerEnv): string {
  return (env.LOCALITY_NOTION_API_BASE_URL ?? "https://api.notion.com").replace(/\/+$/, "");
}

function requireEnv(value: string | undefined, name: string): string {
  if (!value) {
    throw configError(`${name} is required`);
  }
  return value;
}
