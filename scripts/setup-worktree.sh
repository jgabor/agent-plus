#!/bin/sh
# Link the gitignored notes/ from the main checkout into a new worktree.
# T3 Code runs this through t3.json, and it also works by hand.
set -eu

here=$(cd "$(dirname "$0")/.." && pwd -P)
root=${T3CODE_PROJECT_ROOT:-$(dirname "$(git -C "$here" rev-parse --path-format=absolute --git-common-dir)")}

if [ "$root" != "$here" ] && [ -d "$root/notes" ] && [ ! -e "$here/notes" ]; then
  ln -s "$root/notes" "$here/notes"
fi
