# Owner only: agents merge with `cargo xtask merge`, under their labels.
# Prove a PR with `cargo xtask prepare`, then squash it if nothing went wrong.
merge pr:
    #!/usr/bin/env bash
    set -euo pipefail
    pr={{ quote(pr) }}
    fail() { echo "just merge: $*" >&2; exit 1; }

    # The checks are xtask's, so an old checkout runs old checks.
    [ "$(git branch --show-current)" = main ] || fail "run this on main"
    git pull --ff-only --quiet origin main
    [ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] &&
      git diff --quiet HEAD || fail "main has local commits or changes"

    cargo xtask prepare "$pr"

    # prepare gave the merge slot back. Hold it through the squash, so that no
    # other merge can move main first. gh inherits it, as in `xtask merge`.
    exec 9>>"$(git rev-parse --git-common-dir)/xtask/merge.lock"
    flock 9

    view=$(gh pr view "$pr" --json state,headRefOid \
      --jq '"\(.state) \(.headRefOid)"')
    read -r state head <<<"$view"
    case "$state" in
      OPEN) ;;
      MERGED) echo "just merge: PR $pr is already merged"; exit 0 ;;
      *) fail "PR $pr is $state, so there is nothing to merge" ;;
    esac

    gate=$(gh api "repos/{owner}/{repo}/commits/$head/status" \
      --jq '.statuses[] | select(.context == "xtask/gate") | .state')
    [ "$gate" = success ] || fail "xtask/gate isn't green on $head"

    behind=$(gh api "repos/{owner}/{repo}/compare/main...$head" --jq .behind_by)
    [ "$behind" = 0 ] || fail "main moved after the proof. Run this again"

    # prepare accepts a lowered count or a revert, and says so in a comment.
    comments=$(gh pr view "$pr" --json comments --jq '.comments[].body')
    if [[ $comments == *"Merging this PR at \`$head\` accepts:"* ]]; then
      fail "PR $pr has a comment saying what merging accepts." \
        "Read it, and use a merge button if you accept it"
    fi

    # Naming the repository keeps gh away from local branches and worktrees.
    repo=$(gh repo view --json nameWithOwner --jq .nameWithOwner)
    gh pr merge "$pr" -R "$repo" --squash --delete-branch \
      --match-head-commit "$head"
