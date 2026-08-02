Identify the most important customer-impacting launch risk for Locality by reconciling
Slack discussions, Linear issues, and Notion launch/readiness docs. Focus on risks
that are still actionable, not historical noise.

Use the filesystem at `{{SANDBOX_HOME}}/Locality` for Slack, Linear, and Notion context, and use
the codebase at `{{SANDBOX_HOME}}/workspace/locality` only when code evidence is needed.

Do not use Notion MCP, Linear MCP, Slack MCP, direct Notion/Linear/Slack APIs, or
browser automation in this run. Do not create docs, post messages, close issues,
push changes, or update any remote source.

Write the final Markdown report to `{{AGENT_REPORT_PATH}}`.

Report format:

# Customer-Impacting Launch Risk Brief

## Top Risk

## Why It Matters

## Slack Evidence

## Linear Evidence

## Notion Evidence

## Code Evidence

## Recommended Next Action

## Gaps And Confidence

The brief should be concise, specific, and grounded in source paths or command
outputs. If a source is unavailable or no evidence is found, say that directly.
