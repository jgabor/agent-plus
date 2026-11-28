use super::prepare::{last_post, posts};
use super::waits::changing;
use super::*;

/// The program on `PATH`, not a shell builtin of the same name.
fn external(program: &str) -> String {
    let path = env::var("PATH").expect("PATH is set");
    env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|file| file.is_file())
        .unwrap_or_else(|| panic!("no `{program}` on PATH"))
        .display()
        .to_string()
}

/// The timeout of a merge whose proof stands in for cargo's work. That work
/// takes about 2 seconds a proof. Beside a run of these tests, it took at most
/// 4.2 in 20 runs, and 12.2 once, when the machine was swapping.
const TIMEOUT: u64 = 45;

/// A wait that outlasts the timeout.
const PAST_THE_TIMEOUT: Duration = Duration::from_secs(TIMEOUT + 2);

/// A merge of the PR with `TIMEOUT`, whose proof stands in for cargo's work,
/// as `STAND_IN_PROOF` in the tools says.
fn timed_merge(repo: &Repo) -> Command {
    let mut command = repo.command(env!("CARGO_BIN_EXE_xtask"));
    command
        .args(["merge", "9"])
        .env("STAND_IN_PROOF", "1")
        .env("XTASK_MERGE_TIMEOUT_SECONDS", TIMEOUT.to_string());
    command
}

#[test]
fn a_merge_past_its_timeout_stops_and_can_restart() {
    let repo = Repo::new("timeout");
    // On a loaded machine, whatever stops the merge at its timeout can be slow
    // to go on after the kill lands. This `kill` is that slow, once. It runs
    // the real one, because dash's builtin refuses `--`.
    let bin = repo.bin.display();
    let kill = external("kill");
    common::write_tool(
        &repo.bin.join("kill"),
        &format!(
            "#!/bin/sh\n'{kill}' \"$@\"\ncode=$?\nif [ \"$1\" = -KILL ] && [ ! -f '{bin}/slowed' ]; then : > '{bin}/slowed'; sleep 2; fi\nexit $code\n"
        ),
    );
    // The hang outlasts the timeout by far, so the merge stops at the timeout
    // wherever it is when that fires.
    let output = repo.output(
        repo.command(env!("CARGO_BIN_EXE_xtask"))
            .args(["merge", "9"])
            .env("HANG_ON", "cargo fmt")
            .env("HANG_FOR", "60")
            .env("XTASK_MERGE_TIMEOUT_SECONDS", "1"),
    );
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(stderr(&output).contains("timeout"), "{}", stderr(&output));
    assert!(!repo.bin.join("merged").exists());
    assert_eq!(
        posts(&repo).pop(),
        Some((repo.head(), "failure Stopped at the timeout".to_string()))
    );
    assert_merged(&repo, &repo.merge());
}

/// The supervisor says that it stopped the merge even when GitHub takes no
/// status, and the merge ends as it would have.
#[test]
fn a_timeout_that_can_not_be_posted_is_still_a_timeout() {
    let repo = Repo::new("timeout-unposted");
    fs::write(repo.bin.join("status-fails"), "").unwrap();
    let output = repo.output(
        repo.command(env!("CARGO_BIN_EXE_xtask"))
            .args(["merge", "9"])
            .env("HANG_ON", "cargo fmt")
            .env("HANG_FOR", "60")
            .env("XTASK_MERGE_TIMEOUT_SECONDS", "1"),
    );
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    let said = stderr(&output);
    let (_, after) = said.split_once("second timeout").expect(&said);
    assert!(after.contains("couldn't post `xtask/gate`"), "{said}");
    assert!(!repo.bin.join("merged").exists());
}

/// A PR's code runs in its builds and tests, whose input mustn't reach the
/// supervisor. Every cargo call tries to pause the timeout, so one that could
/// would stop it until the hang ends. The hang is at the proof's last cargo
/// call, and outlasts the timeout by far.
#[test]
fn a_command_the_proof_runs_cannot_pause_the_timeout() {
    let repo = Repo::new("timeout-paused");
    let output = repo.output(
        timed_merge(&repo)
            .env("PAUSE_ON", "cargo ")
            .env("HANG_ON", "cargo build --release")
            .env("HANG_FOR", "120"),
    );
    assert!(
        repo.calls().contains("cargo build --release"),
        "{}",
        repo.calls()
    );
    let paused = fs::read_to_string(repo.bin.join("paused")).unwrap_or_default();
    assert!(paused.is_empty(), "{paused}");
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(stderr(&output).contains("timeout"), "{}", stderr(&output));
}

