import { badRequest, configError, HttpError } from "./http/errors";
import type { BrokerEnv } from "./types";

const MAX_BATCH_BYTES = 64 * 1024;
const MAX_BATCH_EVENTS = 50;
const TOKEN = /^[A-Za-z0-9._-]+$/;
const EVENT_KEYS = new Set([
  "schema_version",
  "event_id",
  "occurred_at_ms",
  "anonymous_id",
  "session_id",
  "app",
  "version",
  "build_id",
  "os",
  "arch",
  "name",
  "properties"
]);
const PROPERTY_KEYS = new Set(["code", "connector", "kind", "outcome", "severity", "source_file", "source_line"]);
const OUTCOMES = new Set(["started", "succeeded", "failed", "cancelled"]);
const SEVERITIES = new Set(["info", "warning", "error", "fatal"]);

interface TelemetryBatch {
  schema_version: number;
  events: TelemetryEvent[];
}

interface TelemetryEvent {
  schema_version: number;
  event_id: string;
  occurred_at_ms: number;
  anonymous_id: string;
  session_id: string;
  app: string;
  version: string;
  build_id: string;
  os: string;
  arch: string;
  name: string;
  properties: Record<string, string | number>;
}

export async function ingestTelemetry(request: Request, env: BrokerEnv): Promise<{ accepted: number }> {
  const declaredLength = Number(request.headers.get("content-length") ?? "0");
  if (Number.isFinite(declaredLength) && declaredLength > MAX_BATCH_BYTES) {
    throw new HttpError(413, "telemetry_batch_too_large", "telemetry batch exceeds 64 KiB");
  }
  if (!request.headers.get("content-type")?.includes("application/json")) {
    throw badRequest("invalid_telemetry", "telemetry request must be JSON");
  }
  const text = await boundedRequestText(request);
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    throw badRequest("invalid_json", "request body must be valid JSON");
  }
  const batch = validateBatch(value);
  await forwardToPostHog(batch, env);
  return { accepted: batch.events.length };
}

async function boundedRequestText(request: Request): Promise<string> {
  const reader = request.body?.getReader();
  if (!reader) {
    return "";
  }
  const chunks: Uint8Array[] = [];
  let length = 0;
  while (true) {
    const { done, value } = await reader.read();
    if (done) {
      break;
    }
    length += value.byteLength;
    if (length > MAX_BATCH_BYTES) {
      await reader.cancel();
      throw new HttpError(413, "telemetry_batch_too_large", "telemetry batch exceeds 64 KiB");
    }
    chunks.push(value);
  }
  const bytes = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return new TextDecoder().decode(bytes);
}

function validateBatch(value: unknown): TelemetryBatch {
  const batch = record(value, "telemetry batch");
  exactKeys(batch, new Set(["schema_version", "events"]), "telemetry batch");
  if (batch.schema_version !== 1) {
    throw badRequest("unsupported_telemetry_schema", "telemetry schema version is not supported");
  }
  if (!Array.isArray(batch.events) || batch.events.length === 0 || batch.events.length > MAX_BATCH_EVENTS) {
    throw badRequest("invalid_telemetry", "telemetry batch must contain 1 to 50 events");
  }
  return {
    schema_version: 1,
    events: batch.events.map((event, index) => validateEvent(event, index))
  };
}

function validateEvent(value: unknown, index: number): TelemetryEvent {
  const event = record(value, `events[${index}]`);
  exactKeys(event, EVENT_KEYS, `events[${index}]`);
  if (event.schema_version !== 1) {
    throw badRequest("unsupported_telemetry_schema", `events[${index}] has an unsupported schema`);
  }
  const properties = record(event.properties, `events[${index}].properties`);
  exactKeys(properties, PROPERTY_KEYS, `events[${index}].properties`);
  for (const [key, property] of Object.entries(properties)) {
    if (key === "source_line") {
      if (!Number.isSafeInteger(property) || (property as number) < 1) {
        throw badRequest("invalid_telemetry", `${key} must be a positive integer`);
      }
      continue;
    }
    if (typeof property !== "string" || !TOKEN.test(property) || property.length > 160) {
      throw badRequest("invalid_telemetry", `${key} must be a bounded machine-readable token`);
    }
  }
  if (properties.outcome !== undefined && !OUTCOMES.has(properties.outcome as string)) {
    throw badRequest("invalid_telemetry", "outcome is not recognized");
  }
  if (properties.severity !== undefined && !SEVERITIES.has(properties.severity as string)) {
    throw badRequest("invalid_telemetry", "severity is not recognized");
  }
  if (
    !Number.isSafeInteger(event.occurred_at_ms) ||
    (event.occurred_at_ms as number) < 0 ||
    (event.occurred_at_ms as number) > 8_640_000_000_000_000
  ) {
    throw badRequest("invalid_telemetry", "occurred_at_ms must be a non-negative integer");
  }

  return {
    schema_version: 1,
    event_id: token(event.event_id, "event_id", 80),
    occurred_at_ms: event.occurred_at_ms as number,
    anonymous_id: hexId(event.anonymous_id, "anonymous_id"),
    session_id: hexId(event.session_id, "session_id"),
    app: token(event.app, "app", 40),
    version: token(event.version, "version", 40),
    build_id: token(event.build_id, "build_id", 128),
    os: token(event.os, "os", 40),
    arch: token(event.arch, "arch", 40),
    name: token(event.name, "name", 80),
    properties: properties as Record<string, string | number>
  };
}

async function forwardToPostHog(batch: TelemetryBatch, env: BrokerEnv): Promise<void> {
  const projectKey = env.LOCALITY_POSTHOG_PROJECT_KEY;
  if (!projectKey) {
    throw configError("LOCALITY_POSTHOG_PROJECT_KEY must be configured");
  }
  const host = (env.LOCALITY_POSTHOG_HOST ?? "https://us.i.posthog.com").replace(/\/+$/, "");
  const response = await fetch(`${host}/batch/`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      api_key: projectKey,
      historical_migration: false,
      batch: batch.events.map((event) => ({
        event: event.name,
        timestamp: new Date(event.occurred_at_ms).toISOString(),
        properties: {
          distinct_id: event.anonymous_id,
          $insert_id: event.event_id,
          $process_person_profile: false,
          $lib: "locality",
          schema_version: event.schema_version,
          session_id: event.session_id,
          app: event.app,
          version: event.version,
          build_id: event.build_id,
          os: event.os,
          arch: event.arch,
          ...event.properties
        }
      }))
    })
  });
  if (!response.ok) {
    throw new HttpError(502, "telemetry_upstream_error", `telemetry provider returned HTTP ${response.status}`);
  }
}

function record(value: unknown, label: string): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw badRequest("invalid_telemetry", `${label} must be an object`);
  }
  return value as Record<string, unknown>;
}

function exactKeys(value: Record<string, unknown>, allowed: Set<string>, label: string): void {
  const unknown = Object.keys(value).find((key) => !allowed.has(key));
  if (unknown) {
    throw badRequest("invalid_telemetry", `${label} contains unsupported field ${unknown}`);
  }
}

function token(value: unknown, field: string, maxLength: number): string {
  if (typeof value !== "string" || value.length === 0 || value.length > maxLength || !TOKEN.test(value)) {
    throw badRequest("invalid_telemetry", `${field} must be a bounded machine-readable token`);
  }
  return value;
}

function hexId(value: unknown, field: string): string {
  if (typeof value !== "string" || !/^[a-f0-9]{32}$/.test(value)) {
    throw badRequest("invalid_telemetry", `${field} must be a 128-bit lowercase hex id`);
  }
  return value;
}
