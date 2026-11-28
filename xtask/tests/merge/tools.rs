use super::*;

impl Repo {
    /// Wrappers for `git` and `cargo`, and the fake `gh`. Each logs the call.
    /// `KILL_ON` kills `xtask` at the first call containing it, `KILL_WHEN`
    /// says whether before or after doing the call, `HANG_ON` makes a matching
    /// call sleep for `HANG_FOR` seconds, or 3, and with `HANG_UNTIL` a later
    /// matching call sleeps until that many seconds after the first one
    /// started. `RUN_ON` runs `RUN_COMMAND` just before the first matching call,
    /// even one that then hangs. `PAUSE_ON` makes each matching call that
    /// shares the merge's pipe to its supervisor write `w` to it, which would
    /// pause the timeout. The fake `gh` keeps the newest state posted on each
    /// commit, and gives it back when asked for that commit's status.
    /// `STAND_IN_PROOF` stands in for the proof's cargo work, as its own
    /// comment says.
    pub(super) fn write_tools(&self) {
        let bin = self.bin.display();
        let prelude = format!(
            r#"#!/bin/sh
call="$(basename "$0") $*"
echo "$call" >> '{bin}/calls'
maybe_kill() {{
  if [ -n "$KILL_ON" ] && [ ! -f '{bin}/killed' ] && [ "$KILL_WHEN" = "$1" ]; then
    case "$call" in *"$KILL_ON"*) : > '{bin}/killed'; kill -9 "$PPID"; exit 9 ;; esac
  fi
}}
if [ -n "$RUN_ON" ] && [ ! -f '{bin}/ran' ]; then
  case "$call" in *"$RUN_ON"*) : > '{bin}/ran'; eval "$RUN_COMMAND" ;; esac
fi
if [ -n "$PAUSE_ON" ] && [ -p /dev/fd/0 ]; then
  case "$call" in *"$PAUSE_ON"*) echo "$call" >> '{bin}/paused'; printf w >&0 ;; esac
fi
if [ -n "$HANG_ON" ]; then
  case "$call" in *"$HANG_ON"*)
    if [ -n "$HANG_UNTIL" ] && [ -f '{bin}/hung' ]; then
      left=$(( $(cat '{bin}/hung') + HANG_UNTIL - $(date +%s) ))
      exec sleep $(( left > 0 ? left : 0 ))
    fi
    date +%s > '{bin}/hung'; exec sleep "${{HANG_FOR:-3}}" ;;
  esac
fi
maybe_kill before
"#
        );
        for tool in ["git", "cargo"] {
            let (path, built) = if tool == "cargo" {
                let cargo = common::toolchain().join("cargo");
                let stand_ins = format!("{}{STAND_IN_PROOF}", stand_in_build());
                (cargo.display().to_string(), stand_ins)
            } else {
                (real(tool), String::new())
            };
            let script =
                format!("{prelude}{built}'{path}' \"$@\"\ncode=$?\nmaybe_kill after\nexit $code\n");
            let file = self.bin.join(tool);
            common::write_tool(&file, &script);
        }
        let git = real("git");
        let origin = self.origin.display();
        let gh = format!(
            r#"{prelude}case "$1 $2" in
  "repo view") echo '{{"nameWithOwner":"o/r","owner":{{"login":"o"}}}}' ;;
  "pr view") case "$*" in
    *autoMergeRequest*) if [ -f "{bin}/merged-$3" ]; then state=MERGED; else state=OPEN; fi
      if [ -f "{bin}/armed-$3" ] && [ ! -f "{bin}/disarmed-$3" ]; then auto='{{"mergeMethod":"SQUASH"}}'; else auto=null; fi
      branch=$(cat "{bin}/branch-$3" 2>/dev/null || echo '{BRANCH}')
      head=$('{git}' --git-dir '{origin}' rev-parse "refs/heads/$branch")
      printf '{{"state":"%s","headRefOid":"%s","autoMergeRequest":%s}}\n' "$state" "$head" "$auto" ;;
    *) if [ -f "{bin}/pr-$3.json" ]; then cat "{bin}/pr-$3.json"; elif [ -f '{bin}/merged' ]; then cat '{bin}/pr-merged.json'; else cat '{bin}/pr.json'; fi ;;
  esac ;;
  "pr list") if [ -f '{bin}/prs.json' ]; then cat '{bin}/prs.json'; else echo '[]'; fi ;;
  "issue list") cat '{bin}/issues.json' ;;
  "pr comment") if [ -f '{bin}/comment-fails' ]; then echo 'gh: no comment (HTTP 403)' >&2; exit 1; fi
    cat >> '{bin}/pr-comments'; echo >> '{bin}/pr-comments'; echo 'https://github.com/o/r/pull/9#issuecomment-5' ;;
  "pr merge") case "$*" in *--disable-auto*) : > "{bin}/disarmed-$3"; exit 0 ;; esac
    if [ -f '{bin}/merge-fails' ]; then echo 'no merge' >&2; exit 1; fi
    echo merged >> '{bin}/merges'; : > "{bin}/merged-$3"
    if [ "$3" = 9 ]; then echo "$9" > '{bin}/merged'; fi
    if [ -f '{bin}/merge-fails-late' ]; then echo 'no branch to delete' >&2; exit 1; fi ;;
  "issue view") cat '{bin}/issue.json' ;;
  "api -X") case "$4" in */labels*) n="${{4%/labels*}}"; n="${{n##*/}}"
      if [ -f "{bin}/refuse-label-$n" ]; then echo 'gh: Resource not accessible by integration (HTTP 403)' >&2; exit 1; fi
      exit 0 ;;
      */issues/*) if [ -f "{bin}/refuse-close-${{4##*/}}" ]; then echo 'gh: Resource not accessible by integration (HTTP 403)' >&2; exit 1; fi
      exit 0 ;; esac
    if [ -f '{bin}/status-fails' ]; then echo 'no status' >&2; exit 1; fi
    if [ -f '{bin}/pending-fails' ] && [ "$6" = state=pending ]; then echo 'no pending' >&2; exit 1; fi
    printf '{{"statuses":[{{"context":"xtask/gate","state":"%s","description":"%s"}}]}}\n' "${{6#state=}}" "${{10#description=}}" > "{bin}/status-${{4##*/}}" ;;
  "api repos/"*/comments) cat '{bin}/pr-comments' 2>/dev/null; exit 0 ;;
  "api repos/"*) if [ -f '{bin}/status-read-fails' ]; then echo 'no status to read' >&2; exit 1; fi
    posted="${{2#*/commits/}}"; posted="{bin}/status-${{posted%/status}}"
    if [ -f "$posted" ]; then cat "$posted"; else echo '{{"statuses":[]}}'; fi ;;
  *) echo "unexpected gh call: $*" >&2; exit 3 ;;
