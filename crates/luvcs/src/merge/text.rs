//! `merge.text.v1`: deterministic three-way line merge.
//!
//! - Unit: lines split on `\n`; each line keeps its terminator so the final
//!   line's "no trailing newline" state survives byte-exactly. Input need
//!   not be UTF-8 and bytes are never normalized (no CRLF or whitespace
//!   handling).
//! - Diff: histogram diff. The lowest-occurrence common run is the anchor,
//!   the two sides of it recurse; when every common line occurs more than
//!   [`MAX_CHAIN`] times the range falls back to linear-space Myers.
//! - Three-way: diff3 regions over base. A region changed on one side is
//!   taken, identical changes are taken once, differing changes form one
//!   [`ConflictRegion`]. Hunks that overlap or touch collapse into one
//!   region, so nothing is partially applied inside a region.
//! - No common ancestor: equal sides merge, anything else is one region
//!   covering the whole document.
//!
//! Everything here is pure and total: the same input gives the same output
//! on every host, and no input panics.

use std::collections::HashMap;
use std::ops::Range;

/// Lines that occur more often than this in the old side never anchor a
/// histogram split (Git uses the same bound).
const MAX_CHAIN: usize = 64;

/// One region both sides changed differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRegion {
    /// 0-based index of the region among conflicts, in document order.
    pub index: usize,
    /// Byte offset in the merged output where this region's placeholder sits
    /// (the merged output excludes conflicting content; see
    /// [`TextMergeOutcome::Conflicted`]).
    pub merged_offset: usize,
    pub base: Vec<u8>,
    pub left: Vec<u8>,
    pub right: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextMergeOutcome {
    Merged(Vec<u8>),
    /// `merged` holds everything that did merge, with conflicting regions
    /// omitted; `regions` says where they go.
    Conflicted {
        merged: Vec<u8>,
        regions: Vec<ConflictRegion>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Equal,
    Delete,
    Insert,
    Replace,
}

/// One step of a line diff. The ops of a diff tile `old` and `new` in
/// order without gaps: `Equal` and `Replace` have both ranges non-empty,
/// `Delete` has an empty `new`, `Insert` an empty `old`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineDiffOp {
    pub kind: DiffKind,
    pub old: Range<usize>,
    pub new: Range<usize>,
}

/// What to keep for one conflict region when resolving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionChoice {
    Left,
    Right,
    /// Left followed by right, bytes concatenated as they are.
    Both,
    Base,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    ChoiceCountMismatch { expected: usize, got: usize },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::ChoiceCountMismatch { expected, got } => {
                write!(f, "expected {expected} region choices, got {got}")
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// Splits into lines keeping terminators. `""` has no lines; `"a"` is one
/// line without a newline; `"a\n"` is one line with it.
pub fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            out.push(&bytes[start..=i]);
            start = i + 1;
        }
    }
    if start < bytes.len() {
        out.push(&bytes[start..]);
    }
    out
}

/// Three-way merge of `left` and `right` against their common ancestor.
pub fn merge_text(base: Option<&[u8]>, left: &[u8], right: &[u8]) -> TextMergeOutcome {
    if left == right {
        return TextMergeOutcome::Merged(left.to_vec());
    }
    let Some(base) = base else {
        return TextMergeOutcome::Conflicted {
            merged: Vec::new(),
            regions: vec![ConflictRegion {
                index: 0,
                merged_offset: 0,
                base: Vec::new(),
                left: left.to_vec(),
                right: right.to_vec(),
            }],
        };
    };
    if left == base {
        return TextMergeOutcome::Merged(right.to_vec());
    }
    if right == base {
        return TextMergeOutcome::Merged(left.to_vec());
    }

    let base_lines = split_lines(base);
    let left_lines = split_lines(left);
    let right_lines = split_lines(right);
    let mut l = SideCursor::new(&left_lines, diff_lines(&base_lines, &left_lines));
    let mut r = SideCursor::new(&right_lines, diff_lines(&base_lines, &right_lines));

    let mut merged = Vec::with_capacity(left.len().max(right.len()));
    let mut regions = Vec::new();
    let mut cur = 0;
    loop {
        let start = match (l.peek(), r.peek()) {
            (None, None) => break,
            (Some(h), None) | (None, Some(h)) => h.old.start,
            (Some(a), Some(b)) => a.old.start.min(b.old.start),
        };
        push_lines(&mut merged, &base_lines[cur..start]);

        let l_from = l.map(start);
        let r_from = r.map(start);
        let mut end = start;
        let mut l_changed = false;
        let mut r_changed = false;
        loop {
            let mut progressed = false;
            while let Some(h) = l.peek().filter(|h| h.old.start <= end) {
                end = end.max(h.old.end);
                l.consume();
                l_changed = true;
                progressed = true;
            }
            while let Some(h) = r.peek().filter(|h| h.old.start <= end) {
                end = end.max(h.old.end);
                r.consume();
                r_changed = true;
                progressed = true;
            }
            if !progressed {
                break;
            }
        }
        let l_bytes = l.bytes(l_from, l.map(end));
        let r_bytes = r.bytes(r_from, r.map(end));
        match (l_changed, r_changed) {
            (true, false) => merged.extend_from_slice(&l_bytes),
            (false, true) => merged.extend_from_slice(&r_bytes),
            _ if l_bytes == r_bytes => merged.extend_from_slice(&l_bytes),
            _ => regions.push(ConflictRegion {
                index: regions.len(),
                merged_offset: merged.len(),
                base: join_lines(&base_lines[start..end]),
                left: l_bytes,
                right: r_bytes,
            }),
        }
        cur = end;
    }
    push_lines(&mut merged, &base_lines[cur..]);

    if regions.is_empty() {
        TextMergeOutcome::Merged(merged)
    } else {
        TextMergeOutcome::Conflicted { merged, regions }
    }
}

