# Locality Standup Summary Scenario

This scenario runs inside a prepared Amika sandbox that already has stored
Locality connections for Notion, Linear, and Slack. The runner does the setup
work: it checks `loc connections`, mounts each connector as plain files,
hydrates the mounted Markdown, collects recent git evidence from
`codeflash-ai/locality` and `codeflash-ai/locality-internal`, and then starts
Codex.

Codex owns the final workflow after setup: it reads the mounted evidence,
creates a Notion page named `standup-YYYY-MM-DD`, writes the standup summary,
runs `loc diff`, and pushes through `loc push -y`.

## Run

```bash
export NOTION_STANDUP_PARENT_PAGE_ID=<notion-page-id>
scripts/run-amika-standup-summary.sh --sandbox <machine-id>
```

Optional selectors:

```bash
export LINEAR_CONNECTION_ID=linear-default
export SLACK_CONNECTION_ID=slack-default
export NOTION_CONNECTION_ID=notion-default
export SLACK_TYPES=private_channel,im,mpim
```

The default Slack types avoid public-channel auto-join behavior. Include
`public_channel` only when the connected Slack app is allowed to join readable
public channels for this run.

## Evidence Window

The runner computes a UTC last-24-hours window and passes it to Codex. Linear,
Slack, and Notion do not all expose CLI date-window mount flags, so the mount
scope is connector-native and the prompt enforces the time window during
evidence review. Git logs are collected with `git log --since`.

## Outputs

Remote outputs are written under:

```text
$HOME/standup-summary-runs/<run-id>/
  evidence/
    codex-events.jsonl
  mounts/
  final-message.md
  prompt.md
  standup.md
  trace.md
```

The runner also prints a JSON summary of the run paths to stdout. The Notion
page is pushed through the mounted Notion filesystem by Codex.

## Operational Notes

Set explicit `LINEAR_CONNECTION_ID`, `SLACK_CONNECTION_ID`, or
`NOTION_CONNECTION_ID` when the sandbox has more than one active connection for
that connector. The runner fails rather than guessing.

Existing `$RUN_ID` output directories fail fast to avoid mixing evidence from
separate runs.

Existing repository checkouts must have clean working trees and origins matching
`codeflash-ai/locality` and `codeflash-ai/locality-internal`; otherwise the
runner stops before collecting git evidence.

Codex event logs are redacted before being written to the run evidence.
