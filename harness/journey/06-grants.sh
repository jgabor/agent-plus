#!/bin/sh
set -eu

store="$HOME/.local/share/ap"
here=$(pwd -P)

mkdir -p notes shared
echo "one" >notes/data
echo "two" >shared/data
cat >notes/Agentfile <<'EOF2'
AGENT claude
OS minimal
ADD ./data
ADD ../shared/data /shared
NETWORK -
NETWORK +github.com
EOF2

# Fails unless `ap build` with these arguments fails, with no terminal.
build_fails() {
  if ap build "$@" </dev/null >out 2>err; then
    echo "ap build $* succeeded, but it should have needed approval:" >&2
    cat out err >&2
    exit 1
  fi
}

# Fails unless the build's errors hold this line.
says() {
  if ! grep -qxF -- "$1" err; then
    echo "ap build didn't say \`$1\`:" >&2
    cat err >&2
    exit 1
  fi
}

build_fails notes
says "ap: notes/Agentfile asks for these grants:"
says "  PATH $here/shared/data"
says "  NETWORK -"
says "  NETWORK +github.com"
says "  POLICY +"
if [ -e "$store/agents/notes" ]; then
  echo "a build that needed approval built the agent" >&2
  exit 1
fi

printf 'NETWORK -\nNETWORK +github.com\nPOLICY +\n' >fewer
build_fails --approve=fewer notes
says "ap: notes/Agentfile asks for more than fewer approves:"
says "  PATH $here/shared/data"

printf 'PATH %s/shared/data\nNETWORK -\nNETWORK +github.com\nPOLICY +\n' \
  "$here" >grants
ap build --approve=grants notes </dev/null >out
if ! grep -qx 'agent notes' out; then
  echo "a build with exactly its grants approved didn't build:" >&2
  cat out >&2
  exit 1
fi
ap build notes </dev/null >/dev/null

echo "NETWORK +crates.io" >>notes/Agentfile
build_fails notes
says "ap: notes/Agentfile asks for more than you approved:"
says "  NETWORK +crates.io"
if grep -q 'PATH\|github' err; then
  echo "a wider grant listed more than what changed:" >&2
  cat err >&2
  exit 1
fi

cat >notes/Agentfile <<'EOF2'
AGENT claude
OS minimal
ADD ./data
NETWORK -
EOF2
if ! ap build notes </dev/null >out 2>err; then
  echo "a build that only narrows its grants asked:" >&2
  cat err >&2
  exit 1
fi
