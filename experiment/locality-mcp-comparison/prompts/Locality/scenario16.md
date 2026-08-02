Draft internal release notes for the next Locality build from the evidence that
is actually available. Use Slack discussions, Linear issue status, Notion launch
or release docs, and recent repository work to decide what can be stated as done,
what should be framed as experimental, and what should be omitted.

Use the filesystem at `{{SANDBOX_HOME}}/Locality` for Slack, Linear, and Notion context, and use
the codebase at `{{SANDBOX_HOME}}/workspace/locality` for commit, diff, and test evidence.

Do not use Notion MCP, Linear MCP, Slack MCP, direct Notion/Linear/Slack APIs, or
browser automation in this run. Do not create a release, create docs, post
messages, close issues, push changes, or update any remote source.

Write the final Markdown report to `{{AGENT_REPORT_PATH}}`.

Report format:

# Draft Internal Release Notes

## Highlights

## Fixes And Improvements

## Experimental Or Limited Areas

## Known Issues

## Evidence Notes

## Claims To Avoid

## Gaps And Confidence

The release notes should be concise, specific, and grounded in source paths or
command outputs. If a claim cannot be verified from the available sources, say so.
