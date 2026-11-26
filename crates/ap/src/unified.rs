//! The hunks of a unified diff between two texts, as `git diff` prints
//! them.
//!
//! This follows git's xdiff (`xprepare.c`, `xdiffi.c`, and `xemit.c`) with
//! the options `git diff` uses by default: the Myers algorithm with git's
//! heuristics, the indent heuristic, three lines of context, and git's
//! default function names in the hunk headers. Where the output could
//! differ, it is git's, and tests compare it with `git diff --no-index`.

use std::collections::HashMap;

/// How many lines of context a hunk has.
const CONTEXT: usize = 3;
/// The longest function name a hunk header shows, in bytes.
const FUNC_LEN: usize = 80;

const MAX_EQLIMIT: usize = 1024;
const SIMSCAN_WINDOW: usize = 100;
const KPDIS_RUN: usize = 4;
const MAX_COST_MIN: i64 = 256;
const HEUR_MIN_COST: i64 = 256;
const SNAKE_CNT: i64 = 20;
const K_HEUR: i64 = 4;
const LINE_MAX: i64 = i64::MAX;

const MAX_INDENT: i32 = 200;
const MAX_BLANKS: i32 = 20;
const START_OF_FILE_PENALTY: i32 = 1;
const END_OF_FILE_PENALTY: i32 = 21;
const TOTAL_BLANK_WEIGHT: i32 = -30;
const POST_BLANK_WEIGHT: i32 = 6;
const RELATIVE_INDENT_PENALTY: i32 = -4;
const RELATIVE_INDENT_WITH_BLANK_PENALTY: i32 = 10;
const RELATIVE_OUTDENT_PENALTY: i32 = 24;
const RELATIVE_OUTDENT_WITH_BLANK_PENALTY: i32 = 17;
const RELATIVE_DEDENT_PENALTY: i32 = 23;
const RELATIVE_DEDENT_WITH_BLANK_PENALTY: i32 = 17;
const INDENT_WEIGHT: i32 = 60;
const INDENT_HEURISTIC_MAX_SLIDING: usize = 100;

/// A text's lines, each with its `\n`, but a last one that has none.
fn lines(text: &[u8]) -> Vec<&[u8]> {
    text.split_inclusive(|&byte| byte == b'\n').collect()
}

/// One side of the diff.
struct Side<'a> {
    lines: Vec<&'a [u8]>,
    /// Each line's class: the same number for the same bytes.
    class: Vec<usize>,
    /// Whether each line changed, with a `false` before the first and after
    /// the last, so `changed[i + 1]` is line `i`'s.
    changed: Vec<bool>,
    /// The lines the Myers pass compares, by index.
    reference: Vec<usize>,
}

impl Side<'_> {
    fn len(&self) -> usize {
        self.lines.len()
    }

    fn is_changed(&self, line: i64) -> bool {
        usize::try_from(line + 1)
            .ok()
            .and_then(|at| self.changed.get(at))
            .copied()
            .unwrap_or(false)
    }

    fn set_changed(&mut self, line: usize, value: bool) {
        self.changed[line + 1] = value;
    }

    fn hash(&self, index: i64) -> usize {
        self.class[self.reference[index as usize]]
    }
}

/// The hunks of a unified diff from `old` to `new`, each with its header,
/// or nothing when the two are the same.
pub fn hunks(old: &[u8], new: &[u8]) -> Vec<u8> {
    let mut classes: HashMap<&[u8], usize> = HashMap::new();
    let mut counts: Vec<[usize; 2]> = Vec::new();
    let mut sides = [old, new].map(|text| Side {
        lines: lines(text),
        class: Vec::new(),
        changed: Vec::new(),
        reference: Vec::new(),
    });
    for (pass, side) in sides.iter_mut().enumerate() {
        for line in &side.lines {
            let next = classes.len();
            let class = *classes.entry(line).or_insert(next);
            if class == counts.len() {
                counts.push([0, 0]);
            }
            counts[class][pass] += 1;
            side.class.push(class);
        }
        side.changed = vec![false; side.len() + 2];
    }
    let [mut one, mut two] = sides;
    prepare(&mut one, &mut two, &counts);
    compare(&mut one, &mut two);
    compact(&mut one, &mut two);
    compact(&mut two, &mut one);
    emit(&one, &two, &changes(&one, &two))
}

