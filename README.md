# Agent+

Agent+ runs coding agents in isolated, disposable machines. You describe an
agent in an `Agentfile`: which harness it runs, what it knows, which plugins,
skills, and MCP servers it gets, and what it may reach. Then `ap run` starts
it in a Linux microVM with its own filesystem, network policy, and optional
desktop. Your real harness configuration is never touched.

> [!IMPORTANT]
> **Status: design.** Nothing is implemented yet. This README describes the
> intended product. The Agentfile reference is [SPEC.md](SPEC.md).

`ap` is a single binary, written in Rust. It includes:

- a VMM (virtual machine monitor): KVM on Linux, Hypervisor.framework on macOS
- the Agentfile builder and a content-addressed layer store
- a virtual filesystem with copy-on-write shares
- an egress proxy that enforces network rules and injects secrets
- a desktop with our own Wayland compositor, Chromium, and computer-use
  tools, shown in a native window
- adapters for Claude Code and Codex
- a control API, with an ACP bridge for editors and an in-guest MCP server
  for agents calling agents

## Getting started

### Install

Installation instructions will come with the first release. `ap` will ship
as one binary for each supported host:

| Host | Status |
|---|---|
| Linux x86_64 and arm64 | v0.1.0 (needs access to `/dev/kvm`) |
| macOS on Apple Silicon | v0.1.0 |
| Windows x86_64 | next |
| macOS on Intel | not planned |

Nothing else needs to be installed. `ap` downloads the OS image and the
harness the first time an agent needs them.

`ap` needs a reasonably modern CPU: Intel Broadwell or AMD Zen and newer on
x86_64, or the ARMv8 crypto extensions on arm64 (Apple M1 and newer; a
Raspberry Pi 4 does not qualify).

### Quick start

Create an agent that tries a plugin:

```sh
ap init try-context7
```

This writes `try-context7/Agentfile`, with the network and calls to other
agents closed. Add the plugin you want to try, and the host it talks to:

```
AGENT claude:latest
AUTH login
NETWORK -
NETWORK +mcp.context7.com
POLICY -

PLUGIN https://github.com/anthropics/claude-plugins-official/tree/main/external_plugins/context7
```

If you leave out `NETWORK +mcp.context7.com`, `ap build` suggests it,
because that's where the plugin's MCP server lives.

Run it:

```sh
ap run ./try-context7
```

The first build lists what the agent may do on your host and asks you to
approve it. Here, that's the agents it may call, which is none, and the
network policy: closed, except for `mcp.context7.com` and the hosts Claude
Code needs for its model and sign-in.
Then `ap` boots a microVM and opens Claude Code in your terminal. Sign in
once with `/login`; `AUTH login` remembers it for the next run. When you
exit, the machine is gone. Your own `~/.claude` was never involved.

Point the agent at a project:

```sh
cd ~/code/my-project
ap run try-context7 --share .
```

The share is copy-on-write, and it becomes the agent's working directory.
The agent can edit freely, and your files stay untouched. Review and bring
back what it did:

```sh
ap diff try-context7
ap apply try-context7
```

When the share is a repository's root, `ap apply` lands the agent's work on
a new branch, `ap/try-context7/<run>`. Review it as you would a stranger's
pull request.

Give it a prompt instead of a conversation:

```sh
ap run try-context7 --share . -p "Find unused dependencies and explain them."
```

A run with a prompt is headless. It streams the agent's output and exits
with its status.

Watch it work:

```sh
ap run --gui try-context7
```

The default OS comes with a desktop and Chromium. `--gui` opens the agent's
desktop in a native window.

### Core commands

> [!NOTE]
> The command set is a proposal.

#### Agents

- `ap init [dir]`: create an Agentfile with sensible defaults, network and
  agent calls closed
- `ap build [dir]`: build an agent into the local store
- `ap update [agent]`: re-resolve versions and refs, rewrite
  `Agentfile.lock`, and rebuild
- `ap fmt [dir]`: format an Agentfile
- `ap ls`: list built agents
- `ap inspect <agent>`: show an agent's layers and its approved grants:
  shares, host env vars, secrets, git credentials, network policy, and the
  agents it may call
- `ap rm <agent>`: remove an agent and its persisted state

#### Runs

- `ap run <agent> [-p <prompt>]`: start a run, interactive without a prompt
  and headless with one
- `ap ps`: list running agents, with their forwarded ports
- `ap attach <run>`: attach to a running agent's terminal
- `ap logs <run>`: show a run's output and network decisions
- `ap exec <run> <command>`: run a command inside a running agent
- `ap stop <run>`: stop a run

#### Changes

