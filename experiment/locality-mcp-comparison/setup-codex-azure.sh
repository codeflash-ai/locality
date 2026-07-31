#!/usr/bin/env bash
set -euo pipefail

CODEX_MODEL="${CODEX_MODEL:-gpt-5.6-sol}"
CODEX_REASONING_EFFORT="${CODEX_REASONING_EFFORT:-low}"
AZURE_OPENAI_BASE_URL="${AZURE_OPENAI_BASE_URL:-https://aseem-mp32maxp-eastus2.openai.azure.com/openai/v1}"
CODEX_HOME="${CODEX_HOME:-$HOME/.codex}"

mkdir -p "$CODEX_HOME"

cat > "$CODEX_HOME/config.toml" <<TOML
model = "$CODEX_MODEL"
model_provider = "azure"
model_reasoning_effort = "$CODEX_REASONING_EFFORT"

[model_providers.azure]
name = "Azure OpenAI"
base_url = "$AZURE_OPENAI_BASE_URL"
env_key = "AZURE_OPENAI_API_KEY"
wire_api = "responses"
TOML

chmod 700 "$CODEX_HOME"
chmod 600 "$CODEX_HOME/config.toml"

if [ -n "${AMIKA_AGENT_CWD:-}" ] && [ -d "$AMIKA_AGENT_CWD" ]; then
  mkdir -p "$AMIKA_AGENT_CWD/.codex"
  cat > "$AMIKA_AGENT_CWD/.codex/config.toml" <<'TOML'
sandbox_mode = "workspace-write"
TOML
fi

codex --version || true
