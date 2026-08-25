import { unauthorized, badRequest } from "../http/errors";
import { base64UrlDecode, base64UrlEncode, constantTimeEqual } from "./crypto";

const encoder = new TextEncoder();
const decoder = new TextDecoder();
const PICKER_CAPABILITY_PREFIX = "locpicker_v1";

export interface PickerCompletionCapabilityInput {
  version: 1;
  connector: "google-docs";
  expires_at: number;
  capability_id: string;
  redemption_secret_hash: string;
  document_ids: string[];
}

export interface PickerBrowserCapabilityInput {
  version: 1;
  connector: "google-docs";
  expires_at: number;
  capability_id: string;
  refresh_token_handle: string;
  redemption_secret_hash: string;
}

interface PickerCompletionCapability extends PickerCompletionCapabilityInput {
  document_ids: string[];
}

export interface PickerBrowserCapability extends PickerBrowserCapabilityInput {}

export async function sha256Base64Url(value: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", encoder.encode(value));
  return base64UrlEncode(new Uint8Array(digest));
}

export async function createPickerCompletionCapability(
  input: PickerCompletionCapabilityInput,
  secret: string
): Promise<string> {
  const capability: PickerCompletionCapability = {
    ...input,
    document_ids: canonicalDocumentIds(input.document_ids)
  };
  return encryptCapability(capability, secret);
}

export async function createPickerBrowserCapability(
  input: PickerBrowserCapabilityInput,
  secret: string
): Promise<string> {
  return encryptCapability(input, secret);
}

export async function readPickerBrowserCapability(
  capability: string,
  secret: string,
  now: number = Math.floor(Date.now() / 1000)
): Promise<PickerBrowserCapability> {
  const decoded = await decryptCapability(capability, secret);
  if (
    decoded.version !== 1 ||
    decoded.connector !== "google-docs" ||
    !Number.isSafeInteger(decoded.expires_at) ||
    decoded.expires_at <= now ||
    typeof decoded.capability_id !== "string" ||
    typeof decoded.redemption_secret_hash !== "string" ||
    typeof decoded.refresh_token_handle !== "string"
  ) {
    throw badRequest("invalid_picker_capability", "Google Picker session is invalid or expired");
  }
  return decoded;
}

export async function redeemPickerCompletionCapability(
  capability: string,
  redemptionSecret: string,
  secret: string,
  now: number = Math.floor(Date.now() / 1000)
): Promise<string[]> {
  const decoded = await decryptCapability(capability, secret);
  if (
    decoded.version !== 1 ||
    decoded.connector !== "google-docs" ||
    !Number.isSafeInteger(decoded.expires_at) ||
    decoded.expires_at <= now ||
    typeof decoded.capability_id !== "string" ||
    typeof decoded.redemption_secret_hash !== "string" ||
    !Array.isArray(decoded.document_ids)
  ) {
    throw badRequest("invalid_picker_capability", "Google Picker completion is invalid or expired");
  }
  if (!constantTimeEqual(decoded.redemption_secret_hash, await sha256Base64Url(redemptionSecret))) {
    throw unauthorized("picker_redemption_denied", "Google Picker completion does not belong to this Desktop session");
  }
  return canonicalDocumentIds(decoded.document_ids);
}

async function encryptCapability(
  value: PickerCompletionCapability | PickerBrowserCapability,
  secret: string
): Promise<string> {
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const ciphertext = await crypto.subtle.encrypt(
    { name: "AES-GCM", iv },
    await capabilityKey(secret),
    encoder.encode(JSON.stringify(value))
  );
  return `${PICKER_CAPABILITY_PREFIX}.${base64UrlEncode(iv)}.${base64UrlEncode(new Uint8Array(ciphertext))}`;
}

async function decryptCapability(value: string, secret: string): Promise<PickerCompletionCapability & PickerBrowserCapability> {
  const [prefix, ivText, ciphertextText] = value.split(".");
  if (prefix !== PICKER_CAPABILITY_PREFIX || !ivText || !ciphertextText) {
    throw badRequest("invalid_picker_capability", "Google Picker completion is invalid or expired");
  }
  try {
    const plaintext = await crypto.subtle.decrypt(
      { name: "AES-GCM", iv: toArrayBuffer(base64UrlDecode(ivText)) },
      await capabilityKey(secret),
      toArrayBuffer(base64UrlDecode(ciphertextText))
    );
    return JSON.parse(decoder.decode(plaintext)) as PickerCompletionCapability & PickerBrowserCapability;
  } catch {
    throw badRequest("invalid_picker_capability", "Google Picker completion is invalid or expired");
  }
}

async function capabilityKey(secret: string): Promise<CryptoKey> {
  const digest = await crypto.subtle.digest("SHA-256", encoder.encode(secret));
  return crypto.subtle.importKey("raw", digest, { name: "AES-GCM" }, false, ["encrypt", "decrypt"]);
}

function canonicalDocumentIds(documentIds: string[]): string[] {
  const ids = documentIds.map((id) => id.trim());
  if (ids.length === 0 || ids.some((id) => !id)) {
    throw badRequest("invalid_picker_document_ids", "Google Picker must select one or more documents");
  }
  return [...new Set(ids)].sort();
}

function toArrayBuffer(bytes: Uint8Array): ArrayBuffer {
  const copy = new Uint8Array(bytes.byteLength);
  copy.set(bytes);
  return copy.buffer;
}
