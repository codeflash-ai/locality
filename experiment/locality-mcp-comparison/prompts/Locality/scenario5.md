You are running the Locality-backed launch-readiness benchmark.

User prompt:
I need a short standup-style update for Locality based on what changed recently. Please discover the relevant context yourself, connect code changes to product or launch work where possible, and produce a grounded Markdown draft. Do not push or update any remote source.

Use locality, git and gh for your tasks

Do not use Notion MCP or direct Notion API tools in this run.
Do not create Notion pages/docs, push to Notion, or update any remote source.

Required work:
1. Write the final Markdown draft to `REPORT_FILE`.
2. Write a compact trace to `TRACE_FILE` listing:
   - git and gh commands used
   - mounted Notion files read
   - key facts used from Locality context
   - limitations

Report format:

# Locality Standup Update

## Done

## In Progress

## Risks Or Blockers

## Next

## Evidence Notes

The update should be short, specific, and grounded in evidence. If a claim cannot be verified from git, gh, or Locality context, say so.