/// Trims the lines both sides start and end with, and leaves out of the
/// Myers pass the lines that can't or needn't match, as `xdl_optimize_ctxs`
/// does.
fn prepare<'a>(one: &mut Side<'a>, two: &mut Side<'a>, counts: &[[usize; 2]]) {
    let limit = one.len().min(two.len());
    let start = (0..limit)
        .find(|&i| one.class[i] != two.class[i])
        .unwrap_or(limit);
    let rest = limit - start;
    let end = (0..rest)
        .find(|&i| one.class[one.len() - 1 - i] != two.class[two.len() - 1 - i])
        .unwrap_or(rest);
    for (side, other) in [(&mut *one, 1), (&mut *two, 0)] {
        let lines = side.len() - end - start;
        let limit = bogosqrt(side.len() as u64).min(MAX_EQLIMIT as u64) as usize;
        let actions: Vec<Action> = (start..start + lines)
            .map(|i| match counts[side.class[i]][other] {
                0 => Action::Discard,
                n if n < limit => Action::Keep,
                _ => Action::Investigate,
            })
            .collect();
        for (i, action) in actions.iter().enumerate() {
            let keep = match action {
                Action::Keep => true,
                Action::Discard => false,
                Action::Investigate => !discards(&actions, i),
            };
            match keep {
                true => side.reference.push(i + start),
                false => side.set_changed(i + start, true),
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Action {
    Discard,
    Keep,
    Investigate,
}

/// Whether a line that matches many lines on the other side sits among
/// lines that match none, so it is better left out, as `xdl_clean_mmatch`
/// decides.
fn discards(actions: &[Action], i: usize) -> bool {
    let start = i.saturating_sub(SIMSCAN_WINDOW);
    let end = (actions.len() - 1).min(i + SIMSCAN_WINDOW);
    let (mut discarded, mut investigated) = (0, 1);
    for action in actions[start..i].iter().rev() {
        match action {
            Action::Discard => discarded += 1,
            Action::Investigate => investigated += 1,
            Action::Keep => break,
        }
    }
    if discarded == 0 {
        return false;
    }
    let (mut discarded_after, mut investigated_after) = (0, 1);
    for action in &actions[i + 1..=end] {
        match action {
            Action::Discard => discarded_after += 1,
            Action::Investigate => investigated_after += 1,
            Action::Keep => break,
        }
    }
    if discarded_after == 0 {
        return false;
    }
    let discarded = discarded + discarded_after;
    let investigated = investigated + investigated_after;
    investigated * KPDIS_RUN < investigated + discarded
}

fn bogosqrt(mut n: u64) -> u64 {
    let mut i = 1;
    while n > 0 {
        i <<= 1;
        n >>= 2;
    }
    i
}

/// The diagonals of the forward and the backward search, indexed from
/// `-two.reference.len() - 1`.
struct Diagonals {
    forward: Vec<i64>,
    backward: Vec<i64>,
    base: i64,
    max_cost: i64,
}

impl Diagonals {
    fn f(&mut self, d: i64) -> &mut i64 {
        &mut self.forward[(d + self.base) as usize]
    }

    fn b(&mut self, d: i64) -> &mut i64 {
        &mut self.backward[(d + self.base) as usize]
    }
}

/// Where a box splits, and whether each half needs a minimal diff.
struct Split {
    i1: i64,
    i2: i64,
    min_lo: bool,
    min_hi: bool,
}

/// Marks the lines the Myers pass finds changed, as `xdl_recs_cmp` does,
/// with a stack in place of its recursion.
fn compare(one: &mut Side, two: &mut Side) {
    let diagonals = one.reference.len() + two.reference.len() + 3;
    let mut kv = Diagonals {
        forward: vec![0; diagonals + 1],
        backward: vec![0; diagonals + 1],
        base: two.reference.len() as i64 + 1,
        max_cost: (bogosqrt(diagonals as u64) as i64).max(MAX_COST_MIN),
    };
    let mut boxes = vec![(
        0,
        one.reference.len() as i64,
        0,
        two.reference.len() as i64,
        false,
    )];
    while let Some((mut off1, mut lim1, mut off2, mut lim2, need_min)) = boxes.pop() {
        while off1 < lim1 && off2 < lim2 && one.hash(off1) == two.hash(off2) {
            off1 += 1;
            off2 += 1;
        }
        while off1 < lim1 && off2 < lim2 && one.hash(lim1 - 1) == two.hash(lim2 - 1) {
            lim1 -= 1;
            lim2 -= 1;
        }
        if off1 == lim1 {
            for i in off2..lim2 {
                let line = two.reference[i as usize];
                two.set_changed(line, true);
            }
        } else if off2 == lim2 {
            for i in off1..lim1 {
                let line = one.reference[i as usize];
                one.set_changed(line, true);
            }
        } else {
            let split = split(one, off1, lim1, two, off2, lim2, &mut kv, need_min);
            boxes.push((split.i1, lim1, split.i2, lim2, split.min_hi));
            boxes.push((off1, split.i1, off2, split.i2, split.min_lo));
        }
    }
}

/// Finds where the shortest edit script of a box crosses its middle, or a
/// good place near it when that costs too much, as `xdl_split` does.
#[allow(
    clippy::too_many_arguments,
    reason = "it keeps the arguments of `xdl_split`, which it follows"
)]
fn split(
    one: &Side,
    off1: i64,
    lim1: i64,
    two: &Side,
    off2: i64,
    lim2: i64,
    kv: &mut Diagonals,
    need_min: bool,
) -> Split {
    let (dmin, dmax) = (off1 - lim2, lim1 - off2);
    let (fmid, bmid) = (off1 - off2, lim1 - lim2);
    let odd = (fmid - bmid) & 1 != 0;
    let (mut fmin, mut fmax) = (fmid, fmid);
    let (mut bmin, mut bmax) = (bmid, bmid);
    *kv.f(fmid) = off1;
    *kv.b(bmid) = lim1;
    let found = |i1, i2| Split {
        i1,
        i2,
        min_lo: true,
        min_hi: true,
    };
    let mut ec = 1;
    loop {
        let mut got_snake = false;
        if fmin > dmin {
            fmin -= 1;
            *kv.f(fmin - 1) = -1;
        } else {
            fmin += 1;
        }
        if fmax < dmax {
            fmax += 1;
            *kv.f(fmax + 1) = -1;
        } else {
            fmax -= 1;
        }
        let mut d = fmax;
        while d >= fmin {
            let mut i1 = match *kv.f(d - 1) >= *kv.f(d + 1) {
                true => *kv.f(d - 1) + 1,
                false => *kv.f(d + 1),
            };
            let prev1 = i1;
            let mut i2 = i1 - d;
            while i1 < lim1 && i2 < lim2 && one.hash(i1) == two.hash(i2) {
                i1 += 1;
                i2 += 1;
            }
            if i1 - prev1 > SNAKE_CNT {
                got_snake = true;
            }
            *kv.f(d) = i1;
            if odd && bmin <= d && d <= bmax && *kv.b(d) <= i1 {
                return found(i1, i2);
            }
            d -= 2;
        }

        if bmin > dmin {
            bmin -= 1;
            *kv.b(bmin - 1) = LINE_MAX;
        } else {
            bmin += 1;
        }
        if bmax < dmax {
            bmax += 1;
            *kv.b(bmax + 1) = LINE_MAX;
        } else {
            bmax -= 1;
        }
        let mut d = bmax;
        while d >= bmin {
            let mut i1 = match *kv.b(d - 1) < *kv.b(d + 1) {
                true => *kv.b(d - 1),
                false => *kv.b(d + 1) - 1,
            };
            let prev1 = i1;
            let mut i2 = i1 - d;
            while i1 > off1 && i2 > off2 && one.hash(i1 - 1) == two.hash(i2 - 1) {
                i1 -= 1;
                i2 -= 1;
            }
            if prev1 - i1 > SNAKE_CNT {
                got_snake = true;
            }
            *kv.b(d) = i1;
            if !odd && fmin <= d && d <= fmax && i1 <= *kv.f(d) {
                return found(i1, i2);
            }
            d -= 2;
        }

        if need_min {
            ec += 1;
            continue;
        }

        if got_snake && ec > HEUR_MIN_COST {
            let mut best = 0;
            let mut spl = found(0, 0);
            let mut d = fmax;
            while d >= fmin {
                let dd = (d - fmid).abs();
                let i1 = *kv.f(d);
                let i2 = i1 - d;
                let v = (i1 - off1) + (i2 - off2) - dd;
                if v > K_HEUR * ec
                    && v > best
                    && off1 + SNAKE_CNT <= i1
                    && i1 < lim1
                    && off2 + SNAKE_CNT <= i2
                    && i2 < lim2
                {
                    let mut k = 1;
                    while one.hash(i1 - k) == two.hash(i2 - k) {
                        if k == SNAKE_CNT {
                            best = v;
                            spl.i1 = i1;
                            spl.i2 = i2;
                            break;
                        }
                        k += 1;
                    }
                }
                d -= 2;
            }
            if best > 0 {
                spl.min_lo = true;
                spl.min_hi = false;
                return spl;
            }

            let mut d = bmax;
            while d >= bmin {
                let dd = (d - bmid).abs();
                let i1 = *kv.b(d);
                let i2 = i1 - d;
                let v = (lim1 - i1) + (lim2 - i2) - dd;
                if v > K_HEUR * ec
                    && v > best
                    && off1 < i1
                    && i1 <= lim1 - SNAKE_CNT
                    && off2 < i2
                    && i2 <= lim2 - SNAKE_CNT
                {
                    let mut k = 0;
                    while one.hash(i1 + k) == two.hash(i2 + k) {
                        if k == SNAKE_CNT - 1 {
                            best = v;
                            spl.i1 = i1;
                            spl.i2 = i2;
                            break;
                        }
                        k += 1;
                    }
                }
                d -= 2;
            }
            if best > 0 {
                spl.min_lo = false;
                spl.min_hi = true;
                return spl;
            }
        }

        if ec >= kv.max_cost {
            let (mut fbest, mut fbest1) = (-1, -1);
            let mut d = fmax;
            while d >= fmin {
                let mut i1 = (*kv.f(d)).min(lim1);
                let mut i2 = i1 - d;
                if lim2 < i2 {
                    i1 = lim2 + d;
                    i2 = lim2;
                }
                if fbest < i1 + i2 {
                    fbest = i1 + i2;
                    fbest1 = i1;
                }
                d -= 2;
            }
            let (mut bbest, mut bbest1) = (LINE_MAX, LINE_MAX);
            let mut d = bmax;
            while d >= bmin {
                let mut i1 = off1.max(*kv.b(d));
                let mut i2 = i1 - d;
                if i2 < off2 {
                    i1 = off2 + d;
                    i2 = off2;
                }
                if i1 + i2 < bbest {
                    bbest = i1 + i2;
                    bbest1 = i1;
                }
                d -= 2;
            }
            return match (lim1 + lim2) - bbest < fbest - (off1 + off2) {
                true => Split {
                    i1: fbest1,
                    i2: fbest - fbest1,
                    min_lo: true,
                    min_hi: false,
                },
                false => Split {
                    i1: bbest1,
                    i2: bbest - bbest1,
                    min_lo: false,
                    min_hi: true,
                },
            };
        }
        ec += 1;
    }
}

/// A run of changed lines on one side, from `start` up to `end`, which may
/// be empty.
#[derive(Clone, Copy)]
struct Group {
    start: i64,
    end: i64,
}

impl Side<'_> {
    fn group_init(&self) -> Group {
        let mut g = Group { start: 0, end: 0 };
        while self.is_changed(g.end) {
            g.end += 1;
        }
        g
    }

    fn group_next(&self, g: &mut Group) -> bool {
        if g.end == self.len() as i64 {
            return false;
        }
        g.start = g.end + 1;
        g.end = g.start;
        while self.is_changed(g.end) {
            g.end += 1;
        }
        true
    }

    fn group_previous(&self, g: &mut Group) -> bool {
        if g.start == 0 {
            return false;
        }
        g.end = g.start - 1;
        g.start = g.end;
        while self.is_changed(g.start - 1) {
            g.start -= 1;
        }
        true
    }

    fn slide_down(&mut self, g: &mut Group) -> bool {
        if g.end < self.len() as i64 && self.class[g.start as usize] == self.class[g.end as usize] {
            self.set_changed(g.start as usize, false);
            self.set_changed(g.end as usize, true);
            g.start += 1;
            g.end += 1;
            while self.is_changed(g.end) {
                g.end += 1;
            }
            return true;
        }
        false
    }

    fn slide_up(&mut self, g: &mut Group) -> bool {
        if g.start > 0 && self.class[g.start as usize - 1] == self.class[g.end as usize - 1] {
            g.start -= 1;
            g.end -= 1;
            self.set_changed(g.start as usize, true);
            self.set_changed(g.end as usize, false);
            while self.is_changed(g.start - 1) {
                g.start -= 1;
            }
            return true;
        }
        false
    }

    /// A line's indent, with a tab to the next multiple of 8, or `-1` for a
    /// line of whitespace alone.
    fn indent(&self, line: usize) -> i32 {
        let mut indent = 0;
        for &byte in self.lines[line] {
            if !is_space(byte) {
                return indent;
            }
            match byte {
                b' ' => indent += 1,
                b'\t' => indent += 8 - indent % 8,
                _ => {}
            }
            if indent >= MAX_INDENT {
                return MAX_INDENT;
            }
        }
        -1
    }

    fn measure(&self, split: i64) -> Measure {
        let len = self.len() as i64;
        let (end_of_file, indent) = match split >= len {
            true => (true, -1),
            false => (false, self.indent(split as usize)),
        };
        let (mut pre_blank, mut pre_indent) = (0, -1);
        let mut i = split - 1;
        while i >= 0 {
            pre_indent = self.indent(i as usize);
            if pre_indent != -1 {
                break;
            }
            pre_blank += 1;
            if pre_blank == MAX_BLANKS {
                pre_indent = 0;
                break;
            }
            i -= 1;
        }
        let (mut post_blank, mut post_indent) = (0, -1);
        let mut i = split + 1;
        while i < len {
            post_indent = self.indent(i as usize);
            if post_indent != -1 {
                break;
            }
            post_blank += 1;
            if post_blank == MAX_BLANKS {
                post_indent = 0;
                break;
            }
            i += 1;
        }
        Measure {
            end_of_file,
            indent,
            pre_blank,
            pre_indent,
            post_blank,
            post_indent,
        }
    }
}