/// A PR that changes xtask is proven twice, and each proof gets the whole
/// timeout, so the two together may take longer. The second hang ends a
/// second past the timeout after the first one starts, and the merge reports
/// that it holds the slot well before that, so a timeout that both proofs
/// shared would stop the merge. The first hang takes half the timeout, which
/// leaves each proof about half of it for its other work.
#[test]
fn each_proof_of_a_pr_that_changes_xtask_gets_the_timeout() {
    let repo = changing(
        "timeout-twice",
        &[],
        &[("xtask/src/bench.rs", "// a better measurer\n")],
    );
    let output = repo.output(
        timed_merge(&repo)
            .env("HANG_ON", "cargo fmt --all")
            .env("HANG_FOR", (TIMEOUT / 2).to_string())
            .env("HANG_UNTIL", (TIMEOUT + 1).to_string()),
    );
    assert_eq!(
        repo.calls().matches("cargo fmt --all").count(),
        2,
        "{}\nstdout: {}\nstderr: {}",
        repo.calls(),
        stdout(&output),
        stderr(&output)
    );
    assert_merged(&repo, &output);
}

/// The timeout starts again once the merge has a place, so a merge that waits
/// for one longer than the timeout still merges.
#[test]
fn the_wait_for_a_place_does_not_count_against_the_timeout() {
    let repo = Repo::new("place-wait");
    let directory = repo.root.join(".git/xtask");
    fs::create_dir_all(&directory).unwrap();
    let place = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(directory.join("place-0.lock"))
        .unwrap();
    place.lock().unwrap();
    let mut merge = timed_merge(&repo)
        .env("XTASK_PLACES", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    thread::sleep(PAST_THE_TIMEOUT);
    if merge.try_wait().unwrap().is_some() {
        let output = merge.wait_with_output().unwrap();
        panic!(
            "the merge didn't wait for the place:\n{}\n{}",
            stdout(&output),
            stderr(&output)
        );
    }
    assert_eq!(last_post(&repo), "pending Waiting: a place");
    place.unlock().unwrap();
    assert_merged(&repo, &merge.wait_with_output().unwrap());
    let said: Vec<String> = posts(&repo).into_iter().map(|(_, said)| said).collect();
    assert_eq!(
        said,
        [
            "pending Proving: lint",
            "pending Waiting: a place",
            "pending Proving: lint",
            "pending Proving: tests",
            "pending Proving: journey",
            "success spec 0 of 2, journey 1 of 1",
        ]
    );
}

/// The proof waits for the activity lock once it has its place, as it does
/// behind a pending bench, and that wait doesn't count against the timeout
/// either. The test keeps the one place until the merge waits for it, which
/// the merge shows by keeping `ahead.lock`. Then it keeps the turnstile, as a
/// bench would, frees the place, and waits until the merge has taken it.
#[test]
fn the_proofs_wait_for_the_activity_lock_does_not_count_against_the_timeout() {
    let repo = Repo::new("activity-wait");
    let directory = repo.root.join(".git/xtask");
    fs::create_dir_all(&directory).unwrap();
    let lock = |name: &str| {
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(directory.join(name))
            .unwrap()
    };
    let place = lock("place-0.lock");
    place.lock().unwrap();
    // To files, because nothing reads a pipe while this waits.
    let said = |name: &str| fs::File::create(repo.temp.join(name)).unwrap();
    let mut merge = timed_merge(&repo)
        .env("XTASK_PLACES", "1")
        .stdout(said("merge.out"))
        .stderr(said("merge.err"))
        .spawn()
        .unwrap();
    let ahead = directory.join("ahead.lock");
    common::wait_until("the merge never waited for a place", || {
        common::locked(&ahead)
    });
    let turnstile = lock("turnstile.lock");
    turnstile.lock().unwrap();
    place.unlock().unwrap();
    common::wait_until("the merge never took the place", || !common::locked(&ahead));
    // The wait alone now outlasts the timeout.
    thread::sleep(PAST_THE_TIMEOUT);
    if let Some(status) = merge.try_wait().unwrap() {
        panic!(
            "the merge didn't wait for the activity lock: {status}\n{}\n{}",
            fs::read_to_string(repo.temp.join("merge.out")).unwrap(),
            fs::read_to_string(repo.temp.join("merge.err")).unwrap()
        );
    }
    assert_eq!(last_post(&repo), "pending Waiting: the activity lock");
    turnstile.unlock().unwrap();
    let status = merge.wait().unwrap();
    let output = Output {
        status,
        stdout: fs::read(repo.temp.join("merge.out")).unwrap(),
        stderr: fs::read(repo.temp.join("merge.err")).unwrap(),
    };
    assert_merged(&repo, &output);
    let said: Vec<String> = posts(&repo).into_iter().map(|(_, said)| said).collect();
    assert_eq!(
        said[..6],
        [
            "pending Proving: lint",
            "pending Waiting: a place",
            "pending Proving: lint",
            "pending Waiting: the activity lock",
            "pending Proving: lint",
            "pending Proving: tests",
        ]
    );
}

/// The proof of a PR's own xtask builds it under a shared hold of the activity
/// lock, and that wait doesn't count against the timeout either. The journey
/// step of `main`'s proof waits at a FIFO, so the test knows that proof holds
/// the lock. The test keeps the turnstile, as a bench does, lets the step end,
/// and takes the lock once `main`'s proof lets it go.
#[test]
fn the_build_of_its_own_xtask_waits_for_the_activity_lock_off_the_timeout() {
    let name = "own-build-wait";
    let bin = tmp().join("merge").join(name).join("bin");
    let (ready, go, once) = (bin.join("ready"), bin.join("go"), bin.join("once"));
    let step = format!(
        "#!/bin/sh\nif [ ! -e '{once}' ]; then : > '{once}'; echo > '{ready}'; read _ < '{go}'; fi\nexit 0\n",
        once = once.display(),
        ready = ready.display(),
        go = go.display()
    );
    let repo = changing(
        name,
        &[("harness/journey/one.sh", &step)],
        &[("xtask/src/limit.rs", "// a tighter limit\n")],
    );
    common::fifo(&ready);
    common::fifo(&go);
    // To files, because nothing reads a pipe while this waits.
    let said = |name: &str| fs::File::create(repo.temp.join(name)).unwrap();
    let mut merge = timed_merge(&repo)
        .stdout(said("merge.out"))
        .stderr(said("merge.err"))
        .spawn()
        .unwrap();
    common::meet(&ready, false);
    let lock = |name: &str| {
        fs::OpenOptions::new()
            .write(true)
            .open(repo.root.join(".git/xtask").join(name))
            .unwrap()
    };
    let turnstile = lock("turnstile.lock");
    turnstile.lock().unwrap();
    common::meet(&go, true);
    let activity = lock("activity.lock");
    activity.lock().unwrap();
    // The wait alone now outlasts the timeout.
    thread::sleep(PAST_THE_TIMEOUT);
    if let Some(status) = merge.try_wait().unwrap() {
        panic!(
            "the merge didn't wait for the activity lock: {status}\n{}\n{}",
            fs::read_to_string(repo.temp.join("merge.out")).unwrap(),
            fs::read_to_string(repo.temp.join("merge.err")).unwrap()
        );
    }
    drop((activity, turnstile));
    let status = merge.wait().unwrap();
    let output = Output {
        status,
        stdout: fs::read(repo.temp.join("merge.out")).unwrap(),
        stderr: fs::read(repo.temp.join("merge.err")).unwrap(),
    };
    assert_merged(&repo, &output);
    assert_eq!(repo.calls().matches("cargo fmt --all").count(), 2);
}

/// A PR that changes xtask is proven twice. With no bench between them, the
/// proof of its own xtask keeps the place of the proof before it, so a run
/// that waits for a place goes only after both. The run starts to wait in the
/// first proof's format check, which waits at a FIFO until it does.
#[test]
fn the_proof_of_its_own_xtask_keeps_the_place_of_the_proof_before_it() {
    let repo = changing(
        "own-keeps-place",
        &[],
        &[("xtask/src/limit.rs", "// a tighter limit\n")],
    );
    let (ready, go) = (repo.temp.join("ready"), repo.temp.join("go"));
    common::fifo(&ready);
    common::fifo(&go);
    let merge = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["merge", "9"])
        .env("XTASK_PLACES", "1")
        .env("RUN_ON", "cargo fmt --all")
        .env(
            "RUN_COMMAND",
            format!("echo > '{}'; read _ < '{}'", ready.display(), go.display()),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    common::meet(&ready, false);
    let calls = repo.bin.join("calls");
    let mut waiter = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["run", "sh", "-c", "echo 'waiter ran' >> \"$0\""])
        .arg(&calls)
        .env("XTASK_PLACES", "1")
        .spawn()
        .unwrap();
    let queue = repo.root.join(".git/xtask/places.lock");
    common::wait_until("the run never waited for a place", || {
        common::locked(&queue)
    });
    common::meet(&go, true);
    let output = merge.wait_with_output().unwrap();
    assert!(waiter.wait().unwrap().success());
    assert_merged(&repo, &output);
    let calls = repo.calls();
    let ran = calls.find("waiter ran").expect("the run never ran");
    let own = calls.rfind("cargo fmt --all").unwrap();
    assert!(own > calls.find("cargo fmt --all").unwrap(), "{calls}");
    assert!(ran > own, "the run went between the two proofs:\n{calls}");
}

