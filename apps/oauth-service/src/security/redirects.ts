import { badRequest } from "../http/errors";
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

export interface HostedConnectorRedirectConfig {
  displayName: string;
  hostedCallbackPath: string;
  allowedRedirectUris(env: BrokerEnv): string[];
  hostedCallbackValue(env: BrokerEnv): string | undefined;
}

const CONNECTOR_REDIRECT_CONFIGS: Record<ConnectorId, HostedConnectorRedirectConfig> = {
  notion: {
    displayName: "Notion",
    hostedCallbackPath: "/v1/oauth/notion/callback",
    allowedRedirectUris: (env) => splitList(env.LOCALITY_NOTION_REDIRECT_URIS) ?? DEFAULT_NOTION_REDIRECT_URIS,
    hostedCallbackValue: (env) => env.LOCALITY_NOTION_HOSTED_CALLBACK_URI
  },
  "google-docs": {
    displayName: "Google Docs",
    hostedCallbackPath: "/v1/oauth/google-docs/callback",
    allowedRedirectUris: (env) =>
      splitList(env.LOCALITY_GOOGLE_DOCS_REDIRECT_URIS) ?? DEFAULT_GOOGLE_DOCS_REDIRECT_URIS,
    hostedCallbackValue: (env) => env.LOCALITY_GOOGLE_DOCS_HOSTED_CALLBACK_URI
  },
  "google-calendar": {
    displayName: "Google Calendar",
    hostedCallbackPath: "/v1/oauth/google-calendar/callback",
    allowedRedirectUris: (env) =>
      splitList(env.LOCALITY_GOOGLE_CALENDAR_REDIRECT_URIS) ?? DEFAULT_GOOGLE_CALENDAR_REDIRECT_URIS,
    hostedCallbackValue: (env) => env.LOCALITY_GOOGLE_CALENDAR_HOSTED_CALLBACK_URI
  },
  gmail: {
    displayName: "Gmail",
    hostedCallbackPath: "/v1/oauth/gmail/callback",
    allowedRedirectUris: (env) => splitList(env.LOCALITY_GMAIL_REDIRECT_URIS) ?? DEFAULT_GMAIL_REDIRECT_URIS,
    hostedCallbackValue: (env) => env.LOCALITY_GMAIL_HOSTED_CALLBACK_URI
  },
  slack: {
    displayName: "Slack",
    hostedCallbackPath: "/v1/oauth/slack/callback",
    allowedRedirectUris: (env) => splitList(env.LOCALITY_SLACK_REDIRECT_URIS) ?? DEFAULT_SLACK_REDIRECT_URIS,
    hostedCallbackValue: (env) => env.LOCALITY_SLACK_HOSTED_CALLBACK_URI
  }
};

export function allowedNotionRedirectUris(env: BrokerEnv): string[] {
  return CONNECTOR_REDIRECT_CONFIGS.notion.allowedRedirectUris(env);
}

export function validateNotionRedirectUri(env: BrokerEnv, redirectUri: string): string {
  return validateConnectorRedirectUri(env, "notion", redirectUri);
}

export function hostedNotionCallbackUri(env: BrokerEnv): string | undefined {
  return hostedConnectorCallbackUri(env, "notion");
}

export function validateHostedNotionCallbackUri(callbackUri: string): string {
  return validateHostedConnectorCallbackUri(CONNECTOR_REDIRECT_CONFIGS.notion, callbackUri);
}

export function validateNotionExchangeRedirectUri(env: BrokerEnv, redirectUri: string): string {
  return validateConnectorExchangeRedirectUri(env, "notion", redirectUri);
}

export function allowedGoogleDocsRedirectUris(env: BrokerEnv): string[] {
  return CONNECTOR_REDIRECT_CONFIGS["google-docs"].allowedRedirectUris(env);
}

export function validateGoogleDocsRedirectUri(env: BrokerEnv, redirectUri: string): string {
  return validateConnectorRedirectUri(env, "google-docs", redirectUri);
}

export function validateGoogleDocsExchangeRedirectUri(env: BrokerEnv, redirectUri: string): string {
  return validateConnectorExchangeRedirectUri(env, "google-docs", redirectUri);
}

