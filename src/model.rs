//! Line diff, change blocks, and the IntelliJ-style scroll mapping between the two sides.

use similar::{Algorithm, ChangeTag, DiffTag, TextDiff};
use std::ops::Range;
use std::time::Duration;

/// One run of changed lines: old `[a0, a1)` against new `[b0, b1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block {
    pub a0: usize,
    pub a1: usize,
    pub b0: usize,
    pub b1: usize,
}

impl Block {
    pub fn kind(&self) -> BlockKind {
        match (self.a0 == self.a1, self.b0 == self.b1) {
            (true, _) => BlockKind::Insert,
            (_, true) => BlockKind::Delete,
            _ => BlockKind::Modify,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Insert,
    Delete,
    Modify,
}

pub struct Model {
    pub old: Vec<String>,
    pub new: Vec<String>,
    /// Changes worth showing.
    pub blocks: Vec<Block>,
    /// Changes that only touch ignored text (comments, blank lines): kept so both sides stay
    /// aligned, but not highlighted or counted.
    pub quiet: Vec<Block>,
    /// Changed byte ranges per line, for word-level highlights inside modified blocks.
    pub old_inline: Vec<Vec<Range<usize>>>,
    pub new_inline: Vec<Vec<Range<usize>>>,
}

pub fn split_lines(text: &str) -> Vec<String> {
    let text = text.replace("\r\n", "\n");
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

impl Model {
    #[cfg(test)]
    pub fn new(old_text: &str, new_text: &str) -> Self {
        Self::from_lines(split_lines(old_text), split_lines(new_text), None)
    }

    /// Diff two files by line. With `keys`, lines compare by their keys instead of their text
    /// (whitespace or comments removed), and a change whose lines all have empty keys is quiet.
    pub fn from_lines(old: Vec<String>, new: Vec<String>, keys: Option<(Vec<String>, Vec<String>)>) -> Self {
        let (old_keys, new_keys) = keys.clone().unwrap_or_else(|| (old.clone(), new.clone()));
        let old_refs: Vec<&str> = old_keys.iter().map(String::as_str).collect();
        let new_refs: Vec<&str> = new_keys.iter().map(String::as_str).collect();
        let diff = TextDiff::configure()
            .algorithm(Algorithm::Myers)
            .timeout(Duration::from_secs(2))
            .diff_slices(&old_refs, &new_refs);

        let mut all: Vec<Block> = Vec::new();
        for op in diff.ops() {
            let (tag, a, b) = op.as_tag_tuple();
            if tag == DiffTag::Equal {
                continue;
            }
            // Adjacent delete + insert ops form one block.
            match all.last_mut() {
                Some(last) if last.a1 == a.start && last.b1 == b.start => {
                    last.a1 = a.end;
                    last.b1 = b.end;
                }
                _ => all.push(Block { a0: a.start, a1: a.end, b0: b.start, b1: b.end }),
            }
        }
        let is_quiet = |block: &Block| {
            keys.is_some()
                && old_keys[block.a0..block.a1].iter().chain(&new_keys[block.b0..block.b1]).all(String::is_empty)
        };
        let (quiet, blocks): (Vec<Block>, Vec<Block>) = all.into_iter().partition(is_quiet);

        let mut old_inline = vec![Vec::new(); old.len()];
        let mut new_inline = vec![Vec::new(); new.len()];
        for block in blocks.iter().filter(|b| b.kind() == BlockKind::Modify) {
            for (a, b) in pair_lines(&old[block.a0..block.a1], &new[block.b0..block.b1]) {
                let (a, b) = (block.a0 + a, block.b0 + b);
                let (old_ranges, new_ranges) = inline_changes(&old[a], &new[b]);
                old_inline[a] = old_ranges;
                new_inline[b] = new_ranges;
            }
        }

        Model { old, new, blocks, quiet, old_inline, new_inline }
    }
}

/// The comparison key of a line: without whitespace (`-w`) and/or without comments.
pub fn line_key(line: &str, comments: &[Range<usize>], ignore_whitespace: bool) -> String {
    let mut key = String::with_capacity(line.len());
    let mut last = 0;
    for range in comments {
        key.push_str(&line[last..range.start.min(line.len())]);
        last = range.end.min(line.len());
    }
    key.push_str(&line[last.min(line.len())..]);
    if ignore_whitespace {
        key.retain(|c| !c.is_whitespace());
        key
    } else {
        key.trim_end().to_owned()
    }
}

/// Lines this similar (0 to 1) are treated as edits of each other.
const PAIR_RATIO: f32 = 0.5;
/// Larger blocks are paired by position: similarity search is quadratic.
const MAX_PAIRING: usize = 80;

fn similarity(old: &str, new: &str) -> f32 {
    TextDiff::configure().timeout(Duration::from_millis(20)).diff_chars(old.trim(), new.trim()).ratio()
}

/// Pair each old line of a changed block with its most similar new line, keeping both orders,
/// so word highlights compare a line with its real edit rather than with whatever sits at the
/// same position.
fn pair_lines(old: &[String], new: &[String]) -> Vec<(usize, usize)> {
    if old.len() > MAX_PAIRING || new.len() > MAX_PAIRING {
        return (0..old.len().min(new.len())).filter(|&i| similarity(&old[i], &new[i]) >= PAIR_RATIO).map(|i| (i, i)).collect();
    }
    // Best total similarity over ordered pairings (a small alignment table).
    let (n, m) = (old.len(), new.len());
    let mut score = vec![vec![0f32; m + 1]; n + 1];
    let mut ratio = vec![vec![0f32; m]; n];
    for i in 0..n {
        for j in 0..m {
            ratio[i][j] = similarity(&old[i], &new[j]);
        }
    }
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            let take = if ratio[i][j] >= PAIR_RATIO { ratio[i][j] + score[i + 1][j + 1] } else { 0.0 };
            score[i][j] = take.max(score[i + 1][j]).max(score[i][j + 1]);
        }
    }
    let (mut pairs, mut i, mut j) = (Vec::new(), 0, 0);
    while i < n && j < m {
        if ratio[i][j] >= PAIR_RATIO && score[i][j] == ratio[i][j] + score[i + 1][j + 1] {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if score[i][j] == score[i + 1][j] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs
}

/// Split code into identifier/number runs, whitespace runs, and single other characters, so a
/// changed number inside `a.b(27500)` is its own token.
fn code_tokens(line: &str) -> Vec<&str> {
    let class = |c: char| if c.is_alphanumeric() || c == '_' { 1 } else if c.is_whitespace() { 2 } else { 0 };
    let mut tokens = Vec::new();
    let mut start = 0;
    let mut previous: Option<u8> = None;
    for (index, c) in line.char_indices() {
        let kind = class(c);
        if previous.is_some() && (kind == 0 || Some(kind) != previous) {
            tokens.push(&line[start..index]);
            start = index;
        }
        previous = Some(kind);
    }
    if start < line.len() {
        tokens.push(&line[start..]);
    }
    tokens
}

/// Byte ranges that differ between two lines, by code tokens. Empty when most of the line
/// changed: then the row tint says enough and word marks would cover everything.
fn inline_changes(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let (old_tokens, new_tokens) = (code_tokens(old), code_tokens(new));
    let diff = TextDiff::configure().timeout(Duration::from_millis(50)).diff_slices(&old_tokens, &new_tokens);
    if diff.ratio() < PAIR_RATIO {
        return (Vec::new(), Vec::new());
    }
    let (mut old_ranges, mut new_ranges) = (Vec::new(), Vec::new());
    let (mut old_pos, mut new_pos) = (0, 0);
    for change in diff.iter_all_changes() {
        let len = change.value().len();
        match change.tag() {
            ChangeTag::Equal => {
                old_pos += len;
                new_pos += len;
            }
            ChangeTag::Delete => {
                push_range(&mut old_ranges, old_pos..old_pos + len, old);
                old_pos += len;
            }
            ChangeTag::Insert => {
                push_range(&mut new_ranges, new_pos..new_pos + len, new);
                new_pos += len;
            }
        }
    }
    let changed = |ranges: &[Range<usize>]| ranges.iter().map(|r| r.len()).sum::<usize>();
    let mostly_changed = |ranges: &[Range<usize>], line: &str| changed(ranges) * 2 > line.trim().len();
    if mostly_changed(&old_ranges, old) || mostly_changed(&new_ranges, new) {
        return (Vec::new(), Vec::new());
    }
    (old_ranges, new_ranges)
}

/// One row of one side of the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    /// A source line, by zero-based index.
    Line(usize),
    /// A run of hidden unchanged lines.
    Fold(usize),
    /// Filler on the shorter side, so both sides reach the next fold on the same row.
    Blank,
}

/// The rows each side shows, with unchanged runs folded away beyond `context` lines of a change.
/// Blocks and the scroll map are in display rows, so everything that draws works on rows.
pub struct Display {
    pub left: Vec<Item>,
    pub right: Vec<Item>,
    /// Shown changes, in display rows.
    pub blocks: Vec<Block>,
    /// Every row range where the two sides differ in length (shown and quiet changes), for
    /// the scroll map.
    pub scroll_blocks: Vec<Block>,
}

/// Unchanged runs this short are shown rather than folded into a one-row marker.
const MIN_FOLD: usize = 2;

impl Display {
    pub fn new(model: &Model, context: Option<usize>) -> Self {
        let mut display = Display { left: Vec::new(), right: Vec::new(), blocks: Vec::new(), scroll_blocks: Vec::new() };
        let (mut a, mut b) = (0, 0);
        for (index, block) in model.blocks.iter().enumerate() {
            display.quiet_region(model, (a, block.a0), (b, block.b0), index > 0, true, context);
            let (a0, b0) = (display.left.len(), display.right.len());
            display.left.extend((block.a0..block.a1).map(Item::Line));
            display.right.extend((block.b0..block.b1).map(Item::Line));
            let shown = Block { a0, a1: display.left.len(), b0, b1: display.right.len() };
            display.blocks.push(shown);
            display.scroll_blocks.push(shown);
            a = block.a1;
            b = block.b1;
        }
        let has_blocks = !model.blocks.is_empty();
        display.quiet_region(model, (a, model.old.len()), (b, model.new.len()), has_blocks, false, context);
        display
    }

    /// Pad the shorter side after the change just emitted, so the rows that follow (context and
    /// a fold) sit level on both sides. The padding scrolls with that change, not its ribbon.
    fn pad_last_change(&mut self) {
        let Some(last) = self.scroll_blocks.last_mut() else { return };
        let shortfall = self.left.len().abs_diff(self.right.len());
        if self.left.len() < self.right.len() {
            self.left.extend(std::iter::repeat_n(Item::Blank, shortfall));
            last.a1 += shortfall;
        } else {
            self.right.extend(std::iter::repeat_n(Item::Blank, shortfall));
            last.b1 += shortfall;
        }
    }

    /// The stretch between two shown changes: unchanged lines plus quiet changes. Beyond
    /// `context` lines of a shown change it folds into one row per side; otherwise every line
    /// shows, and each quiet change becomes a scroll block so the sides stay aligned.
    fn quiet_region(
        &mut self,
        model: &Model,
        (a_start, a_end): (usize, usize),
        (b_start, b_end): (usize, usize),
        before: bool,
        after: bool,
        context: Option<usize>,
    ) {
        let (la, lb) = (a_end - a_start, b_end - b_start);
        if let Some(n) = context {
            let (head, tail) = (if before { n } else { 0 }, if after { n } else { 0 });
            if head + tail + MIN_FOLD <= la.min(lb) {
                self.pad_last_change();
                for i in 0..head {
                    self.left.push(Item::Line(a_start + i));
                    self.right.push(Item::Line(b_start + i));
                }
                self.left.push(Item::Fold(la - head - tail));
                self.right.push(Item::Fold(lb - head - tail));
                for i in 0..tail {
                    self.left.push(Item::Line(a_end - tail + i));
                    self.right.push(Item::Line(b_end - tail + i));
                }
                return;
            }
        }
        let (mut a, mut b) = (a_start, b_start);
        let inside = model.quiet.iter().filter(|q| q.a0 >= a_start && q.a1 <= a_end && q.b0 >= b_start && q.b1 <= b_end);
        for quiet in inside {
            for i in 0..quiet.a0 - a {
                self.left.push(Item::Line(a + i));
                self.right.push(Item::Line(b + i));
            }
            let (a0, b0) = (self.left.len(), self.right.len());
            self.left.extend((quiet.a0..quiet.a1).map(Item::Line));
            self.right.extend((quiet.b0..quiet.b1).map(Item::Line));
            self.scroll_blocks.push(Block { a0, a1: self.left.len(), b0, b1: self.right.len() });
            a = quiet.a1;
            b = quiet.b1;
        }
        for i in 0..a_end - a {
            self.left.push(Item::Line(a + i));
            self.right.push(Item::Line(b + i));
        }
    }
}

fn push_range(ranges: &mut Vec<Range<usize>>, range: Range<usize>, text: &str) {
    // Indentation and spacing changes are noise next to real word changes.
    if text[range.clone()].trim().is_empty() {
        return;
    }
    match ranges.last_mut() {
        Some(last) if last.end == range.start => last.end = range.end,
        _ => ranges.push(range),
    }
}

/// One stretch of the shared scroll axis.
#[derive(Debug, Clone, Copy)]
struct Segment {
    start: usize,
    len: usize,
    a0: usize,
    la: usize,
    b0: usize,
    lb: usize,
}

/// Maps one shared scroll position onto both sides. Equal lines advance both sides together;
/// inside a change the longer side advances one line per step and the shorter side follows
/// proportionally, so unchanged lines on both sides always sit on the same row.
#[derive(Debug, Clone)]
pub struct ScrollMap {
    segments: Vec<Segment>,
    total: usize,
}

impl ScrollMap {
    pub fn new(blocks: &[Block], old_len: usize, new_len: usize) -> Self {
        let mut segments = Vec::new();
        let (mut a, mut b, mut start) = (0, 0, 0);
        let mut push = |a0: usize, la: usize, b0: usize, lb: usize, start: &mut usize| {
            let len = la.max(lb);
            if len > 0 {
                segments.push(Segment { start: *start, len, a0, la, b0, lb });
                *start += len;
            }
        };
        for block in blocks {
            push(a, block.a0 - a, b, block.b0 - b, &mut start);
            push(block.a0, block.a1 - block.a0, block.b0, block.b1 - block.b0, &mut start);
            a = block.a1;
            b = block.b1;
        }
        push(a, old_len - a, b, new_len - b, &mut start);
        ScrollMap { segments, total: start }
    }

    pub fn max_position(&self, height: usize) -> usize {
        self.total.saturating_sub(height)
    }

    /// First visible old and new line for a scroll position.
    pub fn tops(&self, position: usize) -> (usize, usize) {
        let Some(segment) = self
            .segments
            .iter()
            .rev()
            .find(|s| s.start <= position)
        else {
            return (0, 0);
        };
        let k = (position - segment.start).min(segment.len);
        (
            segment.a0 + segment.la * k / segment.len,
            segment.b0 + segment.lb * k / segment.len,
        )
    }

    /// Scroll position that puts a row of the old side at the top. Meant for rows outside changes
    /// (file headers), where both sides move together.
    pub fn position_of_left_row(&self, row: usize) -> usize {
        self.segments
            .iter()
            .find(|s| row >= s.a0 && row < s.a0 + s.la)
            .map_or(self.total, |s| s.start + (row - s.a0) * s.len / s.la)
    }

    /// Scroll position where the scroll block starting at old row `a0` and new row `b0` begins.
    pub fn block_start(&self, a0: usize, b0: usize) -> Option<usize> {
        self.segments.iter().find(|s| s.a0 == a0 && s.b0 == b0).map(|s| s.start)
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(old: &str, new: &str) -> Model {
        Model::new(old, new)
    }

    fn lines(text: &str) -> Vec<String> {
        split_lines(text)
    }

    fn scroll_of(m: &Model) -> ScrollMap {
        let d = Display::new(m, None);
        ScrollMap::new(&d.scroll_blocks, d.left.len(), d.right.len())
    }

    #[test]
    fn merges_delete_and_insert_into_one_block() {
        let m = model("a\nb\nc\nd\n", "a\nx\ny\nd\n");
        assert_eq!(m.blocks, vec![Block { a0: 1, a1: 3, b0: 1, b1: 3 }]);
        assert_eq!(m.blocks[0].kind(), BlockKind::Modify);
    }

    #[test]
    fn equal_lines_share_a_row_after_a_change() {
        let m = model("a\nb\nc\nd\n", "a\nx\ny\nz\nw\nb\nc\nd\n");
        // Past the insertion the old line "b" and the new line "b" have the same top.
        let after = scroll_of(&m).block_start(m.blocks[0].a0, m.blocks[0].b0).unwrap() + 4;
        let (left, right) = scroll_of(&m).tops(after);
        assert_eq!(m.old[left], m.new[right]);
    }

    #[test]
    fn tops_are_monotonic_and_reach_both_ends() {
        let m = model(
            "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n",
            "1\nnew\nnew\nnew\nnew\nnew\n4\n5\n9\n10\nextra\n",
        );
        let height = 4;
        let (mut last_left, mut last_right) = (0, 0);
        for position in 0..=scroll_of(&m).max_position(height) {
            let (left, right) = scroll_of(&m).tops(position);
            assert!(left >= last_left && right >= last_right);
            (last_left, last_right) = (left, right);
        }
        assert!(last_left + height >= m.old.len());
        assert!(last_right + height >= m.new.len());
    }

    #[test]
    fn short_and_empty_files() {
        let m = model("", "a\nb\n");
        assert_eq!(m.blocks, vec![Block { a0: 0, a1: 0, b0: 0, b1: 2 }]);
        assert_eq!(scroll_of(&m).max_position(10), 0);
        assert_eq!(scroll_of(&m).tops(0), (0, 0));
        let same = model("a\n", "a\n");
        assert!(same.blocks.is_empty());
    }

    #[test]
    fn folds_unchanged_runs_beyond_the_context() {
        let old: String = (1..=30).map(|i| format!("{i}\n")).collect();
        let new = old.replace("15\n", "fifteen\n");
        let m = model(&old, &new);
        let d = Display::new(&m, Some(3));
        // 14 lines before the change: fold 11, keep 3. 15 after: keep 3, fold 12.
        assert_eq!(d.left[0], Item::Fold(11));
        assert_eq!(&d.left[1..4], &[Item::Line(11), Item::Line(12), Item::Line(13)]);
        assert_eq!(d.left[4], Item::Line(14));
        assert_eq!(d.left.last(), Some(&Item::Fold(12)));
        assert_eq!(d.left.len(), 1 + 3 + 1 + 3 + 1);
        assert_eq!(d.blocks, vec![Block { a0: 4, a1: 5, b0: 4, b1: 5 }]);

        let whole = Display::new(&m, None);
        assert_eq!(whole.left.len(), 30);
        assert!(whole.left.iter().all(|item| matches!(item, Item::Line(_))));
    }

    #[test]
    fn folds_sit_on_the_same_row_on_both_sides() {
        let old: String = (1..=60).map(|i| format!("{i}\n")).collect();
        let new = old.replace("\n10\n", "\n10\na\nb\nc\n").replace("\n40\n", "\nforty\n");
        let d = Display::new(&model(&old, &new), Some(3));
        let folds = |items: &[Item]| items.iter().enumerate().filter(|(_, i)| matches!(i, Item::Fold(_))).map(|(r, _)| r).collect::<Vec<_>>();
        assert_eq!(folds(&d.left), folds(&d.right));
        assert_eq!(folds(&d.left).len(), 3);
        // The three inserted lines are matched by blank rows on the old side, after the change.
        assert_eq!(d.left.iter().filter(|i| **i == Item::Blank).count(), 3);
        // Ribbons still cover only the real lines.
        assert_eq!(d.blocks[0].a1 - d.blocks[0].a0, 0);
        assert_eq!(d.scroll_blocks[0].a1 - d.scroll_blocks[0].a0, 3);
    }

    #[test]
    fn short_gaps_between_changes_stay_visible() {
        let old: String = (1..=10).map(|i| format!("{i}\n")).collect();
        let new = old.replace("2\n", "two\n").replace("9\n", "nine\n");
        let d = Display::new(&model(&old, &new), Some(3));
        // 6 unchanged lines between the changes: 3 + 3 kept, nothing left to fold.
        assert!(!d.left.iter().any(|item| matches!(item, Item::Fold(_))));
        assert_eq!(d.left.len(), 10);
    }

    #[test]
    fn pairs_each_old_line_with_its_real_edit() {
        let lines = |text: &str| text.lines().map(str::to_owned).collect::<Vec<_>>();
        let old = lines("it('keeps the local material when only the price changed', () => {\n  const change = build({ price: true })");
        let new = lines("// The operator swapped the material in this tab\n// so the row keeps its own price.\nit('keeps the local material and its price when the price changed', () => {\n  const change = build({ price: true, preferred: true })");
        assert_eq!(pair_lines(&old, &new), vec![(0, 2), (1, 3)]);
    }

    #[test]
    fn marks_a_changed_number_inside_a_call_chain() {
        let old_line = "expect(result.weightAndPrice?.netPrice?.amount).toBe(27500)";
        let new_line = "expect(result.weightAndPrice?.netPrice?.amount).toBe(70000)";
        let (old, new) = inline_changes(old_line, new_line);
        assert_eq!(&old_line[old[0].clone()], "27500");
        assert_eq!(&new_line[new[0].clone()], "70000");
    }

    #[test]
    fn no_word_marks_when_most_of_the_line_changed() {
        let (old, new) = inline_changes("expect(a).toBe(1)", "// a comment about something else");
        assert!(old.is_empty() && new.is_empty());
    }

    #[test]
    fn comment_only_changes_are_quiet_and_keep_alignment() {
        let old = lines("a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n");
        let new = lines("a\n// note\n// more\nb\nc\nd\ne\nf\ng\nh\nI\nj\nk\n");
        let keys = |lines: &[String]| lines.iter().map(|l| if l.starts_with("//") { String::new() } else { l.clone() }).collect();
        let m = Model::from_lines(old.clone(), new.clone(), Some((keys(&old), keys(&new))));
        assert_eq!(m.blocks, vec![Block { a0: 8, a1: 9, b0: 10, b1: 11 }]);
        assert_eq!(m.quiet, vec![Block { a0: 1, a1: 1, b0: 1, b1: 3 }]);

        // Shown in full, the quiet insertion is a scroll block, so "b" still lines up with "b".
        let d = Display::new(&m, None);
        let scroll = ScrollMap::new(&d.scroll_blocks, d.left.len(), d.right.len());
        let (left, right) = scroll.tops(scroll.block_start(d.blocks[0].a0, d.blocks[0].b0).unwrap());
        assert_eq!((d.left[left], d.right[right]), (Item::Line(8), Item::Line(10)));
        assert_eq!(d.blocks.len(), 1);
        assert_eq!(d.scroll_blocks.len(), 2);

        // Folded, the quiet insertion disappears into the fold, with each side's own count.
        let folded = Display::new(&m, Some(1));
        assert_eq!((folded.left[0], folded.right[0]), (Item::Fold(7), Item::Fold(9)));
    }

    #[test]
    fn keys_drop_comments_and_whitespace() {
        assert_eq!(line_key("let a = 1 // note", &[10..17], false), "let a = 1");
        assert_eq!(line_key("  let  a=1", &[], true), "leta=1");
        assert_eq!(line_key("// only", &[0..7], false), "");
    }

    #[test]
    fn word_ranges_cover_only_the_changed_word() {
        let (old, new) = inline_changes("let total = price * qty;", "let total = cost * qty;");
        assert_eq!(&"let total = price * qty;"[old[0].clone()], "price");
        assert_eq!(&"let total = cost * qty;"[new[0].clone()], "cost");
    }
}
