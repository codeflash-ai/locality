# Natural Retrieval Experiment Design

Draft date: July 22, 2026

Status: alignment draft. Do not run this as-is until we agree on scenarios,
prompt count, and publish behavior.

## Why The Current Benchmark Is Not Enough

The current launch-readiness benchmark is useful for profiling, but it is too
directive for product evidence.

Current Locality path:

- runner locates the target Notion URL;
- runner pulls the target page;
- runner locates and recursively hydrates a known context URL;
- runner gives the agent `CONTEXT_PATHS_FILE`, `CONTEXT_INVENTORY`, and
  `CONTEXT_SEARCH_RESULTS`;
- prompt tells the agent which hydrated context inventory/search files to read.

Current MCP path:

- prompt tells the agent specific Notion search themes;
- agent uses Notion MCP search/fetch to gather context.

That answers:

```text
If context is prepared, can the agent produce a report?
```

The next experiment should answer:

```text
Given a natural work request, can the agent discover relevant company context,
choose useful tools, produce a grounded output, and do it with less cost,
latency, and manual setup?
```

## Hypotheses

### H1: Locality Improves The Agent Work Substrate

Once useful content is available locally, the agent can use normal shell/file
operations with fewer MCP calls and fewer tokens.

Expected signal:

- fewer remote tool calls;
- fewer input tokens;
- more direct evidence paths;
- comparable or better output quality;
- more transparent evidence trail through local files.

### H2: Current Locality Retrieval Is Not Yet Strong Enough

If the agent is not given URLs or context paths, it may fall back to `find`,
`grep`, and broad file reads because `loc search` is still not rich enough over
body chunks, snippets, and relevance.

Expected signal:

- few or no `loc search` calls;
- repeated shell `find`/`grep` scans;
- missed relevant context that exists in mounted files;
- high time spent in discovery despite local data.

### H3: Prehydration Should Be Measured But Not Overweighted

Prehydration is valuable when it happens before the user task or is
relevance-guided. It is not fair to treat all prehydration as agent-run time, but
it is also not fair to ignore synchronous task-time hydration.

Experiment implication:

- measure prep/cache state separately;
- measure agent retrieval/execution separately;
- record whether the run was cold, warm metadata, or warm body cache.

### H4: MCP Will Spend More On Repeated Remote Retrieval

MCP can be flexible, but natural retrieval will likely require multiple search
and fetch calls, more token traffic, and more source-API dependency.

Expected signal:

- more MCP tool calls;
- more input tokens;
- more repeated context transfer;
- output may be more exhaustive if MCP search finds pages Locality has not
  indexed or hydrated.

## Strategies To Compare

### Strategy A: Locality Natural

The agent receives a natural task. It has access to:

- local git checkout;
- mounted Locality source folders;
- `loc` CLI;
- installed Locality guidance semantics.

The agent does not receive:

- Notion URLs;
- known page names;
- precomputed context path files;
- precomputed inventory files;
- Notion MCP tools.

Prompt guidance should say:

```text
Use Locality-connected files and the `loc` CLI when helpful. For discovery,
prefer `loc search <query>` first, then inspect mounted files. Use `loc info`,
`loc status`, and `loc diff` when you need source or sync state. Do not use
Notion MCP or direct Notion APIs.
```

This intentionally reflects the product guidance installed for local agents.

### Strategy B: Notion MCP Natural

The agent receives the same natural task. It has access to:

- local git checkout;
- Notion MCP tools.

The agent does not receive:

- mounted Locality files;
- `loc` CLI;
- precomputed Locality context paths;
- known page URLs.

Prompt guidance should say:

```text
Use Notion MCP for company context and local git commands for repository
context. Do not read Locality-mounted files and do not use `loc`.
```

### Optional Strategy C: Locality Files-Only Ablation

This is not the main product path, but it helps answer whether `loc search` and
Locality guidance are adding value beyond mounted files.

The agent receives:

- mounted Locality source folders;
- local git checkout.

The agent does not use:

- `loc`;
- Notion MCP.

If this performs as well as Strategy A, then `loc search/info/status` are not yet
pulling enough weight for retrieval. If Strategy A is better, the CLI guidance is
creating measurable value.

Recommended: run this ablation only for one scenario in the pilot.

## Cache Conditions

We should not mix all cache states into one number.

### Warm Metadata, Natural Body Retrieval

Recommended primary condition.

Before the benchmark:

- connections and mounts exist;
- metadata/index state is allowed to exist from normal product use;
- no benchmark runner gives the agent exact context paths;
- the agent may call `loc search`, open files, and trigger hydration naturally.

This matches the product vision: Locality has a local knowledge cache, but the
agent still chooses context.

### Warm Body Cache

Secondary condition.

Before the benchmark:

- relevant content is already hydrated from earlier use;
- agent still receives only the natural prompt.

This measures the best-case Locality product path after daily use.

### Cold Setup

Diagnostic only.

Before the benchmark:

- mount exists but target context is not hydrated;
- measure locate/resolve/hydration as setup.

