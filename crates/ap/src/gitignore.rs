//! Which files git ignores, from the `.gitignore` files in a directory.
//!
//! This follows gitignore(5), and where the page is silent it follows git's
//! own `dir.c` and `wildmatch.c`, so the answer is the one `git check-ignore`
//! gives. Only `.gitignore` files inside the directory count: not
//! `.git/info/exclude`, `core.excludesFile`, or files above the directory.
//! Names are compared as bytes, and case matters.

use std::fs;
use std::io;
use std::path::{Component, Path};

/// The `.gitignore` files under one directory.
pub struct Gitignore {
    /// Each `.gitignore` that was read, by the directory that holds it. A
    /// directory is its path relative to the root, as bytes, with a trailing
    /// `/`, and the root is empty.
    files: Vec<(Vec<u8>, Vec<Pattern>)>,
}

struct Pattern {
    /// The pattern, without its `!`, its trailing `/`, or a leading `/`.
    glob: Vec<u8>,
    negated: bool,
    dir_only: bool,
    /// A pattern with no `/` but a trailing one matches a name at any depth.
    /// Any other is matched against the path below its directory.
    anywhere: bool,
}

impl Gitignore {
    /// Reads every `.gitignore` under `root`. Like git, it doesn't look
    /// inside an ignored directory or `.git`, and doesn't follow symlinks,
    /// including a `.gitignore` that is one.
    pub fn load(root: &Path) -> io::Result<Self> {
        let mut ignore = Gitignore { files: Vec::new() };
        ignore.read(root, Vec::new())?;
        Ok(ignore)
    }

    fn read(&mut self, root: &Path, dir: Vec<u8>) -> io::Result<()> {
        let here = root.join(bytes_to_path(&dir));
        let file = here.join(".gitignore");
        if fs::symlink_metadata(&file).is_ok_and(|m| m.is_file()) {
            let patterns = parse(&fs::read(&file)?);
            if !patterns.is_empty() {
                self.files.push((dir.clone(), patterns));
            }
        }
        let mut subdirs = Vec::new();
        for entry in fs::read_dir(&here)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() || entry.file_name() == ".git" {
                continue;
            }
            let mut sub = dir.clone();
            sub.extend_from_slice(entry.file_name().as_encoded_bytes());
            if !self.matches(&sub, true) {
                sub.push(b'/');
                subdirs.push(sub);
            }
        }
        for sub in subdirs {
            self.read(root, sub)?;
        }
        Ok(())
    }

    /// Whether git ignores `path`, which is relative to the root and names a
    /// directory when `is_dir` is set. A path inside an ignored directory is
    /// ignored too, whatever a later pattern says.
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let mut rel = Vec::new();
        for part in path.components() {
            if part == Component::CurDir {
                continue;
            }
            if !rel.is_empty() {
                rel.push(b'/');
            }
            rel.extend_from_slice(part.as_os_str().as_encoded_bytes());
        }
        let parents = rel.iter().enumerate().filter(|&(_, &b)| b == b'/');
        for (end, _) in parents {
            if self.matches(&rel[..end], true) {
                return true;
            }
        }
        self.matches(&rel, is_dir)
    }

    /// Whether the patterns alone ignore `path`, without its parents. The
    /// deepest `.gitignore` with a matching pattern decides, and within one
    /// file the last matching pattern does.
    fn matches(&self, path: &[u8], is_dir: bool) -> bool {
        let name = match path.iter().rposition(|&b| b == b'/') {
            Some(slash) => &path[slash + 1..],
            None => path,
        };
        let applies = self
            .files
            .iter()
            .rev()
            .filter(|(dir, _)| path.len() > dir.len() && path.starts_with(dir));
        for (dir, patterns) in applies {
            let below = &path[dir.len()..];
            let hit = patterns.iter().rev().find(|p| {
                // A name holds no `/`, and neither does a pattern that
                // matches anywhere, so it matches the name as a path.
                let text = if p.anywhere { name } else { below };
                (is_dir || !p.dir_only) && wildmatch(&p.glob, text) == Match::Yes
            });
            if let Some(p) = hit {
                return !p.negated;
            }
        }
        false
    }
}

