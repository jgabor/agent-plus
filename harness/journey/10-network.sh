#!/bin/sh
set -eu

# Runs the fake harness under host, address, and URL rules, and under a
# closed network, and tries the network from inside each guest with
# `ap exec`. Before a run, `ap inspect --network` answers for some of the
# destinations it tries, as the guest and `ap logs` find them. The journey
# puts the fake upstream next to `ap`. It answers on
# your host's loopback, which the guest reaches as `host.ap.internal`, and
# on your host's address on its route out. Each check reads what the guest
# got back, what reached the upstream, what `ap logs` shows, and the hosts
# that `ap run` printed as blocked.

# The journey puts the fake harness next to `ap`. It reaches each agent as
# its Claude Code, as in step 7, and sleeps after it reports, so the run
# lasts until `ap stop` ends it.
fake=$(command -v fake-harness)
if command -v sha256sum >/dev/null; then
  digest=$(sha256sum "$fake" | cut -d ' ' -f 1)
else
  digest=$(shasum -a 256 "$fake" | cut -d ' ' -f 1)
fi
mkdir -p "$XDG_CACHE_HOME/ap"
cp "$fake" "$XDG_CACHE_HOME/ap/$digest"

# The upstream says where it listens on its first line, then notes what
# reached it in `upstream/seen`. When the script ends, its trap stops the
# upstream, and the run that a failed check left going. The timeouts stop
# them if the script dies without its trap.
mkdir upstream
mkfifo upstream.out
timeout -k 5 900 fake-upstream upstream >upstream.out &
upstream=$!
running=
trap 'kill $upstream $running 2>/dev/null || true' EXIT
read -r _ plain _ tls _ address port <upstream.out || true
if [ -z "${port:-}" ]; then
  echo "fake-upstream didn't say where it listens" >&2
  exit 1
fi

# Builds the agent `$1` with the `NETWORK` lines that follow it.
build() {
  agent=$1
  shift
  mkdir "$agent"
  {
    printf 'AGENT claude:0.0.0\nOS minimal\n'
    for rule in "$@"; do echo "NETWORK $rule"; done
    echo 'ENV FAKE_HARNESS_SLEEP 300'
  } >"$agent/Agentfile"
  echo "HARNESS claude 0.0.0 $digest" >"$agent/Agentfile.lock"
  {
    echo 'AGENT claude'
    for rule in "$@"; do echo "NETWORK $rule"; done
    echo 'POLICY +'
  } >"$agent.grants"
  if ! ap build --approve="$agent.grants" "$agent" </dev/null >/dev/null \
    2>"$agent.build"; then
    echo "ap build $agent failed:" >&2
    cat "$agent.build" >&2
    exit 1
  fi
}

# The guest's attempts. Each prints one line: its label and the line the
# destination sent back, or its label and `failed`. `lookup` prints its
# label and getent's status, which is 2 for a name that doesn't exist.
try='
tcp() {
  if out=$(timeout 20 bash -c '\''exec 3<>"/dev/tcp/$0/$1" && printf "ping\n" >&3 && read -r -t 10 reply <&3 && echo "$reply"'\'' "$2" "$3" 2>/dev/null); then
    echo "$1 $out"
  else
    echo "$1 failed"
  fi
}
lookup() {
  timeout 20 getent ahostsv4 "$2" >/dev/null && echo "$1 0" || echo "$1 $?"
}
'

