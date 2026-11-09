//! `cargo xtask mirror`: keeps a parking label the same on an issue and on the
//! open PR that closes it.
//!
//! It finds the newest change a person made to the label on either side, or
//! on a PR for the issue that has closed, and applies it to both. Runs can start late, out of order, or again, and each
//! one still ends on the newest change.
//!
//! A merged PR waits on nobody, so a run for one removes `land-needed` from
//! it and from the issues it closed, whoever changed the label last.

use crate::forge::Gh;
use crate::work::{LAND_NEEDED, PARKED};
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
    let is_pr = gh.is_pr(number)?;
    let others = if is_pr {
        let pr = gh.pull_request(number)?;
        if pr.state == "MERGED" && label == LAND_NEEDED {
            let items: Vec<u64> = std::iter::once(number).chain(pr.closing_issues).collect();
            for item in &items {
                gh.remove_label(*item, label)?;
            }
            println!("mirror: {number} merged, so removed `{label}` from {items:?}");
            return Ok(Verdict::Pass);
        }
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
    // A PR that closed without merging keeps the changes people made on it,
    // and a rerun of an old run has to see them.
    let mut read = items.clone();
    if !is_pr {
        let linked = gh.prs_linked(number)?.into_iter();
        read.extend(linked.filter(|pr| !items.contains(pr)));
    }
    let mut newest: Option<(u64, bool)> = None;
    for item in &read {
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
    // The PR can merge after it was read, and its merge run can finish before
    // the writes above. Reading it again after them closes that window.
    if present && label == LAND_NEEDED {
        let prs = if is_pr { &items[..1] } else { &items[1..] };
        for pr in prs {
            if gh.pull_request(*pr)?.state == "MERGED" {
                for item in &items {
                    gh.remove_label(*item, label)?;
                }
                println!("mirror: {pr} merged meanwhile, so removed `{label}` from {items:?}");
                break;
            }
        }
    }
    Ok(Verdict::Pass)
}
