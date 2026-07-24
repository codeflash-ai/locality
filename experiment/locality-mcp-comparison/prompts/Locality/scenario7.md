You are running the Locality-backed multi-source comparison benchmark.

User prompt:
Have we seen this bug before? A user edited a mounted Notion page at
`engineering-wiki/standups-with-locality/2026-07-02/page.md`, removed visible
conflict markers, but Locality still could not push the file. The UI showed a
pending problem with language like `daemon content cache path is missing`.
Find out whether this is a known class of issue, whether it already appeared in
Slack, Linear, GitHub work, or Notion docs, and what the safest next engineering
action should be.

Use the Locality filesystem path for connected sources. Prefer the context
directories listed in `CONTEXT_PATHS_FILE`, the inventory in
`CONTEXT_INVENTORY`, and search hits in `CONTEXT_SEARCH_RESULTS`. Search mounted
Notion, Slack, Linear, and GitHub-like Locality directories when they are
available. Use `loc status`, `loc info`, `loc search`, `git`, and `gh` when they
help verify evidence.

Do not use Notion MCP, Linear MCP, Slack MCP, direct Notion/Linear/Slack APIs,
or browser automation in this run. Do not create docs, push changes, post
messages, close issues, or update any remote source. Write the Markdown body to
`REPORT_FILE`.

Required work:
1. Identify the strongest matching prior incidents, issues, PRs, commits,
   Slack discussions, or Notion docs.
2. Separate verified evidence from plausible interpretation.
3. Explain whether this looks like a cache/shadow-state issue, a connector
   issue, a conflict-marker validation issue, a File Provider projection issue,
   or something else.
4. Recommend the next safe action for engineering and the safest user recovery
   path.
5. Write a compact trace to `TRACE_FILE` listing:
   - `loc`, `git`, `gh`, and shell searches used
   - mounted Locality files read
   - search terms used
   - source gaps or unavailable connectors

Report format:

# Prior Bug Evidence Report

## Answer

## Evidence Found

## Source-by-Source Findings

## Likely Root Cause Class

## Recommended Engineering Action

## Safe User Recovery Path

## Gaps And Confidence

The draft should be concise, specific, and grounded in source paths or command
outputs. If a source is unavailable or no evidence is found, say that directly.
