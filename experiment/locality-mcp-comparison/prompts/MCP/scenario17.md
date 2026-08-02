Investigate whether Locality has a known issue where a moved or renamed Notion
page appears missing, duplicated, or stale in the mounted filesystem after sync.
Search Slack reports, Linear issues, Notion docs, and repository work for prior
evidence and likely root cause classes.

Use the Slack, Linear, and Notion MCP servers you have access to, and use the
codebase at `{{SANDBOX_HOME}}/workspace/locality` for implementation and test evidence.

Do not use direct Notion/Linear/Slack APIs or browser automation in this run. Do
not read mounted Locality files under `{{SANDBOX_HOME}}/Locality`. Do not create docs, post
messages, close issues, push changes, or update any remote source.

Write the final Markdown report to `{{AGENT_REPORT_PATH}}`.

Report format:

# Moved Page Sync Investigation

## Answer

## Similar Reports

## Linear Context

## Notion Context

## Repo Evidence

## Likely Root Cause Classes

## Safest Next Engineering Action

## Gaps And Confidence

The investigation should be concise, specific, and grounded in source paths, MCP
results, or command outputs. If a source is unavailable or no evidence is found,
say that directly.
