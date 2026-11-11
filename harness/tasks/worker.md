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
   proof. Follow the loop in AGENTS.md from there.
4. For each status update, pipe the whole comment to `cargo xtask status
   <issue> --thread <id>`. Keep its format. It sets `Thread` and `Lease until`
   itself. If it exits 1, this thread no longer holds the issue, so stop and
   change nothing more.
5. For the independent review, load `mcp__t3-code__delegate_task` and call it
   with `role` set to `review`, `mode` set to `wait`, and `timeoutMs` set to
   600000. Give it a `target`, using the first of these that works, and say
   in the PR which one ran:

   ```json
   {"providerInstanceId": "acpRegistry_factory_droid", "model": "gpt-6.1-sol"}
   ```

   ```json
   {"providerInstanceId": "opencode", "model": "opencode/big-pickle"}
   ```

   ```json
   {"providerInstanceId": "claudeAgent", "model": "claude-opus-5-5"}
   ```

6. Open the PR and link it with `mcp__t3-code__link_pull_request`. `xtask/gate`
   is the only check, and no bot reviews this repository, so don't wait for
   either.
7. If the issue has `auto:merge` and the PR doesn't have `spec-change`, run
   `cargo xtask merge <pr> --thread <id>`. If it prints `fail hills`, `fail
   journey`, or `fail budgets` because a count fell, run `cargo xtask prepare
   <pr> --thread <id>` and go on as below. On any other PR, run `cargo xtask
   prepare <pr> --thread <id>`. If either says the issue is held by another
   thread, stop and change nothing more. If `prepare` exits 0, park the issue
   and say that the PR is ready to merge. Never squash it yourself.
8. After a merge, confirm that the issue closed, then remove `in-progress` with
   `gh issue edit <issue> --remove-label in-progress`.
9. Remove your worktree last, because nothing can run in it afterwards:

   ```sh
   here=$(pwd -P)
   branch=$(git branch --show-current)
   cd "$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"
   git worktree remove "$here" && git branch -D "$branch"
   ```

   If `git worktree remove` refuses, the worktree holds changes that didn't
   merge. Leave it, and say so in your status comment.

Your checkout's `xtask` may be older than this brief. If `status`, or a command
given `--thread`, exits 2 with a usage message, rebase your branch onto
`origin/main`, push it with `--force-with-lease`, and run the command again.

When you stop for good and this thread has no open PR, settle it, before you
remove any worktree. Load `mcp__t3-code__t3_thread_organize` with ToolSearch,
and call it with `action` set to `settle` and no `threadId`. This covers a lost
claim, a lost lease, and parking before a PR. A thread with an open PR settles
when the PR merges. If you stopped on an error that no issue records, such as
`claim` exiting 2, leave the thread unsettled so that the owner sees it.
