Create a support handoff for Locality users who hit sync or push problems after
editing mounted Notion files. Use Slack reports, Linear issues, and Notion docs
to distinguish safe user recovery steps from engineering-only investigation.

Use the Slack, Linear, and Notion MCP servers you have access to, and use the
codebase at `{{SANDBOX_HOME}}/workspace/locality` when implementation details clarify the
recovery path.

Do not use direct Notion/Linear/Slack APIs or browser automation in this run. Do
not read mounted Locality files under `{{SANDBOX_HOME}}/Locality`. Do not create docs, post
messages, close issues, push changes, or update any remote source.

Write the final Markdown report to `{{AGENT_REPORT_PATH}}`.

Report format:

# Sync Problem Support Handoff

## Support Summary

## Symptoms To Ask About

## Safe User Recovery Steps

## Do Not Recommend

## Engineering Escalation Criteria

## Slack Evidence

## Linear Evidence

## Notion Evidence

## Gaps And Confidence

The handoff should be concise, specific, and grounded in source paths, MCP
results, or command outputs. If a source is unavailable or no evidence is found,
say that directly.
