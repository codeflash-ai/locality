export interface BrokerEnv {
  LOCALITY_BROKER_SESSION_SECRET: string;
  LOCALITY_REFRESH_HANDLE_KEY?: string;
  LOCALITY_TOKEN_MODE?: "handle" | "raw";
  LOCALITY_NOTION_CLIENT_ID: string;
  LOCALITY_NOTION_CLIENT_SECRET: string;
  LOCALITY_NOTION_REDIRECT_URIS?: string;
  LOCALITY_NOTION_AUTH_BASE_URL?: string;
  LOCALITY_NOTION_API_BASE_URL?: string;
  LOCALITY_NOTION_VERSION?: string;
}

export type ConnectorId = "notion";

export interface ApiErrorBody {
  error: {
    code: string;
    message: string;
  };
}
