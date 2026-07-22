You are running the Locality-backed launch-readiness benchmark.

User prompt:
Act like you are preparing a launch gate memo for Locality. Find the relevant project context and recent code changes, decide what is actually proven, what is still unverified, and what should block launch. Produce a concise Markdown memo. Do not push anything.

Use locality git and gh for your tasks

Use only these context sources:
- local git commands in `REPO_DIR`
- GitHub context available through `gh`
- mounted Locality files under the paths listed in `CONTEXT_PATHS_FILE`
- `CONTEXT_INVENTORY`
- `CONTEXT_SEARCH_RESULTS`
- `OUT_DIR/git-data.json`

Do not use Notion MCP or direct Notion API tools in this run.
Do not push to Notion or update any remote source.

Required work:
1. Read `OUT_DIR/git-data.json`.
2. Inspect relevant recent code changes with git, and use `gh` only for repository or issue context that materially affects the memo.
3. Read the hydrated Notion context inventory and search hits.
4. Open the most relevant mounted `page.md` files to connect code changes to launch context.
5. Decide what is proven, what is unverified, and what should block launch.
6. Write the final Markdown memo to `OUT_DIR/report-body.md`.
7. Write a compact trace to `OUT_DIR/locality-agent-trace.md` listing:
   - git and gh commands used
   - mounted Notion files read
   - key facts used from Locality context
   - limitations

Report format:

# Locality Launch Gate Memo

## Recommendation

## Evidence Reviewed

## Proven

## Unverified

## Launch Blockers

## Required Validation

The memo should be concise, specific, and grounded in evidence. If a claim cannot be verified from git, gh, or Locality context, say so.
