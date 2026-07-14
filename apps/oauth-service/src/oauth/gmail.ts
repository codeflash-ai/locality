import { upstreamError } from "../http/errors";
import type { BrokerEnv } from "../types";
import { googleClientId, googleClientSecret } from "./google";

export const GMAIL_OAUTH_SCOPES = [
  "openid",
  "email",
  "profile",
  "https://www.googleapis.com/auth/gmail.readonly",
  "https://www.googleapis.com/auth/gmail.compose"
];

export interface GmailTokenResponse {
  access_token: string;
  token_type?: string;
  refresh_token?: string;
  expires_in?: number;
  scope?: string;
  id_token?: string;
}

export function gmailAuthorizeUrl(env: BrokerEnv, redirectUri: string, state: string): string {
  const url = new URL(`${gmailAuthBaseUrl(env)}/o/oauth2/v2/auth`);
  url.searchParams.set("client_id", googleClientId(env));
  url.searchParams.set("response_type", "code");
  url.searchParams.set("redirect_uri", redirectUri);
  url.searchParams.set("scope", GMAIL_OAUTH_SCOPES.join(" "));
  url.searchParams.set("state", state);
  url.searchParams.set("access_type", "offline");
  url.searchParams.set("prompt", "consent");
  url.searchParams.set("include_granted_scopes", "true");
  return url.toString();
}

export async function exchangeGmailCode(
  env: BrokerEnv,
  code: string,
  redirectUri: string,
  fetcher: typeof fetch = fetch
): Promise<GmailTokenResponse> {
  return gmailTokenRequest(
    env,
    {
      grant_type: "authorization_code",
      code,
      redirect_uri: redirectUri
    },
    fetcher
  );
}

export async function refreshGmailToken(
  env: BrokerEnv,
  refreshToken: string,
  fetcher: typeof fetch = fetch
): Promise<GmailTokenResponse> {
  return gmailTokenRequest(
    env,
    {
      grant_type: "refresh_token",
      refresh_token: refreshToken
    },
    fetcher
  );
}

async function gmailTokenRequest(
  env: BrokerEnv,
  body: Record<string, string>,
  fetcher: typeof fetch
): Promise<GmailTokenResponse> {
  const clientId = googleClientId(env);
  const clientSecret = googleClientSecret(env);
  const params = new URLSearchParams({
    client_id: clientId,
    client_secret: clientSecret,
    ...body
  });
  const response = await fetcher(`${gmailApiBaseUrl(env)}/token`, {
    method: "POST",
    headers: {
      "Content-Type": "application/x-www-form-urlencoded"
    },
    body: params.toString()
  });
  if (!response.ok) {
    throw upstreamError(`Gmail OAuth returned HTTP ${response.status}`);
  }
  return response.json() as Promise<GmailTokenResponse>;
}

function gmailAuthBaseUrl(env: BrokerEnv): string {
  return (env.LOCALITY_GMAIL_AUTH_BASE_URL ?? "https://accounts.google.com").replace(/\/+$/, "");
}

function gmailApiBaseUrl(env: BrokerEnv): string {
  return (env.LOCALITY_GMAIL_API_BASE_URL ?? "https://oauth2.googleapis.com").replace(/\/+$/, "");
}
