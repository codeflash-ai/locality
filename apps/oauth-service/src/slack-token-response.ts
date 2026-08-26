import type { SlackTokenResponse } from "./oauth/slack";
import { shapeRefreshToken } from "./refresh-handles";
import type { BrokerEnv } from "./types";

export async function shapeSlackTokenResponse(env: BrokerEnv, token: SlackTokenResponse) {
  const refresh = await shapeRefreshToken(env, "slack", token.refresh_token);
  const scopes = token.scope?.split(/[,\s]+/).filter(Boolean) ?? [];
  const workspace = token.team ?? token.enterprise;
  return {
    connector: "slack",
    access_token: token.access_token,
    token_type: token.token_type,
    expires_in: token.expires_in,
    scopes,
    account_id: workspace?.id,
    account_label: workspace?.name,
    workspace_id: workspace?.id,
    workspace_name: workspace?.name,
    bot_id: token.bot_user_id,
    ...refresh
  };
}
