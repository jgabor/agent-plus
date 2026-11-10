//! Extracts the units of SPEC.md, by the rule in HARNESS.md.

use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit {
    pub heading: String,
    pub text: String,
}

const MARKERS: [&str; 3] = ["*Proposed*", "*Proposed:*", "**Planned.**"];

/// Returns the units that are promises or waivable, in document order.
/// Marked units are left out.
pub fn extract(spec: &str) -> Result<Vec<Unit>, String> {
    let lines: Vec<&str> = spec.lines().collect();
    let mut extractor = Extractor::default();
    let mut headings = HashSet::new();
    let mut fence: Option<(u8, usize)> = None;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if let Some(open) = fence {
            if closes_fence(trimmed, open) {
                fence = None;
            }
            continue;
        }
        if !trimmed.is_empty() && !line.starts_with("  ") {
            extractor.marked_item = false;
        }
        if let Some(open) = opens_fence(trimmed) {
            extractor.flush();
            fence = Some(open);
        } else if trimmed.is_empty() {
            extractor.flush();
        } else if let Some(title) = heading(line) {
            extractor.flush();
            if !headings.insert(title.to_string()) {
                return Err(format!("duplicate heading `{title}`"));
            }
            extractor.heading = Some(title.to_string());
        } else if trimmed.starts_with('|') {
            extractor.flush();
            let next_is_separator = lines.get(index + 1).is_some_and(|l| is_separator(l));
            if !next_is_separator && !is_separator(line) {
                extractor.block.push(trimmed);
                extractor.flush_whole();
            }
        } else {
            let body = trimmed.strip_prefix("> ").unwrap_or(trimmed);
            if let Some(item) = list_item(body) {
                extractor.flush();
                extractor.marked_item = false;
                extractor.in_item = true;
                extractor.block.push(item);
            } else {
                extractor.block.push(body);
            }
        }
    }
    extractor.flush();
    Ok(extractor.units)
}

#[derive(Default)]
struct Extractor<'a> {
    heading: Option<String>,
    block: Vec<&'a str>,
    seen: HashSet<(String, String)>,
    units: Vec<Unit>,
    /// The current block began as a list item.
    in_item: bool,
    /// A marker earlier in this list item covers what follows it.
    marked_item: bool,
}

impl Extractor<'_> {
    fn take_text(&mut self) -> Option<String> {
        let text = self.block.join(" ");
        self.block.clear();
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let marked = MARKERS.iter().any(|m| text.contains(m)) || self.marked_item;
        self.marked_item = marked && (self.in_item || self.marked_item);
        self.in_item = false;
        (!text.is_empty() && !marked).then_some(text)
    }

    /// Ends a paragraph or list item, which holds any number of sentences.
    fn flush(&mut self) {
        if let Some(text) = self.take_text() {
            for sentence in sentences(&text) {
                self.add(sentence);
            }
        }
    }

    /// Ends a table row, which is one unit however many full stops it has.
    fn flush_whole(&mut self) {
        if let Some(text) = self.take_text() {
            self.add(text);
        }
    }

    fn add(&mut self, text: String) {
        let Some(heading) = &self.heading else {
            return;
        };
        if !text.ends_with(':') && self.seen.insert((heading.clone(), text.clone())) {
            self.units.push(Unit {
                heading: heading.clone(),
                text,
            });
        }
    }
}

/// The heading whose sub-headings are the `ap` commands.
const COMMANDS: &str = "Commands";

/// When SPEC.md says a command arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrives {
    V010,
    Later,
    Proposal,
}

/// An entry under the Commands heading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// The entry's heading, such as `ap build`.
    pub name: String,
    pub arrives: Arrives,
    /// The headings that the entry says specify the command, as in
    /// "[Build](#build) specifies `ap build`."
    pub specified_by: Vec<String>,
}

/// Every heading, with its depth, in document order.
fn headings_of(spec: &str) -> Vec<(usize, &str, usize)> {
    let mut found = Vec::new();
    let mut fence: Option<(u8, usize)> = None;
    for (index, line) in spec.lines().enumerate() {
        let trimmed = line.trim_start();
        if let Some(open) = fence {
            if closes_fence(trimmed, open) {
                fence = None;
            }
        } else if let Some(open) = opens_fence(trimmed) {
            fence = Some(open);
        } else if let Some(title) = heading(line) {
            let depth = line.bytes().take_while(|b| *b == b'#').count();
            found.push((depth, title, index));
        }
    }
    found
}

