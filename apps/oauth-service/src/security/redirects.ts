import { badRequest, configError } from "../http/errors";
import type { BrokerEnv, ConnectorId } from "../types";

const DEFAULT_NOTION_REDIRECT_URIS = [
  "http://localhost:8757/oauth/notion/callback",
  "http://127.0.0.1:8757/oauth/notion/callback"
];

const DEFAULT_GOOGLE_DOCS_REDIRECT_URIS = [
  "http://localhost:8757/oauth/google-docs/callback",
  "http://127.0.0.1:8757/oauth/google-docs/callback"
];

const DEFAULT_GOOGLE_CALENDAR_REDIRECT_URIS = [
  "http://localhost:8757/oauth/google-calendar/callback",
  "http://127.0.0.1:8757/oauth/google-calendar/callback"
];

const DEFAULT_GMAIL_REDIRECT_URIS = [
  "http://localhost:8757/oauth/gmail/callback",
  "http://127.0.0.1:8757/oauth/gmail/callback"
];

const DEFAULT_SLACK_REDIRECT_URIS = [
  "http://localhost:8757/oauth/slack/callback",
  "http://127.0.0.1:8757/oauth/slack/callback"
];

export function allowedNotionRedirectUris(env: BrokerEnv): string[] {
  return splitList(env.LOCALITY_NOTION_REDIRECT_URIS) ?? DEFAULT_NOTION_REDIRECT_URIS;
}

export function validateNotionRedirectUri(env: BrokerEnv, redirectUri: string): string {
  return validateLoopbackRedirectUri("Notion", allowedNotionRedirectUris(env), redirectUri);
}

export function allowedGoogleDocsRedirectUris(env: BrokerEnv): string[] {
  return splitList(env.LOCALITY_GOOGLE_DOCS_REDIRECT_URIS) ?? DEFAULT_GOOGLE_DOCS_REDIRECT_URIS;
}

export function validateGoogleDocsRedirectUri(env: BrokerEnv, redirectUri: string): string {
  return validateLoopbackRedirectUri("Google Docs", allowedGoogleDocsRedirectUris(env), redirectUri);
}

export function allowedGoogleCalendarRedirectUris(env: BrokerEnv): string[] {
  return splitList(env.LOCALITY_GOOGLE_CALENDAR_REDIRECT_URIS) ?? DEFAULT_GOOGLE_CALENDAR_REDIRECT_URIS;
}

export function validateGoogleCalendarRedirectUri(env: BrokerEnv, redirectUri: string): string {
  return validateLoopbackRedirectUri("Google Calendar", allowedGoogleCalendarRedirectUris(env), redirectUri);
}

export function allowedGmailRedirectUris(env: BrokerEnv): string[] {
  return splitList(env.LOCALITY_GMAIL_REDIRECT_URIS) ?? DEFAULT_GMAIL_REDIRECT_URIS;
}

export function validateGmailRedirectUri(env: BrokerEnv, redirectUri: string): string {
  return validateLoopbackRedirectUri("Gmail", allowedGmailRedirectUris(env), redirectUri);
}

export function allowedSlackRedirectUris(env: BrokerEnv): string[] {
  return splitList(env.LOCALITY_SLACK_REDIRECT_URIS) ?? DEFAULT_SLACK_REDIRECT_URIS;
}

export function validateSlackRedirectUri(env: BrokerEnv, redirectUri: string): string {
  return validateLoopbackRedirectUri("Slack", allowedSlackRedirectUris(env), redirectUri);
}

export function providerCallbackUri(env: BrokerEnv, connector: ConnectorId): string {
  const base = env.LOCALITY_BROKER_PUBLIC_BASE_URL;
  if (!base || base.trim() === "") {
    throw configError("LOCALITY_BROKER_PUBLIC_BASE_URL must be configured");
  }

  let parsed: URL;
  try {
    parsed = new URL(base);
  } catch {
    throw configError("LOCALITY_BROKER_PUBLIC_BASE_URL must be a valid HTTPS URL");
  }

  if (parsed.protocol !== "https:") {
    throw configError("LOCALITY_BROKER_PUBLIC_BASE_URL must use HTTPS");
  }
  if (!parsed.hostname) {
    throw configError("LOCALITY_BROKER_PUBLIC_BASE_URL must include a hostname");
  }
  if (parsed.username || parsed.password) {
    throw configError("LOCALITY_BROKER_PUBLIC_BASE_URL must not include credentials");
  }
  if (parsed.search || parsed.hash) {
    throw configError("LOCALITY_BROKER_PUBLIC_BASE_URL must not include query or fragment");
  }

  parsed.pathname = `${parsed.pathname.replace(/\/+$/, "")}/v1/oauth/${connector}/callback`;
  return parsed.toString();
}

function validateLoopbackRedirectUri(connectorName: string, allowed: string[], redirectUri: string): string {
  let parsed: URL;
  try {
    parsed = new URL(redirectUri);
  } catch {
    throw badRequest("invalid_redirect_uri", "redirect_uri must be a valid URL");
  }
  if (parsed.protocol !== "http:" || !["localhost", "127.0.0.1"].includes(parsed.hostname)) {
    throw badRequest("invalid_redirect_uri", `${connectorName} redirect_uri must be a loopback HTTP URL`);
  }
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
