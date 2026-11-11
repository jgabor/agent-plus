//! Which issues can be taken, and taking one. HARNESS.md defines the states.

use crate::forge::{Comment, Forge, Gh, Issue, LISTED_COMMENTS};
use crate::json::quoted;
use crate::{Problem, Verdict, base, lock, workspace};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const WORKER_SLOTS: usize = 3;
const LEASE_SECONDS: i64 = 12 * 3600;
const IN_PROGRESS: &str = "in-progress";
/// What parks an issue: a PR ready to merge, or a question waiting for an answer.
pub const LAND_NEEDED: &str = "land-needed";
pub const ANSWER_NEEDED: &str = "answer-needed";
pub const PARKED: [&str; 2] = [LAND_NEEDED, ANSWER_NEEDED];
const STATUS_MARK: &str = "<!-- status -->";
const DEFAULT_BRANCH: &str = "main";

/// Seconds since 1970, from `2026-11-09T14:00Z`, with or without seconds.
fn seconds(stamp: &str) -> Option<i64> {
    let (date, time) = stamp.strip_suffix('Z')?.split_once('T')?;
    let mut date = date.split('-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let mut time = time.split(':').map(str::parse::<i64>);
    let (hour, minute) = (time.next()?.ok()?, time.next()?.ok()?);
    let second = time.next().unwrap_or(Ok(0)).ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..60).contains(&second)
        || date.next().is_some()
        || time.next().is_some()
    {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86400 + hour * 3600 + minute * 60 + second)
}

/// The format a lease is written in, which stops at the minute.
fn stamp(seconds: i64) -> String {
    let (year, month, day) = civil_from_days(seconds.div_euclid(86400));
    let rest = seconds.rem_euclid(86400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}Z",
        rest / 3600,
        rest / 60 % 60
    )
}

/// Howard Hinnant's algorithm for the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted = (month + 9) % 12;
    let day_of_year = (153 * shifted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146097 + day_of_era - 719468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719468;
    let era = days.div_euclid(146097);
    let day_of_era = days.rem_euclid(146097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted + 2) / 5 + 1;
    let month = if shifted < 10 {
        shifted + 3
    } else {
        shifted - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[derive(Clone, Debug, PartialEq)]
enum Lease {
    Until(i64),
    Unreadable,
}

#[derive(Clone, Debug)]
struct Status {
    id: u64,
    parked: bool,
    thread: Option<String>,
    branch: Option<String>,
    lease: Lease,
}

impl Status {
    fn parse(comment: &Comment) -> Option<Status> {
        let mut lines = comment.body.lines().map(str::trim);
        if lines.find(|line| !line.is_empty())? != STATUS_MARK {
            return None;
        }
        let mut status = Status {
            id: comment.id,
            parked: false,
            thread: None,
            branch: None,
            lease: Lease::Unreadable,
        };
        for line in lines {
            let Some((name, value)) = line
                .strip_prefix("**")
                .and_then(|rest| rest.split_once(":**"))
            else {
                continue;
            };
            let value = value.trim();
            match name {
                "State" => status.parked = value.eq_ignore_ascii_case("parked"),
                "Thread" => status.thread = (!value.is_empty()).then(|| value.to_string()),
                "Branch" => {
                    let branch = value.trim_matches('`');
                    status.branch = (!branch.is_empty()).then(|| branch.to_string());
                }
                "Lease until" => {
                    status.lease = seconds(value).map_or(Lease::Unreadable, Lease::Until);
                }
                _ => {}
            }
        }
        Some(status)
    }

    fn is_live(&self, now: i64) -> bool {
        !self.parked && self.lease.is_after(now)
    }
}

impl Lease {
    fn is_after(&self, now: i64) -> bool {
        matches!(self, Lease::Until(until) if *until > now)
    }

    fn order(&self) -> i64 {
        match self {
            Lease::Until(until) => *until,
            Lease::Unreadable => i64::MIN,
        }
    }
}

/// The status comment of the last claim: the one with the latest lease,
/// because every edit renews it.
fn latest(statuses: &[Status]) -> Option<&Status> {
    statuses.iter().max_by_key(|s| (s.lease.order(), s.id))
}

/// The status comment of the live claim on an issue, if it has one: the
/// lowest live one, on an open, unparked, `in-progress` issue.
fn live_claim<'a>(issue: &Issue, statuses: &'a [Status], now: i64) -> Option<&'a Status> {
    if !issue.open || is_parked(issue) || !has(issue, IN_PROGRESS) {
        return None;
    }
    statuses.iter().find(|status| status.is_live(now))
}

fn thread_of(status: &Status) -> &str {
    status.thread.as_deref().unwrap_or("unknown")
}

/// Why this thread may not prepare or merge the issue's PR, if it may not. An
/// issue with a live claim is the holder's alone. With none, anyone may.
pub fn refusal(issue: &Issue, owner: &str, now: i64, thread: Option<&str>) -> Option<String> {
    let statuses = statuses_by(issue, owner);
    let holder = thread_of(live_claim(issue, &statuses, now)?);
    if thread == Some(holder) {
        return None;
    }
    let number = issue.number;
    Some(match thread {
        Some(thread) => format!(
            "#{number} is held by thread `{holder}`, not `{thread}`, so only that thread can prepare or merge its PR"
        ),
        None => format!(
            "#{number} is held by thread `{holder}`, so only that thread can prepare or merge its PR, with `--thread`"
        ),
    })
}