pub fn headings(spec: &str) -> HashSet<String> {
    let titles = headings_of(spec).into_iter();
    titles.map(|(_, title, _)| title.to_string()).collect()
}

/// The commands, in document order. A command is a heading one level below
/// Commands, and its entry runs to the next heading.
pub fn commands(spec: &str) -> Vec<Command> {
    let lines: Vec<&str> = spec.lines().collect();
    let headings = headings_of(spec);
    let mut commands = Vec::new();
    let mut inside = None;
    for (at, (depth, title, line)) in headings.iter().enumerate() {
        match inside {
            None if *title == COMMANDS => inside = Some(*depth),
            None => {}
            Some(parent) if *depth <= parent => break,
            Some(parent) if *depth == parent + 1 => {
                let end = headings.get(at + 1).map_or(lines.len(), |next| next.2);
                let words: Vec<&str> = lines[line + 1..end]
                    .iter()
                    .flat_map(|line| line.split_whitespace())
                    .collect();
                let entry = words.join(" ");
                let arrives = if entry.starts_with("**Planned.**") {
                    Arrives::Later
                } else if entry.starts_with("*Proposed") {
                    Arrives::Proposal
                } else {
                    Arrives::V010
                };
                commands.push(Command {
                    name: title.to_string(),
                    arrives,
                    specified_by: specified_by(&entry),
                });
            }
            Some(_) => {}
        }
    }
    commands
}

/// The text of each link that is followed by "specifies".
fn specified_by(entry: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for (at, _) in entry.match_indices(") specifies ") {
        let link = &entry[..at];
        let Some(close) = link.rfind("](#") else {
            continue;
        };
        let anchor = &link[close + 3..];
        let Some(open) = link[..close].rfind('[') else {
            continue;
        };
        let title = &link[open + 1..close];
        let is_link = !anchor.contains(|c: char| c == ')' || c.is_whitespace());
        if is_link && !found.iter().any(|known| known == title) {
            found.push(title.to_string());
        }
    }
    found
}

fn opens_fence(line: &str) -> Option<(u8, usize)> {
    let byte = *line.as_bytes().first()?;
    if byte != b'`' && byte != b'~' {
        return None;
    }
    let length = line.bytes().take_while(|b| *b == byte).count();
    (length >= 3).then_some((byte, length))
}

fn closes_fence(line: &str, (byte, length): (u8, usize)) -> bool {
    let run = line.bytes().take_while(|b| *b == byte).count();
    run >= length && line[run..].trim().is_empty()
}

fn heading(line: &str) -> Option<&str> {
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    line[hashes..]
        .strip_prefix(' ')
        .map(str::trim)
        .filter(|title| !title.is_empty())
}

fn is_separator(line: &str) -> bool {
    let line = line.trim();
    line.starts_with('|')
        && line.contains('-')
        && line.bytes().all(|b| matches!(b, b'|' | b'-' | b':' | b' '))
}

fn list_item(line: &str) -> Option<&str> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(item) = line.strip_prefix(marker) {
            return Some(item);
        }
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0).then(|| line[digits..].strip_prefix(". "))?
}

fn backticks(bytes: &[u8]) -> usize {
    bytes.iter().take_while(|b| **b == b'`').count()
}

/// Finds the end of the code span that a run of `length` backticks opens. A
/// run with no matching close opens no span, and is literal text.
fn closing_run(bytes: &[u8], from: usize, length: usize) -> Option<usize> {
    let mut at = from;
    while at < bytes.len() {
        if bytes[at] == b'`' {
            let run = backticks(&bytes[at..]);
            if run == length {
                return Some(at + run);
            }
            at += run;
        } else {
            at += 1;
        }
    }
    None
}