/// The patterns in a `.gitignore`.
fn parse(text: &[u8]) -> Vec<Pattern> {
    let text = text.strip_prefix(b"\xef\xbb\xbf").unwrap_or(text);
    let mut patterns = Vec::new();
    let mut lines: Vec<&[u8]> = text.split(|&b| b == b'\n').collect();
    // Git reads a line only once its newline is seen, or at the end of the
    // file, so a final empty piece is no line.
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    for line in lines {
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        // Git keeps a pattern as a C string, so a NUL ends it.
        let line = line.split(|&b| b == 0).next().unwrap_or(line);
        let mut p = trim_trailing_spaces(line);
        let negated = p.first() == Some(&b'!');
        if negated {
            p = &p[1..];
        }
        let dir_only = p.last() == Some(&b'/');
        if dir_only {
            p = &p[..p.len() - 1];
        }
        let anywhere = !p.contains(&b'/');
        if !anywhere && p.first() == Some(&b'/') {
            p = &p[1..];
        }
        patterns.push(Pattern {
            glob: p.to_vec(),
            negated,
            dir_only,
            anywhere,
        });
    }
    patterns
}

/// Removes trailing spaces, but not one escaped with a backslash.
fn trim_trailing_spaces(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    let mut space = None;
    let mut i = 0;
    while i < line.len() {
        match line[i] {
            b' ' => {
                space.get_or_insert(i);
            }
            b'\\' => {
                i += 1;
                if i == line.len() {
                    return line;
                }
                space = None;
            }
            _ => space = None,
        }
        i += 1;
    }
    if let Some(s) = space {
        end = s;
    }
    &line[..end]
}

#[derive(PartialEq)]
enum Match {
    Yes,
    No,
    /// No later starting point can match either, so a `*` stops trying.
    AbortAll,
    /// Only a `**` may try a later starting point.
    AbortToStarStar,
}

/// Git's `wildmatch` with `WM_PATHNAME`: `*`, `?`, and brackets don't match
/// `/`, and `**` between slashes matches any number of directories.
fn wildmatch(glob: &[u8], text: &[u8]) -> Match {
    let (mut p, mut t) = (0, 0);
    while p < glob.len() {
        let mut pc = glob[p];
        let tc = text.get(t).copied();
        if tc.is_none() && pc != b'*' {
            return Match::AbortAll;
        }
        let tc = tc.unwrap_or(0);
        match pc {
            b'?' => {
                if tc == b'/' {
                    return Match::No;
                }
            }
            b'*' => return star(glob, p, text, t),
            b'[' => match bracket(glob, p, tc) {
                Some((true, end)) => p = end,
                Some((false, _)) => return Match::No,
                None => return Match::AbortAll,
            },
            _ => {
                if pc == b'\\' {
                    p += 1;
                    pc = glob.get(p).copied().unwrap_or(0);
                }
                if tc != pc {
                    return Match::No;
                }
            }
        }
        p += 1;
        t += 1;
    }
    if t == text.len() {
        Match::Yes
    } else {
        Match::No
    }
}

