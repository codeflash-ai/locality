import { HttpError } from "./http/errors";
import { refreshSlackToken } from "./oauth/slack";
import { resolveRefreshToken, type RefreshRequest } from "./refresh-handles";
import { decryptJsonHandle, encryptJsonHandle } from "./security/crypto";
import { nowSeconds } from "./security/session";
import { shapeSlackTokenResponse } from "./slack-token-response";
import type { ApiErrorBody, BrokerEnv } from "./types";

const SUCCESS_KEY = "successful-refresh";
const REPLAY_TTL_SECONDS = 10 * 60;

interface CachedRefresh {
  encrypted_response: string;
  refreshed_at: number;
  expires_at: number;
}

interface SlackRefreshCoordinatorTransaction {
  put<T>(key: string, value: T): Promise<void>;
  setAlarm(scheduledTime: number | Date): Promise<void>;
}

interface SlackRefreshCoordinatorStorage {
  get<T>(key: string): Promise<T | undefined>;
  deleteAll(): Promise<void>;
  transaction<T>(closure: (txn: SlackRefreshCoordinatorTransaction) => Promise<T>): Promise<T>;
}

/**
 * Coordinates one opaque Slack refresh handle. A successful rotation is
 * persisted before its response is returned so a lost client response can be
 * replayed without presenting Slack with the single-use token again.
 */
export class SlackRefreshCoordinatorCore {
  private inFlight: Promise<CachedRefresh> | undefined;

  constructor(
    private readonly storage: SlackRefreshCoordinatorStorage,
    private readonly env: BrokerEnv
  ) {}

  async fetch(request: Request): Promise<Response> {
    if (request.method !== "POST") {
      return Response.json(
        { error: { code: "method_not_allowed", message: "method not allowed" } } satisfies ApiErrorBody,
        { status: 405, headers: { Allow: "POST" } }
      );
    }

    try {
      const body = (await request.json()) as RefreshRequest;
      if (!body.refresh_token_handle) {
        throw new HttpError(400, "missing_refresh_handle", "refresh_token_handle is required");
      }
      const cached = await this.cachedRefresh();
      const refresh = cached ?? (await this.coalescedRefresh(body));
      return noStoreJson(await this.replayResponse(refresh));
    } catch (error) {
      const httpError =
        error instanceof HttpError ? error : new HttpError(500, "internal_error", "internal server error");
      return noStoreJson(
        { error: { code: httpError.code, message: httpError.message } } satisfies ApiErrorBody,
        httpError.status
      );
    }
  }

  async alarm(): Promise<void> {
    await this.storage.deleteAll();
  }

  private async cachedRefresh(): Promise<CachedRefresh | undefined> {
    const cached = await this.storage.get<CachedRefresh>(SUCCESS_KEY);
    if (!cached || cached.expires_at <= nowSeconds()) {
      return undefined;
    }
    return cached;
  }

  private async coalescedRefresh(body: RefreshRequest): Promise<CachedRefresh> {
    if (!this.inFlight) {
      this.inFlight = this.refreshAndPersist(body).finally(() => {
        this.inFlight = undefined;
      });
    }
    return this.inFlight;
  }

  private async refreshAndPersist(body: RefreshRequest): Promise<CachedRefresh> {
    const refreshToken = await resolveRefreshToken(this.env, "slack", body);
    const token = await refreshSlackToken(this.env, refreshToken);
    const response = await shapeSlackTokenResponse(this.env, token);
    const refreshedAt = nowSeconds();
    const expiresAt = refreshedAt + REPLAY_TTL_SECONDS;
    const encryptedResponse = await encryptJsonHandle(response, refreshCacheSecret(this.env));
    const cached = {
      encrypted_response: encryptedResponse,
      refreshed_at: refreshedAt,
      expires_at: expiresAt
    } satisfies CachedRefresh;

    await this.storage.transaction(async (txn) => {
      await txn.put(SUCCESS_KEY, cached);
      await txn.setAlarm(expiresAt * 1000);
    });
    return cached;
  }

  private async replayResponse(cached: CachedRefresh): Promise<Record<string, unknown>> {
    const response = await decryptJsonHandle<Record<string, unknown>>(
      cached.encrypted_response,
      refreshCacheSecret(this.env)
    );
    const expiresIn = response.expires_in;
    if (typeof expiresIn === "number") {
      const elapsed = Math.max(0, nowSeconds() - cached.refreshed_at);
      response.expires_in = Math.max(0, expiresIn - elapsed);
    }
    return response;
  }
}

export class SlackRefreshCoordinator {
  private readonly core: SlackRefreshCoordinatorCore;

  constructor(state: DurableObjectState, env: BrokerEnv) {
    this.core = new SlackRefreshCoordinatorCore(state.storage, env);
  }

  fetch(request: Request): Promise<Response> {
    return this.core.fetch(request);
  }

  alarm(): Promise<void> {
    return this.core.alarm();
  }
}

function refreshCacheSecret(env: BrokerEnv): string {
  const secret = env.LOCALITY_REFRESH_HANDLE_KEY;
  if (!secret || secret.length < 32) {
    throw new HttpError(500, "broker_config_error", "LOCALITY_REFRESH_HANDLE_KEY must be configured");
  }
  return secret;
}

function noStoreJson(value: unknown, status = 200): Response {
  return Response.json(value, {
    status,
    headers: { "Cache-Control": "no-store" }
  });
}
