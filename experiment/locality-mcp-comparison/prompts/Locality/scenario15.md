Audit stale blockers for the next Locality release. Compare blockers mentioned
in Slack, open or recently closed Linear issues, and Notion release or launch
docs. Separate blockers that are still active from blockers that appear resolved
or unsupported by evidence.

Use the filesystem at `~/Locality` for Slack, Linear, and Notion context, and use
the codebase at `~/workspace/locality` to verify implementation status when
needed.

Do not use Notion MCP, Linear MCP, Slack MCP, direct Notion/Linear/Slack APIs, or
browser automation in this run. Do not create docs, post messages, close issues,
push changes, or update any remote source.

Write the final Markdown report to `/home/ubuntu/final_report.md`.

Report format:

# Release Blocker Staleness Audit

## Summary

## Active Blockers

## Probably Resolved Blockers

## Unsubstantiated Blockers

## Slack Evidence

## Linear Evidence

## Notion Evidence

## Verification Needed

## Gaps And Confidence

The audit should be concise, specific, and grounded in source paths or command
outputs. If a source is unavailable or no evidence is found, say that directly.
