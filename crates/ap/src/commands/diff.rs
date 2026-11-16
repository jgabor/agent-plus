//! `ap diff <agent | run>`, from SPEC.md's `SHARE` section. It reads the
//! run's pending directory itself, during the run or after it, and never
//! asks the run. [`ap::pending`] says how it reads each share.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;

use ap::pending::{self, Share};
use store::Runs;

/// Reads the arguments of `ap diff`, `ap apply`, and `ap discard`: one
/// agent's or run's name. Anything else is bad usage.
pub fn parse(args: &[OsString]) -> Option<String> {
    match args {
        [name] if !name.as_encoded_bytes().starts_with(b"-") => Some(name.to_str()?.to_owned()),
        _ => None,
    }
}

/// Prints the run's changes, and returns the exit status.
pub fn run(name: &str) -> u8 {
    match diff(name) {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("ap: {message}");
            1
        }
    }
}

fn diff(name: &str) -> Result<(), String> {
    let (root, run) = resolve(name)?;
    let Some(run) = run else {
        return Ok(());
    };
    let shares =
        (Runs::new(&root).pending_shares(&run)).map_err(|error| format!("run {run}: {error}"))?;
    let mut out = Vec::new();
    let mut ignored = 0;
    for share in &shares {
        let shown = Share {
            root: &share.root,
            dir: &share.dir,
        }
        .diff()
        .map_err(|error| format!("run {run}: {}: {error}", share.root.display()))?;
        if shares.len() > 1 && !shown.text.is_empty() {
            out.extend(pending::root_line(&share.root));
        }
        out.extend(shown.text);
        ignored += shown.ignored;
    }
    if let Some(line) = pending::ignored_line(ignored) {
        out.extend(line.bytes());
    }
    let mut stdout = io::stdout().lock();
    match stdout.write_all(&out).and_then(|()| stdout.flush()) {
        Err(error) if error.kind() != io::ErrorKind::BrokenPipe => Err(error.to_string()),
        _ => Ok(()),
    }
}

/// The store's root, and the run that `name` means, if it has pending
/// changes: an agent's single run with them, or else the run with that
/// name. An agent with several fails, and lists them.
pub fn resolve(name: &str) -> Result<(PathBuf, Option<String>), String> {
    let root = ap::store::default_root(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
    .ok_or("HOME isn't an absolute path, so there's no place for the store")?;
    let runs = Runs::new(&root);
    let failed = |error: io::Error| error.to_string();
    let is_agent =
        store::valid_name(name) && (store::Agents::new(&root).get(name).map_err(failed)?).is_some();
    if !is_agent {
        let known = store::valid_run_name(name)
            && (runs.get(name).map_err(failed)?.is_some()
                || runs
                    .list_pending()
                    .map_err(failed)?
                    .iter()
                    .any(|run| run == name));
        return match known {
            false => Err(format!("no agent or run is named {name}")),
            true => Ok((
                root,
                runs.has_pending(name)
                    .map_err(failed)?
                    .then(|| name.to_owned()),
            )),
        };
    }
    let mut pending = Vec::new();
    for run in runs.list_pending().map_err(failed)? {
        if let Some(record) = runs.get(&run).map_err(failed)?
            && record.agent == name
            && runs.has_pending(&run).map_err(failed)?
        {
            pending.push(record);
        }
    }
    match pending.len() {
        0 => Ok((root, None)),
        1 => Ok((root, pending.pop().map(|run| run.name))),
        count => {
            pending.sort_by(|a, b| b.started.cmp(&a.started).then(a.name.cmp(&b.name)));
            let names: Vec<String> = pending.into_iter().map(|run| run.name).collect();
            Err(format!(
                "agent {name} has pending changes in {count} runs, so name one of them:\n{}",
                names.join("\n")
            ))
        }
    }
}
