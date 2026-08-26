import { badRequest, configError } from "./http/errors";
import { decryptJsonHandle, encryptJsonHandle } from "./security/crypto";
import { nowSeconds } from "./security/session";
import type { BrokerEnv, ConnectorId } from "./types";

const OPERATIONAL_SECRET_MIN_LENGTH = 32;

export interface RefreshRequest {
  refresh_token?: string;
  refresh_token_handle?: string;
}

interface RefreshHandlePayload {
  v: 1;
  connector: ConnectorId;
  refresh_token: string;
  issued_at: number;
}

export async function shapeRefreshToken(
  env: BrokerEnv,
  connector: ConnectorId,
  refreshToken: string | undefined
) {
  if (!refreshToken) {
    return {};
  }
  if (tokenMode(env) === "raw") {
    return {
      refresh_token_kind: "raw",
      refresh_token: refreshToken
    };
  }
  const secret = requireRefreshHandleSecret(env);
  const handle = await encryptJsonHandle(
    {
      v: 1,
      connector,
      refresh_token: refreshToken,
      issued_at: nowSeconds()
    } satisfies RefreshHandlePayload,
    secret
  );
  return {
    refresh_token_kind: "handle",
    refresh_token_handle: handle
  };
}

export async function resolveRefreshToken(
  env: BrokerEnv,
  connector: ConnectorId,
  body: RefreshRequest
): Promise<string> {
  if (body.refresh_token_handle) {
    return (await decodeRefreshTokenHandle(env, connector, body.refresh_token_handle)).refresh_token;
  }
  if (tokenMode(env) !== "raw") {
    throw badRequest("missing_refresh_handle", "refresh_token_handle is required");
  }
  if (!body.refresh_token || body.refresh_token.trim() === "") {
    throw badRequest("missing_field", "refresh_token is required");
  }
  return body.refresh_token;
}

export async function validateRefreshTokenHandle(
  env: BrokerEnv,
  connector: ConnectorId,
  refreshTokenHandle: string
): Promise<void> {
  await decodeRefreshTokenHandle(env, connector, refreshTokenHandle);
}

export function tokenMode(env: BrokerEnv): "handle" | "raw" {
  const mode = env.LOCALITY_TOKEN_MODE ?? (env.LOCALITY_REFRESH_HANDLE_KEY ? "handle" : "raw");
  if (mode !== "handle" && mode !== "raw") {
    throw configError("LOCALITY_TOKEN_MODE must be either handle or raw");
  }
  return mode;
}

function requireRefreshHandleSecret(env: BrokerEnv): string {
  const secret = env.LOCALITY_REFRESH_HANDLE_KEY;
  if (!secret || secret.length < OPERATIONAL_SECRET_MIN_LENGTH) {
    throw configError("LOCALITY_REFRESH_HANDLE_KEY must be configured");
  }
  return secret;
}

async function decodeRefreshTokenHandle(
  env: BrokerEnv,
  connector: ConnectorId,
  refreshTokenHandle: string
): Promise<RefreshHandlePayload> {
  const secret = requireRefreshHandleSecret(env);
  try {
    const payload = await decryptJsonHandle<RefreshHandlePayload>(refreshTokenHandle, secret);
    if (
      payload.v !== 1 ||
      payload.connector !== connector ||
      typeof payload.refresh_token !== "string" ||
      payload.refresh_token.trim() === ""
    ) {
      throw new Error("invalid refresh handle payload");
    }
    return payload;
  } catch {
    throw badRequest("invalid_refresh_handle", "refresh_token_handle is invalid");
  }
}
