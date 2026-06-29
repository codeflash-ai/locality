export class HttpError extends Error {
  readonly status: number;
  readonly code: string;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = "HttpError";
    this.status = status;
    this.code = code;
  }
}

export function badRequest(code: string, message: string): HttpError {
  return new HttpError(400, code, message);
}

export function unauthorized(code: string, message: string): HttpError {
  return new HttpError(401, code, message);
}

export function upstreamError(message: string): HttpError {
  return new HttpError(502, "upstream_oauth_error", message);
}

export function configError(message: string): HttpError {
  return new HttpError(500, "broker_config_error", message);
}
