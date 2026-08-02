Validate the claim: "Live Mode is ready for internal daily use." Check whether
Slack discussions, Linear issues, Notion docs, and recent repository work support
that claim, contradict it, or leave it unproven.

Use the Slack, Linear, and Notion MCP servers you have access to, and use the
codebase at `{{SANDBOX_HOME}}/workspace/locality` for implementation and test evidence.

Do not use direct Notion/Linear/Slack APIs or browser automation in this run. Do
not read mounted Locality files under `{{SANDBOX_HOME}}/Locality`. Do not create docs, post
messages, close issues, push changes, or update any remote source.

Write the final Markdown report to `{{AGENT_REPORT_PATH}}`.

Report format:

# Live Mode Daily-Use Readiness Check

## Verdict

## Supporting Evidence

## Contradicting Evidence

## Slack Findings

## Linear Findings

## Notion Findings

## Repo Findings

## Required Validation Before Broader Use

## Gaps And Confidence

The report should be concise, specific, and grounded in source paths, MCP results,
or command outputs. If a claim cannot be verified from the available sources, say
so.