/// What the indent heuristic measures about a place a group could start
/// or end.
struct Measure {
    end_of_file: bool,
    indent: i32,
    pre_blank: i32,
    pre_indent: i32,
    post_blank: i32,
    post_indent: i32,
}

#[derive(Clone, Copy, Default)]
struct Score {
    effective_indent: i32,
    penalty: i32,
}

impl Score {
    fn add(&mut self, m: &Measure) {
        if m.pre_indent == -1 && m.pre_blank == 0 {
            self.penalty += START_OF_FILE_PENALTY;
        }
        if m.end_of_file {
            self.penalty += END_OF_FILE_PENALTY;
        }
        let post_blank = match m.indent {
            -1 => 1 + m.post_blank,
            _ => 0,
        };
        let total_blank = m.pre_blank + post_blank;
        self.penalty += TOTAL_BLANK_WEIGHT * total_blank;
        self.penalty += POST_BLANK_WEIGHT * post_blank;
        let indent = match m.indent {
            -1 => m.post_indent,
            indent => indent,
        };
        let any_blanks = total_blank != 0;
        self.effective_indent += indent;
        if indent == -1 || m.pre_indent == -1 || indent == m.pre_indent {
        } else if indent > m.pre_indent {
            self.penalty += match any_blanks {
                true => RELATIVE_INDENT_WITH_BLANK_PENALTY,
                false => RELATIVE_INDENT_PENALTY,
            };
        } else if m.post_indent != -1 && m.post_indent > indent {
            self.penalty += match any_blanks {
                true => RELATIVE_OUTDENT_WITH_BLANK_PENALTY,
                false => RELATIVE_OUTDENT_PENALTY,
            };
        } else {
            self.penalty += match any_blanks {
                true => RELATIVE_DEDENT_WITH_BLANK_PENALTY,
                false => RELATIVE_DEDENT_PENALTY,
            };
        }
    }

