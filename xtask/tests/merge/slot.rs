use super::prepare::{last_post, posts};
use super::waits::changing;
use super::*;

#[test]
fn a_kill_at_each_step_resumes() {
    let cases = [
        ("fetch", "git fetch", "before"),
        ("worktree", "git worktree add", "after"),
        ("rebase-before", "git rebase", "before"),
        ("rebase-after", "git rebase", "after"),
        ("gate", "cargo fmt", "before"),
        ("journey", "cargo build --release", "before"),
        ("commit", "git commit", "after"),
        ("push-before", "git push", "before"),
        ("push-after", "git push", "after"),
        ("pending", "state=pending", "before"),
        ("status", "state=success", "before"),
        ("merge-before", "gh pr merge", "before"),
        ("merge-after", "gh pr merge", "after"),
    ];
    thread::scope(|scope| {
        for (name, on, when) in cases {
            scope.spawn(move || {
                let repo = Repo::new(&format!("kill-{name}"));
                let killed = repo.merge_killed(on, when);
                assert_eq!(killed.status.code(), None, "{name} wasn't killed");
                assert!(repo.bin.join("killed").exists(), "{name}");

                let output = repo.merge();
                assert_merged(&repo, &output);
                assert_eq!(repo.commits_merged(), 2, "{name}");
                assert_eq!(
                    fs::read_to_string(repo.bin.join("merges")).unwrap(),
                    "merged\n",
                    "{name}: {}",
                    repo.calls()
                );
                assert!(repo.merge_trees().is_empty(), "{name}");
                assert_eq!(repo.git(&["worktree", "list"]).lines().count(), 1, "{name}");
            });
        }
    });
}

/// A killed run leaves its tree, and a later merge of another PR removes it,
/// but not the tree of a run that still holds it.
#[test]
fn a_merge_removes_trees_no_live_run_holds() {
    let repo = Repo::new("sweep");
    let killed = repo.merge_killed("git rebase", "after");
    assert_eq!(killed.status.code(), None, "{}", stderr(&killed));
    let left = repo.merge_trees();
    let [left] = &left
        .iter()
        .filter(|n| !n.ends_with(".lock"))
        .collect::<Vec<_>>()[..]
    else {
        panic!("{left:?}");
    };
    // The PR number ends the name, before any padding.
    let at = left.rfind("-9").unwrap();
    let dead = format!("{}-8{}", &left[..at], &left[at + 2..]);
    let live = format!("{}-7{}", &left[..at], &left[at + 2..]);
    let moved = [
        "worktree",
        "move",
        &repo.temp.join(left).display().to_string(),
        &repo.temp.join(&dead).display().to_string(),
    ];
    repo.git(&moved);
    fs::create_dir(repo.temp.join(&live)).unwrap();
    let held = fs::File::create(repo.temp.join(format!("{live}.lock"))).unwrap();
    held.lock().unwrap();

    assert_merged(&repo, &repo.merge());
    let mut left = repo.merge_trees();
    left.sort();
    assert_eq!(left, [live.clone(), format!("{live}.lock")]);
    assert_eq!(repo.git(&["worktree", "list"]).lines().count(), 1);
    drop(held);
}

#[test]
fn a_worktree_locked_by_a_killed_attempt_is_cleared() {
    let repo = Repo::new("locked-worktree");
    assert_eq!(repo.merge_killed("cargo fmt", "before").status.code(), None);
    let listing = repo.git(&["worktree", "list", "--porcelain"]);
    let path = listing
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .nth(1)
        .expect("the killed merge left its worktree")
        .to_string();
    repo.git(&["worktree", "lock", &path]);
    assert_merged(&repo, &repo.merge());
    assert_eq!(repo.git(&["worktree", "list"]).lines().count(), 1);
}