/// The merge runs again in the xtask of `main`, which takes the slot again,
/// and another merge can take it in between. That wait doesn't count against
/// the timeout either. The build of that xtask waits until this test is about
/// to wait for the slot, so that it does before the merge lets the slot go.
#[test]
fn the_wait_for_the_slot_in_the_xtask_of_main_does_not_count_against_the_timeout() {
    let repo = Repo::new("slot-wait-again");
    let build = "cargo build --quiet --package xtask";
    let waiting = repo.temp.join("waiting");
    // To files, because nothing reads a pipe while this waits.
    let said = |name: &str| fs::File::create(repo.temp.join(name)).unwrap();
    let mut merge = timed_merge(&repo)
        .env("RUN_ON", build)
        .env(
            "RUN_COMMAND",
            format!(
                "until [ -f '{}' ]; do sleep 0.1; done; sleep 1",
                waiting.display()
            ),
        )
        .stdout(said("merge.out"))
        .stderr(said("merge.err"))
        .spawn()
        .unwrap();
    let ended = |merge: &mut std::process::Child, why: &str| -> ! {
        let _ = merge.kill();
        let status = merge.wait().unwrap();
        panic!(
            "{why}: {status}\n{}\n{}",
            fs::read_to_string(repo.temp.join("merge.out")).unwrap(),
            fs::read_to_string(repo.temp.join("merge.err")).unwrap()
        );
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    while !repo.calls().contains(build) {
        if merge.try_wait().unwrap().is_some() || Instant::now() > deadline {
            ended(&mut merge, "the merge never built the xtask of `main`");
        }
        thread::sleep(Duration::from_millis(100));
    }
    let slot = fs::OpenOptions::new()
        .write(true)
        .open(repo.root.join(".git/xtask/merge.lock"))
        .unwrap();
    fs::write(&waiting, "").unwrap();
    slot.lock().unwrap();
    if merge.try_wait().unwrap().is_some() || repo.bin.join("merged").exists() {
        ended(&mut merge, "the merge let the slot go only as it ended");
    }
    // The wait alone now outlasts the timeout.
    thread::sleep(PAST_THE_TIMEOUT);
    if merge.try_wait().unwrap().is_some() {
        ended(&mut merge, "the merge didn't wait for the slot");
    }
    slot.unlock().unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = merge.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            ended(&mut merge, "the merge didn't end once it had the slot");
        }
        thread::sleep(Duration::from_millis(100));
    };
    let output = Output {
        status,
        stdout: fs::read(repo.temp.join("merge.out")).unwrap(),
        stderr: fs::read(repo.temp.join("merge.err")).unwrap(),
    };
    assert_merged(&repo, &output);
}

