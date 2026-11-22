#!/bin/sh
set -eu

# Runs the fake harness on a copy-on-write share, in a plain directory and in
# a repository's root. Its prompt steers what it puts in, changes, deletes,
# and commits. Your files must stay as they were until `ap apply`, `ap diff`
# must show each edit, `ap apply` must land the files and the branch, and
# `ap discard` must empty a second run. The `git` on the host is the oracle
# for the branch.

# The journey puts the fake harness next to `ap`. It reaches each agent as
# its Claude Code, as in step 7.
fake=$(command -v fake-harness)
if command -v sha256sum >/dev/null; then
  digest=$(sha256sum "$fake" | cut -d ' ' -f 1)
else
  digest=$(shasum -a 256 "$fake" | cut -d ' ' -f 1)
fi
mkdir -p "$XDG_CACHE_HOME/ap"
cp "$fake" "$XDG_CACHE_HOME/ap/$digest"
printf 'AGENT claude\nNETWORK -\nPOLICY +\n' >grants

# The host's `git`, away from any config but its own identity.
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
export GIT_AUTHOR_NAME=You GIT_AUTHOR_EMAIL=you@localhost
export GIT_COMMITTER_NAME=You GIT_COMMITTER_EMAIL=you@localhost

# Builds the agent `$1`, whose only share is its directory `$1/share`.
build() {
  mkdir -p "$1/share"
  printf 'AGENT claude:0.0.0\nOS minimal\nNETWORK -\nSHARE ./share\n' \
    >"$1/Agentfile"
  echo "HARNESS claude 0.0.0 $digest" >"$1/Agentfile.lock"
  if ! ap build --approve=grants "$1" </dev/null >/dev/null 2>"$1.build"; then
    echo "ap build $1 failed:" >&2
    cat "$1.build" >&2
    exit 1
  fi
}

# Runs the agent `$1` headless with the prompt `$2`, and fails unless the
# harness and `ap run` exit 0. The timeout stops a run that hangs, and with
# it the VM that its `ap run` holds.
run() {
  status=0
  timeout -k 10 120 ap run "$1" -p "$2" </dev/null >"$1.out" 2>"$1.err" ||
    status=$?
  if [ "$status" -ne 0 ]; then
    echo "ap run $1 exited $status:" >&2
    cat "$1.out" "$1.err" >&2
    exit 1
  fi
}

# Fails unless the files below `$1` hold what `$2` lists, one
# `<path>:<contents>` a line, sorted, as when the run started.
unchanged() {
  actual=$(cd "$1" && find . -path ./.git -prune -o -type f -print |
    sed 's|^\./||' | LC_ALL=C sort |
    while read -r path; do printf '%s:%s\n' "$path" "$(cat "$path")"; done)
  if [ "$actual" != "$2" ]; then
    echo "the files in $1 aren't as they should be:" >&2
    echo "$actual" >&2
    echo "but should be:" >&2
    echo "$2" >&2
    exit 1
  fi
}

# Fails unless the file `$1` has a line that is exactly `$2`.
has() {
  if ! grep -qxF -- "$2" "$1"; then
    echo "$1 has no line \"$2\":" >&2
    cat "$1" >&2
    exit 1
  fi
}

# Runs `ap $1 $2` into `$2.$1`, and fails unless it exits 0.
ap_ok() {
  if ! timeout -k 5 60 ap "$1" "$2" </dev/null >"$2.$1" 2>"$2.$1.err"; then
    echo "ap $1 $2 failed:" >&2
    cat "$2.$1" "$2.$1.err" >&2
    exit 1
  fi
}

# In a plain directory.
build plain
printf 'yours\n' >plain/share/changed.txt
printf 'yours\n' >plain/share/deleted.txt
printf 'yours\n' >plain/share/kept.txt
before='changed.txt:yours
deleted.txt:yours
kept.txt:yours'
run plain 'put added/new.txt added by the agent
put changed.txt changed by the agent
delete deleted.txt'
unchanged plain/share "$before"

ap_ok diff plain
for line in '+++ b/added/new.txt' '+added by the agent' '--- a/changed.txt' \
  '-yours' '+changed by the agent' '--- a/deleted.txt'; do
  has plain.diff "$line"
done
if grep -q 'kept.txt' plain.diff; then
  echo "ap diff shows a file the agent didn't change:" >&2
  cat plain.diff >&2
  exit 1
fi