fn touches(body: &str) -> Vec<String> {
    let mut lines = body.lines().map(str::trim);
    if !lines.any(|line| line == "## Touches") {
        return Vec::new();
    }
    let mut paths = Vec::new();
    for line in lines.take_while(|line| !line.starts_with("## ")) {
        paths.extend(
            line.split('`')
                .skip(1)
                .step_by(2)
                .filter(|path| !path.is_empty())
                .map(str::to_string),
        );
    }
    paths
}

/// The issue numbers on lines that start with this phrase.
fn references(body: &str, phrase: &str) -> Vec<u64> {
    body.lines()
        .filter_map(|line| line.trim().strip_prefix(phrase))
        .flat_map(|rest| rest.split('#').skip(1))
        .filter_map(|after| {
            let end = after
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(after.len());
            after[..end].parse().ok()
        })
        .collect()
}

fn overlap(first: &str, second: &str) -> bool {
    let parts = |path: &'_ str| -> Vec<String> {
        path.split('/')
            .filter(|part| !part.is_empty())
            .map(str::to_string)
            .collect()
    };
    let (first, second) = (parts(first), parts(second));
    first.iter().zip(&second).all(|(a, b)| a == b)
}

fn any_overlap(first: &[String], second: &[String]) -> bool {
    first.iter().any(|a| second.iter().any(|b| overlap(a, b)))
}

#[derive(Clone, Debug, PartialEq)]
enum State {
    Closed,
    Parked,
    Claimed,
    Unavailable,
    Blocked,
    Abandoned {
        branch: Option<String>,
    },
    /// `branch` is where an issue that comes back from parking continues.
    Available {
        branch: Option<String>,
    },
}

impl State {
    fn word(&self) -> &'static str {
        match self {
            State::Closed => "closed",
            State::Parked => "parked",
            State::Claimed => "claimed",
            State::Unavailable => "unavailable",
            State::Blocked => "blocked",
            State::Abandoned { .. } => "abandoned",
            State::Available { .. } => "available",
        }
    }

    fn claimable(&self) -> bool {
        matches!(self, State::Abandoned { .. } | State::Available { .. })
    }

    /// The branch a claim must continue on.
    fn branch(&self) -> Option<&String> {
        match self {
            State::Abandoned { branch } | State::Available { branch } => branch.as_ref(),
            _ => None,
        }
    }
}

/// Everything the states are worked out from.
struct View<'a> {
    forge: &'a dyn Forge,
    owner: String,
    now: i64,
    issues: Vec<Issue>,
    open: HashSet<u64>,
    parents: HashSet<u64>,
}

fn has(issue: &Issue, label: &str) -> bool {
    issue.labels.iter().any(|have| have == label)
}

fn is_parked(issue: &Issue) -> bool {
    PARKED.iter().any(|label| has(issue, label))
}

fn autonomy(issue: &Issue) -> Option<&'static str> {
    if has(issue, "auto:pr") {
        Some("auto:pr")
    } else if has(issue, "auto:merge") {
        Some("auto:merge")
    } else {
        None
    }
}

