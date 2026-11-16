//! `ap discard <agent | run>`, from SPEC.md's `SHARE` section. It throws a
//! finished run's pending changes away, and refuses while the run is
//! active, whose share server may still be writing them.

use store::Runs;

use super::diff::resolve;

/// Discards the run's changes, and returns the exit status.
pub fn run(name: &str) -> u8 {
    match discard(name) {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("ap: {message}");
            1
        }
    }
}

fn discard(name: &str) -> Result<(), String> {
    let (root, run) = resolve(name)?;
    let Some(run) = run else {
        return Ok(());
    };
    let runs = Runs::new(&root);
    if runs.active(&run).map_err(|error| error.to_string())? {
        return Err(format!(
            "run {run} is active, so its changes stay. Stop it with `ap stop {run}` first"
        ));
    }
    runs.discard(&run)
        .map_err(|error| format!("can't discard the changes of run {run}: {error}"))
}