/// Other work holds the activity lock for as long as it runs, so the wait of a
/// merge's bench to hold it alone doesn't count against the timeout either.
#[test]
fn a_benchs_wait_for_the_activity_lock_does_not_count_against_the_timeout() {
    let base = record(0, 1, "");
    let repo = Repo::with(
        "bench-wait",
        &[("harness/ledger/3.toml", base.as_str())],
        &[("crates/ap/src/main.rs", "fn main() {\n    println!();\n}\n")],
    );
    let output = outwait_the_timeout(&repo, None, "Proving: bench");
    assert_merged(&repo, &output);
    assert!(stdout(&output).contains("bench: pass"), "{output:?}");
}

/// The bench of a PR's own xtask waits for the lock too. Its first proof has
/// benched by the time it starts, so the lock is taken then, and only the
/// second bench waits.
#[test]
fn a_benchs_wait_for_the_activity_lock_in_its_own_xtask_does_not_count_either() {
    let repo = changing(
        "own-bench-wait",
        &[],
        &[
            ("crates/ap/src/main.rs", "fn main() {\n    println!();\n}\n"),
            ("xtask/src/bench.rs", "// a better measurer\n"),
        ],
    );
    let own = "Proving: its own xtask";
    let output = outwait_the_timeout(&repo, Some(own), own);
    assert_merged(&repo, &output);
    assert_eq!(
        stdout(&output).matches("bench: pass").count(),
        2,
        "{output:?}"
    );
}

