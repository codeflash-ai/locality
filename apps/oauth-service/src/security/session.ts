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

export async function signSession(payload: OAuthSessionPayload, secret: string): Promise<string> {
  const body = utf8Base64Url(JSON.stringify(payload));
  const signature = await hmacSha256Base64Url(secret, body);
  return `${body}.${signature}`;
}

export async function verifySession(token: string, secret: string, now = nowSeconds()): Promise<OAuthSessionPayload> {
  const [body, signature] = token.split(".");
  if (!body || !signature) {
    throw badRequest("invalid_session", "OAuth session token is malformed");
  }
  const expected = await hmacSha256Base64Url(secret, body);
  if (!constantTimeEqual(signature, expected)) {
    throw unauthorized("invalid_session", "OAuth session token signature is invalid");
  }
  let payload: OAuthSessionPayload;
  try {
    payload = JSON.parse(parseUtf8Base64Url(body)) as OAuthSessionPayload;
  } catch {
    throw badRequest("invalid_session", "OAuth session token payload is invalid");
  }
  if (!isOAuthSessionPayload(payload)) {
    throw badRequest("invalid_session", "OAuth session token payload is invalid");
  }
  if (payload.exp <= now) {
    throw unauthorized("expired_session", "OAuth session has expired");
  }
  return payload;
}

export function nowSeconds(): number {
  return Math.floor(Date.now() / 1000);
}

function isOAuthSessionPayload(value: unknown): value is OAuthSessionPayload {
  if (!value || typeof value !== "object") {
    return false;
  }
  const payload = value as Partial<OAuthSessionPayload>;
  return (
    payload.v === 1 &&
    (payload.connector === "notion" ||
      payload.connector === "google-docs" ||
      payload.connector === "google-calendar" ||
      payload.connector === "gmail" ||
      payload.connector === "slack") &&
    typeof payload.state === "string" &&
    typeof payload.redirect_uri === "string" &&
    typeof payload.iat === "number" &&
    typeof payload.exp === "number" &&
    typeof payload.nonce === "string"
  );
}