    fn cmp(&self, other: &Score) -> i32 {
        let indents = (self.effective_indent > other.effective_indent) as i32
            - ((self.effective_indent < other.effective_indent) as i32);
        INDENT_WEIGHT * indents + (self.penalty - other.penalty)
    }
}

/// Whitespace as git's own `isspace` has it, which leaves out the vertical
/// tab and the form feed that C's has.
fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

/// Slides each group of changed lines to where it reads best, as
/// `xdl_change_compact` does with the indent heuristic.
fn compact(side: &mut Side, other: &mut Side) {
    let mut g = side.group_init();
    let mut go = other.group_init();
    loop {
        if g.end != g.start {
            let mut earliest_end;
            let mut end_matching_other;
            loop {
                let size = g.end - g.start;
                end_matching_other = -1;
                while side.slide_up(&mut g) {
                    other.group_previous(&mut go);
                }
                earliest_end = g.end;
                if go.end > go.start {
                    end_matching_other = g.end;
                }
                while side.slide_down(&mut g) {
                    other.group_next(&mut go);
                    if go.end > go.start {
                        end_matching_other = g.end;
                    }
                }
                if size == g.end - g.start {
                    break;
                }
            }
            let size = g.end - g.start;
            if g.end == earliest_end {
            } else if end_matching_other != -1 {
                while go.end == go.start {
                    side.slide_up(&mut g);
                    other.group_previous(&mut go);
                }
            } else {
                let mut shift = earliest_end;
                if g.end - size - 1 > shift {
                    shift = g.end - size - 1;
                }
                if g.end - (INDENT_HEURISTIC_MAX_SLIDING as i64) > shift {
                    shift = g.end - INDENT_HEURISTIC_MAX_SLIDING as i64;
                }
                let mut best: Option<(i64, Score)> = None;
                while shift <= g.end {
                    let mut score = Score::default();
                    score.add(&side.measure(shift));
                    score.add(&side.measure(shift - size));
                    if best.is_none_or(|(_, best)| score.cmp(&best) <= 0) {
                        best = Some((shift, score));
                    }
                    shift += 1;
                }
                let best_shift = best.map_or(g.end, |(shift, _)| shift);
                while g.end > best_shift {
                    side.slide_up(&mut g);
                    other.group_previous(&mut go);
                }
            }
        }
        if !side.group_next(&mut g) {
            break;
        }
        other.group_next(&mut go);
    }
}

