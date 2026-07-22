You are running the Notion-MCP launch-readiness comparison benchmark.

User prompt:
We are considering whether Locality is ready for a broader launch. Review recent engineering work and relevant internal context, then draft a launch-readiness assessment with evidence, risks, blockers, and the next validation steps. Do not publish it remotely.

Use these context sources:
- local git commands in `REPO_DIR`
- `OUT_DIR/git-data.json`
- Notion MCP tools for Notion search/read context

Do not read mounted Locality Notion files under `/home/amika/notion`.
Do not use `loc` commands.
Do not push or update Notion or any remote source in this run.

Required work:
1. Read `OUT_DIR/git-data.json`.
2. Inspect relevant recent engineering work with local git commands as needed.
3. Use Notion MCP to search/read launch readiness, engineering, risk, blocker, validation, and internal project context.
4. Identify evidence, risks, blockers, and next validation steps.
5. Write the final Markdown assessment to `OUT_DIR/notion-mcp-report-body.md`.
6. Write a compact trace to `OUT_DIR/notion-mcp-agent-trace.md` listing:
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