/// Splits after `.`, `?`, or `!` followed by a space, outside code spans.
fn sentences(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'\\' {
            at += 2;
            continue;
        }
        if bytes[at] == b'`' {
            let run = backticks(&bytes[at..]);
            at = closing_run(bytes, at + run, run).unwrap_or(at + run);
            continue;
        }
        if matches!(bytes[at], b'.' | b'?' | b'!') && bytes.get(at + 1) == Some(&b' ') {
            let sentence = &text[start..=at];
            if !sentence.ends_with("e.g.") && !sentence.ends_with("i.e.") {
                out.push(sentence.to_string());
                start = at + 2;
            }
        }
        at += 1;
    }
    if start < text.len() {
        out.push(text[start..].to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMAND_SPEC: &str = "# R\n\n## Build\n\nOne.\n\n## Commands\n\nEvery command.\n\n### ap build\n\n[Build](#build) specifies `ap build`. [Run](#run) says more.\n\n### ap reset\n\n[PERSIST](#persist) specifies `ap reset <agent>`, and [Grants](#grants)\nspecifies `ap reset <digest>`.\n\n```\n### not a command\n```\n\n#### A note\n\n[Deep](#deep) specifies this too.\n\n### ap push\n\n**Planned.** `ap push` shares. [SEAL](#seal) specifies what it leaves out.\n\n### ap mcp\n\n*Proposed:* `ap mcp` drives a desktop.\n\n## Instructions\n\n### AGENT\n\nNot a command.\n";

    fn command(name: &str, arrives: Arrives, specified_by: &[&str]) -> Command {
        Command {
            name: name.to_string(),
            arrives,
            specified_by: specified_by.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn commands_are_the_headings_under_commands() {
        assert_eq!(
            commands(COMMAND_SPEC),
            [
                command("ap build", Arrives::V010, &["Build"]),
                command("ap reset", Arrives::V010, &["PERSIST", "Grants"]),
                command("ap push", Arrives::Later, &["SEAL"]),
                command("ap mcp", Arrives::Proposal, &[]),
            ]
        );
    }

    #[test]
    fn a_spec_without_a_commands_heading_has_no_commands() {
        assert_eq!(commands("# A\n\n### ap build\n\nOne.\n"), []);
    }

    #[test]
    fn a_closing_bracket_that_isnt_a_link_specifies_nothing() {
        let spec = "## Commands\n\n### ap x\n\nSee [Run](#run) (the long one) specifies nothing.\n";
        assert_eq!(commands(spec), [command("ap x", Arrives::V010, &[])]);
    }

    #[test]
    fn headings_leave_out_fenced_lines() {
        let found = headings("# A\n\n```\n# B\n```\n\n## C\n");
        assert_eq!(found, HashSet::from(["A".to_string(), "C".to_string()]));
    }

    fn texts(spec: &str) -> Vec<String> {
        extract(spec).unwrap().into_iter().map(|u| u.text).collect()
    }

    #[test]
    fn a_wrapped_paragraph_is_unwrapped_and_split() {
        let spec = "# A\n\nThe first sentence\nwraps here. The second one? Yes!\n";
        assert_eq!(
            texts(spec),
            ["The first sentence wraps here.", "The second one?", "Yes!"]
        );
    }

    #[test]
    fn a_unit_belongs_to_the_nearest_heading() {
        let spec = "# A\n\nOne.\n\n## B\n\nTwo.\n";
        let units = extract(spec).unwrap();
        assert_eq!(units[0].heading, "A");
        assert_eq!(units[1].heading, "B");
    }

    #[test]
    fn text_before_any_heading_is_not_a_unit() {
        assert_eq!(texts("Stray.\n\n# A\n\nOne.\n"), ["One."]);
    }

    #[test]
    fn duplicate_headings_are_an_error() {
        let error = extract("# A\n\n## B\n\n## B\n").unwrap_err();
        assert!(error.contains("B"), "{error}");
    }

    #[test]
    fn code_fences_are_ignored() {
        let spec = "# A\n\n```\nNot a unit.\n# Not a heading\n```\n\nOne.\n";
        assert_eq!(texts(spec), ["One."]);
    }

    #[test]
    fn a_longer_fence_can_hold_a_shorter_one() {
        let spec = "# A\n\n`````\n````\nNot a unit.\n````\n`````\n\nOne.\n";
        assert_eq!(texts(spec), ["One."]);
    }

    #[test]
    fn a_fence_only_closes_with_at_least_its_own_length() {
        let spec = "# A\n\n````\n```\nNot a unit.\n````\n\nOne.\n";
        assert_eq!(texts(spec), ["One."]);
    }

    #[test]
    fn a_full_stop_inside_a_code_span_does_not_split() {
        let spec = "# A\n\nUse `a. b` or ``c. `d` e.`` here. Next.\n";
        assert_eq!(texts(spec), ["Use `a. b` or ``c. `d` e.`` here.", "Next."]);
    }

    #[test]
    fn a_full_stop_needs_a_following_space() {
        assert_eq!(
            texts("# A\n\nSee ap.toml and v0.1.0 now.\n"),
            ["See ap.toml and v0.1.0 now."]
        );
    }

    #[test]
    fn abbreviations_do_not_end_a_sentence() {
        let spec = "# A\n\nUse a tool, e.g. this one, i.e. mine. Next.\n";
        assert_eq!(
            texts(spec),
            ["Use a tool, e.g. this one, i.e. mine.", "Next."]
        );
    }

    #[test]
    fn list_markers_are_removed_and_items_unwrapped() {
        let spec = "# A\n\n- First item\n  wraps.\n* Second.\n+ Third.\n1. Fourth.\n  - Nested.\n";
        assert_eq!(
            texts(spec),
            [
                "First item wraps.",
                "Second.",
                "Third.",
                "Fourth.",
                "Nested."
            ]
        );
    }

    #[test]
    fn emphasis_at_line_start_is_not_a_list_marker() {
        assert_eq!(
            texts("# A\n\n**Bold.** Rest here.\n"),
            ["**Bold.** Rest here."]
        );
    }

    #[test]
    fn a_table_row_is_one_unit_without_header_or_separator() {
        let spec = "# A\n\n| H1 | H2 |\n|---|:--:|\n| a. b. | c |\n| d  |  e |\n";
        assert_eq!(texts(spec), ["| a. b. | c |", "| d | e |"]);
    }

    #[test]
    fn a_fragment_ending_in_a_colon_is_not_a_unit() {
        let spec = "# A\n\nThese apply:\n\n- One.\n\nFirst. Then this:\n";
        assert_eq!(texts(spec), ["One.", "First."]);
    }

    #[test]
    fn whitespace_is_collapsed() {
        assert_eq!(texts("# A\n\nOne   two\t three.\n"), ["One two three."]);
    }

    #[test]
    fn identical_units_in_one_section_count_once() {
        assert_eq!(texts("# A\n\nSame. Same.\n\nSame.\n"), ["Same."]);
    }

    #[test]
    fn identical_units_in_two_sections_both_count() {
        assert_eq!(texts("# A\n\nSame.\n\n# B\n\nSame.\n").len(), 2);
    }

    #[test]
    fn a_proposed_marker_covers_its_whole_paragraph() {
        let spec = "# A\n\nKept.\n\nSome text. *Proposed:* the rest. More.\n\nKept too.\n";
        assert_eq!(texts(spec), ["Kept.", "Kept too."]);
    }

    #[test]
    fn a_bare_proposed_marker_counts() {
        assert_eq!(
            texts("# A\n\nItems marked *Proposed* differ. Yes.\n\nKept.\n"),
            ["Kept."]
        );
    }

    #[test]
    fn a_planned_marker_covers_its_paragraph() {
        let spec = "# A\n\n**Planned.** After v0.1.0, this. And that.\n\nKept.\n";
        assert_eq!(texts(spec), ["Kept."]);
    }

    #[test]
    fn a_marker_covers_its_list_item_and_table_row_only() {
        let spec =
            "# A\n\n- *Proposed:* no.\n- Yes.\n\n| H |\n|---|\n| *Proposed:* no |\n| yes |\n";
        assert_eq!(texts(spec), ["Yes.", "| yes |"]);
    }

    #[test]
    fn a_marker_covers_every_paragraph_of_its_list_item() {
        let spec = "# A\n\n- *Proposed:* no.\n\n  Still no.\n\n- Yes.\n\nAlso yes.\n";
        assert_eq!(texts(spec), ["Yes.", "Also yes."]);
    }

    #[test]
    fn an_unmatched_backtick_opens_no_span() {
        let spec = "# A\n\nA literal ` character. Next sentence.\n";
        assert_eq!(texts(spec), ["A literal ` character.", "Next sentence."]);
    }

    #[test]
    fn an_escaped_backtick_opens_no_span() {
        let spec = "# A\n\nLiteral \\`a. b\\` is text. Next.\n";
        assert_eq!(texts(spec), ["Literal \\`a.", "b\\` is text.", "Next."]);
    }

    #[test]
    fn a_blockquote_is_a_paragraph() {
        assert_eq!(
            texts("# A\n\n> Quoted text\n> wraps.\n"),
            ["Quoted text wraps."]
        );
    }

    #[test]
    fn the_real_spec_extracts() {
        let spec = include_str!("../../SPEC.md");
        let units = extract(spec).unwrap();
        assert!(units.len() > 500, "only {} units", units.len());
        assert!(units.iter().all(|u| !u.text.contains('\n')));
        assert!(units.iter().all(|u| !u.text.contains("*Proposed")));
        assert!(!units.iter().any(|u| u.text.contains("````")));
    }
}