/// A PR's own xtask can say that its bench waits for the lock when it holds
/// the lock itself. The xtask of `main` sees that the bench doesn't wait, so
/// the timeout runs on, and stops the merge long before the bench would end.
#[test]
fn a_benchs_wait_that_its_own_xtask_only_claims_counts_against_the_timeout() {
    lying();
    let repo = changing(
        "own-bench-lies",
        &[],
        &[
            ("crates/ap/src/main.rs", "fn main() {\n    println!();\n}\n"),
            ("xtask/src/bench.rs", "// variant: lying\n"),
        ],
    );
    let output = repo.output(timed_merge(&repo).env("LIED", repo.bin.join("lied")));
    assert!(
        repo.bin.join("lied").exists(),
        "its own bench never said it waits:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(stderr(&output).contains("timeout"), "{}", stderr(&output));
    assert!(!repo.bin.join("merged").exists());
}

/// Runs a merge with `TIMEOUT`, and holds the activity lock shared
/// from when the calls hold `holding_from`, or from before the merge, until a
/// bench that starts after `bench_from` has waited for it longer than that.
/// Returns how the merge ended.
fn outwait_the_timeout(repo: &Repo, holding_from: Option<&str>, bench_from: &str) -> Output {
    let directory = repo.root.join(".git/xtask");
    fs::create_dir_all(&directory).unwrap();
    let lock = |name: &str| {
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(directory.join(name))
            .unwrap()
    };
    let activity = lock("activity.lock");
    if holding_from.is_none() {
        activity.lock_shared().unwrap();
    }
    // To files, because nothing reads a pipe while this waits.
    let said = |name: &str| fs::File::create(repo.temp.join(name)).unwrap();
    let mut merge = timed_merge(&repo)
        .stdout(said("merge.out"))
        .stderr(said("merge.err"))
        .spawn()
        .unwrap();
    let ended = |merge: &mut std::process::Child, why: &str| -> ! {
        let _ = merge.kill();
        let status = merge.wait().unwrap();
        panic!(
            "{why}: {status}\n{}\n{}",
            fs::read_to_string(repo.temp.join("merge.out")).unwrap(),
            fs::read_to_string(repo.temp.join("merge.err")).unwrap()
        );
    };
    // A bench keeps the turnstile for as long as it waits for the lock. A
    // build only passes through it, so the bench waits once the proof has
    // reached it and the turnstile is held twice in a row.
    let turnstile = lock("turnstile.lock");
    let held = || match turnstile.try_lock() {
        Ok(()) => turnstile.unlock().is_err(),
        Err(_) => true,
    };
    let deadline = Instant::now() + Duration::from_secs(240);
    let mut holding = holding_from.is_none();
    loop {
        if merge.try_wait().unwrap().is_some() || Instant::now() > deadline {
            ended(&mut merge, "the merge's bench never waited for the lock");
        }
        thread::sleep(Duration::from_millis(100));
        if !holding && holding_from.is_some_and(|from| repo.calls().contains(from)) {
            activity.lock_shared().unwrap();
            holding = true;
        }
        if holding && repo.calls().contains(bench_from) && held() {
            thread::sleep(Duration::from_millis(200));
            if held() {
                break;
            }
        }
    }
    // The wait alone now outlasts the timeout.
    thread::sleep(PAST_THE_TIMEOUT);
    if merge.try_wait().unwrap().is_some() {
        ended(&mut merge, "the merge didn't wait for the activity lock");
    }
    let waiting = "pending Waiting: the activity lock";
    assert_eq!(last_post(repo), waiting);
    activity.unlock().unwrap();
    // The timeout goes on with what the proof left of it, so the bench ends
    // within it, one way or the other.
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = merge.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            ended(&mut merge, "the merge didn't end once it had the lock");
        }
        thread::sleep(Duration::from_millis(100));
    };
    // Once the wait ends, the proof says again what it does.
    let said: Vec<String> = posts(repo).into_iter().map(|(_, said)| said).collect();
    let after = said
        .iter()
        .position(|said| said == waiting)
        .and_then(|at| said.get(at + 1));
    assert_eq!(after, Some(&format!("pending {bench_from}")), "{said:?}");
    Output {
        status,
        stdout: fs::read(repo.temp.join("merge.out")).unwrap(),
        stderr: fs::read(repo.temp.join("merge.err")).unwrap(),
    }
}

