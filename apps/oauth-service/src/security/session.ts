import { badRequest, unauthorized } from "../http/errors";
import type { ConnectorId } from "../types";
import { constantTimeEqual, hmacSha256Base64Url, parseUtf8Base64Url, utf8Base64Url } from "./crypto";

export interface OAuthSessionPayload {
  v: 1;
  connector: ConnectorId;
  state: string;
  redirect_uri: string;
  iat: number;
  exp: number;
  nonce: string;
}

export interface OAuthLocalHandoffStatePayload {
  v: 1;
  kind: "local_handoff";
  connector: ConnectorId;
  local_redirect_uri: string;
  provider_redirect_uri: string;
  iat: number;
  exp: number;
  nonce: string;
}

export async function signSession(payload: OAuthSessionPayload, secret: string): Promise<string> {
  return signPayload(payload, secret);
}

export async function verifySession(token: string, secret: string, now = nowSeconds()): Promise<OAuthSessionPayload> {
  const payload = await verifyPayload<OAuthSessionPayload>(token, secret, "session");
  if (!isOAuthSessionPayload(payload)) {
    throw badRequest("invalid_session", "OAuth session token payload is invalid");
  }
  if (payload.exp <= now) {
    throw unauthorized("expired_session", "OAuth session has expired");
  }
  return payload;
}

export async function signLocalHandoffState(payload: OAuthLocalHandoffStatePayload, secret: string): Promise<string> {
  return signPayload(payload, secret);
}

export async function verifyLocalHandoffState(
  token: string,
  secret: string,
  now = nowSeconds()
): Promise<OAuthLocalHandoffStatePayload> {
  const payload = await verifyPayload<OAuthLocalHandoffStatePayload>(token, secret, "state");
  if (!isOAuthLocalHandoffStatePayload(payload)) {
    throw badRequest("invalid_state", "OAuth state payload is invalid");
  }
  if (payload.exp <= now) {
    throw unauthorized("expired_state", "OAuth state has expired");
  }
  return payload;
}

export function nowSeconds(): number {
  return Math.floor(Date.now() / 1000);
}

async function signPayload(payload: unknown, secret: string): Promise<string> {
  const body = utf8Base64Url(JSON.stringify(payload));
  const signature = await hmacSha256Base64Url(secret, body);
  return `${body}.${signature}`;
}

async function verifyPayload<T>(token: string, secret: string, label: "session" | "state"): Promise<T> {
  const [body, signature] = token.split(".");
  if (!body || !signature) {
    throw badRequest(`invalid_${label}`, `OAuth ${label} token is malformed`);
  }
  const expected = await hmacSha256Base64Url(secret, body);
  if (!constantTimeEqual(signature, expected)) {
    throw unauthorized(`invalid_${label}`, `OAuth ${label} token signature is invalid`);
  }
  try {
    return JSON.parse(parseUtf8Base64Url(body)) as T;
  } catch {
    throw badRequest(`invalid_${label}`, `OAuth ${label} token payload is invalid`);
  }
}

function isOAuthSessionPayload(value: unknown): value is OAuthSessionPayload {
  if (!value || typeof value !== "object") {
    return false;
  }
  const payload = value as Partial<OAuthSessionPayload>;
  return (
    payload.v === 1 &&
    isConnectorId(payload.connector) &&
    typeof payload.state === "string" &&
    typeof payload.redirect_uri === "string" &&
    typeof payload.iat === "number" &&
    typeof payload.exp === "number" &&
    typeof payload.nonce === "string"
  );
}

function isOAuthLocalHandoffStatePayload(value: unknown): value is OAuthLocalHandoffStatePayload {
  if (!value || typeof value !== "object") {
    return false;
  }
  const payload = value as Partial<OAuthLocalHandoffStatePayload>;
  return (
    payload.v === 1 &&
    payload.kind === "local_handoff" &&
    isConnectorId(payload.connector) &&
    typeof payload.local_redirect_uri === "string" &&
    typeof payload.provider_redirect_uri === "string" &&
    typeof payload.iat === "number" &&
    typeof payload.exp === "number" &&
    typeof payload.nonce === "string"
  );
}

function isConnectorId(value: unknown): value is ConnectorId {
  return (
    value === "notion" ||
    value === "google-docs" ||
    value === "google-calendar" ||
    value === "gmail" ||
    value === "slack"
  );
}