ap_ok apply plain
unchanged plain/share 'added/new.txt:added by the agent
changed.txt:changed by the agent
kept.txt:yours'

# A second run's changes are gone after `ap discard`.
after='added/new.txt:added by the agent
changed.txt:changed by the agent
kept.txt:yours'
run plain 'put second.txt from a second run
delete kept.txt'
unchanged plain/share "$after"
ap_ok diff plain
has plain.diff '+from a second run'
ap_ok discard plain
unchanged plain/share "$after"
ap_ok diff plain
if [ -s plain.diff ]; then
  echo "ap diff shows changes that ap discard should have thrown away:" >&2
  cat plain.diff >&2
  exit 1
fi

# In a repository's root.
build repo
git -C repo/share init -q -b main
printf 'yours\n' >repo/share/changed.txt
printf 'yours\n' >repo/share/deleted.txt
printf 'yours\n' >repo/share/kept.txt
git -C repo/share add .
git -C repo/share commit -qm Start
start=$(git -C repo/share rev-parse HEAD)
before='changed.txt:yours
deleted.txt:yours
kept.txt:yours'
run repo 'put src/committed.txt committed by the agent
put changed.txt changed by the agent
delete deleted.txt
commit Change some files
put uncommitted.txt left uncommitted'
unchanged repo/share "$before"
if [ "$(git -C repo/share rev-parse HEAD)" != "$start" ] ||
  [ -n "$(git -C repo/share status --porcelain)" ] ||
  [ "$(git -C repo/share for-each-ref --format='%(refname)')" != refs/heads/main ]; then
  echo "the run changed the repository before ap apply" >&2
  git -C repo/share log --all --oneline >&2
  git -C repo/share status >&2
  exit 1
fi

ap_ok diff repo
run=$(sed -n 's|^branch ap/repo/\([0-9a-z]*\) would carry 1 commit$|\1|p' \
  repo.diff)
if [ -z "$run" ]; then
  echo "ap diff doesn't say the branch would carry the agent's 1 commit:" >&2
  cat repo.diff >&2
  exit 1
fi
for line in '+++ b/src/committed.txt' '+committed by the agent' \
  '+changed by the agent' '--- a/deleted.txt' '+++ b/uncommitted.txt' \
  '+left uncommitted'; do
  has repo.diff "$line"
done

ap_ok apply repo
unchanged repo/share "$before"
if [ "$(git -C repo/share rev-parse HEAD)" != "$start" ] ||
  [ -n "$(git -C repo/share status --porcelain)" ]; then
  echo "ap apply touched your HEAD, index, or working tree" >&2
  git -C repo/share status >&2
  exit 1
fi
branch="ap/repo/$run"
git -C repo/share log --format='%an <%ae>|%s' "$branch" >repo.log
printf '%s\n' "Agent+ <ap@localhost>|Uncommitted changes from repo/$run" \
  'Fake Harness <fake-harness@localhost>|Change some files' \
  'You <you@localhost>|Start' >repo.log.expected
if ! cmp -s repo.log repo.log.expected; then
  echo "git log $branch isn't the agent's history:" >&2
  cat repo.log >&2
  exit 1
fi
git -C repo/share ls-tree -r --name-only "$branch~1" >repo.committed
printf 'changed.txt\nkept.txt\nsrc/committed.txt\n' >repo.committed.expected
git -C repo/share ls-tree -r --name-only "$branch" >repo.last
printf 'changed.txt\nkept.txt\nsrc/committed.txt\nuncommitted.txt\n' \
  >repo.last.expected
if ! cmp -s repo.committed repo.committed.expected ||
  ! cmp -s repo.last repo.last.expected; then
  echo "git ls-tree doesn't show the agent's files on $branch:" >&2
  cat repo.committed repo.last >&2
  exit 1
fi
if [ "$(git -C repo/share show "$branch:uncommitted.txt")" != 'left uncommitted' ] ||
  [ "$(git -C repo/share show "$branch:changed.txt")" != 'changed by the agent' ]; then
  echo "$branch doesn't hold what the agent wrote" >&2
  exit 1
fi
if ! git -C repo/share fsck --strict --no-dangling >repo.fsck 2>&1; then
  echo "git fsck --strict found a problem after ap apply:" >&2
  cat repo.fsck >&2
  exit 1
fi
ap_ok diff repo
if [ -s repo.diff ]; then
  echo "ap diff still shows changes after ap apply:" >&2
  cat repo.diff >&2
  exit 1
fi
