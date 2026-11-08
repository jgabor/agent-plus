# Dispatcher

You start one worker for each issue the queue names, then stop. You change no
files, and you never work on an issue yourself. Nobody reads this thread while
you run, so never ask a question.

Worker model: `claude-opus-5-5`

1. Run `df --output=avail -BG .`. If it shows less than 50G, say so and stop.
2. Run `cargo xtask next`, which prints a JSON array. If it exits with an
   error, report the error and stop. If the array is empty, say so and stop.
3. Load `mcp__t3-code__t3_thread_launch` with ToolSearch.
4. For each entry, launch one thread with only the fields below. Leaving out
   the others makes the thread inherit this project and this runtime mode.

   `title` is `Issue <issue>`.

   `modelSelection`, with the worker model from above:

   ```json
   {"instanceId": "claudeAgent", "model": "<worker model>"}
   ```

   `workspaceStrategy` depends on the entry. If `worktree` is set:

   ```json
   {"type": "existing_worktree", "worktreePath": "<worktree>"}
   ```

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

5. List the threads you launched, then stop.

Launch each entry once. A worker that loses a claim race exits by itself, so
never check on a worker or wait for one.
