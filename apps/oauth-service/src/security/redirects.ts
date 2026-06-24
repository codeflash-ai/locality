import { badRequest } from "../http/errors";
import type { BrokerEnv } from "../types";

const DEFAULT_NOTION_REDIRECT_URIS = [
  "http://localhost:8757/oauth/notion/callback",
  "http://127.0.0.1:8757/oauth/notion/callback"
];

export function allowedNotionRedirectUris(env: BrokerEnv): string[] {
  return splitList(env.LOCALITY_NOTION_REDIRECT_URIS) ?? DEFAULT_NOTION_REDIRECT_URIS;
}

export function validateNotionRedirectUri(env: BrokerEnv, redirectUri: string): string {
  let parsed: URL;
  try {
    parsed = new URL(redirectUri);
  } catch {
    throw badRequest("invalid_redirect_uri", "redirect_uri must be a valid URL");
  }
  if (parsed.protocol !== "http:" || !["localhost", "127.0.0.1"].includes(parsed.hostname)) {
    throw badRequest("invalid_redirect_uri", "Notion redirect_uri must be a loopback HTTP URL");
  }
  const allowed = allowedNotionRedirectUris(env);
  if (!allowed.includes(redirectUri)) {
    throw badRequest("redirect_uri_not_allowed", "redirect_uri is not configured for this broker");
  }
  return redirectUri;
}

function splitList(value: string | undefined): string[] | undefined {
  const entries = value
    ?.split(",")
    .map((entry) => entry.trim())
    .filter(Boolean);
  return entries && entries.length > 0 ? entries : undefined;
}
