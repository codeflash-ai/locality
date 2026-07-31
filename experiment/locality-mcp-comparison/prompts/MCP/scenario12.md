Build a regression map for File Provider and mounted-file write issues in
Locality. Look for Slack reports, Linear bugs, Notion design or troubleshooting
docs, and repo changes related to atomic writes, renames, conflicts, missing
content cache files, or failed pushes.

Use the Slack, Linear, and Notion MCP servers you have access to, and use the
codebase at `~/workspace/locality` for implementation and test evidence.

Do not use direct Notion/Linear/Slack APIs or browser automation in this run. Do
not read mounted Locality files under `~/Locality`. Do not create docs, post
messages, close issues, push changes, or update any remote source.

Write the final Markdown report to `/home/ubuntu/final_report.md`.

Report format:

# File Provider Regression Map

## Summary

## Issue Classes

## Slack Reports

## Linear Bugs

## Notion Context

## Repo Evidence

## Open Risks

## Recommended Test Coverage

## Gaps And Confidence

The report should be concise, specific, and grounded in source paths, MCP results,
or command outputs. If a source is unavailable or no evidence is found, say that
directly.
