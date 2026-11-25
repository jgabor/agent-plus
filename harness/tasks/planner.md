# Planner

You file issues for the work between the scores and the goal, then stop. You
write no code. Nobody reads this thread while you run, so never ask a question.

Draft model: `claude-fable-5-2`, at effort `high`

1. Adopt the issues that hold up labeled work. Skip this step unless #840 is
   open with an `auto:*` label. List each open issue that has no parent and
   no label, that the owner's account filed, and that an open issue with an
   `auto:*` label names in its `Blocked by` line:

   ```sh
   gh issue list --state open --limit 1000 \
     --json number,title,body,labels,author,parent > target/open.json
   jq -r --arg owner "$(gh repo view --json owner --jq .owner.login)" '
     [.[] | select(any(.labels[].name; startswith("auto:"))) | .body
       | splits("\r?\n") | sub("^\\s+"; "") | select(startswith("Blocked by"))
       | scan("#([0-9]+)") | .[0] | tonumber] as $named
     | .[] | select(.parent == null and .labels == [] and
       .author.login == $owner and IN(.number; $named[]))
     | "\(.number) \(.title)"' target/open.json
   ```

   Adopt each one under #840 as step 9 says, with the same checks, except
   that it needs no gap and may be outside the milestone: fix its body first,
   make it a sub-issue, and add #840's label only if that worked. It is apart
   from the milestone's limits, so the counts in steps 7 and 10 leave it out.
   Name each one you adopt when you stop, at whatever step.
2. Run `cargo xtask gaps --json > target/gaps.json`. If it doesn't exit 0,
   report the error and stop. The file is large, so read only the start of it
   with `jq '{milestone, steps, budgets, promises: .promises[:40]}'
   target/gaps.json`.
3. Take `milestone` from that output. It is the first milestone on the roadmap
   with a step that doesn't pass. If it is null, every step passes: say so and
   stop.
4. Make sure that this milestone, and the one after it, each have a parent
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
5. Run `cargo xtask next` and count its entries. Then count the issues in
   progress with `gh issue list --state open --label in-progress --json number
   --jq length`. If the two counts add up to 5 or more, say so and stop.
6. Find the parent: the open issue that has an `auto:*` label, no
   `answer-needed` label, and a line in its body that reads `Milestone:
   <name>.`, with your milestone's name. Search with `gh issue list --state
   open --limit 1000 --json number,labels,body --search '"Milestone: <name>."
   in:body'`, and check the line yourself. If there is none, say which
   milestone waits on the owner for its parent, and stop. If there are
   several, take the lowest number. Read its goal. Never add an `auto:*` label
   to an issue that doesn't inherit one.
7. Read the parent's sub-issues, open and closed, with `gh issue view <parent>
   --json subIssues`. Leave out any gap that an open sub-issue already covers.
   Then count the open ones without `land-needed`, blocked or not:

   ```sh
   gh api graphql -F owner='{owner}' -F name='{repo}' -F n=<parent> -f query='
     query($owner: String!, $name: String!, $n: Int!) {
       repository(owner: $owner, name: $name) { issue(number: $n) {
         subIssues(first: 100) { nodes { state labels(first: 100) {
           nodes { name } } } } } } }' --jq '[.data.repository.issue
     .subIssues.nodes[] | select(.state == "OPEN") | select([.labels.nodes[]
     .name] | index("land-needed") | not)] | length'
   ```

   If the count is 3 or more, say that the parent has that many sub-issues
   ahead of what is prepared, and stop.
8. Choose the next work, only from gaps within the parent's goal and its
   milestone. If none are, say so and stop. The milestone's failing steps come
   first, in journey order, then the budgets and promises that the parent's
   goal names.
9. For each gap you chose, up to the limit in step 10, look for an open issue
   that already covers it, among those with no parent and no label that says
   who acts next:

   ```sh
   gh issue list --state open --limit 1000 --json number,title,labels,parent \
     --jq '.[] | select(.parent == null) | select([.labels[].name] | any(
     startswith("auto:") or . == "answer-needed" or . == "land-needed" or
     . == "in-progress") |
     not) | "\(.number) \(.title)"'
   ```

   Read each likely one. Adopt it, in place of filing, only if its goal is
   that gap, its `Definition of done` names checks, not a decision of the
   owner's, and `gh issue view <n> --json subIssues` shows no sub-issues,
   because the queue never offers a parent. If it has a comment from the
   owner that defers it, adopt it only once what that comment waits for has
   happened. Never adopt an issue outside the parent's goal and milestone.

   To adopt it, first edit its body where it is out of date, so that
   `Touches` names real paths and `Blocked by` names what it needs first. The
   queue reads both as soon as the issue has its label. Then make it a
   sub-issue, and add the label only if that worked:

   ```sh
   gh api -X POST 'repos/{owner}/{repo}/issues/<parent>/sub_issues' \
     -F sub_issue_id="$(gh api 'repos/{owner}/{repo}/issues/<n>' --jq .id)" &&
     gh issue edit <n> --add-label <the parent's auto:* label>
   ```
10. File or adopt at most the smaller of 5 minus the sum from step 5, and 3
    minus the count from step 7. File each new one as a sub-issue of the
    parent, with `gh issue create --parent <parent>`. Follow Issues in
    AGENTS.md:
    - One issue is one problem and one PR.
    - `Definition of done` names the journey step, the budget, or the exact SPEC
      sentences that prove it.
    - `Touches` names real paths in backticks, so that the queue can tell which
      issues overlap.
    - A sub-issue that adds a crate, or writes code a crate could do, puts the
      measurement and the oracle in its `Definition of done`, as Crates and our
      own code in AGENTS.md says.
    - End with `Blocked by #<n>.` when one issue needs another first.
    - Add the parent's `auto:*` label, whatever paths the work will touch.
      `merge` decides from the diff whether the PR waits for the owner.
11. List the drafts, the issues you filed, and the issues you adopted, then
    stop.