#[test]
fn a_merge_waits_for_the_merge_slot() {
    let repo = Repo::new("slot");
    let directory = repo.root.join(".git/xtask");
    fs::create_dir_all(&directory).unwrap();
    let held = fs::File::create(directory.join("merge.lock")).unwrap();
    held.lock().unwrap();
    let mut child = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["merge", "9"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (said, lines) = mpsc::channel();
    let errors = BufReader::new(child.stderr.take().unwrap());
    thread::spawn(move || {
        for line in errors.lines() {
            let Ok(line) = line else { break };
            if said.send(line).is_err() {
                break;
            }
        }
    });
    // It reads the PR's diff before the slot, and then waits.
    wait_for_the_early_check(&repo);
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut heard = Vec::new();
    while !heard.iter().any(|line| line == SLOT_WAIT) {
        let left = deadline.saturating_duration_since(Instant::now());
        match lines.recv_timeout(left) {
            Ok(line) => heard.push(line),
            Err(_) => panic!("it never said that it waits: {heard:?}"),
        }
    }
    // It says so before it waits, so give it time to go on if it didn't wait.
    thread::sleep(Duration::from_millis(500));
    let calls = repo.calls();
    assert!(!calls.contains("refs/remotes/origin/"), "{calls}");
    assert!(!calls.contains("git worktree add"), "{calls}");
    assert!(!calls.contains("git rebase"), "{calls}");
    let before = repo.head();
    let waiting = (
        before.clone(),
        "pending Waiting: the merge slot".to_string(),
    );
    assert_eq!(posts(&repo), std::slice::from_ref(&waiting));
    held.unlock().unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0), "{heard:?}");
    assert!(repo.bin.join("merged").exists());
    // The proof then says what it does, as a merge that didn't wait does.
    let said = |commit: &String, text: &str| (commit.clone(), text.to_string());
    assert_eq!(
        posts(&repo),
        [
            waiting,
            said(&before, "pending Proving: lint"),
            said(&before, "pending Proving: tests"),
            said(&before, "pending Proving: journey"),
            said(&repo.head(), "success spec 0 of 2, journey 1 of 1"),
        ]
    );
}

/// A second merge waits for the slot while the first builds the xtask of
/// `main`. The first lets the slot go to run that xtask, which takes it again,
/// and the second doesn't take it in between.
#[test]
fn a_merge_keeps_its_turn_from_the_slot_to_its_proof() {
    let repo = Repo::new("turn");
    repo.other_pr(10, "8-y", &Pr::default());
    let first_head = repo.head();
    let second_head = repo.head_of("8-y");
    let (first, second, heard) = merge_ten_during(&repo, &["merge", "9"]);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    assert_eq!(second.status.code(), Some(0), "{heard:?}");
    let calls = repo.calls();
    let mut proving: Vec<String> = posts(&repo)
        .into_iter()
        .filter(|(_, said)| said.starts_with("pending Proving:"))
        .map(|(commit, _)| commit)
        .collect();
    proving.dedup();
    assert_eq!(proving, [first_head, second_head.clone()], "{calls}");
    // Once, though it let the slot go for the turn.
    let waited: Vec<(String, String)> = posts(&repo)
        .into_iter()
        .filter(|(commit, said)| *commit == second_head && said.starts_with("pending Waiting:"))
        .collect();
    let waiting = "pending Waiting: the merge slot".to_string();
    assert_eq!(waited, [(second_head, waiting)], "{calls}");
    assert_merged_after(&calls, 9);
}

/// Runs `xtask` with `first`, which pauses at its build of the xtask of
/// `main`, starts `merge 10` then, and lets `first` go on once that merge says
/// it waits for the slot. Gives back how each ended, and what the merge said.
pub(super) fn merge_ten_during(repo: &Repo, first: &[&str]) -> (Output, Output, Vec<String>) {
    let (first, mut then) = during(repo, first, &[&["merge", "10"]]);
    let second = then.remove(0);
    let heard = stderr(&second).lines().map(String::from).collect();
    (first, second, heard)
}

