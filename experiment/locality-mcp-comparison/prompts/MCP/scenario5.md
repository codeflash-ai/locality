You are running the Notion-MCP launch-readiness comparison benchmark.

User prompt:
I need a short standup-style update for Locality based on what changed recently. Please discover the relevant context yourself, connect code changes to product or launch work where possible, and produce a grounded Markdown draft. Do not push or update any remote source.

Use these context sources:
- local git commands in `REPO_DIR`
- `OUT_DIR/git-data.json`
- Notion MCP tools for Notion search/read context

Do not read mounted Locality Notion files under `/home/amika/notion`.
Do not use `loc` commands.
Do not push or update Notion or any remote source in this run.

Required work:
1. Read `OUT_DIR/git-data.json`.
2. Inspect recent code changes with local git commands as needed.
3. Use Notion MCP to search/read relevant product, launch, engineering, risk, blocker, and team context.
4. Produce a short standup-style update with grounded evidence.
5. Write the final Markdown draft to `OUT_DIR/notion-mcp-report-body.md`.
6. Write a compact trace to `OUT_DIR/notion-mcp-agent-trace.md` listing:
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