# Runs the agent `$1` headless, with the network variables in `$2`, tries
# the script `$3` in its guest, and stops it. The guest's lines go to
# `$1.tried`, the run's standard error to `$1.err`, and `ap logs` to
# `$1.logs` and `$1.logs.err`. The harness ends on `ap stop`'s `SIGTERM`,
# so `ap run` must exit with its status, 143. The timeout stops a run that
# hangs, and with it the VM that its `ap run` holds, after the guest's
# script has had its time.
run() {
  mkfifo "$1.out"
  env $2 timeout -k 10 240 ap run "$1" -p 'Try the network.' </dev/null \
    >"$1.out" 2>"$1.err" &
  running=$!
  exec 4<"$1.out"
  if ! read -r report <&4 || [ "${report#\{\"arguments\":}" = "$report" ]; then
    echo "ap run $1 didn't start the harness:" >&2
    cat "$1.err" >&2
    exit 1
  fi
  name=$(ap ps </dev/null | sed -n '2s/ .*//p')
  if ! timeout -k 5 180 ap exec "$name" bash -c "$try$3" </dev/null \
    >"$1.tried" 2>"$1.tried.err"; then
    echo "ap exec $name failed:" >&2
    cat "$1.tried" "$1.tried.err" >&2
    exit 1
  fi
  if ! timeout -k 5 60 ap stop "$name" </dev/null >/dev/null 2>"$1.stop"; then
    echo "ap stop $name failed:" >&2
    cat "$1.stop" >&2
    exit 1
  fi
  cat <&4 >/dev/null
  exec 4<&-
  status=0
  wait "$running" || status=$?
  running=
  if [ "$status" -ne 143 ]; then
    echo "ap run $1 exited $status after ap stop, not the harness's 143:" >&2
    cat "$1.err" >&2
    exit 1
  fi
  if ! timeout -k 5 60 ap logs "$name" </dev/null >"$1.logs" \
    2>"$1.logs.err"; then
    echo "ap logs $name failed:" >&2
    cat "$1.logs.err" >&2
    exit 1
  fi
}

# Fails unless the file `$1` holds exactly the lines that follow it.
holds() {
  file=$1
  shift
  : >"$file.expected"
  if [ "$#" -gt 0 ]; then printf '%s\n' "$@" >"$file.expected"; fi
  if ! cmp -s "$file" "$file.expected"; then
    echo "$file should hold:" >&2
    cat "$file.expected" >&2
    echo "but holds:" >&2
    cat "$file" >&2
    exit 1
  fi
}

# Fails unless the guest of `$1` got back exactly the lines that follow it.
tried() {
  agent=$1
  shift
  holds "$agent.tried" "$@"
}

# Fails unless what reached the upstream since the last check is exactly
# the lines that follow. A connection the guest ended is noted before its
# shell returns, but the upstream may take a moment to write it.
seen=0
reached() {
  sleep 1
  tail -n "+$((seen + 1))" upstream/seen >reached
  seen=$(wc -l <upstream/seen)
  holds reached "$@"
}

# Fails unless `ap logs` for `$1` printed exactly the decisions that
# follow, without their times, each as `<decision>  <destination>  <rule>`.
decided() {
  agent=$1
  shift
  sed -n 's/^[0-9-]*T[0-9:]*Z  //p' "$agent.logs.err" |
    sed 's/   */  /g' >"$agent.decided"
  holds "$agent.decided" "$@"
  if grep -qE '^(allow|deny)' "$agent.logs"; then
    echo "ap logs printed decisions with the run's output:" >&2
    cat "$agent.logs" >&2
    exit 1
  fi
}

# Fails unless `ap run` for `$1` printed exactly the blocked hosts that
# follow, each as `<host>  <rule that would allow it>`.
blocked() {
  agent=$1
  shift
  sed -n '/^ap: NETWORK blocked /,$p' "$agent.err" | sed -n 's/^  //p' |
    sed 's/   */  /g' >"$agent.blocked"
  holds "$agent.blocked" "$@"
  if [ "$#" -gt 0 ] && ! grep -qE "^ap: NETWORK blocked $# hosts?\. " \
    "$agent.err"; then
    echo "ap run $agent didn't say it blocked $# hosts:" >&2
    cat "$agent.err" >&2
    exit 1
  fi
}

# Fails unless `ap inspect` answers whether the agent `$1` may reach `$2`
# with the line `$3`, and exits 0 for its `+` or 1 for its `-`.
asked() {
  status=0
  ap inspect "$1" --network="$2" </dev/null >"$1.asked" 2>"$1.asked.err" ||
    status=$?
  holds "$1.asked" "$3"
  case "$3" in
    +*) sign=0 ;;
    *) sign=1 ;;
  esac
  if [ "$status" -ne "$sign" ]; then
    echo "ap inspect $1 --network=$2 exited $status, not $sign:" >&2
    cat "$1.asked.err" >&2
    exit 1
  fi
}

