You are running the Notion-MCP launch-readiness comparison benchmark.

User prompt:
Act like you are preparing a launch gate memo for Locality. Find the relevant project context and recent code changes, decide what is actually proven, what is still unverified, and what should block launch. Produce a concise Markdown memo. Do not push anything.

Use these context sources:
- local git commands in `REPO_DIR`
- Notion MCP tools for Notion search/read context

Do not read mounted Locality Notion files under `/home/amika/notion`.
Do not use `loc` commands.
Do not create Notion pages/docs, push or update Notion, or update any remote source.

Required work:
1. Inspect relevant recent code changes with local git commands as needed.
2. Use Notion MCP to search/read launch, product, risk, blocker, validation, and internal project context.
3. Decide what is proven, what is unverified, and what should block launch.
4. Write the final Markdown memo to `OUT_DIR/notion-mcp-report-body.md`.
5. Write a compact trace to `OUT_DIR/notion-mcp-agent-trace.md` listing:
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