This is useful for performance engineering, but should not dominate the product
comparison because Locality's strategic goal is a warm local cache.

## Pilot Scenario Matrix

To keep cost reasonable, start with two scenarios, two prompt variants, two
strategies, two independent repeats.

```text
2 scenarios x 2 variants x 2 strategies x 2 repeats = 16 agent runs
```

Add the files-only ablation for one scenario if we want a CLI-vs-grep signal:

```text
+ 2 variants x 1 strategy x 2 repeats = 4 extra runs
```

Total pilot with ablation: 20 runs.

## Natural Scenarios

### Scenario 1: Daily Engineering Update

Intent:

Find recent repository work, connect it to company planning/standup context, and
draft a concise update.

Prompt A:

```text
Prepare today's engineering update for the team. Look at recent repository work
and any relevant company context you can access. Summarize what changed, why it
matters, risks, blockers, and suggested next actions. Write the result as a
Markdown draft. Do not publish it remotely.
```

Prompt B:

```text
I need a short standup-style update for Locality based on what changed recently.
Please discover the relevant context yourself, connect code changes to product
or launch work where possible, and produce a grounded Markdown draft. Do not
push or update any remote source.
```

Expected evidence classes:

- recent git commits;
- standup or planning pages;
- launch or product context pages;
- known reliability or platform risks.

### Scenario 2: Launch Readiness Review

Intent:

Assess whether recent work changes launch readiness and identify remaining
release blockers.

Prompt A:

```text
We are considering whether Locality is ready for a broader launch. Review recent
engineering work and relevant internal context, then draft a launch-readiness
assessment with evidence, risks, blockers, and the next validation steps. Do not
publish it remotely.
```

Prompt B:

```text
Act like you are preparing a launch gate memo for Locality. Find the relevant
project context and recent code changes, decide what is actually proven, what is
still unverified, and what should block launch. Produce a concise Markdown memo.
Do not push anything.
```

Expected evidence classes:

- launch planning pages;
- install/distribution docs;
- safety/review/push guidance;
- platform provider and live mode context;
- recent code/test commits.

### Scenario 3: Sync Reliability Bug Triage

Run after the pilot if the first two are stable.

Prompt A:

```text
A user reports that a Notion-mounted page did not sync correctly and that review
state stayed confusing after manual edits. Investigate likely product areas from
recent code and internal context. Draft a technical triage note with suspected
causes, missing evidence, and tests to add. Do not publish it remotely.
```

Prompt B:

```text
Please investigate Locality sync reliability around mounted Notion pages,
conflicts, review state, and Live Mode. Use whatever connected company context
and recent repo work are relevant. Produce a grounded triage memo with concrete
next tests. Do not push anything.
```

Expected evidence classes:

- sync model docs;
- live mode docs;
- conflict/review docs;
- recent commits around pull/push/review/daemon;
- relevant standup or user-report context.

## Prompt Rules

All strategies should share the same natural task text except for tool-access
rules.

Allowed:

- "Use the tools available to you."
- "Discover relevant context yourself."
- "Do not publish remotely."
- "Write Markdown output to the configured output file."

Avoid:

- exact Notion URLs;
- exact page titles;
- exact mounted paths;
- precomputed context inventory;
- search keyword lists that encode the answer;
- telling the agent which specific file to read.

## Outputs Per Run

Each run should write:

```text
experiment/runs/<run-id>/
  scenario.json
  prompt.md
  strategy.md
  report-body.md
  evidence-manifest.json
  agent-trace.md
  codex-events.jsonl
  codex-events.tsv
  codex-summary.json
  codex-transcript.md
  agent-profile/
    summary.md
    summary.json
    combined.speedscope.json
  locality-agent-locality-trace.jsonl
  locality-agent-locality-trace-summary.json
  metrics.tsv
```

For paired strategy comparison:

```text
experiment/runs/<batch-id>/
  comparisons/
    scenario-1-prompt-a-repeat-1.md
    scenario-1-prompt-a-repeat-2.md
  batch-summary.tsv
  batch-summary.md
```

## Evidence Manifest

Every agent should be required to write a machine-readable evidence manifest.
This reduces subjective interpretation after the run.

Suggested shape:

```json
{
  "task": "launch_readiness_review",
  "strategy": "locality-natural",
  "evidence": [
    {
      "kind": "git_commit",
      "id": "6aa3e9bd",
      "reason": "File Provider initial discovery behavior"
    },
    {
      "kind": "locality_file",
      "path": "/home/amika/notion/Go To Market/.../page.md",
      "reason": "launch checklist and open blockers"
    },
    {
      "kind": "notion_mcp_page",
      "title": "Locality Launch",
      "reason": "launch checklist and open blockers"
    }
  ],
  "limitations": [
    "No CI metadata inspected",
    "No remote push attempted"
  ]
}
```

The manifest lets us score:

- relevant evidence count;
- unsupported claims;
- missing expected evidence;
- source diversity;
- whether the agent found context without being spoon-fed.

## Tool-Use Metrics

The experiment should report these per strategy and run:

### General

