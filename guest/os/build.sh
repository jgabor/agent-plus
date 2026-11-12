#!/bin/sh
# Builds the minimal OS layer from Debian 13 at one snapshot.debian.org
# timestamp, and prints the SHA-256 of the image. Run it through
# `cargo xtask run`.
#
# Debian's tools run in a container, so nothing is installed on this machine,
# and our EROFS writer makes the image, so no C tool does.
set -eu

snapshot=20261110T000000Z
# debian:trixie, which runs debian.sh.
image=debian@sha256:9c6e381421d0f48d1b8133e573f33c185ee16cbf505eda9c4a8a4791b25ea8a6
releases="0584fba32e13e0ab8285fb16c27adea1ec03a73669c18702821094fd6ca86675 debian trixie
f2295bafcea87b568e471c54ae551232b84c37f0e6f2e300801e1703b1595ed8 debian trixie-updates
8cfb86ff1190b904539e1c8c2069e9b373ec8add28f59cdab7f4839edb0cb5b6 debian-security trixie-security"

here=$(cd "$(dirname "$0")" && pwd -P)
out=$here/../../target/guest-os
tree=$out/tree
mkdir -p "$out"

# Every time in the root is clamped to the snapshot's.
epoch=$(echo "$snapshot" | sed -E 's/(....)(..)(..)T(..)(..)(..)Z/\1-\2-\3 \4:\5:\6Z/')
epoch=$(date -u -d "$epoch" +%s)
# The tar comes back on stdout, so the container writes nothing here.
docker run --rm -i --env SOURCE_DATE_EPOCH="$epoch" \
  --env DEBIAN_FRONTEND=noninteractive "$image" \
  sh -s "$snapshot" "$releases" < "$here/debian.sh" > "$out/root.tar.part"
mv "$out/root.tar.part" "$out/root.tar"

# Always a fresh tree, so that nothing from an earlier build reaches this one.
# The guest mounts a devtmpfs on /dev, so its device nodes stay out.
if [ -d "$tree" ]; then
  chmod -R u+w "$tree"
  rm -rf "$tree"
fi
mkdir "$tree"
tar -xpf "$out/root.tar" -C "$tree" --exclude='./dev/*'

rm -f "$out/minimal.erofs"
cargo run --quiet --release -p erofs --bin mkerofs -- \
  "$tree" "$out/minimal.erofs.part"
AP_OS_TREE=$tree AP_OS_IMAGE=$out/minimal.erofs.part \
  cargo test --quiet --release -p erofs --test round_trip -- \
  --ignored --exact the_os_layer_holds_its_tree > /dev/null
mv "$out/minimal.erofs.part" "$out/minimal.erofs"

dpkg_status=$tree/var/lib/dpkg/status
awk '/^Package:/ { name = $2 } /^Version:/ { print name, $2 }' "$dpkg_status" \
  > "$out/packages.txt"
sha256sum "$out/minimal.erofs"
