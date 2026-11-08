# Planner

You file issues for the work between the scores and the goal, then stop. You
write no code. Nobody reads this thread while you run, so never ask a question.

1. Run `cargo xtask next` and count its entries. Then count the issues in
   progress with `gh issue list --state open --label in-progress --json number
   --jq length`. If the two counts add up to 3 or more, say so and stop.
2. Find the parent: the lowest-numbered open issue that has an `auto:*` label
   and no `Part of` line in its body. If there is none, say so and stop. Never
   add an `auto:*` label to an issue that doesn't inherit one.
3. Run `cargo xtask gaps --json`. If it doesn't exit 0, report the error and
   stop.
4. Read the parent's children, open and closed, with `gh issue list --state all
   --search '"Part of #<parent>" in:body'`. Leave out any gap that an open
   child already covers.
5. Choose the next work. Failing steps come first, then budgets, then promises.
   Among steps, follow the order of risk in HARNESS.md, under Planning.
6. File issues until the counts from step 1 add up to 3. Follow Issues in
   AGENTS.md:
   - One issue is one problem and one PR.
   - `Done when` names the journey step, the budget, or the exact SPEC
     sentences that prove it.
   - `Touches` names real paths in backticks, so that the queue can tell which
     issues overlap.
   - End with `Part of #<parent>.`, and add `Blocked by #<n>.` when one issue
     needs another first.
   - Add the parent's `auto:*` label. If the work needs a change to SPEC.md, a
     budget, a waiver, or a journey step, add no `auto:*` label.
7. List the issues you filed, then stop.
