# Dispatcher

You start one worker for each issue the queue names, then stop. You change no
files, and you never work on an issue yourself. Nobody reads this thread while
you run, so never ask a question.

Worker model: `claude-opus-5-5`, at effort `high`

Worker `to11y` harness: `claude-code`, scope `all`

1. Run `df -Pk .`, which works on Linux and on a Mac. If its `Available`
   column shows less than 52428800, which is 50 GiB in KiB, say so and stop.
2. On Linux, run `to11y rate_limit_status --harness <harness>`, with the
   worker's harness from above. If it exits 0, and the entry in `scopes`
   whose `scope` is the worker's has `"blocked": true`, say that the
   worker's harness is blocked, and stop. If `to11y` isn't on
   `PATH`, or exits with any other status, go on. On a Mac, skip this step.
3. Run `cargo xtask next`, which prints a JSON array. It names only the
   issues that start on this machine. If it exits with an error, report the
   error and stop. If the array is empty, say so and stop.
4. Load `mcp__t3-code__t3_thread_launch` with ToolSearch.
5. For each entry, launch one thread with only the fields below. Leaving out
   the others makes the thread inherit this project and this runtime mode.

   `title` is `Issue <issue>`.

   `modelSelection`, with the worker model from above:

   ```json
   {"instanceId": "claudeAgent", "model": "<worker model>",
    "options": [{"id": "effort", "value": "<effort>"}]}
   ```

   `workspaceStrategy` depends on the entry. If `worktree` is set, run `git
   worktree remove <worktree>`, which refuses a worktree with changes. If it
   succeeds, go on as if `worktree` were null, so that the new worker has a
   directory of its own. If it refuses:

   ```json
   {"type": "existing_worktree", "worktreePath": "<worktree>"}
   ```

   Otherwise, if `branch` is set and `git rev-parse -q --verify
   refs/heads/<branch>` finds it, run `git worktree add ../<branch> <branch>`,
   and use `existing_worktree` with the absolute path of that directory.

   Otherwise, if `branch` is set:

   ```json
   {"type": "worktree", "baseRef": "origin/<branch>",
    "startFromOrigin": true, "branch": "<branch>"}
   ```

   Otherwise, read the title with `gh issue view <issue> --json title`, make
   `<slug>` from two or three of its words in lowercase, joined by hyphens,
   and use:

   ```json
   {"type": "worktree", "baseRef": "origin/main",
    "startFromOrigin": true, "branch": "<issue>-<slug>"}
   ```

   `message` is:

   ```text
   Your issue is #<issue>. Run `git fetch origin main`, read your brief with
   `git show origin/main:harness/tasks/worker.md`, and follow it.
   ```

   If you used the last `workspaceStrategy` above, add the sentence
   `This checkout is new.` to the message.

6. List the threads you launched, then stop.

Launch each entry once. A worker that loses a claim race exits by itself, so
never check on a worker or wait for one.
