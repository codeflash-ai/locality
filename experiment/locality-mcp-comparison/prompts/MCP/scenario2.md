You are running the Notion-MCP launch-readiness comparison benchmark.

User prompt:
Act like you are preparing a launch gate memo for Locality. Find the relevant project context and recent code changes, decide what is actually proven, what is still unverified, and what should block launch. Produce a concise Markdown memo.

Do not create Notion pages/docs, push or update Notion, or update any remote source.

Required work:
1. Write the final Markdown memo to `REPORT_FILE`.
2. Write a compact trace to `TRACE_FILE` listing:
   - git commands used
   - Notion MCP searches/calls attempted
   - Notion pages or excerpts used
   - limitations

Report format:

# Locality Launch Gate Memo

## Recommendation

## Evidence Reviewed

## Proven

## Unverified

## Launch Blockers

## Required Validation

The memo should be concise, specific, and grounded in evidence. If a claim cannot be verified from git or Notion MCP context, say so.
