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
owner -> labeled parent issue
planner: score gaps -> child issues
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
`cargo xtask merge` requires and the reviewer confirms.

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
and any step that needs it, out of the run and the score. The nightly report
counts them. A step that names a `floor` fails while that floor's check doesn't
exist.

`cargo xtask journey` prints each step as `pass`, `fail`, or `skip`, then the
count out of all steps. It exits 0 whenever it could run, because what fails a
merge is a count lower than the base's. Each step gets a new `HOME` and working
directory, and only `HOME` and `PATH` in its environment.

| # | Step | Needs |
|---|---|---|
| 1 | `ap --version` runs from a release build | |
| 2 | `ap fmt` and parse errors work on a real Agentfile | 1 |
| 3 | The VMM boots a kernel to guest init | 1 |
| 4 | A command runs in the guest and returns its exit code | 3 |
| 5 | `ap build` writes layers, and a rebuild with no changes is a hit | 2 |
| 6 | The first build lists grants, and a wider grant asks again | 5 |
| 7 | `ap run` starts the fake harness in the guest | 4, 5 |
| 8 | Claude Code and Codex start in the guest with no credentials | 7 |
| 9 | A share is copy-on-write, and `ap diff` and `ap apply` work | 7 |
| 10 | `NETWORK` rules hold, and a closed network stays closed | 7 |
| 11 | A hidden secret reaches only its host, and the agent can't read it | 10 |
| 12 | A real headless prompt edits a share and lands on a branch | 8, 9, 11 |
| 13 | The same Agentfile works with one line changed to the other harness | 12 |
| 14 | A plugin, skill, and MCP server install and translate | 8 |
| 15 | `AUTH login` and `PERSIST` survive a restart | 8 |
| 16 | The desktop starts on demand, and computer-use tools work | 7 |
| 17 | The control API, ACP bridge, and agents calling agents work | 8 |
| 18 | Every other v0.1.0 host passes the steps above | all |
| 19 | Installed from a release, the README's quick start works as written | 18 |

### Budgets

**Score:** budgets met, out of the budgets with a target.

| Budget | Target | Band |
|---|---|---|
| Boot to guest init | under 150 ms | 10% or 5 ms, whichever is larger |
| `ap run` to a ready harness, with layers cached | under 1 s | to be set |
| Idle overhead of `ap`, the VMM, and the guest kernel | under 64 MiB | to be set |
| `ap build` with nothing changed | under 100 ms | to be set |
| Desktop cost until first use | disk only | none |
| Stripped release binary | to be set | to be set |
| `ap --version` | none | 20% or 2 ms, whichever is larger |

`harness/budgets.toml` holds the targets and bands. A budget with no band is
recorded and can't fail.

The bench compares the medians of 15 interleaved pairs of base and candidate
runs. A change that worsens a budget by more than its band fails, unless its
ledger record has a reason, taken from a `Budget reason: <text>` line in the
PR body. The reviewer confirms the reason. The nightly report lists every
reason given that day.

Boot time runs from spawning `ap boot` until its ready line appears, by the
host's clock. Each build gets one warm-up run first, and all runs share one
`HOME`, so no download is timed. A base with no `ap boot` is reported as no
comparison, and the candidate's value is still recorded.

## Floors

A failing floor blocks a merge regardless of scores.

| Floor | Verified by |
|---|---|
| No C in the dependency tree, and no build scripts in our crates | `cargo metadata` and `cargo tree` |
| A hidden secret can't be read from the guest | `floor_hidden_secret`, needed by step 11 |
| A closed network stays closed | `floor_closed_network`, needed by step 10 |
| A share can't reach outside its root | `floor_share_root`, needed by step 9 |
| The guest can't run anything on the host | `floor_guest_host`, needed by step 4 |
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
floor passes the gate and fails a release. Escape checks are tests in T1, and
each journey step above can't pass until its check exists.

## Check tiers

| Tier | Runs | Trigger | Model spend |
|---|---|---|---|
| T0 | Format, lint, unit tests, spec quotes, and floors | Every gate | No |
| T1 | The real `ap` and a real microVM, with the fake harness | Every gate | No |
| T2 | Real Claude Code and Codex in the guest, with no credentials | Adapter changes, and nightly | No |
| T3 | A real model turn, on the cheapest model | Nightly, and before a release | Yes |
| Bench | Budgets, against the base build | Changes that can affect a budget, and nightly | No |

