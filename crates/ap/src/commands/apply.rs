//! `ap apply <agent | run>`, from SPEC.md's `SHARE` section. It lands a
//! finished run's pending changes as files, refuses while the run is active,
//! and leaves pending only what it couldn't apply. [`ap::pending`] says how
//! it applies each share. A share whose root holds a `.git` directory comes
//! back as the branch `ap/<agent>/<run>`. Each branch import is checked
//! before anything lands, so one that fails keeps everything pending.

use std::io::{self, Write};

use ap::pending::{self, Share};
use store::Runs;

use super::diff::resolve;

/// Applies the run's changes, and returns the exit status.
pub fn run(name: &str) -> u8 {
    match apply(name) {
        Ok(status) => status,
        Err(message) => {
            eprintln!("ap: {message}");
            1
        }
    }
}

fn apply(name: &str) -> Result<u8, String> {
    let (root, run) = resolve(name)?;
    let Some(run) = run else {
        return Ok(0);
    };
    let runs = Runs::new(&root);
    if runs.active(&run).map_err(|error| error.to_string())? {
        return Err(format!(
            "run {run} is active, so its changes stay. Stop it with `ap stop {run}` first"
        ));
    }
    let shares = (runs.pending_shares(&run)).map_err(|error| format!("run {run}: {error}"))?;
    let shares: Vec<Share> = (shares.iter())
        .map(|share| Share {
            root: &share.root,
            dir: &share.dir,
        })
        .collect();
    let failed =
        |share: &Share, error: io::Error| format!("run {run}: {}: {error}", share.root.display());
    // Every branch import is checked before any share lands.
    let mut imports = Vec::new();
    for share in &shares {
        let repository = (share.is_repository()).map_err(|error| failed(share, error))?;
        imports.push(match repository {
            false => None,
            true => {
                let record = runs.get(&run).map_err(|error| error.to_string())?;
                let agent = record.ok_or_else(|| {
                    format!("run {run} has no record, so its branch would have no name")
                })?;
                let branch = format!("ap/{}/{run}", agent.agent);
                Some(
                    share
                        .import(&branch)
                        .map_err(|error| failed(share, error))?,
                )
            }
        });
    }
    let mut out = Vec::new();
    let mut ignored = 0;
    let mut pending = 0;
    let mut error = None;
    for (share, import) in shares.iter().zip(imports) {
        let applied = match import {
            Some(import) => import.land(),
            None => share.apply(),
        };
        match applied {
            Ok(applied) => {
                if shares.len() > 1 && !applied.text.is_empty() {
                    out.extend(pending::root_line(share.root));
                }
                out.extend(applied.text);
                ignored += applied.ignored;
                pending += applied.pending;
            }
            Err(cause) => {
                error = Some(failed(share, cause));
                break;
            }
        }
    }
    if let Some(line) = pending::ignored_line(ignored) {
        out.extend(line.bytes());
    }
    let mut stdout = io::stdout().lock();
    match stdout.write_all(&out).and_then(|()| stdout.flush()) {
        Err(error) if error.kind() != io::ErrorKind::BrokenPipe => return Err(error.to_string()),
        _ => {}
    }
    if let Some(error) = error {
        return Err(error);
    }
    if pending == 0 {
        runs.discard(&run)
            .map_err(|error| format!("can't remove the pending directory of run {run}: {error}"))?;
        return Ok(0);
    }
    let files = match pending {
        1 => "1 file stays".to_owned(),
        n => format!("{n} files stay"),
    };
    eprintln!("ap: {files} pending in run {run}, which `ap diff {run}` shows");
    Ok(1)
}
