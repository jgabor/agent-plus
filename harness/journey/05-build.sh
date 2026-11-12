#!/bin/sh
set -eu

store="$HOME/.local/share/ap"

mkdir -p notes/src notes/out
cat >notes/Agentfile <<'EOF'
AGENT claude
OS minimal
SYSTEM ./system.md
PROMPT Review the notes.
MODEL opus
ADD ./data
ADD ./src /work
SHARE ./out
EOF
echo "Be careful." >notes/system.md
echo "one" >notes/data
echo "fn main() {}" >notes/src/main.rs
# The approval of these grants is step 6's.
printf 'NETWORK +\nPOLICY +\n' >grants

ap build --approve=grants notes >first
if ! grep -qx 'fetched OS minimal' first || [ "$(grep -c '^built ' first)" -ne 3 ] ||
  ! grep -qx 'agent notes' first; then
  echo "the first build didn't fetch the OS and build three layers:" >&2
  cat first >&2
  exit 1
fi
if [ ! -f "$store/agents/notes" ]; then
  echo "the store has no agent named notes" >&2
  exit 1
fi
if [ "$(grep -c '^LAYER ' "$store/agents/notes")" -ne 4 ] ||
  [ "$(find "$store/zstd-layers" -type f | wc -l)" -ne 4 ]; then
  echo "the store doesn't hold the agent's four layers" >&2
  exit 1
fi

# Every file and directory in the store, with what a write would change.
snapshot() {
  find "$store" -exec stat -c '%n %i %s %y' {} + | sort
}

snapshot >before
ap build --approve=grants notes >second
if [ "$(grep -c '^hit ' second)" -ne 4 ] || grep -q '^built \|^fetched ' second; then
  echo "a rebuild with nothing changed wasn't a hit for every layer:" >&2
  cat second >&2
  exit 1
fi
snapshot >after
if ! cmp -s before after; then
  echo "a rebuild with nothing changed touched the store:" >&2
  diff before after >&2 || true
  exit 1
fi

echo "two" >notes/data
ap build --approve=grants notes >third
if [ "$(grep -c '^built \|^fetched ' third)" -ne 1 ] ||
  ! grep -q '^built ADD ./data$' third; then
  echo "changing one added file didn't rebuild only its layer:" >&2
  cat third >&2
  exit 1
fi
if [ "$(find "$store/zstd-layers" -type f | wc -l)" -ne 4 ]; then
  echo "the rebuild left a layer that nothing uses" >&2
  exit 1
fi
