Have we seen reports of Locality overwriting or losing local edits after Live
Mode pulls remote changes? Search Slack, Linear, Notion docs, and repository work
for prior evidence, related safeguards, and the safest recovery or escalation
path.

Use the Slack, Linear, and Notion MCP servers you have access to, and use the
codebase at `~/workspace/locality` for implementation and test evidence.

Do not use direct Notion/Linear/Slack APIs or browser automation in this run. Do
not read mounted Locality files under `~/Locality`. Do not create docs, post
messages, close issues, push changes, or update any remote source.

Write the final Markdown report to `/home/amika/final_report.md`.

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

The report should be concise, specific, and grounded in source paths, MCP results,
or command outputs. If a source is unavailable or no evidence is found, say that
directly.
