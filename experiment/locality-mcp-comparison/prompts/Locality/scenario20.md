Have we seen reports of Locality overwriting or losing local edits after Live
Mode pulls remote changes? Search Slack, Linear, Notion docs, and repository work
for prior evidence, related safeguards, and the safest recovery or escalation
path.

Use the filesystem at `{{SANDBOX_HOME}}/Locality` for Slack, Linear, and Notion context, and use
the codebase at `{{SANDBOX_HOME}}/workspace/locality` for implementation and test evidence.

Do not use Notion MCP, Linear MCP, Slack MCP, direct Notion/Linear/Slack APIs, or
browser automation in this run. Do not create docs, post messages, close issues,
push changes, or update any remote source.

Write the final Markdown report to `{{AGENT_REPORT_PATH}}`.

Report format:

# Live Mode Edit-Loss Evidence Report

## Answer

## Prior Evidence

## Slack Findings

## Linear Findings

## Notion Findings

## Repo Findings

## Safest Recovery Path

## Recommended Engineering Action

## Gaps And Confidence

The report should be concise, specific, and grounded in source paths or command
outputs. If a source is unavailable or no evidence is found, say that directly.
