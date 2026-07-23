You are running the Notion-MCP launch-readiness comparison benchmark.

User prompt:
I need a short standup-style update for Locality based on what changed recently. Please discover the relevant context yourself, connect code changes to product or launch work where possible, and produce a grounded Markdown draft. Do not push or update any remote source.

Required work:
1. Inspect recent code changes with local git commands as needed.
2. Use Notion MCP to search/read relevant product, launch, engineering, risk, blocker, and team context.
3. Produce a short standup-style update with grounded evidence.
4. Write the final Markdown draft to `REPORT_FILE`.
5. Write a compact trace to `TRACE_FILE` listing:
   - git commands used
   - Notion MCP searches/calls attempted
   - Notion pages or excerpts used
   - limitations

Report format:

# Locality Standup Update

## Done

## In Progress

## Risks Or Blockers

## Next

## Evidence Notes

The update should be short, specific, and grounded in evidence. If a claim cannot be verified from git or Notion MCP context, say so.
