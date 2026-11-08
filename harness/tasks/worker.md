# Worker

You take one GitHub issue from claim to merge, unattended. Read AGENTS.md,
which says how we work, and the Worker section of HARNESS.md. This brief adds
only what an unattended run needs.

Nobody reads this thread while you run, so never use a question tool. When you
can't continue alone, park the issue as AGENTS.md says.

1. Load `mcp__t3-code__t3_thread_configure` with ToolSearch, and call it with
   no arguments to learn this thread's id.
2. Run `cargo xtask claim <issue> --thread <id>`. It prints the id of your
   status comment. If it exits 1, another claim won, so stop and change
   nothing.
3. If the branch or worktree holds earlier work, treat it as context, not
   proof. Follow the loop in AGENTS.md from there.
4. For each status update, edit your comment with `gh api -X PATCH
   repos/{owner}/{repo}/issues/comments/<id> -F body=@-`. Keep its format, and
   set `Lease until` to 12 hours from now.
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
7. If the issue has `auto:merge`, run `cargo xtask merge <pr>`. Never pass
   `--owner`. If the issue has `auto:pr`, or the PR has `spec-change`, park
   the issue and say that the PR is ready to merge.
8. After a merge, confirm that the issue closed, then remove `in-progress` with
   `gh issue edit <issue> --remove-label in-progress`.
9. Remove your worktree last, because nothing can run in it afterwards:

   ```sh
   here=$(pwd -P)
   branch=$(git branch --show-current)
   cd "$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"
   git worktree remove --force "$here"
   git branch -D "$branch"
   ```