/// Runs `xtask` with `first`, which pauses at its build of the xtask of
/// `main`. Starts each of `then` in turn, once the one before it says that it
/// waits for the slot, and lets `first` go on once the last says so. Gives
/// back how each ended.
fn during(repo: &Repo, first: &[&str], then: &[&[&str]]) -> (Output, Vec<Output>) {
    let building = repo.bin.join("building");
    let go = repo.bin.join("go");
    // Bounded, so that a failed test leaves nothing waiting for good.
    let pause = format!(
        ": > '{}'; n=0; until [ -f '{}' ] || [ $n -ge 600 ]; do sleep 0.1; n=$((n+1)); done",
        building.display(),
        go.display()
    );
    let first = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(first)
        .env("RUN_ON", "build --quiet --package xtask")
        .env("RUN_COMMAND", pause)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    while !building.exists() {
        assert!(Instant::now() < deadline, "{}", repo.calls());
        thread::sleep(Duration::from_millis(10));
    }
    let waiters: Vec<Waiter> = then
        .iter()
        .map(|args| Waiter::start(repo, args, deadline))
        .collect();
    fs::write(&go, "").unwrap();
    let mut pids = vec![first.id()];
    pids.extend(waiters.iter().map(|waiter| waiter.child.id()));
    let done = bounded(pids);
    let first = first.wait_with_output().unwrap();
    let then = waiters.into_iter().map(Waiter::end).collect();
    let _ = done.send(());
    (first, then)
}

/// Kills `pids` unless the test sends on what this gives back within five
/// minutes, so that a run that never ends fails the test instead of hanging
/// it. A test that panics drops it, which kills them at once.
fn bounded(pids: Vec<u32>) -> mpsc::Sender<()> {
    let (done, ended) = mpsc::channel::<()>();
    thread::spawn(move || {
        if ended.recv_timeout(Duration::from_secs(300)).is_err() {
            for pid in pids {
                let _ = Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .status();
            }
        }
    });
    done
}

/// A run of `xtask` that has said it waits for the merge slot.
struct Waiter {
    child: std::process::Child,
    heard: Vec<String>,
    lines: mpsc::Receiver<String>,
}

impl Waiter {
    /// Starts `args`, a command and a PR, and returns once it says that it
    /// waits for the slot.
    #[track_caller]
    fn start(repo: &Repo, args: &[&str], deadline: Instant) -> Waiter {
        let mut child = repo
            .command(env!("CARGO_BIN_EXE_xtask"))
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let errors = BufReader::new(child.stderr.take().unwrap());
        let (said, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in errors.lines() {
                let Ok(line) = line else { break };
                if said.send(line).is_err() {
                    break;
                }
            }
        });
        let waits = format!(
            "{}: PR {} waits for the merge slot, which another run holds",
            args[0], args[1]
        );
        let mut heard = Vec::new();
        while !heard.contains(&waits) {
            let left = deadline.saturating_duration_since(Instant::now());
            match lines.recv_timeout(left) {
                Ok(line) => heard.push(line),
                Err(_) => panic!("{} {} never waited: {heard:?}", args[0], args[1]),
            }
        }
        Waiter {
            child,
            heard,
            lines,
        }
    }

    /// How it ended, with all it said on stderr.
    fn end(self) -> Output {
        let mut output = self.child.wait_with_output().unwrap();
        let mut said = self.heard;
        said.extend(self.lines.iter());
        output.stderr = said.join("\n").into_bytes();
        output
    }
}

/// Three runs wait for the slot, one after another, behind a merge that hands
/// it to the xtask of `main`, and each gets it in the order it came.
#[test]
fn waiters_get_the_merge_slot_in_the_order_they_came() {
    let repo = Repo::new("queue");
    let waiters = [
        ("merge", 10, "8-y"),
        ("prepare", 11, "6-w"),
        ("land", 12, "5-v"),
    ];
    for (_, number, branch) in waiters {
        repo.other_pr(number, branch, &Pr::default());
    }
    let numbers: Vec<String> = waiters.iter().map(|(_, n, _)| n.to_string()).collect();
    let then: Vec<[&str; 2]> = waiters
        .iter()
        .zip(&numbers)
        .map(|((command, _, _), number)| [*command, number.as_str()])
        .collect();
    let then: Vec<&[&str]> = then.iter().map(|args| &args[..]).collect();
    let (first, then) = during(&repo, &["merge", "9"], &then);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    for output in &then {
        assert_eq!(output.status.code(), Some(0), "{}", stderr(output));
    }
    let calls = repo.calls();
    let mut took = vec![calls.find("gh pr merge 9 ").expect(&calls)];
    for (_, _, branch) in waiters {
        let fetched = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
        took.push(calls.find(&fetched).expect(&calls));
    }
    assert!(took.is_sorted(), "{took:?}\n{calls}");
}

