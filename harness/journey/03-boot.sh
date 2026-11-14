#!/bin/sh
set -eu

release=$(ap boot --timeout 30)
case "$release" in
  *-ap.3) ;;
  *)
    echo "unexpected guest kernel release: $release" >&2
    exit 1
    ;;
esac
if [ "$release" = "$(uname -r)" ]; then
  echo "the release is the host's: $release" >&2
  exit 1
fi
