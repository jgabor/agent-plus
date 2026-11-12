#!/bin/sh
# Runs in the container that build.sh starts, and writes the root file system
# of the minimal tier to stdout as a tar. Its arguments are the snapshot
# timestamp, and the pinned SHA-256 of each suite's InRelease.
set -eu

export snapshot="$1" releases="$2"
archive=http://snapshot.debian.org/archive
sources="deb $archive/debian/$snapshot trixie main
deb $archive/debian/$snapshot trixie-updates main
deb $archive/debian-security/$snapshot trixie-security main"

# Checks each InRelease that apt fetched into the root given as $1. Each one
# is signed, and lists the digests of its package indexes, which list the
# digests of their packages, and apt checks both.
cat > /check-releases <<'EOF'
#!/bin/sh
set -eu
echo "$releases" | while read -r sum name suite; do
  file=$1/var/lib/apt/lists/snapshot.debian.org_archive_${name}_${snapshot}
  if ! echo "$sum  ${file}_dists_${suite}_InRelease" | sha256sum -c --status
  then
    echo "build.sh: the $suite InRelease doesn't match its SHA-256" >&2
    exit 1
  fi
done
EOF
chmod +x /check-releases

# The tools that build the root come from the same snapshot.
rm -f /etc/apt/sources.list.d/*
echo "$sources" > /etc/apt/sources.list
apt-get -qq -o Acquire::Check-Valid-Until=false update >&2
/check-releases /
apt-get -qq -y --no-install-recommends install mmdebstrap > /dev/null

# The essential packages hold a shell and coreutils. Documentation, manuals,
# and translations are left out, except for each package's copyright file.
# mmdebstrap copies the container's resolv.conf and hostname into the root,
# so the last hook replaces them.
mmdebstrap --quiet --mode=root --variant=essential --include=git \
  --aptopt='Acquire::Check-Valid-Until "false"' \
  --extract-hook='/check-releases "$1"' \
  --dpkgopt='path-exclude=/usr/share/doc/*' \
  --dpkgopt='path-include=/usr/share/doc/*/copyright' \
  --dpkgopt='path-exclude=/usr/share/man/*' \
  --dpkgopt='path-exclude=/usr/share/info/*' \
  --dpkgopt='path-exclude=/usr/share/locale/*' \
  --customize-hook='rm -f "$1/etc/resolv.conf"' \
  --customize-hook='echo localhost > "$1/etc/hostname"' \
  trixie - "$sources"
