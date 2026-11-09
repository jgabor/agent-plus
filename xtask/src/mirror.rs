//! `cargo xtask mirror`: keeps a parking label the same on an issue and on the
//! open PR that closes it.
//!
//! It applies the event's change to both sides, not just the other one. Runs
//! for one label on one issue go one at a time, so the last run carries the
//! newest change, and it overwrites whatever an older run copied.

use crate::forge::Gh;
use crate::work::PARKED;
use crate::{Problem, Verdict, workspace};

pub fn run(number: &str, label: &str, action: &str) -> Result<Verdict, Problem> {
    let number: u64 = number
        .parse()
        .map_err(|_| Problem(format!("`mirror` takes a number, but got `{number}`")))?;
    let present = match action {
        "labeled" => true,
        "unlabeled" => false,
        _ => {
            return Err(Problem(format!(
                "`mirror` takes `labeled` or `unlabeled`, but got `{action}`"
            )));
        }
    };
    if !PARKED.contains(&label) {
        println!("mirror: `{label}` isn't mirrored");
        return Ok(Verdict::Pass);
    }
    let gh = Gh::new(&workspace::root()?);
    let others = if gh.is_pr(number)? {
        let pr = gh.pull_request(number)?;
        if pr.state == "OPEN" {
            pr.closing_issues
        } else {
            Vec::new()
        }
    } else {
        gh.open_prs_closing(number)?
    };
    if others.is_empty() {
        println!("mirror: {number} has no other side for `{label}`");
        return Ok(Verdict::Pass);
    }
    for item in std::iter::once(number).chain(others.iter().copied()) {
        if present {
            gh.apply_label(item, label)?;
        } else {
            gh.remove_label(item, label)?;
        }
    }
    println!(
        "mirror: {} `{label}` on {number} and {others:?}",
        if present { "applied" } else { "removed" }
    );
    Ok(Verdict::Pass)
}