export function allowedGoogleCalendarRedirectUris(env: BrokerEnv): string[] {
  return CONNECTOR_REDIRECT_CONFIGS["google-calendar"].allowedRedirectUris(env);
}

export function validateGoogleCalendarRedirectUri(env: BrokerEnv, redirectUri: string): string {
  return validateConnectorRedirectUri(env, "google-calendar", redirectUri);
}

export function validateGoogleCalendarExchangeRedirectUri(env: BrokerEnv, redirectUri: string): string {
  return validateConnectorExchangeRedirectUri(env, "google-calendar", redirectUri);
}

export function allowedGmailRedirectUris(env: BrokerEnv): string[] {
  return CONNECTOR_REDIRECT_CONFIGS.gmail.allowedRedirectUris(env);
}

export function validateGmailRedirectUri(env: BrokerEnv, redirectUri: string): string {
  return validateConnectorRedirectUri(env, "gmail", redirectUri);
}

export function validateGmailExchangeRedirectUri(env: BrokerEnv, redirectUri: string): string {
  return validateConnectorExchangeRedirectUri(env, "gmail", redirectUri);
}

export function allowedSlackRedirectUris(env: BrokerEnv): string[] {
  return CONNECTOR_REDIRECT_CONFIGS.slack.allowedRedirectUris(env);
}

export function validateSlackRedirectUri(env: BrokerEnv, redirectUri: string): string {
  return validateConnectorRedirectUri(env, "slack", redirectUri);
}

export function validateSlackExchangeRedirectUri(env: BrokerEnv, redirectUri: string): string {
  return validateConnectorExchangeRedirectUri(env, "slack", redirectUri);
}

export function hostedConnectorCallbackUri(env: BrokerEnv, connector: ConnectorId): string | undefined {
  const config = CONNECTOR_REDIRECT_CONFIGS[connector];
  const value = config.hostedCallbackValue(env)?.trim();
  if (!value) {
    return undefined;
  }
  return validateHostedConnectorCallbackUri(config, value);
}

export function validateHostedConnectorCallbackUri(
  config: HostedConnectorRedirectConfig,
  callbackUri: string
): string {
  const hasExplicitPort = hasExplicitAuthorityPort(callbackUri);
  let parsed: URL;
  try {
    parsed = new URL(callbackUri);
  } catch {
    throw badRequest(
      "invalid_hosted_callback_uri",
      `hosted ${config.displayName} callback URI must be a valid URL`
    );
  }
  if (
    parsed.protocol !== "https:" ||
    parsed.username !== "" ||
    parsed.password !== "" ||
    parsed.hostname === "" ||
    hasExplicitPort ||
    parsed.port !== "" ||
    parsed.pathname !== config.hostedCallbackPath ||
    parsed.search !== "" ||
    parsed.hash !== ""
  ) {
    throw badRequest(
      "invalid_hosted_callback_uri",
      `hosted ${config.displayName} callback URI must be an HTTPS URL at ${config.hostedCallbackPath} without userinfo, port, query, or fragment`
    );
  }
  return parsed.toString();
}

export function validateConnectorRedirectUri(env: BrokerEnv, connector: ConnectorId, redirectUri: string): string {
  const config = CONNECTOR_REDIRECT_CONFIGS[connector];
  return validateLoopbackRedirectUri(config.displayName, config.allowedRedirectUris(env), redirectUri);
}

export function validateConnectorExchangeRedirectUri(
  env: BrokerEnv,
  connector: ConnectorId,
  redirectUri: string
): string {
  const hosted = hostedConnectorCallbackUri(env, connector);
  if (hosted && redirectUri === hosted) {
    return redirectUri;
  }
  return validateConnectorRedirectUri(env, connector, redirectUri);
}

function hasExplicitAuthorityPort(value: string): boolean {
  const authority = /^[a-zA-Z][a-zA-Z0-9+.-]*:\/\/([^/?#]*)/.exec(value)?.[1];
  if (!authority) {
    return false;
  }
  const host = authority.slice(authority.lastIndexOf("@") + 1);
  if (host.startsWith("[")) {
    return host.includes("]:");
  }
  return host.includes(":");
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
