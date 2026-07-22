You are running the Notion-MCP launch-readiness comparison benchmark.

User prompt:
Act like you are preparing a launch gate memo for Locality. Find the relevant project context and recent code changes, decide what is actually proven, what is still unverified, and what should block launch. Produce a concise Markdown memo. Do not push anything.

Use these context sources:
- local git commands in `REPO_DIR`
- `OUT_DIR/git-data.json`
- Notion MCP tools for Notion search/read context

Do not read mounted Locality Notion files under `/home/amika/notion`.
Do not use `loc` commands.
Do not push or update Notion or any remote source in this run.

Required work:
1. Read `OUT_DIR/git-data.json`.
2. Inspect relevant recent code changes with local git commands as needed.
3. Use Notion MCP to search/read launch, product, risk, blocker, validation, and internal project context.
4. Decide what is proven, what is unverified, and what should block launch.
5. Write the final Markdown memo to `OUT_DIR/notion-mcp-report-body.md`.
6. Write a compact trace to `OUT_DIR/notion-mcp-agent-trace.md` listing:
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