/// The first waiter gets the slot during a turn, and waits for the turn to end.
/// Two more come while the turn's holder has the slot back, as a refresh does
/// for its squash. The refresh lets the turn go first, and then the slot.
#[test]
fn a_waiter_keeps_its_place_while_it_waits_for_a_turn() {
    let repo = Repo::new("queue-turn");
    repo.other_pr(10, "8-y", &Pr::default());
    repo.other_pr(11, "6-w", &Pr::default());
    let directory = repo.root.join(".git/xtask");
    fs::create_dir_all(&directory).unwrap();
    let turn = fs::File::create(directory.join("turn.lock")).unwrap();
    turn.lock().unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    // It says so once it has let the slot go.
    let first = Waiter::start(&repo, &["merge", "9"], deadline);
    let slot = fs::File::create(directory.join("merge.lock")).unwrap();
    slot.lock().unwrap();
    let waiters = [
        first,
        Waiter::start(&repo, &["prepare", "10"], deadline),
        Waiter::start(&repo, &["land", "11"], deadline),
    ];
    turn.unlock().unwrap();
    slot.unlock().unwrap();
    let done = bounded(waiters.iter().map(|waiter| waiter.child.id()).collect());
    for waiter in waiters {
        let output = waiter.end();
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    }
    let _ = done.send(());
    let calls = repo.calls();
    let took: Vec<usize> = [BRANCH, "8-y", "6-w"]
        .iter()
        .map(|branch| {
            let fetched = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
            calls.find(&fetched).expect(&calls)
        })
        .collect();
    assert!(took.is_sorted(), "{took:?}\n{calls}");
}

/// A waiter that is killed while it waits holds up none that came after it.
#[test]
fn a_killed_waiter_holds_no_one_up() {
    let repo = Repo::new("queue-killed");
    repo.other_pr(10, "8-y", &Pr::default());
    let directory = repo.root.join(".git/xtask");
    fs::create_dir_all(&directory).unwrap();
    let held = fs::File::create(directory.join("merge.lock")).unwrap();
    held.lock().unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut killed = Waiter::start(&repo, &["merge", "9"], deadline);
    let second = Waiter::start(&repo, &["merge", "10"], deadline);
    let done = bounded(vec![second.child.id()]);
    // The pid of a child this test started. Its supervisor goes first, and its
    // watcher then kills the rest, which leaves the first number behind.
    killed.child.kill().unwrap();
    assert_eq!(killed.child.wait().unwrap().code(), None);
    let first = directory.join("merge-queue/0.lock");
    loop {
        let left = fs::File::open(&first).expect("the killed waiter's number is gone");
        if left.try_lock_shared().is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "the killed waiter still waits");
        thread::sleep(Duration::from_millis(10));
    }
    held.unlock().unwrap();
    let output = second.end();
    let _ = done.send(());
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(repo.calls().contains("gh pr merge 10 "), "{}", repo.calls());
}

/// Merge 10 does nothing under the slot until PR `number` has merged.
#[track_caller]
pub(super) fn assert_merged_after(calls: &str, number: u64) {
    let started = calls
        .find("+refs/heads/8-y:refs/remotes/origin/8-y")
        .expect(calls);
    let merged = calls.find(&format!("gh pr merge {number} ")).expect(calls);
    assert!(merged < started, "{calls}");
}

