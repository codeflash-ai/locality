You are running the Locality-backed launch-readiness benchmark.

Goal: generate a concise, accurate launch-readiness report from the local git checkout plus hydrated Notion context exposed as local files by Locality.

Use only these context sources:
- local git commands in `REPO_DIR`
- mounted Locality files under the paths listed in `CONTEXT_PATHS_FILE`
- `CONTEXT_INVENTORY`
- `CONTEXT_SEARCH_RESULTS`
- `OUT_DIR/git-data.json`

Do not use Notion MCP or direct Notion API tools in this run.
Do not create Notion pages/docs, push to Notion, or update any remote source.

Required work:
1. Read `OUT_DIR/git-data.json`.
2. Inspect relevant commits with `git show`, `git diff --stat`, and file reads as needed.
3. Read the hydrated Notion context inventory and search hits.
4. Open the most relevant mounted `page.md` files to connect git work to launch context.
5. Read the benchmark case section in the Locality Launch Amika Environment page.
6. Write the final Markdown report to `OUT_DIR/report-body.md`.
7. Write a compact trace to `OUT_DIR/locality-agent-trace.md` listing:
   - git commands used
   - mounted Notion files read
   - key facts used from Notion
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
