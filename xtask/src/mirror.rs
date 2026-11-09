//! `cargo xtask mirror`: keeps a parking label the same on an issue and on the
//! open PR that closes it.
//!
//! It copies what the side that changed says now, not what the event said, so
//! a run that starts late still ends on the newest state, and a rerun is safe.

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
    let changed = gh.labeled(number)?;
    let others = if changed.is_pr {
        let pr = gh.pull_request(number)?;
        if pr.state == "OPEN" {
            pr.closing_issues
        } else {
            Vec::new()
        }
    } else {
        gh.open_prs_closing(number)?
    };
    let present = changed.labels.iter().any(|name| name == label);
    for other in &others {
        if present {
            gh.apply_label(*other, label)?;
        } else {
            gh.remove_label(*other, label)?;
        }
    }
    println!(
        "mirror: {number} {} `{label}`, copied to {others:?}",
        if present { "has" } else { "lacks" }
    );
    Ok(Verdict::Pass)
}
