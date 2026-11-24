#!/bin/sh
set -eu

# Builds Claude Code and Codex with no credentials, lets `ap build` resolve,
# download, and lock each harness, then reads what each one reports before
# sign-in. The journey keeps downloads in `$XDG_CACHE_HOME/ap` across steps,
# so each harness is downloaded once.

# Builds the agent `$1` from the harness and channel `$2`, and fails unless
# the lockfile pins that harness by an exact version and a digest, which for
# Codex the digests of the code-mode host and of bubblewrap, each or `-`,
# follow. Each agent is named for
# its harness, which its grants name under `NETWORK -`.
build() {
  mkdir -p "$1"
  printf 'AGENT %s\nNETWORK -\nPOLICY +\n' "$1" >"$1.grants"
  printf 'AGENT %s\nOS minimal\nNETWORK -\n' "$2" >"$1/Agentfile"
  if ! ap build --approve="$1.grants" "$1" </dev/null >/dev/null 2>"$1.build"; then
    echo "ap build $1 failed for AGENT $2:" >&2
    cat "$1.build" >&2
    exit 1
  fi
  if [ ! -f "$1/Agentfile.lock" ]; then
    echo "ap build didn't download $2 or write $1/Agentfile.lock" >&2
    exit 1
  fi
  case $1 in
    codex) host=' ([0-9a-f]{64}|-) ([0-9a-f]{64}|-)' ;;
    *) host='' ;;
  esac
  if ! grep -Eq "^HARNESS $1 [^ :]+ [0-9a-f]{64}$host\$" "$1/Agentfile.lock" ||
    grep -Eq "^HARNESS $1 (stable|latest) " "$1/Agentfile.lock"; then
    echo "ap build didn't lock $2 to an exact version and digest:" >&2
    cat "$1/Agentfile.lock" >&2
    exit 1
  fi
}

# The runs this script started, and the processes that hold them, so that
# none outlasts it.
runs=""
holders=""
cleanup() {
  for run in $runs; do
    timeout -k 5 30 ap stop "$run" >/dev/null 2>&1 || true
  done
  for pid in $holders; do
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  done
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

# Starts the agent `$1` interactively, in a terminal that `script` gives it,
# and sets `run` to the new run's name. With no credentials, the harness
# waits at its sign-in screen, which keeps the run open for `ap exec`. The
# terminal's input stays open until this script exits, and the run outlasts
# the slowest probe below.
start() {
  mkfifo "$1.keys"
  timeout -k 10 900 script -qec "ap run $1" /dev/null <"$1.keys" \
    >"$1.screen" 2>&1 &
  holders="$holders $!"
  case $1 in
    claude) exec 3>"$1.keys" ;;
    *) exec 4>"$1.keys" ;;
  esac
  run=""
  tries=0
  while [ -z "$run" ]; do
    if [ "$tries" -ge 60 ]; then
      echo "ap run $1 didn't show up in ap ps within 60 seconds:" >&2
      cat "$1.screen" >&2
      ap ps >&2 || true
      exit 1
    fi
    sleep 1
    tries=$((tries + 1))
    run=$(ap ps 2>/dev/null | awk -v agent="$1" '$2 == agent { print $1; exit }')
  done
  runs="$runs $run"
}

# Fails unless `ap exec` of `$3...` in the run `$1` prints exactly `$2`.
expect() {
  target=$1
  expected=$2
  shift 2
  got=$(timeout -k 10 60 ap exec "$target" "$@" </dev/null) || {
    echo "ap exec $target $* exited $?" >&2
    exit 1
  }
  if [ "$got" != "$expected" ]; then
    echo "ap exec $target $* printed \"$got\", not \"$expected\"" >&2
    exit 1
  fi
}

# Fails unless the harness's report in `$1` has `$2`. A third argument, if
# given, says how the command that wrote the report ended.
says() {
  if ! grep -qF -- "$2" "$1"; then
    echo "the harness didn't report $2${3:+, and $3}:" >&2
    cat "$1" >&2
    exit 1
  fi
}

# Fails unless the harness in the run `$1` runs as `agent` in `/home/agent`.
in_home() {
  expect "$1" agent id -un
  expect "$1" /home/agent printenv HOME
  expect "$1" /home/agent pwd
}

build claude claude:stable
build codex codex:latest

# Claude Code reports its `init` event before it needs a sign-in. With no
# credentials, it exits 1 after that, so the probe needs no input. Any other
# status fails the step, such as the 125 of an `ap exec` whose run ended
# before the command did. Claude Code prints the event only once its start
# is done, which took 38 to 132 seconds on a loaded machine. A traced start
# kept making progress the whole time, with no wait on the network. The
# deadline is far past that, because Claude Code exits as soon as it has
# reported, so a slow start costs only its own time. Claude Code prints
# nothing before its `init` event, so a probe that times out leaves an empty
# report. A failed check says how the probe ended, and after how long.
start claude
in_home "$run"
begun=$(date +%s)
status=0
timeout -k 10 600 ap exec "$run" claude -p "Say what you can see." \
  --output-format stream-json --verbose </dev/null >claude.init 2>&1 ||
  status=$?
took=$(($(date +%s) - begun))
case $status in
  124) ended="the probe timed out after $took seconds" ;;
  *) ended="the probe exited $status after $took seconds" ;;
esac
says claude.init '"subtype":"init"' "$ended"
says claude.init '"cwd":"/home/agent"' "$ended"
says claude.init '"mcp_servers":' "$ended"
says claude.init '"slash_commands":' "$ended"
if [ "$status" -ne 1 ]; then
  echo "Claude Code exits 1 with no credentials, but $ended:" >&2
  cat claude.init >&2
  exit 1
fi
timeout -k 5 30 ap stop "$run"

# Codex reports through its app-server. The probe starts it in the run with
# `ap exec`, sends `initialize`, `initialized`, and `thread/start` as JSON-RPC
# lines, and keeps its input open until the `thread/start` result arrives.
start codex
in_home "$run"
: >codex.rpc
# The input side reads the output file the app-server writes, on purpose.
# shellcheck disable=SC2094
{
  printf '%s\n' \
    '{"id":1,"method":"initialize","params":{"clientInfo":{"name":"journey","version":"0"}}}' \
    '{"method":"initialized"}' \
    '{"id":2,"method":"thread/start","params":{}}'
  tries=0
  while [ "$tries" -lt 60 ] && ! grep -q '"id":2' codex.rpc; do
    sleep 1
    tries=$((tries + 1))
  done
} | timeout -k 10 90 ap exec "$run" codex app-server >codex.rpc 2>codex.err ||
  true
if ! grep '"id":2' codex.rpc | grep -q '"result"'; then
  echo "codex app-server gave no thread/start result:" >&2
  cat codex.rpc codex.err >&2
  exit 1
fi
says codex.rpc '"cwd":"/home/agent"'
says codex.rpc '"approvalPolicy":'
says codex.rpc '"sandbox":'