/// A merge whose xtask of `main` can't start ends the wait it posted, as an
/// error does.
#[test]
fn a_merge_whose_xtask_of_main_cant_start_ends_its_wait() {
    let repo = Repo::new("cant-start");
    let directory = repo.root.join(".git/xtask");
    fs::create_dir_all(&directory).unwrap();
    let held = fs::File::create(directory.join("merge.lock")).unwrap();
    held.lock().unwrap();
    // The stand-in build then leaves a program that nobody can run.
    let child = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["merge", "9"])
        .env("RUN_ON", "build --quiet --package xtask")
        .env(
            "RUN_COMMAND",
            r#"cp() { command cp "$@" && chmod a-x "$2"; }"#,
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while posts(&repo).is_empty() {
        assert!(Instant::now() < deadline, "{}", repo.calls());
        thread::sleep(Duration::from_millis(10));
    }
    held.unlock().unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("couldn't run the xtask of `main`"),
        "{}",
        stderr(&output)
    );
    assert_eq!(last_post(&repo), "failure Stopped by an error");
}

/// So neither the slot nor the turn outlives a killed merge in a command it
/// started. Only `gh pr merge` keeps the slot.
#[test]
fn a_command_that_a_merge_starts_holds_neither_the_slot_nor_the_turn() {
    // Before the turn, and in the xtask of `main` after it.
    for (name, on) in [
        ("build", "build --quiet --package xtask"),
        ("gate", "cargo fmt"),
    ] {
        let repo = Repo::new(&format!("no-lock-{name}"));
        let fds = repo.bin.join("fds");
        let list = format!("ls -l /proc/$$/fd/ > '{}'", fds.display());
        assert_merged(&repo, &repo.merge_running(on, &list));
        let open = fs::read_to_string(&fds).unwrap();
        assert!(open.contains(" -> "), "{name}: {open}");
        for lock in ["merge.lock", "turn.lock"] {
            assert!(!open.contains(lock), "{name}: {open}");
        }
    }
}

#[test]
fn the_pr_is_read_again_after_the_slot() {
    let repo = Repo::new("read-again");
    let directory = repo.root.join(".git/xtask");
    fs::create_dir_all(&directory).unwrap();
    let held = fs::File::create(directory.join("merge.lock")).unwrap();
    held.lock().unwrap();
    let child = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["merge", "9"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !repo.calls().contains("gh issue view") {
        assert!(Instant::now() < deadline, "{}", repo.calls());
        thread::sleep(Duration::from_millis(10));
    }
    let waiting = "pending Waiting: the merge slot";
    while !posts(&repo).iter().any(|(_, said)| said == waiting) {
        assert!(Instant::now() < deadline, "{}", repo.calls());
        thread::sleep(Duration::from_millis(10));
    }
    repo.set_pr(&Pr {
        base: "release",
        ..Pr::default()
    });
    held.unlock().unwrap();
    let output = child.wait_with_output().unwrap();
    assert_refused(&repo, &output, "targets `release`");
    // What it waited for doesn't stay on the PR.
    let ended = last_post(&repo);
    assert!(ended.starts_with("failure "), "{ended}");
    assert!(ended.contains("targets `release`"), "{ended}");
}

/// The xtask of `main` ends what the merge posted before it ran that xtask,
/// as when it refuses a PR that changed while it was built.
#[test]
fn the_xtask_of_main_ends_the_wait_that_the_merge_posted() {
    let repo = Repo::new("wait-then-main-refuses");
    let release = Pr {
        base: "release",
        ..Pr::default()
    };
    let changed = repo.bin.join("pr-release.json");
    fs::write(&changed, pr_json(&release, "OPEN")).unwrap();
    let directory = repo.root.join(".git/xtask");
    fs::create_dir_all(&directory).unwrap();
    let held = fs::File::create(directory.join("merge.lock")).unwrap();
    held.lock().unwrap();
    let child = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["merge", "9"])
        .env("RUN_ON", "cargo build --quiet --package xtask")
        .env(
            "RUN_COMMAND",
            format!(
                "cp '{}' '{}'",
                changed.display(),
                repo.bin.join("pr.json").display()
            ),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let waiting = "pending Waiting: the merge slot";
    while !posts(&repo).iter().any(|(_, said)| said == waiting) {
        assert!(Instant::now() < deadline, "{}", repo.calls());
        thread::sleep(Duration::from_millis(10));
    }
    held.unlock().unwrap();
    let output = child.wait_with_output().unwrap();
    assert_refused(&repo, &output, "targets `release`");
    let ended = last_post(&repo);
    assert!(ended.starts_with("failure "), "{ended}");
    assert!(ended.contains("targets `release`"), "{ended}");
}

/// A merge that the gate's tests run inherits the environment of the merge
/// that runs the gate, and what that merge posted isn't this one's to end.
#[test]
fn a_wait_that_an_outer_merge_posted_is_not_this_ones() {
    let repo = Repo::new("outer-wait");
    let release = Pr {
        base: "release",
        ..Pr::default()
    };
    let changed = repo.bin.join("pr-release.json");
    fs::write(&changed, pr_json(&release, "OPEN")).unwrap();
    let outer = "0123456789abcdef0123456789abcdef01234567";
    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["merge", "9"])
        .env("XTASK_MERGE_POSTED", outer)
        .env("RUN_ON", "cargo build --quiet --package xtask")
        .env(
            "RUN_COMMAND",
            format!(
                "cp '{}' '{}'",
                changed.display(),
                repo.bin.join("pr.json").display()
            ),
        )
        .output()
        .unwrap();
    assert_refused(&repo, &output, "targets `release`");
    assert_eq!(posts(&repo), [], "{}", repo.calls());
}

