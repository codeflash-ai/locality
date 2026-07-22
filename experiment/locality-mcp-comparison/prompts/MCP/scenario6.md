You are running the MCP comparison benchmark.

User prompt:
i want you to analyze the progress made by different team members from July 15 to July 21 in the year 2025 on the repo codeflash-ai/codeflash , read the linear issues for that time range and read the notion doc named 'Company' and create a notion doc in the page title `Locality Launch` followed by the current date and time in the title to distinguish it which summarizes your findings.

Use these context sources:
- local git commands in `REPO_DIR`
- `OUT_DIR/git-data.json`
- available MCP tools for Notion and Linear context

Do not read mounted Locality Notion files under `/home/amika/notion`.
Do not use `loc` commands.
Do not push or update Notion or any remote source in this run. For this benchmark, satisfy the requested Notion doc by writing the Markdown body to `OUT_DIR/notion-mcp-report-body.md`.

Required work:
1. Read `OUT_DIR/git-data.json`.
2. Inspect relevant codeflash-ai/codeflash repository progress for July 15 through July 21, 2025 using local git commands as needed.
3. Use MCP tools to read Linear issues for that time range if available.
4. Use MCP tools to find and read the Notion doc named `Company` if available.
5. Summarize progress by team member and connect it to the `Locality Launch` context where possible.
6. Write the final Markdown draft to `OUT_DIR/notion-mcp-report-body.md`.
7. Write a compact trace to `OUT_DIR/notion-mcp-agent-trace.md` listing:
   - git commands used
   - MCP searches/calls attempted
   - Notion pages, Linear issues, or excerpts used
   - limitations

Report format:

# Locality Launch Team Progress Summary

## Summary

## Team Member Progress

## Linear Issue Context

## Company Context

## Launch Relevance

## Risks, Gaps, And Limitations

The draft should be concise, specific, and grounded in evidence. If a claim cannot be verified from git or MCP context, say so.
