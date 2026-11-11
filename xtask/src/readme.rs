//! The README's Core commands: one list item for each `ap` command, in the
//! words a user reads. The roadmap shows them, so no command is described
//! twice.

use crate::spec;

pub const README_FILE: &str = "README.md";
const HEADING: &str = "Core commands";

/// One list item under Core commands, such as
/// "`ap init [dir]`: create an Agentfile".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// The code spans before the colon, such as `ap init [dir]`.
    spans: Vec<String>,
    /// The whole item, on one line.
    pub text: String,
    /// What follows the colon.
    pub about: String,
}

/// The items under Core commands that describe a command, in document order.
/// An item describes one when it has a code span, then a colon. Fenced lines
/// are examples, and aren't items.
pub fn command_lines(readme: &str) -> Vec<Line> {
    let lines: Vec<&str> = readme.lines().collect();
    let fenced = spec::fenced(&lines);
    let headings = spec::headings_of(&lines, &fenced);
    let Some(at) = headings.iter().position(|(_, title, _)| *title == HEADING) else {
        return Vec::new();
    };
    let (depth, _, start) = headings[at];
    let closes = |(next, _, _): &&(usize, &str, usize)| *next <= depth;
    let end = headings[at + 1..]
        .iter()
        .find(closes)
        .map_or(lines.len(), |(_, _, line)| *line);
    let is_heading = |line: usize| headings.iter().any(|(_, _, known)| *known == line);

    let mut items: Vec<String> = Vec::new();
    let mut open = false;
    for index in start + 1..end {
        let line = lines[index];
        if fenced[index] || is_heading(index) || line.trim().is_empty() {
            open = false;
        } else if let Some(item) = line.strip_prefix("- ") {
            items.push(item.trim().to_string());
            open = true;
        } else if open && line.starts_with(' ') {
            let item = items.last_mut().expect("an open item is the last one");
            item.push(' ');
            item.push_str(line.trim());
        } else {
            open = false;
        }
    }
    items.iter().filter_map(|item| read_line(item)).collect()
}

/// The first line that describes this command: one with a code span that is
/// the command, or that starts with it and a space, as `ap init [dir]` does
/// for `ap init`.
pub fn line_for<'a>(lines: &'a [Line], command: &str) -> Option<&'a Line> {
    let names = |span: &String| {
        span.strip_prefix(command)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    };
    lines.iter().find(|line| line.spans.iter().any(names))
}

/// Splits an item at its first colon outside a code span.
fn read_line(item: &str) -> Option<Line> {
    let mut spans = Vec::new();
    let mut span: Option<usize> = None;
    for (at, c) in item.char_indices() {
        match (c, span) {
            ('`', Some(from)) => {
                spans.push(item[from..at].to_string());
                span = None;
            }
            ('`', None) => span = Some(at + 1),
            (':', None) if !spans.is_empty() => {
                return Some(Line {
                    spans,
                    text: item.to_string(),
                    about: item[at + 1..].trim().to_string(),
                });
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const README: &str = "# Demo

- `ap early`: before the heading, so it isn't a line

### Core commands

Some words, and `ap words`: not an item.

#### Agents

- `ap init [dir]`: create an Agentfile with defaults,
  network closed
- `ap init --force`: a second line for the same command
- `ap secret set|rm|ls`: manage secrets: all of them
- `ap push` and `ap pull`: share agents (after v0.1.0)
- no code span: so it describes no command
- `ap bare` has no colon

```
- `ap fenced`: an example
```

#### About

- `ap --version`: print the version

## Why

- `ap late`: after the section, so it isn't a line
";

    fn about(command: &str) -> Option<String> {
        let lines = command_lines(README);
        line_for(&lines, command).map(|line| line.about.clone())
    }

    #[test]
    fn an_item_under_core_commands_describes_its_command() {
        assert_eq!(about("ap --version").as_deref(), Some("print the version"));
        assert_eq!(
            about("ap secret").as_deref(),
            Some("manage secrets: all of them")
        );
    }

    #[test]
    fn a_wrapped_item_is_one_line_and_the_first_item_wins() {
        assert_eq!(
            about("ap init").as_deref(),
            Some("create an Agentfile with defaults, network closed")
        );
    }

    #[test]
    fn one_item_can_describe_two_commands() {
        let lines = command_lines(README);
        let text = "`ap push` and `ap pull`: share agents (after v0.1.0)";
        for command in ["ap push", "ap pull"] {
            assert_eq!(
                line_for(&lines, command).map(|l| l.text.as_str()),
                Some(text)
            );
        }
    }

    #[test]
    fn a_command_is_named_whole() {
        // `ap in` isn't `ap init`, and `ap secret set` isn't a command's line.
        assert_eq!(about("ap in"), None);
        assert_eq!(about("ap secret set"), None);
    }

    #[test]
    fn only_items_under_the_heading_count() {
        for command in ["ap early", "ap words", "ap bare", "ap fenced", "ap late"] {
            assert_eq!(about(command), None, "{command}");
        }
        assert_eq!(command_lines(README).len(), 5);
    }

    #[test]
    fn a_readme_without_the_heading_describes_nothing() {
        assert_eq!(command_lines("# Demo\n\n- `ap init`: create\n"), Vec::new());
    }
}
