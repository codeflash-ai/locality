import { configError } from "../http/errors";
import type { BrokerEnv } from "../types";

export function googleClientId(env: BrokerEnv): string {
  return requireEnv(env.LOCALITY_GOOGLE_CLIENT_ID, "LOCALITY_GOOGLE_CLIENT_ID");
}

export function googleClientSecret(env: BrokerEnv): string {
  return requireEnv(env.LOCALITY_GOOGLE_CLIENT_SECRET, "LOCALITY_GOOGLE_CLIENT_SECRET");
}

function requireEnv(value: string | undefined, name: string): string {
  if (!value) {
    throw configError(`${name} is required`);
  }
  return value;
}