/// Renders the merged text with git-style markers (`<<<<<<< left`,
/// `|||||||  base`, `=======`, `>>>>>>> right`) for display only. A segment
/// that does not end in a newline gets one before the next marker so the
/// markers stay on their own lines; the stored outcome is unchanged.
pub fn render_with_markers(outcome: &TextMergeOutcome) -> Vec<u8> {
    let (merged, regions) = match outcome {
        TextMergeOutcome::Merged(bytes) => return bytes.clone(),
        TextMergeOutcome::Conflicted { merged, regions } => (merged, regions),
    };
    let mut out = Vec::with_capacity(merged.len() + regions.len() * 64);
    let mut cur = 0;
    for region in regions {
        let off = region.merged_offset.clamp(cur, merged.len());
        push_terminated(&mut out, &merged[cur..off]);
        cur = off;
        out.extend_from_slice(b"<<<<<<< left\n");
        push_terminated(&mut out, &region.left);
        out.extend_from_slice(b"||||||| base\n");
        push_terminated(&mut out, &region.base);
        out.extend_from_slice(b"=======\n");
        push_terminated(&mut out, &region.right);
        out.extend_from_slice(b">>>>>>> right\n");
    }
    out.extend_from_slice(&merged[cur..]);
    out
}

/// Applies a choice per region to produce the final bytes. `choices[i]`
/// answers `regions[i]`; the counts must match.
pub fn resolve(
    merged: &[u8],
    regions: &[ConflictRegion],
    choices: &[RegionChoice],
) -> Result<Vec<u8>, ResolveError> {
    if regions.len() != choices.len() {
        return Err(ResolveError::ChoiceCountMismatch {
            expected: regions.len(),
            got: choices.len(),
        });
    }
    let mut choices = choices.iter();
    Ok(assemble(merged, regions, |region| {
        match choices.next().expect("counts checked") {
            RegionChoice::Left => region.left.clone(),
            RegionChoice::Right => region.right.clone(),
            RegionChoice::Both => [region.left.as_slice(), &region.right].concat(),
            RegionChoice::Base => region.base.clone(),
        }
    }))
}

/// The merged bytes with `fill(region)` put where each region goes, in
/// order: the general form of [`resolve`], for answers that are not one of
/// the region's own versions.
pub fn assemble(
    merged: &[u8],
    regions: &[ConflictRegion],
    mut fill: impl FnMut(&ConflictRegion) -> Vec<u8>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(merged.len());
    let mut cur = 0;
    for region in regions {
        let off = region.merged_offset.clamp(cur, merged.len());
        out.extend_from_slice(&merged[cur..off]);
        cur = off;
        out.extend_from_slice(&fill(region));
    }
    out.extend_from_slice(&merged[cur..]);
    out
}

fn push_lines(out: &mut Vec<u8>, lines: &[&[u8]]) {
    for line in lines {
        out.extend_from_slice(line);
    }
}

fn join_lines(lines: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(lines.iter().map(|l| l.len()).sum());
    push_lines(&mut out, lines);
    out
}

fn push_terminated(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(bytes);
    if bytes.last().is_some_and(|&b| b != b'\n') {
        out.push(b'\n');
    }
}

/// Walks one side's hunks (non-`Equal` ops) in base order and maps base
/// line indices to that side's line indices.
struct SideCursor<'a> {
    lines: &'a [&'a [u8]],
    hunks: Vec<LineDiffOp>,
    next: usize,
    /// `side index - base index` for every base index at or after the last
    /// consumed hunk.
    shift: isize,
}

impl<'a> SideCursor<'a> {
    fn new(lines: &'a [&'a [u8]], ops: Vec<LineDiffOp>) -> Self {
        let hunks = ops
            .into_iter()
            .filter(|op| op.kind != DiffKind::Equal)
            .collect();
        SideCursor {
            lines,
            hunks,
            next: 0,
            shift: 0,
        }
    }

    fn peek(&self) -> Option<&LineDiffOp> {
        self.hunks.get(self.next)
    }

    fn consume(&mut self) {
        if let Some(h) = self.hunks.get(self.next) {
            self.shift += h.new.len() as isize - h.old.len() as isize;
            self.next += 1;
        }
    }

    fn map(&self, base_index: usize) -> usize {
        (base_index as isize + self.shift).clamp(0, self.lines.len() as isize) as usize
    }

    fn bytes(&self, from: usize, to: usize) -> Vec<u8> {
        let to = to.clamp(from, self.lines.len());
        join_lines(&self.lines[from..to])
    }
}

