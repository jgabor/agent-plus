//! The harness adapters, from SPEC.md's AGENT section. An adapter says where
//! the harness's configuration goes, which first-run state it prepares, and
//! what its command line holds. `ap build` puts the files it renders in the
//! agent's final layer, and `ap run` starts the harness with its arguments.

pub mod claude;
pub mod codex;

/// `SYSTEM`'s values as one text: in file order, separated by a blank line,
/// and ending in a newline. A value's own blank lines are kept.
pub fn system(values: &[Vec<u8>]) -> Vec<u8> {
    let mut text: Vec<u8> = Vec::new();
    for value in values {
        if !text.is_empty() {
            if !text.ends_with(b"\n") {
                text.push(b'\n');
            }
            text.push(b'\n');
        }
        text.extend_from_slice(value);
    }
    if !text.ends_with(b"\n") {
        text.push(b'\n');
    }
    text
}

/// An exact version, as numbers to compare. A part that isn't a number, such
/// as a pre-release suffix, ends it.
fn version(text: &str) -> Vec<u64> {
    text.split('.')
        .map_while(|part| part.parse().ok())
        .collect()
}

/// The level as `EFFORT` writes it, which is also how both harnesses write
/// it.
fn effort_name(effort: agentfile::Effort) -> &'static str {
    use agentfile::Effort;
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::Xhigh => "xhigh",
        Effort::Max => "max",
    }
}
