You are running the Locality-backed launch-readiness benchmark.

User prompt:
Act like you are preparing a launch gate memo for Locality. Find the relevant project context and recent code changes, decide what is actually proven, what is still unverified, and what should block launch. Produce a concise Markdown memo. Do not push anything.

Use locality, git and gh for your tasks

Do not use Notion MCP or direct Notion API tools in this run.
Do not create Notion pages/docs, push to Notion, or update any remote source.

Required work:
1. Write the final Markdown memo to `OUT_DIR/report-body.md`.
2. Write a compact trace to `OUT_DIR/locality-agent-trace.md` listing:
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