/// Line diff of `old` against `new` (histogram with Myers fallback).
pub fn diff_lines(old: &[&[u8]], new: &[&[u8]]) -> Vec<LineDiffOp> {
    let mut d = Differ::new(old, new);
    d.stack.push(Task::Histogram(0..old.len(), 0..new.len()));
    d.run();
    normalize(d.ops)
}

/// Plain linear-space Myers over the whole input; used as the histogram
/// fallback and pinned by tests as the alignment histogram improves on.
#[cfg(test)]
fn diff_lines_myers(old: &[&[u8]], new: &[&[u8]]) -> Vec<LineDiffOp> {
    let mut d = Differ::new(old, new);
    d.stack.push(Task::Myers(0..old.len(), 0..new.len()));
    d.run();
    normalize(d.ops)
}

enum Task {
    Histogram(Range<usize>, Range<usize>),
    Myers(Range<usize>, Range<usize>),
    Equal(Range<usize>, Range<usize>),
}

/// Explicit work stack instead of recursion so deeply nested splits cannot
/// overflow the stack. Tasks are pushed in reverse document order, so when
/// one is popped everything before it has already been emitted.
struct Differ<'a> {
    old: &'a [&'a [u8]],
    new: &'a [&'a [u8]],
    stack: Vec<Task>,
    ops: Vec<LineDiffOp>,
}

struct Anchor {
    a_start: usize,
    b_start: usize,
    len: usize,
    cnt: usize,
}

impl<'a> Differ<'a> {
    fn new(old: &'a [&'a [u8]], new: &'a [&'a [u8]]) -> Self {
        Differ {
            old,
            new,
            stack: Vec::new(),
            ops: Vec::new(),
        }
    }

    fn run(&mut self) {
        while let Some(task) = self.stack.pop() {
            match task {
                Task::Equal(ar, br) => self.emit(DiffKind::Equal, ar, br),
                Task::Histogram(ar, br) => self.histogram(ar, br),
                Task::Myers(ar, br) => self.myers(ar, br),
            }
        }
    }

    fn emit(&mut self, kind: DiffKind, old: Range<usize>, new: Range<usize>) {
        if old.is_empty() && new.is_empty() {
            return;
        }
        self.ops.push(LineDiffOp { kind, old, new });
    }

    /// Emits the common prefix now and schedules the common suffix, then
    /// handles the empty cases. Returns the remaining middle, or `None`
    /// when nothing is left to split.
    fn trim(&mut self, ar: Range<usize>, br: Range<usize>) -> Option<(Range<usize>, Range<usize>)> {
        let (old, new) = (self.old, self.new);
        let a = &old[ar.clone()];
        let b = &new[br.clone()];
        let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
        let suffix = a[prefix..]
            .iter()
            .rev()
            .zip(b[prefix..].iter().rev())
            .take_while(|(x, y)| x == y)
            .count();
        self.emit(
            DiffKind::Equal,
            ar.start..ar.start + prefix,
            br.start..br.start + prefix,
        );
        self.stack.push(Task::Equal(
            ar.end - suffix..ar.end,
            br.end - suffix..br.end,
        ));
        let ar = ar.start + prefix..ar.end - suffix;
        let br = br.start + prefix..br.end - suffix;
        if ar.is_empty() && br.is_empty() {
            None
        } else if ar.is_empty() {
            self.emit(DiffKind::Insert, ar, br);
            None
        } else if br.is_empty() {
            self.emit(DiffKind::Delete, ar, br);
            None
        } else {
            Some((ar, br))
        }
    }

    fn histogram(&mut self, ar: Range<usize>, br: Range<usize>) {
        let Some((ar, br)) = self.trim(ar, br) else {
            return;
        };
        let (old, new) = (self.old, self.new);
        let a = &old[ar.clone()];
        let b = &new[br.clone()];

        // Occurrences of every old line in the range; positions are only
        // kept for lines rare enough to anchor.
        let mut table: HashMap<&[u8], (usize, Vec<usize>)> = HashMap::new();
        for (i, line) in a.iter().enumerate() {
            let entry = table.entry(*line).or_default();
            entry.0 += 1;
            if entry.0 <= MAX_CHAIN {
                entry.1.push(i);
            }
        }
        let occ: Vec<usize> = a.iter().map(|line| table[*line].0).collect();

        let mut best: Option<Anchor> = None;
        let mut any_common = false;
        let mut bi = 0;
        while bi < b.len() {
            let mut b_next = bi + 1;
            if let Some((count, positions)) = table.get(b[bi]) {
                any_common = true;
                if *count <= MAX_CHAIN {
                    for &p in positions {
                        let mut a_s = p;
                        let mut b_s = bi;
                        while a_s > 0 && b_s > 0 && a[a_s - 1] == b[b_s - 1] {
                            a_s -= 1;
                            b_s -= 1;
                        }
                        let mut a_e = p + 1;
                        let mut b_e = bi + 1;
                        while a_e < a.len() && b_e < b.len() && a[a_e] == b[b_e] {
                            a_e += 1;
                            b_e += 1;
                        }
                        let len = a_e - a_s;
                        let cnt = occ[a_s..a_e].iter().copied().min().unwrap_or(usize::MAX);
                        let better = match &best {
                            None => true,
                            Some(cur) => cnt < cur.cnt || (cnt == cur.cnt && len > cur.len),
                        };
                        if better {
                            best = Some(Anchor {
                                a_start: a_s,
                                b_start: b_s,
                                len,
                                cnt,
                            });
                        }
                        b_next = b_next.max(b_e);
                    }
                }
            }
            bi = b_next;
        }

        match best {
            Some(anchor) => {
                let a_mid = ar.start + anchor.a_start..ar.start + anchor.a_start + anchor.len;
                let b_mid = br.start + anchor.b_start..br.start + anchor.b_start + anchor.len;
                self.stack
                    .push(Task::Histogram(a_mid.end..ar.end, b_mid.end..br.end));
                self.stack.push(Task::Equal(a_mid.clone(), b_mid.clone()));
                self.stack.push(Task::Histogram(
                    ar.start..a_mid.start,
                    br.start..b_mid.start,
                ));
            }
            None if any_common => self.stack.push(Task::Myers(ar, br)),
            None => self.emit(DiffKind::Replace, ar, br),
        }
    }

    fn myers(&mut self, ar: Range<usize>, br: Range<usize>) {
        let Some((ar, br)) = self.trim(ar, br) else {
            return;
        };
        match find_middle_snake(&self.old[ar.clone()], &self.new[br.clone()]) {
            Some((x, y)) if (x, y) != (0, 0) && (x, y) != (ar.len(), br.len()) => {
                let (x, y) = (ar.start + x, br.start + y);
                self.stack.push(Task::Myers(x..ar.end, y..br.end));
                self.stack.push(Task::Myers(ar.start..x, br.start..y));
            }
            _ => self.emit(DiffKind::Replace, ar, br),
        }
    }
}

