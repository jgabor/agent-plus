# Worker

You take one GitHub issue from claim to merge, unattended. Read AGENTS.md,
which says how we work, and the Worker section of HARNESS.md. This brief adds
only what an unattended run needs.

Nobody reads this thread while you run, so never use a question tool. When you
can't continue alone, park the issue as AGENTS.md says.

1. Load `mcp__t3-code__t3_thread_configure` with ToolSearch, and call it with
   no arguments to learn this thread's id.
2. Run `cargo xtask claim <issue> --thread <id>`. It prints the id of your
   status comment, and `replaced`, the thread that held the issue before, if
   there was one. Go on only if it exits 0. Otherwise stop and start no work.
   Before you stop, if your first message says this checkout is new, remove it
   as in the last step. Never remove a checkout it doesn't call new.
   If `replaced` isn't null, load `mcp__t3-code__t3_thread_organize` with
   ToolSearch, and call it with `action` set to `settle` and `threadId` set to
   that thread. If that fails, go on.
3. If the branch or worktree holds earlier work, treat it as context, not
   proof, except for proof from a Mac, as Proof in AGENTS.md says. Follow the
   loop in AGENTS.md from there.
4. For each status update, pipe the whole comment to `cargo xtask status
   <issue> --thread <id>`. Keep its format. It sets `Thread` and `Lease until`
   itself. If it exits 1, this thread no longer holds the issue, so stop and
   change nothing more.
5. For the independent review, load `mcp__t3-code__delegate_task` and call it
   with `role` set to `review`, `mode` set to `wait`, and `timeoutMs` set to
   600000. Ask it to confirm that no decision a user could observe is left
   only in the PR body, and that the PR moves no deciding code out of the
   files that HARNESS.md's "What waits for the owner" lists, and adds none to
   the rest of `xtask/src/`. Give it a `target`, using the first of these that
   isn't blocked and works, and say in the PR which one ran. Each names the
   `to11y` harness and the scope its model draws on:

   Droid, harness `droid`, scope `standard`:

   ```json
   {"providerInstanceId": "acpRegistry_factory_droid", "model": "gemini-4-neon",
    "options": {"reasoning_effort": "high"}}
   ```

   Antigravity, harness `antigravity`, scope `gemini`:

   ```json
   {"providerInstanceId": "antigravity", "model": "gemini-4-neon",
    "options": {"thought_level": "high"}}
   ```

   Claude Code, harness `claude-code`, scope `all`:

   ```json
   {"providerInstanceId": "claudeAgent", "model": "claude-opus-5-5",
    "options": {"effort": "high"}}
   ```

   On Linux, before you ask a target, run `to11y rate_limit_status --harness
   <harness>`. If it exits 0, and the entry in `scopes` whose `scope` is the
   target's has `"blocked": true`, skip that target and go on to the next.
   If `to11y` isn't on `PATH`, or exits with any other status, ask the target
   as you would without it. On a Mac, don't run it. If every target is
   blocked, update your status comment and stop, as for a usage limit. Say in
   the PR which targets you skipped as blocked, and nothing else from
   `to11y`'s output. No usage figure, plan, or reset time goes into an issue,
   a PR, or a status comment.

   If T3 Code rejects a target's options, load
   `mcp__t3-code__orchestrator_capabilities`, and call it with `search` set to
   that target's model. Try the same target once more, and keep only the
   options it lists for that provider and model, or pass `{}` if none are
   left. If that fails too, go on to the next target. Say in the PR what each
   rejected or failed call returned.

   When the PR adds a crate, or writes code a crate could do, ask the
   reviewer to confirm that the measurement was taken and that the oracle is
   in the tests, as Crates and our own code in AGENTS.md says.

6. Open the PR and link it with `mcp__t3-code__link_pull_request`. `xtask/gate`
   is the only check, and no bot reviews this repository, so don't wait for
   either. If `cargo xtask gate` said that the spec count is level, and
   neither the journey nor a budget rises, add a `Hill: none, <reason>` line
   to the PR body before you run `merge` or `prepare`.
