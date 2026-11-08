//! Which issues can be taken, and taking one. HARNESS.md defines the states.

use crate::forge::{Comment, Forge, Gh, Issue};
use crate::json::quoted;
use crate::{Problem, Verdict, base, workspace};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const WORKER_SLOTS: usize = 3;
const LEASE_MINUTES: i64 = 12 * 60;
const IN_PROGRESS: &str = "in-progress";
const NEEDS_OWNER: &str = "needs-owner";
const STATUS_MARK: &str = "<!-- status -->";
const DEFAULT_BRANCH: &str = "main";

/// Minutes since 1970, from `2026-11-09T14:00Z`, with or without seconds.
fn minutes(stamp: &str) -> Option<i64> {
    let (date, time) = stamp.strip_suffix('Z')?.split_once('T')?;
    let mut date = date.split('-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let mut time = time.split(':').map(str::parse::<i64>);
    let (hour, minute) = (time.next()?.ok()?, time.next()?.ok()?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || date.next().is_some()
    {
        return None;
    }
    Some(days_from_civil(year, month, day) * 1440 + hour * 60 + minute)
}

fn stamp(minutes: i64) -> String {
    let (year, month, day) = civil_from_days(minutes.div_euclid(1440));
    let rest = minutes.rem_euclid(1440);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}Z",
        rest / 60,
        rest % 60
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
                "Branch" => {
                    let branch = value.trim_matches('`');
                    status.branch = (!branch.is_empty()).then(|| branch.to_string());
                }
                "Lease until" => {
                    status.lease = minutes(value).map_or(Lease::Unreadable, Lease::Until);
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
    Abandoned { branch: Option<String> },
    Available,
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
            State::Available => "available",
        }
    }

    fn claimable(&self) -> bool {
        matches!(self, State::Abandoned { .. } | State::Available)
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
        let issues = forge.issues()?;
        let open = issues
            .iter()
            .filter(|issue| issue.open)
            .map(|issue| issue.number)
            .collect();
        let parents = issues
            .iter()
            .flat_map(|issue| references(&issue.body, "Part of"))
            .collect();
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
        if has(issue, NEEDS_OWNER) {
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
            None => State::Available,
        })
    }

    /// For an `in-progress` issue: `None` if it's held, or `Some` with the
    /// branch that's still there if its lease expired and nothing was
    /// committed since. An issue with no lease that can be read stays held,
    /// because taking it could take someone's work.
    fn holder(&self, issue: &Issue) -> Result<Option<Option<String>>, Problem> {
        let statuses = self.statuses(issue);
        if statuses.is_empty() {
            eprintln!(
                "next: #{} is `{IN_PROGRESS}` with no status comment from the owner",
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
        let newest = statuses
            .iter()
            .max_by_key(|s| match s.lease {
                Lease::Until(until) => until,
                Lease::Unreadable => i64::MIN,
            })
            .expect("statuses isn't empty");
        let Some(branch) = &newest.branch else {
            return Ok(Some(None));
        };
        let Some(date) = self.forge.branch_date(branch)? else {
            return Ok(Some(None));
        };
        let date = minutes(&date)
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
        let branch = match state {
            State::Abandoned { branch } => branch.clone(),
            _ => None,
        };
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

fn now_minutes() -> Result<i64, Problem> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Problem(format!("the clock is before 1970: {e}")))?;
    Ok((elapsed.as_secs() / 60) as i64)
}

pub fn run_next() -> Result<Verdict, Problem> {
    let root = workspace::root()?;
    let entries = next(&Gh::new(&root), now_minutes()?, &worktrees(&root)?)?;
    println!("{}", render_next(&entries));
    Ok(Verdict::Pass)
}

#[derive(Debug, PartialEq)]
enum Claim {
    Won { comment: u64 },
    Lost(String),
}

fn render_status(thread: &str, branch: &str, lease: i64, nonce: &str) -> String {
    format!(
        "{STATUS_MARK}\n**State:** working\n**Thread:** {thread}\n**Branch:** `{branch}`\n**Lease until:** {}\n**Done:** nothing yet\n**Next:** read the issue\n**Decisions:** none yet\n<!-- claim {nonce} -->\n",
        stamp(lease)
    )
}

/// Claims an issue as AGENTS.md describes: label it, write a status comment,
/// and read it again. The nonce tells this claim from another one that edited
/// the same comment.
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
    let (held, _) = view.held(&states);
    if any_overlap(&touches(&issue.body), &held) {
        return Ok(Claim::Lost(format!(
            "#{number} overlaps an issue that is in progress"
        )));
    }

    forge.add_label(number, IN_PROGRESS)?;
    let body = render_status(thread, branch, now + LEASE_MINUTES, nonce);
    let earlier = view
        .statuses(issue)
        .into_iter()
        .find(|status| !status.is_live(now) && status.lease != Lease::Unreadable);
    let mine = match earlier {
        Some(status) => {
            forge.edit_comment(status.id, &body)?;
            status.id
        }
        None => forge.post_comment(number, &body)?,
    };

    let after = forge.issue(number)?;
    let intact = after
        .comments
        .iter()
        .any(|comment| comment.id == mine && comment.body == body);
    let winner = statuses_by(&after, &view.owner)
        .into_iter()
        .find(|status| status.is_live(now));
    if intact && winner.is_some_and(|status| status.id == mine) {
        return Ok(Claim::Won { comment: mine });
    }
    if intact {
        let retired = body.replace("**State:** working", "**State:** parked");
        forge.edit_comment(mine, &retired)?;
    }
    Ok(Claim::Lost(format!("another claim on #{number} won")))
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
    let now = now_minutes()?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let nonce = format!("{}-{nanos}", std::process::id());
    let thread = thread.unwrap_or("unknown");
    match claim(&Gh::new(&root), parsed, branch, thread, now, &nonce)? {
        Claim::Won { comment } => {
            println!(
                "{{\"issue\":{parsed},\"comment\":{comment},\"branch\":{}}}",
                quoted(branch)
            );
            Ok(Verdict::Pass)
        }
        Claim::Lost(reason) => {
            eprintln!("claim: {reason}");
            Ok(Verdict::Fail)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Claim, claim, minutes, overlap, stamp};
    use crate::Problem;
    use crate::forge::{Comment, Forge, Issue};
    use std::cell::{Cell, RefCell};

    const OWNER: &str = "owner";
    const NOW: i64 = 1_000_000;
    const TOUCHES: &str = "## Touches\n`crates/a`\n";

    type Interference = Box<dyn Fn(&mut Vec<Issue>)>;

    /// An in-memory forge. `interfere` runs once, just before the claim reads
    /// the issue again, as another worker's writes would land.
    struct Fake {
        issues: RefCell<Vec<Issue>>,
        next_id: Cell<u64>,
        writes: RefCell<Vec<String>>,
        interfere: RefCell<Option<Interference>>,
    }

    impl Fake {
        fn new(issues: Vec<Issue>) -> Fake {
            Fake {
                issues: RefCell::new(issues),
                next_id: Cell::new(100),
                writes: RefCell::new(Vec::new()),
                interfere: RefCell::new(None),
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
            Ok(None)
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
        }
    }

    fn try_claim(forge: &Fake) -> Claim {
        claim(forge, 7, "7-x", "thread", NOW, "n")
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
            assert_eq!(stamp(minutes(text).unwrap()), text);
        }
        assert_eq!(minutes("1970-01-01T01:00Z"), Some(60));
        assert_eq!(minutes("1970-01-02T00:00:59Z"), Some(1440));
    }

    #[test]
    fn a_bad_stamp_has_no_minutes() {
        for text in [
            "",
            "tomorrow",
            "2026-11-09",
            "2026-13-09T14:00Z",
            "2026-11-09T25:00Z",
            "2026-11-09T14:00",
            "2026-11-09-01T14:00Z",
        ] {
            assert_eq!(minutes(text), None, "{text}");
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
        assert_eq!(try_claim(&forge), Claim::Won { comment: 100 });
        assert_eq!(forge.writes(), ["label 7 in-progress", "post 7"]);
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
        let lease = format!("**Lease until:** {}\n", stamp(NOW + 720));
        assert!(body.contains(&lease), "{body}");
    }

    #[test]
    fn an_earlier_parked_or_expired_comment_is_edited() {
        for (state, lease) in [("parked", NOW + 500), ("working", NOW - 1)] {
            let earlier = comment(40, OWNER, &status(state, lease));
            let forge = Fake::new(vec![issue(vec![earlier])]);
            assert_eq!(try_claim(&forge), Claim::Won { comment: 40 });
            assert_eq!(forge.writes(), ["label 7 in-progress", "edit 40"]);
        }
    }

    fn rival(id: u64, author: &'static str) -> Interference {
        Box::new(move |issues| {
            issues[0]
                .comments
                .push(comment(id, author, &status("working", NOW + 100)));
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
                    issues[0].comments[0].body = status("working", NOW + 700);
                })),
                won: false,
                retired: false,
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
        parked.labels.push("needs-owner".to_string());
        let mut live = issue(vec![comment(40, OWNER, &status("working", NOW + 5))]);
        live.labels.push("in-progress".to_string());
        let mut blocked = issue(vec![]);
        blocked.body.push_str("\nBlocked by #8.\n");
        let blocker = Issue {
            number: 8,
            body: String::new(),
            labels: vec![],
            ..issue(vec![])
        };
        let mut other = issue(vec![comment(40, OWNER, &status("working", NOW + 5))]);
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
}
