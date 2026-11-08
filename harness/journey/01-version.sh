#!/bin/sh
set -eu

version=$(ap --version)
case "$version" in
  "ap "[0-9]*.[0-9]*.[0-9]*) ;;
  *)
    echo "unexpected version output: $version" >&2
    exit 1
    ;;
esac
