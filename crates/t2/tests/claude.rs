//! Real Claude Code in a guest, with no credentials: a static `ap` builds an
//! agent from Anthropic's release, and runs it in a terminal, as you would.

use std::fs;
use std::thread;
use std::time::{Duration, Instant};

mod common;
use common::{Interactive, Scratch, pin};

/// How long Claude Code may take to start in a guest. A start took 38 to 132
/// seconds on a loaded machine in journey step 8.
const STARTS_WITHIN: Duration = Duration::from_secs(600);

/// `screen` without escape sequences and whitespace.
fn visible(screen: &str) -> String {
    let mut text = String::new();
    let mut characters = screen.chars();
    while let Some(character) = characters.next() {
        match character {
            '\x1b' => match characters.next() {
                // An escape sequence ends at the first byte after its
                // intermediate bytes, as in ESC `(` `B`.
                Some(' '..='/') => {
                    for next in characters.by_ref() {
                        if !(' '..='/').contains(&next) {
                            break;
                        }
                    }
                }
                // A control sequence ends at its final byte.
                Some('[') => {
                    for next in characters.by_ref() {
                        if ('\x40'..='\x7e').contains(&next) {
                            break;
                        }
                    }
                }
                // A string ends at BEL, or at ESC `\`.
                Some(']' | 'P' | '_' | '^') => {
                    while let Some(next) = characters.next() {
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' {
                            characters.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            character if character.is_whitespace() || character.is_control() => {}
            character => text.push(character),
        }
    }
    text
}

// spec(Run): The Claude Code adapter marks `/` as trusted (`projects["/"].hasTrustDialogAccepted` in `~/.claude.json`), so an interactive run whose working directory isn't in a git repository doesn't ask whether to trust it.
#[test]
fn an_interactive_claude_code_run_reaches_its_prompt_with_no_dialog() {
    let scratch = Scratch::new("dialogs");
    let pin = pin("claude");
    scratch.build(
        "claude",
        &format!(
            "AGENT claude:{}\nOS minimal\nNETWORK -\nMODE auto\n",
            pin.version
        ),
        &pin.lock,
    );
    reaches_its_prompt(Interactive::start(scratch.run("claude")));
}

// Claude Code 2.1.323 counts no trust record above a repository's root, so
// the record for `/` doesn't cover this one.
// spec(Run): Each run also marks its working directory and every directory above it as trusted, so an interactive run whose working directory is in a git repository doesn't ask whether to trust it.
#[test]
fn an_interactive_claude_code_run_in_a_shared_repository_reaches_its_prompt() {
    let scratch = Scratch::new("repository-dialog");
    let pin = pin("claude");
    scratch.build(
        "claude",
        &format!(
            "AGENT claude:{}\nOS minimal\nNETWORK -\nMODE auto\n",
            pin.version
        ),
        &pin.lock,
    );
    // The least that git takes as a repository. Claude Code reads the
    // quote and the backslash in its name from the state as JSON.
    let project = scratch.root.join("a \"project\" \\ here");
    for directory in [".git/objects", ".git/refs/heads"] {
        fs::create_dir_all(project.join(directory)).unwrap();
    }
    fs::write(project.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    let mut run = scratch.run("claude");
    run.arg("--share").arg(&project);
    reaches_its_prompt(Interactive::start(run));
}

// Claude Code asks whether to use a key from its environment that its state
// doesn't approve. The key isn't a real one, and nothing sends it anywhere
// it could be used.
// spec(AUTH): With Claude Code, the run's `~/.claude.json` approves the placeholder, so an interactive run doesn't ask whether to use the key.
#[test]
fn an_interactive_run_with_auth_key_doesnt_ask_whether_to_use_it() {
    let scratch = Scratch::new("key-dialog");
    let pin = pin("claude");
    scratch.build(
        "keyed",
        &format!(
            "AGENT claude:{}\nOS minimal\nNETWORK -\nMODE auto\nAUTH key\n",
            pin.version
        ),
        &pin.lock,
    );
    let mut run = scratch.run("keyed");
    run.env("ANTHROPIC_API_KEY", "sk-ant-api03-not-a-real-key-5c0e");
    reaches_its_prompt(Interactive::start(run));
}

/// Fails unless Claude Code shows its prompt, with no dialog before it.
fn reaches_its_prompt(mut run: Interactive) {
    // The prompt's status line names the mode. Claude Code draws it only
    // once no dialog is in the way.
    let deadline = Instant::now() + STARTS_WITHIN;
    loop {
        let exited = run.run.try_wait().unwrap();
        // Claude Code moves the cursor in place of most spaces.
        let text = visible(&run.shown());
        assert!(
            !text.contains("Itrustthisfolder"),
            "Claude Code asked whether to trust its working directory: {text}"
        );
        assert!(
            !text.contains("DetectedacustomAPIkey"),
            "Claude Code asked whether to use its key: {text}"
        );
        if text.contains("⏵⏵automodeon") {
            break;
        }
        assert!(exited.is_none(), "ap run exited {exited:?}: {text}");
        assert!(
            Instant::now() < deadline,
            "Claude Code never showed its prompt: {text}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn visible_text_drops_escape_sequences_and_whitespace() {
    let screen = "\x1b[?25l\x1b]0;title\x07⏵⏵ auto\x1b[1Cmode\x1b(B on\r\n\x1b]8;;x\x1b\\on";
    assert_eq!(visible(screen), "⏵⏵automodeonon");
}
