//! Text with a cursor, for typing notes: lines break at `\n`, and long lines wrap at spaces
//! to the width of the box.

use unicode_width::UnicodeWidthChar;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TextArea {
    chars: Vec<char>,
    /// Index into `chars`: the cursor sits before this character.
    cursor: usize,
}

fn width(c: char) -> usize {
    c.width().unwrap_or(0)
}

impl TextArea {
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_blank(&self) -> bool {
        self.chars.iter().all(|c| c.is_whitespace())
    }

    pub fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    /// Pasted text; line ends become `\n`, tabs four spaces.
    pub fn insert_str(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n").replace('\t', "    ");
        for c in text.chars().filter(|&c| c == '\n' || !c.is_control()) {
            self.insert(c);
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    /// Delete the word before the cursor (Ctrl-W, Alt-Backspace).
    pub fn delete_word(&mut self) {
        let mut start = self.cursor;
        while start > 0 && self.chars[start - 1].is_whitespace() && self.chars[start - 1] != '\n' {
            start -= 1;
        }
        while start > 0 && !self.chars[start - 1].is_whitespace() {
            start -= 1;
        }
        if start == self.cursor && start > 0 {
            start -= 1;
        }
        self.chars.drain(start..self.cursor);
        self.cursor = start;
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.chars.len());
    }

    /// Start or end of the screen row the cursor is on.
    pub fn home(&mut self, columns: usize) {
        let rows = self.rows(columns);
        self.cursor = rows[self.cursor_row(&rows)].0;
    }

    pub fn end(&mut self, columns: usize) {
        let rows = self.rows(columns);
        self.cursor = rows[self.cursor_row(&rows)].1;
    }

    /// The same column on the screen row above or below, or the start or end at the edges.
    pub fn vertical(&mut self, columns: usize, down: bool) {
        let rows = self.rows(columns);
        let row = self.cursor_row(&rows);
        let target = if down { row + 1 } else { row.wrapping_sub(1) };
        let Some(&(start, end)) = rows.get(target) else {
            self.cursor = if down { self.chars.len() } else { 0 };
            return;
        };
        let column: usize = self.chars[rows[row].0..self.cursor].iter().map(|&c| width(c)).sum();
        let (mut at, mut used) = (start, 0);
        while at < end && used + width(self.chars[at]) <= column {
            used += width(self.chars[at]);
            at += 1;
        }
        self.cursor = at;
    }

    /// Screen rows as ranges of `chars`, end excluded; `\n` belongs to no row. Long lines
    /// break after the last space that fits, or anywhere when a word is wider than the box.
    pub fn rows(&self, columns: usize) -> Vec<(usize, usize)> {
        let columns = columns.max(4);
        let mut rows = Vec::new();
        let mut line_start = 0;
        for line_end in (0..=self.chars.len()).filter(|&i| i == self.chars.len() || self.chars[i] == '\n') {
            let mut start = line_start;
            loop {
                let (mut at, mut used, mut space) = (start, 0, None);
                while at < line_end && used + width(self.chars[at]) <= columns {
                    if self.chars[at] == ' ' {
                        space = Some(at);
                    }
                    used += width(self.chars[at]);
                    at += 1;
                }
                if at >= line_end {
                    rows.push((start, line_end));
                    break;
                }
                // Break after the space, which ends this row; a word too wide breaks anywhere.
                let end = space.map_or(at.max(start + 1), |s| s + 1);
                rows.push((start, end));
                start = end;
            }
            line_start = line_end + 1;
        }
        rows
    }

    /// The row the cursor is on. At a wrap point it is the start of the next row.
    pub fn cursor_row(&self, rows: &[(usize, usize)]) -> usize {
        let last_with = rows.iter().rposition(|&(start, _)| start <= self.cursor).unwrap_or(0);
        let (start, end) = rows[last_with];
        // After the last character of a row that ends a line, the cursor stays on that row.
        if self.cursor >= start && self.cursor <= end { last_with } else { last_with.saturating_sub(1) }
    }

    /// The cursor's row and its column on that row.
    pub fn cursor_place(&self, columns: usize) -> (usize, usize) {
        let rows = self.rows(columns);
        let row = self.cursor_row(&rows);
        let column = self.chars[rows[row].0..self.cursor].iter().map(|&c| width(c)).sum();
        (row, column)
    }

    /// The text of a row.
    pub fn row_text(&self, (start, end): (usize, usize)) -> String {
        self.chars[start..end].iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(text: &str) -> TextArea {
        let mut area = TextArea::default();
        area.insert_str(text);
        area
    }

    #[test]
    fn wraps_at_spaces_and_breaks_lines() {
        let area = typed("one two three\n\nfour");
        let rows: Vec<String> = area.rows(8).into_iter().map(|r| area.row_text(r)).collect();
        assert_eq!(rows, ["one two ", "three", "", "four"]);
        let long = typed("abcdefghij");
        assert_eq!(long.rows(4).into_iter().map(|r| long.row_text(r)).collect::<Vec<_>>(), ["abcd", "efgh", "ij"]);
        assert_eq!(typed("a\r\nb\tc").text(), "a\nb    c");
    }

    #[test]
    fn moves_the_cursor_by_rows() {
        let mut area = typed("one two three\nfour");
        // At the end: row 2 ("four"), column 4.
        assert_eq!(area.cursor_place(8), (2, 4));
        area.vertical(8, false);
        assert_eq!(area.cursor_place(8), (1, 4));
        area.vertical(8, false);
        area.home(8);
        assert_eq!(area.cursor_place(8), (0, 0));
        area.end(8);
        area.insert('!');
        assert_eq!(area.text(), "one two !three\nfour");
        // The cursor is at column 1 of "!three": one row up is column 1 of "one two ".
        area.vertical(8, false);
        assert_eq!(area.cursor_place(8), (0, 1));
        area.vertical(80, true);
        area.vertical(80, true);
        assert_eq!(area.cursor_place(80), (1, 4));
        area.delete_word();
        assert_eq!(area.text(), "one two !three\n");
        area.backspace();
        area.insert('\n');
        assert_eq!(area.text(), "one two !three\n");
        assert!(!area.is_blank());
        assert!(typed(" \n ").is_blank());
    }
}