/// A merged PR keeps the status it merged with, even when a merge of it
/// waits for the slot.
#[test]
fn a_wait_posts_nothing_on_a_merged_pr() {
    let repo = Repo::new("wait-merged");
    fs::write(repo.bin.join("merged-9"), "").unwrap();
    repo.set_pr(&Pr {
        state: "MERGED",
        ..Pr::default()
    });
    let output = run_while_the_slot_is_held_for_a_moment(&repo);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("waits for the merge slot"),
        "{}",
        stderr(&output)
    );
    assert_eq!(posts(&repo), [], "{}", repo.calls());
}

/// Runs `land` while another run holds the merge slot, and lets the slot go
/// once `land` says that it waits for it.
fn run_while_the_slot_is_held_for_a_moment(repo: &Repo) -> Output {
    let directory = repo.root.join(".git/xtask");
    fs::create_dir_all(&directory).unwrap();
    let held = fs::File::create(directory.join("merge.lock")).unwrap();
    held.lock().unwrap();
    let said = repo.temp.join("land.err");
    let child = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["land", "9"])
        .stdout(Stdio::piped())
        .stderr(fs::File::create(&said).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !fs::read_to_string(&said)
        .unwrap()
        .contains("waits for the merge slot")
    {
        assert!(Instant::now() < deadline, "{}", repo.calls());
        thread::sleep(Duration::from_millis(10));
    }
    held.unlock().unwrap();
    let mut output = child.wait_with_output().unwrap();
    output.stderr = fs::read(&said).unwrap();
    output
}

/// Waits until the merge has removed both refs of its check before the slot,
/// which it does once the check is done.
#[track_caller]
fn wait_for_the_early_check(repo: &Repo) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while repo
        .calls()
        .matches("update-ref -d refs/xtask/early-")
        .count()
        < 2
    {
        assert!(Instant::now() < deadline, "{}", repo.calls());
        thread::sleep(Duration::from_millis(10));
    }
}

