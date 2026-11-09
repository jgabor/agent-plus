#!/bin/sh
set -eu

version=$(ap boot --timeout 30 -- ap --version)
if [ "$version" != "$(ap --version)" ]; then
  echo "unexpected version from the guest: $version" >&2
  exit 1
fi

status=0
ap boot --timeout 30 -- ap --no-such-flag >stdout 2>stderr || status=$?
if [ "$status" -ne 2 ]; then
  echo "a bad flag in the guest exited $status, not 2" >&2
  exit 1
fi
if [ -s stdout ] || ! grep -q usage stderr; then
  echo "the guest's usage message didn't arrive on stderr alone" >&2
  exit 1
fi

status=0
ap boot --timeout 30 -- no-such-program || status=$?
if [ "$status" -ne 127 ]; then
  echo "a missing program in the guest exited $status, not 127" >&2
  exit 1
fi
