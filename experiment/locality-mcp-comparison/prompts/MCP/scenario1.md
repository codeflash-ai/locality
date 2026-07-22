You are running the Notion-MCP launch-readiness comparison benchmark.

Goal: generate the same launch-readiness report using local git metadata plus Notion MCP for Notion context.

Use these context sources:
- local git commands in `REPO_DIR`
- `OUT_DIR/git-data.json`
- Notion MCP tools for Notion search/read context

Do not read mounted Locality Notion files under `/home/amika/notion`.
Do not use `loc` commands.
Do not create Notion pages/docs, push or update Notion, or update any remote source.

Required work:
1. Read `OUT_DIR/git-data.json`.
2. Inspect relevant commits with local git commands as needed.
3. Use Notion MCP to search/read context around:
   - Locality Launch Amika Environment
   - launch readiness
   - install/update/distribution
   - safe diff, push, review, and prompt-injection guidance
   - Live Mode, conflicts, File Provider, Windows Cloud Files
   - connector launch readiness
   - standups and internal daily-use workflow
4. Read the benchmark case section in the Locality Launch Amika Environment page through Notion MCP.
5. Write the final Markdown report to `OUT_DIR/notion-mcp-report-body.md`.
6. Write a compact trace to `OUT_DIR/notion-mcp-agent-trace.md` listing:
   - git commands used
   - Notion MCP searches/calls attempted
   - Notion pages or excerpts used
   - limitations

Report format:

# Locality Launch Readiness Report

## Executive Summary

## What Changed In Git

## How This Maps To Launch Readiness

## Cross-Page Context Used

## Risks And Blockers

## Recommended Next Actions

## Measurement Notes

The report should be human, specific, and grounded in evidence. Avoid generic filler. If a claim cannot be verified from git or Notion context, say so.