# Under a host rule for your host, it connects, and another name neither
# resolves nor connects.
build host - +host.ap.internal
asked host "host.ap.internal:$plain" \
  "+ host.ap.internal:$plain  NETWORK +host.ap.internal"
asked host "other.example:$plain" "- other.example:$plain  the default"
run host '' "
tcp host host.ap.internal $plain
lookup other other.example
tcp other other.example $plain
"
tried host 'host upstream:ping' 'other 2' 'other failed'
reached "plain ping"
decided host \
  "allow  host.ap.internal:$plain  NETWORK +host.ap.internal" \
  'deny  other.example  the default'
blocked host 'other.example  NETWORK +other.example'

# Under a closed network, only the harness's own hosts resolve, and nothing
# connects: not your host, not your host's address, and no other name.
build closed -
run closed '' "
lookup harness api.anthropic.com
lookup host host.ap.internal
tcp host host.ap.internal $plain
tcp address $address $port
lookup other other.example
"
tried closed 'harness 0' 'host 2' 'host failed' 'address failed' 'other 2'
reached
decided closed \
  'deny  host.ap.internal  the default' \
  "deny  $address:$port  the default" \
  'deny  other.example  the default'
blocked closed \
  'host.ap.internal  NETWORK +host.ap.internal' \
  "$address  NETWORK +$address" \
  'other.example  NETWORK +other.example'

# Under a URL rule for your host, an allowed path reaches the upstream and
# a denied one doesn't. git's client trusts the guest's bundle, which holds
# the run's authority, and the proxy trusts the upstream's, which
# `SSL_CERT_FILE` names.
url="https://host.ap.internal:$tls"
build url - "+$url/allowed"
asked url "$url/allowed/repo/info/refs" \
  "+ $url/allowed/repo/info/refs  NETWORK +$url/allowed"
asked url "$url/denied/repo/info/refs" \
  "- $url/denied/repo/info/refs  the default"
run url "SSL_CERT_FILE=$PWD/upstream/ca.pem" "
echo \"allowed \$(timeout 20 git ls-remote $url/allowed/repo 2>&1 | head -1)\"
echo \"denied \$(timeout 20 git ls-remote $url/denied/repo 2>&1 | head -1)\"
"
case "$(sed -n 1p url.tried)" in
  'allowed '*'not found'*) ;;
  *)
    echo "the allowed path didn't reach the upstream:" >&2
    cat url.tried >&2
    exit 1
    ;;
esac
case "$(sed -n 2p url.tried)" in
  'denied '*'unable to access'*) ;;
  *)
    echo "the denied path wasn't refused:" >&2
    cat url.tried >&2
    exit 1
    ;;
esac
reached "tls /allowed/repo/info/refs?service=git-upload-pack"
decided url \
  "allow  host.ap.internal:$tls  NETWORK +$url/allowed" \
  "deny  host.ap.internal:$tls  the default"
blocked url 'host.ap.internal  NETWORK +host.ap.internal'

# Under an address rule, that address connects, and another doesn't. No
# name resolves, so your host's name doesn't either.
build address - "+$address"
asked address "$address:$port" "+ $address:$port  NETWORK +$address"
asked address "192.0.2.7:$port" "- 192.0.2.7:$port  the default"
run address '' "
tcp address $address $port
tcp other 192.0.2.7 $port
lookup host host.ap.internal
"
tried address 'address upstream:ping' 'other failed' 'host 2'
reached "address ping"
decided address \
  "allow  $address:$port  NETWORK +$address" \
  "deny  192.0.2.7:$port  the default" \
  'deny  host.ap.internal  the default'
blocked address \
  '192.0.2.7  NETWORK +192.0.2.7' \
  'host.ap.internal  NETWORK +host.ap.internal'
