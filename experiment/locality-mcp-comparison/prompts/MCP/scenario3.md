You are running the Notion-MCP launch-readiness comparison benchmark.

User prompt:
We are considering whether Locality is ready for a broader launch. Review recent engineering work and relevant internal context, then draft a launch-readiness assessment with evidence, risks, blockers, and the next validation steps. Do not publish it remotely.

Use these context sources:
- local git commands in `REPO_DIR`
- Notion MCP tools for Notion search/read context

Do not read mounted Locality Notion files under `/home/amika/notion`.
Do not use `loc` commands.
Do not create Notion pages/docs, push or update Notion, or update any remote source.

Required work:
1. Inspect relevant recent engineering work with local git commands as needed.
2. Use Notion MCP to search/read launch readiness, engineering, risk, blocker, validation, and internal project context.
3. Identify evidence, risks, blockers, and next validation steps.
4. Write the final Markdown assessment to `OUT_DIR/notion-mcp-report-body.md`.
5. Write a compact trace to `OUT_DIR/notion-mcp-agent-trace.md` listing:
   - git commands used
   - Notion MCP searches/calls attempted
   - Notion pages or excerpts used
   - limitations

Report format:

# Locality Launch Readiness Assessment

## Assessment

## Evidence

## Risks

## Blockers

## Next Validation Steps

The assessment should be concise, specific, and grounded in evidence. If a claim cannot be verified from git or Notion MCP context, say so.
