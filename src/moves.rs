//! Moved code: removed lines that come back, the same apart from indentation and spaces, as
//! added lines elsewhere in the diff (in the same file or another one), like
//! `git diff --color-moved`.

use std::collections::HashMap;
use std::ops::Range;

/// A block of lines moved from one place to another. Line ranges are zero-based, end excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Move {
    pub old_file: usize,
    pub old: Range<usize>,
    pub new_file: usize,
    pub new: Range<usize>,
}

/// One file's lines, and which of them the diff changed.
pub struct Side<'a> {
    pub lines: &'a [String],
    pub changed: &'a [bool],
}

/// Fewer letters and digits than this in a whole block is too little to call a move: a lone
/// `}` or `return;` shows up everywhere. Git uses the same number.
const MIN_ALNUM: usize = 20;

/// A line's text without its spacing, so a block moved into a deeper indentation still matches.
fn key(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn alnum(line: &str) -> usize {
    line.chars().filter(|c| c.is_alphanumeric()).count()
}

/// The moved blocks among the changed lines of all files: `files[i]` is (old side, new side).
/// Each removed block is matched with the longest run of added lines that has the same text.
pub fn find(files: &[(Side, Side)]) -> Vec<Move> {
    let old_keys: Vec<Vec<String>> = files.iter().map(|(old, _)| old.lines.iter().map(|l| key(l)).collect()).collect();
    let new_keys: Vec<Vec<String>> = files.iter().map(|(_, new)| new.lines.iter().map(|l| key(l)).collect()).collect();
    // Where each added line's text is. Lines with almost no letters (`}`, `*/`) start no
    // match, which keeps this fast; they still join a run that started elsewhere.
    let mut added: HashMap<&str, Vec<(usize, usize)>> = HashMap::new();
    for (file, (_, new)) in files.iter().enumerate() {
        for (line, text) in new_keys[file].iter().enumerate() {
            if new.changed[line] && alnum(text) >= 3 {
                let places = added.entry(text.as_str()).or_default();
                // A line found everywhere (a common call) is no help in finding moves.
                if places.len() < 64 {
                    places.push((file, line));
                }
            }
        }
    }
    let mut used: Vec<Vec<bool>> = files.iter().map(|(_, new)| vec![false; new.lines.len()]).collect();
    let mut moves = Vec::new();
    for (old_file, (old, _)) in files.iter().enumerate() {
        let mut line = 0;
        while line < old.lines.len() {
            let start_key = &old_keys[old_file][line];
            let candidates = if old.changed[line] { added.get(start_key.as_str()) } else { None };
            let mut best: Option<(usize, usize, usize)> = None;
            for &(new_file, new_line) in candidates.into_iter().flatten() {
                let new = &files[new_file].1;
                let mut length = 0;
                while line + length < old.lines.len()
                    && new_line + length < new.lines.len()
                    && old.changed[line + length]
                    && new.changed[new_line + length]
                    && !used[new_file][new_line + length]
                    && old_keys[old_file][line + length] == new_keys[new_file][new_line + length]
                {
                    length += 1;
                }
                // Blank lines at the end belong to no move.
                while length > 0 && old_keys[old_file][line + length - 1].is_empty() {
                    length -= 1;
                }
                if length > best.map_or(0, |b| b.2) {
                    best = Some((new_file, new_line, length));
                }
            }
            match best {
                Some((new_file, new_line, length)) if old_keys[old_file][line..line + length].iter().map(|k| alnum(k)).sum::<usize>() >= MIN_ALNUM => {
                    used[new_file][new_line..new_line + length].iter_mut().for_each(|u| *u = true);
                    moves.push(Move { old_file, old: line..line + length, new_file, new: new_line..new_line + length });
                    line += length;
                }
                _ => line += 1,
            }
        }
    }
    moves
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    #[test]
    fn finds_blocks_moved_within_and_across_files() {
        // File 0: a function moves from the top to the bottom, indented one level deeper.
        let old0 = lines("fn helper() {\n    compute_the_answer(42)\n}\nfn main() {}\nx");
        let new0 = lines("fn main() {}\nmod inner {\n    fn helper() {\n        compute_the_answer(42)\n    }\n}\nx");
        let (changed_old0, changed_new0) = (vec![true, true, true, false, false], vec![false, true, true, true, true, true, false]);
        // File 1: a line too short to count moves to file 2.
        let old1 = lines("return;\nlet configuration_value = load_settings();");
        let changed_old1 = vec![true, true];
        let new2 = lines("let configuration_value = load_settings();\nreturn;");
        let changed_new2 = vec![true, true];
        let empty: Vec<String> = Vec::new();
        let files = [
            (Side { lines: &old0, changed: &changed_old0 }, Side { lines: &new0, changed: &changed_new0 }),
            (Side { lines: &old1, changed: &changed_old1 }, Side { lines: &empty, changed: &[] }),
            (Side { lines: &empty, changed: &[] }, Side { lines: &new2, changed: &changed_new2 }),
        ];
        let moves = find(&files);
        assert_eq!(moves, [
            Move { old_file: 0, old: 0..3, new_file: 0, new: 2..5 },
            Move { old_file: 1, old: 1..2, new_file: 2, new: 0..1 },
        ]);
    }

    #[test]
    fn unchanged_lines_and_small_blocks_are_not_moves() {
        let old = lines("a\nb\nshort");
        let new = lines("short\na\nb");
        let files = [(Side { lines: &old, changed: &[true, true, true] }, Side { lines: &new, changed: &[true, false, false] })];
        assert!(find(&files).is_empty());
    }
}