/// Matches a run of `*` at `glob[p]` and the rest of the glob.
fn star(glob: &[u8], mut p: usize, text: &[u8], mut t: usize) -> Match {
    let start = p;
    while glob.get(p) == Some(&b'*') {
        p += 1;
    }
    let mut any_depth = false;
    if p - start >= 2 {
        let after_slash = start == 0 || glob[start - 1] == b'/';
        let rest = &glob[p..];
        let before_slash = rest.is_empty() || rest[0] == b'/' || rest.starts_with(b"\\/");
        if after_slash && before_slash {
            // `/**/` also matches no directory at all.
            if rest.first() == Some(&b'/') && wildmatch(&glob[p + 1..], &text[t..]) == Match::Yes {
                return Match::Yes;
            }
            any_depth = true;
        }
    }
    if p == glob.len() {
        if !any_depth && text[t..].contains(&b'/') {
            return Match::No;
        }
        return Match::Yes;
    }
    if !any_depth && glob[p] == b'/' {
        return match text[t..].iter().position(|&b| b == b'/') {
            Some(slash) => wildmatch(&glob[p..], &text[t + slash..]),
            None => Match::No,
        };
    }
    while t < text.len() {
        let m = wildmatch(&glob[p..], &text[t..]);
        if m != Match::No && (!any_depth || m != Match::AbortToStarStar) {
            return m;
        }
        if m == Match::No && !any_depth && text[t] == b'/' {
            return Match::AbortToStarStar;
        }
        t += 1;
    }
    Match::AbortAll
}

/// Matches `c` against the bracket expression at `glob[p]`. It returns
/// whether it matched and the index of the closing `]`, or nothing when the
/// expression is malformed.
fn bracket(glob: &[u8], mut p: usize, c: u8) -> Option<(bool, usize)> {
    let at = |i: usize| glob.get(i).copied().unwrap_or(0);
    p += 1;
    let mut pc = at(p);
    let negated = pc == b'!' || pc == b'^';
    if negated {
        p += 1;
        pc = at(p);
    }
    let mut prev = 0u8;
    let mut matched = false;
    loop {
        if pc == 0 {
            return None;
        }
        if pc == b'\\' {
            p += 1;
            pc = at(p);
            if pc == 0 {
                return None;
            }
            matched |= c == pc;
        } else if pc == b'-' && prev != 0 && at(p + 1) != 0 && at(p + 1) != b']' {
            p += 1;
            pc = at(p);
            if pc == b'\\' {
                p += 1;
                pc = at(p);
                if pc == 0 {
                    return None;
                }
            }
            matched |= prev <= c && c <= pc;
            pc = 0;
        } else if pc == b'[' && at(p + 1) == b':' {
            let s = p + 2;
            let mut e = s;
            while at(e) != 0 && at(e) != b']' {
                e += 1;
            }
            if at(e) == 0 {
                return None;
            }
            if e == s || at(e - 1) != b':' {
                // No `:]`, so the `[` is an ordinary member.
                matched |= c == b'[';
            } else {
                matched |= class(&glob[s..e - 1], c)?;
                p = e;
                pc = 0;
            }
        } else {
            matched |= c == pc;
        }
        prev = pc;
        p += 1;
        pc = at(p);
        if pc == b']' {
            break;
        }
    }
    Some((matched != negated && c != b'/', p))
}

/// Whether `c` is in the POSIX class `name`, or nothing if there is no such
/// class.
fn class(name: &[u8], c: u8) -> Option<bool> {
    Some(match name {
        b"alnum" => c.is_ascii_alphanumeric(),
        b"alpha" => c.is_ascii_alphabetic(),
        b"blank" => c == b' ' || c == b'\t',
        b"cntrl" => c.is_ascii_control(),
        b"digit" => c.is_ascii_digit(),
        b"graph" => c.is_ascii_graphic(),
        b"lower" => c.is_ascii_lowercase(),
        b"print" => c.is_ascii_graphic() || c == b' ',
        b"punct" => c.is_ascii_punctuation(),
        // Git's own `isspace`, which leaves out vertical tab and form feed.
        b"space" => matches!(c, b' ' | b'\t' | b'\n' | b'\r'),
        b"upper" => c.is_ascii_uppercase(),
        b"xdigit" => c.is_ascii_hexdigit(),
        _ => return None,
    })
}

fn bytes_to_path(bytes: &[u8]) -> &Path {
    // SAFETY: every byte string here was built from `OsStr` bytes and ASCII
    // `/`, so it is valid encoded bytes.
    Path::new(unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(bytes) })
}