/// Runs `command` while another run holds the merge slot, and fails if it
/// waits for the slot.
#[track_caller]
fn run_while_the_slot_is_held(repo: &Repo, command: &str) -> Output {
    let directory = repo.root.join(".git/xtask");
    fs::create_dir_all(&directory).unwrap();
    let held = fs::File::create(directory.join("merge.lock")).unwrap();
    held.lock().unwrap();
    let mut child = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args([command, "9"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let ended = loop {
        if child.try_wait().unwrap().is_some() {
            break true;
        }
        if Instant::now() > deadline {
            break false;
        }
        thread::sleep(Duration::from_millis(50));
    };
    held.unlock().unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        ended,
        "{command} waited for the slot\nstdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    output
}

/// A PR that waits for the owner fails `merge` before the slot, with the words
/// it fails with under the slot.
#[test]
fn a_pr_that_waits_fails_before_the_slot() {
    let repo = changing(
        "early-owner",
        &[],
        &[(".github/workflows/x.yml", "name: x\n")],
    );
    let output = run_while_the_slot_is_held(&repo, "merge");
    assert_refused(&repo, &output, "it changes `.github/workflows/x.yml`");
    assert!(
        stdout(&output).contains("fail owner"),
        "{}",
        stdout(&output)
    );
    assert!(!repo.calls().contains("git rebase"), "{}", repo.calls());
    assert_eq!(last_post(&repo), "failure Failed: owner");
    // `prepare` takes it, so only the slot tells it apart.
    assert_prepared(&repo, &repo.prepare());
}

/// A change that `main` already has, as from a squashed PR that the branch
/// was built on, is no part of what merging the branch changes.
#[test]
fn a_change_main_already_has_does_not_fail_before_the_slot() {
    let repo = changing(
        "early-squashed",
        &[],
        &[
            (".github/workflows/x.yml", "name: x\n"),
            ("docs.txt", "x\n"),
        ],
    );
    repo.change_branch("crates/demo/src/lib.rs", CHECK_ONE);
    repo.advance_main(".github/workflows/x.yml", "name: x\n");
    assert_merged(&repo, &repo.merge());
}

/// A `Hill` line with no reason, or a change that no count comes from and no
/// line, fails before the slot, under either command.
#[test]
fn a_hill_line_the_proof_would_refuse_fails_before_the_slot() {
    let base = record(0, 1, "");
    let cases = [
        (
            "Closes #7.\n\nHill: none\n",
            "crates/demo/src/lib.rs",
            CHECK_ONE,
        ),
        ("Closes #7.\n", "AGENTS.md", "# Rules\n"),
    ];
    for (index, (body, path, content)) in cases.into_iter().enumerate() {
        for command in ["merge", "prepare"] {
            let repo = Repo::with(
                &format!("early-hill-{index}-{command}"),
                &[("harness/ledger/3.toml", base.as_str())],
                &[(path, content)],
            );
            repo.set_pr(&Pr {
                body,
                ..Pr::default()
            });
            let output = run_while_the_slot_is_held(&repo, command);
            assert_refused(&repo, &output, "Hill: none, <reason>");
            assert!(
                stdout(&output).contains("fail hill-line"),
                "{}",
                stdout(&output)
            );
            assert!(!repo.calls().contains("git rebase"), "{}", repo.calls());
            assert_eq!(last_post(&repo), "failure Failed: hill-line");
        }
    }
}

/// What the PR says while the merge waits for the slot is what counts.
#[test]
fn the_xtask_of_main_checks_the_hill_line_again_under_the_slot() {
    let repo = Repo::new("early-hill-again");
    let directory = repo.root.join(".git/xtask");
    fs::create_dir_all(&directory).unwrap();
    let held = fs::File::create(directory.join("merge.lock")).unwrap();
    held.lock().unwrap();
    let child = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["merge", "9"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_the_early_check(&repo);
    repo.set_pr(&Pr {
        body: "Closes #7.\n\nHill: none\n",
        ..Pr::default()
    });
    held.unlock().unwrap();
    let output = child.wait_with_output().unwrap();
    assert_refused(&repo, &output, "Hill: none, <reason>");
    assert!(repo.calls().contains("git rebase"), "{}", repo.calls());
    assert_eq!(last_post(&repo), "failure Failed: hill-line");
}

/// The same, for a change that makes the PR wait.
#[test]
fn the_xtask_of_main_checks_what_waits_again_under_the_slot() {
    let repo = Repo::new("early-owner-again");
    let directory = repo.root.join(".git/xtask");
    fs::create_dir_all(&directory).unwrap();
    let held = fs::File::create(directory.join("merge.lock")).unwrap();
    held.lock().unwrap();
    let child = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["merge", "9"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_the_early_check(&repo);
    repo.change_branch(".github/workflows/x.yml", "name: x\n");
    held.unlock().unwrap();
    let output = child.wait_with_output().unwrap();
    assert_refused(&repo, &output, "it changes `.github/workflows/x.yml`");
    assert!(repo.calls().contains("git rebase"), "{}", repo.calls());
    assert_eq!(last_post(&repo), "failure Failed: owner");
}

#[test]
fn a_branch_that_moved_during_the_merge_is_not_overwritten() {
    let repo = Repo::new("moved");
    let output = repo.merge_running(
        "git push",
        &repo.update_ref("refs/heads/7-x", "refs/heads/main"),
    );
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert_eq!(repo.head(), repo.origin(&["rev-parse", "refs/heads/main"]));
    assert!(!repo.calls().contains("gh pr merge"), "{}", repo.calls());
}

#[test]
fn a_main_that_moved_during_the_proof_stops_the_merge() {
    let repo = Repo::new("main-moved");
    let output = repo.merge_running(
        "cargo build --release",
        &repo.update_ref("refs/heads/main", "refs/heads/7-x"),
    );
    assert_refused(&repo, &output, "`main` moved");
    assert_eq!(
        last_post(&repo),
        "failure `main` moved during the merge, so this head wasn't tested on it. Run it again"
    );
}

#[test]
fn a_pr_that_changed_during_the_proof_is_refused() {
    let cases = [
        (
            "base-refused",
            Pr {
                base: "release",
                ..Pr::default()
            },
            "targets `release`",
        ),
        (
            "body-changed",
            Pr {
                body: "Closes #7.\n\nHill: none, something else.\n",
                ..Pr::default()
            },
            "changed",
        ),
    ];
    for (name, next, message) in cases {
        let repo = Repo::new(&format!("changed-{name}"));
        repo.set_next_pr(&next);
        let swap = format!("cp '{0}/pr-next.json' '{0}/pr.json'", repo.bin.display());
        let output = repo.merge_running("cargo build --release", &swap);
        assert_refused(&repo, &output, message);
    }
}

#[test]
fn a_long_reason_is_cut_to_what_github_keeps() {
    let repo = Repo::new("long-reason");
    let base = "b".repeat(150);
    repo.set_next_pr(&Pr {
        base: &base,
        ..Pr::default()
    });
    let swap = format!("cp '{0}/pr-next.json' '{0}/pr.json'", repo.bin.display());
    let output = repo.merge_running("cargo build --release", &swap);
    let why = format!("PR 9 targets `{base}`, and only `main` is merged here");
    assert_refused(&repo, &output, &why);
    let cut: String = why.chars().take(140).collect();
    assert_eq!(last_post(&repo), format!("failure {cut}"));
}

/// The checks read no PR label, and a person can park or unpark the issue at
/// any moment.
#[test]
fn a_label_the_checks_do_not_read_can_change_during_the_proof() {
    for label in ["land-needed", "answer-needed", "spec-change", "other"] {
        let repo = Repo::new(&format!("unread-{label}"));
        repo.set_next_pr(&Pr {
            labels: &[label],
            ..Pr::default()
        });
        let swap = format!("cp '{0}/pr-next.json' '{0}/pr.json'", repo.bin.display());
        assert_merged(&repo, &repo.merge_running("cargo build --release", &swap));
    }
}

#[test]
fn a_killed_merge_keeps_the_slot_until_its_squash_finishes() {
    let repo = Repo::new("squash-outlives");
    let killed = repo.merge_running("gh pr merge", "kill -9 $PPID; sleep 2");
    assert_eq!(killed.status.code(), None);
    let output = repo.merge();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("already merged"),
        "{}",
        stdout(&output)
    );
    assert_eq!(
        fs::read_to_string(repo.bin.join("merges")).unwrap(),
        "merged\n"
    );
}

#[test]
fn a_merge_resumes_from_another_worktree_and_clears_the_first() {
    let repo = Repo::new("other-worktree");
    let killed = repo.merge_killed("cargo fmt", "before");
    assert_eq!(killed.status.code(), None);
    let other = repo.temp.parent().unwrap().join("other");
    let name = other.display().to_string();
    repo.git(&["worktree", "add", "-q", "--detach", &name, "main"]);
    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["merge", "9"])
        .current_dir(&other)
        .output()
        .unwrap();
    assert_merged(&repo, &output);
    assert!(repo.merge_trees().is_empty(), "{:?}", repo.merge_trees());
    assert_eq!(repo.git(&["worktree", "list"]).lines().count(), 2);
}