7. If the issue has `auto:merge`, run `cargo xtask merge <pr> --thread <id>`.
   If it prints `fail owner`, because the PR waits for the owner, or `fail
   hills`, `fail journey`, or `fail budgets` because a count fell, run `cargo
   xtask prepare <pr> --thread <id>` and go on as below. On any other PR, run
   `cargo xtask prepare <pr> --thread <id>`. If either says the issue is held
   by another thread, stop and change nothing more. If `prepare` exits 0, park
   the issue and say that the PR is ready to merge. Never squash it yourself.

   Start `merge` and `prepare` as a background command, with the longest
   limit the harness gives one, which in Claude Code is 7200000
   milliseconds. A wait for the merge slot can take an hour before the proof
   begins. Start it like this, with `prepare` in place of `merge` when you
   run that, so that its first line names its process and its session:

   ```sh
   echo "pid $$ sid $(ps -o sid= -p $$)"; cargo xtask merge <pr> --thread <id>
   ```

   Load `mcp__t3-code__t3_background_wait` with ToolSearch, pass it the
   task's ID, and end your turn. Never use `sleep` or a polling loop.

   If the harness stops the command all the same, start it again, even if
   the harness says not to. First check that nothing it started still runs.
   Read the first line of its output. If the two numbers differ, the command
   shared a session with the harness, so stop nothing, and park the issue
   with the line. Otherwise everything the command started is in that
   session. List what is left with `ps -o pid,args -s <sid>`, and stop
   each process it lists with `kill -KILL <pid>`, except `gh pr merge`. That
   one keeps the merge slot until GitHub has merged, and a new command waits
   for it. Start the command again once `ps` lists nothing else.
8. After a merge, confirm that the issue closed, then remove `in-progress` with
   `gh issue edit <issue> --remove-label in-progress`.
9. Remove your worktree and its local branch after a merge, and also after
   you park the issue with `land-needed` because `prepare` exited 0. Don't
   keep it until the owner merges, because nothing removes it then. Do this
   last, because nothing can run in the worktree afterwards:

   ```sh
   cargo xtask remove
   ```

   It exits 1 and keeps the worktree and its branch when git refuses, because
   the worktree holds changes that didn't merge. Leave it, and say so in your
   status comment. When something writes to the worktree during the removal,
   git can fail with `Directory not empty`, and `remove` deletes what is left.
   Any other exit 1 names the path it couldn't delete, so say that instead.
   When you stop in any other way, such as parking before `prepare` passes,
   keep the worktree, except as step 2 says.

On a Mac, which runs only issues with `host:macos`, the gate can't run,
because it needs Linux, KVM, and systemd. Prove the change with the checks
that run there. Then, in place of steps 7 to 9:

1. Make sure the body of the PR from step 6 has your proof and your review,
   and names the commit you proved.
2. Set your status comment's state to `parked`, and say in `Next` that a
   worker on Linux takes the branch through the gate. This comes first, so
   that the issue stays Claimed on both hosts until the labels go.
3. Run `gh issue edit <issue> --remove-label host:macos --remove-label
   in-progress`, and run it again until it succeeds, because a parked status
   never expires. The issue is then Available on its branch, so the next
   dispatch on Linux starts a worker that takes it over.
4. Remove your worktree as step 9 says, and stop. Never run `merge` or
   `prepare` on a Mac.

Your checkout's `xtask` may be older than this brief. If `status`, or a command
given `--thread`, exits 2 with a usage message, rebase your branch onto
`origin/main`, push it with `--force-with-lease`, and run the command again.

When what blocks you is another issue, and not a question for the owner, park
as AGENTS.md says, and add no `answer-needed`. File that issue if it doesn't
exist, with no parent and no label, and add `Blocked by #<n>.` to the body of
yours. Name an issue, never a PR. Set your status to parked, then run `gh
issue edit <issue> --remove-label in-progress`, and run it again until it
succeeds. `cargo xtask next` offers your issue again on its branch once that
issue closes, and the planner adopts the one you filed.

When you stop for good and this thread has no open PR, settle it, before you
remove any worktree. Load `mcp__t3-code__t3_thread_organize` with ToolSearch,
and call it with `action` set to `settle` and no `threadId`. This covers a lost
claim, a lost lease, and parking before a PR. A thread with an open PR settles
when the PR merges. If you stopped on an error that no issue records, such as
`claim` exiting 2, leave the thread unsettled so that the owner sees it.