- `ap diff <agent | run>`: show unapplied changes in copy-on-write shares
- `ap apply <agent | run>`: bring changes back (a branch when the share is a
  repository's root, files otherwise)
- `ap discard <agent | run>`: throw unapplied changes away

#### State and secrets

- `ap reset <agent | digest>`: delete an agent's persisted state, including the
  sign-in from `AUTH login`
- `ap secret set|rm|ls`: manage secrets in the OS keychain

#### Integration

- `ap acp <agent>`: expose an agent over the Agent Client Protocol, for
  editors such as Zed
- `ap api`: serve the control API on a local socket

#### Distribution

- `ap build --exe`: export a standalone executable
- `ap push` and `ap pull`: share agents through a registry (after v0.1.0)

#### Diagnostics

- `ap boot [--timeout N] [-- COMMAND...]`: boot a bare machine with no agent,
  then print its kernel release and power off, or run the command and exit
  with its status

#### About

- `ap --version`: print the version

## Why Agent+?

Trying a new plugin, skill, or MCP server today means installing it into the
same harness config you use for real work. You run code you haven't read
with your credentials, your files, and your network, and undoing it is
manual. Trying the same thing in a second harness means learning a second
plugin format.

Agent+ applies the Dockerfile idea to agents. One file declares the agent.
One binary builds it and runs it in a microVM. Nothing leaks into your real
setup, and the agent gets your files, environment, and secrets only through
grants you approve.

### The problem

Agent setups drift. Every harness keeps its own config directory,
instruction file, plugin format, and MCP configuration, and every experiment
leaves something behind. Reproducing the agent that worked yesterday means
reconstructing a config directory by hand.

Agents also run with your permissions. Approval prompts on every action
defeat the point of delegating, while skipping prompts on your own machine
is a real risk, especially with code you just downloaded. Computer use
raises the stakes: in today's desktop apps, it drives your real desktop. A
sandbox makes all of this easy only if the boundary is real, and only if it
costs nothing to use.

### One Agentfile, any harness

An Agentfile describes intent, not one harness's file layout. `SYSTEM`
becomes `CLAUDE.md` for Claude Code and `AGENTS.md` for Codex. A plugin
written for one harness is translated for the other. `AUTH login` maps to
whatever file the harness keeps its sign-in in. To try the same setup in a
different harness, change one line.

Every harness gets the same machine: the same Debian-based OS, the same
toolchains (Rust, Go, Python, and Node through `vp`), Chromium, and the same
filesystem layout. Differences between agents come from the harness, not
from the environment.

### A desktop built for agents

Computer use today is a loop: take a screenshot, guess, click, sleep, and
take another screenshot. An agent's desktop runs our own Wayland compositor,
which knows when the screen has settled and what every window is. Every
harness gets the same tools for it:

- **Settled results.** An action returns once the screen has stopped
  changing, so an agent doesn't guess with sleeps.
- **Windows as objects.** Agents list, launch, focus, move, and close
  windows by ID, and capture one window instead of the whole screen.
- **A semantic UI tree.** Where an app exposes accessibility data, an agent
  acts on a named button instead of coordinates.
- **Batched actions.** A sequence of clicks and keys is one call with one
  result, instead of a model round trip for each step.

### Safe where it counts

- **A real boundary.** Every agent runs in its own Linux microVM, with its
  own kernel. Nothing is shared with your host kernel.
- **You approve what an Agentfile asks of your host.** Files outside its
  directory, write access, host environment variables, secrets, your git
  credentials, the network policy, and the agents it may call are grants.
  `ap` lists them on the first build and asks again whenever they widen.
  Shares are checked again on every run, so a share can't be swapped for a
  link to somewhere else. An Agentfile from a cloned repository can't
  quietly read your `~/.ssh`.
- **Secrets the agent can't read.** By default, the agent sees a
  placeholder. The egress proxy swaps in the real value only in approved
  headers, on requests to that secret's hosts.
- **The harness's own review stays on.** Agents run in `MODE auto` by
  default, so the harness's reviewer checks anything beyond routine work in
  the workspace. `MODE full` turns it off when the VM is all you want.
- **Your files stay yours.** Shares are copy-on-write. `ap diff` shows what
  changed, and `ap apply` brings it back, as a git branch when the share is
  a repository's root. Nothing under `.git/` is ever applied, because hooks
  and config can run commands on your host. `PERMISSIONS` keeps specific
  files out of reach for every process in the agent.
- **Network rules that mean what they say.** The most specific rule wins,
  and matching respects label and path boundaries, so `+kagi.com` never
  allows `kagi.com.evil.net`. The reference lists the limits.
- **Computer use stays inside.** The agent clicks and types on its own
  desktop, inside the VM, never on yours.
- **Your account, the vendor's binary.** Proprietary harnesses are
  downloaded from their vendors on your machine and are never
  redistributed. You sign in through the harness's own login flow, or with
  your own key.

Two defaults are open on purpose, so trusted work needs no setup:

- **The network allows everything** unless an Agentfile says otherwise,
  including your local network, services on your host's network addresses,
  and cloud metadata endpoints. `ap init` writes `NETWORK -` into new
  Agentfiles, so the agents you create start closed. Check the `NETWORK`
  lines of any Agentfile you didn't write.
- **Agents can call agents.** Through a built-in MCP server, an agent can
  start and message any other agent in your store, unless its `POLICY` says
  otherwise. A called agent acts with its own grants, so `POLICY` decides
  who an agent can ask for help. `ap init` writes `POLICY -` into new
  Agentfiles, and `ap build` lists who an agent may call.

`AUTH login` keeps the harness's own credential file between runs, like a
dev container that keeps credentials in a volume. Anything in the agent can
read it. With Claude Code, it also keeps `.claude.json`, which can list MCP
servers, so code that ran once can add a server that starts on later runs.

### Fast and small

`ap` is built so that an agent costs only what its processes use:

- **Independent layers.** Each build instruction is its own layer, keyed by
  its resolved inputs. Change one line and only that layer rebuilds.
- **One store.** Layers are content-addressed and shared across agents, so
  the OS and the harness download once.
- **Lazy everything.** The guest reads only the blocks it touches, memory is
  committed only when the guest uses it, and the desktop starts only when
  something needs it.
- **No codec locally.** The native window draws the guest's framebuffer
  directly. The remote viewer, in the next release, gets lossless tiles for
  still content and [PyroWave](https://github.com/Themaister/pyrowave) for
  motion.

> [!NOTE]
> These budgets are targets, not measurements. Every change will be
> measured against them, and a regression blocks a release.

| Budget | Target |
|---|---|
| Boot to guest init | under 150 ms |
| `ap run` to a ready harness, with layers cached | under 1 s |
| Idle overhead of `ap`, the VMM, and the guest kernel | under 64 MiB |
| `ap build` with nothing changed | under 100 ms |
| Desktop cost until first use | disk only |

### Pure Rust

`ap` is pure Rust:

- There is no C anywhere in its dependency tree, and building it compiles
  no C.
- Our own crates have no build scripts. Dependencies may keep pure-Rust
  build scripts, but only after an audit.
- Building needs the Rust toolchain plus the platform linker that rustc
  calls: the system C toolchain on Linux, or the Xcode Command Line Tools
  on macOS.

That covers everything `ap` runs on your host, including the VMM, the
network stack, TLS ([graviola](https://github.com/ctz/graviola)), the
filesystem, and the display codec. It can't cover what runs inside the
guest: the Linux kernel, the Debian userland, Chromium, the harnesses, and
the xkbcommon library our compositor needs come from their own projects,
pinned by digest.

### Prior art

Agent+ builds on ideas that already work:

- **[Docker Sandboxes](https://docs.docker.com/ai/sandboxes/)** runs
  harnesses in microVMs, with placeholder credentials and deny-by-default
  networking. It runs local MCP servers on the host rather than in the VM,
  its kit format has no plugin capability, and it offers no single-file
  export or desktop.
- **[smolvm](https://github.com/smol-machines/smolvm)** packs a machine into
  a self-contained executable that boots in under 200 ms. It does not know
  what an agent is.
- **[docker-agent](https://docs.docker.com/ai/docker-agent/)** distributes
  agents as OCI artifacts and serves them over ACP. It runs harnesses on the
  host, without isolation.
- **[Claude Cowork](https://support.claude.com/en/articles/14479288-claude-cowork-architecture-overview)**
  runs the code Claude writes in a local Linux VM with filtered egress. Its
  agent loop and computer use stay on your host, and it runs only Claude.

What Agent+ adds is the combination: harness-aware plugins, skills, and MCP
servers; one daemonless binary; the same desktop for every harness, with
computer use kept inside the VM; and a control API that drives every
harness the same way.

## Roadmap

- **v0.1.0:** Linux and macOS on Apple Silicon. Claude Code and Codex. The
  store, grant approval, copy-on-write shares, network rules, secrets,
  `AUTH`, `PERSIST`, `PERMISSIONS`, `POLICY`, the desktop (our Wayland
  compositor, Chromium, and computer-use tools) with a native window, the
  control API with its ACP bridge, and the built-in MCP server for agents
  calling agents.
- **Next:** Windows, Claude Desktop as a harness, running inside the
  agent's VM, the remote viewer, registries (`ap push` and `ap pull`), and
  approval on first run for pulled images.
- **Later** (*proposed*): VM snapshots for resuming mid-task, composition
  (reusing a base agent), a `RUN` escape hatch if demand for custom
  toolchains fragments, `ap mcp`, which lets apps on your host drive an
  agent's desktop, and ChatGPT Desktop as a harness, after Claude Desktop.

## License

MIT OR Apache-2.0, at your option.

Agent+ is not affiliated with Anthropic or OpenAI. Claude, Claude Code,
Codex, ChatGPT, and other names are trademarks of their owners.
