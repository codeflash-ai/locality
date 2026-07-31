Investigate whether Locality has a known issue where a moved or renamed Notion
page appears missing, duplicated, or stale in the mounted filesystem after sync.
Search Slack reports, Linear issues, Notion docs, and repository work for prior
evidence and likely root cause classes.

Use the filesystem at `~/Locality` for Slack, Linear, and Notion context, and use
the codebase at `~/workspace/locality` for implementation and test evidence.

Do not use Notion MCP, Linear MCP, Slack MCP, direct Notion/Linear/Slack APIs, or
browser automation in this run. Do not create docs, post messages, close issues,
push changes, or update any remote source.

Write the final Markdown report to `/home/ubuntu/final_report.md`.

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

The investigation should be concise, specific, and grounded in source paths or
command outputs. If a source is unavailable or no evidence is found, say that directly.
