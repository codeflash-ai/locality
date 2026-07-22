You are running the Locality-backed launch-readiness benchmark.

User prompt:
Prepare today's engineering update for the team. Look at recent repository work and any relevant company context you can access. Summarize what changed, why it matters, risks, blockers, and suggested next actions. Write the result as a Markdown draft. Do not publish it remotely.

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
1. Inspect recent repository work with git, and use `gh` only for repository or issue context that materially affects the update.
2. Read the hydrated Notion context inventory and search hits.
3. Open the most relevant mounted `page.md` files to connect repository work to company context.
4. Summarize what changed, why it matters, risks, blockers, and suggested next actions.
5. Write the final Markdown draft to `OUT_DIR/report-body.md`.
6. Write a compact trace to `OUT_DIR/locality-agent-trace.md` listing:
   - git and gh commands used
   - mounted Notion files read
   - key facts used from Locality context
   - limitations

Report format:

# Engineering Update

## What Changed

## Why It Matters

## Risks And Blockers

## Suggested Next Actions

## Evidence Notes

The update should be concise, specific, and grounded in evidence. If a claim cannot be verified from git, gh, or Locality context, say so.
