#!/bin/sh
set -eu

# Runs step 12 under Codex. It reads step 12's own text, so a later change
# to step 12 reaches this step too. In step 12's Agentfile, two lines
# change: `AGENT` names Codex, and `MODEL` names a Codex model, because
# `MODEL` passes a name through unchanged, and no name works in both
# harnesses. Its grants change only in `AGENT`. The model is the cheapest
# one Codex lists, as step 12's is the cheapest Claude model. The journey
# gives this step the OpenAI paid checks key as `CODEX_API_KEY`, the
# variable that Codex's `AUTH key` reads, so step 12's checks for its key
# read that variable instead. Nothing this step prints may hold the key.

model=gpt-6-luna
twelve="$(dirname "$0")/12-prompt.sh"
sed -e 's/^AGENT claude$/AGENT codex/' \
  -e "s/^printf 'AGENT claude\\\\n/printf 'AGENT codex\\\\n/" \
  -e "s/^MODEL .*/MODEL $model/" \
  -e 's/ANTHROPIC_API_KEY/CODEX_API_KEY/g' \
  -e 's/Claude Code/Codex/g' \
  "$twelve" >step.sh

# Fails unless step 12's text still has the shape the lines above change,
# so this step can't quietly run Claude Code, a Claude model, or Codex's
# default model.
has() {
  if [ "$(grep -c -e "$1" step.sh)" -ne "$2" ]; then
    echo "$twelve has changed, so step 13 can't build on it:" \
      "expected $2 lines matching $1" >&2
    exit 1
  fi
}
has '^AGENT codex$' 1
has "^printf 'AGENT codex\\\\n" 1
has 'AGENT claude' 0
has "^MODEL $model\$" 1
has 'MODEL' 1
has 'ANTHROPIC_API_KEY' 0
has 'CODEX_API_KEY:-' 1

exec /bin/sh step.sh
