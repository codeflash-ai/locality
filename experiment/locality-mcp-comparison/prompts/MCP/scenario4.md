You are running the Notion-MCP launch-readiness comparison benchmark.

User prompt:
Prepare today's engineering update for the team. Look at recent repository work and any relevant company context you can access. Summarize what changed, why it matters, risks, blockers, and suggested next actions. Write the result as a Markdown draft. Do not publish it remotely.

Use these context sources:
- local git commands in `REPO_DIR`
- `OUT_DIR/git-data.json`
- Notion MCP tools for Notion search/read context

Do not read mounted Locality Notion files under `/home/amika/notion`.
Do not use `loc` commands.
Do not push or update Notion or any remote source in this run.

Required work:
1. Read `OUT_DIR/git-data.json`.
2. Inspect recent repository work with local git commands as needed.
3. Use Notion MCP to search/read relevant company, launch, engineering, risk, blocker, and team context.
4. Summarize what changed, why it matters, risks, blockers, and suggested next actions.
5. Write the final Markdown draft to `OUT_DIR/notion-mcp-report-body.md`.
6. Write a compact trace to `OUT_DIR/notion-mcp-agent-trace.md` listing:
   - git commands used
   - Notion MCP searches/calls attempted
   - Notion pages or excerpts used
   - limitations

Report format:

# Engineering Update

## What Changed

## Why It Matters

## Risks And Blockers

## Suggested Next Actions

## Evidence Notes

The update should be concise, specific, and grounded in evidence. If a claim cannot be verified from git or Notion MCP context, say so.
