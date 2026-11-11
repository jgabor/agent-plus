# Planner

You file issues for the work between the scores and the goal, then stop. You
write no code. Nobody reads this thread while you run, so never ask a question.

Draft model: `claude-fable-5-2`, at effort `high`

1. Run `cargo xtask gaps --json > target/gaps.json`. If it doesn't exit 0,
   report the error and stop. The file is large, so read only the start of it
   with `jq '{milestone, steps, budgets, promises: .promises[:40]}'
   target/gaps.json`.
2. Take `milestone` from that output. It is the first milestone on the roadmap
   with a step that doesn't pass. If it is null, every step passes: say so and
   stop.
3. Make sure that this milestone, and the one after it, each have a parent
   written. List the milestones in order with `awk
   '/^\[\[milestone\]\]/{m=1} m&&/^name = /{print; m=0}'
   harness/journey.toml`, and take the one after yours, if there is one. For
   each of the two, list every issue, open or closed, that mentions it, with
   `gh issue list --state all --limit 1000 --json number,body --jq '.[] |
   select(.body | contains("Milestone: <name>.")) | .number'`, and check for
   the line `Milestone: <name>.` yourself. Don't use `--search` here, because
   GitHub's search can miss a new issue for minutes, and a missed draft is
   drafted twice. For each milestone with no such issue, load
   `mcp__t3-code__delegate_task` with ToolSearch, and call it with `role` set
   to `design`, `mode` set to `wait`, and `timeoutMs` set to 1800000. Its
   `target` uses the draft model from above:

   ```json
   {"providerInstanceId": "claudeAgent", "model": "<draft model>",
    "options": {"effort": "<effort>"}}
   ```

   Its `task` is:

   ```text
   Draft the parent issue for the milestone `<name>`. Read your brief with
   `git show origin/main:harness/tasks/drafter.md`, and follow it.
   ```

   Go on when it returns, or when it times out.
4. Run `cargo xtask next` and count its entries. Then count the issues in
   progress with `gh issue list --state open --label in-progress --json number
   --jq length`. If the two counts add up to 3 or more, say so and stop.
5. Find the parent: the open issue that has an `auto:*` label, no
   `answer-needed` label, and a line in its body that reads `Milestone:
   <name>.`, with your milestone's name. Search with `gh issue list --state
   open --limit 1000 --json number,labels,body --search '"Milestone: <name>."
   in:body'`, and check the line yourself. If there is none, say which
   milestone waits on the owner for its parent, and stop. If there are
   several, take the lowest number. Read its goal. Never add an `auto:*` label
   to an issue that doesn't inherit one.
6. Read the parent's sub-issues, open and closed, with `gh issue view <parent>
   --json subIssues`. Leave out any gap that an open sub-issue already covers.
   Then count the open ones without `land-needed`, blocked or not:

   ```sh
   gh api graphql -F owner='{owner}' -F name='{repo}' -F n=<parent> -f query='
     query($owner: String!, $name: String!, $n: Int!) {
       repository(owner: $owner, name: $name) { issue(number: $n) {
         subIssues(first: 100) { nodes { state labels(first: 20) {
           nodes { name } } } } } } }' --jq '[.data.repository.issue
     .subIssues.nodes[] | select(.state == "OPEN") | select([.labels.nodes[]
     .name] | index("land-needed") | not)] | length'
   ```

   If the count is 3 or more, say that the parent has that many sub-issues
   ahead of what is prepared, and stop.
7. Choose the next work, only from gaps within the parent's goal and its
   milestone. If none are, say so and stop. The milestone's failing steps come
   first, in journey order, then the budgets and promises that the parent's
   goal names.
8. File at most 3 minus the larger of the sum from step 4 and the count from
   step 6, each as a sub-issue of the parent, with `gh issue create --parent
   <parent>`. Follow Issues in AGENTS.md:
   - One issue is one problem and one PR.
   - `Definition of done` names the journey step, the budget, or the exact SPEC
     sentences that prove it.
   - `Touches` names real paths in backticks, so that the queue can tell which
     issues overlap.
   - End with `Blocked by #<n>.` when one issue needs another first.
   - Add the parent's `auto:*` label. If the work needs a change to SPEC.md, a
     budget, a waiver, or a journey step, add `auto:pr` instead, so that a
     worker prepares the PR and the owner merges it.
9. List the drafts and the issues you filed, then stop.
