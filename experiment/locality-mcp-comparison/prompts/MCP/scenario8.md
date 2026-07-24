You are running the MCP-backed multi-source comparison benchmark.

User prompt:
Can we make a new Locality release this week? Check the connected knowledge
around launch/release readiness, current engineering risks, open issues,
recent GitHub activity, and team discussions. Give a practical release call:
go, no-go, or go with named conditions.

Use MCP tools for Notion, Linear, and Slack when available. If a GitHub MCP
server is available, use it for GitHub evidence; otherwise use local `git` and
`gh` against the checked-out repository. Do not use `loc`, mounted Locality
files, or Locality context inventory files in this run.

Do not create a release, create docs, push changes, post messages, close
issues, or update any remote source. Write the Markdown body to `REPORT_FILE`.

Required work:
1. Find release-related context in Notion or other MCP-accessible docs.
2. Check Linear for open blockers, recent fixes, and release-critical issues.
3. Check Slack for launch/release discussions, unresolved decisions, or risk
   signals when Slack MCP is available.
4. Check GitHub evidence through GitHub MCP when available, otherwise local git
   and `gh`: recent commits, open PRs, release tags, workflows, and notable
   failures when available.
5. Produce a release recommendation with explicit conditions and owner-facing
   next actions.
6. Write a compact trace to `TRACE_FILE` listing:
   - MCP searches/calls attempted
   - `git` and `gh` commands used
   - source records or excerpts used
   - source gaps or unavailable MCP servers

Report format:

# Release Readiness Decision

## Recommendation

## Evidence Summary

## Source-by-Source Findings

## Blockers And Conditions

## Suggested Release Plan

## Gaps And Confidence

The draft should be concise, specific, and grounded in source records or command
outputs. If a source is unavailable or no evidence is found, say that directly.
