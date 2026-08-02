Recommend the top five Locality work items for the next sprint by combining
Slack urgency signals, Linear issue priority/status, and Notion company or
launch priorities. Favor work that reduces launch risk or unblocks daily use.

Use the filesystem at `{{SANDBOX_HOME}}/Locality` for Slack, Linear, and Notion context, and use
the codebase at `{{SANDBOX_HOME}}/workspace/locality` only to verify whether referenced work is
already implemented.

Do not use Notion MCP, Linear MCP, Slack MCP, direct Notion/Linear/Slack APIs, or
browser automation in this run. Do not create docs, post messages, close issues,
push changes, or update any remote source.

Write the final Markdown report to `{{AGENT_REPORT_PATH}}`.

Report format:

# Next Sprint Priority Recommendation

## Recommendation

## Top Five Work Items

## Slack Urgency Signals

## Linear Priority Context

## Notion Strategy Context

## Tradeoffs

## Gaps And Confidence

The recommendation should be concise, specific, and grounded in source paths or
command outputs. If a source is unavailable or no evidence is found, say that directly.
