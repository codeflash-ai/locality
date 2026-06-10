# Deviations From `plan.md`

`plan.md` is authoritative. Any intentional implementation deviation must be documented here before it becomes part of the codebase.

## Active Deviations

None.

## Open Design Questions Carried From `plan.md`

- Hydration aggressiveness remains configurable. The code defaults to the 90-day policy and no eager-under-size threshold.
- `_view.csv` remains read-only unless the plan is updated.
- Journals now store core shadow preimages and apply effects for undo planning; native connector preimages remain undecided.
- `afs` remains the working title.
