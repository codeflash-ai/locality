You are running the Locality-backed launch-readiness benchmark.

User prompt:
I need a short standup-style update for Locality based on what changed recently. Please discover the relevant context yourself, connect code changes to product or launch work where possible, and produce a grounded Markdown draft. Do not push or update any remote source.

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
1. Inspect recent code changes with git, and use `gh` only for repository or issue context that materially affects the update.
2. Read the hydrated Notion context inventory and search hits.
3. Open the most relevant mounted `page.md` files to connect code changes to product or launch work.
4. Produce a short standup-style update with grounded evidence.
5. Write the final Markdown draft to `OUT_DIR/report-body.md`.
6. Write a compact trace to `OUT_DIR/locality-agent-trace.md` listing:
   - git and gh commands used
   - mounted Notion files read
   - key facts used from Locality context
   - limitations

Report format:

# Locality Standup Update

## Done

## In Progress

## Risks Or Blockers

## Next

## Evidence Notes

The update should be short, specific, and grounded in evidence. If a claim cannot be verified from git, gh, or Locality context, say so.
