You are running the Locality-backed comparison benchmark.

User prompt:
i want you to analyze the progress made by different team members from July 15 to July 21 in the year 2025 on the repo codeflash-ai/codeflash, read the linear issues for that time range, read the notion doc named 'Company', and write a Markdown draft titled `Locality Launch` followed by the current date and time in the title to distinguish it which summarizes your findings. use `loc`, `git`, and `gh` to fulfil the tasks; do not rely on notion or linear mcp or api calls.

Use only these context sources:
- `loc` commands
- local git commands in `REPO_DIR`
- GitHub context available through `gh`
- mounted Locality files under the paths listed in `CONTEXT_PATHS_FILE`
- `CONTEXT_INVENTORY`
- `CONTEXT_SEARCH_RESULTS`

Do not use Notion MCP, Linear MCP, or direct Notion/Linear API tools in this run.
Do not create Notion pages/docs, push to Notion, or update any remote source. Write the Markdown body to `OUT_DIR/report-body.md`.

Required work:
1. Inspect relevant codeflash-ai/codeflash repository progress for July 15 through July 21, 2025 using git and `gh` where available.
2. Use `loc` and mounted Locality files to find and read relevant Notion context, including the doc named `Company` if available.
3. Use Locality-accessible context for Linear issue information if available; if Linear issues are not accessible through allowed sources, state that limitation.
4. Summarize progress by team member and connect it to the `Locality Launch` context where possible.
5. Write the final Markdown draft to `OUT_DIR/report-body.md`.
6. Write a compact trace to `OUT_DIR/locality-agent-trace.md` listing:
   - `loc`, git, and gh commands used
   - mounted Locality files read
   - key facts used from Locality context
   - unavailable or unverified Linear/Notion context

Report format:

# Locality Launch Team Progress Summary

## Summary

## Team Member Progress

## Linear Issue Context

## Company Context

## Launch Relevance

## Risks, Gaps, And Limitations

The draft should be concise, specific, and grounded in evidence. If a claim cannot be verified from loc, git, gh, or mounted Locality context, say so.