/// A change: `len1` lines at `i1` in the old text replaced by `len2` at
/// `i2` in the new.
struct Change {
    i1: usize,
    len1: usize,
    i2: usize,
    len2: usize,
}

fn changes(one: &Side, two: &Side) -> Vec<Change> {
    let mut changes = Vec::new();
    let (mut i1, mut i2) = (0, 0);
    while i1 < one.len() || i2 < two.len() {
        if one.is_changed(i1 as i64) || two.is_changed(i2 as i64) {
            let (s1, s2) = (i1, i2);
            while one.is_changed(i1 as i64) {
                i1 += 1;
            }
            while two.is_changed(i2 as i64) {
                i2 += 1;
            }
            changes.push(Change {
                i1: s1,
                len1: i1 - s1,
                i2: s2,
                len2: i2 - s2,
            });
        } else {
            i1 += 1;
            i2 += 1;
        }
    }
    changes
}

/// git's default function name: a line that starts with a letter, `_`, or
/// `$`, cut to 80 bytes, without trailing whitespace.
fn func_name(line: &[u8]) -> Option<&[u8]> {
    let first = *line.first()?;
    if !(first.is_ascii_alphabetic() || first == b'_' || first == b'$') {
        return None;
    }
    let mut name = &line[..line.len().min(FUNC_LEN)];
    while let [rest @ .., last] = name
        && is_space(*last)
    {
        name = rest;
    }
    Some(name)
}

