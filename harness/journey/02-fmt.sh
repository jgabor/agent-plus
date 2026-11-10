#!/bin/sh
set -eu

# The Agentfile from SPEC.md's Example, as `ap fmt` writes it.
cat >expected <<'EOF'
AGENT claude:2.1.319
OS standard

SYSTEM
```
You are a helpful assistant that can do many things.

You are particularly good at developing Rust applications that are simple
to use, even when they are very complex underneath.
```

MODEL opus
EFFORT high
AUTH login

SECRET GITHUB_PERSONAL_ACCESS_TOKEN @api.githubcopilot.com
ENV LOG_LEVEL debug

REPO jgabor/agent-plus
PLUGIN https://github.com/anthropics/claude-plugins-official/tree/main/external_plugins/context7
SKILL https://github.com/anthropics/skills/tree/main/skills/algorithmic-art
MCP https://github.com/anthropics/claude-plugins-official/tree/main/external_plugins/github
ADD ./fixtures/database.sqlite
SHARE ./references
SHARE /var/log/output.log rw
WORKDIR ~/git/agent-plus

NETWORK -
NETWORK +kagi.com
NETWORK +github.com
NETWORK +mcp.context7.com
NETWORK +api.githubcopilot.com
NETWORK +https://codeberg.org/forgejo/forgejo
POLICY -
PORT 5173

COMPRESSION 3
EOF

# The same file, badly formatted, with CRLF line endings: leading and extra
# blank lines, lowercase names, extra spaces, trailing whitespace, and an
# end-of-line fence.
mkdir project
awk '{ printf "%s\r\n", $0 }' >project/Agentfile <<'EOF'


agent   claude:2.1.319  
os	standard



system ```
You are a helpful assistant that can do many things.

You are particularly good at developing Rust applications that are simple
to use, even when they are very complex underneath.
```   

Model opus
effort  high
AUTH login

secret  GITHUB_PERSONAL_ACCESS_TOKEN @api.githubcopilot.com
env LOG_LEVEL debug

repo jgabor/agent-plus
PLUGIN   https://github.com/anthropics/claude-plugins-official/tree/main/external_plugins/context7
skill https://github.com/anthropics/skills/tree/main/skills/algorithmic-art
mcp https://github.com/anthropics/claude-plugins-official/tree/main/external_plugins/github
ADD ./fixtures/database.sqlite
share ./references
SHARE /var/log/output.log rw	
workdir ~/git/agent-plus


network -
NETWORK +kagi.com
network +github.com
NETWORK  +mcp.context7.com
NETWORK +api.githubcopilot.com
NETWORK +https://codeberg.org/forgejo/forgejo
policy -
port 5173

compression 3


EOF
cp project/Agentfile messy

status=0
ap fmt --check project 2>stderr || status=$?
if [ "$status" -ne 1 ]; then
  echo "ap fmt --check on a badly formatted file exited $status, not 1" >&2
  exit 1
fi
if ! cmp -s project/Agentfile messy; then
  echo "ap fmt --check changed the file" >&2
  exit 1
fi

ap fmt project
if ! cmp -s project/Agentfile expected; then
  echo "ap fmt didn't write the expected file:" >&2
  diff expected project/Agentfile >&2 || true
  exit 1
fi

(cd project && ap fmt --check)
ap fmt project
if ! cmp -s project/Agentfile expected; then
  echo "formatting a formatted file changed it" >&2
  exit 1
fi

# Two errors: an unknown instruction on line 2, and a second MODEL on line 4.
printf 'AGENT claude\nNOPE x\nMODEL opus\nMODEL sonnet\n' >project/Agentfile
cp project/Agentfile broken
status=0
ap fmt project 2>stderr || status=$?
if [ "$status" -ne 1 ]; then
  echo "ap fmt on a file with errors exited $status, not 1" >&2
  exit 1
fi
if ! grep -q ':2: ' stderr || ! grep -q ':4: ' stderr; then
  echo "ap fmt didn't name both lines with errors:" >&2
  cat stderr >&2
  exit 1
fi
if ! cmp -s project/Agentfile broken; then
  echo "ap fmt changed a file with errors" >&2
  exit 1
fi
