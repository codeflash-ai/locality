You are running the Locality-backed comparison benchmark.

User prompt:
i want you to analyze the progress made by different team members from July 15 to July 21 in the year 2025 on the repo codeflash-ai/codeflash, read the linear issues for that time range, read the notion doc named 'Company'. use `loc`, `git`, and `gh` to fulfil the tasks;


Do not use Notion MCP, Linear MCP, or direct Notion/Linear API tools in this run.
Do not create Notion pages/docs, push to Notion, or update any remote source. Write the Markdown body to `REPORT_FILE`.

Required work:
1. Write the final Markdown draft to `REPORT_FILE`.
2. Write a compact trace to `TRACE_FILE` listing:
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