/// Myers' linear-space middle snake. Returns the start of the middle snake
/// as `(x, y)` relative to the ranges, or `None` if the search exhausted
/// its band (which cannot happen for consistent input, but the caller
/// falls back to a `Replace` rather than trusting that).
fn find_middle_snake(a: &[&[u8]], b: &[&[u8]]) -> Option<(usize, usize)> {
    let n = a.len();
    let m = b.len();
    if n == 0 || m == 0 {
        return None;
    }
    let delta = n as isize - m as isize;
    let odd = delta.rem_euclid(2) == 1;
    let d_max = ((n + m).div_ceil(2) + 1) as isize;
    let offset = d_max + 2;
    let size = (2 * d_max + 5) as usize;
    let mut vf = vec![0usize; size];
    let mut vb = vec![0usize; size];
    let idx = |k: isize| (k + offset) as usize;

    for d in 0..d_max {
        let mut k = d;
        while k >= -d {
            let mut x = if k == -d || (k != d && vf[idx(k - 1)] < vf[idx(k + 1)]) {
                vf[idx(k + 1)]
            } else {
                vf[idx(k - 1)] + 1
            };
            let mut y = (x as isize - k).max(0) as usize;
            let (x0, y0) = (x, y);
            while x < n && y < m && a[x] == b[y] {
                x += 1;
                y += 1;
            }
            vf[idx(k)] = x;
            if odd
                && (k - delta).abs() < d
                && vf[idx(k)] + vb[idx(delta - k)] >= n
                && x0 <= n
                && y0 <= m
            {
                return Some((x0, y0));
            }
            k -= 2;
        }

        let mut k = d;
        while k >= -d {
            let mut x = if k == -d || (k != d && vb[idx(k - 1)] < vb[idx(k + 1)]) {
                vb[idx(k + 1)]
            } else {
                vb[idx(k - 1)] + 1
            };
            let mut y = (x as isize - k).max(0) as usize;
            while x < n && y < m && a[n - 1 - x] == b[m - 1 - y] {
                x += 1;
                y += 1;
            }
            vb[idx(k)] = x;
            if !odd
                && (k - delta).abs() <= d
                && vb[idx(k)] + vf[idx(delta - k)] >= n
                && x <= n
                && y <= m
            {
                return Some((n - x, m - y));
            }
            k -= 2;
        }
    }
    None
}

/// Coalesces the raw op stream: consecutive `Equal` ops merge, and any run
/// of changes merges into one `Delete` / `Insert` / `Replace`.
fn normalize(ops: Vec<LineDiffOp>) -> Vec<LineDiffOp> {
    let mut out: Vec<LineDiffOp> = Vec::with_capacity(ops.len());
    for op in ops {
        if op.old.is_empty() && op.new.is_empty() {
            continue;
        }
        if let Some(last) = out.last_mut() {
            let same_class = (last.kind == DiffKind::Equal) == (op.kind == DiffKind::Equal);
            if same_class && last.old.end == op.old.start && last.new.end == op.new.start {
                last.old.end = op.old.end;
                last.new.end = op.new.end;
                last.kind = classify(last.kind, &last.old, &last.new);
                continue;
            }
        }
        let kind = classify(op.kind, &op.old, &op.new);
        out.push(LineDiffOp { kind, ..op });
    }
    out
}

