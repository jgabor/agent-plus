#!/bin/sh
set -eu

# Runs the fake harness with a hidden secret bound to your host, given in
# the environment, and tries it from inside the guest with `ap exec`. The
# journey puts the fake upstream next to `ap`, and its HTTPS listener
# answers on your host's loopback, which the guest reaches as
# `host.ap.internal`. The checks read what the guest holds, and what
# reached the upstream. Then a run without the secret's value must fail
# before the harness starts.

# The journey puts the fake harness next to `ap`. It reaches the agent as
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

# The upstream says where it listens on its first line, and keeps each
# HTTPS request whole in `upstream/requests`. When the script ends, its
# trap stops the upstream, and the run that a failed check left going. The
# timeouts stop them if the script dies without its trap.
mkdir upstream
mkfifo upstream.out
timeout -k 5 900 fake-upstream upstream >upstream.out &
upstream=$!
running=
trap 'kill $upstream $running 2>/dev/null || true' EXIT
read -r _ _ _ tls _ <upstream.out || true
if [ -z "${tls:-}" ]; then
  echo "fake-upstream didn't say where it listens" >&2
  exit 1
fi

# The secret's value. A pattern that matches it, with one character in
# brackets, lets the guest look for it without being handed it.
value=journey-hidden-value-5c9e27b4d1
pattern='j[o]urney-hidden-value-5c9e27b4d1'

mkdir hidden
cat >hidden/Agentfile <<'EOF'
AGENT claude:0.0.0
OS minimal
NETWORK -
NETWORK +host.ap.internal
SECRET SECRET @host.ap.internal
ENV FAKE_HARNESS_SLEEP 300
EOF
echo "HARNESS claude 0.0.0 $digest" >hidden/Agentfile.lock
cat >hidden.grants <<'EOF'
AGENT claude
SECRET SECRET @host.ap.internal
NETWORK -
NETWORK +host.ap.internal
POLICY +
EOF
if ! ap build --approve=hidden.grants hidden </dev/null >/dev/null \
  2>hidden.build; then
  echo "ap build hidden failed:" >&2
  cat hidden.build >&2
  exit 1
fi

# Fails with what `$1` holds, under the message that follows it.
show() {
  file=$1
  shift
  echo "$*:" >&2
  cat "$file" >&2
  exit 1
}

# Runs the agent headless with the secret's value in its environment, and
# the upstream's authority in `SSL_CERT_FILE`, so the proxy trusts it. The
# guest's script prints what its variable holds, then, for a pattern, how
# many processes' environments and arguments, and how many files, match.
# A file and a process's arguments hold the placeholder too, so the search
# for it shows that each search finds what it looks for. Then it sends one
# request over TLS to your host whose `Host` names it, and one whose `Host`
# names another host, each with the placeholder in the query, in
# `Authorization`, which the adapter allows, and in `X-Other`, which it
# doesn't. The timeout stops a run that hangs, and with it the VM that its
# `ap run` holds.
mkfifo hidden.out
env SECRET="$value" SSL_CERT_FILE="$PWD/upstream/ca.pem" \
  timeout -k 10 240 ap run hidden -p 'Use the secret.' </dev/null \
  >hidden.out 2>hidden.err &
running=$!
exec 4<hidden.out
if ! read -r report <&4 || [ "${report#\{\"arguments\":}" = "$report" ]; then
  show hidden.err "ap run hidden didn't start the harness"
fi
name=$(ap ps </dev/null | sed -n '2s/ .*//p')
try="
search() {
  environ=0
  cmdline=0
  for f in /proc/[0-9]*/environ; do
    grep -qaE -- \"\$2\" \"\$f\" 2>/dev/null && environ=\$((environ + 1))
  done
  for f in /proc/[0-9]*/cmdline /proc/cmdline; do
    grep -qaE -- \"\$2\" \"\$f\" 2>/dev/null && cmdline=\$((cmdline + 1))
  done
  files=\$(grep -rlsaE -D skip --exclude-dir=proc --exclude-dir=sys -- \"\$2\" / | wc -l)
  echo \"\$1 environ \$environ cmdline \$cmdline files \$files\"
}
send() {
  answer=\$({
    printf 'GET /%s?key=%s HTTP/1.1\r\nHost: %s\r\nAuthorization: Bearer %s\r\nX-Other: %s\r\n\r\n' \
      \"\$1\" \"\$SECRET\" \"\$2\" \"\$SECRET\" \"\$SECRET\"
    sleep 3
  } |
    timeout 20 openssl s_client -quiet -no_ign_eof -servername host.ap.internal \
      -connect host.ap.internal:$tls 2>/dev/null | head -1 | tr -d '\r')
  echo \"\$1 \$answer\"
}
echo \"held \$SECRET\"
printf '%s' \"\$SECRET\" >\"\$(mktemp)\"
bash -c 'sleep 60; true' holder \"\$SECRET\" &
holder=\$!
until grep -qa -- \"\$SECRET\" \"/proc/\$holder/cmdline\" 2>/dev/null; do sleep 0.1; done
search placeholder \"\$SECRET\"
search value '$pattern'
kill \"\$holder\"
send bound host.ap.internal:$tls
send other other.example
"
case "$try" in
  *"$value"*)
    echo "the guest's script holds the value" >&2
    exit 1
    ;;
