//! `cargo xtask score`: the scores of the current tree.

use crate::gate::workspace_root;
use crate::{Problem, Verdict, spec};
use std::fs;

pub fn run() -> Result<Verdict, Problem> {
    let root = workspace_root()?;
    let text = fs::read_to_string(root.join("SPEC.md"))
        .map_err(|e| Problem(format!("couldn't read SPEC.md: {e}")))?;
    let units = spec::extract(&text).map_err(|e| Problem(format!("SPEC.md: {e}")))?;
    println!("spec 0 of {}", units.len());
    Ok(Verdict::Pass)
}
