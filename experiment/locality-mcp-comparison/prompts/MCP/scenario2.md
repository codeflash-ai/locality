You are preparing a launch gate memo for Locality. Find the relevant project context and recent code changes at `~/workspace/locality`, decide what is actually proven, what is still unverified, and what should block launch. Produce a concise Markdown memo.

Use the Notion MCP server you have access to for project and launch context, and
use the codebase at `~/workspace/locality` for repository evidence.

Do not use direct non-MCP Notion APIs or browser automation in this run. Do not
read mounted Locality files under `~/Locality`. Do not create a new notion page
or modify existing notion pages.

Write the final Markdown report to `/home/amika/final_report.md`.

Report format:

# Locality Launch Gate Memo

## Recommendation

## Evidence Reviewed

## Proven

## Unverified

## Launch Blockers

## Required Validation

The memo should be concise, specific, and grounded in evidence. If a claim cannot
be verified from git, gh, MCP results, or command outputs, say so.
