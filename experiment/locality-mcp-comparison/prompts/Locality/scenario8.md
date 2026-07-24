You are running the Locality-backed multi-source comparison benchmark.

User prompt:
Can we make a new Locality release this week? Check the connected knowledge
around launch/release readiness, current engineering risks, open issues,
recent GitHub activity, and team discussions. Give a practical release call:
go, no-go, or go with named conditions.

Use the Locality filesystem path for connected sources. Prefer the context
directories listed in `CONTEXT_PATHS_FILE`, the inventory in
`CONTEXT_INVENTORY`, and search hits in `CONTEXT_SEARCH_RESULTS`. Search mounted
Notion, Slack, Linear, and GitHub-like Locality directories when they are
available. Use `loc status`, `loc info`, `loc search`, `git`, and `gh` when they
help verify evidence.

Do not use Notion MCP, Linear MCP, Slack MCP, direct Notion/Linear/Slack APIs,
or browser automation in this run. Do not create a release, create docs, push
changes, post messages, close issues, or update any remote source. Write the
Markdown body to `REPORT_FILE`.

Required work:
1. Find release-related context in Notion or other mounted docs.
2. Check Linear for open blockers, recent fixes, and release-critical issues.
3. Check Slack for launch/release discussions, unresolved decisions, or risk
   signals when Slack context is mounted.
4. Check GitHub evidence through local git and `gh`: recent commits, open PRs,
   release tags, workflows, and notable failures when available.
5. Produce a release recommendation with explicit conditions and owner-facing
   next actions.
6. Write a compact trace to `TRACE_FILE` listing:
   - `loc`, `git`, `gh`, and shell searches used
   - mounted Locality files read
   - source gaps or unavailable connectors

Report format:

# Release Readiness Decision

## Recommendation

## Evidence Summary

## Source-by-Source Findings

## Blockers And Conditions

## Suggested Release Plan

## Gaps And Confidence

The draft should be concise, specific, and grounded in source paths or command
outputs. If a source is unavailable or no evidence is found, say that directly.
