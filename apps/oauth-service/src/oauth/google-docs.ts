import { upstreamError } from "../http/errors";
import type { BrokerEnv } from "../types";
import { googleClientId, googleClientSecret } from "./google";

export const GOOGLE_DOCS_OAUTH_SCOPES = [
  "openid",
  "email",
  "profile",
  "https://www.googleapis.com/auth/documents",
  "https://www.googleapis.com/auth/drive.file",
  "https://www.googleapis.com/auth/drive.metadata"
];

export interface GoogleDocsTokenResponse {
  access_token: string;
  token_type?: string;
  refresh_token?: string;
  expires_in?: number;
  scope?: string;
  id_token?: string;
}

export function googleDocsAuthorizeUrl(env: BrokerEnv, redirectUri: string, state: string): string {
  const url = new URL(`${googleDocsAuthBaseUrl(env)}/o/oauth2/v2/auth`);
  url.searchParams.set("client_id", googleClientId(env));
  url.searchParams.set("response_type", "code");
  url.searchParams.set("redirect_uri", redirectUri);
  url.searchParams.set("scope", GOOGLE_DOCS_OAUTH_SCOPES.join(" "));
  url.searchParams.set("state", state);
  url.searchParams.set("access_type", "offline");
  url.searchParams.set("prompt", "consent");
  url.searchParams.set("include_granted_scopes", "true");
  return url.toString();
}

export async function exchangeGoogleDocsCode(
  env: BrokerEnv,
  code: string,
  redirectUri: string,
  fetcher: typeof fetch = fetch
): Promise<GoogleDocsTokenResponse> {
  return googleDocsTokenRequest(
    env,
    {
      grant_type: "authorization_code",
      code,
      redirect_uri: redirectUri
    },
    fetcher
  );
}

export async function refreshGoogleDocsToken(
  env: BrokerEnv,
  refreshToken: string,
  fetcher: typeof fetch = fetch
): Promise<GoogleDocsTokenResponse> {
  return googleDocsTokenRequest(
    env,
    {
      grant_type: "refresh_token",
      refresh_token: refreshToken
    },
    fetcher
  );
}

async function googleDocsTokenRequest(
  env: BrokerEnv,
  body: Record<string, string>,
  fetcher: typeof fetch
): Promise<GoogleDocsTokenResponse> {
  const clientId = googleClientId(env);
  const clientSecret = googleClientSecret(env);
  const params = new URLSearchParams({
    client_id: clientId,
    client_secret: clientSecret,
    ...body
  });
  const response = await fetcher(`${googleDocsApiBaseUrl(env)}/token`, {
    method: "POST",
    headers: {
      "Content-Type": "application/x-www-form-urlencoded"
    },
    body: params.toString()
  });
  if (!response.ok) {
    throw upstreamError(`Google Docs OAuth returned HTTP ${response.status}`);
  }
  return response.json() as Promise<GoogleDocsTokenResponse>;
}

function googleDocsAuthBaseUrl(env: BrokerEnv): string {
  return (env.LOCALITY_GOOGLE_DOCS_AUTH_BASE_URL ?? "https://accounts.google.com").replace(/\/+$/, "");
}

function googleDocsApiBaseUrl(env: BrokerEnv): string {
  return (env.LOCALITY_GOOGLE_DOCS_API_BASE_URL ?? "https://oauth2.googleapis.com").replace(/\/+$/, "");
}
