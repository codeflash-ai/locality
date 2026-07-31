Have we seen this bug before? A user edited a mounted Notion page at
`abc/xyz/page.md`, removed visible
conflict markers, but Locality still could not push the file. The UI showed a
pending problem with language like `daemon content cache path is missing`.
Find out whether this is a known class of issue, whether it already appeared in
Slack, Linear, GitHub work, or Notion docs, and what the safest next engineering
action should be.

Use the Slack, Linear, and Notion MCP servers you have access to for remote
evidence, and use the codebase at `~/workspace/locality` for implementation and
test evidence.

Do not use direct non-MCP Notion/Linear/Slack APIs or browser automation in this
run. Do not read mounted Locality files under `~/Locality`. Do not create a new
notion page or modify existing notion pages.

Write the final Markdown report to `/home/ubuntu/final_report.md`.

Report format:

# Prior Bug Evidence Report

## Answer

## Evidence Found

## Source-by-Source Findings

## Likely Root Cause Class

## Recommended Engineering Action

## Safe User Recovery Path

## Gaps And Confidence

The draft should be concise, specific, and grounded in source paths, MCP results,
or command outputs. If a source is unavailable or no evidence is found, say that
directly.
