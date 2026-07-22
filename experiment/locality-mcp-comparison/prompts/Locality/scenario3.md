You are running the Locality-backed launch-readiness benchmark.

User prompt:
We are considering whether Locality is ready for a broader launch. Review recent engineering work and relevant internal context, then draft a launch-readiness assessment with evidence, risks, blockers, and the next validation steps. Do not publish it remotely.

Use locality git and gh for your tasks

Use only these context sources:
- local git commands in `REPO_DIR`
- GitHub context available through `gh`
- mounted Locality files under the paths listed in `CONTEXT_PATHS_FILE`
- `CONTEXT_INVENTORY`
- `CONTEXT_SEARCH_RESULTS`

Do not use Notion MCP or direct Notion API tools in this run.
Do not create Notion pages/docs, push to Notion, or update any remote source.

Required work:
1. Inspect relevant recent engineering work with git, and use `gh` only for repository or issue context that materially affects the assessment.
2. Read the hydrated Notion context inventory and search hits.
3. Open the most relevant mounted `page.md` files to connect engineering work to internal launch context.
4. Identify evidence, risks, blockers, and next validation steps.
5. Write the final Markdown assessment to `OUT_DIR/report-body.md`.
6. Write a compact trace to `OUT_DIR/locality-agent-trace.md` listing:
   - git and gh commands used
   - mounted Notion files read
   - key facts used from Locality context
   - limitations

Report format:

# Locality Launch Readiness Assessment

## Assessment

## Evidence

## Risks

## Blockers

## Next Validation Steps

The assessment should be concise, specific, and grounded in evidence. If a claim cannot be verified from git, gh, or Locality context, say so.
