#!/bin/sh
set -eu

# The journey puts the fake harness next to `ap`. It reaches the agent as its
# Claude Code: the lockfile pins it by version and digest, and the download
# cache already holds it by that digest, so the build fetches nothing.
fake=$(command -v fake-harness)
if command -v sha256sum >/dev/null; then
  digest=$(sha256sum "$fake" | cut -d ' ' -f 1)
else
  digest=$(shasum -a 256 "$fake" | cut -d ' ' -f 1)
fi
mkdir -p "$HOME/.cache/ap" fake
cp "$fake" "$HOME/.cache/ap/$digest"

cat >fake/Agentfile <<'EOF'
AGENT claude:0.0.0
OS minimal
NETWORK -
ENV FAKE_HARNESS_WRITE ran
ENV FAKE_HARNESS_STATUS 3
EOF
echo "HARNESS claude 0.0.0 $digest" >fake/Agentfile.lock
printf 'NETWORK -\nPOLICY +\n' >grants
ap build --approve=grants fake </dev/null >/dev/null

prompt="Say what you can see."

# Runs the agent headless into `$1`, and fails unless `ap run` exits with
# the fake harness's status. The timeout stops a run that hangs, and with it
# the VM that its `ap run` holds.
run() {
  status=0
  timeout -k 10 120 ap run fake -p "$prompt" </dev/null >"$1" 2>"$1.err" ||
    status=$?
  if [ "$status" -ne 3 ]; then
    echo "ap run fake -p \"$prompt\" exited $status, not the harness's 3:" >&2
    cat "$1" "$1.err" >&2
    exit 1
  fi
  if [ "$(wc -l <"$1")" -ne 1 ] || ! grep -q '^{"arguments":' "$1"; then
    echo "ap run didn't stream the harness's one line of output:" >&2
    cat "$1" >&2
    exit 1
  fi
}

# Fails unless the report in `$1` has `$2`.
says() {
  if ! grep -qF -- "$2" "$1"; then
    echo "the harness didn't see $2:" >&2
    cat "$1" >&2
    exit 1
  fi
}

# The part of the report in `$1` that holds the array named `$2`.
array() {
  sed -n "s/.*\"$2\":\[\([^]]*\)\].*/\1/p" "$1"
}

run first
says first '"FAKE_HARNESS_WRITE":"ran"'
says first '"user":"agent"'
says first '"directory":"/home/agent"'
says first '"HOME":"/home/agent"'
case "$(array first arguments)" in
  *"\"$prompt\""*) ;;
  *)
    echo "the harness didn't get the prompt:" >&2
    cat first >&2
    exit 1
    ;;
esac
case "$(array first interfaces)" in
  '"lo"') ;;
  *)
    echo "the harness sees a network interface besides loopback, or none:" >&2
    cat first >&2
    exit 1
    ;;
esac

run second
case "$(array second files)" in
  *'"ran"'*)
    echo "a file the first run wrote outlasted it:" >&2
    cat second >&2
    exit 1
    ;;
esac