esac
if ! timeout -k 5 300 ap exec "$name" bash -c "$try" </dev/null \
  >hidden.tried 2>hidden.tried.err; then
  cat hidden.tried.err >>hidden.tried
  show hidden.tried "ap exec $name failed"
fi
if ! timeout -k 5 60 ap stop "$name" </dev/null >/dev/null 2>hidden.stop; then
  show hidden.stop "ap stop $name failed"
fi
cat <&4 >/dev/null
exec 4<&-
status=0
wait "$running" || status=$?
running=
if [ "$status" -ne 143 ]; then
  show hidden.err "ap run hidden exited $status after ap stop, not 143"
fi

# The guest holds a placeholder, which isn't the value, in its variable,
# and finds it in environments, arguments, and a file. It finds the value
# nowhere, and both requests got the upstream's answer.
placeholder=$(sed -n 's/^held //p' hidden.tried)
if [ -z "$placeholder" ] || [ "$placeholder" = "$value" ]; then
  show hidden.tried "the guest's variable didn't hold a placeholder"
fi
case "$(sed -n 2p hidden.tried)" in
  'placeholder environ '[1-9]*' cmdline '[1-9]*' files '[1-9]*) ;;
  *) show hidden.tried "the guest didn't find its placeholder" ;;
esac
printf '%s\n' "held $placeholder" "$(sed -n 2p hidden.tried)" \
  'value environ 0 cmdline 0 files 0' \
  'bound HTTP/1.1 404 Not Found' 'other HTTP/1.1 404 Not Found' \
  >hidden.expected
if ! cmp -s hidden.tried hidden.expected; then
  echo "the guest should have printed:" >&2
  cat hidden.expected >&2
  show hidden.tried "but printed"
fi

# The bound host got the value in `Authorization` alone, and the
# placeholder in the query and `X-Other`. The request whose `Host` names
# another host got the placeholder unchanged in every place.
bound=$(grep -l '^GET /bound?' upstream/requests/* || true)
other=$(grep -l '^GET /other?' upstream/requests/* || true)
if [ -z "$bound" ] || [ -z "$other" ]; then
  echo "the upstream didn't get both requests:" >&2
  cat upstream/requests/* >&2
  exit 1
fi
cr=$(printf '\r')
printf '%s\r\n' "GET /bound?key=$placeholder HTTP/1.1" \
  "Host: host.ap.internal:$tls" "Authorization: Bearer $value" \
  "X-Other: $placeholder" '' >bound.expected
printf '%s\r\n' "GET /other?key=$placeholder HTTP/1.1" \
  'Host: other.example' "Authorization: Bearer $placeholder" \
  "X-Other: $placeholder" '' >other.expected
for request in bound other; do
  eval "kept=\$$request"
  if ! cmp -s "$kept" "$request.expected"; then
    echo "the $request request should have reached the upstream as:" >&2
    sed "s/$cr\$//" "$request.expected" >&2
    echo "but reached it as:" >&2
    sed "s/$cr\$//" "$kept" >&2
    exit 1
  fi
done

# Without the secret's value in the environment, the run fails before the
# harness starts, and the error names the secret.
status=0
timeout -k 10 120 ap run hidden -p 'Use the secret.' </dev/null \
  >unset.out 2>unset.err || status=$?
if [ "$status" -ne 125 ]; then
  show unset.err "ap run without the secret exited $status, not 125"
fi
if ! grep -q 'SECRET SECRET' unset.err; then
  show unset.err "ap run without the secret didn't name it"
fi
if [ -s unset.out ]; then
  show unset.out "ap run without the secret started the harness"
fi
