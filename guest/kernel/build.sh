#!/bin/sh
# Builds the guest kernel from a pinned kernel.org source, and prints the
# SHA-256 of the result. Run it through `cargo xtask run`.
set -eu

version=7.3.3
release=$version-ap.2
archive=linux-$version.tar.xz
url=https://cdn.kernel.org/pub/linux/kernel/v7.x/$archive
archive_sha256=79c66dde66cd96877fcde8f7d36aec946b1f423d2b97b5a7b1751b75c764f3d1

here=$(cd "$(dirname "$0")" && pwd -P)
out=$here/../../target/guest-kernel
source=$out/linux-$version
mkdir -p "$out"

if [ ! -f "$out/$archive" ]; then
  curl --fail --silent --show-error --location --proto '=https' \
    --output "$out/$archive.part" "$url"
  mv "$out/$archive.part" "$out/$archive"
fi
if ! echo "$archive_sha256  $out/$archive" | sha256sum --check --status; then
  rm -f "$out/$archive"
  echo "build.sh: $archive doesn't match its pinned SHA-256" >&2
  exit 1
fi

# Always a fresh tree, so that nothing from an earlier build reaches this one.
rm -rf "$source"
tar -xJf "$out/$archive" -C "$out"

# LOCALVERSION is set, and empty, so that the enclosing git checkout can't add
# a "+" to the release string.
kmake() {
  make -C "$source" -j"$(nproc)" ARCH=x86_64 LOCALVERSION= \
    KBUILD_BUILD_TIMESTAMP='1970-01-01 00:00:00 UTC' \
    KBUILD_BUILD_USER=ap KBUILD_BUILD_HOST=ap KBUILD_BUILD_VERSION=1 "$@"
}

kmake KCONFIG_ALLCONFIG="$here/config" allnoconfig

# Kconfig drops an option whose dependencies aren't met, and says nothing.
grep -E '^(CONFIG_|# CONFIG_.* is not set$)' "$here/config" | while read -r line
do
  if ! grep -qxF -- "$line" "$source/.config"; then
    echo "build.sh: the kernel config lost: $line" >&2
    exit 1
  fi
done

kmake vmlinux

built=$(kmake -s kernelrelease)
if [ "$built" != "$release" ]; then
  echo "build.sh: built $built, expected $release" >&2
  exit 1
fi

cp "$source/vmlinux" "$out/vmlinux"
sha256sum "$out/vmlinux"