fn emit(one: &Side, two: &Side, changes: &[Change]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut first = 0;
    while first < changes.len() {
        let mut last = first;
        while let Some(next) = changes.get(last + 1)
            && next.i1 - (changes[last].i1 + changes[last].len1) <= 2 * CONTEXT
        {
            last += 1;
        }
        let (start, end) = (&changes[first], &changes[last]);
        let s1 = start.i1.saturating_sub(CONTEXT);
        let s2 = start.i2.saturating_sub(CONTEXT);
        let context = CONTEXT
            .min(one.len() - (end.i1 + end.len1))
            .min(two.len() - (end.i2 + end.len2));
        let e1 = end.i1 + end.len1 + context;
        let e2 = end.i2 + end.len2 + context;
        out.extend(format!("@@ -{} +{} @@", range(s1, e1 - s1), range(s2, e2 - s2)).bytes());
        if let Some(name) = (0..s1).rev().find_map(|line| func_name(one.lines[line])) {
            out.push(b' ');
            out.extend(name);
        }
        out.push(b'\n');
        let line = |out: &mut Vec<u8>, prefix: u8, text: &[u8]| {
            out.push(prefix);
            out.extend(text);
            if !text.ends_with(b"\n") {
                out.extend(b"\n\\ No newline at end of file\n");
            }
        };
        let mut at = s2;
        for change in &changes[first..=last] {
            for context in &two.lines[at..change.i2] {
                line(&mut out, b' ', context);
            }
            for removed in &one.lines[change.i1..change.i1 + change.len1] {
                line(&mut out, b'-', removed);
            }
            for added in &two.lines[change.i2..change.i2 + change.len2] {
                line(&mut out, b'+', added);
            }
            at = change.i2 + change.len2;
        }
        for context in &two.lines[at..e2] {
            line(&mut out, b' ', context);
        }
        first = last + 1;
    }
    out
}

/// A hunk header's range: its first line, counted from 1, and how many
/// lines, which is left out when it is one. An empty range names the line
/// before it.
fn range(start: usize, len: usize) -> String {
    match len {
        0 => format!("{start},0"),
        1 => format!("{}", start + 1),
        _ => format!("{},{len}", start + 1),
    }
}