T1 and T2 checks live in the packages `t1` and `t2`, so T0 leaves them out. A
change can affect a budget when its diff touches `crates/`, `.cargo/`,
`guest/kernel/`, `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`,
`harness/budgets.toml`, or `xtask/src/bench.rs`.

**Fake harness.** A guest program that implements the adapter contract and
records its arguments, environment, files, and network view.

**Harness probe (T2).** The harness starts with no credentials, and the check
reads what it reports before sign-in. For Claude Code, that is the `init` event
of `claude -p <prompt> --output-format stream-json --verbose`, which lists the
working directory, MCP servers, skills, and commands. The Codex probe is
undefined (see [Open decisions](#open-decisions)).

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

**Fuzzing.** The nightly job fuzzes the Agentfile parser, network rule
matching, and device input from the guest.

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
| `SPEC.md` | Changed only by `spec-change` PRs |
| `harness/ledger/` | One new file per merge |
| `harness/waived.txt` | Changed only by `spec-change` PRs, with union merge through `.gitattributes` |
| `harness/journey/` | Step scripts, which any PR may change |
| `xtask/`, `.github/`, and the rest of `harness/` | Changed only by `spec-change` PRs, so a PR can't weaken its own gate |
| `Cargo.lock` | Regenerated on rebase. A new dependency gets its own PR |

The gate fails when a diff touches one of these `spec-change` paths and its PR
doesn't have the label. That includes a typo, because a program can't tell a
typo from a change of meaning. A `spec-change` PR may also edit the `// spec(`
lines and waivers that quote the changed text. A branch with no open PR yet
prints `skip labels`, because `cargo xtask merge` gates it again with the PR
open. `cargo xtask merge` refuses a `spec-change` PR, and `cargo xtask prepare`
readies it for the owner.

## Data formats

### Journey definition

```toml
[[milestone]]
name = "Boot"
commands = ["ap --version", "ap boot"]

[[step]]
id = 3
name = "The VMM boots a kernel to guest init"
milestone = "Boot"
needs = [1]
script = "harness/journey/03-boot.sh"
```

A step can also set `floor = "floor_guest_host"`, or `spend = true`.

The `[[milestone]]` tables are in roadmap order. `commands` lists the `ap`
subcommands that the milestone delivers, and it can be empty. Every step names
one milestone, every milestone has a step, and no command is listed twice. A
file that breaks one of these is an error.

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
| `budgets` | Every budget's latest value. One this change didn't measure carries over from the previous record |

A change's base scores are those of the newest record in the history of its
base, or zero if there is none.

### Issue and status comment

The issue body and status comment follow [AGENTS.md](AGENTS.md#issues). `xtask`
reads the `Touches`, `Blocked by`, and `Part of` lines, and the `State` and
`Lease until` fields.

- The paths in `Touches` are its backticked words. Two issues overlap when one
  path is a prefix of the other.
- An issue with no `## Touches` isn't claimable, and neither is a parent: an
  issue that another issue names in `Part of`.
- A parent that delivers a milestone names it on a line of its own,
  `Milestone: <name>.`. The planner reads that line, and `xtask` doesn't.
- `xtask` reads status comments only from the repository owner's account.
- When two live claims exist, the status comment with the lowest id wins.

## Commands

Every command lives in one `xtask` crate, in Rust. Exit status 0 is a pass, 1
is a failure, and 2 is bad usage or an environment error.

| Command | Behavior |
|---|---|
| `cargo xtask gate` | Runs T0, T1, and the floors, compares counts with the base, and prints the result. It posts no status |
| `cargo xtask score` | Prints the three scores |
| `cargo xtask gaps` | Names the first milestone on the roadmap with a step that doesn't pass. Then lists failing steps in journey order, each with its milestone, then missed budgets, then unproven promises in SPEC order |
| `cargo xtask journey` | Runs the journey against a release build |
| `cargo xtask bench` | Takes the measuring slot, and compares with the base |
| `cargo xtask next` | Prints claimable issues as JSON |
| `cargo xtask claim <issue> [--thread <link>]` | Claims an issue for the current branch, prints the status comment id as JSON, and exits 1 if another claim wins |
| `cargo xtask record --issue <n> --pr <n>` | Writes the ledger record for the current head |
| `cargo xtask merge <pr>` | Runs the [merge sequence](#merge-sequence) |
| `cargo xtask prepare <pr> [--keep-budgets]` | Runs the [merge sequence](#merge-sequence) up to the squash, under the rules for the owner |
| `cargo xtask refresh` | Runs the [refresh](#refresh) of `land-needed` PRs that are behind `main` |
| `cargo xtask land <pr>` | Runs the [merge sequence](#merge-sequence) through the squash, under the rules for the owner |
| `cargo xtask run <command>` | Runs a build or test command under a shared hold of the activity lock and a memory limit |
| `cargo xtask nightly` | Runs the [nightly sequence](#nightly-job) |
| `cargo xtask report` | Builds the nightly report from the ledger |

`cargo xtask next` prints an array of
`{"issue", "touches", "autonomy", "branch", "worktree"}`, lowest issue number
first, with
at most one entry per free worker slot. When two issues overlap, the lower
number stays. Any issue that overlaps a live claim is left out, and an abandoned
issue is included. `branch` names the existing branch of an abandoned issue, and
`worktree` names where that branch is still checked out. Both are otherwise
null.

`score`, `gaps`, and `report` take `--json`. Results go to stdout, and
diagnostics go to stderr.

## Issue states

| State | Condition | Next |
|---|---|---|
| Unavailable | No `auto:*` label | Available, when the owner labels it |
| Blocked | An issue in `Blocked by` is open | Available, when it closes |
| Available | Claimable under AGENTS.md | Claimed |
| Claimed | `in-progress`, with a live lease | Parked, Abandoned, or Closed |
| Abandoned | `in-progress`, with an expired lease and no commits since | Claimed |
| Parked | `land-needed` or `answer-needed` | Available, when the owner removes the label |
| Closed | Its PR merged | |

## Processes

### Scheduling

The dispatcher, planner, and nightly job are T3 Code scheduled tasks. Each is
bound to its own thread, whose settings give it its model and full access,
because no one is there to answer a prompt. Each run starts a fresh session, so
no run depends on the context of an earlier one. A run moves its checkout to
`origin/main`, then follows its brief in `harness/tasks/`.

### Dispatch

Runs every dispatch period.

1. Stop if free disk is below the minimum.
2. Run `cargo xtask next`.
3. For each entry, start one worker with a brief naming the issue: in its
   `worktree` if it has one, otherwise in a new worktree on its `branch`, or
   from `origin/main` if it has neither.

Dispatch is idempotent, because all state is on GitHub. A worker that loses a
claim race exits.

### Planning

Runs every planning period, and stops at once if the issues in progress and
the issues `cargo xtask next` lists already fill the worker slots. The planner
reads `cargo xtask gaps`, and files child issues under a parent. Each child
inherits its parent's `auto:*` label.

The planner works on one milestone at a time: the first on the roadmap with a
step that doesn't pass, which `cargo xtask gaps` names. Its parent is the open
issue that the owner has labeled, and whose body names that milestone in a
`Milestone:` line. With no such issue, the planner says which milestone has no
parent, and stops. A parent whose milestone passes is never chosen, so leaving
it open holds nothing up.

The order of the milestones in `harness/journey.toml` is the order of the work.

### Worker

1. Claim the issue.
2. Read the issue, the sentences it names, and the base scores.
3. Write the check, and record it failing.
4. Make the smallest change that passes.
5. Run the gate, and the bench if a budget can be affected.
6. If a floor failed, or no count rose and the PR has no `Hill: none` line,
   revert and try again, within the attempt limit. Past the limit, park the
   issue.
7. Get a review, within the round limit.
8. Open the PR, and wait for checks and reviews.
9. Merge if the label is `auto:merge`. Otherwise park with the PR ready.

Model selection escalates in a fixed order: improve the brief, raise the
effort, then change the model. Reviewers use a different provider from the
worker when one is available.

### Merge sequence

Only one merge runs at a time, under an exclusive lock on a file in the shared
git directory. The kernel releases the lock when the process exits, so a killed
merge can't keep the slot.

`cargo xtask merge` runs every step. `cargo xtask prepare` stops before the
squash, and leaves the owner a complete PR to merge with any button. `cargo
xtask land` follows `prepare`'s rules, and goes on through the squash. Only
these three commands post a green `xtask/gate`, so green means complete.

`land` takes the merge slot first, and then makes sure the newest checks run,
because xtask compiles from the checkout. If `main` is behind `origin/main`, it
fast-forwards and starts again through `cargo`. It refuses local commits,
changes to tracked files, and any other branch that isn't at `origin/main`.

1. `merge` refuses a PR unless its issue has `auto:merge`, and refuses a
   `spec-change` PR. All three commands refuse a PR that comes from a fork,
   doesn't close exactly one issue, doesn't target `main`, or has changes
   requested.
2. Take the merge slot, and read the PR again.
3. Fetch `main`, rebase the branch onto it, and drop any ledger record an
   earlier attempt left at the tip.
4. Run the gate on the rebased head, the journey without `spend` steps, and the
   bench if a budget can be affected. A lowered count fails `merge`, and passes
   `prepare`. An exact revert under `prepare` skips the bench, because its base
   may be what doesn't build. If the rebase or this proof fails, `prepare`
   posts `failure` on the branch's current head and pushes nothing.
5. Read the PR again, and stop if it would now be refused or `main` has moved.
   Commit the ledger record, whose `commit` is the gated head and whose `base`
   is the `main` it was tested on, and push with `--force-with-lease`.
6. Verify that the new head differs from the gated head only by that record.
   If a count fell or the PR is an exact revert, comment on the PR naming what
   merging accepts. Post `xtask/gate` on the new head. `prepare` stops here,
   and so does `land` if merging would accept something, so the owner reads it.
7. Squash-merge with `--match-head-commit`, delete the branch, and release the
   slot.

A merge past the merge timeout stops and releases the slot. Running it again
starts over from the fetch, and so does running it after a kill at any step.
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

A workflow runs `cargo xtask refresh` on every push to `main`, one run at a
time, with `xtask` built from `main`. GitHub-hosted x86_64 runners can run the
VMM, once a udev rule opens KVM and a systemd user session is started.

1. Find each open PR whose issue has `land-needed` and that is behind `main`,
   oldest first. A PR that isn't behind is left alone.
2. Post `pending` as `xtask/gate` on each.
3. Run `cargo xtask prepare <pr> --keep-budgets` for each as its own process.
   It runs no bench, because a shared runner can't measure a budget. The new
   record keeps the budget values and reason of the record at the tip of the
   branch, and `prepare` fails if the tip has none.
4. Try every PR. A PR that fails gets `failure` and stays `land-needed`, and
   the command exits 1.

If `main` moves during a run, `prepare --keep-budgets` refuses the rest,
because `xtask` was built from the older `main`. Those PRs go red until the
run that the new push started proves them again.

### Nightly job

Runs at the nightly time. The scheduled task runs `cargo xtask nightly`, which
does the following on `main`:

1. Run the bench first, because the remaining tiers are long.
2. Run the gate, T2, T3, the journey, fuzzing, and mutation testing.
3. Post the report to one pinned issue: the three scores, what merged, what
   regressed and why, and what waits on the owner.
4. Post status `xtask/nightly` on the tested commit.
5. If a floor failed, file an issue, bisect from the newest commit with a
   passing `xtask/nightly` to this one, and open a revert PR for the first bad
   commit. The revert is an exact inverse, apart from keeping the reverted
   ledger record, and is exempt from the removed-check floor under `prepare`.
   After commenting on the issue, the job runs `prepare` on the PR, and if that
   fails, comments how to run it. A later night leaves an issue that says how
   it ended alone.

The command's result is final. If it fails, the agent files an issue describing
the failure, with no `auto:*` label.

### Release check

The owner starts this check before a release, through a GitHub workflow with a
`workflow_dispatch` trigger. A fresh GitHub-hosted runner installs the release
binary and runs the journey, which is what the clean-machine requirement in
[Release criteria](#release-criteria) needs. It is built once journey step 18
passes. No scheduled work runs on GitHub.

## Isolation and limits

| Resource | Rule |
|---|---|
| `ap` state | `HOME` is a directory inside the worktree |
| Build output | One target directory per worktree, deleted at merge |
| Ports | Assigned by the OS |
| VMs | Each has a timeout and a recorded PID |
| Builds and VM runs | Shared hold of the activity lock, through `xtask` |
| Bench | Exclusive hold of the activity lock |
| Merge | Exclusive merge slot |
| GitHub API | One watcher per PR |

| Setting | Initial value |
|---|---|
| Worker slots | 3 |
| Measuring slots | 1 |
| Merge slots | 1 |
| Dispatch period | 30 minutes |
| Planning period | 1 hour |
| Nightly time | 03:00 local |
| Fuzzing and mutation limit | 1 hour each |
| Memory for fuzzing, mutation, the gate's tests, and `cargo xtask run` | 8 GiB per job |
| Lease length | 12 hours |
| Merge timeout | 30 minutes |
| Attempts on one failing check | 3 |
| Time limit on an issue | Twice its estimate |
| Review rounds | 3, or 5 for security, data loss, or concurrency |
| Minimum free disk | 50 GiB |

## Failure handling

| Fault | Detected by | Response |
|---|---|---|
| Session lost or context compacted | Status comment and branch | The next agent resumes from them |
| Host reboot | Expired lease | Dispatch starts a new worker |
| Usage limit | Harness error | The worker updates its status and stops. Dispatch retries after the reset |
| GitHub rate limit | API error | Fall back to REST, and wait for the reset |
| No progress | Attempt or time limit | The worker parks the issue |
| PR growth | Review round limit | Extra work becomes new issues |
| Weakened check | Review, the removed-check floor, and mutation testing | The gate fails, or an issue opens |
| Spec and code diverge | Quote mismatch | The gate fails |
| Worker collision | `Touches` overlap, and rebase conflict | Not dispatched together. On conflict, the worker reassesses |
| Broken `main` | Nightly job | Bisect and revert PR |
| Noisy timing | Band and activity lock | Interleaved runs are repeated |
| Disk exhaustion | Free-space check | Dispatch stops |
| Stale VM | VM timeout | The VM exits, and the check fails |

## Owner actions

- Add `auto:*` labels to top-level issues.
- Merge `spec-change` PRs, which cover SPEC.md, `xtask/`, `.github/`, and
  `harness/`, with any merge button once `xtask/gate` is green.
- Merge `auto:pr` PRs, revert PRs, and any PR that lowers a hill, the same way.
  A PR comment says what merging a lowered count or a revert accepts.
- If the PR is behind `main`, wait for the [refresh](#refresh) to prove it
  again. A Merge click on a PR that is behind can arm auto-merge instead. The
  request survives the refresh's force-push, and the PR merges once its new
  `xtask/gate` is green.
- Don't use Update branch. It adds a commit with no ledger record at the tip
  and no `xtask/gate` status, so the ruleset blocks the PR, and the next
  refresh turns it red.
- Or run `cargo xtask land <pr>`, which proves the PR and squashes it. It is
  the right tool when no refresh will prove the PR, because its issue isn't
  `land-needed` or its refresh failed, or when a budget needs a new bench. It
  stops at a PR with a comment saying what merging accepts, so the owner reads
  that and uses a merge button.
- Turn the scheduled tasks on.
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
| Hosts for macOS, and a merge slot that spans machines. GitHub-hosted macOS runners don't support nested virtualization, so macOS needs a real host | Those lanes don't run |
| Limit on model spend for workers and T3 | No unattended T3 runs |
| Codex probe for T2 | T2 covers Claude Code only |
| Whether GitHub-hosted Linux runners can run the VMM for the release check. Their documented KVM support is for Android, and it is untested here | The owner runs the release check by hand on a clean machine |
| Bands for every budget except boot and `ap --version`, and the binary size target | Those budgets are recorded, and don't fail |
| Whether agents get their own GitHub account. Everyone acts as the owner's account today, so no program can tell an agent from the owner | The guards stop mistakes, not an agent that ignores its rules |
| Running workers inside `ap` agents once step 12 passes, which needs nested KVM | Workers run on the host |
