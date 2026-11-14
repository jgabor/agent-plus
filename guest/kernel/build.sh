#!/bin/sh
# Builds the guest kernel from a pinned kernel.org source, in a pinned
# toolchain, and prints the SHA-256 of the result. It needs docker, and an
# x86-64 host or emulation. Run it through `cargo xtask run`.
set -eu

version=7.3.3
release=$version-ap.3
archive=linux-$version.tar.xz
url=https://cdn.kernel.org/pub/linux/kernel/v7.x/$archive
archive_sha256=79c66dde66cd96877fcde8f7d36aec946b1f423d2b97b5a7b1751b75c764f3d1

# The compiler is part of the build's input: another GCC gives another digest.
# The toolchain is this image, with packages from the Ubuntu archive as it was
# at this snapshot.
image=ubuntu@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852
snapshot=20261111T000000Z
packages='bc bison flex gcc libc6-dev libelf-dev make xz-utils'

# Inside the toolchain, the build always sees the same paths.
work=/work
here=$(cd "$(dirname "$0")" && pwd -P)

if [ "${1-}" = compile ]; then
  out=$work/target/guest-kernel
  source=$out/linux-$version

  gcc --version | head -n 1
  ld --version | head -n 1

  # Always a fresh tree, so that nothing from an earlier build reaches this
  # one.
  rm -rf "$source"
  tar -xJf "$out/$archive" -C "$out"

  # LOCALVERSION is set, and empty, so that the enclosing git checkout can't
  # add a "+" to the release string.
  kmake() {
    make -C "$source" -j"$(nproc)" ARCH=x86_64 LOCALVERSION= \
      KBUILD_BUILD_TIMESTAMP='1970-01-01 00:00:00 UTC' \
      KBUILD_BUILD_USER=ap KBUILD_BUILD_HOST=ap KBUILD_BUILD_VERSION=1 "$@"
  }

  kmake KCONFIG_ALLCONFIG="$here/config" allnoconfig

  # Kconfig drops an option whose dependencies aren't met, and says nothing.
  grep -E '^(CONFIG_|# CONFIG_.* is not set$)' "$here/config" |
    while read -r line; do
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
  exit 0
fi

out=$here/../../target/guest-kernel
mkdir -p "$out"
out=$(cd "$out" && pwd -P)

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

# The base image has no CA certificates, and the snapshot is served only over
# https, so the certificates come from the current archive. They can't reach
# the kernel.
docker build --iidfile "$out/toolchain.id" - <<DOCKERFILE
FROM --platform=linux/amd64 $image
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update -qq \\
 && apt-get install -qq -y --no-install-recommends ca-certificates \\
 && rm -f /etc/apt/sources.list.d/ubuntu.sources \\
 && printf '%s\\n' 'Types: deb' \\
      'URIs: https://snapshot.ubuntu.com/ubuntu/$snapshot/' \\
      'Suites: noble noble-updates noble-security' \\
      'Components: main universe' \\
      'Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg' \\
      > /etc/apt/sources.list.d/snapshot.sources \\
 && apt-get update -qq \\
 && apt-get install -qq -y --no-install-recommends $packages \\
 && rm -rf /var/lib/apt/lists/*
DOCKERFILE
toolchain=$(cat "$out/toolchain.id")

# No network: everything the build reads is already here.
docker run --rm --platform linux/amd64 --network none \
  --user "$(id -u):$(id -g)" \
  --volume "$here:$work/guest/kernel:ro" \
  --volume "$out:$work/target/guest-kernel" \
  "$toolchain" sh "$work/guest/kernel/build.sh" compile

sha256sum "$out/vmlinux"
