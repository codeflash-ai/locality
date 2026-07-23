You are running the Locality-backed launch-readiness benchmark.

User prompt:
We are considering whether Locality is ready for a broader launch. Review recent engineering work and relevant internal context, then draft a launch-readiness assessment with evidence, risks, blockers, and the next validation steps. Do not publish it remotely.

Use locality, git and gh for your tasks.

Do not use Notion MCP or direct Notion API tools in this run.
Do not create Notion pages/docs, push to Notion, or update any remote source.

Required work:
1. Write the final Markdown assessment to `REPORT_FILE`.
2. Write a compact trace to `TRACE_FILE` listing:
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