- total wall time;
- agent wall time;
- input tokens;
- cached input tokens;
- output tokens;
- reasoning output tokens;
- command/tool count;
- errors and retries;
- output word count.

### Locality

- `loc search` count and duration;
- `loc info` count and duration;
- `loc status` count and duration;
- `loc locate` / future `loc locate --offline` count and duration;
- `loc pull` count and duration;
- `loc diff` count and duration;
- direct file reads count;
- `find` / `rg` / `grep` count;
- hydrated files touched;
- Locality trace spans emitted by agent-run `loc` commands.

### MCP

- Notion MCP search count;
- Notion MCP fetch/read count;
- Notion MCP errors/retries;
- remote pages read;
- duplicated or repeated page fetches;
- tool-result token volume if available.

### Output Quality

Manual or semi-automatic rubric:

- factual grounding;
- correct recent git summary;
- relevant company context found;
- missed important context;
- unsupported claims;
- actionable recommendations;
- concise enough for the target workflow;
- source transparency.

## Profiler Work Needed Before Running

`experiment/agent-conversation-profile.mjs` has useful concepts:

- activity grouping;
- tool wait grouping;
- `bash_loc` vs `bash_other`;
- Perfetto, Speedscope, SnakeViz, and folded-stack outputs;
- summary Markdown.

However, it currently does not profile the timestamped Codex JSONL from the
comparison runner because those records use `observed_at_ms`, and the script
only recognizes `timestamp`, `created_at`, `time`, and `ts`.

Before the next experiment, update the profiler or normalize inputs:

1. Add `observed_at_ms` to supported timestamp keys.
2. Recognize nested Codex records shaped like:

   ```json
   {"observed_at_ms": 123, "event": {"type": "item.started", "item": {...}}}
   ```

3. Classify Codex `command_execution` as shell tool calls.
4. Extract command text from `event.item.command`.
5. Categorize shell commands:
   - `loc_search`;
   - `loc_info`;
   - `loc_status`;
   - `loc_locate`;
   - `loc_pull`;
   - `loc_diff`;
   - `git`;
   - `rg_grep_find`;
   - `file_read`;
   - `other_shell`.
6. Categorize MCP calls by server/tool name.
7. Emit per-run and batch-level tool statistics.

This gives a clearer answer to:

```text
Did the Locality agent actually use Locality, or did it just grep files?
```

## Expected Bottlenecks To Watch

### Locality

Likely bottlenecks:

- poor body-search coverage if hydrated shadows are not indexed well;
- agent falling back to broad filesystem scans;
- `loc search` returning metadata-only results without useful snippets;
- `loc locate` doing remote repair when the user did not give a URL;
- recursive hydration if the agent or runner pulls directories;
- missing `rg` in sandbox, forcing slower `find`/`sed` patterns;
- insufficient agent guidance around when to use `loc info/status/search`.

Important nuance:

`loc info` and `loc status` are not discovery tools. They are context and safety
tools once the agent has a candidate path. The main discovery primitive should
be `loc search` now and `loc context build` later.

### MCP

Likely bottlenecks:

- repeated search/fetch calls;
- Notion search API limitations;
- high token volume from tool results;
- remote latency and content-filter stream interruptions;
- no durable local working set unless the agent creates one manually.

## Success Criteria

Locality is meaningfully better if:

- output quality is equal or better;
- the agent finds relevant context without exact URLs;
- fewer tokens are used;
- fewer remote tool calls are used;
- the evidence trail is clearer;
- the agent can write a local draft and inspect sync state;
- setup cost is either amortized by warm cache or visibly reduced by better
  retrieval.

Locality is not yet better if:

- agent relies mostly on blind `find`/`grep`;
- `loc search` is rarely used or unhelpful;
- relevant context is missed because it was not prehydrated;
- setup/hydration dominates each natural task;
- MCP finds better context with fewer manual hints.

## Recommended Pilot

Start with:

```text
Scenario 1: Daily Engineering Update
Scenario 2: Launch Readiness Review
Prompt variants: A and B for each
Strategies: Locality Natural and Notion MCP Natural
Repeats: 2 independent runs per prompt/strategy
Cache condition: warm metadata, natural body retrieval
Publish: no remote push
```

That gives 16 runs and should be enough to expose whether Locality's natural
retrieval loop works.

Then add:

```text
Scenario 1 only
Strategy: Locality Files-Only Ablation
Repeats: 2
```

This tells us whether `loc search/info/status` are adding value beyond a mounted
folder and shell search.

## Alignment Questions

1. Should the pilot include the files-only ablation now, or keep the first run to
   Locality Natural vs Notion MCP Natural?
2. Should the Locality strategy be allowed to trigger hydration by opening files
   and running `loc pull`, or should task-time hydration be disabled for the
   first pass?
3. Should we publish any output page to Notion, or keep all outputs local until
   the scoring rubric is stable?
4. Do we want to use only `gpt-5.6-luna`, or compare Luna vs Terra after the
   benchmark harness is stable?
5. Should expected evidence pages be defined privately in a scoring file, so the
   prompt remains natural but scoring can check recall?
