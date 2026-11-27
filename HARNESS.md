# Harness design

> **Status: built.** Every stage in [Build stages](#build-stages) is in place.
> Numeric settings are initial values.

## Purpose

The harness develops Agent+ to v0.1.0 with several unattended agents working
concurrently, over runs of hours to days. It selects work, proves changes,
measures them, and merges them.

Scope:

- scoring progress toward v0.1.0
- the gate every change passes
- the queue, roles, and merge process
- isolation between concurrent workers
- recovery from failures

Out of scope:

- product behavior, defined in [SPEC.md](SPEC.md)
- the conduct of an individual agent, defined in [AGENTS.md](AGENTS.md)
- publishing releases, which the owner does

## Requirements

| ID | Requirement |
|---|---|
| R1 | Workers run concurrently, and share no mutable state outside the declared slots |
| R2 | A worker can stop at any point, and any other agent can continue from GitHub and git alone |
| R3 | Scoring, gating, issue selection, and merge order are decided by programs |
| R4 | Every change is compared with its base on the same scores |
| R5 | Two changes that pass separately can't merge through `xtask merge` without being tested together |
| R6 | The harness needs the owner only for the actions listed in [Owner actions](#owner-actions) |
| R7 | Only tier T3 spends on model calls for checks |

## Release criteria

v0.1.0 is complete when all of these hold on a release binary installed on a
clean machine:

- Every promise within v0.1.0 has a passing check.
- No sentence within v0.1.0 is marked *Proposed*.
- Every journey step passes on Linux x86_64 and macOS on Apple Silicon.
- Every budget is met.
- Every floor holds, and none is pending.

## Terms

| Term | Meaning |
|---|---|
| Unit | A sentence, list item, or table row extracted from SPEC.md |
| Promise | A unit that isn't marked or waived |
| Check | An automated test that proves one or more promises |
| Hill | A score that one change can raise |
| Floor | A condition that must always hold |
| Journey | The ordered steps of real use of `ap` |
| Milestone | A named group of journey steps that one parent issue delivers |
| Roadmap | The milestones in order, and what comes after v0.1.0 |
| Budget | A performance or size target |
| Ledger | The per-merge record of scores and budgets |
| Gate | The program that decides whether a change may merge |
| Lane | An independently buildable area of the codebase |
| Slot | An exclusive or counted right to use a shared resource |
| Activity lock | A host-wide lock that builds and VM runs share, and a bench holds alone |
| Lease | A time-limited claim on an issue |

## Architecture

```
drafter: milestone -> draft parent issue
owner -> labeled parent issue
planner: score gaps -> sub-issues
dispatcher: claimable issues -> one worker per free slot
worker: worktree -> check -> change -> gate -> review -> PR
merge slot: rebase -> gate -> ledger record -> squash merge -> main
nightly job: main -> slow tiers -> report, or a revert PR
```

| Component | Kind | Model | Responsibility |
|---|---|---|---|
| `xtask` | Program | None | Scoring, gating, measuring, selection, and merging |
| Dispatcher | T3 Code scheduled task | Sonnet | Starts workers for claimable issues |
| Planner | T3 Code scheduled task | Opus | Files issues from score gaps |
| Triage | T3 Code scheduled task | Opus | Says what each unlabeled issue needs, and closes the fixed and the covered |
| Drafter | Delegated agent, from the planner | Fable | Drafts the parent of a milestone that has none |
| Worker | T3 Code thread, one per issue | Opus | Takes one issue from claim to merge |
| Reviewer | Delegated agent, one per round | Another provider | Reviews a diff, and changes nothing |
| Nightly job | T3 Code scheduled task | Sonnet | Runs `cargo xtask nightly` on `main`, and files an issue on failure |

| State | Stored in |
|---|---|
| Work queue, claims, and progress | GitHub issues, labels, and status comments |
| Work in flight | Pushed branches |
| Scores and budgets over time | `harness/ledger/` |
| Waived sentences | `harness/waived.txt` |
| Journey and milestone definitions | `harness/journey.toml` |
| Budget targets and bands | `harness/budgets.toml` |
| Audited build scripts | `harness/build-scripts.txt` |

No component keeps state anywhere else.

## Hills

Hills are compared as counts: proven promises, passing steps, and budgets met.
A new sentence raises a denominator, which doesn't lower a hill.

A PR raises at least one count and lowers none. A PR for tooling or seams that
can't raise one carries a `Hill: none, <reason>` line in its body, which
`cargo xtask merge` requires and the reviewer confirms. A `Hill:` line that
gives no reason fails the merge, whatever the counts do.

### Spec coverage

**Score:** proven promises divided by all promises.

`xtask` extracts units from SPEC.md by this rule:

- Code fences are ignored. A unit belongs to the nearest heading above it, and
  headings are unique.
- A paragraph or list item is unwrapped, then split after `.`, `?`, or `!`
  followed by a space, outside code spans. `e.g.` and `i.e.` don't end a
  sentence.
- A non-header table row is one unit.
- A fragment ending in a colon isn't a unit.
- A unit's text is its source with whitespace collapsed and any list marker
  removed.
- Identical units in one section count once.
- A `*Proposed*`, `*Proposed:*`, or `**Planned.**` marker covers its whole
  paragraph, list item, or table row. Marked units aren't promises.

A check names its promise in a comment directly above the test, quoting the
sentence exactly:

```rust
// spec(SHARE): A missing `<src>` is an error, at build time and at run time.
#[test]
fn missing_src_fails() { /* ... */ }
```

- `spec(<heading>)` names the SPEC.md section that holds the sentence.
- A quote is the unit's text on one line, however long.
- A quote that no longer matches SPEC.md fails the gate.
- A promise is proven when a check quoting it passes. A test in a package that
  depends on a `fake-*` crate earns no score. The fake harness running inside
  a T1 guest is allowed, because it isn't linked into the test.
- Check function names are unique within a package, which is how a test result
  maps back to its check.
- Descriptive units are listed in `harness/waived.txt`, each with a reason. A
  waiver that matches no unit fails the gate, and so does a check that quotes
  a waived unit.
- A PR shows each new check failing before the change that makes it pass. This
  is evidence for the reviewer, not something a program can check.
- The nightly job runs mutation testing on crates changed that day. A check
  that survives a mutation of the code it covers opens an issue.

### Journey

**Score:** passing steps divided by all steps.

A step is a script that drives an installed release binary in a clean
environment. The binary is statically linked for
`x86_64-unknown-linux-musl`, so the same file can run as the guest's `/init`.
A step counts only if every step in its `needs` also passes.

A step marked `spend` makes a real model call. The merge sequence leaves it,
and any step that needs it, out of the run and the score. The nightly job runs
each one once, as a [paid check](#check-tiers), and the roadmap counts it like
any other step. The nightly report counts those it couldn't run. A step that
names a `floor` fails while that floor's check doesn't exist.

`cargo xtask journey` prints each step as `pass`, `fail`, or `skip`, then the
count out of all steps. It exits 0 whenever it could run, because what fails a
merge is a count lower than the base's. Each step gets a new `HOME` and working
directory, and only `HOME` and `PATH` in its environment.

The steps, with their `needs` and milestones, are listed only in
`harness/journey.toml`, in the format under
[Journey definition](#journey-definition). The `Roadmap details` issue shows
each step as the last journey ran it.

### Budgets

**Score:** budgets met, out of the budgets with a target.

| Budget | Target | Band |
|---|---|---|
| Boot to guest init | under 150 ms | 10% or 5 ms, whichever is larger |
| `ap run` to a ready harness, with layers cached | under 1 s | 10% or 5 ms, whichever is larger |
| Idle overhead of `ap`, the VMM, and the guest kernel | under 64 MiB | 5% or 2 MiB, whichever is larger |
| `ap build` with nothing changed | under 100 ms | 10% or 5 ms, whichever is larger |
| Desktop cost until first use | disk only | none |
| Stripped release binary | to be set | 5% |
| `ap --version` | none | 20% or 2 ms, whichever is larger |
| The pinned `OS minimal` layer | none | none |
| The pinned guest kernel | none | none |

`harness/budgets.toml` holds the targets and bands. A budget with no band is
recorded and can't fail.

The sizes of the pinned layer and kernel are the same on every machine, so the
bench doesn't measure them. Nothing compares them, so they can't have a target
or a band. Every record reads them from the copies that T1 fetches into
`target/guest`. A missing copy, or one whose digest isn't the one its pin
names, is a problem. A refreshed record reads them too, from the rebased tree.

The bench compares the medians of 15 interleaved pairs of base and candidate
runs. A change that worsens a budget by more than its band fails, unless its
ledger record has a reason, taken from a `Budget reason: <text>` line in the
PR body. The reviewer confirms the reason. The nightly report lists every
reason given that day.

Boot time runs from spawning `ap boot` until its ready line appears, by the
host's clock. Each build gets one warm-up run first, and all runs share one
`HOME`, so no download is timed. A base with no `ap boot` is reported as no
comparison, and the candidate's value is still recorded. A base whose `ap boot`
rejects `--timeout` is a problem.

A build with nothing changed is timed on a fixed directory, the Agentfile of
step 5, with its grants approved through `--approve`. Each build of `ap` has
its own `HOME`, and builds the directory once first, so every timed run is a
rebuild. A base with no `ap build`, or one from before `--approve`, is
reported as no comparison.

`ap run` is timed headless, with `-p`, from spawning it until the first byte
of its output, on a fixed agent whose harness is the fake one. Each build of
`ap` builds an agent of its own with its grants approved through `--approve`,
and runs it once first. All runs share one `HOME`, so no build or download is
timed. Every run has a timeout, and ends before the next starts. A base with no
`ap run` is reported as no comparison.

Idle overhead is measured the same way, on an agent whose harness waits after
it reports. Once the run has reported, the bench sums the proportional set
size, from `/proc/<pid>/smaps_rollup`, of `ap run` and every process under it,
which includes the run process that holds the VM. It then ends the run, and
waits until those processes have stopped.

## Floors

A failing floor blocks a merge regardless of scores.

| Floor | Verified by |
|---|---|
| No C in the dependency tree, and no build scripts in our crates | `cargo metadata` and `cargo tree` |
| A hidden secret can't be read from the guest | `floor_hidden_secret` |
| A closed network stays closed | `floor_closed_network` |
| A share can't reach outside its root | `floor_share_root` |
| The guest can't run anything on the host | `floor_guest_host` |
| On Linux, the process that runs a guest can't start a process or a program, or open a host file | `floor_vmm_confined` |
| No check is removed while the sentence it quoted is still a promise | Comparison with the base |
| `main` builds and passes the gate | Nightly job |

**C floor.** On Linux x86_64 with glibc and with musl, Linux arm64, and macOS
arm64, with all features on and counting dev and build dependencies, the
packages Cargo builds include none with a `links` key, none named `cc`,
`cmake`, `bindgen`, or `pkg-config`, no C, C++, or assembly source files, and
no prebuilt library. A package reachable only through a weak feature, like
`dep?/x`, isn't built, so it doesn't count. Assembly inside Rust source, as in
graviola, is allowed. A
build script is allowed only in a dependency listed in
`harness/build-scripts.txt`, and never in our own crates. T0 also builds with
`CC` and `CXX` pointing at a program that fails.

**Removed-check floor.** The base is the commit where the branch left
`origin/main`. For each promise, the gate counts the checks that quote it in the
base and in the working tree. A lower count fails, so a check can be renamed or
moved, but not dropped. In a repository with no `origin/main`, the gate prints
`skip base` and compares nothing.

**Pending floors.** A floor whose check doesn't exist yet is pending. A pending
floor passes the gate and fails a release. Escape checks are tests in T1. A
journey step names the floor it needs with its `floor` field in
`harness/journey.toml`, and it can't pass until that floor's check exists.

## Check tiers

| Tier | Runs | Trigger | Model spend |
|---|---|---|---|
| T0 | Format, lint for Linux and macOS, unit tests, spec quotes, and floors | Every gate | No |
| T1 | The real `ap` and a real microVM, with the fake harness | Every gate | No |
| T2 | Real Claude Code and Codex in the guest, with no credentials | Adapter changes, and nightly | No |
| T3 | A real model turn, on the cheapest model | Nightly, and before a release | Yes |
| Bench | Budgets, against the base build | Changes that can affect a budget, and nightly | No |

**Paid checks (T3).** They run on a Claude Console workspace and an OpenAI
project used only for them, each with a monthly spend limit the owner sets.
The providers enforce the limits, so no program here can pass them. Each key
is scoped to its workspace or project, with an expiry. The nightly job gives
a step marked `spend` its provider's key and no other, the way a user's
`AUTH key` run gets it. It reads the Anthropic key
from `~/.config/agent-plus/paid-checks/anthropic-api-key`, and gives it as
`ANTHROPIC_API_KEY`. It reads the OpenAI key from
`~/.config/agent-plus/paid-checks/openai-api-key`, and gives it only to step
13, as `CODEX_API_KEY`. Nothing prints a key. Without its file, a step
doesn't run. Claude Code runs on Haiku 5.5, `claude-haiku-5-5`, and Codex on
its default model.

T1 and T2 checks live in the packages `t1` and `t2`, so T0 leaves them out.
T0 lints the whole workspace twice: once for Linux, and once for
`aarch64-apple-darwin`. Linux can't link or run a macOS build, but it can
compile and lint one, so the code that only macOS builds stays checked. A
change can affect a budget that the bench measures when its diff touches
`.cargo/`, `guest/kernel/`, `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`,
`harness/budgets.toml`, `xtask/src/bench.rs`, or what `ap` and
`fake-harness` are built from under `crates/`. Under `crates/`, neither is
built from a package's `tests/` directory, or from the packages `t1`, `t2`,
`fuzz`, `fake-upstream`, and `fake-vm`. Every other path there counts, and a
test fails when a package is on neither side of that rule.

**Level spec count.** When the spec count is level with its base's, the gate
says that the PR body needs a `Hill: none, <reason>` line unless the journey
or a budget rises, and still passes.

**Adapter changes.** A change is an adapter change when the tree, with its
untracked files, differs from its base under `crates/ap/src/adapter/`,
`crates/ap/src/lock.rs`, `crates/t2/`, `crates/fake-upstream/`, `guest/os/`,
or `guest/kernel/`. The gate runs `t2` for an adapter change, and for a tree
that is `main` as it is: one with no base, or no change from it but in
`harness/ledger/`. It prints `pass t2` or `fail t2` on a line of its own.
Otherwise it prints `skip t2` and leaves `t2` out. A `t2` check counts toward
the spec score only when `t2` runs, so a promise needs a check in another
tier too. The gate's `spec` check fails a promise that a `t2` check quotes
when no passing check in another tier proves it. A check in a package that
links a `fake-*` crate earns no score, so it doesn't count. The nightly job
tests `main` as it is, so its gate runs `t2`, and that run is its T2 check.

**Guest kernel.** T1 fetches the kernel that `guest/kernel/pin.toml` pins
into `target/guest/vmlinux`, and checks its digest on every use. The gate keeps
a copy at `xtask/kernel/vmlinux` in the shared git directory, and gives it to a
tree that lacks it, so a fresh tree doesn't fetch it again. Only a file whose
digest matches the pin moves either way. A gate in a tree that holds the kernel
makes no request for it. A local server proves `ap`'s downloader on every gate.
The t1 tests that download from a real release are ignored, and only the
nightly job runs them.

**OS layer.** T1 fetches the `OS minimal` layer that `guest/os/pin.toml` pins
into a store in `target/guest` once, and its tests start their stores from it.
A gate in a tree that holds it makes no request for it. One ignored t1 test
has `ap build` download the layer from its real release, and only the nightly
job runs it.

**Fake harness.** A guest program that implements the adapter contract and
records its arguments, environment, files, and network view.

**Harness probe (T2).** The harness starts with no credentials, and the check
reads what it reports before sign-in. For Claude Code, that is the `init` event
of `claude -p <prompt> --output-format stream-json --verbose`, which lists the
working directory, MCP servers, skills, and commands. For Codex, the check
starts `codex app-server` with the `-c` overrides the adapter gives a run. It
sends JSON-RPC requests after the `initialize` request and the `initialized`
notification. The result of `thread/start` gives the working directory, model,
reasoning effort, approval policy, approval reviewer, sandbox, and the
`AGENTS.md` files that Codex loaded. `skills/list` lists the skills, and
`mcpServerStatus/list` lists the MCP servers with their tools. None of these
needs a sign-in or the network. Codex lists no commands, so the check reads
none. `codex exec --json` reports none of this before its first model call.

**Bench.** Builds the static `ap` of the base and of the candidate under a
shared hold of the activity lock, then measures in the measuring slot, which
is an exclusive hold. It interleaves runs of the two builds, and compares them.

**Activity lock.** Every build, test, and VM run started through `xtask` takes
the lock shared. A bench takes it exclusively: it waits for current holders to
finish, and new holders wait while a bench is pending. Every holder passes
through a turnstile file, which a pending bench holds. Both files live in the
shared git directory. A nested `xtask` inherits its parent's hold. The kernel
releases a hold when its process exits. `cargo` builds `xtask` itself before
it can take the lock, which is one reason `xtask` has no dependencies.

**Places.** A run of the tests also takes one of a set number of places, so
only that many run at once in a repository, across all its worktrees, and
the rest wait their turn. The gate, a merge, `prepare`, the nightly job,
`score`, `gaps`, `record`, `roadmap --details`, and `cargo xtask run` each
take one, before the activity lock, so a run that waits for a place never
holds up a bench. A nested `xtask`
inherits its parent's place, and the kernel frees a place when its holder
exits. The places live beside the activity lock.

A proof under the merge slot waits for a place apart from the queue, and gets
the next one that frees, because every merge waits for it. Before each
package, a run of the tests with a place of its own lets waiting work go
first. It gives its place to a proof under the slot that waits for one, or
lets go of the activity lock while a bench waits for it. Then it takes back
what it let go: the place first, then the lock through the turnstile. The
nightly job does the same between its checks. A proof under the slot keeps
both, and so does a run whose parent passed its place down.

**Fuzzing.** The nightly job fuzzes the Agentfile parser, network rule
matching, and device input from the guest. A target for code with an oracle
compares the two, and a difference fails like a crash. An oracle kept as a
dev-dependency counts toward the C floor.

**Flaky tests.** A test is never retried. It is fixed, or quarantined with a
linked issue. Quarantine lowers the spec score, so it needs the owner.

## Lanes

Each lane is a crate with its own tests. It builds against a fake of each
neighboring lane, so lanes have no ordering dependency during development.

| Lane | Owns | Builds against |
|---|---|---|
| Agentfile | Parser, formatter, lockfile, and grants | Nothing |
| Store | Layers and image assembly | Fixtures |
| VMM | KVM, virtio devices, and boot | A small kernel and test init |
| Filesystem | Copy-on-write shares, `ap diff`, and `ap apply` | An in-process device |
| Network | Proxy, rules, secrets, and TLS | Local fake upstreams |
| Adapters | Claude Code and Codex translation | Golden files, then T2 |
| CLI | Commands and approvals | A fake VM |
| Guest | Init, guest agent, and built-in MCP server | A fake host channel |
| Desktop | Compositor, Chromium, and computer-use tools | A headless display |
| Control | Control API and ACP bridge | A fake run |
| macOS | Hypervisor.framework backend | A macOS host |

Files outside any single lane follow these rules:

| File | Rule |
|---|---|
| `SPEC.md` | A new sentence merges with its check. A rewrite waits for the owner |
| `harness/ledger/` | One new file per merge |
| `harness/waived.txt` | A new waiver waits for the owner. Union merge through `.gitattributes` |
| `harness/journey/` | Step scripts, which any PR may change |
| `Cargo.lock` | Regenerated on rebase. A new dependency gets its own PR, and that PR's ledger record is its cost |

### What waits for the owner

`cargo xtask merge` reads a PR's diff with `main` to decide whether the PR
waits for the owner. The owner's merge is for awareness, not a security
boundary. A PR waits when it:

- changes or removes a SPEC.md unit that `main` has. A unit ignores
  whitespace, so a reflowed paragraph doesn't wait.
- adds a waiver to `harness/waived.txt`.
- raises a target or a band in `harness/budgets.toml`, or removes a budget.
- adds a line to `harness/build-scripts.txt`.
- lowers the number of `#[test]` functions under `xtask/`. They are counted,
  not named, so a test can be renamed or moved.
- lowers a count.
- changes a path under `.github/`, because a workflow there runs with a token
  that can write, and a leaked token can't be taken back.
- changes a file under `xtask/src/` that decides what a merge accepts:
  `accepts.rs`, `base.rs`, `floors.rs`, `gate.rs`, `journey.rs`, `ledger.rs`,
  `readme.rs`, `score.rs`, `spec.rs`, `testrun.rs`, or `waits.rs`. A file
  decides when it says what waits for the owner, what a count or a floor is
  and whether one fell, which checks a proof runs, or what makes a check pass.

Each of these lowers the bar for later PRs, or can't be undone by a revert.
Everything else merges under `auto:merge`: a new SPEC.md sentence with its
check, AGENTS.md, HARNESS.md, the task briefs, the rest of `harness/`, and the
rest of `xtask/`, such as the timeout and the slot in `merge.rs`, and how
`bench.rs` measures a budget. Nightly mutation testing is what notices a
weakened test that kept its count. The xtask built from `main` decides, so a
PR can't let itself through. `merge` fails a PR that waits with `fail owner`,
and names each reason. `prepare` passes it, and its comment lists each reason
under "Merging this PR at `<head>` accepts:".

## Data formats

### Journey definition

```toml
[[milestone]]
name = "Boot"
summary = "Start a small virtual machine, and run a command in it"
commands = ["ap --version", "ap boot"]

[[step]]
id = 3
name = "The VMM boots a kernel to guest init"
milestone = "Boot"
needs = [1]
script = "harness/journey/03-boot.sh"
```

A step can also set `floor = "floor_guest_host"`, or `spend = true`. A step
that spends gets the Anthropic key, unless it also sets
`provider = "openai"`.

The `[[milestone]]` tables are in roadmap order. `summary` says what a user
can do when the milestone is done, in words that need no knowledge of the
harness, because the roadmap shows it to anyone. `commands` lists the `ap`
subcommands that the milestone delivers, and it can be empty. Every step names
one milestone, every milestone has a step and a summary, and no command is
listed twice. A file that breaks one of these is an error.

The milestones list exactly the commands that SPEC.md has for v0.1.0. A
command is a heading under SPEC.md's Commands heading, and it is for v0.1.0
unless its entry starts with `**Planned.**` or `*Proposed*`. The gate's `spec`
check fails when a milestone lists a command that has no such heading or is
marked, and when a v0.1.0 command is in no milestone.

The README describes every command in SPEC.md, marked or not, in an item
under its Core commands heading. The item starts with the command in
backticks, as in `` `ap init [dir]`: create an Agentfile ``, and one item can
name two commands. The roadmap shows what follows the colon, so no command is
described twice. The `spec` check fails when a command has no such item.

### Budgets

```toml
[[budget]]
key = "version_ms"
name = "ap --version"
band_pct = 20.0
band_abs = 2.0
```

`target` is optional, and so is the band.

### Waivers and audited build scripts

`harness/waived.txt` holds one waiver per line,
`<heading><TAB><unit><TAB><reason>`. `harness/build-scripts.txt` holds one
audited package per line, `<name> <version> <reason>`. Lines starting with `#`
are comments. A missing file lists nothing.

### Ledger record

Each merge adds `harness/ledger/<issue>.toml`. Git history orders the records.

```toml
issue = 123
pr = 130
commit = "abc1234"
base = "def5678"
date = "2026-11-20"
host = "linux-x86_64"
spec = { proven = 417, promises = 812 }
journey = { passing = 7, steps = 19 }
reason = ""

[budgets]
boot_ms = 141.2
run_ms = 930
idle_mib = 58
noop_build_ms = 71
binary_bytes = 9312000
```

| Field | Meaning |
|---|---|
| `commit` | The rebased head that was gated. The merge is that tree plus this record |
| `spec`, `journey` | Scores at that commit |
| `reason` | Required when a budget is outside its band, otherwise empty |
| `budgets` | Every budget's latest value. One this change didn't measure carries over from the previous record. The pinned sizes are read for every record |

A change's base scores are those of the newest record in the history of its
base, or zero if there is none.

When the bench runs, both budget counts come from that run: the candidate's
from its values, and the base's from the values measured for the base. A
budget the base can't be measured for isn't met for the base. When the bench
doesn't run, both counts come from the ledger, the candidate's from its new
record.

### Issue and status comment

The issue body and status comment follow [AGENTS.md](AGENTS.md#issues). `xtask`
reads the `Touches` and `Blocked by` lines, the parent that GitHub records for
a sub-issue, and the `State`, `Thread`, `Branch`, and `Lease until` fields.

- The paths in `Touches` are its backticked words. Two issues overlap when one
  path is a prefix of the other, in whole parts, unless they name different
  sections of one file.
- A path names one section of a file when it ends in `#` and the heading's
  link anchor, such as `SPEC.md#network`. Anchors compare without case. A
  path with no section overlaps each section of its file.
- An issue with no `## Touches` isn't claimable, and neither is a parent: an
  issue that GitHub lists another issue under, as a sub-issue, or one with a
  line that starts with `Milestone:`, even before it has a sub-issue.
- A parent that delivers a milestone names it on a line of its own,
  `Milestone: <name>.`. The planner and `cargo xtask roadmap` read that line.
- `xtask` reads status comments only from the repository owner's account.
- When two live claims exist, the status comment with the lowest id wins.
- An issue has a live claim when it is open, `in-progress`, not parked, and
  has a live status comment. The thread in the winning comment holds it.
- The last claim is the status comment with the latest lease, because every
  edit renews it. A tie goes to the lower id.

## Commands

Every command lives in one `xtask` crate, in Rust. Exit status 0 is a pass, 1
is a failure, and 2 is bad usage or an environment error.

| Command | Behavior |
|---|---|
| `cargo xtask gate` | Runs T0, T1, the floors, and T2 for an [adapter change](#check-tiers), compares counts with the base, and prints the result. It posts no status |
| `cargo xtask score` | Prints the three scores |
| `cargo xtask gaps` | Names the first milestone on the roadmap with a step that doesn't pass. Then lists failing steps in journey order, each with its milestone, then missed budgets, then unproven promises in SPEC order |
| `cargo xtask journey` | Runs the journey against a release build |
| `cargo xtask bench` | Takes the measuring slot, and compares with the base |
| `cargo xtask next` | Prints claimable issues as JSON |
| `cargo xtask claim <issue> [--thread <link>]` | Claims an issue for the current branch, prints the status comment id and the thread it replaced as JSON, and exits 1 if another claim wins |
| `cargo xtask status <issue> --thread <link>` | Replaces the holder's status comment with stdin, sets its `Thread` and renews its `Lease until`, and exits 1 for any other thread |
| `cargo xtask record --issue <n> --pr <n>` | Writes the ledger record for the current head |
| `cargo xtask merge <pr> [--thread <link>]` | Runs the [merge sequence](#merge-sequence) |
| `cargo xtask prepare <pr> [--keep-budgets] [--thread <link>]` | Runs the [merge sequence](#merge-sequence) up to the squash, under the rules for the owner |
| `cargo xtask refresh` | Runs the [refresh](#refresh), which proves and merges the `land-needed` PRs that the owner approved |
| `cargo xtask land <pr>` | Runs the [merge sequence](#merge-sequence) through the squash, under the rules for the owner |
| `cargo xtask prove <pr> <merge\|prepare\|land> [--keep <file>]` | Proves a PR that changes `xtask/` outside `xtask/tests/` with its own xtask, in its rebased tree, for the [merge sequence](#merge-sequence) |
| `cargo xtask run <command>` | Runs a build or test command in a place, under a shared hold of the activity lock and a memory limit. On macOS it runs with no memory limit, and says so on stderr |
| `cargo xtask nightly` | Runs the [nightly sequence](#nightly-job) |
| `cargo xtask report` | Builds the nightly report from the ledger |
| `cargo xtask roadmap [--details]` | Prints the [roadmap](#roadmap), or its details, and writes nothing |
| `cargo xtask remove` | Removes the worktree it runs in, and its local branch. It exits 1 and keeps both when git refuses a worktree with changes. When git fails after it starts deleting, `remove` deletes what is left at the path git recorded, and deletes the branch only once the directory stays gone |

`cargo xtask next` prints an array of
`{"issue", "touches", "autonomy", "branch", "worktree"}`, lowest issue number
first, with at most one entry per free worker slot on this host. On a Mac it
names only issues with `host:macos`, and on Linux only issues without it.
`claim` refuses an issue for the other host. When two issues overlap, the
lower number stays, whichever host each is for, because no claim slot spans
the two. Any issue that overlaps a live claim is left out, and an abandoned
issue is included. `branch` names the existing branch of an abandoned issue,
or the branch of the last claim on an available issue, if GitHub still has it.
`worktree` names where that branch is still checked out. Both are otherwise
null.

`claim` prints `{"issue", "comment", "branch", "replaced"}`. `replaced` is the
thread of the last claim, or null if there was none or it was this thread.

While an issue has a live claim, `merge` and `prepare` refuse its PR with exit
1, and name the holder, unless `--thread` names it. They check again after the
proof. With no live claim, they take any thread or none. `status` takes only
the holder, or with no live claim, the thread of the last claim. To `xtask`, a
thread is only a string.

`score`, `gaps`, and `report` take `--json`. Results go to stdout, and
diagnostics go to stderr.

## Issue states

| State | Condition | Next |
|---|---|---|
| Unavailable | No `auto:*` label | Available, when the owner labels it, or the planner adopts it under a labeled parent |
| Blocked | An issue in `Blocked by` is open | Available, on its branch if it has one, when it closes |
| Available | Claimable under AGENTS.md | Claimed |
| Claimed | `in-progress`, with a live lease | Parked, Blocked when it waits for another issue, Abandoned, Closed, or Available on its branch, when a Mac hands it over |
| Abandoned | `in-progress`, with an expired lease and no commits since | Claimed |
| Parked | `land-needed` or `answer-needed` | Available on its branch, when the owner removes the label |
| Closed | Its PR merged, or triage closed it as fixed or covered | |

An issue with `host:macos` is Available and Abandoned only on a Mac, and one
without it only on Linux. A live claim takes a worker slot only on its own
host, but its `Touches` hold on both, because both push to one `main`.

## Processes

### Scheduling

The dispatcher, planner, triage, and nightly job are T3 Code scheduled tasks.
Each is bound to its own thread, whose settings give it its model and full
access, because no one is there to answer a prompt. Each run starts a fresh
session, so no run depends on the context of an earlier one. A run moves its
checkout to `origin/main`, then follows its brief in `harness/tasks/`.

### Dispatch

Runs every dispatch period.

1. Stop if free disk is below the minimum.
2. Stop if the worker's harness is blocked by a usage limit.
3. Run `cargo xtask next`.
4. For each entry, start one worker with a brief naming the issue: in a new
   worktree on its `branch`, or from `origin/main` if it has none. A clean
   `worktree` is removed first. One with changes is reused as it is.

Dispatch is idempotent, because all state is on GitHub. A worker that loses a
claim race exits.

The dispatcher learns whether a harness is blocked from `to11y
rate_limit_status`, which reads its limits from the sign-in it already has. A
harness is blocked when the scope its model draws on is. A command needs no
MCP setup in the session that runs it, and it shows in the transcript. `to11y`
runs only on Linux. When it is missing or fails, and on a Mac, the dispatcher
goes on, and a worker started against a limit stops at its first turn, as
before.

A Mac runs a dispatcher of its own, with the same brief. `next` there names
only issues with `host:macos`, against the Mac's own worker slots, and `next`
on Linux leaves those issues out. `xtask` learns the host from `uname -s`.

### Planning

Runs every planning period. The planner reads `cargo xtask gaps`, and files
sub-issues under a parent. Each sub-issue inherits its parent's `auto:*` label,
whatever paths it will touch, because `merge` decides from the diff whether its
PR waits for the owner. Before it files one, the planner looks for an open
issue with no parent and no `auto:*`, `answer-needed`, `land-needed`, or
`in-progress` label that already covers the gap. It adopts that issue as a
sub-issue, with the parent's label, if it has no sub-issues of its own, and its
definition of done names checks and not a decision of the owner's. An adopted
issue counts as a filed one. The planner files none if the issues in progress
and the issues `cargo xtask next` lists already fill the worker slots. It also
keeps a parent to at most 3 open sub-issues without `land-needed`, whether they
are blocked or not, because a sub-issue filed far ahead is written before the
work it builds on exists. A prepared PR waiting for the owner doesn't count.

Every run starts by adopting the issues that hold up labeled work: each open
issue with no parent and no label, filed by the owner's account, that an open
issue with an `auto:*` label names in its `Blocked by` line. The planner
adopts it under #840, with the same checks and #840's label, while #840 is
open and labeled. It needs no gap, and it is apart from the milestone's limits,
because the work it blocks is already planned. It still takes a worker slot.
An issue that someone else filed waits for the owner.

The planner works on one milestone at a time: the first on the roadmap with a
step that doesn't pass, which `cargo xtask gaps` names. `gaps` runs no step
marked `spend`, nor one that needs such a step, so for each of those it skips,
it reads the open issue titled `Roadmap details`, which the nightly job last
wrote. The step passes when that issue ticks it under the same id and name.
Otherwise it keeps its milestone current. The milestone's parent is the open
issue that the owner has labeled, and whose body names that milestone in a
`Milestone:` line. A parent whose milestone passes is never chosen, so leaving
it open holds nothing up.

A milestone never waits for its parent to be written. When that milestone, or
the one after it, has no issue that names it in a `Milestone:` line, open or
closed, the planner has a drafter write one, before it checks the worker slots.
The drafter runs on the stronger model that the planner's brief names, because
every sub-issue builds on the parent. A draft has `answer-needed` and no
`auto:*` label, so nothing claims it or plans under it, and the roadmap lists
it under what waits on the owner. Its decisions separate what it chose from
what needs the owner's answer. Once the owner labels it and removes
`answer-needed`, the planner files sub-issues under it. Until then, the planner
says which milestone waits on the owner, and stops. No program closes a parent,
because closing it is the owner's acceptance of the milestone.

The order of the milestones in `harness/journey.toml` is the order of the work.

### Triage

Runs at the triage time, after the nightly job has written the roadmap. Its
inbox is the open issues with no parent and no `auto:*`, `answer-needed`,
`land-needed`, or `in-progress` label, apart from the ones `xtask` writes.
`Roadmap details` lists nearly the same issues under "To label or close". An
agent that finds other work files it this way, so the inbox grows with every
review.

The task reads each issue against `origin/main`, and closes it only with
proof. It closes one as completed when its definition of done already holds,
and names the commit. It closes one as a duplicate when another open issue has
the same goal, after it copies the owner's decisions to the issue that stays.
It checks that an issue is still in its inbox before it closes it. It never
closes a parent, never adds or removes a label, and never works an issue.

It says what each of the rest needs in one comment on `Roadmap details`, which
it replaces on every run: to label, to answer, waiting, or after v0.1.0, each
with one line of reason, and the most urgent first. The comment also lists the
open parents whose sub-issues are all closed, labeled or not, because closing
a parent is the owner's.

### Worker

1. Claim the issue.
2. Read the issue, the sentences it names, and the base scores.
3. Write the check, and record it failing.
4. Make the smallest change that passes.
5. Tidy what it changed: one copy of each thing, and nothing left unused.
6. Run the gate, and the bench if a budget can be affected. When the gate
   says the spec count is level, add a `Hill: none, <reason>` line to the PR
   body before `merge`, unless the journey or a budget rises.
7. If a floor failed, or no count rose and the PR has no `Hill: none` line,
   revert and try again, within the attempt limit. Past the limit, park the
   issue.
8. Get a review, within the round limit, from the first reviewer that isn't
   blocked.
9. Open the PR. `xtask/gate` is its only check, the worker's own `merge` or
   `prepare` posts it, and no bot reviews the repository, so the worker waits
   for neither.
10. Merge if the label is `auto:merge`. Otherwise park with the PR ready.
    `merge` and `prepare` run as a background command with the harness's
    longest limit, because a wait for the slot can take an hour, and the
    worker waits for it without polling. If the harness stops it all the
    same, the worker stops what is left in the command's session, except a
    `gh pr merge` that keeps the slot, and starts it again.
11. Remove the worktree and its local branch with `cargo xtask remove`, after
    a merge, or after parking with `land-needed` because `prepare` passed.

A worker that another issue blocks, and not a question for the owner, files
that issue if it doesn't exist, and adds `Blocked by` to its own. It parks
its status and removes `in-progress`, with no `answer-needed`. The issue is
Blocked, and the first dispatch after the other issue closes continues it on
its branch. The planner gives the other issue a label by adopting it.

A worker on a Mac does steps 1 to 5 and 8, with the checks that run there,
because the gate needs Linux, KVM, and systemd. It then opens the PR with its
proof and its review, parks its status comment, removes `host:macos` and
`in-progress`, and removes its worktree as step 11 does. The issue is
Available on its branch, so the next dispatch on Linux starts a worker that
takes it over, runs the gate, and merges or prepares the PR. That worker keeps
the Mac's proof, as AGENTS.md says. Only the gate on Linux decides a merge, so
no merge slot spans machines.

Model selection escalates in a fixed order: improve the brief, raise the
effort, then change the model. Reviewers use a different provider from the
worker when one is available. Before it asks a reviewer, a worker on Linux
checks that reviewer's scope with `to11y rate_limit_status`, as the
dispatcher does, and skips it while it is blocked. The PR names a skipped
reviewer, and nothing else from `to11y`, so no usage figure, plan, or reset
time reaches GitHub.

### Merge sequence

Only one merge runs at a time, under an exclusive lock on a file in the shared
git directory. The kernel releases the lock when the process exits, so a killed
merge can't keep the slot.

`cargo xtask merge` runs every step. `cargo xtask prepare` stops before the
squash, and leaves the owner a complete PR to merge with any button. `cargo
xtask land` follows `prepare`'s rules, and goes on through the squash. Only
these three commands post a green `xtask/gate`, so green means complete.

The checkout that a command starts from doesn't judge the PR, so it can be
any branch, with any changes. Once the command holds the merge slot and has
fetched, it builds the xtask of the fetched `main` in a worktree of its own,
and runs the sequence again in that xtask, which fetches again. If `main` has
moved by then, it builds again. Before the command lets the slot go to run
that xtask, it takes a second lock, its turn, and keeps it until that xtask
ends. Another merge, or a refresh, that gets the slot during a turn lets it
go, and waits for the turn to end, so the command keeps its place from the slot
to its proof. Nothing that a merge starts inherits the slot or the turn, except
`gh pr merge`, which keeps the slot. The rebase and the proof run in another
worktree, whose path is padded to 160 bytes. That is longer than a worker's
worktree, and shorter than a `#!` line may be, so a check that passes only in a
short path fails here too.

These worktrees, and the tree the bench builds its base in, are kept in the
system's temp directory, or in `/var/tmp` when only that one is on a disk,
because each build's output takes gigabytes. They stay outside the checkout,
because Cargo reads `.cargo/config.toml` from every parent directory. A run
holds a lock on a file beside its tree. `merge` and `prepare` remove each tree
of the repository whose lock no live run holds, which is what a killed run
leaves, in either place.

A PR that changes `xtask/` outside `xtask/tests/` is proven twice. `main`'s
xtask proves it, so that it can't pass by changing its judge. The xtask of its
rebased tree proves it too, through its `prove` command, so that the record says
what `main` will measure after the merge. It must pass both, and its record is
its own xtask's. Merging accepts what either accepts, and the comment marks what
only `main`'s accepts. The second proof keeps the place of the first, unless
the first ran a bench, which gives the place up. A PR that changes a file xtask
reads together with the code that reads it, or how `prove` is called, still has
to be staged: teach the reader first, and change the file in a later PR.

1. `merge` refuses a PR unless its issue has `auto:merge`. All three commands
   refuse a PR that comes from a fork, doesn't close exactly one issue, doesn't
   target `main`, or has changes requested. `merge` and `prepare` refuse a
   thread that doesn't hold the issue's live claim.
   `merge` and `prepare` then fetch `main` and the branch into refs of their
   own, with no tags and no maintenance, so that the fetch takes no lock the
   merge in the slot needs. They compare `main` with the tree that merging the
   branch into it makes, and fail a PR that step 4 would fail before its gate,
   with the words step 4 uses, and post `failure` on the branch's head. The
   xtask that starts a command can be any, so this only saves a wait. `main`'s
   xtask checks again in step 4, and decides.
2. Take the merge slot, and read the PR again. If another run holds the slot,
   the command first says that it waits for it, and posts that on the PR, so a
   queue doesn't look like a hang. If it waited, and then refuses the PR, it
   posts `failure` with why.
3. Fetch `main`, rebase the branch onto it, and drop any ledger record an
   earlier attempt left at the tip.
4. Post `pending` on the branch's current head. Run the gate on the rebased
   head, the journey without `spend` steps, and the bench if a budget can be
   affected. If the PR changes `xtask/` outside `xtask/tests/`, its own xtask
   then runs them too. A lowered count fails `merge`, and passes `prepare`. So
   does a PR that [waits for the owner](#what-waits-for-the-owner), and `merge`
   fails it before the gate runs. A PR with no `Hill: none, <reason>` line fails
   before the gate runs too, if its `Hill:` line gives no reason, or if it
   changes only paths that no count comes from: `AGENTS.md`, `HARNESS.md`,
   `harness/tasks/`, `.github/`, and `xtask/tests/`. An exact revert under
   `prepare` skips the bench, because its base may be what doesn't build. If the
   rebase or this proof fails, the command posts `failure` on the branch's
   current head and pushes nothing.
5. Read the PR again, and stop if it would now be refused or `main` has moved.
   Commit the ledger record, whose `commit` is the gated head and whose `base`
   is the `main` it was tested on, and push with `--force-with-lease`.
6. Verify that the new head differs from the gated head only by that record.
   If the PR waits for the owner, a count fell, or the PR is an exact revert,
   comment on the PR naming what merging accepts. If the PR's auto-merge is
   armed, and the comment names what the owner's approval doesn't cover, as
   [Refresh](#refresh) says, turn auto-merge off. Post `xtask/gate` on the new
   head. `prepare` stops here, and so does `land` if merging would accept
   something, so the owner reads it.
7. Squash-merge with `--match-head-commit`, delete the branch, and release the
   slot.

GitHub can fail a call that it takes a moment later. So once the proof has
passed, each call in steps 5 and 6 that can fail is tried once more, two
seconds later, and a comment that a failed call posted all the same isn't
posted again.

The description of `xtask/gate` says what the merge waits for, what the proof
is doing, and then how it ended. A merge that doesn't wait posts no `Waiting:`
status:

| When | State | Description |
|---|---|---|
| The merge waits for the merge slot | `pending` | `Waiting: the merge slot` |
| The merge waits for a place, or for the activity lock | `pending` | `Waiting: a place`, `Waiting: the activity lock` |
| A wait during the proof ends | `pending` | What the proof is doing, as `Proving: lint` |
| The proof starts | `pending` | `Proving: lint` |
| The tests, T2, the journey, or a bench starts | `pending` | `Proving: tests`, `Proving: t2`, `Proving: journey`, `Proving: bench` |
| The PR's own xtask starts | `pending` | `Proving: its own xtask` |
| The PR is proven | `success` | The counts, as `spec 417 of 812, journey 7 of 19` |
| A check failed | `failure` | The checks that failed, as `Failed: lint, tests` |
| A check of the PR's own xtask failed | `failure` | As `Failed in its own xtask: journey` |
| The branch doesn't rebase, the PR or `main` changed during the proof, or the merge refuses the PR after it posted a wait | `failure` | Why, in its first 140 characters |
| An error stopped the merge after it posted a `pending` | `failure` | `Stopped by an error` |
| An error stopped it after the proof passed | `failure` | `Proven, then stopped by an error` |
| The merge stopped at the merge timeout | `failure` | `Stopped at the timeout` |

The status goes on the head that GitHub shows for the PR: the fetched head
until the push, and the new head after it. So proving a green PR again turns
it yellow until the proof ends. Before the fetch, a wait reads that head from
GitHub, and posts nothing on a PR that merged or closed. The timeout does the
same. In a GitHub Actions run, each status links to the run. A `pending` or
`failure` that can't be posted is reported, and changes nothing else.

A merge past the merge timeout stops, with every command it started, and
releases the slot. So does a merge whose caller stops, as on Ctrl-C. Running it
again starts over from the fetch, and so does running it after a kill at any
step. A merge stopped at the timeout posts `failure` on the head that the PR
shows. One whose caller stops it can leave `pending` until the next proof.
The timeout stops while `main`'s xtask waits to take the slot again, because
the slot is let go as the merge starts that xtask.
The timeout starts again when `main`'s xtask takes the slot, and when a PR's own
xtask starts its proof, so each proof gets all of it. The PR's own xtask can't
start it again. It stops while a proof waits for a place, and while `main`'s
xtask then waits for a shared hold of the activity lock, for its gate or to
build the PR's own xtask. It starts again once the proof has both. It also
stops while the proof's bench waits to hold the activity lock alone, because
other work holds that lock for as long as it runs, and then goes on from where
it stopped. A PR's own xtask can't stop the timeout either. It tells `main`'s
xtask when its bench waits, and the timeout stops only while `main`'s xtask
also sees, by trying the locks, that a bench waits for work that shares the
activity lock. So its bench's wait behind another bench counts, and so does
any wait it only claims.
A `gh pr merge` that is still running keeps the slot until it finishes, so the
next merge can't start on a `main` that is about to move.

A record is trusted only if its `base` is its squash commit's parent. If `main`
moved before the owner merged, the record is skipped as a floor, and the
nightly report lists that commit under "Not in the ledger". Records from before
`base` existed have none, and are trusted.

`xtask/gate` is the only check. Since 2026-11-09, a ruleset on `main` makes
GitHub enforce it too:

- `xtask/gate` must be green, on a branch that is up to date with `main`.
- Only squash merges are allowed.
- Nobody can bypass it, the owner included.
- Auto-merge is allowed.

So GitHub refuses a merge that has no current `xtask/gate`, whatever button is
pressed. The nightly job still reports any commit on `main` with no ledger
record.

### Refresh

A PR that waits for the owner is approved when the owner arms auto-merge on
it, or clicks Merge on it while it is behind `main`, which arms auto-merge.
Nothing proves a waiting PR before that, so until then it shows as behind.
The approval covers the reasons that the PR's newest "Merging this PR at
`<head>` accepts:" comment listed when the owner gave it, and nothing if there
was none. The head in that line doesn't count. A count that fell is covered by
a fall at least as big, on any `main`, and a reason is the same whichever xtask
found it.

`cargo xtask refresh` runs on the owner's Linux machine, in a checkout of its
own at `~/agent-plus-refresh`. That checkout is a worktree of the repository
that the workers' worktrees come from, so the refresh and every worker's merge
take one merge slot. A systemd user timer in `harness/refresh/` starts it every
five minutes, and its service moves the checkout to the newest `main` first,
so the refresh runs the `xtask` of `main`. The owner turns the timer on, as
[Owner actions](#owner-actions) says, and no agent installs or starts it. A
refresh that starts while another runs proves nothing, and exits 0. A refresh
that finds no PR to prove only reads GitHub, so it is cheap to run often.

For each PR it goes on to prove or merge, the refresh takes the merge slot.
If another run holds it, the refresh says that it waits for it, and posts that
on a queued PR. Then it takes the slot's turn, and keeps it until it is done
with the PR. The PR's `prepare` runs in that turn, and takes the slot ahead of
every other merge, as the xtask of `main` does in a merge. The refresh takes
the slot back the same way for the squash. So no merge moves `main` between a
PR's proof and its squash. The refresh lets both go before it proves `main`.

1. Find each open PR whose issue has `land-needed` and whose auto-merge is
   armed, oldest first. Every other PR, and its status, is left alone.
2. Prove each one that is behind `main`, or whose head has no `success` from
   `xtask/gate`. First post `pending` as `xtask/gate` on each, saying how many
   PRs are proven before it: `Queued, next`, `Queued, 1 PR ahead`, or
   `Queued, 2 PRs ahead`.
3. Run `cargo xtask prepare <pr> --keep-budgets` for each as its own process.
   It runs no bench. The new record reads the sizes of the pinned layer and
   kernel from the rebased tree, as every record does, so it follows a pin
   that `main` changed. It keeps the reason and the other budget values of the
   record at the tip of the branch, and `prepare` fails if the tip has none.
4. Once a PR's head has `success`, `refresh` squashes it, if its auto-merge is
   still armed, its head is the one that was proven, it still closes the issue
   this run found it with, and no review requests changes. Any other PR stays
   open. A merge moves `main`, so the PRs after it are checked against the new
   `main`. After the squash, `refresh` removes `land-needed` from the PR and its
   issue, so that it waits for no workflow to do it. Then it closes the issue as
   completed, in case GitHub didn't. It tries each of these steps, even when
   one fails. The PR merged all the same, so its row says `merged`, and
   `refresh` names each label or issue left behind on stderr, and exits 1. The
   next run lists only open PRs, so it can't retry them.
   A PR whose proof in this run accepts what the approval doesn't cover isn't
   merged, and it and its issue keep `land-needed`. GitHub merges an armed PR
   once it is green, so its `prepare` turned auto-merge off before posting
   `success`, as the [merge sequence](#merge-sequence) does. The PR has the new
   comment, and the owner approves it by arming the PR again.
5. Try every PR, and exit 1 if any failed. A failed `prepare` posts `failure`
   and says why. If it ended with neither a `success` nor a `failure` on the
   PR's head, as when it was killed, `refresh` posts `failure` as `Not
   proven`. It posts nothing if it can't read the head's status.
6. A proof that failed only because `main` moved keeps `land-needed`, and the
   next run proves it again. So does a proof that passed and was then stopped,
   as by a GitHub call that failed twice, which `refresh` doesn't take as the
   change's. `prepare` pushes only after the proof passes, so `refresh` takes a
   head that moved during the proof the same way, even when GitHub took no
   status for it. A head that someone else pushed during the proof moved too,
   and nothing proved it. A PR whose head or status `refresh` can't read after
   the proof keeps `land-needed` too. A failure that `prepare` gave in words
   came from the PR and git: the tip has no record, the branch doesn't rebase,
   or what the PR says changed during the proof. `refresh` takes each of those
   as the change's, and proves nothing on `main` for it. After the first
   failure of any other kind, which is a failed check, `Stopped by an error`,
   `Stopped at the timeout`, or `Not proven`, `refresh` runs its own gate on
   the newest `main`, in a tree of its own, once per run.
   That gate gets the merge timeout, which here counts its wait for a place too.
   It is stopped there, with every command it started, and that counts as a
   `main` that fails. It is stopped the same way when the refresh itself stops.
   So a run where every PR is proven pays nothing more. If `main` fails too, the
   fault is the machine's or `main`'s. Then no PR that failed that way in the
   run loses `land-needed` or gets a comment, and `refresh` names what failed
   on stderr and in the run's summary, and exits 1. Otherwise the failure is the
   change's. For a failure that is the change's, `refresh` removes `land-needed`
   from the PR and its issue, so the dispatcher restarts the issue. It tries
   each one, even when one fails, and names each failure. Then it comments on
   the PR with the reason, and says the issue's label is gone only if it is. It
   comments once for each head, so a later run that fails the same head again
   posts nothing. Either way auto-merge stays armed, so the PR merges once a
   refresh proves it, without another approval.

Before each proof, `refresh` prints `refresh: proving PR 5 (1 of 2)`. In
Actions, each proof sits in a `::group::PR 5 (1 of 2): <title>` fold, and the
proof of `main` in its own fold. `refresh` appends a table to
`$GITHUB_STEP_SUMMARY`, with a row per PR: the PR, the verdict (`merged`,
`proven`, or `not proven`), the status description, and how long the proof
took. Under the table, a line for each proven PR that stays open says why.
Elsewhere it prints no fold markers and writes no summary.

Each `prepare` proves its PR with the xtask of the `main` it fetched, as in the
[merge sequence](#merge-sequence), so a PR that changes `xtask/` outside
`xtask/tests/` is proven both ways here too. If `main` moves during a run, the
rest are proven on the newer `main`.

### Runner gate

Some faults show only on a GitHub-hosted runner, such as a uid that isn't
1000, an AppArmor rule, a small socket buffer, or a slower machine. Nothing
else runs our tests there.

So `runner-gate.yml` runs `cargo xtask gate` on a GitHub-hosted runner at each
push to `main`. GitHub-hosted x86_64 runners can run the VMM, once a udev rule
opens KVM and a systemd user session is started. The `runner-setup` action in
`.github/actions/` does both first, and lets the runner's user map ids in a
user namespace. The gate finds such a fault within one merge of the commit
that brought it, and it blocks no PR. The gate on the owner's machines still
decides every merge.

- The job that runs the gate has a token that can only read. It posts no
  `xtask/gate` status, and it merges and labels nothing.
- A newer push replaces a run that is queued, not one that runs.
- The gate gets the merge timeout, because a hang is one of the faults it is
  for. A gate that runs out of time fails. The job has a longer limit, for the
  steps before the gate, and a job that reaches it files no issue.
- When the gate fails there, a second job files one open issue titled
  `` The gate fails on a GitHub runner at `main` ``, with the commit and a link
  to the run. While that issue is open, a later failure adds nothing. That job
  runs no code from the repository, and it is the only one whose token can
  write.

### Nightly job

Runs at the nightly time. The scheduled task runs `cargo xtask nightly`, which
does the following on `main`:

1. Run the bench first, because the remaining tiers are long.
2. Run the gate with T2, the ignored t1 tests that download from a real
   release, T3, the journey, fuzzing, and mutation testing.
3. Post the report to one pinned issue. It leads with what needs the owner:
   a verdict that names each failed check, the three scores, what regressed
   and why, commits not in the ledger, and what waits on the owner. Then come
   each sentence of SPEC.md added, changed, or removed since the last report,
   under its heading and then its PR, and the changes to AGENTS.md and
   HARNESS.md, under each PR, with a line for each file that names the
   headings whose text changed. A sentence with no mark was added, a changed
   one shows what it was and what it is now, and a removed one says so.
   Under one heading, a commit's removed sentences and its added ones pair up
   in order, and each pair is a change. A marked sentence counts, whitespace
   doesn't, and a commit with no record is named by its commit. What merged
   folds below that, leaving out the PRs named above, with both counts in its
   summary, as in `Merged: 41, 17 of them above`. When it leaves out every
   PR, the summary is a line of its own. Last comes a line with the count of
   checks in each state, which names each check that didn't pass. A section
   with nothing in it is left out. A PR or an issue is named as a link whose
   text is its title, at most once under a heading.
4. Post status `xtask/nightly` on the tested commit.
5. If a floor failed, file an issue, bisect from the newest commit with a
   passing `xtask/nightly` to this one, and open a revert PR for the first bad
   commit. The revert is an exact inverse, apart from keeping the reverted
   ledger record, and is exempt from the removed-check floor under `prepare`.
   After commenting on the issue, the job runs `prepare` on the PR, and if that
   fails, comments how to run it. A later night leaves an issue that says how
   it ended alone.

6. Write the [roadmap](#roadmap) and its details, with this run of the
   journey, to the open issues titled `Roadmap` and `Roadmap details`.

The command has one limit for the whole run, and keeps the report's share of
it. A step with a limit of its own stops at that limit, or at the time the run
has left before the report's share, whichever comes first. It ends as it does at
its own limit, and a step with no time left fails.

The command's result is final. If it fails, the agent files an issue describing
the failure, with no `auto:*` label.

### Roadmap

The roadmap is two issues, and each has one reader. Nobody edits either one.
To change the roadmap, change what it is built from.

`cargo xtask roadmap` prints the roadmap for anyone, as markdown, and the
nightly job makes it the body of the open issue titled `Roadmap`. A reader
needs no knowledge of the harness, so it names no parent, step, or count of
promises. It has three parts:

1. A table of the milestones, in the order of `harness/journey.toml`, each
   with its `summary` and its status. A milestone is done when every one of
   its steps passes in the journey that just ran. Only the nightly job runs
   the steps marked `spend`, so only its roadmap can show their milestone as
   done. The first one that isn't
   done is in progress, and the ones after it are planned.
2. Each milestone's commands, each with what the README's Core commands says
   about it. A command is ticked when its milestone is done, so nobody ticks
   its box.
3. What comes after the first release: the README's item for each command
   that SPEC.md marks as planned or proposed, and the open sub-issues of the
   roadmap issue.

`cargo xtask roadmap --details` prints the details for the owner and the
agents, and the nightly job makes them the body of the open issue titled
`Roadmap details`. It files that issue when there is none, and the roadmap
ends with a link to it. The details have four parts:

1. Each milestone, with its commands and its steps as the journey just ran
   them. The milestone's parent is the issue whose body names it in a
   `Milestone:` line, and it is shown with how many of its sub-issues are
   closed. An open parent whose steps all pass is ready to close.
2. What comes after v0.1.0: the commands that SPEC.md marks as planned or
   proposed, and the open sub-issues of the roadmap issue.
3. What waits on the owner: open PRs and issues labeled `land-needed` or
   `answer-needed`, and open issues with no `auto:*` label and no parent in
   this repository. A listed PR stands for the issues it closes, so they are
   left out, and so are the issues that `xtask` writes.
4. Busy and large files: a table of the 10 Rust files that the most commits
   on `main` changed in the 7 days before the commit the details are written
   at, busiest first, and then every other Rust file with 2,000 lines or
   more, largest first. Each row has the file's count of those commits, each
   a PR, its lines, and how many open issues have an entry under `Touches`
   that overlaps it, by the rule the queue uses. A file that `main` no longer
   has isn't listed. The table only reports. The owner decides which file to
   split.

The details show each command with how many of its promises are proven. Its
promises are those under its own heading in SPEC.md, and under each heading
that its entry says specifies it, as in "[Build](#build) specifies `ap build`".
A pointer that names no heading fails the gate's `spec` check.

The same inputs give the same text, and the nightly job leaves a body that
already says it alone. With no open issue titled `Roadmap`, it writes neither
issue, so closing that issue turns the roadmap off.

### Release check

The owner starts this check before a release, through a GitHub workflow with a
`workflow_dispatch` trigger. A fresh GitHub-hosted runner installs the release
binary and runs the journey, which is what the clean-machine requirement in
[Release criteria](#release-criteria) needs. Such an x86_64 runner boots our
guest once the `runner-setup` action readies it, as the
[runner gate](#runner-gate) shows, so the release check uses that action
first. It is built once journey step 18 passes. No scheduled work runs on
GitHub.

## Isolation and limits

| Resource | Rule |
|---|---|
| `ap` state | `HOME` is a directory inside the worktree |
| Build output | One target directory per worktree, deleted when its worker is done: at the merge, or when it parks a prepared PR |
| Ports | Assigned by the OS |
| VMs | Each has a timeout and a recorded PID |
| Builds and VM runs | Shared hold of the activity lock, through `xtask` |
| Test runs and `cargo xtask run` | A place, then a shared hold of the activity lock |
| Bench | Exclusive hold of the activity lock |
| Merge | Exclusive merge slot |
| Refresh | One run at a time, and the merge slot from each PR's proof to its squash |

| Setting | Initial value |
|---|---|
| Worker slots on Linux | 5 |
| Worker slots on a Mac | 1 |
| Measuring slots | 1 |
| Merge slots | 1 |
| Places for test runs | 2 per repository |
| Dispatch period | 10 minutes |
| Refresh period | 5 minutes |
| Planning period | 1 hour |
| Nightly time | 03:00 local |
| Triage time | 06:00 local |
| Nightly limit | 110 minutes, the last 10 kept for the report |
| Fuzzing and mutation limit | 1 hour each, or what the nightly limit leaves |
| Memory for fuzzing, mutation, the gate's tests, and `cargo xtask run` | 8 GiB per job, and none on macOS |
| Lease length | 12 hours |
| Merge timeout | 30 minutes |
| Attempts on one failing check | 3 |
| Time limit on an issue | Twice its estimate |
| Review rounds | 3, or 5 for security, data loss, or concurrency |
| Minimum free disk | 50 GiB |

A job's memory limit is a systemd scope, because `ulimit` counts each process
alone. macOS has no systemd, and nothing there limits a whole process tree, so
`cargo xtask run` and a test run there have no memory limit. A job with a time
limit doesn't start there, so fuzzing and mutation run only on Linux.

The places stay at 2, below the 5 worker slots on Linux, because three test
runs at once failed on timing in 13 of 18 runs on the owner's host. If gates
start to fail on timing, lower the worker slots on Linux to 4.

## Failure handling

| Fault | Detected by | Response |
|---|---|---|
| Session lost or context compacted | Status comment and branch | The next agent resumes from them |
| Host reboot | Expired lease | Dispatch starts a new worker |
| Usage limit | `to11y`, or a harness error | Dispatch starts no worker while the worker's harness is blocked, and a worker skips a blocked reviewer. A worker that hits a limit updates its status and stops. Dispatch retries after the reset |
| GitHub rate limit | API error | Fall back to REST, and wait for the reset |
| No progress | Attempt or time limit | The worker parks the issue |
| PR growth | Review round limit | Extra work becomes new issues |
| Weakened check | Review, the removed-check floor, and mutation testing | The gate fails, or an issue opens |
| Spec and code diverge | Quote mismatch | The gate fails |
| Worker collision | `Touches` overlap, and rebase conflict | Not dispatched together. On conflict, the worker reassesses |
| Broken `main` | Nightly job | Bisect and revert PR |
| A gate failure only a GitHub runner shows | [Runner gate](#runner-gate) | An issue opens |
| Noisy timing | Band and activity lock | Interleaved runs are repeated |
| Disk exhaustion | Free-space check | Dispatch stops |
| Stale VM | VM timeout | The VM exits, and the check fails |

## Owner actions

- Add `auto:*` labels to top-level issues. The triage comment on `Roadmap
  details` says which unlabeled issues are ready, which need an answer, and
  which parents are ready to close.
- Merge each PR that [waits for the owner](#what-waits-for-the-owner) with any
  merge button once `xtask/gate` is green. A PR comment says why it waits.
- Merge `auto:pr` PRs, revert PRs, and any PR that lowers a hill, the same way.
  A PR comment says what merging a lowered count or a revert accepts.
- If the PR is behind `main`, arm auto-merge on it, or click Merge, which
  arms it. That is the approval. The [refresh](#refresh) then proves the PR
  on the newest `main` and merges it. Nothing proves a waiting PR before it.
- Don't use Update branch. It adds a commit with no ledger record at the tip
  and no `xtask/gate` status, so the ruleset blocks the PR, and the next
  refresh turns it red.
- Or run `cargo xtask land <pr>`, which proves the PR and squashes it. It is
  the right tool when no refresh will prove the PR, because its issue isn't
  `land-needed` or its refresh failed, or when a budget needs a new bench. It
  stops at a PR with a comment saying what merging accepts, so the owner reads
  that and uses a merge button. A PR that adds a budget's measurer merges by
  `cargo xtask prepare <pr>` from its own branch instead, as the
  [merge sequence](#merge-sequence) says.
- Turn the scheduled tasks on.
- Turn the [refresh](#refresh) on, once, on the Linux machine whose repository
  the workers' worktrees come from. Replace `<repository>` with the path of
  its main checkout:

  ```sh
  git -C <repository> fetch origin main
  git -C <repository> worktree add --detach ~/agent-plus-refresh origin/main
  cd ~/agent-plus-refresh/harness/refresh
  systemctl --user link "$PWD/agent-plus-refresh.service"
  systemctl --user enable --now "$PWD/agent-plus-refresh.timer"
  ```

  It merges with your `gh` sign-in, and commits its records as your git user.
  The service names `git` and `cargo` with no directory, so systemd looks for
  them in `/usr/local/bin` and `/usr/bin`. The refresh finds `gh` on the path
  that `systemctl --user show-environment` prints.
  It runs while your user's systemd does, so `loginctl enable-linger` keeps it
  running when you are signed out. After a change to its units on `main`, run
  `systemctl --user daemon-reload`.
- Set up the [paid checks](#check-tiers): create the Claude Console workspace
  and its monthly spend limit, a service account scoped to it, and a key with
  an expiry. Do the same with an OpenAI project. Put the keys in
  `~/.config/agent-plus/paid-checks/anthropic-api-key` and
  `~/.config/agent-plus/paid-checks/openai-api-key` on the machine that runs
  the nightly job, readable only by you. Never commit or paste them.
- Answer `answer-needed` issues. The owner can set or clear `land-needed` and
  `answer-needed` on the issue or on the open PR that closes it. A workflow
  runs `cargo xtask mirror` to copy the change to the other side, and only the
  issue's label decides whether it can be claimed. When the PR merges, the
  same workflow removes `land-needed` from both.
- Publish releases.

## Build stages

The harness is built before product features.

| Stage | Delivers | Acceptance |
|---|---|---|
| H0 | Workspace, `xtask gate`, T0, and floors | Passes on an empty workspace, and fails on a planted C dependency |
| H1 | Spec score, quotes, and waivers | The score prints, and editing a quoted sentence fails the gate |
| H2 | Fake harness, fake VM, and lane seams | One promise is proven through the real parser |
| H3 | Activity lock, journey runner, ledger, and bench | Step 1 passes and writes a record, and a planted slowdown fails |
| H4 | `xtask next`, `xtask claim`, merge slot, and the dispatcher and planner tasks | A planted issue merges unattended, even when its worker is killed midway |
| H5 | `xtask nightly`, and the nightly task | A planted break on `main` is bisected, and a revert PR opens |

## Open decisions

| Decision | Default until decided |
|---|---|
| Whether the nightly job may merge a pure revert | It opens the PR, and the owner merges |
| Limit on model spend for workers | Workers run no step marked `spend` |
| The binary size target, and bands for every budget except boot, idle overhead, a build with nothing changed, `ap --version`, and the binary | Those budgets are recorded, and don't fail |
| Whether agents get their own GitHub account. Everyone acts as the owner's account today, so no program can tell an agent from the owner | The guards stop mistakes, not an agent that ignores its rules |
| Running workers inside `ap` agents once step 12 passes, which needs nested KVM | Workers run on the host |