esac
code=$?
maybe_kill after
exit $code
"#
        );
        let file = self.bin.join("gh");
        common::write_tool(&file, &gh);
    }
}

/// The fixture has no xtask to build, so a merge that builds one in a tree
/// gets the test's own binary, or the variant that a marker in the tree names.
fn stand_in_build() -> String {
    format!(
        r#"if [ "$1 $2 $3 $4 $5" = "build --quiet --package xtask --target-dir" ]; then
  built='{own}'
  if grep -qs 'variant: unprotected' xtask/src/merge.rs; then built='{unprotected}'; fi
  if grep -qs 'variant: failing' xtask/src/journey.rs; then built='{failing}'; fi
  if grep -qs 'variant: lying' xtask/src/bench.rs; then built='{lying}'; fi
  mkdir -p "$6/debug" && rm -f "$6/debug/xtask" && cp "$built" "$6/debug/xtask"
  code=$?
  maybe_kill after
  exit $code
fi
"#,
        own = env!("CARGO_BIN_EXE_xtask"),
        unprotected = variant_binary("unprotected").display(),
        failing = variant_binary("failing").display(),
        lying = variant_binary("lying").display(),
    )
}

/// With `STAND_IN_PROOF` set, the proof's format check, lint, and tests pass
/// at once, and each release build leaves a program that exits 0. A test that
/// isn't about what cargo does sets it, so that its proof's work takes about a
/// second, where the real commands take up to 40 on a loaded machine.
const STAND_IN_PROOF: &str = r#"if [ -n "$STAND_IN_PROOF" ]; then
  case "$1 $2" in
    "fmt --all" | "clippy --workspace" | "test --package") maybe_kill after; exit 0 ;;
    "build --release")
      while [ $# -gt 0 ]; do
        case "$1" in --package) package=$2 ;; --target) target=$2 ;; --target-dir) dir=$2 ;; esac
        shift
      done
      built="$dir/$target/release/$package"
      mkdir -p "${built%/*}" && printf '#!/bin/sh\nexit 0\n' > "$built" && chmod +x "$built"
      code=$?
      maybe_kill after
      exit $code ;;
  esac
fi
"#;

#[test]
fn a_tool_just_written_can_run_while_other_threads_start_processes() {
    let directory = tmp().join("merge/fresh-tool");
    if directory.exists() {
        fs::remove_dir_all(&directory).unwrap();
    }
    fs::create_dir_all(&directory).unwrap();
    thread::scope(|scope| {
        for worker in 0..16 {
            let directory = &directory;
            scope.spawn(move || {
                for round in 0..200 {
                    let tool = directory.join(format!("tool-{worker}-{round}"));
                    common::write_tool(&tool, "#!/bin/sh\nexit 0\n");
                    let status = Command::new(&tool).status().unwrap();
                    assert!(status.success());
                }
            });
        }
    });
}