impl<'a> View<'a> {
    fn read(forge: &'a dyn Forge, now: i64) -> Result<View<'a>, Problem> {
        let owner = forge.owner()?;
        let mut issues = forge.issues()?;
        for issue in &mut issues {
            if issue.open && has(issue, IN_PROGRESS) && issue.comments.len() >= LISTED_COMMENTS {
                *issue = forge.issue(issue.number)?;
            }
        }
        let open = issues
            .iter()
            .filter(|issue| issue.open)
            .map(|issue| issue.number)
            .collect();
        let parents = issues.iter().filter_map(|issue| issue.parent).collect();
        Ok(View {
            forge,
            owner,
            now,
            issues,
            open,
            parents,
        })
    }

    fn statuses(&self, issue: &Issue) -> Vec<Status> {
        statuses_by(issue, &self.owner)
    }

    fn classify(&self, issue: &Issue) -> Result<State, Problem> {
        if !issue.open {
            return Ok(State::Closed);
        }
        if is_parked(issue) {
            return Ok(State::Parked);
        }
        let mut expired = None;
        if has(issue, IN_PROGRESS) {
            match self.holder(issue)? {
                Some(branch) => expired = Some(branch),
                None => return Ok(State::Claimed),
            }
        }
        if autonomy(issue).is_none()
            || touches(&issue.body).is_empty()
            || self.parents.contains(&issue.number)
        {
            return Ok(State::Unavailable);
        }
        if references(&issue.body, "Blocked by")
            .iter()
            .any(|number| self.open.contains(number))
        {
            return Ok(State::Blocked);
        }
        Ok(match expired {
            Some(branch) => State::Abandoned { branch },
            None => State::Available {
                branch: self.returning(issue)?,
            },
        })
    }

    /// The branch of the last claim, if GitHub still has it, so that an issue
    /// back from parking continues where its PR is.
    fn returning(&self, issue: &Issue) -> Result<Option<String>, Problem> {
        let statuses = self.statuses(issue);
        let Some(branch) = latest(&statuses).and_then(|s| s.branch.clone()) else {
            return Ok(None);
        };
        Ok(self.forge.branch_date(&branch)?.map(|_| branch))
    }

    /// For an `in-progress` issue: `None` if it's held, or `Some` with the
    /// branch that's still there if its lease expired and nothing was
    /// committed since. An issue with no lease that can be read stays held,
    /// because taking it could take someone's work.
    fn holder(&self, issue: &Issue) -> Result<Option<Option<String>>, Problem> {
        let mut statuses = self.statuses(issue);
        statuses.retain(|status| !status.parked);
        if statuses.is_empty() {
            eprintln!(
                "next: #{} is `{IN_PROGRESS}` with no status comment in use from the owner",
                issue.number
            );
            return Ok(None);
        }
        if statuses.iter().any(|s| s.lease == Lease::Unreadable) {
            eprintln!("next: #{} has a lease that can't be read", issue.number);
            return Ok(None);
        }
        if statuses.iter().any(|s| s.lease.is_after(self.now)) {
            return Ok(None);
        }
        let newest = latest(&statuses).expect("statuses isn't empty");
        let Some(branch) = &newest.branch else {
            return Ok(Some(None));
        };
        let Some(date) = self.forge.branch_date(branch)? else {
            return Ok(Some(None));
        };
        let date = seconds(&date)
            .ok_or_else(|| Problem(format!("couldn't read the date of `{branch}`: {date}")))?;
        if newest.lease.is_after(date) || newest.lease == Lease::Until(date) {
            Ok(Some(Some(branch.clone())))
        } else {
            Ok(None)
        }
    }

    /// The paths of every live claim, and how many there are.
    fn held(&self, states: &[(&Issue, State)]) -> (Vec<String>, usize) {
        let held: Vec<_> = states
            .iter()
            .filter(|(_, state)| *state == State::Claimed)
            .collect();
        let paths = held
            .iter()
            .flat_map(|(issue, _)| touches(&issue.body))
            .collect();
        (paths, held.len())
    }

    fn states(&self) -> Result<Vec<(&Issue, State)>, Problem> {
        let mut states = self
            .issues
            .iter()
            .filter(|issue| issue.open)
            .map(|issue| Ok((issue, self.classify(issue)?)))
            .collect::<Result<Vec<_>, Problem>>()?;
        states.sort_by_key(|(issue, _)| issue.number);
        Ok(states)
    }
}

fn statuses_by(issue: &Issue, owner: &str) -> Vec<Status> {
    let mut statuses: Vec<Status> = issue
        .comments
        .iter()
        .filter(|comment| comment.author == owner)
        .filter_map(Status::parse)
        .collect();
    statuses.sort_by_key(|status| status.id);
    statuses
}

#[derive(Debug, PartialEq)]
struct Entry {
    issue: u64,
    touches: Vec<String>,
    autonomy: &'static str,
    branch: Option<String>,
    worktree: Option<String>,
}

fn next(
    forge: &dyn Forge,
    now: i64,
    worktrees: &HashMap<String, String>,
) -> Result<Vec<Entry>, Problem> {
    let view = View::read(forge, now)?;
    let states = view.states()?;
    let (mut taken, held) = view.held(&states);
    let free = WORKER_SLOTS.saturating_sub(held);
    let mut entries = Vec::new();
    for (issue, state) in &states {
        if entries.len() == free {
            break;
        }
        if !state.claimable() {
            continue;
        }
        let paths = touches(&issue.body);
        if any_overlap(&paths, &taken) {
            continue;
        }
        let branch = state.branch().cloned();
        let worktree = branch.as_ref().and_then(|b| worktrees.get(b).cloned());
        taken.extend(paths.iter().cloned());
        entries.push(Entry {
            issue: issue.number,
            touches: paths,
            autonomy: autonomy(issue).expect("a claimable issue has an autonomy label"),
            branch,
            worktree,
        });
    }
    Ok(entries)
}

fn render_next(entries: &[Entry]) -> String {
    let optional = |value: &Option<String>| value.as_deref().map_or("null".to_string(), quoted);
    let items: Vec<String> = entries
        .iter()
        .map(|entry| {
            let touches: Vec<String> = entry.touches.iter().map(|p| quoted(p)).collect();
            format!(
                "{{\"issue\":{},\"touches\":[{}],\"autonomy\":{},\"branch\":{},\"worktree\":{}}}",
                entry.issue,
                touches.join(","),
                quoted(entry.autonomy),
                optional(&entry.branch),
                optional(&entry.worktree),
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// Which worktree holds each branch.
fn worktrees(root: &Path) -> Result<HashMap<String, String>, Problem> {
    let listing = base::git_text(root, &["worktree", "list", "--porcelain"])?;
    let mut found = HashMap::new();
    let mut path = None;
    for line in listing.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            path = Some(rest.to_string());
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/")
            && let Some(path) = &path
        {
            found.insert(branch.to_string(), path.clone());
        }
    }
    Ok(found)
}

pub fn now_seconds() -> Result<i64, Problem> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Problem(format!("the clock is before 1970: {e}")))?;
    Ok(elapsed.as_secs() as i64)
}

pub fn run_next() -> Result<Verdict, Problem> {
    let root = workspace::root()?;
    let entries = next(&Gh::new(&root), now_seconds()?, &worktrees(&root)?)?;
    println!("{}", render_next(&entries));
    Ok(Verdict::Pass)
}

#[derive(Debug, PartialEq)]
enum Claim {
    /// `replaced` is the thread that held the issue last, if it isn't this one.
    Won {
        comment: u64,
        replaced: Option<String>,
    },
    Lost(String),
}

fn render_status(thread: &str, branch: &str, lease: i64, nonce: &str) -> String {
    format!(
        "{STATUS_MARK}\n**State:** working\n**Thread:** {thread}\n**Branch:** `{branch}`\n**Lease until:** {}\n**Done:** nothing yet\n**Next:** read the issue\n**Decisions:** none yet\n<!-- claim {nonce} -->\n",
        stamp(lease)
    )
}

/// Claims an issue as AGENTS.md describes: write a status comment, label it,
/// and read it again. The comment comes first so that a claim that dies here
/// still has a lease that runs out. The nonce tells this claim from another
/// one that edited the same comment.
fn claim(
    forge: &dyn Forge,
    number: u64,
    branch: &str,
    thread: &str,
    now: i64,
    nonce: &str,
) -> Result<Claim, Problem> {
    let view = View::read(forge, now)?;
    let states = view.states()?;
    let Some((issue, state)) = states.iter().find(|(issue, _)| issue.number == number) else {
        return Ok(Claim::Lost(format!("#{number} isn't an open issue")));
    };
    if !state.claimable() {
        return Ok(Claim::Lost(format!("#{number} is {}", state.word())));
    }
    if let Some(kept) = state.branch()
        && kept != branch
    {
        return Ok(Claim::Lost(format!(
            "#{number} continues on `{kept}`, but this is `{branch}`"
        )));
    }
    let (held, _) = view.held(&states);
    if any_overlap(&touches(&issue.body), &held) {
        return Ok(Claim::Lost(format!(
            "#{number} overlaps an issue that is in progress"
        )));
    }

    let body = render_status(thread, branch, now + LEASE_SECONDS, nonce);
    let statuses = view.statuses(issue);
    let replaced = latest(&statuses)
        .and_then(|status| status.thread.clone())
        .filter(|last| last != thread);
    let earlier = statuses
        .into_iter()
        .find(|status| !status.is_live(now) && status.lease != Lease::Unreadable);
    let mine = match earlier {
        Some(status) => {
            forge.edit_comment(status.id, &body)?;
            status.id
        }
        None => forge.post_comment(number, &body)?,
    };
    forge.add_label(number, IN_PROGRESS)?;

    let after = forge.issue(number)?;
    let intact = after
        .comments
        .iter()
        .any(|comment| comment.id == mine && comment.body == body);
    let winner = statuses_by(&after, &view.owner)
        .into_iter()
        .find(|status| status.is_live(now));
    let eligible = after.open && !is_parked(&after);
    if intact && eligible && winner.is_some_and(|status| status.id == mine) {
        return Ok(Claim::Won {
            comment: mine,
            replaced,
        });
    }
    if intact {
        let retired = body.replace("**State:** working", "**State:** parked");
        forge.edit_comment(mine, &retired)?;
    }
    Ok(Claim::Lost(format!("another claim on #{number} won")))
}

/// Sets up a new worktree as `scripts/setup-worktree.sh` does, because a
/// worktree that `t3_thread_launch` or `git worktree add` makes skips
/// `t3.json`. Safe to repeat.
fn set_up(worktree: &Path) -> Result<(), Problem> {
    let common = base::git_text(
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let resolve = |path: &Path| {
        path.canonicalize()
            .map_err(|e| Problem(format!("couldn't resolve {}: {e}", path.display())))
    };
    let common = resolve(Path::new(common.trim()))?;
    let Some(main) = common.parent() else {
        return Ok(());
    };
    let here = resolve(worktree)?;
    let notes = main.join("notes");
    let link = here.join("notes");
    if main == here || !notes.is_dir() || fs::symlink_metadata(&link).is_ok() {
        return Ok(());
    }
    std::os::unix::fs::symlink(&notes, &link)
        .map_err(|e| Problem(format!("couldn't link {}: {e}", link.display())))
}

pub fn run_claim(number: &str, thread: Option<&str>) -> Result<Verdict, Problem> {
    let parsed: u64 = number
        .parse()
        .map_err(|_| Problem(format!("`claim` takes an issue number, but got `{number}`")))?;
    let root = workspace::root()?;
    let branch = base::git_text(&root, &["branch", "--show-current"])?;
    let branch = branch.trim();
    if branch.is_empty() || branch == DEFAULT_BRANCH {
        return Err(Problem(format!(
            "`claim` records the current branch, but this is `{branch}`. Check out the issue's branch"
        )));
    }
    set_up(&root)?;
    let _slot = lock::claim_slot(&root)?;
    let now = now_seconds()?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let nonce = format!("{}-{nanos}", std::process::id());
    let thread = thread.unwrap_or("unknown");
    match claim(&Gh::new(&root), parsed, branch, thread, now, &nonce)? {
        Claim::Won { comment, replaced } => {
            println!(
                "{{\"issue\":{parsed},\"comment\":{comment},\"branch\":{},\"replaced\":{}}}",
                quoted(branch),
                replaced.as_deref().map_or("null".to_string(), quoted)
            );
            Ok(Verdict::Pass)
        }
        Claim::Lost(reason) => {
            eprintln!("claim: {reason}");
            Ok(Verdict::Fail)
        }
    }
}

/// Replaces the status comment with `body`, for the thread that holds the
/// issue: the live claim's thread, or with none, the last claim's. It sets the
/// `Thread` and `Lease until` lines, so every update renews the lease. Gives
/// the comment's id, or why this thread may not.
fn update_status(
    forge: &dyn Forge,
    number: u64,
    thread: &str,
    body: &str,
    now: i64,
) -> Result<Result<u64, String>, Problem> {
    let shape = || {
        Problem(format!(
            "a status update is a status comment: `{STATUS_MARK}`, then a `**Thread:**` and a `**Lease until:**` line"
        ))
    };
    if Status::parse(&Comment {
        id: 0,
        author: String::new(),
        body: body.to_string(),
    })
    .is_none()
    {
        return Err(shape());
    }
    let mut thread_line = false;
    let mut lease_line = false;
    let lease = stamp(now + LEASE_SECONDS);
    let mut written: String = body
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("**Thread:**") {
                thread_line = true;
                format!("**Thread:** {thread}")
            } else if line.trim_start().starts_with("**Lease until:**") {
                lease_line = true;
                format!("**Lease until:** {lease}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    written.push('\n');
    if !thread_line || !lease_line {
        return Err(shape());
    }

    let issue = forge.issue(number)?;
    let statuses = statuses_by(&issue, &forge.owner()?);
    let Some(current) = live_claim(&issue, &statuses, now).or_else(|| latest(&statuses)) else {
        return Ok(Err(format!(
            "#{number} has no status comment. Claim it first"
        )));
    };
    let holder = thread_of(current);
    if holder != thread {
        return Ok(Err(format!(
            "#{number} is held by thread `{holder}`, not `{thread}`, so only that thread can update its status"
        )));
    }
    forge.edit_comment(current.id, &written)?;
    Ok(Ok(current.id))
}

pub fn run_status(number: &str, thread: &str) -> Result<Verdict, Problem> {
    let parsed: u64 = number.parse().map_err(|_| {
        Problem(format!(
            "`status` takes an issue number, but got `{number}`"
        ))
    })?;
    let mut body = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut body)
        .map_err(|e| Problem(format!("couldn't read the status from stdin: {e}")))?;
    let root = workspace::root()?;
    match update_status(&Gh::new(&root), parsed, thread, &body, now_seconds()?)? {
        Ok(comment) => {
            println!("{{\"issue\":{parsed},\"comment\":{comment}}}");
            Ok(Verdict::Pass)
        }
        Err(reason) => {
            eprintln!("status: {reason}");
            Ok(Verdict::Fail)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Claim, claim, next, overlap, refusal, seconds, set_up, stamp, update_status};
    use crate::Problem;
    use crate::forge::{Comment, Forge, Issue};
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    const OWNER: &str = "owner";
    const NOW: i64 = 60_000_000;
    const TOUCHES: &str = "## Touches\n`crates/a`\n";

    type Interference = Box<dyn Fn(&mut Vec<Issue>)>;

    /// An in-memory forge. `interfere` runs once, just before the claim reads
    /// the issue again, as another worker's writes would land.
    struct Fake {
        issues: RefCell<Vec<Issue>>,
        next_id: Cell<u64>,
        writes: RefCell<Vec<String>>,
        interfere: RefCell<Option<Interference>>,
        branch_date: Option<String>,
    }

    impl Fake {
        fn new(issues: Vec<Issue>) -> Fake {
            Fake {
                issues: RefCell::new(issues),
                next_id: Cell::new(100),
                writes: RefCell::new(Vec::new()),
                interfere: RefCell::new(None),
                branch_date: None,
            }
        }

        fn writes(&self) -> Vec<String> {
            self.writes.borrow().clone()
        }

        fn body_of(&self, id: u64) -> String {
            self.issues.borrow()[0]
                .comments
                .iter()
                .find(|comment| comment.id == id)
                .unwrap()
                .body
                .clone()
        }
    }

    impl Forge for Fake {
        fn owner(&self) -> Result<String, Problem> {
            Ok(OWNER.to_string())
        }

        fn issues(&self) -> Result<Vec<Issue>, Problem> {
            Ok(self.issues.borrow().clone())
        }

        fn issue(&self, number: u64) -> Result<Issue, Problem> {
            if let Some(interfere) = self.interfere.borrow_mut().take() {
                interfere(&mut self.issues.borrow_mut());
            }
            Ok(self
                .issues
                .borrow()
                .iter()
                .find(|issue| issue.number == number)
                .unwrap()
                .clone())
        }

        fn branch_date(&self, _branch: &str) -> Result<Option<String>, Problem> {
            Ok(self.branch_date.clone())
        }

        fn add_label(&self, number: u64, label: &str) -> Result<(), Problem> {
            self.writes
                .borrow_mut()
                .push(format!("label {number} {label}"));
            Ok(())
        }

        fn post_comment(&self, number: u64, body: &str) -> Result<u64, Problem> {
            let id = self.next_id.get();
            self.next_id.set(id + 1);
            self.writes.borrow_mut().push(format!("post {number}"));
            self.issues.borrow_mut()[0]
                .comments
                .push(comment(id, OWNER, body));
            Ok(id)
        }

        fn edit_comment(&self, id: u64, body: &str) -> Result<(), Problem> {
            self.writes.borrow_mut().push(format!("edit {id}"));
            for issue in self.issues.borrow_mut().iter_mut() {
                for comment in &mut issue.comments {
                    if comment.id == id {
                        comment.body = body.to_string();
                    }
                }
            }
            Ok(())
        }
    }

    fn comment(id: u64, author: &str, body: &str) -> Comment {
        Comment {
            id,
            author: author.to_string(),
            body: body.to_string(),
        }
    }

    fn status(state: &str, lease: i64) -> String {
        format!(
            "<!-- status -->\n**State:** {state}\n**Branch:** `b`\n**Lease until:** {}\n",
            stamp(lease)
        )
    }

    fn issue(comments: Vec<Comment>) -> Issue {
        Issue {
            number: 7,
            open: true,
            body: TOUCHES.to_string(),
            labels: vec!["auto:merge".to_string()],
            comments,
            parent: None,
        }
    }

    fn try_claim(forge: &Fake) -> Claim {
        try_claim_on(forge, "7-x")
    }

    fn try_claim_on(forge: &Fake, branch: &str) -> Claim {
        claim(forge, 7, branch, "thread", NOW, "n")
            .map_err(|e| e.0)
            .unwrap()
    }

    #[test]
    fn a_stamp_reads_back_as_the_same_minute() {
        for text in [
            "1970-01-01T00:00Z",
            "2024-02-29T23:59Z",
            "2026-11-09T14:00Z",
            "2100-03-01T00:01Z",
        ] {
            assert_eq!(stamp(seconds(text).unwrap()), text);
        }
        assert_eq!(seconds("1970-01-01T01:00Z"), Some(3600));
        assert_eq!(seconds("1970-01-02T00:00:59Z"), Some(86459));
    }

    #[test]
    fn a_bad_stamp_has_no_seconds() {
        for text in [
            "",
            "tomorrow",
            "2026-11-09",
            "2026-13-09T14:00Z",
            "2026-11-09T25:00Z",
            "2026-11-09T14:00",
            "2026-11-09-01T14:00Z",
            "2026-11-09T14:00:60Z",
            "2026-11-09T14:00:00:00Z",
        ] {
            assert_eq!(seconds(text), None, "{text}");
        }
    }

    #[test]
    fn overlap_compares_whole_path_parts() {
        assert!(overlap("a/b", "a/b/c"));
        assert!(overlap("a/", "a"));
        assert!(!overlap("a/bc", "a/b"));
    }

    #[test]
    fn a_first_claim_labels_and_posts_a_full_status() {
        let forge = Fake::new(vec![issue(vec![])]);
        assert_eq!(
            try_claim(&forge),
            Claim::Won {
                comment: 100,
                replaced: None
            }
        );
        assert_eq!(forge.writes(), ["post 7", "label 7 in-progress"]);
        let body = forge.body_of(100);
        for line in [
            "<!-- status -->\n",
            "**State:** working\n",
            "**Thread:** thread\n",
            "**Branch:** `7-x`\n",
            "**Done:** ",
            "**Next:** ",
            "**Decisions:** ",
        ] {
            assert!(body.contains(line), "{line}: {body}");
        }
        let lease = format!("**Lease until:** {}\n", stamp(NOW + 12 * 3600));
        assert!(body.contains(&lease), "{body}");
    }

    #[test]
    fn an_earlier_parked_or_expired_comment_is_edited() {
        for (state, lease) in [("parked", NOW + 3600), ("working", NOW - 60)] {
            let earlier = comment(40, OWNER, &status(state, lease));
            let forge = Fake::new(vec![issue(vec![earlier])]);
            assert_eq!(
                try_claim(&forge),
                Claim::Won {
                    comment: 40,
                    replaced: None
                }
            );
            assert_eq!(forge.writes(), ["edit 40", "label 7 in-progress"]);
        }
    }

    fn rival(id: u64, author: &'static str) -> Interference {
        Box::new(move |issues| {
            issues[0]
                .comments
                .push(comment(id, author, &status("working", NOW + 3600)));
        })
    }

    #[test]
    fn claim_race_table() {
        struct Case {
            name: &'static str,
            interfere: Option<Interference>,
            won: bool,
            retired: bool,
        }
        for case in [
            Case {
                name: "alone",
                interfere: None,
                won: true,
                retired: false,
            },
            Case {
                name: "a lower live comment wins",
                interfere: Some(rival(50, OWNER)),
                won: false,
                retired: true,
            },
            Case {
                name: "a higher live comment loses",
                interfere: Some(rival(900, OWNER)),
                won: true,
                retired: false,
            },
            Case {
                name: "a stranger can't win",
                interfere: Some(rival(50, "stranger")),
                won: true,
                retired: false,
            },
            Case {
                name: "overwritten by another claim",
                interfere: Some(Box::new(|issues| {
                    issues[0].comments[0].body = status("working", NOW + 7200);
                })),
                won: false,
                retired: false,
            },
            Case {
                name: "parked to land while claiming",
                interfere: Some(Box::new(|issues| {
                    issues[0].labels.push("land-needed".to_string());
                })),
                won: false,
                retired: true,
            },
            Case {
                name: "parked on a question while claiming",
                interfere: Some(Box::new(|issues| {
                    issues[0].labels.push("answer-needed".to_string());
                })),
                won: false,
                retired: true,
            },
            Case {
                name: "closed while claiming",
                interfere: Some(Box::new(|issues| issues[0].open = false)),
                won: false,
                retired: true,
            },
            Case {
                name: "deleted by someone",
                interfere: Some(Box::new(|issues| issues[0].comments.clear())),
                won: false,
                retired: false,
            },
        ] {
            let forge = Fake::new(vec![issue(vec![])]);
            *forge.interfere.borrow_mut() = case.interfere;
            let result = try_claim(&forge);
            assert_eq!(
                matches!(result, Claim::Won { .. }),
                case.won,
                "{}",
                case.name
            );
            assert_eq!(
                forge.writes().contains(&"edit 100".to_string()),
                case.retired,
                "{}",
                case.name
            );
            if case.retired {
                assert!(
                    forge.body_of(100).contains("**State:** parked"),
                    "{}",
                    case.name
                );
            }
        }
    }

    #[test]
    fn a_takeover_must_be_on_the_abandoned_branch() {
        let taken = || {
            let mut held = issue(vec![comment(40, OWNER, &status("working", NOW - 600))]);
            held.labels.push("in-progress".to_string());
            let mut forge = Fake::new(vec![held]);
            forge.branch_date = Some(stamp(NOW - 1200));
            forge
        };
        let wrong = taken();
        assert!(matches!(try_claim_on(&wrong, "7-x"), Claim::Lost(_)));
        assert_eq!(wrong.writes(), Vec::<String>::new());
        let right = taken();
        assert_eq!(
            try_claim_on(&right, "b"),
            Claim::Won {
                comment: 40,
                replaced: None
            }
        );
    }

    #[test]
    fn a_retired_comment_is_no_longer_live() {
        let forge = Fake::new(vec![issue(vec![])]);
        *forge.interfere.borrow_mut() = Some(rival(50, OWNER));
        try_claim(&forge);
        let retired = forge.body_of(100);
        let parsed = super::Status::parse(&comment(100, OWNER, &retired)).unwrap();
        assert!(!parsed.is_live(NOW));
    }

    #[test]
    fn an_issue_that_can_not_be_claimed_gets_no_writes() {
        let mut parked = issue(vec![]);
        parked.labels.push("land-needed".to_string());
        let mut live = issue(vec![comment(40, OWNER, &status("working", NOW + 3600))]);
        live.labels.push("in-progress".to_string());
        let mut blocked = issue(vec![]);
        blocked.body.push_str("\nBlocked by #8.\n");
        let blocker = Issue {
            number: 8,
            body: String::new(),
            labels: vec![],
            ..issue(vec![])
        };
        let mut other = issue(vec![comment(40, OWNER, &status("working", NOW + 3600))]);
        other.number = 2;
        other.labels.push("in-progress".to_string());
        for issues in [
            vec![parked],
            vec![live],
            vec![blocked, blocker],
            vec![issue(vec![]), other],
            vec![],
        ] {
            let forge = Fake::new(issues);
            assert!(matches!(try_claim(&forge), Claim::Lost(_)));
            assert_eq!(forge.writes(), Vec::<String>::new());
        }
    }

    fn status_of(state: &str, lease: i64, thread: &str) -> String {
        format!(
            "<!-- status -->\n**State:** {state}\n**Thread:** {thread}\n**Branch:** `b`\n**Lease until:** {}\n**Done:** x\n",
            stamp(lease)
        )
    }

    fn held_by(thread: &str) -> Issue {
        let mut held = issue(vec![comment(
            40,
            OWNER,
            &status_of("working", NOW + 3600, thread),
        )]);
        held.labels.push("in-progress".to_string());
        held
    }

    /// #121 on 2026-11-11: parked on a question, answered, and dispatched
    /// again, which must continue on the branch its PR is on.
    #[test]
    fn a_returning_issue_continues_on_its_branch() {
        let mut parked = issue(vec![comment(
            40,
            OWNER,
            &status_of("parked", NOW - 3600, "old"),
        )]);
        parked.labels.push("answer-needed".to_string());
        let mut forge = Fake::new(vec![parked]);
        forge.branch_date = Some(stamp(NOW - 7200));
        assert_eq!(next(&forge, NOW, &HashMap::new()).unwrap(), []);

        forge.issues.borrow_mut()[0]
            .labels
            .retain(|l| l != "answer-needed");
        let worktrees = HashMap::from([("b".to_string(), "/w/b".to_string())]);
        let entries = next(&forge, NOW, &worktrees).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].branch.as_deref(), Some("b"));
        assert_eq!(entries[0].worktree.as_deref(), Some("/w/b"));

        assert!(matches!(try_claim_on(&forge, "7-x"), Claim::Lost(_)));
        assert_eq!(forge.writes(), Vec::<String>::new());
        assert_eq!(
            try_claim_on(&forge, "b"),
            Claim::Won {
                comment: 40,
                replaced: Some("old".to_string())
            }
        );
    }

    #[test]
    fn a_returning_issue_whose_branch_is_gone_starts_fresh() {
        let forge = Fake::new(vec![issue(vec![comment(
            40,
            OWNER,
            &status_of("parked", NOW - 3600, "old"),
        )])]);
        let entries = next(&forge, NOW, &HashMap::new()).unwrap();
        assert_eq!(entries[0].branch, None);
        assert!(matches!(try_claim_on(&forge, "7-x"), Claim::Won { .. }));
    }

    #[test]
    fn a_claim_replaces_no_thread_when_it_is_the_first_or_the_same() {
        let forge = Fake::new(vec![issue(vec![])]);
        assert_eq!(
            try_claim(&forge),
            Claim::Won {
                comment: 100,
                replaced: None
            }
        );
        let forge = Fake::new(vec![issue(vec![comment(
            40,
            OWNER,
            &status_of("parked", NOW - 3600, "thread"),
        )])]);
        assert_eq!(
            try_claim(&forge),
            Claim::Won {
                comment: 40,
                replaced: None
            }
        );
    }

    #[test]
    fn only_the_holder_passes_a_live_claim() {
        let held = held_by("t");
        assert_eq!(refusal(&held, OWNER, NOW, Some("t")), None);
        for thread in [Some("u"), None] {
            let why = refusal(&held, OWNER, NOW, thread).unwrap();
            assert!(why.contains("`t`"), "{why}");
        }
    }

    #[test]
    fn with_no_live_claim_anyone_passes() {
        let mut parked = held_by("t");
        parked.labels.push("land-needed".to_string());
        let mut unlabeled = held_by("t");
        unlabeled.labels.clear();
        let mut closed = held_by("t");
        closed.open = false;
        let mut expired = held_by("t");
        expired.comments[0].body = status_of("working", NOW - 60, "t");
        let mut retired = held_by("t");
        retired.comments[0].body = status_of("parked", NOW + 3600, "t");
        let stranger = {
            let mut held = held_by("t");
            held.comments[0].author = "stranger".to_string();
            held
        };
        for issue in [parked, unlabeled, closed, expired, retired, stranger] {
            for thread in [Some("t"), Some("u"), None] {
                assert_eq!(refusal(&issue, OWNER, NOW, thread), None);
            }
        }
    }

    #[test]
    fn the_lowest_live_status_holds_the_claim() {
        let mut held = held_by("t");
        held.comments.push(comment(
            30,
            OWNER,
            &status_of("working", NOW + 3600, "first"),
        ));
        assert_eq!(refusal(&held, OWNER, NOW, Some("first")), None);
        assert!(refusal(&held, OWNER, NOW, Some("t")).is_some());
    }

    fn update(forge: &Fake, thread: &str, body: &str) -> Result<u64, String> {
        update_status(forge, 7, thread, body, NOW).unwrap()
    }

    #[test]
    fn the_holder_updates_its_status_and_renews_the_lease() {
        let forge = Fake::new(vec![held_by("t")]);
        let body = status_of("working", NOW - 99_999, "t").replace("x\n", "tests pass\n");
        assert_eq!(update(&forge, "t", &body), Ok(40));
        assert_eq!(forge.writes(), ["edit 40"]);
        let written = forge.body_of(40);
        assert!(written.contains("**Done:** tests pass\n"), "{written}");
        let lease = format!("**Lease until:** {}\n", stamp(NOW + 12 * 3600));
        assert!(written.contains(&lease), "{written}");
    }

    #[test]
    fn another_thread_can_not_update_the_status() {
        let forge = Fake::new(vec![held_by("t")]);
        let why = update(&forge, "u", &status_of("working", NOW, "u")).unwrap_err();
        assert!(why.contains("`t`"), "{why}");
        assert_eq!(forge.writes(), Vec::<String>::new());
    }

    #[test]
    fn the_last_holder_updates_a_parked_or_closed_issue() {
        let mut closed = held_by("t");
        closed.open = false;
        closed.comments[0].body = status_of("parked", NOW - 60, "t");
        let forge = Fake::new(vec![closed]);
        let body = status_of("parked", NOW, "t");
        assert!(update(&forge, "u", &body).is_err());
        assert_eq!(update(&forge, "t", &body), Ok(40));
    }

    #[test]
    fn a_status_update_must_be_a_status_comment() {
        let forge = Fake::new(vec![held_by("t")]);
        for body in ["just words\n", "<!-- status -->\n**State:** working\n"] {
            assert!(update_status(&forge, 7, "t", body, NOW).is_err(), "{body}");
        }
        let empty = Fake::new(vec![issue(vec![])]);
        assert!(update(&empty, "t", &status_of("working", NOW, "t")).is_err());
        assert_eq!(forge.writes(), Vec::<String>::new());
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(["-c", "user.name=x", "-c", "user.email=x@x"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// A main checkout with one commit, and a new worktree of it.
    fn checkout(name: &str) -> (PathBuf, PathBuf) {
        let top = std::env::temp_dir().join(format!("xtask-work-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&top);
        let main = top.join("main");
        fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "x"]);
        git(&main, &["worktree", "add", "-q", "-b", "7-x", "../7-x"]);
        (main, top.join("7-x"))
    }

    #[test]
    fn claim_links_notes_from_the_main_checkout_when_it_has_them() {
        let (main, worktree) = checkout("notes");
        fs::create_dir(main.join("notes")).unwrap();
        set_up(&worktree).unwrap();
        let link = worktree.join("notes");
        assert_eq!(
            fs::read_link(&link).unwrap(),
            main.canonicalize().unwrap().join("notes")
        );
        set_up(&worktree).unwrap();
        assert_eq!(
            fs::read_link(&link).unwrap(),
            main.canonicalize().unwrap().join("notes")
        );
        fs::remove_dir_all(main.parent().unwrap()).unwrap();
    }

    #[test]
    fn claim_links_no_notes_when_the_main_checkout_has_none() {
        let (main, worktree) = checkout("none");
        set_up(&worktree).unwrap();
        set_up(&worktree).unwrap();
        assert!(fs::symlink_metadata(worktree.join("notes")).is_err());
        assert!(fs::symlink_metadata(main.join("notes")).is_err());
        fs::remove_dir_all(main.parent().unwrap()).unwrap();
    }

    #[test]
    fn claim_keeps_notes_that_are_already_there() {
        let (main, worktree) = checkout("kept");
        fs::create_dir(main.join("notes")).unwrap();
        fs::create_dir(worktree.join("notes")).unwrap();
        set_up(&worktree).unwrap();
        assert!(
            fs::symlink_metadata(worktree.join("notes"))
                .unwrap()
                .is_dir()
        );
        fs::remove_dir_all(main.parent().unwrap()).unwrap();
    }

    #[test]
    fn claim_links_no_notes_in_the_main_checkout_itself() {
        let (main, _) = checkout("itself");
        fs::create_dir(main.join("notes")).unwrap();
        set_up(&main).unwrap();
        assert!(fs::symlink_metadata(main.join("notes")).unwrap().is_dir());
        fs::remove_dir_all(main.parent().unwrap()).unwrap();
    }
}
