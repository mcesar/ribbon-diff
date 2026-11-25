//! Text selection on one side of the view, and copying it to the clipboard.

use base64::Engine;
use std::io::Write;
use std::process::{Command, Stdio};
use unicode_width::UnicodeWidthChar;

/// Screen columns of a tab, as drawn.
const TAB_WIDTH: usize = 4;

/// A selection on one side, in that side's stream rows and text columns. The text column counts
/// from the start of the line, so the selection stays on the same text when the view scrolls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub old_side: bool,
    pub anchor: (usize, usize),
    pub head: (usize, usize),
}

impl Selection {
    /// First and last cell, in reading order. Both are selected.
    pub fn bounds(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head { (self.anchor, self.head) } else { (self.head, self.anchor) }
    }

    /// The selected columns of one row, end excluded, or None when the row is outside.
    pub fn columns(&self, row: usize) -> Option<(usize, usize)> {
        let (start, end) = self.bounds();
        if row < start.0 || row > end.0 {
            return None;
        }
        let from = if row == start.0 { start.1 } else { 0 };
        let to = if row == end.0 { end.1 + 1 } else { usize::MAX };
        Some((from, to))
    }
}

fn char_width(c: char) -> usize {
    if c == '\t' { TAB_WIDTH } else { c.width().unwrap_or(0) }
}

/// Display width of a line, with tabs drawn as spaces.
pub fn line_width(line: &str) -> usize {
    line.chars().map(char_width).sum()
}

/// The characters of `line` that are drawn in columns `from..to`. A character that is only
/// partly inside counts as inside.
pub fn slice_columns(line: &str, from: usize, to: usize) -> &str {
    let (mut start, mut end) = (None, line.len());
    let mut column = 0;
    for (byte, c) in line.char_indices() {
        let next = column + char_width(c);
        if start.is_none() && next > from {
            start = Some(byte);
        }
        if column >= to {
            end = byte;
            break;
        }
        column = next;
    }
    match start {
        Some(start) if start < end => &line[start..end],
        _ => "",
    }
}

/// Copy text to the system clipboard. OSC 52 asks the terminal to do it, which also works over
/// ssh; on a local Mac, pbcopy does it too, for terminals that do not allow OSC 52.
pub fn copy(text: &str) -> std::io::Result<()> {
    if cfg!(target_os = "macos") && std::env::var_os("SSH_TTY").is_none() {
        if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    }
    let data = base64::engine::general_purpose::STANDARD.encode(text);
    let mut out = std::io::stdout();
    out.write_all(format!("\x1b]52;c;{data}\x07").as_bytes())?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_by_columns_with_tabs_and_wide_characters() {
        assert_eq!(slice_columns("const total = 1", 6, 11), "total");
        assert_eq!(slice_columns("const", 3, usize::MAX), "st");
        assert_eq!(slice_columns("short", 10, 20), "");
        // A tab is four columns: column 2 is inside it.
        assert_eq!(slice_columns("\tx = 1", 2, 5), "\tx");
        // A two-column character is kept when either of its columns is selected.
        assert_eq!(slice_columns("a漢b", 2, 3), "漢");
        assert_eq!(line_width("\ta漢"), 7);
    }

    #[test]
    fn selection_columns_cover_whole_middle_rows() {
        let selection = Selection { old_side: true, anchor: (7, 4), head: (5, 2) };
        assert_eq!(selection.columns(4), None);
        assert_eq!(selection.columns(5), Some((2, usize::MAX)));
        assert_eq!(selection.columns(6), Some((0, usize::MAX)));
        assert_eq!(selection.columns(7), Some((0, 5)));
        assert_eq!(selection.columns(8), None);
    }
}
