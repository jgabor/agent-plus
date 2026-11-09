# Agentfile reference

> **Status: draft.** Nothing described here is implemented. Items marked
> *Proposed* are design proposals that still need review.

Agent+ builds agents from the instructions in an Agentfile. An Agentfile is
a text file that declares which coding-agent harness to run, what the agent
knows, what it can reach, and what it may keep. `ap build` turns an
Agentfile into an image in the local store. `ap run` starts that image in an
isolated Linux microVM.

This page describes the instructions you can use in an Agentfile.

## Overview

The Agentfile supports the following instructions:

| Instruction | Phase | Repeatable | Description |
|---|---|---|---|
| [`AGENT`](#agent) | build | no | Set the harness and its version. Required, and must come first. |
| [`OS`](#os) | build | no | Choose the OS tier: `minimal`, `standard`, or `desktop`. |
| [`SYSTEM`](#system) | build | yes | Set the harness's global instructions. |
| [`PROMPT`](#prompt) | build | no | Embed the prompt for repeated or scheduled runs. Makes runs headless. |
| [`MODEL`](#model) | build | no | Set the model. |
| [`EFFORT`](#effort) | build | no | Set the reasoning effort. |
| [`MODE`](#mode) | build | no | Set how much the harness does without asking. |
| [`ADD`](#add) | build | yes | Copy host files into the image. |
| [`REPO`](#repo) | build | yes | Clone a git repository into the image. |
| [`PLUGIN`](#plugin) | build | yes | Install a plugin, translated for the harness. |
| [`SKILL`](#skill) | build | yes | Install Agent Skills. |
| [`MCP`](#mcp) | build | yes | Register MCP servers. |
| [`COMPRESSION`](#compression) | build | no | Set the zstd level for this Agentfile's layers. |
| [`ENV`](#env) | run | yes | Set environment variables the agent can read. |
| [`SECRET`](#secret) | run | yes | Provide a credential the agent can use without reading it. |
| [`SEAL`](#seal) | build | yes | Encrypt a credential into the image, for handing a working agent to someone. |
| [`AUTH`](#auth) | run | no | Choose how the harness signs in. |
| [`SHARE`](#share) | run | yes | Share a host file or directory with the agent. |
| [`WORKDIR`](#workdir) | run | no | Choose where the harness starts. |
| [`PERSIST`](#persist) | run | yes | Name the state that survives between runs. |
| [`PERMISSIONS`](#permissions) | run | yes | Allow or deny reads and writes of guest files. |
| [`PORT`](#port) | run | yes | Forward a guest port to the host. |
| [`NETWORK`](#network) | run | yes | Allow or deny network destinations. |
| [`POLICY`](#policy) | run | yes | Choose which other agents this agent may call. |

Every instruction has a phase:

- **Build** instructions are resolved when you run `ap build`. Their content
  is captured into the image.
- **Run** instructions are stored in the image as declarations and bound
  each time the agent starts. A run instruction never bakes content from
  your disk or environment into the image.

`ap build` reports every error it can find in the Agentfile itself. Errors
that depend on your host at run time, such as a missing secret or an unset
variable, fail the run.

## Format

An Agentfile is a UTF-8 text file. The default name is `Agentfile`. Line
endings are normalized to LF when the file is read.

```
# Comment
INSTRUCTION arguments
```

- Instructions are case-insensitive. By convention they are uppercase.
- Each instruction occupies one line. Text values may continue in a fenced
  block (see [Text values](#text-values)).
- A line whose first non-whitespace character is `#` is a comment. There are
  no trailing comments: a `#` anywhere else is part of the argument, because
  prompts, URLs, and paths contain `#`.
- Blank lines are ignored outside fenced blocks.
- There is no line continuation. Use a fenced block for long text, or repeat
  the instruction.
- `AGENT` must be the first instruction, and it appears exactly once.
- Unknown instructions, and a non-repeatable instruction that appears twice,
  are errors.
- Apart from `AGENT`, which comes first, instruction order matters only for
  text: repeated `SYSTEM` values concatenate in file order, and instructions
  from plugins follow in `PLUGIN` order.

### Arguments

Arguments are separated by whitespace. Double quotes group characters into
one argument. Inside quotes, `\"` is a quote and `\\` is a backslash. The
values of `SYSTEM`, `PROMPT`, and `ENV` follow their own rule (see
[Text values](#text-values)).

```
SHARE "./My Notes"
```

An Agentfile has no flags. An unquoted argument that starts with `--` is an
error, which keeps flags available to later versions.

A few instructions end in an optional word that sets a mode.

```
SHARE ./out rw
REPO jgabor/agent-plus history
SECRET AWS_SECRET_ACCESS_KEY visible
```

The word is matched exactly and must be the last argument. To use one of
these words as a destination, write it as a path, such as `./rw`.

### Text values

`SYSTEM` and `PROMPT` take text in one of three forms.

**Single-line text.** If the rest of the line is exactly one quoted string,
the value is that string's content. Otherwise, the value is the rest of the
line, verbatim, quotes included.

```
SYSTEM You are a careful reviewer.
SYSTEM "Be concise."
SYSTEM Say "hello" first.
```

The values are `You are a careful reviewer.`, `Be concise.`, and
`Say "hello" first.` In the third line, the rest of the line is not one
quoted string, so the quotes are kept.

**Bare path.** A value that is one argument and starts with an unquoted
`./`, `../`, `/`, or `~/` is a path. The file is read at build time on the
host and its content is captured into the image. A missing file is an
error, never silently treated as text. Quote the whole value to use it as
text.

```
SYSTEM ./prompts/system.md
PROMPT "/review"
```

Here `/review` is a slash command, so it is quoted. Unquoted, `ap build`
would look for a file named `/review` and fail.

A path with spaces stays one argument if you quote only the part that has
them. `ap` removes those quotes.

```
SYSTEM ./"My Prompts"/system.md
```

**Fenced block.** For multi-line text, use a CommonMark-style code fence.

````
SYSTEM
```
You are a careful reviewer.

# Rules
Prefer small diffs.
```
````

Fence rules:

- The opening fence is either the line directly after the instruction, or
  the instruction's entire argument (`SYSTEM ``` `). After a bare `SYSTEM`
  or `PROMPT`, the very next line must be the opening fence, starting in the
  first column. A blank line or an indented fence in between is an error.
- Text followed by backticks on the instruction line is single-line text,
  not a fence. `ap fmt` rewrites the end-of-line form to the next-line form.
- An opening fence is three or more backticks, optionally followed by an
  info string. The info string is ignored, so ` ```markdown ` works.
- The block ends at the first line that starts in the first column and
  holds only backticks, at least as many as the opening fence, optionally
  followed by whitespace. To put a code block inside the text, open with
  four backticks.

  `````
  SYSTEM
  ````
  Answer in this format:

  ```json
  {"verdict": "approve"}
  ```
  ````
  `````

- Content lines are kept verbatim, including indentation. Lines starting
  with `#` are content, not comments, and `$` is never expanded.
- The value is the lines between the fences, joined with newlines and
  ending with a newline.
- An empty block and an unclosed fence are errors.

### Paths

Host paths appear in text paths, `ADD`, `SHARE`, and local sources. They
resolve like this:

- A relative path resolves against the directory that contains the
  Agentfile. That directory is the build context.
- `~/` is your home directory on the host.
- Absolute paths are used as-is.
- Before any check, host paths are canonicalized: `.` and `..` are resolved
  and symlinks are followed, including a symlink you name directly. A
  symlink inside the build context that points outside it counts as
  outside. Symlinks found inside a directory that `ADD` copies are handled
  differently (see [ADD](#add)).
- After the check, `ap` opens the canonical path it checked, refusing
  symlinks along the way, and reads or shares it only through that open
  handle. If a symlink was swapped in after the check, the build or run
  fails instead of following it.

Guest paths appear as destinations. `~/` and relative guest paths resolve
under `/home/agent`, the agent's home directory. Guest paths are normalized
before any check.

When `ADD` or `SHARE` has no destination, the guest path follows one
**mirror rule**.

| Host path | Guest path |
|---|---|
| `./data` | `~/data` |
| `./fixtures/db.sqlite` | `~/fixtures/db.sqlite` |
| `.` | `~/<name of the Agentfile's directory>` |
| `~/notes` | `~/notes` |
| `/var/log/output.log` | `/var/log/output.log` |
| `../shared` | error: give an explicit destination |

Destination rules:

- Two destinations where one equals or contains the other are an error,
  whichever instructions they come from. For example, `ADD ./git`
  collides with `REPO`'s `~/git/<name>`.
- These guest paths are reserved. A destination that equals, contains, or
  is inside one of them is an error.
  - System directories: `/bin`, `/boot`, `/dev`, `/etc`, `/lib*`, `/proc`,
    `/run`, `/sbin`, `/sys`, `/usr`
  - Harness configuration directories, such as `~/.claude` and `~/.codex`.
    Use `PLUGIN`, `SKILL`, `MCP`, `AUTH`, and `PERSIST` instead.

### Variables

`$NAME` and `${NAME}` expand only in `ENV` values. They expand at run time,
from the host environment of the `ap` process that starts the run (see
[Agents calling agents](#agents-calling-agents)).

- `${NAME:-default}` uses `default` when `NAME` is unset or empty.
- An unset variable without a default makes the run fail, and the error
  names the variable.
- Quotes don't stop expansion. In `ENV GREETING "Hello, $USER"`, `$USER`
  expands.
- `\$` is a literal dollar sign.

Everywhere else, `$` is literal.

### Sources

`REPO`, `PLUGIN`, `SKILL`, and `MCP` take a source:

| Form | Example | Meaning |
|---|---|---|
| Shorthand | `anthropics/skills/skills/algorithmic-art@main` | A GitHub repository (`owner/repo`), with an optional subdirectory and an optional `@ref` |
| Forge URL | `https://github.com/anthropics/skills/tree/main/skills/algorithmic-art` | A repository page; the ref and subdirectory are read from the URL. *Proposed:* GitHub, GitLab, and Forgejo (including Codeberg) URL layouts |
| Git URL | `https://codeberg.org/forgejo/forgejo` | A repository root at its default branch, with an optional `@ref` |
| Local path | `./my-plugin` | A directory on the host, captured at build time |

A ref in a shorthand or Git URL runs from the first `@` in the path to the
end, so it may contain `/`, as in `torvalds/linux@v6.12` or
`https://codeberg.org/forgejo/forgejo@release/v1`.

Every ref resolves to a commit at build time and is recorded in the
lockfile. A local path is useful when you develop a plugin or skill and want
to try it in a sandbox.

`ap build` fetches each source without credentials first. A private source
then uses your own git credentials: your ssh-agent and the credential
helpers in your global git configuration. They are used on your host, only
during the build, and they never reach the guest. A source that needs them
is a [grant](#grants), because an Agentfile could otherwise copy your
private code into its agent.

## Grants

Grants are what an Agentfile asks of your host. `ap` asks you to approve
them before it uses them.

| Grant | Comes from |
|---|---|
| Host paths outside the build context | Text paths, `ADD`, local sources, `SHARE` |
| Write access to host files | `SHARE` with `rw` |
| Host environment variables | `$NAME` in `ENV` values |
| Secrets, their bound hosts, and their visibility | `SECRET`, `SEAL`, `AUTH key`, `AUTH token` |
| Your git credentials, for sources that need them | `REPO`, `PLUGIN`, `SKILL`, `MCP` |
| Network policy | `NETWORK`, including the default |
| Other agents it may call | `POLICY`, including the default |

How approval works:

- On the first `ap build` of an Agentfile, `ap` lists its grants and asks
  you to approve them. The approval is stored with the Agentfile's
  canonical path, not with the agent's name, so another directory never
  inherits it.
- A later build that **widens** a grant asks again and shows what changed.
  It doesn't matter who edited the Agentfile, so a wider grant that arrives
  through `git pull` or `ap apply` is caught at your next build. These
  changes widen a grant.
  - a host path outside the build context that you haven't approved,
    including an approved path whose canonical location changed
  - write access (`rw`) to a path that didn't have it
  - a new host environment variable
  - a new secret, a new bound host, or a secret that becomes `visible` or
    moves from `SECRET` to `SEAL`
  - adding `AUTH key` or `AUTH token`, or changing one to the other
  - a source that newly needs your git credentials
  - any network change other than removing a `+` rule or adding a `-` rule,
    including a change in the harness hosts that stay allowed under
    `NETWORK -`
  - a `POLICY` change that lets the agent call an agent it couldn't before

  A build that only narrows grants does not ask, and neither does a change
  that touches no grant, such as a new `PLUGIN`.
- Share sources resolve at run time, so they are checked again at every
  run. If a share now resolves somewhere you haven't approved, for example
  because `data` became a symlink to `~/.ssh`, the run asks again.
- The build context is pinned. `ap build` records its canonical path, and
  a run with a relative `SHARE` fails if that path no longer canonicalizes
  to itself, for example because a directory in it became a symlink.
  Otherwise, an agent that can write to the directory above it could
  replace the build context with a link to `~`, and `SHARE .` would share
  your home without asking.
- Without a terminal, a build or run that needs approval fails.
  `--approve=<file>`, on `ap build` and `ap run`, approves exactly the
  grants listed in `<file>`, in the format that `ap inspect --grants`
  prints, and fails if the Agentfile asks for more. In CI, keep that file
  where the code under test can't change it.
- Approval prompts escape control characters and bidirectional overrides
  in paths, names, and hosts, so a grant shows as it is. `ap diff` and
  `ap logs` do the same for names that an agent chose.
- A run that another agent starts never rebuilds (see
  [Agents calling agents](#agents-calling-agents)). When it needs approval,
  `ap` asks you, on the terminal or control API client that started the
  first run, never the calling agent.
- Options you type on the command line (for example, `ap run --share .`)
  are already your decision and need no approval. An agent that starts
  another agent can pass a prompt, but no options.
- Images that weren't built from a local Agentfile (pulled or exported)
  have no build context. They ask for approval on their first run, and the
  approval is stored with the image's digest. Every `SHARE` in such an image
  is a grant, approved by the path it resolves to, and a relative `SHARE`
  resolves against the directory you start them from. Persisted state lives
  in the local store under the image's digest, never under a local agent's
  name, so such an image never sees a local agent's sign-in. A new version
  of the image starts with empty state. `ap reset` also takes a digest, so
  you can delete an old version's state.

## Build

`ap build [path]` reads the Agentfile in `path` (default: the current
directory) and writes an image into the local store.

- **Name.** The image is named after the Agentfile's directory.
  `ap build -t <name>` overrides it. A name is 1 to 63 lowercase letters,
  digits, and `-`, starting and ending with a letter or digit, so it is safe
  in git branch names (see [SHARE](#share)). A directory name that doesn't
  fit needs `-t`. The store remembers which directory each name was built
  from, and building a different directory under an existing name is an
  error. If you moved the directory, `ap build --replace` takes over the
  name and keeps the agent's persisted state, and history and pending
  changes for paths under the old directory move to the new one.
- **Layers.** Each build instruction produces one layer. A layer is keyed by
  the instruction, its arguments, and its resolved inputs: commit SHAs, URL
  digests, and file hashes. Layers are independent, so changing one
  instruction rebuilds only that layer. A final, small layer holds the
  harness configuration rendered from `SYSTEM`, `PROMPT`, `MODEL`, `EFFORT`,
  `MODE`, `PLUGIN`, `SKILL`, and `MCP`.
- **Store.** Layers are stored by the digest of their uncompressed content
  and shared across every agent in the store. A kernel, OS, or harness layer
  is downloaded once, however many agents use it. `ap rm` and rebuilds free
  a layer only when no agent or active run still needs it, and no repository
  an agent kept still depends on it (see [REPO](#repo)).
- **Lockfile.** `ap build` writes `Agentfile.lock` next to the Agentfile. It
  records the resolved harness version and digest, the OS image digest, the
  commit for every source, and the digest of every file `ap` downloads.
  Builds reuse locked resolutions; `ap update` resolves them again and
  rebuilds the agent. Commit the lockfile for reproducible builds. The
  lockfile covers what `ap` installs at build time, not what the agent
  downloads while it runs.
- **No compiler.** `ap build` assembles layers. It never invokes a compiler,
  and it needs nothing beyond the `ap` binary.
- **Translation.** The harness adapter renders `SYSTEM`, `PLUGIN`, `SKILL`,
  and `MCP` into the harness's own files. Anything it cannot translate is a
  build warning. `ap build --strict` turns warnings into errors.
- **Network hints.** `ap build` lists the hosts that plugins and MCP servers
  declare, such as a remote MCP server's URL. For each one the network
  policy would deny, it suggests a host rule for that exact host, such as
  `NETWORK +mcp.context7.com`. It never adds rules itself.
- **Working directory.** The working directory can depend on how many
  directories an Agentfile has (see [Run](#run)). To tell directories from
  files, `ap build` checks every share source, so a missing source is a
  build error (see [SHARE](#share)).

## Run

`ap run <agent> [-p <prompt>]` starts an agent in a new Linux microVM.

- **Name or path.** An argument that starts with `./`, `../`, `/`, or `~/`
  is a path to a directory with an Agentfile; `ap` rebuilds it if anything
  changed, then runs it. With a path, `ap run` takes `-t <name>`, like
  `ap build`. Anything else is an agent name.
- **Flags.** `ap run` takes `-p`/`--prompt`, `-i`, `-t`, `--share`,
  `--network`, `--resume`, `--gui`, and `--approve=<file>`. Of these, only
  `-p`, `--share`, and `--network` change what the Agentfile says, and only
  for that run. Nothing else in an Agentfile can be overridden at run time.
- **Headless or interactive.** A run is headless when it has a non-empty
  prompt, either `PROMPT` or `-p`/`--prompt` on the command line. A headless
  run streams the harness's output and exits with the harness's status. A
  run that another agent started stays open instead (see
  [Agents calling agents](#agents-calling-agents)). A run without a prompt
  opens the harness's interactive interface in your terminal. `ap run -i`
  forces interactive and submits the prompt as the first message.
- **Exit status.** If `ap` fails before the harness starts, for example on
  a missing secret or a refused approval, it exits with status 125.
- **Working directory.** The harness starts in the first of these that
  applies.
  1. the share from `ap run --share`
  2. the [`WORKDIR`](#workdir)
  3. the Agentfile's only directory `SHARE` or `REPO`
  4. `/home/agent`

  Harnesses read project instructions and settings from their working
  directory, and Claude Code keys session history by its path. The Codex
  adapter marks every share as trusted (`trust_level = "trusted"`), so
  Codex loads a repository's own configuration, hooks, and rules.
- **Ephemeral.** Every run starts from the image with an empty writable
  overlay, and the overlay is discarded when the run ends. Two things
  outlast it: the state that `AUTH login` and `PERSIST` keep (see
  [PERSIST](#persist)), and unapplied copy-on-write changes in shares (see
  [SHARE](#share)).
- **User.** The agent runs as the non-root user `agent`, with
  `HOME=/home/agent`.
- **Updates.** Harness self-updates are disabled. The harness version
  changes only through the Agentfile and the lockfile.
- **Sign-in.** When the guest asks to open a URL, `ap` opens only `https`
  URLs, and only after you confirm. Sign-in pages often redirect to a local
  callback (`http://localhost:<port>` or `http://127.0.0.1:<port>`). When
  the URL names such a callback as its `redirect_uri`, as the harness's own
  sign-in page and an MCP server's OAuth page do, `ap` binds that callback
  port on the host's `127.0.0.1` and forwards one connection into the
  guest, or gives up after five minutes. It relays that connection without
  parsing it, so the guest sees whatever the browser sends, which can
  include cookies your browser holds for other apps on `localhost`. The
  guest chooses the port, through the URL. This needs no grant, because it
  happens only for a page you confirmed, and only once.
- **Terminal output.** Bytes from the guest are filtered before they reach
  your terminal. Escape sequences that write or read your clipboard (OSC
  52), or that make your terminal reply with text the guest chose, such as
  a window title report, are removed. Queries that only report your
  terminal's own state, such as cursor position and colors, pass through,
  because full-screen interfaces wait for the answers.
- **Blocked hosts.** When a run ends, `ap` prints the hosts that `NETWORK`
  blocked and the rule that would allow each one, on standard error. It
  never asks during the run, and it never adds a rule itself.
- **Desktop.** With `OS desktop`, `ap run --gui <agent>` opens the agent's
  desktop in a native window.

## Instructions

### AGENT

```
AGENT <harness>[:<version>]
```

`AGENT` sets the harness, the coding agent that runs inside the VM, and its
version. It must be the first instruction, and it appears exactly once.

```
AGENT claude:2.1.319
AGENT claude:stable
AGENT codex:latest
```

| Harness | Runs |
|---|---|
| `claude` | Claude Code |
| `codex` | OpenAI Codex CLI |

`<version>` is an exact version or a channel the harness publishes, such as
`latest` or `stable`. Without a version, `ap` uses `latest`. Channels
resolve to an exact version at build time and are recorded in the lockfile.

**Planned.** After v0.1.0, `claude-desktop` will run Claude Desktop's Linux
build inside the agent's VM. It will need `OS desktop`, and since it has no
headless mode, it won't take `PROMPT`. ChatGPT Desktop's Linux app is in
preview, without computer use. `ap` doesn't support it yet, and it comes
after Claude Desktop.

Each harness has an adapter. The adapter defines the following.

- where configuration goes
- which files `AUTH login` and each `PERSIST` name map to
- how each `AUTH` method signs in
- which permission settings `MODE` sets
- how headless runs start
- which first-run state it prepares, so a new VM doesn't repeat the
  harness's setup
- which headers secrets are injected into
- which network hosts the harness needs (see [NETWORK](#network))

**Redistribution.** Claude Code is proprietary. `ap` downloads it from
Anthropic on your machine during the build, verifies it against the
lockfile, and never redistributes it.

- **Planned.** `ap push` replaces proprietary harness layers with references.
  The receiving machine downloads them from the vendor, pinned by digest.
- `ap build --exe` leaves proprietary layers out, and the executable
  downloads them on first run. `--include-proprietary` embeds them in an
  executable for your own offline use, with a notice that the file must not
  be shared.

Open-source harnesses, such as Codex, ship inside images as-is.

### OS

```
OS minimal | standard | desktop
```

`OS` chooses one of three curated operating system images. The default is
`desktop`.

| Tier | Contents |
|---|---|
| `minimal` | The harness, a shell, git, and coreutils |
| `standard` | `minimal`, plus curated toolchains |
| `desktop` | `standard`, plus our Wayland compositor, Chromium, and computer-use tools |

The OS images are not customizable. Every harness gets the same filesystem,
toolchains, and browser.

How the images are made:

- The userland comes from Debian 13 ("trixie"), at one snapshot.debian.org
  timestamp per `ap` release, so arm64 and x86_64 get the same package
  versions.
- The kernel (built with our own configuration) and the init (part of `ap`,
  written in Rust; there is no systemd) are ours. The VMM boots them
  directly; there is no installer or bootloader.
- Images are read-only, versioned with `ap` releases, pinned by the
  lockfile, and updated with `ap update`. Security fixes, including
  Chromium's, reach an agent when you update it to an image that has them.

The `standard` toolchains are git, bash, coreutils, curl, ripgrep, a C build
toolchain (gcc, make, pkg-config), Rust (through rustup), Go, Python
(through uv), and Node.js (through Vite+, `vp`). The version managers come
with a default version installed. A project that asks for another version
(for example, through `rust-toolchain.toml` or `.python-version`) gets it
downloaded at run time, where `NETWORK` allows. Those downloads are not in
the lockfile. Demand decides which toolchains ship.

The `desktop` tier starts lazily: when a program in the guest first opens
the display, on the first computer-use call, or when you attach with
`--gui`. Until then it costs disk space, not memory.

**Compositor.** The desktop runs our own Wayland compositor, built on
Smithay, with XWayland for X11 apps. X11-only tools, such as `xdotool`,
can't see native Wayland windows, so agents use the tools below.

**Computer use.** In the `desktop` tier, the guest's built-in MCP server
(see [Agents calling agents](#agents-calling-agents)) gives every harness
computer-use tools for the agent's own desktop.

The VMM implements the pixel-level tools: screenshots, pointer, keyboard,
and scrolling. Screenshots come from the guest's framebuffer, and input
goes through virtual keyboard and pointer devices, so capture and input
happen outside the guest.

The compositor knows more than pixels, so it adds tools that replace the
usual loop of screenshot, guess, click, and sleep.

- **Settled results.** An action returns once the screen has stopped
  changing, which the compositor knows from its damage tracking. If it
  doesn't settle within a time limit, as with a video or a spinner, the
  action returns anyway and says so.
- **Windows as objects.** List, launch, focus, move, and close windows by
  ID, and capture one window instead of the whole screen.
- **A semantic UI tree.** The agent reads apps' accessibility data and can
  act on a named button instead of coordinates. Apps have to expose that
  data, and support varies.
- **Batched actions.** A sequence of clicks and keys runs in one call with
  one result, instead of a model round trip for each step.

MCP browser tools in the guest can use Chromium's DevTools endpoint.

*Proposed:* the Claude Code adapter can connect Claude in Chrome to the
guest's Chromium. The extension is Anthropic's, so it is downloaded on your
machine like the harness. It works only with a subscription sign-in, and it
needs the host `bridge.claudeusercontent.com`.

### SYSTEM

```
SYSTEM <text>
SYSTEM <path>
SYSTEM
```
followed by a fenced block.

`SYSTEM` sets the harness's global instructions, the equivalent of a global
`AGENTS.md`. The adapter writes it to the harness's own location.

| Harness | File |
|---|---|
| `claude` | `~/.claude/CLAUDE.md` |
| `codex` | `~/.codex/AGENTS.md` |

`SYSTEM` can repeat. The values concatenate in file order, separated by a
blank line. Instructions that plugins contribute follow, in `PLUGIN` order.

```
SYSTEM ~/.claude/CLAUDE.md
SYSTEM Never push to main.
```

See [Text values](#text-values) for the forms.

### PROMPT

```
PROMPT <text>
PROMPT <path>
PROMPT
```
followed by a fenced block.

`PROMPT` embeds your instructions in the agent, so the same task can run on
a schedule or be repeated without retyping it. You can leave it out and give
the prompt at run time with `-p` or `--prompt`.

An agent with a `PROMPT` runs headless: `ap run <agent>` gives the prompt to
the harness, streams the output, and exits with the harness's status.

```
PROMPT Review the diff on the current branch and list risks.
```

`-p` replaces `PROMPT` for that run.

```
ap run reviewer -p "Review only the tests."
```

An empty prompt clears `PROMPT` for that run. `ap run reviewer -p ""` opens
the interactive interface with nothing submitted.

A prompt that starts with `/` must be quoted, or it is read as a path.

```
PROMPT "/review"
```

### MODEL

```
MODEL <model>
```

`MODEL` sets the model the harness uses. `<model>` is passed to the harness
unchanged, so it accepts whatever names or aliases the harness accepts.

```
MODEL opus
```

Without `MODEL`, the harness uses its default model. `MODEL` overrides any
model a plugin sets.

### EFFORT

```
EFFORT low | medium | high | xhigh | max
```

`EFFORT` sets how much the model reasons before it answers. The adapter
maps the level to the harness's own setting.

```
EFFORT high
```

- Without `EFFORT`, the harness uses its default effort.
- `EFFORT` works with or without `MODEL`.
- The adapter knows which levels each harness version accepts. A level the
  harness doesn't accept is a build warning, and the adapter uses the
  nearest lower level, or the harness's lowest level if none is lower.
  `ap build --strict` makes that an error.
- `ap` doesn't know which levels each model supports, because `MODEL` is
  passed through unchecked. A level the model doesn't support is left to
  the harness, at run time.
- `EFFORT` overrides any effort a plugin sets.

### MODE

```
MODE read | ask | edit | auto | full
```

`MODE` sets how much the harness does inside the VM without asking. The
default is `auto`.

| Mode | The harness | `claude` | `codex` |
|---|---|---|---|
| `read` | reads and explores, and doesn't edit. In an interactive Claude Code run, approving its plan lets it start editing | `plan` | `read-only` sandbox, approval `never` |
| `ask` | reads, and asks before anything else | `default` | `read-only` sandbox, approval `on-request` |
| `edit` | also edits files in its workspace. Claude Code asks before commands other than common file operations. Codex runs commands inside its sandbox and asks before leaving it | `acceptEdits` | `workspace-write` sandbox, approval `on-request` |
| `auto` | runs without routine prompts. The harness's own reviewer checks actions beyond reads and workspace edits (Claude Code) or beyond the sandbox (Codex) | `auto` | `workspace-write` sandbox, approval `on-request`, reviewer `auto_review` |
| `full` | skips permission prompts and the harness's checks, except the few actions the harness never auto-approves. The VM is the boundary | `bypassPermissions` | `--dangerously-bypass-approvals-and-sandbox` |

- The adapter sets the mode on the harness's command line, with flags or
  Codex's `-c` overrides, every time it starts the harness, including on
  `--resume`, so a project's settings or an earlier session can't pick a
  different one.
  A project's Claude Code settings can still turn a mode off:
  `disableAutoMode` makes `auto` start in manual mode, and
  `permissions.disableBypassPermissionsMode` makes Claude Code reject
  `full`. With `full` on Claude Code, the adapter also sets the user-level
  key that skips the first-run warning
  (`skipDangerousModePermissionPrompt`).
- The harness's workspace is the working directory plus every `SHARE` and
  `REPO`. The adapter adds those on the harness's command line, with
  `--add-dir` or a configuration override. For a single file, it adds the
  parent directory, unless that is the home directory.
- Codex's `workspace-write` sandbox keeps network access off by default.
  With `auto`, the adapter turns it on
  (`sandbox_workspace_write.network_access`), because `NETWORK` already
  decides what the agent can reach. With `edit`, it stays off, so Codex asks
  before commands that need the network.
- `MODE` overrides any permission settings a plugin sets.
- `auto` depends on the harness. With Claude Code, it needs a recent model,
  an administrator can turn it off, and its classifier calls count toward
  token use on the API and Enterprise plans. When `auto` isn't available,
  Claude Code starts in its manual mode instead, so the run behaves like
  `ask`.
- Every Codex mode except `full` relies on Codex's own sandbox, so the
  guest kernel has to support that sandbox.
- In a headless Codex run, `MODE auto` uses `codex exec --approve-for-me`,
  which selects the same sandbox, approval policy, and reviewer.

When the harness needs an approval, the request goes to whoever started the
run: your terminal for `ap run`, or the control API client. A run that
another agent started sends its requests to whoever started the first run
in the chain, never to the calling agent. When there is no one to ask,
requests are denied.

`MODE` governs the harness's own tools. Hooks, MCP servers, and other code
that runs outside those tools aren't bound by it. So whatever the mode, the
agent can still change what its `rw` shares and persisted state hold, use
a secret's bound hosts, and reach what `NETWORK` allows. Narrow those with
`SHARE` modes, `PERMISSIONS`, and `NETWORK` rules.

### ADD

```
ADD <src> [<dest>]
```

`ADD` copies a host file or directory into the image at build time.

```
ADD ./fixtures/database.sqlite
ADD ./prompts /opt/prompts
```

- Without `<dest>`, the [mirror rule](#paths) decides where the content
  lands.
- The content is part of the image. Writes by the agent go to the run's
  overlay and disappear when the run ends, unless `PERSIST home` keeps them.
- `<src>` itself is canonicalized like any host path, so a symlink you name
  is followed. Inside a directory, symlinks are copied as symlinks and
  never followed. Special files (devices, sockets, FIFOs) are skipped with
  a warning.
- When `<src>` is a directory, files ignored by git (according to the
  `.gitignore` files inside it) are skipped. A file named explicitly is
  always copied.
- A `<src>` outside the build context is a [grant](#grants).

`ADD` is the instruction for putting arbitrary files in the image. Never
use it for credentials; use [`SECRET`](#secret).

### REPO

```
REPO <source> [<dest>] [history]
```

`REPO` clones a git repository into the image at a pinned commit.

```
REPO torvalds/linux
REPO jgabor/agent-plus history
REPO https://codeberg.org/forgejo/forgejo ~/src/forgejo
```

- `<source>` is a [source](#sources).
- Without `<dest>`, the repository lands in `~/git/<repository name>`. A
  trailing `.git` is not part of the name. Two repositories with the same
  name are an error; give one a destination.
- By default, `REPO` stores only the pinned commit (depth 1), leaves Git LFS
  pointers unresolved, and does not fetch submodules. `history` stores
  the full history.
- The repository is image content. Changes the agent makes disappear when
  the run ends, unless `PERSIST home` keeps them. To keep work, have the
  agent push it (where `NETWORK` and a `SECRET` allow), or work in a `SHARE`
  instead.
- With `PERSIST home`, a persisted repository is replaced or kept whole,
  never file by file. When a rebuild moves a `REPO` to a new commit, the
  next run starts from the new commit, unless the agent left work in its
  copy. Work means a moved `HEAD`, new commits, branches, tags, or stashes,
  or files that differ from `HEAD` and that git doesn't ignore. With work,
  the agent keeps its copy at the old commit: `ap` moves the old repository
  into the persisted home, so it no longer depends on the old layer. To
  move to the new commit, have the agent pull it, or delete all persisted
  state with `ap reset <agent>`. `ap` checks for work the way it reads any
  repository an agent wrote (see [SHARE](#share)).

### PLUGIN

```
PLUGIN <source> [<name>...]
```

`PLUGIN` installs a harness plugin. `ap` translates it for the harness set
by `AGENT`, whatever format the plugin was written in.

```
PLUGIN https://github.com/anthropics/claude-plugins-official/tree/main/external_plugins/context7
PLUGIN anthropics/claude-plugins-official context7 github
```

- With `<name>` arguments, `<source>` must be a marketplace, and the names
  select the plugins to install.
- Without names, `<source>` must be a plugin. A source that is only a
  marketplace is an error that lists its plugins.

Recognized formats:

| Format | Manifest |
|---|---|
| Claude Code | `.claude-plugin/plugin.json` and `.claude-plugin/marketplace.json` |
| Agent Plugins 1.0 | `plugin.json` at the plugin root |
| Codex | `.codex-plugin/plugin.json` |

A plugin that ships several manifests is read from the one native to the
target harness, then from Agent Plugins 1.0, then from the others in table
order.

The plugin is parsed into a neutral model with these components:
instructions, skills, commands, subagents, hooks, MCP servers, and
settings. That model is then rendered for the harness version in the
lockfile.

- A component with no equivalent in that harness (for example, a hook on an
  event the harness never fires) produces a build warning that names it.
  `ap build --strict` makes that an error.
- Two components with the same name, from any combination of `PLUGIN`,
  `SKILL`, and `MCP` instructions, are an error.
- Agentfile instructions win over plugin settings: `MODEL`, `EFFORT`,
  `MODE`, `ENV`, and `SECRET` override what a plugin sets.

### SKILL

```
SKILL <source> [<name>...]
```

`SKILL` installs skills in the [Agent Skills](https://agentskills.io)
format.

```
SKILL https://github.com/anthropics/skills/tree/main/skills/algorithmic-art
SKILL anthropics/skills pdf docx
SKILL ./skills/my-skill
```

- When `<source>` is a directory with a `SKILL.md`, `ap` installs that
  skill.
- Otherwise, `ap` installs every skill found under `<source>`, and `<name>`
  limits the set.
- The adapter writes skills to the harness's skills directory.

### MCP

```
MCP <source>
MCP <name> <url>
```

`MCP` registers Model Context Protocol servers with the harness.

```
MCP https://github.com/anthropics/claude-plugins-official/tree/main/external_plugins/github
MCP example https://mcp.example.com/mcp
```

- The first form registers the servers declared in the source's `.mcp.json`
  or `mcp.json`.
- The second form registers a remote server that speaks MCP over HTTP. A
  server that needs a header is declared in a `.mcp.json` instead (the first
  form), where `${VAR}` can name a `SECRET`.

Rules:

- MCP servers run inside the guest, never on your host. A server that has
  to live on your host is reached over the network, as `host.ap.internal`,
  with a `+` rule (see [NETWORK](#network)).
- Commands like `npx` and `uvx` use the OS toolchains. Packages they
  download at run time need `NETWORK` to allow the registry, and they are
  not in the lockfile. *Proposed:* prefetching and locking them at build
  time comes later.
- `${VAR}` references in server configuration resolve against the guest
  environment, which includes `ENV` values and `SECRET` placeholders.
- Remote servers are reached through the egress proxy, under `NETWORK`.
  `ap build` suggests a rule for each remote server whose host the policy
  denies.
- The adapter writes the servers from `MCP`, and `ap`'s own (see
  [Agents calling agents](#agents-calling-agents)), into a file it manages
  and points the harness at it (`--mcp-config` for Claude Code), so a
  rebuild's changes reach a persisted home.
- The harness also loads the servers it finds on its own, as it does on
  your host: a plugin's servers, a project's `.mcp.json` or Codex project
  configuration, servers kept in Claude Code's `.claude.json`, and claude.ai
  connectors under a claude.ai sign-in. `NETWORK` applies to all of them.

### COMPRESSION

```
COMPRESSION <level>
```

`COMPRESSION` sets the zstd level for the layers this Agentfile builds. The
default is `3`.

```
COMPRESSION 19
COMPRESSION -3
```

- Levels `1` to `22` follow zstd. Negative levels `-1` to `-7` select the
  fast modes, like `zstd --fast=N`.
- Base layers (the OS and the harness) keep the level their publisher
  chose.
- Layers are addressed by their uncompressed content, so changing the level
  recompresses layers without changing what is inside them.

### ENV

```
ENV <key> <value>
```

`ENV` sets an environment variable inside the guest. The value follows the
[single-line text](#text-values) rule: one quoted string is unquoted, and
anything else is taken verbatim.

```
ENV LOG_LEVEL debug
ENV AWS_REGION $AWS_REGION
ENV EDITOR ${EDITOR:-vi}
```

- The value is either literal or uses [variables](#variables) read from
  the host at run time. A host value is never baked into the image. Reading
  a host variable is a [grant](#grants).
- Every `ENV` value is visible to the agent and to every process in the VM.
  Never put credentials in `ENV`; use `SECRET`.
- Setting the same key twice, or a key that a `SECRET` or `SEAL` also names,
  is an error. So is a key the adapter sets, such as `CLAUDE_CONFIG_DIR` or the
  certificate variables under [SECRET](#secret), and a name the harness
  reads for sign-in, because only [`AUTH`](#auth) signs the harness in.

### SECRET

```
SECRET <name> [@<host>...] [visible]
```

`SECRET` gives the agent a credential that, by default, it can use but
never read.

```
SECRET GITHUB_TOKEN
SECRET STRIPE_API_KEY @api.stripe.com
SECRET AWS_SECRET_ACCESS_KEY visible
```

Every `SECRET` is a [grant](#grants).

**Where the value comes from.** At run time, `ap` reads the value from the
host environment variable `<name>`. If that is unset, it reads `ap`'s secret
store (`ap secret set <name>`, backed by the OS keychain). A missing value
makes the run fail, and the error names the secret.

**Hidden (the default).** Inside the guest, the environment variable
`<name>` holds a placeholder. The egress proxy replaces the placeholder with
the real value only under both of these conditions.

- on TLS connections to the secret's bound hosts
- in request headers on the adapter's allowlist (for example,
  `Authorization` and `x-api-key`), never in query strings or bodies

A connection counts as one to a bound host only when the name behind the
address it connects to (see [NETWORK](#network)), its TLS server name, and
each request's `Host` or `:authority` all name that host, under either
network default. On any other connection, the placeholder passes through
unchanged.

In an `Authorization: Basic` header, which git sends over HTTPS, the proxy
decodes the credentials, replaces the placeholder in them, and encodes them
again. When `GITHUB_TOKEN` or `GH_TOKEN` is bound to `github.com`, `ap` also
configures git in the guest to use it, so `git push` works over HTTPS.

The real value is never sent to any other host. *Proposed:* placeholders
keep the real value's known prefix and length, so clients that check formats
(`sk-ant-`, `ghp_`) accept them.

Bound hosts:

- Bound hosts come from the `@<host>` list. A bound host matches that exact
  host only, not its subdomains. An `@<host>` list replaces the adapter's
  defaults.
- Well-known names have adapter defaults, so `@<host>` is optional for them.

  | Name | Default bound hosts |
  |---|---|
  | `ANTHROPIC_API_KEY` | `api.anthropic.com` |
  | `OPENAI_API_KEY`, `CODEX_API_KEY` | `api.openai.com` |
  | `GITHUB_TOKEN`, `GH_TOKEN` | `github.com`, `api.github.com` |

- A hidden secret with neither `@<host>` nor an adapter default is an error.
- A bound host that `NETWORK` denies is an error.
- A bound host that is one of the harness's own hosts is an error, and so is
  a `SECRET` that uses a name the harness reads for sign-in. Use
  [`AUTH`](#auth) for those.

Injection requires decrypting TLS to the bound hosts (see
[Enforcement](#network)). The guest trusts a certificate authority
generated for each run. `ap` installs it in the guest's system store and
Chromium's certificate database, and sets `NODE_EXTRA_CA_CERTS`,
`SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`, `CURL_CA_BUNDLE`, and
`GIT_SSL_CAINFO`, because many tools ignore the system store.

**`visible`** hands the real value to the agent. Use it for credentials a
proxy cannot inject, such as AWS request-signing keys and SSH keys.

A hidden secret does not stop misuse during a run: the agent can still call
its bound hosts with it. Narrow what it can reach with `NETWORK` URL rules.
*Proposed:* method restrictions per secret, such as a read-only GitHub
token, come later.

### SEAL

```
SEAL <name> [@<host>...] [visible]
```

`SEAL` encrypts a credential into the image, for handing a working agent to
someone. At run time it works like a [`SECRET`](#secret) with the same
arguments, and the rules for bound hosts and `visible` are the same.

```
SEAL DEPLOY_TOKEN @deploy.example.com
SEAL AWS_SECRET_ACCESS_KEY visible
```

Every `SEAL` is a [grant](#grants).

- `ap build` reads the value from the same places a run reads a `SECRET`'s,
  encrypts it with [age](https://age-encryption.org), and stores the
  ciphertext in its own layer. The recipients are the SSH public keys in
  `~/.config/ap/recipients`, one per line. Without that file, `ap build`
  asks for a passphrase, and fails if there is no terminal to ask on.
- The layer is keyed by the ciphertext, never by the plaintext.
- A rebuild reuses the stored ciphertext while the value and the recipients
  are unchanged, checked against a keyed hash that `ap` keeps on the host,
  so the layer stays cached.
- The host decrypts it at run time, with the matching private key from
  `~/.ssh` or by asking for the passphrase. A run that can't decrypt it
  fails, and the error names the secret.
- `ap build --exe` includes sealed layers, so a handed-over executable keeps
  the secret.
- **Planned.** `ap push` never includes sealed layers, so a sealed secret can't
  reach a registry. A pulled agent has no sealed layer, so it reads the secret
  from your environment or secret store, like any other secret.
- A name can't appear in both `SECRET` and `SEAL`.

### AUTH

```
AUTH login | key | token
```

`AUTH` chooses how the harness signs in. It can appear once, and the adapter
maps it to that harness's own mechanism, so the line still works after you
change `AGENT`.

```
AUTH login
```

| Method | How the harness signs in | What the agent can read |
|---|---|---|
| `login` | Through its own login flow, inside the agent. The sign-in is kept between runs | The harness's credential file |
| `key` | With the harness's API key from your host | A placeholder only |
| `token` | With a subscription token from your host. Claude Code only | The token |
| no `AUTH` line | Through its own login flow, on each run. Nothing is kept | The harness's credential file, during the run |

**`AUTH login`** signs in to your subscription (for example, Claude Max or
ChatGPT Pro) or account through the harness's own flow. `ap` never reads,
parses, or sends the credentials that flow creates. The one exception is
the sign-in callback, which it relays without parsing (see [Run](#run)). The
harness's own credential file stays in the agent's persisted state (see
[PERSIST](#persist)), much as a dev container keeps credentials in a volume.
With Claude Code, the adapter sets `CLAUDE_CONFIG_DIR`, so `.claude.json`,
the record that you finished first-launch setup, is kept too. It can list
MCP servers, so code that ran once can add one that starts on later runs
(see [MCP](#mcp)). `ap init` writes `AUTH login` into new Agentfiles.

**`AUTH key`** uses the harness's API key. On your host, `ap` reads
`ANTHROPIC_API_KEY` for Claude Code and `CODEX_API_KEY` for Codex, the way
it reads any [secret](#secret). The agent sees only a placeholder, and the
adapter signs the harness in with it. The proxy swaps in the real value on
the hosts the adapter lists for that key, with the same checks as for a
hidden secret.

**`AUTH token`** uses a subscription token you created, such as one from
`claude setup-token`. On your host, `ap` reads `CLAUDE_CODE_OAUTH_TOKEN`
like any secret and passes it through untouched, so the agent can read it.
With any harness but Claude Code, it is a build error.

- `AUTH key` and `AUTH token` are [grants](#grants).
- A headless run can't complete a login flow, so one that isn't signed in,
  including a called run, ends with the harness's error and status. Sign an
  agent with a `PROMPT` in with `ap run <agent> -p ""`. In CI, use
  `AUTH key`. With Claude Code, `AUTH token` works too.
- The adapter never turns off a sign-in method the harness offers.
- With `AUTH login` or `AUTH token`, any code in the agent, including plugin
  code, can read the credential and send it anywhere the agent can reach.
  Under `NETWORK -`, that is the hosts you allowed and the harness's own,
  other sites those servers also serve (see the limits under
  [NETWORK](#network)), and anywhere an agent it may call can reach (see
  [POLICY](#policy)). Under `NETWORK +`, it is anywhere.
- Whether `AUTH login` and `AUTH token` fit Anthropic's terms is an open
  question.

### SHARE

```
SHARE <src> [<dest>] [rw | ro]
```

`SHARE` gives the agent a live view of a host file or directory while it
runs.

```
SHARE ./references
SHARE ./data /data
SHARE /var/log/output.log rw
SHARE ~/notes ro
```

| Mode | Agent writes | Your files |
|---|---|---|
| copy-on-write (default) | Allowed; kept in the agent's overlay | Unchanged until `ap apply` |
| `rw` | Allowed | Changed immediately |
| `ro` | Fail with a read-only error | Unchanged |

How it works:

- `SHARE` is a run instruction. The image stores the share as a request,
  never your files.
- `<src>` is stored relative to the build context and resolved at run time,
  against the directory the agent was built from, which is pinned (see
  [Grants](#grants)).
- A missing `<src>` is an error, at build time and at run time. `ap build`
  records whether `<src>` is a file or a directory, and a run fails if that
  changed, because it can change the working directory (see [Run](#run)).
- At every run, `ap` canonicalizes `<src>` again. If it now resolves
  somewhere you haven't approved, the run asks for approval again (see
  [Grants](#grants)).
- Without `<dest>`, the [mirror rule](#paths) decides the guest path.
- A `<src>` outside the build context, and any `rw` share, is a
  [grant](#grants).
- The agent sees only what is under the share's root. A symlink inside the
  share appears to the guest as a symlink and is resolved inside the guest,
  never on your host, so it can't reach host files outside the share.
- Files the agent has not written show your changes live. The exception is
  the `.git` directory at the root of a copy-on-write share: everything in
  it except objects is captured when the run starts, so commits you make
  during the run don't move `HEAD` under the agent. Objects are read live,
  which keeps the capture small in a large repository, so pruning objects
  on your side during a run can remove ones the agent needs. A nested
  repository's `.git` stays live.
- Tracked files stay live, so a commit, pull, or branch switch on your side
  during a run looks like uncommitted changes to the agent. If the agent
  commits them, they land on its branch.
- In a linked worktree, `.git` is a file that points outside the share, so
  git doesn't work inside the guest, and `ap apply` applies files rather
  than a branch. Share the main working tree if the agent needs git.
- File locks pass through to the host.

In `rw` shares, the files your host acts on are read-only to the agent.

- An entry named `Agentfile` or `Agentfile.lock` can't be created, changed,
  removed, or renamed, so an agent can't change what a later build does.
  Edits still work through copy-on-write and `ap apply`.
- An entry named `.git` can't be created, removed, or renamed, and when it
  is a file (as in worktrees), it can't be changed.
- In every git directory, including `.git/modules/*` and
  `.git/worktrees/*`, these are read-only, because git runs hooks and
  configured commands on your host: `config`, `config.worktree`,
  `commondir`, `gitdir`, and `hooks/`.
- Names match case-insensitively, as they do on macOS filesystems.

Anything else the agent changes in an `rw` share, it changes for real,
including files your tools run later, such as build scripts and hooks kept
in the working tree. [`PERMISSIONS`](#permissions) can protect specific
files.

**Bringing changes back.** Unapplied copy-on-write changes are kept per run,
and they survive the end of the run until you apply or discard them. The
next run starts from your current files, not from earlier unapplied
changes.

When `ap` reads git state that an agent wrote, here or in a persisted
repository, it reads refs, objects, the index, and ignore files with its
own code. It never runs that repository's hooks or uses its config or
attributes, because those can run commands.

- `ap diff <agent | run>` shows what changed, during a run or after it.
- `ap discard <agent | run>` throws the changes away.
- `ap apply <agent | run>` brings them back.
  - When the share's root on your host holds a `.git` directory, the agent's
    commits, and the files it wrote but didn't commit, land as a new branch,
    `ap/<agent>/<run>`, imported as git objects and checked with `git fsck`.
    The branch starts from the commit your `HEAD` was at when the run
    started. If the agent's history no longer descends from that commit, or
    the repository had no commits, the branch is the agent's history as it
    stands. Your working tree, hooks, and config are not touched.
  - Otherwise, changed, added, and deleted files are applied to your files,
    even if a repository lies above or below the share. A file that changed
    on your side after the agent first wrote it is not applied, and
    `ap apply` reports it.
  - Nothing is ever applied under a path component that matches `.git`
    case-insensitively, in any repository, including nested ones.
  - Symlinks are never followed. A symlink is applied only if its target
    stays inside the share. Special files are never applied.
  - Git-ignored files are neither shown nor applied. `ap diff` and
    `ap apply` report how many files they skipped, and why.
- `ap apply` and `ap discard` fail while the run is active. After `ap apply`,
  the only changes still pending are files it couldn't apply, because they
  changed on your side too or a write failed, and `ap discard` removes them.
  If the branch import fails, everything stays pending. Files it leaves out
  on purpose, such as git-ignored ones, don't stay pending.
- With an agent name, `ap diff`, `ap apply`, and `ap discard` act on its
  single run with pending changes. If several runs have pending changes,
  they list the runs and ask for one.
- `ap rm` and `ap reset` delete pending changes too, so they refuse while an
  agent has any, unless you pass `--force`.

`ap run --share <directory>` adds a copy-on-write share for a single run.
It can appear once and takes no destination: the path resolves against your
current directory, it maps to `~/<directory name>`, and it becomes the
working directory, ahead of any `WORKDIR` in the Agentfile. A destination
that overlaps another one fails the run.

### WORKDIR

```
WORKDIR <path>
```

`WORKDIR` sets where the harness starts.

```
SHARE ./app
REPO torvalds/linux
WORKDIR ~/app/packages/web
```

Harnesses read project instructions, settings, and MCP servers from their
working directory, so where they start decides which project they see. You
need `WORKDIR` only to start somewhere other than the default: an Agentfile
with one directory `SHARE` or `REPO` starts in it, and any other starts in
`/home/agent` (see [Run](#run)).

- `<path>` is a guest path (see [Paths](#paths)). It can be a subdirectory
  of a share or repository.
- A path that isn't a directory when the run starts fails the run, and the
  error names it. `ap` doesn't create it.
- `ap run --share` wins over `WORKDIR`.

### PERSIST

```
PERSIST sessions | home
```

Runs are ephemeral. `PERSIST` names the state that survives between runs.

```
PERSIST sessions
```

| Name | Keeps | Example (Claude Code) |
|---|---|---|
| `sessions` | Conversation history, so runs can resume | `~/.claude/projects` |
| `home` | The agent's whole home directory, including `sessions` but not shares | `/home/agent` |

- The adapter maps each name to that harness's own files, so a `PERSIST`
  line still works after you change `AGENT`.
- Persisted state lives in the store, under the agent's name, together with
  the sign-in from `AUTH login`. An image with no local Agentfile keeps it
  under its digest instead (see [Grants](#grants)). It survives rebuilds.
  `ap reset <agent>` deletes it.
- Only `AUTH login` persists a sign-in. Without it, `PERSIST home` leaves
  out the harness's login files. Claude Code's `.claude.json` isn't a
  credential file: `AUTH login` and `PERSIST home` both keep it, and the
  adapter prepares it only when it is absent.
- Both names keep one history per host directory. Two directories with the
  same name both appear as `~/<name>` in the guest, and `ap` puts the right
  history in place when the run starts. A run whose working directory isn't
  a share uses one history for the agent.
- Persisted state is readable by everything in the guest.
- Concurrent runs of the same agent share the sign-in and its sessions. File
  locks pass through to the host, so a harness's own locks work across runs.
  Those locks protect the file, not the token exchange, so two runs can
  still race a refresh. OpenAI says not to share one `auth.json` across
  concurrent jobs, and a lost race can sign Codex out until you sign in
  again. For parallel Codex runs, use `AUTH key`. With `PERSIST sessions`
  and no `PERSIST home`, the Codex adapter gives each run its own
  `CODEX_SQLITE_HOME`, because that database's write-ahead log can't be
  shared between VMs. That database is discarded when the run ends, so
  Codex's goals, memory state, and conversation index don't carry over to
  later runs.
- With `PERSIST home`, only one run of the agent can be active at a time. A
  second run fails, and the error names the active one. Files the adapter
  manages (the harness binary and its generated configuration) are
  regenerated from the image on every run. For other files that both the
  agent and a rebuild changed, the agent's version wins. A repository from
  `REPO` counts as one file here (see [REPO](#repo)). A share isn't part of
  a persisted home: its destination always shows the share, and its changes
  follow [SHARE](#share).
- A persisted home keeps whatever a run leaves in it, and later runs execute
  with the same grants. That includes shell startup files such as
  `.bashrc`, programs in `~/.local/bin`, harness settings the adapter
  doesn't manage (such as Claude Code's `.claude.json`), and transcripts.
  Code that ran once can leave something behind for every later run.
- `ap run --resume <agent>` resumes the harness's latest session for that
  directory, by session ID, so a headless session counts too. It needs
  `PERSIST sessions` or `PERSIST home`. It fails if that directory has no
  session, or if another run of the agent is active in it. `PROMPT` isn't
  submitted again. A non-empty `-p` is submitted, and the run is headless
  unless you add `-i`. Without one, the run is interactive. A session
  continues on the agent's current image, even after a rebuild.

### PERMISSIONS

```
PERMISSIONS +read:<path> | -read:<path>
PERMISSIONS +write:<path> | -write:<path>
```

`PERMISSIONS` decides which guest files the agent can read and write. `ap`'s
filesystem enforces the rules, so they hold for every process in the guest,
whichever harness runs: the harness, plugin code, and any script the agent
starts.

```
SHARE . rw
PERMISSIONS -write:.husky
PERMISSIONS -write:.github/workflows
PERMISSIONS -read:.env
```

- By default, everything inside the guest is allowed.
- A rule covers its path and everything under it. There are no wildcards.
- The most specific rule wins, as in [NETWORK](#network): a longer path
  beats a shorter one, and two rules for the same path and access with
  opposite signs resolve to deny.
- `<path>` is a guest path. A relative path resolves against the working
  directory (see [Run](#run)), so `.env` follows the project the agent is
  working in. When `ap run --share` moves the working directory, a relative
  rule applies in both places, so `--share` can't lift it. Use `~/` or `/`
  for anything else.
- `read` covers reading or running a file and listing a directory. `write`
  covers creating, changing, renaming, and removing. A denied access fails
  with a permission error.
- Rules apply to a file's real path, so a symlink can't get around them.
  Renaming or hard-linking a denied path fails too. So does renaming a
  directory above it, or creating a symlink at it or above it, because the
  file would otherwise leave its rule behind.
- Paths in a share match the way your host's filesystem does, so on macOS,
  `-read:.env` also covers `.ENV`.
- A `+` rule only allows again what a broader `-` rule denied. It can't make
  an `ro` share writable, or open the files that `SHARE` keeps read-only.

`PERMISSIONS` grants nothing on your host, so it needs no approval. It sees
files, not commands, so it can't express a rule like "no `git push`".

### PORT

```
PORT <guest port> [<host port>]
```

`PORT` forwards a TCP port from the guest to the host, so you can open a
server the agent starts in your own browser.

```
PORT 5173
PORT 8080 9000
```

- The host side binds to `127.0.0.1` only. The guest side connects to the
  guest's loopback interface, so a server that binds only `localhost`, as
  Vite does by default, works.
- Without `<host port>`, `ap` uses the same number, or the next free port
  if it is taken, and prints the URL.
- With `<host port>`, a taken port is an error.

### NETWORK

```
NETWORK + | -
NETWORK +<rule> | -<rule>
```

`NETWORK` decides which destinations the agent can reach. `+` allows and `-`
denies.

**Default.** A bare `NETWORK +` allows every destination that no rule
denies. A bare `NETWORK -` denies every destination that no rule allows.
Without a bare rule, the default is `+`. `true`, `yes`, and `1` mean `+`;
`false`, `no`, and `0` mean `-`. Two bare rules that disagree are an error.
`ap init` writes `NETWORK -` into new Agentfiles, so the agents you create
start closed.

The default `+` really does allow everything. That includes your local
network, services listening on your host's network addresses, and cloud
metadata endpoints when the host runs in a cloud. For untrusted code, start
from `NETWORK -` and allow what it needs.

**Rules.**

| Kind | Example | Matches |
|---|---|---|
| Host | `+kagi.com`, `+github.com:22` | `kagi.com` and its subdomains, on any port or the given port |
| IP or CIDR | `-1.1.1.1`, `+10.1.0.0/16`, `-[2606:4700::]/32:443` | Connections to those addresses, on any port or the given port |
| URL prefix | `+https://github.com/jgabor/agent-plus` | That scheme and exact host, and paths under that prefix |

Matching follows boundaries:

- `kagi.com` matches `api.kagi.com`, but never `kagi.com.evil.net` or
  `notkagi.com`.
- `https://github.com/jgabor/agent-plus` matches
  `https://github.com/jgabor/agent-plus/issues`, but never
  `https://github.com/jgabor/agent-plus-evil`. URL rules match the exact
  host, not subdomains.
- A URL rule covers only its own paths on its own host. The rule above
  doesn't allow `git clone https://github.com/jgabor/agent-plus.git`,
  because `agent-plus.git` is a different path segment. Downloads that
  redirect to another host, such as GitHub release assets, need a rule for
  that host too.
- Request paths are normalized (RFC 3986) before matching. Requests whose
  paths contain encoded slashes (`%2F`) or dot segments under a host with
  URL rules are denied.
- A port suffix limits a host or IP rule to that port. IPv6 addresses go in
  brackets.

**Evaluation.** The most specific matching rule wins, so line order never
matters. For each connection:

1. **Name decision.** Among URL and host rules that match the connection's
   name, take the most specific. A URL rule beats a host rule, a longer
   path beats a shorter one, and a host with more labels beats one with
   fewer. A rule with a port beats the same rule without one.
2. **Address decision.** Among IP rules that match the destination address,
   take the one with the longest prefix. A rule with a port beats the same
   rule without one.
3. If both decisions exist and disagree, deny. If only one exists, it
   decides. If neither exists, the default decides.
4. Two equally specific rules with opposite signs resolve to deny.

```
AGENT claude
NETWORK -
NETWORK +github.com
NETWORK -gist.github.com
NETWORK +https://codeberg.org/forgejo/forgejo
```

| Request | Result | Deciding rule |
|---|---|---|
| `https://github.com/torvalds/linux` | allow | `+github.com` |
| `https://gist.github.com/x` | deny | `-gist.github.com` |
| `https://codeberg.org/forgejo/forgejo/issues` | allow | URL rule |
| `https://codeberg.org/other/repo` | deny | default `-` |
| `https://api.anthropic.com/v1/messages` | allow | harness host |
| `https://api.openai.com/v1/responses` | deny | default `-` (a harness host only for `codex`) |

**Command line.** `ap run --network=<rule>` adds rules for a single run. A
bare rule on the command line replaces the Agentfile's default, and
command-line rules win ties against Agentfile rules. The checks that
`ap build` runs on `NETWORK` and `SECRET` rules run again on the merged
rules, and a failure fails the run.

**Harness hosts.** The hosts the harness needs for its model API and
sign-in stay allowed under `NETWORK -`, so an agent never loses its own
model. An explicit `-` rule for such a host still wins. `ap build` lists
these hosts, and `ap inspect` shows them. They are an exfiltration path:
code in the agent can send data to them with a key it brought itself.
With `AUTH key`, the proxy allows only requests that carry the key's
placeholder on the key's hosts. The other harness hosts stay an exfiltration
path.

**Your host.** Services on your host's loopback interface are reachable
only as `host.ap.internal`, and only when a `+` rule names it. The proxy
refuses any other connection whose real destination is a loopback or
unspecified address, including IPv4-mapped forms, and an IP rule can't
allow one.

**Enforcement.**

- **DNS.** The guest's only resolver is inside `ap`. It answers a name only
  if some rule, or the default, could allow a connection to it. Under
  `NETWORK -` with only IP rules, no name resolves except the harness
  hosts, so only connections to literal addresses work. The resolver
  removes HTTPS and SVCB records, so clients can't use Encrypted Client
  Hello to hide the server name.
- **Attribution.** The resolver answers each name with its own address
  from a private pool that `ap` chooses to overlap no IP rule, so `ap`
  knows which name every address belongs to. A connection is attributed to
  the name behind the address it connects to, which is how host rules apply
  to traffic without a server name, such as SSH.
- **Name checks.** When a host or URL rule decides a connection, the
  attributed name must agree with whatever the protocol shows without
  decrypting: the TLS server name (SNI), or the `Host` header of plain
  HTTP. A connection that no name rule decides skips this check, so under
  `NETWORK +`, clients that resolve names on their own (DNS over HTTPS,
  hard-coded addresses) keep working. For an address from the pool, the
  proxy resolves the name itself and connects only to the addresses it
  gets.
- **Decryption.** The proxy decrypts TLS, with the per-run certificate
  authority, only where it must.
  - on hosts with URL rules, because the rules need the request path
  - under a broader rule of the opposite sign: with `+github.com` and
    `-gist.github.com`, on every TLS connection that `+github.com` allows,
    because a client can reuse one connection for several names. The
    default is never such a rule, so under `NETWORK +`, a `-` rule on its
    own never causes decryption.
  - on a hidden secret's bound hosts and on the hosts of `AUTH key`, to
    inject the value

  On a decrypted connection, the proxy verifies the upstream server's
  certificate and checks every request's host and path, including each
  request on a reused HTTP/2 connection (`:authority`). On a host with URL
  rules, traffic that isn't HTTP is denied.
- **Harness hosts are never decrypted for a rule.** A rule set that would
  need it, such as a URL rule for a harness host or `-console.anthropic.com`
  under `+anthropic.com`, is a build error, because those connections can
  carry your subscription sign-in. `AUTH key` is the one case where the
  proxy decrypts a harness host's traffic.
- **IP rules** are checked against the real destination address.
- **UDP** follows the same rules, with names attributed through the
  resolver. QUIC to a host that needs decrypting is blocked, so clients
  fall back to TCP.
- **Limits.**
  - Under `NETWORK +`, a deny rule for a name is best effort: a client that
    connects to a literal IP address without a name only meets IP rules.
  - On a TLS connection that the proxy doesn't decrypt, it sees the server
    name but not the requests inside. A server you allow can then be asked
    for other names it serves, including names the policy denies: other
    names on its certificate, or other sites behind the same shared front,
    such as a CDN.
  - `NETWORK` covers only the agent's own connections. The agent can ask
    an agent it may call to connect for it (see [POLICY](#policy)), and a
    harness's hosted tools, such as web search, run on the vendor's servers,
    so a query can carry data out even under `NETWORK -`.

  Use `NETWORK -` with allow rules when the boundary matters, and URL rules
  where a server's other names matter, because URL rules turn on
  decryption.

### POLICY

```
POLICY + | -
POLICY +<agent> | -<agent>
```

`POLICY` decides which other agents this agent may list, start, and message
through the built-in MCP server (see
[Agents calling agents](#agents-calling-agents)). `+` allows and `-` denies.

```
POLICY +reviewer
POLICY +tester
```

| Agentfile | May call |
|---|---|
| no `POLICY` line | every agent in your store |
| `POLICY +` | every agent in your store |
| `POLICY -` | no agent |
| `POLICY +reviewer` | `reviewer` only |
| `POLICY -stranger` | every agent except `stranger` |

- A bare `+` or `-` sets the default, with the same spellings as `NETWORK`.
  Two bare rules that disagree are an error.
- Without a bare rule, any `+<agent>` rule makes the default `-`. Naming the
  agents it may call means only those.
- `<agent>` is an exact agent name, and it can name an agent you haven't
  built yet. Two rules with opposite signs for one name resolve to deny.
- `POLICY` is a [grant](#grants). `ap build` lists who the agent may call,
  even when that is every agent.
- Plugins can't add rules.
- A called agent can call the agents its own `POLICY` allows, so a chain can
  reach further than this agent's own list.

`ap init` writes `POLICY -` into new Agentfiles, so the agents you create
start closed.

## Guest environment

| Path | Content |
|---|---|
| `/home/agent` | `$HOME`, and the working directory when nothing else sets one |
| `~/git/<repo>` | Repositories from `REPO` |
| `~/<path>` | `ADD` and `SHARE` content, by the mirror rule |
| `~/.claude`, `~/.codex` | Harness configuration generated by the adapter |

The agent runs as `agent`, a non-root user.

### Agents calling agents

Inside every guest, a built-in MCP server lets the agent list, start, and
message the agents that [`POLICY`](#policy) allows. An agent it may not call
doesn't appear in the list. `ap` routes those calls through its control API.
In the `desktop` tier, the same server provides the computer-use tools (see
[OS](#os)).

- A called agent runs from its stored image, with its own approved grants.
  A called run never rebuilds it. The caller can send prompts and read
  results, but this server gives it no way to change the called agent's
  configuration. It starts a run with a prompt only, never with run options
  such as `--share` or `--network`.
- A called run is a session. It stays open after it answers, so the caller
  can message it again. Only the run that started it can message or stop
  it. It also stops when that run ends, and a message to a stopped run is
  an error.
- An agent can't call itself or any agent already in the chain that started
  it, so chains can't loop.
- A called run is started by the `ap` process of the first run in the chain,
  so its `$NAME` values and secrets come from that process's environment.
- `ap` labels a prompt from a calling agent as coming from that agent, so
  the called agent doesn't treat it as your instruction.
- Approvals for a called run, such as a share that now resolves somewhere
  new or a request under `MODE ask`, go to you, never to the calling agent
  (see [Grants](#grants) and [MODE](#mode)).
- Any code in the agent, including plugin code, can use this server, and
  whatever `POLICY` allows, it allows to all of that code. An agent it may
  call acts with that agent's own grants, including its network, so
  `NETWORK -` doesn't stop an agent from sending data anywhere an agent it
  may call can reach.

## Example

````
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
````

The GitHub MCP server sends `Bearer ${GITHUB_PERSONAL_ACCESS_TOKEN}`, so the
harness sends the placeholder, and the proxy swaps in the real token on
connections to `api.githubcopilot.com`. The agent starts in
`~/git/agent-plus`, because of `WORKDIR`. Without it, the agent would start
in `/home/agent`, since `SHARE ./references` is a second directory.

The first `ap build` of this Agentfile asks you to approve its grants.

- `GITHUB_PERSONAL_ACCESS_TOKEN`, hidden, bound to `api.githubcopilot.com`
- write access to `/var/log/output.log`, which is also outside the build
  context
- the network policy: deny by default; allow `kagi.com`, `github.com`,
  `mcp.context7.com`, `api.githubcopilot.com`, one Codeberg repository,
  and the hosts Claude Code needs for its model and sign-in
- the agents it may call: none

## Host requirements

| Host | Status | Requirement |
|---|---|---|
| Linux x86_64 | v0.1.0 | Read and write access to `/dev/kvm` |
| macOS on Apple Silicon | v0.1.0 | Hypervisor.framework |
| Linux arm64 | next | Read and write access to `/dev/kvm` |
| Windows x86_64 | next | Windows Hypervisor Platform enabled |
| macOS on Intel | not planned | |

The guest's CPU architecture always matches the host's. Debian builds the
same packages for both.

The TLS backend, graviola, sets a CPU floor.

- **x86_64:** AES-NI, SSSE3, AVX, AVX2, ADX, BMI2, and PCLMULQDQ. In
  practice, Intel Broadwell or AMD Zen and newer.
- **arm64:** AES, SHA2, PMULL, and NEON (the ARMv8 crypto extensions). Apple
  M1 and newer qualify. A Raspberry Pi 4 does not.
