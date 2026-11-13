//! `ap ps`, from SPEC.md's `ap ps` section. It reads the runtime directory,
//! and lists each run there whose `ap run` still holds the lock on its
//! record: a header row, then a row per run, newest first, with its name, its
//! agent, and when it started, in UTC.

use std::ffi::OsString;
use std::io;
use std::path::Path;

use store::{Run, Runs};

/// Reads `ap ps`'s arguments: none, or `-a`, which isn't built yet.
pub fn parse(args: &[OsString]) -> Option<bool> {
    match args {
        [] => Some(false),
        [all] if all == "-a" => Some(true),
        _ => None,
    }
}

/// Lists the active runs, and returns the exit status.
pub fn run(all: bool) -> u8 {
    if all {
        eprintln!("ap: `ap ps -a` is not built yet");
        return 1;
    }
    let home = std::env::var_os("HOME");
    let Some(root) = ap::store::default_root(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        home.as_deref(),
    ) else {
        eprintln!("ap: HOME isn't an absolute path, so there's no place for the store");
        return 1;
    };
    let dir = ap::runs::runtime_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), &root);
    match active(&dir, &Runs::new(&root)) {
        Ok(runs) => {
            print!("{}", table(runs));
            0
        }
        Err(error) => {
            eprintln!("ap: {error}");
            1
        }
    }
}

/// The runs with a socket in `dir` that are active.
fn active(dir: &Path, runs: &Runs) -> io::Result<Vec<Run>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("{}: {error}", dir.display()),
            ));
        }
    };
    let mut active = Vec::new();
    for entry in entries {
        let file = entry?.file_name();
        let Some(name) = file
            .to_str()
            .and_then(|file| file.strip_suffix(".sock"))
            .filter(|name| store::valid_run_name(name))
        else {
            continue;
        };
        // A run can end between the two reads.
        if runs.active(name)?
            && let Some(run) = runs.get(name)?
        {
            active.push(run);
        }
    }
    Ok(active)
}

/// The header and a row per run, newest first, in aligned columns.
fn table(mut runs: Vec<Run>) -> String {
    runs.sort_by(|a, b| b.started.cmp(&a.started).then_with(|| a.name.cmp(&b.name)));
    let mut rows = vec![["NAME".to_owned(), "AGENT".to_owned(), "STARTED".to_owned()]];
    rows.extend(
        runs.into_iter()
            .map(|run| [run.name, run.agent, utc(run.started)]),
    );
    let name_width = rows.iter().map(|row| row[0].len()).max().unwrap_or(0);
    let agent_width = rows.iter().map(|row| row[1].len()).max().unwrap_or(0);
    rows.iter()
        .map(|[name, agent, started]| {
            format!("{name:name_width$}  {agent:agent_width$}  {started}\n")
        })
        .collect()
}

/// A time in nanoseconds since the Unix epoch, to the second, in UTC.
fn utc(nanoseconds: u128) -> String {
    let seconds = (nanoseconds / 1_000_000_000) as u64;
    let (days, time) = (seconds / 86_400, seconds % 86_400);
    // Howard Hinnant's `civil_from_days`, for days since 1970-01-01.
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        time / 60 % 60,
        time % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(name: &str, agent: &str, started: u128) -> Run {
        Run {
            name: name.to_owned(),
            agent: agent.to_owned(),
            started,
        }
    }

    #[test]
    fn a_time_prints_in_utc_to_the_second() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(951_782_400_999_999_999), "2000-02-29T00:00:00Z");
        assert_eq!(utc(1_794_567_729_123_456_789), "2026-11-13T11:02:09Z");
        assert_eq!(utc(4_107_542_399_000_000_000), "2100-02-28T23:59:59Z");
    }

    #[test]
    fn rows_are_newest_first_under_aligned_headers() {
        let second = 1_000_000_000;
        let table = table(vec![
            run("k3x9q2mf", "notes", 10 * second),
            run("a0b1c2d3", "my-long-agent", 30 * second),
            run("zz9", "x", 20 * second),
        ]);
        assert_eq!(
            table,
            "NAME      AGENT          STARTED\n\
             a0b1c2d3  my-long-agent  1970-01-01T00:00:30Z\n\
             zz9       x              1970-01-01T00:00:20Z\n\
             k3x9q2mf  notes          1970-01-01T00:00:10Z\n"
        );
        assert_eq!(table_header(), "NAME  AGENT  STARTED\n");
    }

    fn table_header() -> String {
        table(Vec::new())
    }
}
