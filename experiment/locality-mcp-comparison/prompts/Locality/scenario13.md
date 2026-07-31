Find launch or product decisions that appear in Slack but are not clearly
captured in Linear or Notion. Identify the source-of-truth gaps that could cause
the team to miss work, duplicate work, or make a stale launch decision.

Use the filesystem at `~/Locality` for Slack, Linear, and Notion context. Use the
codebase at `~/workspace/locality` only if repository evidence is needed to
confirm whether a decision was implemented.

Do not use Notion MCP, Linear MCP, Slack MCP, direct Notion/Linear/Slack APIs, or
browser automation in this run. Do not create docs, post messages, close issues,
push changes, or update any remote source.

Write the final Markdown report to `/home/ubuntu/final_report.md`.

Report format:

# Cross-App Decision Capture Audit

## Summary

## Decisions Found In Slack

## Linear Coverage

## Notion Coverage

## Source-Of-Truth Gaps

## Recommended Cleanup

## Gaps And Confidence

The audit should be concise, specific, and grounded in source paths or command
outputs. If a source is unavailable or no evidence is found, say that directly.