fn classify(kind: DiffKind, old: &Range<usize>, new: &Range<usize>) -> DiffKind {
    if kind == DiffKind::Equal {
        DiffKind::Equal
    } else if new.is_empty() {
        DiffKind::Delete
    } else if old.is_empty() {
        DiffKind::Insert
    } else {
        DiffKind::Replace
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<&[u8]> {
        split_lines(s.as_bytes())
    }

    fn op(kind: DiffKind, old: Range<usize>, new: Range<usize>) -> LineDiffOp {
        LineDiffOp { kind, old, new }
    }

    /// Checks that `ops` tile both inputs without gaps and rebuild `new`
    /// from `old`.
    fn check_diff(old: &[&[u8]], new: &[&[u8]], ops: &[LineDiffOp]) {
        let (mut o, mut n) = (0, 0);
        let mut rebuilt: Vec<&[u8]> = Vec::new();
        for op in ops {
            assert_eq!(op.old.start, o, "old gap in {ops:?}");
            assert_eq!(op.new.start, n, "new gap in {ops:?}");
            match op.kind {
                DiffKind::Equal => {
                    assert!(!op.old.is_empty() && op.old.len() == op.new.len());
                    assert_eq!(&old[op.old.clone()], &new[op.new.clone()]);
                    rebuilt.extend_from_slice(&old[op.old.clone()]);
                }
                DiffKind::Delete => assert!(!op.old.is_empty() && op.new.is_empty()),
                DiffKind::Insert => {
                    assert!(op.old.is_empty() && !op.new.is_empty());
                    rebuilt.extend_from_slice(&new[op.new.clone()]);
                }
                DiffKind::Replace => {
                    assert!(!op.old.is_empty() && !op.new.is_empty());
                    rebuilt.extend_from_slice(&new[op.new.clone()]);
                }
            }
            o = op.old.end;
            n = op.new.end;
        }
        assert_eq!(o, old.len());
        assert_eq!(n, new.len());
        assert_eq!(rebuilt, new);
        // Changes never sit next to each other after normalization.
        for pair in ops.windows(2) {
            assert!(
                (pair[0].kind == DiffKind::Equal) != (pair[1].kind == DiffKind::Equal),
                "uncoalesced ops {ops:?}"
            );
        }
    }

    fn equal_lines(ops: &[LineDiffOp]) -> usize {
        ops.iter()
            .filter(|o| o.kind == DiffKind::Equal)
            .map(|o| o.old.len())
            .sum()
    }

    fn conflicted(outcome: TextMergeOutcome) -> (Vec<u8>, Vec<ConflictRegion>) {
        match outcome {
            TextMergeOutcome::Conflicted { merged, regions } => (merged, regions),
            TextMergeOutcome::Merged(bytes) => {
                panic!(
                    "expected a conflict, got {:?}",
                    String::from_utf8_lossy(&bytes)
                )
            }
        }
    }

    fn region(
        index: usize,
        merged_offset: usize,
        base: &str,
        left: &str,
        right: &str,
    ) -> ConflictRegion {
        ConflictRegion {
            index,
            merged_offset,
            base: base.as_bytes().to_vec(),
            left: left.as_bytes().to_vec(),
            right: right.as_bytes().to_vec(),
        }
    }

    fn merged(s: &str) -> TextMergeOutcome {
        TextMergeOutcome::Merged(s.as_bytes().to_vec())
    }

    fn merge(base: &str, left: &str, right: &str) -> TextMergeOutcome {
        merge_text(Some(base.as_bytes()), left.as_bytes(), right.as_bytes())
    }

    #[test]
    fn split_lines_keeps_terminators() {
        assert!(split_lines(b"").is_empty());
        assert_eq!(split_lines(b"a"), vec![&b"a"[..]]);
        assert_eq!(split_lines(b"a\n"), vec![&b"a\n"[..]]);
        assert_eq!(
            split_lines(b"a\n\nb"),
            vec![&b"a\n"[..], &b"\n"[..], &b"b"[..]]
        );
        assert_eq!(
            split_lines(b"\r\n\xff\n"),
            vec![&b"\r\n"[..], &b"\xff\n"[..]]
        );
    }

    #[test]
    fn diff_basic_ops() {
        let old = lines("a\nb\nc\n");
        assert_eq!(
            diff_lines(&old, &old),
            vec![op(DiffKind::Equal, 0..3, 0..3)]
        );
        assert!(diff_lines(&[], &[]).is_empty());
        assert_eq!(
            diff_lines(&old, &[]),
            vec![op(DiffKind::Delete, 0..3, 0..0)]
        );
        assert_eq!(
            diff_lines(&[], &old),
            vec![op(DiffKind::Insert, 0..0, 0..3)]
        );
        let new = lines("a\nX\nc\n");
        assert_eq!(
            diff_lines(&old, &new),
            vec![
                op(DiffKind::Equal, 0..1, 0..1),
                op(DiffKind::Replace, 1..2, 1..2),
                op(DiffKind::Equal, 2..3, 2..3),
            ]
        );
        let new = lines("x\ny\n");
        assert_eq!(
            diff_lines(&old, &new),
            vec![op(DiffKind::Replace, 0..3, 0..2)]
        );
    }

    #[test]
    fn histogram_anchors_on_unique_line_where_lcs_matches_braces() {
        // Two blocks that share their `margin` / `}` lines; `.foo1` is moved
        // after `.bar`, grows a line and a trailing blank. LCS happily matches the braces of
        // `.foo1` to the braces of `.bar` (five equal lines) and produces an
        // edit script nobody can read. Histogram anchors on the unique
        // `.bar {` run and reports the move as one delete and one insert.
        let old = lines(".foo1 {\n    margin: 0;\n}\n\n.bar {\n    margin: 0;\n}\n");
        let new =
            lines(".bar {\n    margin: 0;\n}\n\n.foo1 {\n    margin: 0;\n    color: green;\n}\n\n");
        let hist = diff_lines(&old, &new);
        check_diff(&old, &new, &hist);
        assert_eq!(
            hist,
            vec![
                op(DiffKind::Delete, 0..4, 0..0),
                op(DiffKind::Equal, 4..7, 0..3),
                op(DiffKind::Insert, 7..7, 3..9),
            ]
        );
        let myers = diff_lines_myers(&old, &new);
        check_diff(&old, &new, &myers);
        assert_eq!(
            equal_lines(&myers),
            5,
            "LCS alignment is the longer, worse one"
        );
        assert_eq!(equal_lines(&hist), 3);
    }

    #[test]
    fn histogram_prefers_rarer_anchor_over_frequent_line() {
        // `x` is frequent, `u` unique. Even though matching every `x` would
        // be a longer common subsequence, the unique `u` is the anchor.
        let old = lines("x\nx\nu\nx\nx\n");
        let new = lines("x\nx\nx\nx\nu\n");
        let ops = diff_lines(&old, &new);
        check_diff(&old, &new, &ops);
        let anchored = ops
            .iter()
            .any(|o| o.kind == DiffKind::Equal && o.old.contains(&2) && o.new.contains(&4));
        assert!(anchored, "u was not matched to u: {ops:?}");
    }

    #[test]
    fn falls_back_to_myers_when_every_common_line_is_frequent() {
        let repeated: Vec<&[u8]> = std::iter::repeat_n(&b"same\n"[..], MAX_CHAIN + 10).collect();
        let mut old = repeated.clone();
        old.insert(20, b"old-only\n");
        let mut new = repeated.clone();
        new.insert(50, b"new-only\n");
        new.push(b"tail\n");
        let ops = diff_lines(&old, &new);
        check_diff(&old, &new, &ops);
        assert_eq!(equal_lines(&ops), MAX_CHAIN + 10);
    }

    #[test]
    fn one_side_change_is_taken() {
        let base = "a\nb\nc\n";
        assert_eq!(merge(base, "a\nB\nc\n", base), merged("a\nB\nc\n"));
        assert_eq!(merge(base, base, "a\nb\nc\nd\n"), merged("a\nb\nc\nd\n"));
        assert_eq!(merge(base, "", base), merged(""));
    }

    #[test]
    fn identical_change_on_both_sides_is_taken_once() {
        assert_eq!(
            merge("a\nb\nc\n", "a\nX\nc\n", "a\nX\nc\n"),
            merged("a\nX\nc\n")
        );
        // Same change plus an independent one on the right.
        assert_eq!(
            merge("a\nb\nc\nd\ne\n", "a\nX\nc\nd\ne\n", "a\nX\nc\nd\nE\n"),
            merged("a\nX\nc\nd\nE\n")
        );
    }

    #[test]
    fn independent_changes_merge_cleanly() {
        assert_eq!(
            merge("a\nb\nc\nd\ne\n", "A\nb\nc\nd\ne\n", "a\nb\nc\nd\nE\n"),
            merged("A\nb\nc\nd\nE\n")
        );
        // Insert on the left, delete on the right, separated by an equal line.
        assert_eq!(
            merge("a\nb\nc\nd\n", "a\nI\nb\nc\nd\n", "a\nb\nc\n"),
            merged("a\nI\nb\nc\n")
        );
        // Independent inserts at different ends.
        assert_eq!(
            merge("m\n", "top\nm\n", "m\nbottom\n"),
            merged("top\nm\nbottom\n")
        );
    }

    #[test]
    fn overlapping_change_conflicts_with_exact_bytes() {
        let (m, regions) = conflicted(merge("a\nb\nc\n", "a\nL\nc\n", "a\nR\nc\n"));
        assert_eq!(m, b"a\nc\n");
        assert_eq!(regions, vec![region(0, 2, "b\n", "L\n", "R\n")]);

        // Deleted on one side, edited on the other.
        let (m, regions) = conflicted(merge("a\nb\nc\n", "a\nc\n", "a\nB\nc\n"));
        assert_eq!(m, b"a\nc\n");
        assert_eq!(regions, vec![region(0, 2, "b\n", "", "B\n")]);

        // Two separate conflicts keep their own index and offset.
        let (m, regions) = conflicted(merge(
            "a\nb\nc\nd\ne\n",
            "L1\nb\nc\nd\nL2\n",
            "R1\nb\nc\nd\nR2\n",
        ));
        assert_eq!(m, b"b\nc\nd\n");
        assert_eq!(
            regions,
            vec![
                region(0, 0, "a\n", "L1\n", "R1\n"),
                region(1, 6, "e\n", "L2\n", "R2\n")
            ]
        );
    }

    #[test]
    fn adjacent_hunks_collapse_into_one_region() {
        // Left edits `b`, right edits `c`: the hunks touch, so one region.
        let (m, regions) = conflicted(merge("a\nb\nc\nd\n", "a\nL\nc\nd\n", "a\nb\nR\nd\n"));
        assert_eq!(m, b"a\nd\n");
        assert_eq!(regions, vec![region(0, 2, "b\nc\n", "L\nc\n", "b\nR\n")]);

        // An insert at the boundary of the other side's edit also joins it.
        let (m, regions) = conflicted(merge("a\nb\nc\n", "a\nI\nb\nc\n", "a\nR\nc\n"));
        assert_eq!(m, b"a\nc\n");
        assert_eq!(regions, vec![region(0, 2, "b\n", "I\nb\n", "R\n")]);

        // Both sides insert at the same place differently.
        let (m, regions) = conflicted(merge("a\nb\n", "a\nL\nb\n", "a\nR\nb\n"));
        assert_eq!(m, b"a\nb\n");
        assert_eq!(regions, vec![region(0, 2, "", "L\n", "R\n")]);
    }

    #[test]
    fn missing_trailing_newline_is_preserved() {
        assert_eq!(merge("a\nb\nc", "A\nb\nc", "a\nb\nC"), merged("A\nb\nC"));
        assert_eq!(merge("a\nb\n", "a\nb\nc", "A\nb\n"), merged("A\nb\nc"));
        // Adding the final newline is a change to the last line.
        let (m, regions) = conflicted(merge("a\nb", "a\nb\n", "a\nB"));
        assert_eq!(m, b"a\n");
        assert_eq!(regions, vec![region(0, 2, "b", "b\n", "B")]);
        // Bytes are never normalized.
        assert_eq!(
            merge("a\r\nb\r\n", "a\r\nb\r\nc\r\n", "A\r\nb\r\n"),
            merged("A\r\nb\r\nc\r\n")
        );
    }

    #[test]
    fn no_common_ancestor() {
        assert_eq!(merge_text(None, b"same\n", b"same\n"), merged("same\n"));
        let (m, regions) = conflicted(merge_text(None, b"l\n", b"r\nr\n"));
        assert!(m.is_empty());
        assert_eq!(regions, vec![region(0, 0, "", "l\n", "r\nr\n")]);
        let (m, regions) = conflicted(merge_text(None, b"", b"r"));
        assert!(m.is_empty());
        assert_eq!(regions, vec![region(0, 0, "", "", "r")]);
    }

    #[test]
    fn resolve_round_trips_each_choice() {
        let (m, regions) = conflicted(merge("a\nb\nc\n", "a\nL\nc\n", "a\nR\nc\n"));
        let pick = |c| resolve(&m, &regions, &[c]).unwrap();
        assert_eq!(pick(RegionChoice::Left), b"a\nL\nc\n");
        assert_eq!(pick(RegionChoice::Right), b"a\nR\nc\n");
        assert_eq!(pick(RegionChoice::Both), b"a\nL\nR\nc\n");
        assert_eq!(pick(RegionChoice::Base), b"a\nb\nc\n");
        assert_eq!(
            resolve(&m, &regions, &[]),
            Err(ResolveError::ChoiceCountMismatch {
                expected: 1,
                got: 0
            })
        );
        assert_eq!(resolve(b"x", &[], &[]).unwrap(), b"x");

        let (m, regions) = conflicted(merge(
            "a\nb\nc\nd\ne\n",
            "L1\nb\nc\nd\nL2\n",
            "R1\nb\nc\nd\nR2\n",
        ));
        assert_eq!(
            resolve(&m, &regions, &[RegionChoice::Left, RegionChoice::Right]).unwrap(),
            b"L1\nb\nc\nd\nR2\n"
        );
    }

    #[test]
    fn markers_render_for_display() {
        let outcome = merge("a\nb\nc\n", "a\nL\nc\n", "a\nR\nc\n");
        assert_eq!(
            String::from_utf8(render_with_markers(&outcome)).unwrap(),
            "a\n<<<<<<< left\nL\n||||||| base\nb\n=======\nR\n>>>>>>> right\nc\n"
        );
        assert_eq!(render_with_markers(&merged("x\n")), b"x\n");
        // A side without a trailing newline still keeps markers on their own lines.
        let outcome = merge_text(None, b"l", b"r");
        assert_eq!(
            String::from_utf8(render_with_markers(&outcome)).unwrap(),
            "<<<<<<< left\nl\n||||||| base\n=======\nr\n>>>>>>> right\n"
        );
    }

    #[test]
    fn deterministic_and_mirrored() {
        let base = "a\nb\nc\nd\ne\nf\ng\n";
        let left = "A\nb\nc\nL\ne\nf\nL2\n";
        let right = "a\nb\nc\nR\ne\nf\nR2\n";
        let first = merge(base, left, right);
        let second = merge(base, left, right);
        assert_eq!(first, second);
        let (m, regions) = conflicted(first);
        assert_eq!(m, b"A\nb\nc\ne\nf\n");
        let (m2, mirrored) = conflicted(merge(base, right, left));
        assert_eq!(m, m2);
        let swapped: Vec<ConflictRegion> = regions
            .iter()
            .cloned()
            .map(|r| ConflictRegion {
                left: r.right,
                right: r.left,
                ..r
            })
            .collect();
        assert_eq!(mirrored, swapped);
    }

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }

        fn below(&mut self, n: u64) -> usize {
            (self.next() % n) as usize
        }

        fn text(&mut self) -> Vec<u8> {
            const ALPHABET: [&[u8]; 6] = [b"a\n", b"b\n", b"c\n", b"\n", b"d", b"a\r\n"];
            let len = self.below(9);
            let mut out = Vec::new();
            for i in 0..len {
                let piece = ALPHABET[self.below(ALPHABET.len() as u64)];
                // Unterminated lines are only valid at the very end.
                if piece.ends_with(b"\n") || i + 1 == len {
                    out.extend_from_slice(piece);
                } else {
                    out.extend_from_slice(b"d\n");
                }
            }
            out
        }

        fn edit(&mut self, from: &[u8]) -> Vec<u8> {
            let mut lines: Vec<Vec<u8>> = split_lines(from).iter().map(|l| l.to_vec()).collect();
            for _ in 0..self.below(3) {
                let at = self.below(lines.len() as u64 + 1);
                match self.below(3) {
                    0 if at < lines.len() => {
                        lines.remove(at);
                    }
                    1 => lines.insert(at, b"n\n".to_vec()),
                    _ if at < lines.len() => lines[at] = b"m\n".to_vec(),
                    _ => {}
                }
            }
            let mut out: Vec<u8> = lines.concat();
            if self.below(4) == 0 && out.ends_with(b"\n") {
                out.pop();
            }
            out
        }
    }

    #[test]
    fn random_inputs_never_panic_and_keep_invariants() {
        let mut rng = Lcg(0x5eed);
        for _ in 0..3000 {
            let base = rng.text();
            let left = rng.edit(&base);
            let right = if rng.below(2) == 0 {
                rng.edit(&base)
            } else {
                rng.text()
            };
            let (bl, ll, rl) = (split_lines(&base), split_lines(&left), split_lines(&right));
            check_diff(&bl, &ll, &diff_lines(&bl, &ll));
            check_diff(&ll, &rl, &diff_lines_myers(&ll, &rl));

            assert_eq!(
                merge_text(Some(&base), &left, &base),
                TextMergeOutcome::Merged(left.clone())
            );
            assert_eq!(
                merge_text(Some(&base), &left, &left),
                TextMergeOutcome::Merged(left.clone())
            );
            let outcome = merge_text(Some(&base), &left, &right);
            assert_eq!(outcome, merge_text(Some(&base), &left, &right));
            render_with_markers(&outcome);
            if let TextMergeOutcome::Conflicted { merged, regions } = &outcome {
                assert!(!regions.is_empty());
                let mut last = 0;
                for (i, r) in regions.iter().enumerate() {
                    assert_eq!(r.index, i);
                    assert!(r.merged_offset >= last && r.merged_offset <= merged.len());
                    assert_ne!(r.left, r.right);
                    last = r.merged_offset;
                }
                // Regions sit on line boundaries of the merged output, and
                // every choice resolves without error.
                for r in regions {
                    let at_boundary = r.merged_offset == 0 || merged[r.merged_offset - 1] == b'\n';
                    assert!(at_boundary || r.merged_offset == merged.len());
                }
                for choice in [
                    RegionChoice::Left,
                    RegionChoice::Right,
                    RegionChoice::Both,
                    RegionChoice::Base,
                ] {
                    let choices = vec![choice; regions.len()];
                    let resolved = resolve(merged, regions, &choices).unwrap();
                    let expected: usize = merged.len()
                        + regions
                            .iter()
                            .map(|r| match choice {
                                RegionChoice::Left => r.left.len(),
                                RegionChoice::Right => r.right.len(),
                                RegionChoice::Both => r.left.len() + r.right.len(),
                                RegionChoice::Base => r.base.len(),
                            })
                            .sum::<usize>();
                    assert_eq!(resolved.len(), expected);
                }
                let mirrored = merge_text(Some(&base), &right, &left);
                if let TextMergeOutcome::Conflicted {
                    merged: m2,
                    regions: r2,
                } = mirrored
                {
                    assert_eq!(&m2, merged);
                    assert_eq!(r2.len(), regions.len());
                    for (a, b) in regions.iter().zip(&r2) {
                        assert_eq!(
                            (&a.left, &a.right, &a.base, a.merged_offset),
                            (&b.right, &b.left, &b.base, b.merged_offset)
                        );
                    }
                } else {
                    panic!("mirror merged where original conflicted");
                }
            }
        }
    }

    #[test]
    fn large_input_does_not_overflow_the_stack() {
        // Every line unique and interleaved with inserts so the histogram
        // split chain is as deep as the input is long.
        let old: Vec<String> = (0..3_000).map(|i| format!("{i}\n")).collect();
        let new: Vec<String> = old
            .iter()
            .flat_map(|l| [l.clone(), format!("x{l}")])
            .collect();
        let old: Vec<&[u8]> = old.iter().map(|s| s.as_bytes()).collect();
        let new: Vec<&[u8]> = new.iter().map(|s| s.as_bytes()).collect();
        let ops = diff_lines(&old, &new);
        check_diff(&old, &new, &ops);
        let ops = diff_lines_myers(&old, &new);
        check_diff(&old, &new, &ops);
    }
}
