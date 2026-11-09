//! `cargo xtask mirror`: keeps a parking label the same on an issue and on the
//! open PR that closes it.
//!
//! It finds the newest change a person made to the label on either side, and
//! applies it to both. Runs can start late, out of order, or again, and each
//! one still ends on the newest change.

use crate::forge::Gh;
use crate::work::PARKED;
use crate::{Problem, Verdict, workspace};

pub fn run(number: &str, label: &str) -> Result<Verdict, Problem> {
    let number: u64 = number
        .parse()
        .map_err(|_| Problem(format!("`mirror` takes a number, but got `{number}`")))?;
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
    let items: Vec<u64> = std::iter::once(number).chain(others).collect();
    let mut newest: Option<(u64, bool)> = None;
    for item in &items {
        for change in gh.label_changes(*item, label)? {
            if newest.is_none_or(|(id, _)| change.0 > id) {
                newest = Some(change);
            }
        }
    }
    let Some((_, present)) = newest else {
        println!("mirror: no person has changed `{label}` on {items:?}");
        return Ok(Verdict::Pass);
    };
    for item in &items {
        if present {
            gh.apply_label(*item, label)?;
        } else {
            gh.remove_label(*item, label)?;
        }
    }
    println!(
        "mirror: {} `{label}` on {items:?}",
        if present { "applied" } else { "removed" }
    );
    Ok(Verdict::Pass)
}
