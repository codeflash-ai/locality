# Workflow: Summarize Week

Inputs:

- `log.md`
- changed files from mounted sources
- reviewed metrics

Steps:

1. Extract shipped work, user evidence, and decisions.
2. Ignore private or unreviewed facts unless the output remains private.
3. Draft `templates/weekly-update.md`.
4. Add source links for every claim that may be published.
5. Mark unresolved claims as `review_status: draft`.