/// A command that a merge starts, and that outlives its parent: it ignores
/// hangups, and nothing waits for it. It writes its process id to `stray`.
pub(super) fn stray(repo: &Repo) -> String {
    format!(
        "(trap '' HUP; exec sleep 30) </dev/null >/dev/null 2>&1 & echo $! > '{}/stray'",
        repo.bin.display()
    )
}

fn stray_pid(repo: &Repo) -> Option<String> {
    let text = fs::read_to_string(repo.bin.join("stray")).ok()?;
    text.ends_with('\n').then(|| text.trim().to_string())
}

/// A zombie counts as stopped, because only its reaping is left.
fn running(pid: &str) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
    })
}

/// A killed process needs a moment to die, so this waits a little for it.
#[track_caller]
pub(super) fn assert_stray_stopped(repo: &Repo) {
    let pid = stray_pid(repo).expect("the stray command never started");
    let deadline = Instant::now() + Duration::from_secs(5);
    while running(&pid) {
        if Instant::now() > deadline {
            let _ = Command::new("kill").args(["-KILL", &pid]).status();
            panic!("the stray command, process {pid}, kept running");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_merge_past_its_timeout_stops_what_it_started() {
    let repo = Repo::new("timeout-stray");
    // The stray starts before the merge takes the slot, which starts the
    // timer, and the hang outlasts the timeout by far. So the timeout is what
    // stops the merge, wherever the merge is when it fires.
    let output = repo.output(
        repo.command(env!("CARGO_BIN_EXE_xtask"))
            .args(["merge", "9"])
            .env("RUN_ON", "gh pr view")
            .env("RUN_COMMAND", stray(&repo))
            .env("HANG_ON", "cargo fmt")
            .env("HANG_FOR", "60")
            .env("XTASK_MERGE_TIMEOUT_SECONDS", "1")
            // As a merge that a merge's own tests start inherits it.
            .env(
                "XTASK_MERGE_WORKER_OF",
                std::os::unix::process::parent_id().to_string(),
            ),
    );
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(stderr(&output).contains("timeout"), "{}", stderr(&output));
    assert_stray_stopped(&repo);
}

#[test]
fn a_merge_that_ends_stops_what_it_started() {
    let repo = Repo::new("ended-stray");
    let output = repo.merge_running("cargo fmt", &stray(&repo));
    assert_merged(&repo, &output);
    assert_stray_stopped(&repo);
}

/// As on Ctrl-C, which reaches the caller's process group but not the merge's,
/// and as when a test gives up on a merge that hangs, so that the test fails,
/// and the merge whose proof runs the tests goes on.
#[test]
fn a_merge_whose_caller_is_killed_stops_what_it_started() {
    let repo = Repo::new("caller-killed");
    let mut merge = repo.command(env!("CARGO_BIN_EXE_xtask"));
    merge
        .args(["merge", "9"])
        .env("RUN_ON", "cargo fmt")
        .env("RUN_COMMAND", format!("{}; sleep 30", stray(&repo)));
    let started = Instant::now();
    let killed = output_unless(&mut merge, || {
        stray_pid(&repo).is_some() || started.elapsed() > HUNG
    });
    assert!(killed.is_err(), "the merge ended on its own: {killed:?}");
    assert_stray_stopped(&repo);
    assert_merged(&repo, &repo.merge());
}
