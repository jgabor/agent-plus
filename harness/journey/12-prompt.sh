#!/bin/sh
set -eu

# Runs real Claude Code headless on Haiku, signed in with `AUTH key`, on a
# copy-on-write share that is a repository's root. The prompt asks for one
# edit. Your files must stay as they were until `ap apply`, `ap diff` must
# show the edit, and `ap apply` must land it on the run's branch. The `git`
# on the host is the oracle for the branch. The journey gives this step the
# paid checks key as `ANTHROPIC_API_KEY`, and nothing this step prints may
# hold it.

if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  echo "ANTHROPIC_API_KEY is empty, so the step can't sign Claude Code in" >&2
  exit 1
fi

# Prints the files `$@` to standard error, with the key, if one holds it,
# shown as `<key>`. `awk` reads the key from its environment, so it stays
# off every command line.
show() {
  awk '{
    key = ENVIRON["ANTHROPIC_API_KEY"]
    while ((at = index($0, key)) > 0)
      $0 = substr($0, 1, at - 1) "<key>" substr($0, at + length(key))
    print
  }' "$@" >&2
}

# Fails with what the files after `$1` hold, under the message `$1`.
fail() {
  echo "$1:" >&2
  shift
  show "$@"
  exit 1
}

# The host's `git`, away from any config but its own identity.
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
export GIT_AUTHOR_NAME=You GIT_AUTHOR_EMAIL=you@localhost
export GIT_COMMITTER_NAME=You GIT_COMMITTER_EMAIL=you@localhost

# `MODE edit` lets Claude Code edit files in its workspace without asking,
# which a headless run can't do.
mkdir -p repo/share
cat >repo/Agentfile <<'EOF'
AGENT claude
OS minimal
MODEL claude-haiku-5-5
MODE edit
AUTH key
NETWORK -
SHARE ./share
EOF
printf 'AGENT claude\nAUTH key\nNETWORK -\nPOLICY +\n' >grants
if ! ap build --approve=grants repo </dev/null >/dev/null 2>repo.build; then
  fail "ap build repo failed" repo.build
fi

git -C repo/share init -q -b main
printf 'draft\n' >repo/share/notes.txt
git -C repo/share add .
git -C repo/share commit -qm Start
start=$(git -C repo/share rev-parse HEAD)

# Fails unless your repository is as it was when the run started.
untouched() {
  if [ "$(cat repo/share/notes.txt)" != draft ] ||
    [ "$(git -C repo/share rev-parse HEAD)" != "$start" ] ||
    [ -n "$(git -C repo/share status --porcelain)" ]; then
    git -C repo/share status >repo.status 2>&1
    fail "$1" repo.status
  fi
}

# One short turn. The timeout stops a run that hangs, and with it the VM
# that its `ap run` holds. Claude Code's start alone took up to 132 seconds
# on a loaded machine, as step 8 says.
status=0
timeout -k 10 600 ap run repo -p "In notes.txt, replace the line draft with \
the line final. Change nothing else, and don't run git." \
  </dev/null >repo.out 2>repo.err || status=$?
if [ "$status" -ne 0 ]; then
  fail "ap run repo exited $status" repo.out repo.err
fi
untouched "the run changed your repository before ap apply"
if [ "$(git -C repo/share for-each-ref --format='%(refname)')" != \
  refs/heads/main ]; then
  git -C repo/share for-each-ref >repo.refs 2>&1
  fail "the run added a ref to your repository before ap apply" repo.refs
fi

# Runs `ap $1 repo` into `repo.$1`, and fails unless it exits 0.
ap_ok() {
  if ! timeout -k 5 60 ap "$1" repo </dev/null >"repo.$1" 2>"repo.$1.err"; then
    fail "ap $1 repo failed" "repo.$1" "repo.$1.err"
  fi
}

ap_ok diff
run=$(sed -n \
  's|^branch ap/repo/\([0-9a-z]*\) would carry [0-9]* commits*$|\1|p' repo.diff)
if [ -z "$run" ]; then
  fail "ap diff doesn't name the run's branch" repo.diff
fi
for line in '--- a/notes.txt' '+++ b/notes.txt' '-draft' '+final'; do
  if ! grep -qxF -- "$line" repo.diff; then
    fail "ap diff has no line \"$line\"" repo.diff
  fi
done

ap_ok apply
untouched "ap apply touched your HEAD, index, or working tree"
branch="ap/repo/$run"
if ! git -C repo/share merge-base --is-ancestor "$start" "$branch" \
  2>repo.branch; then
  fail "$branch doesn't build on your HEAD" repo.branch
fi
if [ "$(git -C repo/share show "$branch:notes.txt")" != final ]; then
  git -C repo/share show "$branch:notes.txt" >repo.branch 2>&1 || true
  fail "notes.txt on $branch isn't the agent's edit" repo.branch
fi
# The second `ap diff` writes to files of its own, so the first one's are
# still there for the check below.
if ! timeout -k 5 60 ap diff repo </dev/null >repo.after 2>repo.after.err; then
  fail "ap diff repo failed after ap apply" repo.after repo.after.err
fi
if [ -s repo.after ]; then
  fail "ap diff still shows changes after ap apply" repo.after
fi

# Nothing `ap` wrote for this step holds the key.
for file in repo.build repo.out repo.err repo.diff repo.diff.err repo.apply \
  repo.apply.err repo.after repo.after.err; do
  if awk 'index($0, ENVIRON["ANTHROPIC_API_KEY"]) { found = 1 }
    END { exit !found }' "$file"; then
    echo "$file holds the key" >&2
    exit 1
  fi
done
