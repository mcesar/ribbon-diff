//! The interactive two-pane view over one or more changed files.

use crate::ribbons::{self, RibbonColors, View};
use crate::highlight::{Highlighter, Paint, Rgb, Tokens};
use crate::termcolors::{self, TermColors};
use crate::model::{Block, BlockKind, Display, Item, Keep, Model, ScrollMap};
use crate::notes::{self, Note};
use std::collections::HashSet;
use std::path::PathBuf;
use crate::search::{self, Match, Search};
use crate::select::{self, Selection};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use crossterm::{cursor, execute, terminal};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Terminal;
use std::io::{self, Write};
use std::ops::Range;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const RIBBON_COLUMNS: u16 = 6;
const TAB: &str = "    ";

/// How much of the change color goes into a changed row and into its changed words. A faint
/// row with clearly stronger words keeps the edit itself easy to spot.
const ROW_TINT: f32 = 0.10;
const WORD_TINT: f32 = 0.42;

const INSERT: Rgb = [98, 186, 108];
const DELETE: Rgb = [224, 98, 88];
/// Moved lines, on both sides.
const MOVED: Rgb = [150, 125, 230];

/// Button, wheel, and drag reports (SGR encoding). Motion is reported only while a button is
/// down. crossterm's EnableMouseCapture also turns on "report every motion", which can flood the
/// tty input queue while a frame is being written.
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";

/// Kitty keyboard protocol: report every key as an escape code (8) with its shifted form (4),
/// so Shift+Space differs from Space. Terminals without the protocol ignore it. Pushed on the
/// alternate screen and popped before leaving it, since each screen keeps its own flags.
const KEYS_ON: &str = "\x1b[>13u";
const KEYS_OFF: &str = "\x1b[<u";
/// While typing: only "disambiguate" from the Kitty keyboard protocol, so Shift-Enter can be
/// told from Enter while text still arrives as text (composed characters stay whole).
const KEYS_TYPING: &str = "\x1b[<u\x1b[>1u\x1b[?2004h";
/// Back from typing: bracketed paste off, and the full key reports again.
const KEYS_TYPED: &str = "\x1b[?2004l\x1b[<u\x1b[>13u";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Graphics {
    Kitty,
    Blocks,
}

/// How the two versions are laid out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LayoutMode {
    /// Unified in narrow terminals, split otherwise.
    Auto,
    /// Two sides with ribbons between them.
    Split,
    /// One column: removed lines above the lines that replace them.
    Unified,
}

/// Auto layout is unified below this many columns.
const AUTO_UNIFIED_BELOW: u16 = 120;

/// The selection column for "the whole line", with room to add one.
const LINE_END: usize = usize::MAX / 4;

pub enum Outcome {
    Quit,
    QuitAll,
}

/// One changed file, as given to the viewer.
pub struct FileDiff {
    pub old_name: String,
    pub new_name: String,
    pub model: Model,
    /// The new side on disk, for opening it in an editor. None when there is no such file.
    pub path: Option<PathBuf>,
}

pub struct Settings {
    /// None: use Kitty graphics when the terminal answers the Kitty query, or `kitty_hint` is set.
    pub graphics: Option<Graphics>,
    /// The environment names a terminal known to show Kitty graphics.
    pub kitty_hint: bool,
    /// Unchanged lines kept around each change; the `c` key shows the whole file instead.
    pub context: usize,
    pub highlighter: Arc<Highlighter>,
    /// With `--watch`: a fresh set of changed files whenever they change on disk.
    pub reloads: Option<mpsc::Receiver<Vec<FileDiff>>>,
    /// Read the changed files again, after the editor closes (without `--watch`).
    pub load_again: Option<Box<dyn Fn() -> Vec<FileDiff>>>,
    /// Rows kept above a change, a match, or a note the view jumps to, and around the cursor
    /// of a keyboard selection.
    pub scroll_off: usize,
    /// Start with long lines wrapped.
    pub wrap: bool,
    pub layout: LayoutMode,
    /// Keys from the config file: a key, and the key it acts as.
    pub keys: Vec<(crate::config::Key, crate::config::Key)>,
    /// Where notes for Claude are kept: `.ribbon/notes.json`.
    pub notes_file: PathBuf,
    /// Notes from an agent to show, read-only (`--agent-context`).
    pub agent_context: Option<PathBuf>,
    /// Shown in the status line at the start, such as the commit for `ribbon show`.
    pub notice: Option<String>,
    /// Show moved blocks of code in their own color (`--no-moved` turns it off).
    pub moved: bool,
    /// With `--listen`: commands from `ribbon send`.
    pub commands: Option<mpsc::Receiver<crate::listen::Request>>,
    /// `ribbon pr`: the pull request, and its checks in one line whenever they are read.
    pub pull_request: Option<crate::github::PullRequest>,
    pub checks: Option<mpsc::Receiver<String>>,
}

/// Color files in stream order on a background thread; each result redraws. Dropping the
/// receiver stops the thread.
fn spawn_highlighting(files: &[FileState], highlighter: &Arc<Highlighter>) -> mpsc::Receiver<Highlighted> {
    let (sender, highlights) = mpsc::channel::<Highlighted>();
    let jobs: Vec<(usize, String, Vec<String>, Vec<String>)> = files
        .iter()
        .enumerate()
        .filter(|(_, f)| f.old_tokens.is_empty() || f.new_tokens.is_empty())
        .map(|(i, f)| (i, f.diff.new_name.clone(), f.diff.model.old.clone(), f.diff.model.new.clone()))
        .collect();
    let highlighter = Arc::clone(highlighter);
    thread::spawn(move || {
        for (file, name, old, new) in jobs {
            for (is_old, lines) in [(true, old), (false, new)] {
                let tokens = highlighter.highlight(&name, &lines);
                if sender.send(Highlighted { file, is_old, tokens }).is_err() {
                    return;
                }
            }
        }
    });
    highlights
}

/// Columns of the widest line, and the width of the line number column.
fn measure(files: &[FileState]) -> (usize, u16) {
    let max_width = files
        .iter()
        .flat_map(|f| f.diff.model.old.iter().chain(&f.diff.model.new))
        .map(|line| line.replace('\t', TAB).width())
        .max()
        .unwrap_or(0);
    let max_line = files.iter().map(|f| f.diff.model.old.len().max(f.diff.model.new.len())).max().unwrap_or(1).max(1);
    (max_width, (max_line.to_string().len() + 1) as u16)
}

/// Syntax colors for one side of one file, sent by a background thread.
struct Highlighted {
    file: usize,
    is_old: bool,
    tokens: Vec<Tokens>,
}

struct FileState {
    diff: FileDiff,
    old_kinds: Vec<Option<BlockKind>>,
    new_kinds: Vec<Option<BlockKind>>,
    /// For each line, the moved block it belongs to (an index into the app's moves).
    old_moved: Vec<Option<usize>>,
    new_moved: Vec<Option<usize>>,
    /// For each block, its repeated edit and which place it is (0 for the first).
    block_repeat: Vec<Option<(usize, usize)>>,
    /// Lines of the repeats after the first, drawn dimmer: the edit was already read.
    old_seen: Vec<bool>,
    new_seen: Vec<bool>,
    old_tokens: Vec<Tokens>,
    new_tokens: Vec<Tokens>,
    added: usize,
    removed: usize,
}

impl FileState {
    fn new(diff: FileDiff) -> Self {
        let kinds = |old_side: bool| {
            let len = if old_side { diff.model.old.len() } else { diff.model.new.len() };
            let mut kinds = vec![None; len];
            for block in &diff.model.blocks {
                let range = if old_side { block.a0..block.a1 } else { block.b0..block.b1 };
                for line in range {
                    kinds[line] = Some(block.kind());
                }
            }
            kinds
        };
        FileState {
            old_kinds: kinds(true),
            new_kinds: kinds(false),
            added: diff.model.blocks.iter().map(|b| b.b1 - b.b0).sum(),
            removed: diff.model.blocks.iter().map(|b| b.a1 - b.a0).sum(),
            old_moved: vec![None; diff.model.old.len()],
            new_moved: vec![None; diff.model.new.len()],
            block_repeat: vec![None; diff.model.blocks.len()],
            old_seen: vec![false; diff.model.old.len()],
            new_seen: vec![false; diff.model.new.len()],
            diff,
            old_tokens: Vec::new(),
            new_tokens: Vec::new(),
        }
    }
}

/// Find the edits made in more than one place, leaving out moved code, and mark their blocks.
fn mark_repeats(files: &mut [FileState]) -> Vec<crate::repeats::Group> {
    let moved = |file: &FileState, block: &Block| {
        file.old_moved[block.a0..block.a1].iter().chain(&file.new_moved[block.b0..block.b1]).any(Option::is_some)
    };
    let groups = {
        let models: Vec<&Model> = files.iter().map(|f| &f.diff.model).collect();
        crate::repeats::find(&models, |file, block| moved(&files[file], &files[file].diff.model.blocks[block]))
    };
    for file in files.iter_mut() {
        file.block_repeat.iter_mut().for_each(|r| *r = None);
        file.old_seen.iter_mut().chain(file.new_seen.iter_mut()).for_each(|s| *s = false);
    }
    for (group, found) in groups.iter().enumerate() {
        for (place, &(file, block)) in found.places.iter().enumerate() {
            let state = &mut files[file];
            state.block_repeat[block] = Some((group, place));
            if place > 0 {
                let b = state.diff.model.blocks[block];
                state.old_seen[b.a0..b.a1].iter_mut().chain(state.new_seen[b.b0..b.b1].iter_mut()).for_each(|s| *s = true);
            }
        }
    }
    groups
}

/// Find the moved blocks in all files, and mark their lines.
fn mark_moves(files: &mut [FileState]) -> Vec<crate::moves::Move> {
    use crate::moves::{find, Side};
    let changed: Vec<(Vec<bool>, Vec<bool>)> =
        files.iter().map(|f| (f.old_kinds.iter().map(Option::is_some).collect(), f.new_kinds.iter().map(Option::is_some).collect())).collect();
    let sides: Vec<(Side, Side)> = files
        .iter()
        .zip(&changed)
        .map(|(f, (old, new))| (Side { lines: &f.diff.model.old, changed: old }, Side { lines: &f.diff.model.new, changed: new }))
        .collect();
    let moves = find(&sides);
    for file in files.iter_mut() {
        file.old_moved.iter_mut().chain(file.new_moved.iter_mut()).for_each(|m| *m = None);
    }
    for (index, found) in moves.iter().enumerate() {
        files[found.old_file].old_moved[found.old.clone()].iter_mut().for_each(|m| *m = Some(index));
        files[found.new_file].new_moved[found.new.clone()].iter_mut().for_each(|m| *m = Some(index));
    }
    moves
}

/// One row of one side of the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    /// The title row that starts a file.
    Header(usize),
    /// A source line: file, zero-based line index.
    Line(usize, usize),
    /// More of a long line, with wrapping on: file, line index, and which screen row of the
    /// line (1 or more).
    Wrap(usize, usize, usize),
    /// Hidden unchanged lines: file, how many, and the first new line hidden (which names the
    /// fold for opening it).
    Fold(usize, usize, usize),
    /// One row of a note under a line: the note's index in the app's notes, and which row.
    Note(usize, usize),
    /// In the unified layout, a line of the old version: file, line index, and which screen
    /// row of the line (0, or more when it wraps).
    Old(usize, usize, usize),
    /// Space between two files.
    Gap,
}

/// Where one note shows: under `line` of `file`, on one side, taking `rows` rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NoteRows {
    file: usize,
    old_side: bool,
    line: usize,
    note: usize,
    rows: usize,
}

/// Every file one after another, like `git diff` or a pull request page. Header and gap rows
/// sit on both sides, so the sides stay level between changes across file boundaries too.
struct Stream {
    left: Vec<Row>,
    right: Vec<Row>,
    blocks: Vec<Block>,
    scroll: ScrollMap,
    /// Left row of each file's header.
    headers: Vec<usize>,
    /// Columns per screen row when long lines wrap.
    wrap: Option<usize>,
    /// One column for both versions (the two sides hold the same rows).
    unified: bool,
}

impl Stream {
    /// `opened` holds the folds to show, as (new file name, first new line hidden). With
    /// `wrap`, long lines take more rows, and the shorter of two level lines gets blank rows.
    /// With `unified`, both sides hold the same single column: each change's old lines (as
    /// `Row::Old`) above its new lines, and no ribbons.
    fn new(files: &[FileState], context: Option<usize>, opened: &HashSet<(String, usize)>, wrap: Option<usize>, notes: &[NoteRows], unified: bool) -> Self {
        let (mut left, mut right, mut headers) = (Vec::new(), Vec::new(), Vec::new());
        let (mut blocks, mut scroll_blocks) = (Vec::new(), Vec::new());
        for (index, file) in files.iter().enumerate() {
            if index > 0 {
                left.push(Row::Gap);
                right.push(Row::Gap);
            }
            headers.push(left.len());
            left.push(Row::Header(index));
            right.push(Row::Header(index));
            let name = &file.diff.new_name;
            let file_notes: Vec<&NoteRows> = notes.iter().filter(|n| n.file == index).collect();
            // Lines with notes stay visible.
            let open = Keep {
                folds: opened.iter().filter(|(n, _)| n == name).map(|&(_, start)| start).collect(),
                old: file_notes.iter().filter(|n| n.old_side).map(|n| n.line).collect(),
                new: file_notes.iter().filter(|n| !n.old_side).map(|n| n.line).collect(),
            };
            let display = Display::with_open(&file.diff.model, context, &open);
            let row = |old_side: bool| {
                let display = &display;
                move |(at, item): (usize, &Item)| match *item {
                    Item::Line(line) => Row::Line(index, line),
                    Item::Fold(hidden) => {
                        let fold = display.folds.iter().find(|&&(left, right, _)| at == if old_side { left } else { right });
                        Row::Fold(index, hidden, fold.map_or(usize::MAX, |&(_, _, start)| start))
                    }
                    Item::Blank => Row::Gap,
                }
            };
            let mut file_left: Vec<Row> = display.left.iter().enumerate().map(row(true)).collect();
            let mut file_right: Vec<Row> = display.right.iter().enumerate().map(row(false)).collect();
            let mut file_scroll_blocks = display.scroll_blocks.clone();
            let mut file_blocks = display.blocks.clone();

            if unified {
                let (mut rows, mut file_blocks) = unified_rows(&file_left, &file_right, &display.blocks, &display.scroll_blocks);
                expand_unified(file, wrap, &file_notes, &mut rows, &mut file_blocks);
                let start = right.len();
                blocks.extend(file_blocks.iter().map(|b| Block { a0: b.a0 + start, a1: b.a1 + start, b0: b.b0 + start, b1: b.b1 + start }));
                left.extend(rows.iter().copied());
                right.extend(rows);
                continue;
            }

            // Pad the shorter side with blank rows right after the file's last change, so both
            // sides end the file on the same row and the next file header lines up. The padding
            // belongs to the last change for scrolling, but not to its ribbon.
            if let Some(last) = file_scroll_blocks.last_mut() {
                let shortfall = file_left.len().abs_diff(file_right.len());
                if file_left.len() < file_right.len() {
                    file_left.splice(last.a1..last.a1, std::iter::repeat_n(Row::Gap, shortfall));
                    last.a1 += shortfall;
                } else {
                    file_right.splice(last.b1..last.b1, std::iter::repeat_n(Row::Gap, shortfall));
                    last.b1 += shortfall;
                }
            }

            if wrap.is_some() || !file_notes.is_empty() {
                expand_rows(file, wrap, &file_notes, (&mut file_left, &mut file_right), (&mut file_blocks, &mut file_scroll_blocks));
            }

            let (left_start, right_start) = (left.len(), right.len());
            let shift = |b: &Block| Block { a0: b.a0 + left_start, a1: b.a1 + left_start, b0: b.b0 + right_start, b1: b.b1 + right_start };
            blocks.extend(file_blocks.iter().map(shift));
            scroll_blocks.extend(file_scroll_blocks.iter().map(shift));
            left.extend(file_left);
            right.extend(file_right);
        }
        let scroll = ScrollMap::new(&scroll_blocks, left.len(), right.len());
        Stream { left, right, blocks, scroll, headers, wrap, unified }
    }

    /// The file title to pin at the top: the file at the top of the view, unless its own
    /// header row is already there.
    fn sticky_file(&self, top: usize, old_side: bool) -> Option<usize> {
        let rows = if old_side { &self.left } else { &self.right };
        match rows.get(top) {
            Some(Row::Header(_)) | None => None,
            _ => Some(rows[..top].iter().rev().find_map(|r| if let Row::Header(file) = r { Some(*file) } else { None }).unwrap_or(0)),
        }
    }

    /// Scroll positions that put each shown change a few rows below the top.
    fn change_starts(&self, scroll_off: usize) -> Vec<usize> {
        self.blocks
            .iter()
            .filter_map(|b| if self.unified { Some(self.scroll.position_of_row(b.b0, false)) } else { self.scroll.block_start(b.a0, b.b0) })
            .map(|s| s.saturating_sub(scroll_off))
            .collect()
    }

    fn header_position(&self, file: usize) -> usize {
        self.headers.get(file).map_or(0, |&row| self.scroll.position_of_left_row(row))
    }

    /// The file whose rows are at the top of the view.
    fn file_at(&self, position: usize) -> usize {
        let (left_top, _) = self.scroll.tops(position);
        self.headers.iter().rposition(|&row| row <= left_top).unwrap_or(0)
    }
}

/// The unified column of one file, from its two sides: rows outside changes once (from the new
/// side), and for each shown change its old lines, then its new lines. A change whose
/// differences are ignored (quiet) shows only its new lines. Also returns the changes, as
/// row ranges of the column (the same on both "sides").
fn unified_rows(left: &[Row], right: &[Row], shown: &[Block], scroll_blocks: &[Block]) -> (Vec<Row>, Vec<Block>) {
    let (mut rows, mut blocks) = (Vec::new(), Vec::new());
    let mut b = 0;
    let mut sorted = scroll_blocks.to_vec();
    sorted.sort_by_key(|block| block.b0);
    let end = Block { a0: left.len(), a1: left.len(), b0: right.len(), b1: right.len() };
    let not_gap = |r: &&Row| **r != Row::Gap;
    for block in sorted.iter().chain([&end]) {
        rows.extend(right[b..block.b0].iter().filter(not_gap));
        let start = rows.len();
        let is_shown = shown.iter().any(|s| s.a0 == block.a0 && s.b0 == block.b0);
        if is_shown {
            rows.extend(left[block.a0..block.a1].iter().filter_map(|r| match *r {
                Row::Line(file, line) => Some(Row::Old(file, line, 0)),
                _ => None,
            }));
        }
        rows.extend(right[block.b0..block.b1].iter().filter(not_gap));
        if is_shown {
            blocks.push(Block { a0: start, a1: rows.len(), b0: start, b1: rows.len() });
        }
        b = block.b1;
    }
    (rows, blocks)
}

/// Wrap rows and note rows in the unified column.
fn expand_unified(file: &FileState, wrap: Option<usize>, notes: &[&NoteRows], rows: &mut Vec<Row>, blocks: &mut [Block]) {
    if wrap.is_none() && notes.is_empty() {
        return;
    }
    let model = &file.diff.model;
    let mut out = Vec::new();
    let mut map = Vec::new();
    for &row in rows.iter() {
        map.push(out.len());
        let (is_old, f, line) = match row {
            Row::Line(f, line) => (false, f, line),
            Row::Old(f, line, _) => (true, f, line),
            _ => {
                out.push(row);
                continue;
            }
        };
        out.push(row);
        if let Some(width) = wrap {
            let text = if is_old { &model.old[line] } else { &model.new[line] };
            let parts = select::line_width(text).div_ceil(width.max(1)).max(1);
            out.extend((1..parts).map(|part| if is_old { Row::Old(f, line, part) } else { Row::Wrap(f, line, part) }));
        }
        for note in notes.iter().filter(|n| n.old_side == is_old && n.line == line) {
            out.extend((0..note.rows).map(|part| Row::Note(note.note, part)));
        }
    }
    map.push(out.len());
    for block in blocks.iter_mut() {
        *block = Block { a0: map[block.a0], a1: map[block.a1], b0: map[block.b0], b1: map[block.b1] };
    }
    *rows = out;
}

/// Give each long line one more row per `width` columns (with wrapping on), and each line
/// with notes the rows of its notes. Outside changes, the rows of the two sides are level
/// pairs: both get as many rows as the taller of the two, so they stay level (lines that only
/// look equal, with `-w`, can wrap differently). Blocks move to match.
fn expand_rows(
    file: &FileState,
    wrap: Option<usize>,
    notes: &[&NoteRows],
    (left, right): (&mut Vec<Row>, &mut Vec<Row>),
    (blocks, scroll_blocks): (&mut [Block], &mut [Block]),
) {
    let model = &file.diff.model;
    let rows_of = |row: Row, old_side: bool| -> Vec<Row> {
        let Row::Line(f, line) = row else { return vec![row] };
        let mut rows = vec![row];
        if let Some(width) = wrap {
            let text = if old_side { &model.old[line] } else { &model.new[line] };
            let parts = select::line_width(text).div_ceil(width.max(1)).max(1);
            rows.extend((1..parts).map(|part| Row::Wrap(f, line, part)));
        }
        for note in notes.iter().filter(|n| n.old_side == old_side && n.line == line) {
            rows.extend((0..note.rows).map(|part| Row::Note(note.note, part)));
        }
        rows
    };
    let push = |out: &mut Vec<Row>, rows: Vec<Row>, height: usize| {
        let count = rows.len();
        out.extend(rows);
        out.extend(std::iter::repeat_n(Row::Gap, height - count));
    };
    let (mut new_left, mut new_right) = (Vec::new(), Vec::new());
    // Old row -> new row, with one entry past the end.
    let (mut left_map, mut right_map) = (Vec::new(), Vec::new());
    let (mut a, mut b) = (0, 0);
    let mut sorted: Vec<Block> = scroll_blocks.to_vec();
    sorted.sort_by_key(|block| block.a0);
    for block in sorted.iter().copied().chain([Block { a0: left.len(), a1: left.len(), b0: right.len(), b1: right.len() }]) {
        while a < block.a0 && b < block.b0 {
            let (l, r) = (rows_of(left[a], true), rows_of(right[b], false));
            let height = l.len().max(r.len());
            left_map.push(new_left.len());
            right_map.push(new_right.len());
            push(&mut new_left, l, height);
            push(&mut new_right, r, height);
            a += 1;
            b += 1;
        }
        // Each side of a change grows on its own.
        for &row in &left[a..block.a1] {
            left_map.push(new_left.len());
            let rows = rows_of(row, true);
            let height = rows.len();
            push(&mut new_left, rows, height);
        }
        for &row in &right[b..block.b1] {
            right_map.push(new_right.len());
            let rows = rows_of(row, false);
            let height = rows.len();
            push(&mut new_right, rows, height);
        }
        (a, b) = (block.a1, block.b1);
    }
    left_map.push(new_left.len());
    right_map.push(new_right.len());
    for block in blocks.iter_mut().chain(scroll_blocks.iter_mut()) {
        *block = Block { a0: left_map[block.a0], a1: left_map[block.a1], b0: right_map[block.b0], b1: right_map[block.b1] };
    }
    *left = new_left;
    *right = new_right;
}

fn rgb([r, g, b]: Rgb) -> Color {
    Color::Rgb(r, g, b)
}

fn mix(a: Rgb, b: Rgb, amount: f32) -> Rgb {
    [0, 1, 2].map(|i| (a[i] as f32 * amount + b[i] as f32 * (1.0 - amount)).round() as u8)
}

struct Layout {
    left: Rect,
    ribbons: Rect,
    right: Rect,
    status: Rect,
}

/// Which sides are on screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Panes {
    Both,
    Old,
    New,
}

/// One side alone takes the whole width, with no ribbon column; the other side gets no columns.
fn layout(area: Rect, panes: Panes) -> Layout {
    let body_height = area.height.saturating_sub(1);
    let (left_width, ribbons_width) = match panes {
        Panes::Both => {
            let side = area.width.saturating_sub(RIBBON_COLUMNS) / 2;
            (side, area.width.saturating_sub(side * 2))
        }
        Panes::Old => (area.width, 0),
        Panes::New => (0, 0),
    };
    let right_width = area.width - left_width - ribbons_width;
    let body = |x, width| Rect { x, y: area.y, width, height: body_height };
    Layout {
        left: body(area.x, left_width),
        ribbons: body(area.x + left_width, ribbons_width),
        right: body(area.x + left_width + ribbons_width, right_width),
        status: Rect { y: area.y + area.height.saturating_sub(1), height: 1.min(area.height), ..area },
    }
}

/// All colors on screen. The terminal theme colors syntax with the terminal's ANSI palette and
/// tints changed rows with the terminal's own red and green, mixed into its background. An RGB
/// theme does the same with the theme's colors.
pub(crate) struct Theme {
    /// `Some((background, foreground))` for an RGB theme; None for the terminal palette.
    rgb: Option<(Rgb, Rgb)>,
    /// The background the tints mix into: the RGB theme's, or the one the terminal reported.
    tint_base: Option<Rgb>,
    /// Comment text: halfway between text and background, so it reads on tinted rows too.
    comment: Color,
    ribbons: RibbonColors,
}

impl Theme {
    fn new(highlighter: &Highlighter, terminal: TermColors) -> Self {
        let rgb = highlighter.colors();
        let (tint_base, ribbons) = match rgb {
            Some((background, _)) => (Some(background), RibbonColors { background: Some(background), insert: INSERT, delete: DELETE }),
            None => (
                terminal.background,
                RibbonColors {
                    background: terminal.background,
                    insert: terminal.green.unwrap_or(INSERT),
                    delete: terminal.red.unwrap_or(DELETE),
                },
            ),
        };
        let comment = match (rgb, terminal.foreground, terminal.background) {
            (Some((bg, fg)), _, _) | (None, Some(fg), Some(bg)) => self::rgb(mix(fg, bg, 0.6)),
            // Unknown colors mean no tints either, so the palette's gray stays readable.
            _ => Color::DarkGray,
        };
        Theme { rgb, tint_base, comment, ribbons }
    }

    fn base(&self) -> Style {
        match self.rgb {
            Some((bg, fg)) => Style::default().bg(rgb(bg)).fg(rgb(fg)),
            None => Style::default().bg(Color::Reset).fg(Color::Reset),
        }
    }

    fn muted(&self) -> Color {
        match self.rgb {
            Some((bg, fg)) => rgb(mix(fg, bg, 0.45)),
            None => Color::DarkGray,
        }
    }

    /// Header, status line, and fold rows.
    fn panel(&self) -> Style {
        match self.rgb {
            Some((bg, fg)) => Style::default().bg(rgb(mix(fg, bg, 0.12))).fg(rgb(fg)),
            None => Style::default().bg(Color::Reset).fg(Color::Reset),
        }
    }

    /// File names: bold, and yellow on the terminal palette.
    fn file_title(&self) -> Style {
        match self.rgb {
            Some(_) => self.panel().add_modifier(Modifier::BOLD),
            None => self.panel().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        }
    }

    /// Red on the old side, green on the new side, whatever the change.
    fn side_rgb(&self, old_side: bool) -> Rgb {
        if old_side { self.ribbons.delete } else { self.ribbons.insert }
    }

    /// Moved lines: one color on both sides, so a move does not read as a removal and an
    /// addition.
    fn moved_color(&self) -> Color {
        match (self.rgb, self.tint_base) {
            (None, None) => Color::Magenta,
            _ => rgb(MOVED),
        }
    }

    /// Tints for a repeat of an edit already read: half as strong.
    fn seen_tints(&self, old_side: bool) -> Option<(Color, Color)> {
        let base = self.tint_base?;
        let color = self.side_rgb(old_side);
        Some((rgb(mix(color, base, ROW_TINT / 2.0)), rgb(mix(color, base, WORD_TINT / 2.0))))
    }

    fn moved_tint(&self) -> Option<Color> {
        Some(rgb(mix(MOVED, self.tint_base?, ROW_TINT)))
    }

    /// Color for changed line numbers and, without tints, changed text.
    fn change_color(&self, old_side: bool) -> Color {
        match (self.rgb, self.tint_base) {
            (None, None) if old_side => Color::Red,
            (None, None) => Color::Green,
            _ => rgb(self.side_rgb(old_side)),
        }
    }

    /// Row tint and stronger word tint for a changed line. None when the terminal did not
    /// report its background: then changed text is colored instead, like difftastic.
    fn tints(&self, old_side: bool) -> Option<(Color, Color)> {
        let base = self.tint_base?;
        let color = self.side_rgb(old_side);
        Some((rgb(mix(color, base, ROW_TINT)), rgb(mix(color, base, WORD_TINT))))
    }

    fn token_style(&self, paint: Paint) -> Style {
        let ansi = |index: u8| match index {
            1 => Color::Red,
            2 => Color::Green,
            3 => Color::Yellow,
            4 => Color::Blue,
            5 => Color::Magenta,
            6 => Color::Cyan,
            _ => Color::Reset,
        };
        match paint {
            Paint::Rgb(color) => Style::default().fg(rgb(color)),
            Paint::Plain => Style::default().fg(self.base().fg.unwrap_or(Color::Reset)),
            Paint::Ansi(index) => Style::default().fg(ansi(index)),
            Paint::Comment => Style::default().fg(self.comment).add_modifier(Modifier::ITALIC),
        }
    }
}

/// Where code starts on a side, and how many columns it has. Line numbers are on the right
/// edge of the side, or on its left edge.
fn code_area(area: Rect, number_width: u16, numbers_right: bool) -> (u16, u16) {
    let code_x = if numbers_right { area.x + 1 } else { area.x + number_width + 1 };
    (code_x, area.width.saturating_sub(number_width + 1))
}

/// Change the cells of text columns `from..to` of a line drawn at `code_x`, after sideways scroll.
fn paint_columns(buf: &mut Buffer, (code_x, code_width): (u16, u16), y: u16, hscroll: usize, (from, to): (usize, usize), paint: impl Fn(&mut Cell)) {
    let from = from.saturating_sub(hscroll);
    let to = to.saturating_sub(hscroll).min(code_width as usize);
    for x in from..to {
        if let Some(cell) = buf.cell_mut((code_x + x as u16, y)) {
            paint(cell);
        }
    }
}

/// Line numbers sit next to the ribbon column: at the right edge of the old side, the left edge of the
/// new. A side alone has them on the left.
fn draw_side(buf: &mut Buffer, area: Rect, top: usize, app: &App, old_side: bool) {
    // A side that is not on screen. In the unified layout its rows hold new lines, which
    // the old side's data can't index.
    if area.width == 0 {
        return;
    }
    let theme = &app.theme;
    let rows = if old_side { &app.stream.left } else { &app.stream.right };
    let number_width = app.number_width;
    let numbers_right = app.numbers_right(old_side);
    let (code_x, code_width) = code_area(area, number_width, numbers_right);
    let number_x = if numbers_right { area.x + area.width.saturating_sub(number_width) } else { area.x };

    let shown = app.shown_rows(old_side, top, area.height as usize);
    for row in 0..area.height {
        let y = area.y + row;
        let row_area = Rect { y, height: 1, ..area };
        let at = shown.get(row as usize).copied().unwrap_or(usize::MAX);
        match rows.get(at) {
            None | Some(Row::Gap) => clear(buf, row_area, theme.base()),
            Some(&Row::Header(index)) => draw_file_title(buf, row_area, app, index, old_side, app.is_selected(old_side, at)),
            Some(&Row::Note(index, part)) => draw_note_row(buf, row_area, code_x, app, index, part),
            Some(Row::Fold(_, hidden, _)) => {
                let style = theme.base().fg(theme.muted()).add_modifier(Modifier::ITALIC);
                clear(buf, row_area, style);
                let label = format!("⋯ {hidden} unchanged lines (z or click to show)");
                buf.set_line(code_x, y, &Line::styled(label, style), code_width);
            }
            Some(&(Row::Line(file_index, index) | Row::Wrap(file_index, index, _) | Row::Old(file_index, index, _))) => {
                let part = match rows[at] {
                    Row::Wrap(_, _, part) | Row::Old(_, _, part) => part,
                    _ => 0,
                };
                // The version this line is from: the side's own, or old in the unified column.
                let pane_old_side = old_side;
                let old_side = old_side || matches!(rows[at], Row::Old(..));
                // Wrapped rows show the next slice of the line; otherwise the sideways scroll.
                let skip = match app.stream.wrap {
                    Some(width) => part * width,
                    None => app.hscroll,
                };
                let file = &app.files[file_index];
                let (lines, tokens, kinds, inline) = if old_side {
                    (&file.diff.model.old, &file.old_tokens, &file.old_kinds, &file.diff.model.old_inline)
                } else {
                    (&file.diff.model.new, &file.new_tokens, &file.new_kinds, &file.diff.model.new_inline)
                };
                let changed = kinds[index].is_some();
                let moved = (if old_side { &file.old_moved } else { &file.new_moved })[index].is_some();
                // Moved lines take one tint, with no word marks: their words did not change.
                let seen = (if old_side { &file.old_seen } else { &file.new_seen })[index];
                let tints = match (changed, moved) {
                    (true, true) => theme.moved_tint().map(|tint| (tint, tint)),
                    (true, false) if seen => theme.seen_tints(old_side),
                    (true, false) => theme.tints(old_side),
                    _ => None,
                };
                let row_style = match tints {
                    Some((tint, _)) => theme.base().bg(tint),
                    None => theme.base(),
                };
                clear(buf, row_area, row_style);
                let number_style = if moved {
                    row_style.fg(theme.moved_color())
                } else if changed {
                    row_style.fg(theme.change_color(old_side))
                } else {
                    row_style.fg(theme.muted())
                };
                let number = if numbers_right {
                    format!("{:>width$} ", index + 1, width = number_width as usize - 1)
                } else {
                    // In the unified column, changed lines start with - or +, as in a patch.
                    let sign = match (app.unified, changed, old_side) {
                        (true, true, true) => '-',
                        (true, true, false) => '+',
                        _ => ' ',
                    };
                    format!("{sign}{:<width$}", index + 1, width = number_width as usize - 1)
                };
                // Lines marked with `ribbon send highlight` show their numbers in reverse cyan.
                let marked = app.mark.as_ref().is_some_and(|m| {
                    if m.old_side == old_side {
                        *(if old_side { &file.diff.old_name } else { &file.diff.new_name }) == m.path && (m.range.0..=m.range.1).contains(&(index + 1))
                    } else {
                        // The unified column shows unchanged old lines as their new lines.
                        app.unified
                            && m.old_side
                            && file.diff.old_name == m.path
                            && (m.range.0 - 1..m.range.1).take(2000).any(|old| file.diff.model.new_line_outside_changes(old) == Some(index))
                    }
                });
                let number_style = if marked { number_style.bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD) } else { number_style };
                if part == 0 || marked {
                    let number = if part == 0 { number } else { " ".repeat(number_width as usize) };
                    buf.set_line(number_x, y, &Line::styled(number, number_style), number_width);
                }
                let plain = || vec![(Paint::Plain, lines[index].clone())];
                let line_tokens = tokens.get(index).cloned().unwrap_or_else(plain);
                let change = changed.then(|| Change {
                    color: if moved { theme.moved_color() } else { theme.change_color(old_side) },
                    strong_bg: tints.map(|(_, strong)| strong),
                    // A whole-line change, or a modified line without word matches, is all changed.
                    whole_line: moved || inline[index].is_empty(),
                });
                let words: &[Range<usize>] = if moved { &[] } else { &inline[index] };
                let spans = skip_columns(styled_tokens(&line_tokens, words, row_style, change, theme), skip);
                buf.set_line(code_x, y, &Line::from(spans), code_width);
                let line = &lines[index];
                let code = (code_x, code_width);
                if let Some(found) = &app.search {
                    let current = found.matches.get(found.current);
                    for bytes in search::find_all(line, &found.query) {
                        let is_current = current.is_some_and(|m| m.old_side == pane_old_side && m.row == at - part && m.bytes == bytes);
                        // Yellow like most editors; the current match is orange.
                        let bg = if is_current { Color::Indexed(208) } else { Color::Yellow };
                        let columns = (select::line_width(&line[..bytes.start]), select::line_width(&line[..bytes.end]));
                        paint_columns(buf, code, y, skip, columns, |cell| {
                            cell.set_bg(bg).set_fg(Color::Black);
                        });
                    }
                }
                if let Some((from, to)) = app.selected_columns(pane_old_side, at) {
                    // Selected text is reversed, up to the end of the line.
                    let columns = (from, to.min(select::line_width(line)));
                    paint_columns(buf, code, y, skip, columns, |cell| {
                        cell.modifier.insert(Modifier::REVERSED);
                    });
                }
            }
        }
    }
}

/// One row of a note: a colored bar, then the note's title, text, reasoning, or reply.
fn draw_note_row(buf: &mut Buffer, area: Rect, code_x: u16, app: &App, index: usize, part: usize) {
    let theme = &app.theme;
    let note = &app.notes[index];
    let panel = theme.panel();
    clear(buf, area, panel);
    // Open notes from the person are yellow, done ones green, and notes from agents magenta.
    // Findings from a code review that wait for a decision are cyan, dismissed ones grey.
    let accent = if index >= app.label_start {
        theme.muted()
    } else if note.is_open() {
        Color::Yellow
    } else if note.is_undecided() {
        Color::Cyan
    } else if note.status.as_deref() == Some("done") {
        Color::Green
    } else if note.status.as_deref() == Some("dismissed") {
        theme.muted()
    } else if note.status.as_deref() == Some("posted") {
        Color::Blue
    } else {
        Color::Magenta
    };
    let rows = notes::rows(note, app.note_width);
    let Some((kind, text)) = rows.get(part) else { return };
    let style = match kind {
        notes::Part::Title => panel.fg(accent).add_modifier(Modifier::BOLD),
        notes::Part::Summary => panel,
        notes::Part::Rationale => panel.fg(theme.muted()).add_modifier(Modifier::ITALIC),
        notes::Part::Reply => panel.fg(Color::Green),
    };
    let picked = app.picked_note == Some(index);
    let bar = if picked { "█ " } else { "▌ " };
    let width = area.width.saturating_sub(code_x - area.x);
    let line = Line::from(vec![Span::styled(bar, panel.fg(accent)), Span::styled(text.clone(), style)]);
    buf.set_line(code_x, area.y, &line, width);
}

/// One side of a file's title row: its path and its line count on that side.
/// A selected title shows its path reversed; copying it copies the whole path.
fn draw_file_title(buf: &mut Buffer, area: Rect, app: &App, index: usize, old_side: bool, selected: bool) {
    let theme = &app.theme;
    let file = &app.files[index];
    clear(buf, area, theme.panel());
    let name = if old_side { &file.diff.old_name } else { &file.diff.new_name };
    let counts = match (app.unified, old_side) {
        (true, _) => format!("-{} +{}", file.removed, file.added),
        (false, true) => format!("-{}", file.removed),
        (false, false) => format!("+{}", file.added),
    };
    let room = area.width.saturating_sub(counts.len() as u16 + 3) as usize;
    let path_style = if selected { theme.file_title().add_modifier(Modifier::REVERSED) } else { theme.file_title() };
    let spans = vec![
        Span::raw(" "),
        Span::styled(fit_path(name, room), path_style),
        Span::raw(" "),
        Span::styled(counts, theme.panel().fg(theme.change_color(old_side))),
    ];
    buf.set_line(area.x, area.y, &Line::from(spans), area.width);
}

/// Drop the first `columns` terminal columns of a styled line, for horizontal scrolling.
fn skip_columns(spans: Vec<Span<'static>>, columns: usize) -> Vec<Span<'static>> {
    if columns == 0 {
        return spans;
    }
    let mut left = columns;
    let mut kept = Vec::new();
    for span in spans {
        if left == 0 {
            kept.push(span);
            continue;
        }
        let mut text = String::new();
        for c in span.content.chars() {
            let width = c.width().unwrap_or(0);
            if left == 0 {
                text.push(c);
            } else if width > left {
                // A wide character cut in half: keep its place with spaces.
                text.push_str(&" ".repeat(width - left));
                left = 0;
            } else {
                left -= width;
            }
        }
        if !text.is_empty() {
            kept.push(Span::styled(text, span.style));
        }
    }
    kept
}

/// How changed text on a changed line is drawn.
#[derive(Clone, Copy)]
struct Change {
    color: Color,
    /// With tints: a stronger background for changed words. Without: color the text instead.
    strong_bg: Option<Color>,
    whole_line: bool,
}

/// Split syntax tokens at word-change boundaries, style them, and expand tabs.
fn styled_tokens(tokens: &Tokens, changed: &[Range<usize>], row: Style, change: Option<Change>, theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut offset = 0;
    for (paint, text) in tokens {
        let start = offset;
        offset += text.len();
        let mut cuts = vec![0, text.len()];
        for range in changed {
            for edge in [range.start, range.end] {
                if edge > start && edge < offset && text.is_char_boundary(edge - start) {
                    cuts.push(edge - start);
                }
            }
        }
        cuts.sort_unstable();
        cuts.dedup();
        for pair in cuts.windows(2) {
            let piece = &text[pair[0]..pair[1]];
            if piece.is_empty() {
                continue;
            }
            let absolute = start + pair[0];
            let in_word = changed.iter().any(|r| r.contains(&absolute));
            let mut style = row.patch(theme.token_style(*paint));
            if let Some(change) = change {
                match change.strong_bg {
                    // Keep syntax colors; mark changed words with a stronger tint.
                    Some(strong) if in_word => style = style.bg(strong),
                    Some(_) => {}
                    // No tints available: changed text takes the side's color.
                    None if change.whole_line || in_word => style = style.fg(change.color),
                    None => {}
                }
            }
            spans.push(Span::styled(piece.replace('\t', TAB), style));
        }
    }
    spans
}

/// Kitty mode leaves blank cells under the image; block mode draws the ribbons itself.
fn draw_ribbons(buf: &mut Buffer, area: Rect, blocks: &[Block], view: View, graphics: Graphics, theme: &Theme) {
    let base = theme.base();
    clear(buf, area, base);
    if graphics == Graphics::Kitty {
        return;
    }
    let background = base.bg.unwrap_or(Color::Reset);
    let color = |c: Option<Rgb>| c.map_or(background, rgb);
    let cells = ribbons::half_blocks(blocks, view, &theme.ribbons);
    for (row, line) in cells.iter().enumerate() {
        for (col, (top, bottom)) in line.iter().enumerate() {
            if top.is_none() && bottom.is_none() {
                continue;
            }
            if let Some(cell) = buf.cell_mut((area.x + col as u16, area.y + row as u16)) {
                cell.set_char('▀');
                cell.set_style(Style::default().fg(color(*top)).bg(color(*bottom)));
            }
        }
    }
}

/// Shorten a path from the left so its file name stays visible.
fn fit_path(path: &str, width: usize) -> String {
    let count = path.chars().count();
    if count <= width || width < 2 {
        return path.to_owned();
    }
    let tail: String = path.chars().skip(count - (width - 1)).collect();
    format!("…{tail}")
}

/// Blank a rectangle: styles alone keep the old characters.
fn clear(buf: &mut Buffer, area: Rect, style: Style) {
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_char(' ');
                cell.set_style(style);
            }
        }
    }
}

fn draw_status(buf: &mut Buffer, area: Rect, app: &App, file: usize) {
    let style = app.theme.panel().fg(app.theme.muted());
    clear(buf, area, style);
    // Say which side is alone on screen, and how to get both back.
    let side = match app.panes {
        _ if app.unified => " UNIFIED (s split)  ·",
        Panes::Both => "",
        Panes::Old => " OLD SIDE ONLY (< both)  ·",
        Panes::New => " NEW SIDE ONLY (> both)  ·",
    };
    let file_part = if app.files.len() > 1 {
        format!("{side} file {}/{}  ·", file + 1, app.files.len())
    } else {
        side.to_owned()
    };
    let open = app.notes.iter().filter(|n| n.is_open()).count();
    let file_part = match (open, app.label_start) {
        (_, 0) => file_part,
        (0, all) => format!("{file_part} {all} notes  ·"),
        (open, _) => format!("{file_part} {open} open notes  ·"),
    };
    let file_part = match (&app.pull_request, &app.checks) {
        (Some(pr), Some(checks)) => format!(" PR #{}  {checks}  ·{file_part}", pr.number),
        (Some(pr), None) => format!(" PR #{}  ·{file_part}", pr.number),
        _ => file_part,
    };
    let undecided = app.notes.iter().filter(|n| n.is_undecided()).count();
    let file_part = if undecided > 0 { format!("{file_part} {undecided} findings to decide (F)  ·") } else { file_part };
    let on_finding = app.picked_note.filter(|&n| app.notes[n].is_undecided() && app.note_on_screen(n)).is_some();
    let message = match (&app.notice, &app.search) {
        _ if app.review.is_some() => app.review.as_ref().zip(app.pull_request.as_ref()).map(|(review, pr)| {
            let notes = match review.notes.len() {
                0 => "no notes".to_owned(),
                1 => "1 note as a comment".to_owned(),
                n => format!("{n} notes as comments"),
            };
            match review.verdict {
                None => format!(" review PR #{} ({}) with {notes}:   c comment  a approve  r request changes   Esc cancel", pr.number, pr.repo),
                Some(verdict) => format!(" {} · summary (optional): {}█   Enter post  Esc cancel", verdict.name(), review.summary),
            }
        }),
        _ if app.visual && app.notice.is_none() => {
            Some(" VISUAL  j/k extend  o other end  J/K move  y copy  a note  Y change  e edit  Esc cancel".to_owned())
        }
        _ if app.copy_menu => Some(" copy:  p path  a absolute path  f file name  l path:line  c change   Esc cancel".to_owned()),
        _ if app.note_draft.is_some() => Some(" typing a note".to_owned()),
        _ if app.goto.is_some() => {
            app.goto.as_ref().map(|text| format!(" :{text}█   line: 42  lines: 40-52  old side: o42  other file: main.rs:42   Enter go  Esc cancel"))
        }
        (Some(notice), _) => Some(format!(" {notice}")),
        (None, Some(found)) if found.editing => {
            Some(format!(" /{}█   {} matches   Enter done  Esc cancel", found.query, found.matches.len()))
        }
        (None, Some(found)) => Some(format!(
            " match {}/{}   /{}   n/N next/previous  Esc clear search  q quit",
            found.current + 1,
            found.matches.len(),
            found.query
        )),
        (None, None) if on_finding => Some(format!("{file_part} finding:  F fix (Claude does it)  D dismiss  }}/{{ other notes")),
        (None, None) => None,
    };
    if let Some(message) = message {
        buf.set_line(area.x, area.y, &Line::styled(message, style), area.width);
        return;
    }
    let context = if app.whole_file { "c: context" } else { "c: whole file" };
    let text = format!(
        "{file_part} change {}/{}{}{}   n/N change  ]/[ file  f files  {context}  a note  }}/{{ notes  z/Z fold  w wrap  s layout  v select  e edit  y/Y copy  / search  q quit  </> side  j/k/space scroll",
        app.current_change(),
        app.stream.blocks.len(),
        match app.moves.len() {
            0 => String::new(),
            1 => "  ·  1 move (m)".to_owned(),
            n => format!("  ·  {n} moves (m)"),
        } + &match app.repeats.len() {
            0 => String::new(),
            1 => "  ·  1 repeated edit (r)".to_owned(),
            n => format!("  ·  {n} repeated edits (r)"),
        },
        if app.hscroll > 0 { format!("  ·  col {}", app.hscroll + 1) } else { String::new() },
    );
    buf.set_line(area.x, area.y, &Line::styled(text, style), area.width);
}

/// A centered list of files; the selected one is highlighted.
/// The places of one repeated edit; the selected one is highlighted.
fn draw_repeat_list(buf: &mut Buffer, area: Rect, app: &App, group: usize, selected: usize) {
    let found = &app.repeats[group];
    let width = area.width.saturating_sub(8).min(110).max(20);
    let height = (found.places.len() as u16 + 2).min(area.height.saturating_sub(4)).max(3);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let frame = match app.theme.rgb {
        Some(_) => app.theme.panel(),
        None => Style::default().bg(Color::Black).fg(Color::Reset),
    };
    clear(buf, Rect::new(x, y, width, height), frame);
    let title = format!(" {} in {} places  (Enter go to, Esc close)", found.what, found.places.len());
    buf.set_line(x, y, &Line::styled(title, frame.add_modifier(Modifier::BOLD)), width);
    let rows = (height - 2) as usize;
    let first = selected.saturating_sub(rows.saturating_sub(1));
    for (row, (index, &(file, block))) in found.places.iter().enumerate().skip(first).take(rows).enumerate() {
        let style = if index == selected { frame.add_modifier(Modifier::REVERSED) } else { frame };
        let row_area = Rect::new(x, y + 1 + row as u16, width, 1);
        clear(buf, row_area, style);
        let diff = &app.files[file].diff;
        let b = diff.model.blocks[block];
        let (line, text) = if b.b1 > b.b0 { (b.b0 + 1, diff.model.new[b.b0].trim()) } else { (b.a0 + 1, diff.model.old[b.a0].trim()) };
        let place = format!("{}:{line}", diff.new_name);
        buf.set_line(x, row_area.y, &Line::styled(format!(" {place:<40} {text}"), style), width);
    }
}

/// A centered list of every note; the selected one is highlighted.
fn draw_note_list(buf: &mut Buffer, area: Rect, app: &App, selected: usize) {
    let theme = &app.theme;
    let width = area.width.saturating_sub(8).min(110).max(20);
    let height = (app.label_start as u16 + 2).min(area.height.saturating_sub(4)).max(3);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let frame = match theme.rgb {
        Some(_) => theme.panel(),
        None => Style::default().bg(Color::Black).fg(Color::Reset),
    };
    clear(buf, Rect::new(x, y, width, height), frame);
    let title = format!(" {} notes  (Enter go to, x delete, F/D fix/dismiss a finding, Esc close)", app.label_start);
    buf.set_line(x, y, &Line::styled(title, frame.add_modifier(Modifier::BOLD)), width);
    let rows = (height - 2) as usize;
    let first = selected.saturating_sub(rows.saturating_sub(1));
    let placed = app.note_rows();
    for (row, (index, note)) in app.notes[..app.label_start].iter().enumerate().skip(first).take(rows).enumerate() {
        let style = if index == selected { frame.add_modifier(Modifier::REVERSED) } else { frame };
        let row_area = Rect::new(x, y + 1 + row as u16, width, 1);
        clear(buf, row_area, style);
        let author = note.author.as_deref().unwrap_or("note");
        let who = note.state().map_or_else(|| author.to_owned(), |state| format!("{author} {state}"));
        let place = match placed.iter().find(|p| p.note == index) {
            Some(p) => format!("{}:{}", note.path, p.line + 1),
            None => format!("{} (not in this diff)", note.path),
        };
        let line = format!(" {who:<12} {place}  {}", note.summary.replace('\n', " "));
        buf.set_line(x, row_area.y, &Line::styled(line, style), width);
    }
}

/// A centered list of the changed files, filtered by what was typed; the selected one is
/// highlighted.
fn draw_picker(buf: &mut Buffer, area: Rect, app: &App, selected: usize) {
    let theme = &app.theme;
    let shown = app.picker_files();
    let width = area.width.saturating_sub(8).min(110).max(20);
    let height = (app.files.len() as u16 + 2).min(area.height.saturating_sub(4)).max(3);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let frame = match theme.rgb {
        Some(_) => theme.panel(),
        None => Style::default().bg(Color::Black).fg(Color::Reset),
    };
    clear(buf, Rect::new(x, y, width, height), frame);
    let count = app.files.len();
    let noun = if count == 1 { "changed file" } else { "changed files" };
    let title = if app.picker_filter.is_empty() {
        format!(" {count} {noun}  (type to filter, ↑/↓ move, Enter open, Esc close)")
    } else {
        format!(" {}/{count} {noun}  filter: {}█   (Esc clears)", shown.len(), app.picker_filter)
    };
    buf.set_line(x, y, &Line::styled(title, frame.add_modifier(Modifier::BOLD)), width);

    let rows = (height - 2) as usize;
    let first = selected.saturating_sub(rows.saturating_sub(1));
    for (row, (place, &index)) in shown.iter().enumerate().skip(first).take(rows).enumerate() {
        let file = &app.files[index];
        let style = if place == selected { frame.add_modifier(Modifier::REVERSED) } else { frame };
        let row_area = Rect::new(x, y + 1 + row as u16, width, 1);
        clear(buf, row_area, style);
        let changes = file.diff.model.blocks.len();
        let spans = vec![
            Span::styled(format!(" {:>3} {:<8}", changes, if changes == 1 { "change" } else { "changes" }), style.fg(theme.muted())),
            Span::styled(format!("{:>5} ", format!("+{}", file.added)), style.fg(theme.change_color(false))),
            Span::styled(format!("{:>5}  ", format!("-{}", file.removed)), style.fg(theme.change_color(true))),
            Span::styled(fit_path(&file.diff.new_name, width.saturating_sub(27) as usize), style),
        ];
        buf.set_line(x, row_area.y, &Line::from(spans), width);
    }
}

fn cell_pixels(columns: u16, rows: u16) -> (u16, u16) {
    match terminal::window_size() {
        Ok(size) if size.width > 0 && size.height > 0 && columns > 0 && rows > 0 => {
            ((size.width / columns).max(1), (size.height / rows).max(1))
        }
        _ => (10, 20),
    }
}

struct App {
    /// Columns scrolled to the right, on both sides.
    hscroll: usize,
    /// The widest line in any file, in columns, to stop scrolling past all text.
    max_width: usize,
    /// Code columns on one side, from the last frame (for half-screen steps).
    code_width: usize,
    files: Vec<FileState>,
    stream: Stream,
    position: usize,
    whole_file: bool,
    context: usize,
    /// The file list, with the selected row of the filtered list.
    picker: Option<usize>,
    /// What was typed in the file list.
    picker_filter: String,
    theme: Theme,
    number_width: u16,
    /// The two sides from the last frame, to place mouse events.
    sides: (Rect, Rect),
    selection: Option<Selection>,
    /// True while the mouse button that started the selection is down.
    dragging: bool,
    /// The selection was made on the line numbers: whole lines, which the keys of `v` change.
    gutter: bool,
    /// A short message that replaces the key help until the next key.
    notice: Option<String>,
    search: Option<Search>,
    /// Body rows from the last frame.
    body_rows: usize,
    panes: Panes,
    /// Counts reloads, so the ribbon image is drawn again for new changes.
    generation: usize,
    /// Folds opened with `z` or a click: new file name, first new line hidden.
    opened: HashSet<(String, usize)>,
    /// Long lines wrap instead of scrolling sideways.
    wrap: bool,
    /// `y` without a selection: the next key picks what to copy.
    copy_menu: bool,
    /// A file and line (zero-based) to open in the editor after this event.
    pending_edit: Option<(PathBuf, usize)>,
    /// The change n/N moved to last, by index.
    picked_change: Option<usize>,
    /// Rows kept above jump targets and around the selection cursor.
    scroll_off: usize,
    keys: Vec<(crate::config::Key, crate::config::Key)>,
    layout_mode: LayoutMode,
    /// The unified layout is on screen (from `layout_mode` and the terminal width).
    unified: bool,
    /// The panes to go back to when the split layout returns.
    split_panes: Panes,
    /// A keyboard selection (`v`) is being made: j/k move its end.
    visual: bool,
    /// The line where the keyboard selection started: side and stream row.
    visual_anchor: (bool, usize),
    /// Notes from the notes file, then from --agent-context.
    notes: Vec<Note>,
    notes_file: PathBuf,
    agent_file: Option<PathBuf>,
    /// Times and sizes of both files when last read, to read them again when they change.
    notes_stamp: String,
    /// Columns for the text of a note.
    note_width: usize,
    /// The note `}`/`{` or the list went to last.
    picked_note: Option<usize>,
    /// A note being typed.
    note_draft: Option<Draft>,
    /// Text columns of the note editor, from the last frame.
    editor_columns: usize,
    /// `:` was pressed: what has been typed after it, such as `42` or `main.rs:40-52`.
    goto: Option<String>,
    /// Blocks of code moved from one place to another; empty with `moved = false`.
    moves: Vec<crate::moves::Move>,
    /// Edits made in more than one place.
    repeats: Vec<crate::repeats::Group>,
    /// Notes before this index are real; the rest are labels ribbon adds (on repeated edits).
    label_start: usize,
    /// `r`: the places of one repeated edit, and the selected row.
    repeat_list: Option<(usize, usize)>,
    find_moves: bool,
    /// The list of notes, with its selected row.
    note_list: Option<usize>,
    /// Lines marked by a `highlight` command.
    mark: Option<Mark>,
    pull_request: Option<crate::github::PullRequest>,
    /// The pull request's checks, such as "CI ✓ 9 passed".
    checks: Option<String>,
    /// A review being made with `P`.
    review: Option<ReviewDraft>,
}

/// `P`: the notes to post, then the verdict picked, then the summary typed.
struct ReviewDraft {
    notes: Vec<usize>,
    verdict: Option<crate::github::Verdict>,
    summary: String,
}

/// Lines marked from outside (`ribbon send highlight`). Kept by name and line, so the mark
/// stays when the rows are built again.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mark {
    path: String,
    old_side: bool,
    /// 1-based, inclusive.
    range: (usize, usize),
}

/// A note being typed: where it goes, and its text so far.
struct Draft {
    path: String,
    old_side: bool,
    /// 1-based, inclusive.
    range: (usize, usize),
    anchor: String,
    text: crate::textarea::TextArea,
    /// The screen row of the note's last line when typing started, to keep it in sight.
    line_y: Option<u16>,
}

/// What follows `:`: an optional path and colon, an optional `o` for the old side, then a
/// line or a range of lines (1-based).
fn parse_goto(text: &str) -> Result<(Option<&str>, bool, (usize, usize)), String> {
    let text = text.trim();
    let (path, lines) = match text.rsplit_once(':') {
        Some((path, lines)) => (Some(path.trim()).filter(|p| !p.is_empty()), lines.trim()),
        None => (None, text),
    };
    let (old_side, lines) = match lines.strip_prefix('o') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, lines),
    };
    let number = |n: &str| n.trim().parse::<usize>().ok().filter(|&n| n >= 1);
    let range = match lines.split_once(['-', ',']) {
        Some((a, b)) => number(a).zip(number(b)),
        None => number(lines).map(|n| (n, n)),
    };
    match range {
        Some((first, last)) if first <= last => Ok((path, old_side, (first, last))),
        Some(_) => Err("the first line comes after the last".into()),
        None => Err(format!("{text:?}: type a line (42), lines (40-52), o42 for the old side, or PATH:42")),
    }
}

/// Where the note editor sits: up to 90 columns wide, tall enough for its text, just below
/// the line the note is on, or above it when there is no room below.
fn note_editor_area(area: Rect, text_rows: usize, line_y: Option<u16>) -> Rect {
    let width = area.width.saturating_sub(4).min(90).max(20.min(area.width));
    let height = (text_rows.clamp(3, 16) as u16 + 2).min(area.height.saturating_sub(2)).max(3.min(area.height));
    // The last row is the status line.
    let bottom = area.y + area.height.saturating_sub(1);
    let y = match line_y {
        Some(line) if line + 1 + height <= bottom => line + 1,
        Some(line) if line >= area.y + height => line - height,
        _ => area.y + (area.height.saturating_sub(height)) / 3,
    };
    Rect::new(area.x + (area.width - width) / 2, y, width, height)
}

/// Text columns inside the note editor: its width less the border and a space on each side.
fn note_editor_columns(area: Rect) -> usize {
    note_editor_area(area, 0, None).width.saturating_sub(4) as usize
}

/// The note being typed, in a box over the diff, with a cursor.
fn draw_note_editor(buf: &mut Buffer, area: Rect, app: &App, draft: &Draft) {
    use ratatui::widgets::{Block, Widget};
    let columns = note_editor_columns(area);
    let rows = draft.text.rows(columns);
    let editor = note_editor_area(area, rows.len(), draft.line_y);
    let frame = match app.theme.rgb {
        Some(_) => app.theme.panel(),
        None => Style::default().bg(Color::Black).fg(Color::Reset),
    };
    clear(buf, editor, frame);
    let lines = if draft.range.0 == draft.range.1 { format!("{}", draft.range.0) } else { format!("{}–{}", draft.range.0, draft.range.1) };
    let side = if draft.old_side { " (old side)" } else { "" };
    let post = if app.pull_request.is_some() { " · Ctrl-P post now" } else { "" };
    Block::bordered()
        .border_style(frame.fg(Color::Yellow))
        .title(Line::styled(format!(" note on {}:{lines}{side} ", draft.path), frame.add_modifier(Modifier::BOLD)))
        .title_bottom(Line::styled(format!(" Enter save · Shift-Enter new line{post} · Esc cancel "), frame.fg(app.theme.muted())))
        .render(editor, buf);
    let (cursor_row, cursor_column) = draft.text.cursor_place(columns);
    let visible = editor.height.saturating_sub(2) as usize;
    let first = cursor_row.saturating_sub(visible.saturating_sub(1));
    let x = editor.x + 2;
    for (screen_row, &row) in rows.iter().enumerate().skip(first).take(visible).map(|(i, r)| (i - first, r)) {
        let y = editor.y + 1 + screen_row as u16;
        buf.set_line(x, y, &Line::styled(draft.text.row_text(row), frame), columns as u16);
    }
    if draft.text.is_blank() && cursor_row == 0 {
        buf.set_line(x + 1, editor.y + 1, &Line::styled("What should change here?", frame.fg(app.theme.muted()).add_modifier(Modifier::ITALIC)), columns as u16 - 1);
    }
    // The cursor: the cell under it reversed.
    let (cx, cy) = (x + cursor_column.min(columns) as u16, editor.y + 1 + (cursor_row - first) as u16);
    if let Some(cell) = buf.cell_mut((cx, cy)) {
        if cell.symbol().trim().is_empty() {
            cell.set_symbol(" ");
        }
        cell.modifier.insert(Modifier::REVERSED);
    }
}

impl App {
    /// Show a new set of changed files (from `--watch`) and stay on the same file and line.
    /// Files whose text did not change keep their colors. Returns false when nothing is shown
    /// differently.
    fn reload(&mut self, diffs: Vec<FileDiff>) -> bool {
        if diffs.is_empty() {
            self.notice = Some("no changes now".into());
            return false;
        }
        // Where the view is: a file, a new-side line near the top (its index and text), and its
        // screen row.
        let (_, right_top) = self.stream.scroll.tops(self.position);
        let anchor = self.stream.right.iter().enumerate().skip(right_top).find_map(|(row, r)| match *r {
            Row::Line(file, index) => {
                let diff = &self.files[file].diff;
                Some((diff.new_name.clone(), Some((index, diff.model.new[index].clone())), row - right_top))
            }
            Row::Header(file) => Some((self.files[file].diff.new_name.clone(), None, row - right_top)),
            _ => None,
        });
        let wrap = self.stream.wrap;

        let mut old_files: Vec<FileState> = std::mem::take(&mut self.files);
        self.files = diffs
            .into_iter()
            .map(|diff| {
                let mut state = FileState::new(diff);
                let same = old_files.iter_mut().find(|f| {
                    f.diff.new_name == state.diff.new_name && f.diff.model.old == state.diff.model.old && f.diff.model.new == state.diff.model.new
                });
                if let Some(same) = same {
                    state.old_tokens = std::mem::take(&mut same.old_tokens);
                    state.new_tokens = std::mem::take(&mut same.new_tokens);
                }
                state
            })
            .collect();
        (self.max_width, self.number_width) = measure(&self.files);
        if self.find_moves {
            self.moves = mark_moves(&mut self.files);
        }
        self.repeats = mark_repeats(&mut self.files);
        self.add_labels();
        self.stream = self.build_stream(wrap);
        self.generation += 1;
        self.selection = None;
        self.dragging = false;
        self.picked_change = None;
        if self.picker.is_some() {
            self.picker = Some(0);
        }

        self.position = match anchor {
            Some((name, line, offset)) => match self.files.iter().position(|f| f.diff.new_name == name) {
                Some(file) => {
                    // Lines above may have been added or removed: look for the same text nearest
                    // to the old place. Then the first shown line from there, or the file title.
                    let new_lines = &self.files[file].diff.model.new;
                    let line = line.map(|(index, text)| {
                        new_lines.iter().enumerate().filter(|(_, l)| **l == text).map(|(i, _)| i).min_by_key(|i| i.abs_diff(index)).unwrap_or(index)
                    });
                    let row = line
                        .and_then(|line| self.stream.right.iter().position(|r| matches!(*r, Row::Line(f, i) if f == file && i >= line)))
                        .unwrap_or_else(|| self.header_row(file, false));
                    self.stream.scroll.position_of_row(row, false).saturating_sub(offset)
                }
                None => self.position,
            },
            None => self.position,
        };
        if self.search.is_some() {
            self.update_matches();
            self.current_from(self.position);
        }
        self.notice = Some("reloaded".into());
        true
    }

    /// Unchanged lines shown around changes, or None for whole files.
    fn context_lines(&self) -> Option<usize> {
        (!self.whole_file).then_some(self.context)
    }

    fn rows(&self, old_side: bool) -> &[Row] {
        if old_side { &self.stream.left } else { &self.stream.right }
    }

    /// The file and line of a row, and which screen row of a wrapped line it is.
    /// The line of a row: whether it is an old line, its file and index, and which screen row of
    /// a wrapped line it is. In the unified layout one column holds lines of both versions.
    fn line_of(&self, old_side: bool, row: usize) -> Option<(bool, usize, usize, usize)> {
        // In the unified column, plain lines are new lines on either "side".
        let side_is_old = old_side && !self.stream.unified;
        match self.rows(old_side).get(row)? {
            &Row::Line(file, line) => Some((side_is_old, file, line, 0)),
            &Row::Wrap(file, line, part) => Some((side_is_old, file, line, part)),
            &Row::Old(file, line, part) => Some((true, file, line, part)),
            _ => None,
        }
    }

    /// The text of a row's line.
    fn text_of(&self, old_side: bool, row: usize) -> Option<&str> {
        let (is_old, file, line, _) = self.line_of(old_side, row)?;
        let model = &self.files[file].diff.model;
        Some(if is_old { &model.old[line] } else { &model.new[line] })
    }

    /// The selected text columns of a row's line (whole line, not just the row), end
    /// excluded, or None when the row is outside the selection.
    fn selected_columns(&self, old_side: bool, row: usize) -> Option<(usize, usize)> {
        let selection = self.selection.filter(|s| s.old_side == old_side)?;
        let (start, end) = selection.bounds();
        if row < start.0 || row > end.0 {
            return None;
        }
        let line = |r: usize| self.line_of(old_side, r).map(|(is_old, file, line, _)| (is_old, file, line));
        let from = if line(row) == line(start.0) { start.1 } else { 0 };
        let to = if line(row) == line(end.0) { end.1 + 1 } else { usize::MAX };
        Some((from, to))
    }

    /// Build the rows again (after opening a fold or changing the wrap width), keeping the
    /// same line at the same place on screen.
    fn rebuild(&mut self, wrap: Option<usize>) {
        let (_, right_top) = self.stream.scroll.tops(self.position);
        let anchor = (right_top..self.stream.right.len()).find_map(|row| match self.stream.right[row] {
            Row::Line(file, line) => Some((Row::Line(file, line), 0, row - right_top)),
            Row::Wrap(file, line, part) => Some((Row::Line(file, line), part, row - right_top)),
            header @ Row::Header(_) => Some((header, 0, row - right_top)),
            _ => None,
        });
        self.stream = self.build_stream(wrap);
        self.generation += 1;
        self.selection = None;
        if let Some((target, part, offset)) = anchor {
            if let Some(row) = self.stream.right.iter().position(|r| *r == target) {
                // Stay on the same slice of a wrapped line, if it still has that many.
                let row = (row..row + part + 1).take_while(|&r| r == row || matches!(self.stream.right.get(r), Some(Row::Wrap(..)))).last().unwrap_or(row);
                self.position = self.stream.scroll.position_of_row(row, false).saturating_sub(offset);
            }
        }
        if self.search.is_some() {
            self.update_matches();
            self.current_from(self.position);
        }
    }

    /// Show the lines of a fold.
    fn open_fold(&mut self, file: usize, start: usize) {
        self.opened.insert((self.files[file].diff.new_name.clone(), start));
        self.rebuild(self.stream.wrap);
    }

    /// `z`: open the first fold on screen.
    fn open_fold_on_screen(&mut self) {
        let old_side = self.panes == Panes::Old;
        let (left_top, right_top) = self.stream.scroll.tops(self.position);
        let top = if old_side { left_top } else { right_top };
        let fold = self.shown_rows(old_side, top, self.body_rows).into_iter().find_map(|row| match self.rows(old_side)[row] {
            Row::Fold(file, _, start) => Some((file, start)),
            _ => None,
        });
        match fold {
            Some((file, start)) => self.open_fold(file, start),
            None => self.notice = Some("no fold on screen (c shows whole files)".into()),
        }
    }

    /// The new-side line at, or level with, a row: the row itself on the new side; on the old
    /// side, the first new line of the same file from the level row down.
    fn new_line_near(&self, old_side: bool, row: usize) -> Option<(usize, usize)> {
        let (is_old, file, line, _) = self.line_of(old_side, row).or_else(|| match self.rows(old_side).get(row)? {
            &Row::Header(file) => Some((false, file, 0, 0)),
            _ => None,
        })?;
        if !is_old {
            return Some((file, line));
        }
        // Unified: the first new line below.
        if self.stream.unified {
            return (row..self.stream.right.len()).find_map(|r| match self.stream.right[r] {
                Row::Line(f, line) if f == file => Some((f, line)),
                Row::Header(f) if f != file => Some((file, 0)),
                _ => None,
            });
        }
        let (_, right_top) = self.stream.scroll.tops(self.stream.scroll.position_of_row(row, true));
        (right_top..self.stream.right.len()).take(500).find_map(|r| match self.stream.right[r] {
            Row::Line(f, line) if f == file => Some((f, line)),
            Row::Header(f) if f != file => Some((file, 0)),
            _ => None,
        })
    }

    /// The line that `e` opens and `y l` copies: the start of the selection, or else the change
    /// picked with n/N, or else the first changed new line on screen, or else the first new line
    /// on screen.
    fn focus(&self) -> Option<(usize, usize)> {
        if let Some(selection) = self.selection {
            let (start, _) = selection.bounds();
            return self.new_line_near(selection.old_side, start.0);
        }
        // The change picked with n/N.
        if let Some(block) = self.picked_change.filter(|&i| self.change_on_screen(i)).and_then(|i| self.stream.blocks.get(i)) {
            let new_line = (block.b0..block.b1).find_map(|row| self.new_line_near(false, row));
            if let Some(found) = new_line.or_else(|| self.new_line_near(true, block.a0)) {
                return Some(found);
            }
        }
        let old_side = self.panes == Panes::Old;
        let (left_top, right_top) = self.stream.scroll.tops(self.position);
        let top = if old_side { left_top } else { right_top };
        let shown = self.shown_rows(old_side, top, self.body_rows.max(1));
        let changed = shown.iter().find(|&&row| {
            self.line_of(old_side, row).is_some_and(|(is_old, file, line, _)| {
                let state = &self.files[file];
                (if is_old { &state.old_kinds } else { &state.new_kinds })[line].is_some()
            })
        });
        let any = shown.iter().find(|&&row| self.line_of(old_side, row).is_some());
        changed.or(any).and_then(|&row| self.new_line_near(old_side, row))
    }

    /// The change that holds the selection, or else the current one, as a patch. With one
    /// side alone, that side's lines.
    fn change_text(&self) -> Option<(String, &'static str)> {
        let index = match self.selection {
            Some(selection) => {
                let row = selection.bounds().0 .0;
                self.stream.blocks.iter().position(|b| {
                    let (from, to) = if selection.old_side { (b.a0, b.a1) } else { (b.b0, b.b1) };
                    (from..to).contains(&row)
                })
            }
            None => None,
        }
        .unwrap_or(self.current_change().checked_sub(1)?);
        let block = *self.stream.blocks.get(index)?;
        // A side's lines in the change (in the unified column, the old or new ones).
        let lines = |old_side: bool, rows: Range<usize>| -> Vec<(usize, usize)> {
            rows.filter_map(|r| match self.line_of(old_side, r)? {
                (is_old, file, line, 0) if is_old == old_side => Some((file, line)),
                _ => None,
            })
            .collect()
        };
        let (old, new) = (lines(true, block.a0..block.a1), lines(false, block.b0..block.b1));
        let file = old.first().or(new.first())?.0;
        let model = &self.files[file].diff.model;
        let text = |old_side: bool, lines: &[(usize, usize)]| -> Vec<String> {
            lines.iter().map(|&(_, l)| if old_side { model.old[l].clone() } else { model.new[l].clone() }).collect()
        };
        match self.panes {
            _ if self.unified => {}
            Panes::Old => return Some((text(true, &old).join("\n"), "old lines")),
            Panes::New => return Some((text(false, &new).join("\n"), "new lines")),
            Panes::Both => {}
        }
        // Where an empty side sits: after the last line above it on that side.
        let start = |old_side: bool, lines: &[(usize, usize)], first_row: usize| -> usize {
            match lines.first() {
                Some(&(_, line)) => line + 1,
                None => (0..first_row).rev().find_map(|r| match (self.rows(old_side)[r], self.line_of(old_side, r)) {
                    (_, Some((is_old, f, line, 0))) if is_old == old_side && f == file => Some(line + 1),
                    (Row::Header(_), _) => Some(0),
                    _ => None,
                }).unwrap_or(0),
            }
        };
        let diff = &self.files[file].diff;
        let mut patch = format!(
            "--- a/{}\n+++ b/{}\n@@ -{},{} +{},{} @@\n",
            diff.old_name,
            diff.new_name,
            start(true, &old, block.a0),
            old.len(),
            start(false, &new, block.b0),
            new.len()
        );
        for line in text(true, &old) {
            patch.push_str(&format!("-{line}\n"));
        }
        for line in text(false, &new) {
            patch.push_str(&format!("+{line}\n"));
        }
        Some((patch, "change"))
    }

    fn copy_text(&mut self, text: &str, what: &str) {
        self.notice = Some(match select::copy(text) {
            Ok(()) if text.lines().count() > 1 => format!("copied {what} ({} lines)", text.lines().count()),
            Ok(()) => format!("copied {what}: {text}"),
            Err(error) => format!("copy failed: {error}"),
        });
    }

    /// The key after `y` without a selection.
    fn apply_copy_menu(&mut self, key: &KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        self.copy_menu = false;
        let focus = self.focus();
        let file = focus.map_or_else(|| self.current_file(), |(file, _)| file);
        let Some(state) = self.files.get(file) else { return };
        let name = state.diff.new_name.clone();
        let copied = match key.code {
            KeyCode::Char('p') => Some((name, "path")),
            KeyCode::Char('a') => {
                let absolute = state.diff.path.as_ref().map(|p| std::path::absolute(p).unwrap_or_else(|_| p.clone()));
                Some((absolute.map_or(name, |p| p.display().to_string()), "absolute path"))
            }
            KeyCode::Char('f') => {
                let file_name = std::path::Path::new(&name).file_name().map(|n| n.to_string_lossy().into_owned());
                Some((file_name.unwrap_or(name), "file name"))
            }
            KeyCode::Char('l') => focus.map(|(_, line)| (format!("{name}:{}", line + 1), "path:line")),
            KeyCode::Char('c') => self.change_text(),
            _ => None,
        };
        if let Some((text, what)) = copied {
            self.copy_text(&text, what);
        }
    }

    /// `e`: open the focus line in the editor, after this event.
    fn request_edit(&mut self) {
        let Some((file, line)) = self.focus() else {
            self.notice = Some("no line to open".into());
            return;
        };
        match self.files[file].diff.path.clone().filter(|p| p.exists()) {
            Some(path) => self.pending_edit = Some((path, line)),
            None => {
                self.notice = Some(match &self.pull_request {
                    Some(pr) => format!("to edit, check out the pull request with no changes: gh pr checkout {}", pr.number),
                    None => format!("{} is not a file on disk", self.files[file].diff.new_name),
                })
            }
        }
    }

    fn build_stream(&self, wrap: Option<usize>) -> Stream {
        Stream::new(&self.files, self.context_lines(), &self.opened, wrap, &self.note_rows(), self.unified)
    }

    /// Where each note shows: its file in the diff, side, and line. Notes on files that are not
    /// in the diff are left out (the list still shows them).
    fn note_rows(&self) -> Vec<NoteRows> {
        let width = self.note_width.max(20);
        self.notes
            .iter()
            .enumerate()
            .filter_map(|(index, note)| {
                let file = self.files.iter().position(|f| *if note.old_side { &f.diff.old_name } else { &f.diff.new_name } == note.path)?;
                let model = &self.files[file].diff.model;
                let line = notes::place(note, if note.old_side { &model.old } else { &model.new })?;
                // The unified column shows old lines only inside changes: a note on any other old
                // line goes under the new line paired with it.
                let (old_side, line) = match (self.unified && note.old_side).then(|| model.new_line_outside_changes(line)).flatten() {
                    Some(new_line) => (false, new_line.min(model.new.len().saturating_sub(1))),
                    None => (note.old_side, line),
                };
                Some(NoteRows { file, old_side, line, note: index, rows: notes::rows(note, width).len() })
            })
            .collect()
    }

    fn notes_stamp(&self) -> String {
        let agent = self.agent_file.as_deref().map(crate::git::stamp).unwrap_or_default();
        format!("{}|{agent}", crate::git::stamp(&self.notes_file))
    }

    /// Read both notes files. A file that can't be read keeps the notes on screen and says why.
    fn load_notes(&mut self) {
        self.notes_stamp = self.notes_stamp();
        let mut loaded = match notes::load(&self.notes_file, true) {
            Ok(notes) => notes,
            Err(error) => {
                self.notice = Some(format!("{error:#}"));
                return;
            }
        };
        if let Some(agent) = &self.agent_file {
            match notes::load(agent, false) {
                Ok(notes) => loaded.extend(notes),
                Err(error) => self.notice = Some(format!("{error:#}")),
            }
        }
        self.notes = loaded;
        self.label_start = self.notes.len();
        self.add_labels();
        self.picked_note = self.picked_note.filter(|&n| n < self.label_start);
    }

    /// A label under the first place of each repeated edit, kept after the real notes.
    fn add_labels(&mut self) {
        self.notes.truncate(self.label_start.min(self.notes.len()));
        self.label_start = self.notes.len();
        for group in &self.repeats {
            let Some(&(file, block)) = group.places.first() else { continue };
            let diff = &self.files[file].diff;
            let b = diff.model.blocks[block];
            let (old_side, path, line) = if b.b1 > b.b0 { (false, &diff.new_name, b.b1) } else { (true, &diff.old_name, b.a1) };
            let others = group.places.len() - 1;
            self.notes.push(Note {
                id: None,
                path: path.clone(),
                old_side,
                range: (line, line),
                anchor: None,
                summary: format!("{}. The {others} other place{} show dimmer; n skips them, r lists them.", group.what, if others == 1 { "" } else { "s" }),
                rationale: None,
                author: Some(format!("same edit in {} places", group.places.len())),
                status: None,
                reply: None,
                editable: false,
            });
        }
    }

    /// The index of a block in the whole stream, as n/N count changes.
    fn change_index(&self, file: usize, block: usize) -> usize {
        self.files[..file].iter().map(|f| f.diff.model.blocks.len()).sum::<usize>() + block
    }

    /// The file and block of a change in the whole stream.
    fn change_at(&self, mut index: usize) -> Option<(usize, usize)> {
        for (file, state) in self.files.iter().enumerate() {
            if index < state.diff.model.blocks.len() {
                return Some((file, index));
            }
            index -= state.diff.model.blocks.len();
        }
        None
    }

    /// True for a change that repeats an edit seen earlier: n and N skip it.
    fn is_seen_repeat(&self, index: usize) -> bool {
        self.change_at(index).and_then(|(file, block)| self.files[file].block_repeat[block]).is_some_and(|(_, place)| place > 0)
    }

    /// `r`: list the places of the repeated edit at the current change.
    fn open_repeat_list(&mut self) {
        let here = self.current_change().checked_sub(1).and_then(|i| self.change_at(i)).and_then(|(file, block)| self.files[file].block_repeat[block]);
        match here {
            Some((group, place)) => self.repeat_list = Some((group, place)),
            None if self.repeats.is_empty() => self.notice = Some("no edit is repeated in this diff".into()),
            None => self.notice = Some(format!("this change is not repeated; {} edits are (n goes to them)", self.repeats.len())),
        }
    }

    fn apply_repeat_list(&mut self, key: &KeyEvent, group: usize, selected: usize) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let last = self.repeats[group].places.len().saturating_sub(1);
        self.repeat_list = match key.code {
            KeyCode::Esc | KeyCode::Char('r') | KeyCode::Char('q') => None,
            KeyCode::Enter => {
                let (file, block) = self.repeats[group].places[selected];
                let index = self.change_index(file, block);
                if let Some(&start) = self.stream.change_starts(self.scroll_off).get(index) {
                    self.position = start;
                    self.picked_change = Some(index);
                }
                None
            }
            KeyCode::Char('j') | KeyCode::Down => Some((group, (selected + 1).min(last))),
            KeyCode::Char('k') | KeyCode::Up => Some((group, selected.saturating_sub(1))),
            _ => Some((group, selected)),
        };
    }

    /// Read the notes again when a notes file changed. Returns true when it did.
    fn refresh_notes(&mut self) -> bool {
        if self.notes_stamp() == self.notes_stamp {
            return false;
        }
        self.load_notes();
        self.rebuild(self.stream.wrap);
        true
    }

    /// The side and stream row where a note starts.
    fn note_row(&self, note: usize) -> Option<(bool, usize)> {
        [false, true].into_iter().find_map(|old_side| self.rows(old_side).iter().position(|r| *r == Row::Note(note, 0)).map(|row| (old_side, row)))
    }

    fn note_on_screen(&self, note: usize) -> bool {
        let Some((old_side, row)) = self.note_row(note) else { return false };
        let (left_top, right_top) = self.stream.scroll.tops(self.position);
        let top = if old_side { left_top } else { right_top };
        row >= top && row < top + self.body_rows
    }

    /// Scroll to a note, showing its line above it. A note on the hidden side shows both sides.
    fn go_to_note(&mut self, note: usize) {
        let Some((old_side, row)) = self.note_row(note) else {
            self.notice = Some(format!("{} is not in this diff", self.notes[note].path));
            return;
        };
        if (old_side && self.panes == Panes::New) || (!old_side && self.panes == Panes::Old) {
            self.panes = Panes::Both;
        }
        self.picked_note = Some(note);
        self.position = self.stream.scroll.position_of_row(row, old_side).saturating_sub(self.scroll_off + 1);
    }

    /// `}` and `{`: the next or previous note in the stream.
    fn step_note(&mut self, forward: bool) {
        let mut order: Vec<(usize, usize)> = (0..self.label_start)
            .filter_map(|n| self.note_row(n).map(|(old_side, row)| (self.stream.scroll.position_of_row(row, old_side), n)))
            .collect();
        order.sort();
        if order.is_empty() {
            self.notice = Some("no notes (a adds one)".into());
            return;
        }
        let here = self.picked_note.filter(|&n| self.note_on_screen(n)).and_then(|n| order.iter().position(|&(_, m)| m == n));
        let index = match (here, forward) {
            (Some(i), true) => (i + 1 < order.len()).then_some(i + 1),
            (Some(i), false) => i.checked_sub(1),
            (None, true) => order.iter().position(|&(p, _)| p > self.position),
            (None, false) => order.iter().rposition(|&(p, _)| p <= self.position),
        };
        match index {
            Some(i) => self.go_to_note(order[i].1),
            None => self.notice = Some(if forward { "no more notes below" } else { "no more notes above" }.into()),
        }
    }

    /// The note `x` deletes: the one picked last, while it is on screen.
    fn delete_note(&mut self, note: usize) {
        let Some(found) = self.notes.get(note) else { return };
        let (true, Some(id)) = (found.editable, found.id.clone()) else {
            self.notice = Some("notes from --agent-context can't be deleted".into());
            return;
        };
        match notes::remove(&self.notes_file, &id) {
            Ok(()) => {
                self.notice = Some("note deleted".into());
                self.picked_note = None;
                self.load_notes();
                self.rebuild(self.stream.wrap);
            }
            Err(error) => self.notice = Some(format!("{error:#}")),
        }
    }

    /// `P`: start a review of the pull request, with the open notes (the person's, and the
    /// findings marked to fix) as its comments.
    fn ask_to_post(&mut self) {
        if self.pull_request.is_none() {
            self.notice = Some("P reviews a pull request: open one with ribbon pr NUMBER".into());
            return;
        }
        let notes: Vec<usize> = (0..self.notes.len()).filter(|&n| self.notes[n].is_open()).collect();
        self.review = Some(ReviewDraft { notes, verdict: None, summary: String::new() });
    }

    /// Keys while making a review: c/a/r pick the verdict, then the summary is typed.
    fn apply_review_key(&mut self, key: &KeyEvent) {
        use crate::github::Verdict;
        if key.kind == KeyEventKind::Release {
            return;
        }
        let Some(review) = &mut self.review else { return };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let cancel = key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('c'));
        match review.verdict {
            None => {
                review.verdict = match key.code {
                    KeyCode::Char('c') if !ctrl => Some(Verdict::Comment),
                    KeyCode::Char('a') => Some(Verdict::Approve),
                    KeyCode::Char('r') => Some(Verdict::RequestChanges),
                    _ => None,
                };
                match review.verdict {
                    // Plain keys while typing: the Kitty keyboard protocol can split composed characters.
                    Some(_) => {
                        let _ = io::stdout().write_all(KEYS_OFF.as_bytes());
                    }
                    None => {
                        self.review = None;
                        self.notice = Some("review canceled".into());
                    }
                }
            }
            Some(_) if cancel => {
                self.review = None;
                self.notice = Some("review canceled".into());
                let _ = io::stdout().write_all(KEYS_ON.as_bytes());
            }
            Some(verdict) => match key.code {
                KeyCode::Enter => {
                    let review = self.review.take().expect("a review");
                    let _ = io::stdout().write_all(KEYS_ON.as_bytes());
                    self.post(&review.notes, verdict, &review.summary);
                }
                KeyCode::Backspace => {
                    review.summary.pop();
                }
                KeyCode::Char(c) if !ctrl => review.summary.push(c),
                _ => {}
            },
        }
    }

    /// Post notes as one review, then mark them "posted" so they are not posted again and
    /// are no longer tasks for Claude.
    fn post(&mut self, notes: &[usize], verdict: crate::github::Verdict, summary: &str) {
        let Some(pr) = self.pull_request.clone() else { return };
        let chosen: Vec<&Note> = notes.iter().filter_map(|&n| self.notes.get(n)).collect();
        let result = crate::github::commentable(&pr).and_then(|lines| {
            let (comments, elsewhere) = crate::github::review(&chosen, &lines);
            let count = comments.len();
            crate::github::post_review(&pr, verdict, summary, comments, &elsewhere).map(|url| (url, count, elsewhere.len()))
        });
        match result {
            Ok((url, on_lines, in_text)) => {
                let ids: Vec<String> = chosen.iter().filter_map(|n| n.id.clone()).collect();
                let marked = self.mark_posted(&ids);
                let text = if in_text > 0 { format!(", {in_text} in the review text (lines not in the diff)") } else { String::new() };
                let comments = if on_lines == 1 { "1 comment".to_owned() } else { format!("{on_lines} comments") };
                self.notice = Some(match marked {
                    Ok(()) => format!("posted the review ({}) with {comments}{text}: {url}", verdict.name()),
                    Err(error) => format!("posted ({url}), but could not mark the notes: {error:#}"),
                });
            }
            Err(error) => self.notice = Some(format!("{error:#}")),
        }
    }

    /// Ctrl-P while typing a note: post that note as a comment at once, not in a review.
    fn post_now(&mut self, note: usize) {
        let Some(pr) = self.pull_request.clone() else { return };
        let Some(found) = self.notes.get(note) else { return };
        let result = crate::github::commentable(&pr).and_then(|lines| {
            let (mut comments, _) = crate::github::review(&[found], &lines);
            if comments.is_empty() {
                anyhow::bail!("note saved; its lines are not in the pull request's diff, so it can only go in the review text (P)");
            }
            crate::github::post_comment(&pr, comments.remove(0))
        });
        match result {
            Ok(url) => {
                let ids: Vec<String> = found.id.iter().cloned().collect();
                self.notice = Some(match self.mark_posted(&ids) {
                    Ok(()) => format!("posted: {url}"),
                    Err(error) => format!("posted ({url}), but could not mark the note: {error:#}"),
                });
            }
            Err(error) => self.notice = Some(format!("{error:#}")),
        }
    }

    fn mark_posted(&mut self, ids: &[String]) -> anyhow::Result<()> {
        let marked = ids.iter().try_for_each(|id| notes::set_status(&self.notes_file, id, "posted"));
        self.load_notes();
        self.rebuild(self.stream.wrap);
        marked
    }

    /// `F` and `D`: mark a finding to fix ("open") or dismiss it, then go to the next one.
    fn decide(&mut self, note: usize, status: &str, advance: bool) {
        let Some(found) = self.notes.get(note) else { return };
        let (true, Some(id)) = (found.is_finding() && found.editable, found.id.clone()) else {
            self.notice = Some("F and D act on findings from a code review".into());
            return;
        };
        if let Err(error) = notes::set_status(&self.notes_file, &id, status) {
            self.notice = Some(format!("{error:#}"));
            return;
        }
        self.load_notes();
        self.rebuild(self.stream.wrap);
        self.picked_note = self.notes.iter().position(|n| n.id.as_deref() == Some(&id));
        let left = self.notes.iter().filter(|n| n.is_undecided()).count();
        let done = if status == "open" { "to fix" } else { "dismissed" };
        if advance && left > 0 {
            self.next_finding();
        }
        self.notice = Some(match left {
            0 => format!("finding {done}; all decided: ask Claude to fix the open ones"),
            _ => format!("finding {done}; {left} left  (F fix  D dismiss)"),
        });
    }

    /// The next finding that waits for a decision, after the picked note; at the end, the
    /// first one.
    fn next_finding(&mut self) {
        let mut order: Vec<(usize, usize)> = (0..self.notes.len())
            .filter(|&n| self.notes[n].is_undecided())
            .filter_map(|n| self.note_row(n).map(|(old_side, row)| (self.stream.scroll.position_of_row(row, old_side), n)))
            .collect();
        order.sort();
        let here = self.picked_note.and_then(|n| self.note_row(n)).map_or(self.position, |(old_side, row)| self.stream.scroll.position_of_row(row, old_side));
        match order.iter().find(|&&(p, n)| p > here || (p == here && Some(n) != self.picked_note)).or(order.first()) {
            Some(&(_, note)) => {
                self.go_to_note(note);
                self.notice = Some("finding:  F fix (Claude does it)  D dismiss".into());
            }
            None if self.notes.iter().any(|n| n.is_undecided()) => self.notice = Some("the findings are on files not in this diff (A lists them)".into()),
            None => self.notice = Some("no findings to decide".into()),
        }
    }

    /// The file a command names: by its name on that side, or else the one file whose name
    /// ends with it.
    fn file_named(&self, path: &str, old_side: bool) -> Result<usize, String> {
        let name = |f: &FileState| if old_side { f.diff.old_name.clone() } else { f.diff.new_name.clone() };
        if let Some(file) = self.files.iter().position(|f| name(f) == path) {
            return Ok(file);
        }
        let ending = format!("/{}", path.trim_start_matches("./"));
        let found: Vec<usize> = (0..self.files.len()).filter(|&f| name(&self.files[f]).ends_with(&ending)).collect();
        match found.as_slice() {
            [file] => Ok(*file),
            [] => Err(format!("{path} is not in this diff")),
            _ => Err(format!("{path} names {} files in this diff; give more of the path", found.len())),
        }
    }

    /// The pane and stream row of a line (zero-based) of one version of a file, if shown.
    fn line_row(&self, file: usize, old_side: bool, line: usize) -> Option<(bool, usize)> {
        if !self.stream.unified {
            return self.rows(old_side).iter().position(|r| *r == Row::Line(file, line)).map(|row| (old_side, row));
        }
        // The unified column shows old lines only inside changes; others sit at their new line.
        let model = &self.files[file].diff.model;
        let target = match old_side.then(|| model.new_line_outside_changes(line)) {
            Some(Some(new_line)) => Row::Line(file, new_line),
            Some(None) => Row::Old(file, line, 0),
            None => Row::Line(file, line),
        };
        self.stream.right.iter().position(|r| *r == target).map(|row| (false, row))
    }

    /// Show a line that a fold hides. Returns false when it can't be shown.
    fn reveal(&mut self, file: usize, old_side: bool, line: usize) -> bool {
        if self.line_row(file, old_side, line).is_some() {
            return true;
        }
        let model = &self.files[file].diff.model;
        let Some(new_line) = (if old_side { model.new_line_outside_changes(line) } else { Some(line) }) else { return false };
        let fold = self.stream.right.iter().find_map(|r| match *r {
            Row::Fold(f, hidden, start) if f == file && start <= new_line && new_line < start.saturating_add(hidden) => Some(start),
            _ => None,
        });
        match fold {
            Some(start) => {
                self.open_fold(file, start);
                self.line_row(file, old_side, line).is_some()
            }
            None => false,
        }
    }

    /// Scroll to a file, or to a line (zero-based) of it, opening its fold and showing its side.
    fn navigate(&mut self, file: usize, old_side: bool, line: Option<usize>) -> Result<(), String> {
        let model = &self.files[file].diff.model;
        let count = if old_side { model.old.len() } else { model.new.len() };
        if let Some(line) = line.filter(|&l| l >= count) {
            return Err(format!("line {} is past the end ({count} lines)", line + 1));
        }
        // The unified column holds both versions.
        if !self.stream.unified && ((old_side && self.panes == Panes::New) || (!old_side && self.panes == Panes::Old)) {
            self.panes = Panes::Both;
        }
        let row = match line {
            Some(line) if self.reveal(file, old_side, line) => self.line_row(file, old_side, line),
            _ => None,
        };
        self.position = match row {
            Some((pane, row)) => self.stream.scroll.position_of_row(row, pane).saturating_sub(self.scroll_off + 1),
            None => self.stream.scroll.position_of_row(self.header_row(file, old_side), old_side),
        };
        Ok(())
    }

    /// A command from `ribbon send`. Reloading is left to the event loop, which owns the
    /// loader and the colors.
    fn command(&mut self, command: crate::listen::Command) -> serde_json::Value {
        use crate::listen::{error, ok, Command};
        use serde_json::json;
        match command {
            Command::Navigate { path, old_side, line } => {
                match self.file_named(&path, old_side).and_then(|file| self.navigate(file, old_side, line.map(|l| l - 1))) {
                    Ok(()) => ok(json!({})),
                    Err(problem) => error(problem),
                }
            }
            Command::Highlight { path, old_side, range } => {
                let file = match self.file_named(&path, old_side) {
                    Ok(file) => file,
                    Err(problem) => return error(problem),
                };
                // Open every fold the range crosses, up to a sane size.
                for line in (range.0 - 1..range.1).take(2000) {
                    self.reveal(file, old_side, line);
                }
                if let Err(problem) = self.navigate(file, old_side, Some(range.0 - 1)) {
                    return error(problem);
                }
                let diff = &self.files[file].diff;
                let path = if old_side { diff.old_name.clone() } else { diff.new_name.clone() };
                self.mark = Some(Mark { path, old_side, range });
                ok(json!({}))
            }
            Command::Clear => {
                self.mark = None;
                ok(json!({}))
            }
            Command::Note { path, old_side, range, summary, rationale, author, status } => {
                // A note on a file that is not in the diff is kept, and shows when the file does.
                let (path, anchor, shown) = match self.file_named(&path, old_side) {
                    Ok(file) => {
                        let diff = &self.files[file].diff;
                        let (name, lines) = if old_side { (&diff.old_name, &diff.model.old) } else { (&diff.new_name, &diff.model.new) };
                        (name.clone(), lines.get(range.1 - 1).cloned(), true)
                    }
                    Err(_) => (path, None, false),
                };
                let id = notes::new_id();
                let note = Note { id: Some(id.clone()), path, old_side, range, anchor, summary, rationale, author: Some(author), status, reply: None, editable: true };
                if let Err(problem) = notes::add(&self.notes_file, &note) {
                    return error(format!("{problem:#}"));
                }
                self.load_notes();
                self.rebuild(self.stream.wrap);
                // Ids are milliseconds: two notes in the same one would share an id.
                std::thread::sleep(Duration::from_millis(2));
                ok(json!({ "id": id, "in_diff": shown }))
            }
            Command::Status => {
                let focus = self.focus().map(|(file, line)| json!({ "path": self.files[file].diff.new_name, "line": line + 1 }));
                let file = self.current_file();
                ok(json!({
                    "file": self.files.get(file).map(|f| f.diff.new_name.clone()),
                    "focus": focus,
                    "files": self.files.len(),
                    "changes": self.stream.blocks.len(),
                    "open_notes": self.notes.iter().filter(|n| n.is_open()).count(),
                    "findings_to_decide": self.notes.iter().filter(|n| n.is_undecided()).count(),
                    "highlight": self.mark.as_ref().map(|m| json!({ "path": m.path, "side": if m.old_side { "old" } else { "new" }, "range": [m.range.0, m.range.1] })),
                }))
            }
            Command::Reload => error("reload is handled by the event loop"),
        }
    }

    /// Typing after `:`.
    fn apply_goto_key(&mut self, key: &KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let Some(text) = &mut self.goto else { return };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.goto = None,
            KeyCode::Char('c') if ctrl => self.goto = None,
            KeyCode::Backspace if text.is_empty() => self.goto = None,
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Enter => {
                let text = self.goto.take().unwrap_or_default();
                if let Err(problem) = self.go_to_lines(&text) {
                    self.notice = Some(problem);
                }
            }
            KeyCode::Char(c) if !ctrl => text.push(c),
            _ => {}
        }
    }

    /// `:` then `42`, `40-52`, `o42` (old side), or `PATH:42`: select those lines of the
    /// current file (or of the file whose path matches), opening their folds, as `v` would.
    fn go_to_lines(&mut self, text: &str) -> Result<(), String> {
        let (path, old_side, (first, last)) = parse_goto(text)?;
        let file = match path {
            None => self.current_file(),
            Some(path) => self.file_named(path, old_side).or_else(|exact| {
                // Words of the path in any order, as the file list matches.
                let words: Vec<String> = path.to_lowercase().split(['/', ' ']).filter(|w| !w.is_empty()).map(str::to_owned).collect();
                let found: Vec<usize> = (0..self.files.len())
                    .filter(|&f| {
                        let name = if old_side { &self.files[f].diff.old_name } else { &self.files[f].diff.new_name }.to_lowercase();
                        words.iter().all(|w| name.contains(w.as_str()))
                    })
                    .collect();
                match found.as_slice() {
                    [file] => Ok(*file),
                    [] => Err(exact),
                    _ => Err(format!("{path} matches {} files; give more of the path", found.len())),
                }
            })?,
        };
        let model = &self.files[file].diff.model;
        let count = if old_side { model.old.len() } else { model.new.len() };
        if last > count {
            return Err(format!("{} has {count} lines{}", self.files[file].diff.new_name, if old_side { " on the old side" } else { "" }));
        }
        self.select_lines(file, old_side, first, last)
    }

    /// Select lines (1-based, inclusive) of one version of a file as `v` would, opening their
    /// folds and scrolling to them.
    fn select_lines(&mut self, file: usize, old_side: bool, first: usize, last: usize) -> Result<(), String> {
        for line in (first - 1..last).take(2000) {
            self.reveal(file, old_side, line);
        }
        self.navigate(file, old_side, Some(first - 1))?;
        let (Some((pane, start)), Some((_, end))) = (self.line_row(file, old_side, first - 1), self.line_row(file, old_side, last - 1)) else {
            return Err("those lines are not shown".into());
        };
        self.visual = true;
        self.visual_anchor = (pane, start);
        self.set_visual_head(end);
        self.follow_row(pane, end);
        Ok(())
    }

    /// The moved block at the selection, or else the first one on screen.
    fn move_here(&self) -> Option<(usize, bool)> {
        let moved_at = |old_side: bool, row: usize| {
            let (is_old, file, line, _) = self.line_of(old_side, row)?;
            let state = &self.files[file];
            (if is_old { &state.old_moved } else { &state.new_moved })[line].map(|m| (m, is_old))
        };
        if let Some(selection) = self.selection {
            let (start, end) = selection.bounds();
            if let Some(found) = (start.0..=end.0).find_map(|row| moved_at(selection.old_side, row)) {
                return Some(found);
            }
        }
        let (left_top, right_top) = self.stream.scroll.tops(self.position);
        let panes: &[bool] = match self.panes {
            Panes::Both if !self.unified => &[true, false],
            Panes::Old => &[true],
            _ => &[false],
        };
        // The topmost moved line on screen, on either side.
        panes
            .iter()
            .filter_map(|&old_side| {
                let top = if old_side { left_top } else { right_top };
                self.shown_rows(old_side, top, self.body_rows.max(1)).into_iter().enumerate().find_map(|(y, row)| moved_at(old_side, row).map(|m| (y, m)))
            })
            .min_by_key(|&(y, _)| y)
            .map(|(_, m)| m)
    }

    /// `m`: on moved code, select where it went (or where it came from).
    fn jump_to_other_end(&mut self) {
        let Some((index, from_old)) = self.move_here() else {
            self.notice = Some(if self.moves.is_empty() { "no moved code in this diff" } else { "no moved code on screen (it shows in purple)" }.into());
            return;
        };
        let found = self.moves[index].clone();
        let (file, old_side, range) = if from_old { (found.new_file, false, found.new.clone()) } else { (found.old_file, true, found.old.clone()) };
        let place = |file: usize, old_side: bool, range: &std::ops::Range<usize>| {
            let diff = &self.files[file].diff;
            format!("{}:{}–{}", if old_side { &diff.old_name } else { &diff.new_name }, range.start + 1, range.end)
        };
        let message = format!("moved from {} to {}  (m: back)", place(found.old_file, true, &found.old), place(found.new_file, false, &found.new));
        match self.select_lines(file, old_side, range.start + 1, range.end) {
            Ok(()) => self.notice = Some(message),
            Err(problem) => self.notice = Some(problem),
        }
    }

    /// `a`: start a note on the selected lines, or on the line `e` would open.
    fn start_note(&mut self) {
        let place = match self.selection {
            Some(selection) => {
                let (start, end) = selection.bounds();
                let lines: Vec<(bool, usize, usize)> =
                    (start.0..=end.0).filter_map(|row| self.line_of(selection.old_side, row).map(|(o, f, l, _)| (o, f, l))).collect();
                // The note goes on the last line's file and version (unified columns mix both).
                lines.last().map(|&(is_old, file, last)| {
                    let first = lines.iter().find(|&&(o, f, _)| o == is_old && f == file).map_or(last, |&(_, _, l)| l);
                    (file, is_old, first, last)
                })
            }
            None => self.focus().map(|(file, line)| (file, false, line, line)),
        };
        let Some((file, old_side, first, last)) = place else {
            self.notice = Some("select lines, or scroll to a change, to add a note".into());
            return;
        };
        let diff = &self.files[file].diff;
        let lines = if old_side { &diff.model.old } else { &diff.model.new };
        let Some(anchor) = lines.get(last).cloned() else {
            self.notice = Some("no line here for a note".into());
            return;
        };
        let path = if old_side { diff.old_name.clone() } else { diff.new_name.clone() };
        // The screen row of the last line, so the editor does not cover it.
        let line_y = self.line_row(file, old_side, last).and_then(|(pane, row)| {
            let (left_top, right_top) = self.stream.scroll.tops(self.position);
            let top = if pane { left_top } else { right_top };
            let area = if pane { self.sides.0 } else { self.sides.1 };
            (row >= top && row < top + self.body_rows).then(|| area.y + (row - top) as u16)
        });
        self.note_draft = Some(Draft { path, old_side, range: (first + 1, last + 1), anchor, text: Default::default(), line_y });
        let _ = io::stdout().write_all(KEYS_TYPING.as_bytes());
    }

    /// Typing a note.
    /// Typing a note. Enter saves; Shift-Enter (or Alt-Enter, or Ctrl-J, for terminals that
    /// don't tell Shift-Enter apart) starts a new line; Ctrl-P saves and posts it at once in
    /// `ribbon pr`; Esc cancels.
    fn apply_draft_key(&mut self, key: &KeyEvent, columns: usize) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let Some(draft) = &mut self.note_draft else { return };
        let (ctrl, alt, shift) = (key.modifiers.contains(KeyModifiers::CONTROL), key.modifiers.contains(KeyModifiers::ALT), key.modifiers.contains(KeyModifiers::SHIFT));
        let text = &mut draft.text;
        match key.code {
            KeyCode::Esc => {}
            KeyCode::Char('c') if ctrl => {}
            KeyCode::Enter if shift || alt => return text.insert('\n'),
            KeyCode::Char('j') if ctrl => return text.insert('\n'),
            KeyCode::Enter | KeyCode::Char('p') if text.is_blank() => return,
            KeyCode::Enter if !ctrl => return self.save_draft(false),
            // Ctrl-P (or Ctrl-Enter, where the terminal sends it) posts at once.
            KeyCode::Enter | KeyCode::Char('p') if ctrl && self.pull_request.is_some() => return self.save_draft(true),
            KeyCode::Enter => return self.save_draft(false),
            KeyCode::Backspace if ctrl || alt => return text.delete_word(),
            KeyCode::Char('w') if ctrl => return text.delete_word(),
            KeyCode::Backspace => return text.backspace(),
            KeyCode::Delete => return text.delete(),
            KeyCode::Left => return text.left(),
            KeyCode::Right => return text.right(),
            KeyCode::Up => return text.vertical(columns, false),
            KeyCode::Down => return text.vertical(columns, true),
            KeyCode::Home => return text.home(columns),
            KeyCode::Char('a') if ctrl => return text.home(columns),
            KeyCode::End => return text.end(columns),
            KeyCode::Char('e') if ctrl => return text.end(columns),
            KeyCode::Tab => return text.insert_str("\t"),
            KeyCode::Char(c) if !ctrl => return text.insert(c),
            _ => return,
        }
        self.close_draft();
    }

    fn close_draft(&mut self) {
        self.note_draft = None;
        let _ = io::stdout().write_all(KEYS_TYPED.as_bytes());
    }

    /// Save the note being typed, and with `post_now` post it to the pull request too.
    fn save_draft(&mut self, post_now: bool) {
        let Some(draft) = self.note_draft.take() else { return };
        let note = Note {
            id: Some(notes::new_id()),
            path: draft.path.clone(),
            old_side: draft.old_side,
            range: draft.range,
            anchor: Some(draft.anchor.clone()),
            summary: draft.text.text().trim().to_owned(),
            rationale: None,
            author: Some("you".into()),
            status: Some("open".into()),
            reply: None,
            editable: true,
        };
        if let Err(error) = notes::add(&self.notes_file, &note) {
            // Keep the text, so nothing typed is lost.
            self.note_draft = Some(draft);
            self.notice = Some(format!("{error:#}"));
            return;
        }
        self.close_draft();
        self.notice = Some(format!("note saved in {}", self.notes_file.display()));
        self.selection = None;
        self.load_notes();
        self.rebuild(self.stream.wrap);
        self.picked_note = self.notes.iter().position(|n| n.id == note.id);
        if let Some(index) = self.picked_note.filter(|_| post_now) {
            self.post_now(index);
        }
    }

    fn apply_note_list(&mut self, key: &KeyEvent, selected: usize) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let last = self.label_start.saturating_sub(1);
        self.note_list = match key.code {
            KeyCode::Esc | KeyCode::Char('A') | KeyCode::Char('q') => None,
            KeyCode::Enter => {
                self.go_to_note(selected);
                None
            }
            KeyCode::Char('x') => {
                self.delete_note(selected);
                (self.label_start > 0).then(|| selected.min(self.label_start - 1))
            }
            KeyCode::Char(key @ ('F' | 'D')) => {
                self.decide(selected, if key == 'F' { "open" } else { "dismissed" }, false);
                Some(selected)
            }
            KeyCode::Char('j') | KeyCode::Down => Some((selected + 1).min(last)),
            KeyCode::Char('k') | KeyCode::Up => Some(selected.saturating_sub(1)),
            _ => Some(selected),
        };
    }

    /// A key from the config file's [keys] table acts as the key it names.
    fn remap(&self, event: &Event) -> Event {
        let Event::Key(key) = event else { return event.clone() };
        // Shift is part of the character itself ("K"), so it is not compared.
        let modifiers = key.modifiers - KeyModifiers::SHIFT;
        match self.keys.iter().find(|((code, mods), _)| *code == key.code && *mods == modifiers) {
            Some(&(_, (code, mods))) => Event::Key(KeyEvent { code, modifiers: mods, ..*key }),
            None => event.clone(),
        }
    }

    /// `v`: select whole lines with the keyboard, from the line `e` would open (or the current
    /// selection), on the side alone or the new side.
    fn start_visual(&mut self) {
        let old_side = self.panes == Panes::Old;
        let row = match self.selection {
            Some(selection) => Some((selection.old_side, selection.bounds().0 .0)),
            None => {
                let (left_top, right_top) = self.stream.scroll.tops(self.position);
                let top = if old_side { left_top } else { right_top };
                let shown = self.shown_rows(old_side, top, self.body_rows.max(1));
                let changed = shown.iter().copied().find(|&row| {
                    self.line_of(old_side, row).is_some_and(|(is_old, file, line, _)| {
                        let state = &self.files[file];
                        (if is_old { &state.old_kinds } else { &state.new_kinds })[line].is_some()
                    })
                });
                changed.or_else(|| shown.iter().copied().find(|&row| self.line_of(old_side, row).is_some())).map(|row| (old_side, row))
            }
        };
        let Some((old_side, row)) = row else {
            self.notice = Some("no line on screen to select".into());
            return;
        };
        self.visual = true;
        self.visual_anchor = (old_side, row);
        self.set_visual_head(row);
    }

    /// Select whole lines from the anchor to `head`, which may be above it.
    fn set_visual_head(&mut self, head: usize) {
        let (old_side, anchor) = self.visual_anchor;
        self.selection = Some(if head >= anchor {
            Selection { old_side, anchor: (anchor, 0), head: (head, LINE_END) }
        } else {
            Selection { old_side, anchor: (anchor, LINE_END), head: (head, 0) }
        });
    }

    /// The first row of the next or previous line on a side (wrapped rows and notes are
    /// skipped).
    fn next_line_row(&self, old_side: bool, row: usize, forward: bool) -> Option<usize> {
        let starts_line = |row: usize| self.line_of(old_side, row).is_some_and(|(_, _, _, part)| part == 0);
        if forward { (row + 1..self.rows(old_side).len()).find(|&r| starts_line(r)) } else { (0..row).rev().find(|&r| starts_line(r)) }
    }

    /// Scroll so a row of the selection has `scroll_off` rows around it.
    fn follow_row(&mut self, old_side: bool, row: usize) {
        let (left_top, right_top) = self.stream.scroll.tops(self.position);
        let top = if old_side { left_top } else { right_top };
        let off = self.scroll_off.min(self.body_rows.saturating_sub(1) / 2);
        if row < top + off {
            self.position = self.stream.scroll.position_of_row(row.saturating_sub(off), old_side);
        } else if row + off + 1 > top + self.body_rows {
            self.position = self.stream.scroll.position_of_row((row + off + 1).saturating_sub(self.body_rows), old_side);
        }
    }

    /// Keys of a keyboard selection: j/k (and arrows) move its end by one line, o swaps its
    /// two ends, J/K move the whole selection by one line. The view follows the end.
    /// Returns false for keys that are not about the selection.
    fn apply_visual_key(&mut self, key: &KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return true;
        }
        let (old_side, anchor) = self.visual_anchor;
        let head = self.selection.map_or(anchor, |s| s.head.0);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Char('k') | KeyCode::Up => {
                let forward = matches!(key.code, KeyCode::Char('j') | KeyCode::Down);
                let Some(next) = self.next_line_row(old_side, head, forward) else { return true };
                self.set_visual_head(next);
                self.follow_row(old_side, next);
            }
            KeyCode::Char('o') => {
                self.visual_anchor = (old_side, head);
                self.set_visual_head(anchor);
                self.follow_row(old_side, anchor);
            }
            KeyCode::Char('J') | KeyCode::Char('K') => {
                let forward = key.code == KeyCode::Char('J');
                let moved = self.next_line_row(old_side, anchor, forward).zip(self.next_line_row(old_side, head, forward));
                let Some((anchor, head)) = moved else { return true };
                self.visual_anchor = (old_side, anchor);
                self.set_visual_head(head);
                self.follow_row(old_side, head);
            }
            _ => return false,
        }
        true
    }

    /// Switch between the split and the unified layout, on the same line.
    fn set_unified(&mut self, unified: bool) {
        if unified == self.unified {
            return;
        }
        self.unified = unified;
        if unified {
            self.split_panes = self.panes;
            // The column is drawn where the new side goes, at full width.
            self.panes = Panes::New;
        } else {
            self.panes = self.split_panes;
        }
        self.visual = false;
        self.rebuild(self.stream.wrap);
    }

    fn numbers_right(&self, old_side: bool) -> bool {
        old_side && self.panes == Panes::Both
    }

    /// The file whose title is pinned over the first row, if any.
    fn sticky_file(&self) -> Option<usize> {
        let (left_top, right_top) = self.stream.scroll.tops(self.position);
        match self.panes {
            Panes::New => self.stream.sticky_file(right_top, false),
            _ => self.stream.sticky_file(left_top, true),
        }
    }

    /// The stream row of a file's title on one side.
    fn header_row(&self, file: usize, old_side: bool) -> usize {
        let rows = if old_side { &self.stream.left } else { &self.stream.right };
        rows.iter().position(|r| *r == Row::Header(file)).unwrap_or(0)
    }

    fn is_selected(&self, old_side: bool, row: usize) -> bool {
        self.selection.is_some_and(|s| s.old_side == old_side && s.columns(row).is_some())
    }

    /// Stream rows drawn on one side, from `top`. A side alone drops the blank rows that keep
    /// it level with the other side.
    fn shown_rows(&self, old_side: bool, top: usize, count: usize) -> Vec<usize> {
        if self.panes == Panes::Both {
            return (top..top + count).collect();
        }
        let rows = if old_side { &self.stream.left } else { &self.stream.right };
        (top..rows.len()).filter(|&r| rows[r] != Row::Gap).take(count).collect()
    }

    /// The scroll position `delta` rows away. With one side alone, rows of that side: inside a
    /// change, the shared position moves by the longer side, which can leave the shown side still.
    fn scrolled(&self, position: usize, delta: isize) -> usize {
        let old_side = match self.panes {
            Panes::Both => return position.saturating_add_signed(delta),
            Panes::Old => true,
            Panes::New => false,
        };
        let (left_top, right_top) = self.stream.scroll.tops(position);
        let (top, rows) = if old_side { (left_top, self.stream.left.len()) } else { (right_top, self.stream.right.len()) };
        // Count only the rows drawn: blank rows are skipped.
        let side = if old_side { &self.stream.left } else { &self.stream.right };
        let mut row = top;
        let mut left = delta.unsigned_abs();
        while left > 0 {
            let next = if delta > 0 { row + 1 } else if row > 0 { row - 1 } else { break };
            if next >= rows {
                break;
            }
            row = next;
            if side[row] != Row::Gap {
                left -= 1;
            }
        }
        if row == top {
            return position;
        }
        let target = self.stream.scroll.position_of_row(row, old_side);
        // Keep moving when rounding lands on the same position.
        if target == position { position.saturating_add_signed(delta.signum()) } else { target }
    }

    fn current_file(&self) -> usize {
        self.stream.file_at(self.position)
    }

    /// One-based index of the change at the top of the view. Changes too close to the start
    /// to scroll to count as current from the start.
    fn current_change(&self) -> usize {
        // n/N pick a change even when the view can't scroll to it (all of it fits on screen).
        if let Some(index) = self.picked_change.filter(|&i| self.change_on_screen(i)) {
            return index + 1;
        }
        let passed = self.stream.change_starts(self.scroll_off).iter().filter(|&&s| s <= self.position).count();
        if self.stream.blocks.is_empty() { 0 } else { passed.max(1) }
    }

    fn change_on_screen(&self, index: usize) -> bool {
        let Some(block) = self.stream.blocks.get(index) else { return false };
        let (left_top, right_top) = self.stream.scroll.tops(self.position);
        let rows = self.body_rows;
        (block.a0 < left_top + rows && block.a1.max(block.a0 + 1) > left_top) || (block.b0 < right_top + rows && block.b1.max(block.b0 + 1) > right_top)
    }

    fn toggle_whole_file(&mut self) {
        let change = self.current_change();
        let file = self.current_file();
        self.whole_file = !self.whole_file;
        self.selection = None;
        // Changes-only view starts again with every fold closed.
        self.opened.clear();
        let had_search = self.search.is_some();
        self.stream = self.build_stream(self.stream.wrap);
        self.generation += 1;
        let starts = self.stream.change_starts(self.scroll_off);
        // Stay on the same change, or on the same file when it has none above the view.
        self.position = match starts.get(change.saturating_sub(1)) {
            Some(&start) if self.stream.file_at(start) == file => start,
            _ => self.stream.header_position(file),
        };
        // Folded lines are not searched, so the matches change with the folds.
        if had_search {
            self.update_matches();
            self.current_from(self.position);
        }
    }

    /// Apply one input event. Returns how to quit, if the event quits.
    fn apply(&mut self, event: &Event, page: usize, max: usize) -> Option<Outcome> {
        if let Some(selected) = self.picker {
            if let Event::Key(key) = event {
                self.apply_picker(key, selected);
            }
            return None;
        }
        if let (Some(_), Event::Key(key)) = (&self.review, event) {
            self.apply_review_key(key);
            return None;
        }
        if let Event::Key(key) = event {
            if key.kind != KeyEventKind::Release {
                self.notice = None;
            }
            if self.copy_menu {
                self.apply_copy_menu(key);
                return None;
            }
        }
        if let (Some((group, selected)), Event::Key(key)) = (self.repeat_list, event) {
            self.apply_repeat_list(key, group, selected);
            return None;
        }
        if let (Some(selected), Event::Key(key)) = (self.note_list, event) {
            self.apply_note_list(key, selected);
            return None;
        }
        if let Some(draft) = &mut self.note_draft {
            match event {
                Event::Key(key) => {
                    let columns = self.editor_columns;
                    self.apply_draft_key(key, columns);
                }
                Event::Paste(text) => draft.text.insert_str(text),
                _ => {}
            }
            return None;
        }
        if self.search.as_ref().is_some_and(|s| s.editing) {
            if let Event::Key(key) = event {
                self.apply_search_key(key);
            }
            return None;
        }
        if self.goto.is_some() {
            if let Event::Key(key) = event {
                self.apply_goto_key(key);
            }
            return None;
        }
        let remapped = self.remap(event);
        let event = &remapped;
        if let Event::Mouse(mouse) = event {
            if self.select(mouse) {
                // Lines picked on the line numbers act like `v`: j/k, o, and J/K go on from them.
                self.visual = self.gutter && self.selection.is_some();
                return None;
            }
        }
        if self.visual {
            if let Event::Key(key) = event {
                if self.apply_visual_key(key) {
                    return None;
                }
            }
        }
        if let Some(columns) = self.horizontal(event) {
            self.hscroll = columns.min(self.max_width.saturating_sub(self.code_width / 2));
            return None;
        }
        let file = self.current_file();
        let files = self.files.len();
        let starts = self.stream.change_starts(self.scroll_off);
        let position = self.position;
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                self.position = match key.code {
                    KeyCode::Char('/') => {
                        self.search = Some(Search {
                            query: String::new(),
                            editing: true,
                            matches: Vec::new(),
                            current: 0,
                            origin: (position, self.hscroll),
                        });
                        position
                    }
                    KeyCode::Esc if self.search.is_some() => {
                        self.search = None;
                        position
                    }
                    KeyCode::Char('n') if self.search.is_some() => {
                        self.step_match(true);
                        self.position
                    }
                    KeyCode::Char('N') | KeyCode::Char('p') if self.search.is_some() => {
                        self.step_match(false);
                        self.position
                    }
                    KeyCode::Esc if self.selection.is_some() => {
                        self.selection = None;
                        position
                    }
                    KeyCode::Char('y') if self.selection.is_some() => {
                        self.copy_selection();
                        position
                    }
                    KeyCode::Char('y') => {
                        self.copy_menu = true;
                        position
                    }
                    KeyCode::Char('Y') => {
                        match self.change_text() {
                            Some((text, what)) => self.copy_text(&text, what),
                            None => self.notice = Some("no change to copy".into()),
                        }
                        position
                    }
                    KeyCode::Char('e') => {
                        self.request_edit();
                        position
                    }
                    KeyCode::Char('a') => {
                        self.start_note();
                        self.visual = false;
                        position
                    }
                    KeyCode::Char('v') => {
                        self.start_visual();
                        self.position
                    }
                    KeyCode::Char('r') => {
                        self.open_repeat_list();
                        self.position
                    }
                    KeyCode::Char('m') => {
                        self.jump_to_other_end();
                        self.position
                    }
                    KeyCode::Char(':') => {
                        self.goto = Some(String::new());
                        self.position
                    }
                    KeyCode::Char('}') => {
                        self.step_note(true);
                        self.position
                    }
                    KeyCode::Char('{') => {
                        self.step_note(false);
                        self.position
                    }
                    KeyCode::Char('x') => {
                        match self.picked_note.filter(|&n| self.note_on_screen(n)) {
                            Some(note) => self.delete_note(note),
                            None => self.notice = Some("go to a note with } or { first".into()),
                        }
                        self.position
                    }
                    KeyCode::Char('P') => {
                        self.ask_to_post();
                        self.position
                    }
                    KeyCode::Char(key @ ('F' | 'D')) => {
                        match self.picked_note.filter(|&n| self.notes[n].is_undecided() && self.note_on_screen(n)) {
                            Some(note) => self.decide(note, if key == 'F' { "open" } else { "dismissed" }, true),
                            None => self.next_finding(),
                        }
                        self.position
                    }
                    KeyCode::Char('A') => {
                        if self.label_start == 0 {
                            self.notice = Some("no notes (a adds one)".into());
                        } else {
                            self.note_list = Some(self.picked_note.unwrap_or(0));
                        }
                        position
                    }
                    KeyCode::Char('z') => {
                        self.open_fold_on_screen();
                        self.position
                    }
                    KeyCode::Char('Z') => {
                        if self.opened.is_empty() {
                            self.notice = Some("no opened folds".into());
                        } else {
                            self.opened.clear();
                            self.rebuild(self.stream.wrap);
                        }
                        self.position
                    }
                    KeyCode::Char('w') => {
                        // The event loop builds the rows again at the new width.
                        self.wrap = !self.wrap;
                        self.hscroll = 0;
                        position
                    }
                    KeyCode::Char('q') | KeyCode::Esc => return Some(Outcome::Quit),
                    KeyCode::Char('c') if ctrl => return Some(Outcome::QuitAll),
                    KeyCode::Char('Q') => return Some(Outcome::QuitAll),
                    KeyCode::Char('j') | KeyCode::Down | KeyCode::Enter => self.scrolled(position, 1),
                    KeyCode::Char('k') | KeyCode::Up => self.scrolled(position, -1),
                    KeyCode::Char('f') if ctrl => self.scrolled(position, page as isize),
                    KeyCode::Char('b') if ctrl => self.scrolled(position, -(page as isize)),
                    KeyCode::Char('d') => self.scrolled(position, page as isize / 2),
                    KeyCode::Char('u') => self.scrolled(position, -(page as isize / 2)),
                    KeyCode::Char(' ') if key.modifiers.contains(KeyModifiers::SHIFT) => self.scrolled(position, -(page as isize)),
                    KeyCode::Char(' ') | KeyCode::PageDown => self.scrolled(position, page as isize),
                    KeyCode::PageUp => self.scrolled(position, -(page as isize)),
                    // Show one side alone; the same key again shows both.
                    KeyCode::Char('<' | '>') if self.unified => {
                        self.notice = Some("the unified layout shows both versions; s splits it".into());
                        position
                    }
                    KeyCode::Char('s') => {
                        // The event loop switches the layout.
                        self.layout_mode = if self.unified { LayoutMode::Split } else { LayoutMode::Unified };
                        position
                    }
                    KeyCode::Char('<') => {
                        self.panes = if self.panes == Panes::Old { Panes::Both } else { Panes::Old };
                        self.selection = None;
                        position
                    }
                    KeyCode::Char('>') => {
                        self.panes = if self.panes == Panes::New { Panes::Both } else { Panes::New };
                        self.selection = None;
                        position
                    }
                    KeyCode::Char('g') | KeyCode::Home => 0,
                    KeyCode::Char('G') | KeyCode::End => max,
                    // Next or previous change from the one picked last, while it is on screen
                    // (n/N pick a change even when the view can't scroll to it); otherwise from
                    // the scroll position.
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('p') => {
                        let picked = self.picked_change.filter(|&i| self.change_on_screen(i));
                        let forward = key.code == KeyCode::Char('n');
                        let mut index = if forward {
                            picked.map_or_else(|| starts.iter().position(|&s| s > position), |i| Some(i + 1)).filter(|&i| i < starts.len())
                        } else {
                            picked.map_or_else(|| starts.iter().rposition(|&s| s < position), |i| i.checked_sub(1))
                        };
                        // Repeats of an edit already read are skipped.
                        let mut skipped = 0;
                        while let Some(i) = index.filter(|&i| self.is_seen_repeat(i)) {
                            skipped += 1;
                            index = if forward { Some(i + 1).filter(|&i| i < starts.len()) } else { i.checked_sub(1) };
                        }
                        if skipped > 0 {
                            self.notice = Some(format!("skipped {skipped} repeat{} of an edit already shown (r lists them)", if skipped == 1 { "" } else { "s" }));
                        }
                        match index {
                            Some(index) => {
                                self.picked_change = Some(index);
                                starts[index]
                            }
                            None => position,
                        }
                    }
                    KeyCode::Char(']') if file + 1 < files => self.stream.header_position(file + 1),
                    // [ goes to the top of this file first, then to the previous file.
                    KeyCode::Char('[') => {
                        let here = self.stream.header_position(file);
                        if position > here || file == 0 { here } else { self.stream.header_position(file - 1) }
                    }
                    KeyCode::Char('c') => {
                        self.toggle_whole_file();
                        self.position
                    }
                    KeyCode::Char('f') | KeyCode::Tab => {
                        // Open on the current file, unfiltered.
                        self.picker_filter.clear();
                        self.picker = Some(file);
                        position
                    }
                    _ => position,
                };
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollDown => self.position = self.scrolled(position, 3),
                MouseEventKind::ScrollUp => self.position = self.scrolled(position, -3),
                _ => {}
            },
            _ => {}
        }
        if self.selection.is_none() {
            self.visual = false;
        }
        None
    }

    /// The text of the line that starts at a row (not the later rows of a wrapped line).
    fn line_at(&self, old_side: bool, row: usize) -> Option<&str> {
        match self.line_of(old_side, row)? {
            (_, _, _, 0) => self.text_of(old_side, row),
            _ => None,
        }
    }

    /// Find the query in every shown line of both sides, in scroll order.
    fn update_matches(&mut self) {
        let Some(query) = self.search.as_ref().map(|s| s.query.clone()) else { return };
        let mut matches = Vec::new();
        // The unified column is the same on both sides: search it once.
        let sides: &[bool] = if self.stream.unified { &[false] } else { &[true, false] };
        for &old_side in sides {
            let rows = if old_side { self.stream.left.len() } else { self.stream.right.len() };
            for row in 0..rows {
                if let Some(line) = self.line_at(old_side, row) {
                    matches.extend(search::find_all(line, &query).into_iter().map(|bytes| Match { old_side, row, bytes }));
                }
            }
        }
        let scroll = &self.stream.scroll;
        matches.sort_by_key(|m| (scroll.position_of_row(m.row, m.old_side), !m.old_side, m.row, m.bytes.start));
        if let Some(found) = &mut self.search {
            found.matches = matches;
            found.current = 0;
        }
    }

    /// Make the first match at or after a scroll position the current one.
    fn current_from(&mut self, position: usize) {
        let scroll = &self.stream.scroll;
        if let Some(found) = &mut self.search {
            found.current = found.matches.iter().position(|m| scroll.position_of_row(m.row, m.old_side) >= position).unwrap_or(0);
        }
    }

    /// Scroll so the current match is on screen, when it is not already.
    fn show_match(&mut self) {
        let Some(m) = self.search.as_ref().and_then(|s| s.matches.get(s.current)).cloned() else { return };
        let (left_top, right_top) = self.stream.scroll.tops(self.position);
        let top = if m.old_side { left_top } else { right_top };
        let Some(line) = self.line_at(m.old_side, m.row) else { return };
        let (from, to) = (select::line_width(&line[..m.bytes.start]), select::line_width(&line[..m.bytes.end]));
        // A wrapped line shows the match on a later row.
        let row = m.row + self.stream.wrap.map_or(0, |width| from / width.max(1));
        // Keep `scroll_off` rows around the match (the first row can be under the sticky title).
        let off = self.scroll_off.min(self.body_rows.saturating_sub(1) / 2);
        if row <= top + off || row + off >= top + self.body_rows {
            self.position = self.stream.scroll.position_of_row(row, m.old_side).saturating_sub(off);
        }
        if self.wrap {
            return;
        }
        // Prefer the line start; scroll sideways only for a match past the right edge.
        if to <= self.code_width {
            self.hscroll = 0;
        } else if from < self.hscroll || to > self.hscroll + self.code_width {
            self.hscroll = from.saturating_sub(self.code_width / 4);
        }
    }

    fn step_match(&mut self, forward: bool) {
        let Some(found) = &mut self.search else { return };
        let count = found.matches.len();
        if count == 0 {
            return;
        }
        found.current = if forward { (found.current + 1) % count } else { (found.current + count - 1) % count };
        self.show_match();
    }

    /// Typing the query: the view follows the first match after where the search started.
    fn apply_search_key(&mut self, key: &KeyEvent) {
        let Some(found) = &mut self.search else { return };
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let origin = found.origin;
        let cancel = match key.code {
            KeyCode::Esc => true,
            KeyCode::Char('c') if ctrl => true,
            KeyCode::Backspace if found.query.is_empty() => true,
            KeyCode::Enter => {
                found.editing = false;
                if found.query.is_empty() {
                    self.search = None;
                } else if found.matches.is_empty() {
                    let hint = if self.whole_file { "" } else { " (folded lines are not searched; c shows the whole file)" };
                    self.notice = Some(format!("no match for /{}{hint}", found.query));
                    self.search = None;
                }
                return;
            }
            KeyCode::Backspace => {
                found.query.pop();
                false
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                found.query.push(c);
                false
            }
            _ => return,
        };
        (self.position, self.hscroll) = origin;
        if cancel {
            self.search = None;
            return;
        }
        self.update_matches();
        self.current_from(origin.0);
        self.show_match();
    }

    /// The (stream row, text column) under a screen cell on one side. Cells past an edge count
    /// as the nearest cell inside the side.
    fn cell_at(&self, old_side: bool, x: u16, y: u16) -> (usize, usize) {
        let area = if old_side { self.sides.0 } else { self.sides.1 };
        let (code_x, code_width) = code_area(area, self.number_width, self.numbers_right(old_side));
        let (left_top, right_top) = self.stream.scroll.tops(self.position);
        let top = if old_side { left_top } else { right_top };
        let row = y.saturating_sub(area.y).min(area.height.saturating_sub(1)) as usize;
        let column = x.saturating_sub(code_x).min(code_width.saturating_sub(1)) as usize;
        // The pinned title stands for the file's own title row.
        if let (0, Some(file)) = (row, self.sticky_file()) {
            return (self.header_row(file, old_side), column + self.hscroll);
        }
        let shown = self.shown_rows(old_side, top, row + 1);
        let at = shown.get(row).or(shown.last()).copied().unwrap_or(top);
        // On a wrapped line, the column counts from the line start.
        let skip = match self.stream.wrap {
            Some(width) => self.line_of(old_side, at).map_or(0, |(_, _, _, part)| part * width),
            None => self.hscroll,
        };
        (at, column + skip)
    }

    /// True when column `x` of a side is on its line numbers, not its code.
    fn on_line_number(&self, old_side: bool, x: u16) -> bool {
        let area = if old_side { self.sides.0 } else { self.sides.1 };
        let (code_x, code_width) = code_area(area, self.number_width, self.numbers_right(old_side));
        x < code_x || x >= code_x + code_width
    }

    /// Start, extend, or finish a selection with the left button. Returns true when the event
    /// was used.
    fn select(&mut self, mouse: &MouseEvent) -> bool {
        let (x, y) = (mouse.column, mouse.row);
        let inside = |r: Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let side = if inside(self.sides.0) { Some(true) } else if inside(self.sides.1) { Some(false) } else { None };
                // A click on a line number picks that whole line; Shift extends the lines picked
                // before, and a drag on the numbers picks more.
                if let Some(old_side) = side.filter(|&old_side| self.on_line_number(old_side, x)) {
                    let (row, _) = self.cell_at(old_side, x, y);
                    if self.line_of(old_side, row).is_some() {
                        let extend = mouse.modifiers.contains(KeyModifiers::SHIFT) && self.gutter && self.selection.is_some_and(|s| s.old_side == old_side);
                        if !extend {
                            self.visual_anchor = (old_side, row);
                        }
                        self.set_visual_head(row);
                        self.gutter = true;
                        self.dragging = true;
                        return true;
                    }
                }
                self.gutter = false;
                self.selection = side.map(|old_side| {
                    let cell = self.cell_at(old_side, x, y);
                    Selection { old_side, anchor: cell, head: cell }
                });
                self.dragging = self.selection.is_some();
                true
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                let Some(selection) = self.selection else { return true };
                // Dragging onto the top or bottom row scrolls, to select more than one screen.
                let area = if selection.old_side { self.sides.0 } else { self.sides.1 };
                if y <= area.y {
                    self.position = self.position.saturating_sub(1);
                } else if y + 1 >= area.y + area.height {
                    self.position += 1;
                }
                let head = self.cell_at(selection.old_side, x, y);
                if self.gutter {
                    if self.line_of(selection.old_side, head.0).is_some() {
                        self.set_visual_head(head.0);
                    }
                } else {
                    self.selection = Some(Selection { head, ..selection });
                }
                true
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging => {
                self.dragging = false;
                if self.gutter {
                    return true;
                }
                match self.selection {
                    // A click on a fold opens it.
                    Some(s) if s.anchor == s.head && matches!(self.rows(s.old_side).get(s.anchor.0), Some(Row::Fold(..))) => {
                        self.selection = None;
                        if let Some(&Row::Fold(file, _, start)) = self.rows(s.old_side).get(s.anchor.0) {
                            self.open_fold(file, start);
                        }
                    }
                    // A click without a drag only clears the old selection.
                    Some(s) if s.anchor == s.head => self.selection = None,
                    Some(_) => self.copy_selection(),
                    None => {}
                }
                true
            }
            _ => false,
        }
    }

    /// The selected source text, one line per selected source row, and the line count.
    /// A file title gives its whole path; folds and blank rows are skipped.
    fn selected_text(&self, selection: &Selection) -> (String, usize) {
        let rows = if selection.old_side { &self.stream.left } else { &self.stream.right };
        let (start, end) = selection.bounds();
        let mut lines = Vec::new();
        let mut last_line: Option<(bool, usize, usize)> = None;
        for row in start.0..=end.0 {
            match rows.get(row) {
                // A wrapped line counts once.
                Some(&(Row::Line(..) | Row::Wrap(..) | Row::Old(..))) if last_line != self.line_of(selection.old_side, row) .map(|(o, f, l, _)| (o, f, l)) => {
                    last_line = self.line_of(selection.old_side, row).map(|(o, f, l, _)| (o, f, l));
                    let line = self.text_of(selection.old_side, row).unwrap_or("");
                    let (from, to) = self.selected_columns(selection.old_side, row).unwrap_or((0, 0));
                    lines.push(select::slice_columns(line, from, to));
                }
                Some(&Row::Header(file)) => {
                    let diff = &self.files[file].diff;
                    lines.push(if selection.old_side { &diff.old_name } else { &diff.new_name });
                }
                _ => {}
            }
        }
        (lines.join("\n"), lines.len())
    }

    fn copy_selection(&mut self) {
        let Some(selection) = self.selection else { return };
        let (text, count) = self.selected_text(&selection);
        if count == 0 {
            return;
        }
        let noun = if count == 1 { "line" } else { "lines" };
        self.notice = Some(match select::copy(&text) {
            Ok(()) => format!("copied {count} {noun} ({} characters)", text.chars().count()),
            Err(error) => format!("copy failed: {error}"),
        });
    }

    /// The new horizontal scroll for a sideways key or wheel event, if it is one.
    fn horizontal(&self, event: &Event) -> Option<usize> {
        if self.wrap {
            return None;
        }
        let (step, half) = (8, (self.code_width / 2).max(1));
        let (now, back) = (self.hscroll, |by: usize| self.hscroll.saturating_sub(by));
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Char('l') | KeyCode::Right => Some(now + step),
                KeyCode::Char('h') | KeyCode::Left => Some(back(step)),
                KeyCode::Char('L') => Some(now + half),
                KeyCode::Char('H') => Some(back(half)),
                KeyCode::Char('0') => Some(0),
                _ => None,
            },
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollRight => Some(now + step),
                MouseEventKind::ScrollLeft => Some(back(step)),
                MouseEventKind::ScrollDown if mouse.modifiers.contains(KeyModifiers::SHIFT) => Some(now + step),
                MouseEventKind::ScrollUp if mouse.modifiers.contains(KeyModifiers::SHIFT) => Some(back(step)),
                _ => None,
            },
            _ => None,
        }
    }

    /// The files the list shows: those whose path has every word of the filter, ignoring case.
    fn picker_files(&self) -> Vec<usize> {
        let words: Vec<String> = self.picker_filter.to_lowercase().split_whitespace().map(str::to_owned).collect();
        (0..self.files.len())
            .filter(|&i| {
                let path = self.files[i].diff.new_name.to_lowercase();
                words.iter().all(|w| path.contains(w.as_str()))
            })
            .collect()
    }

    /// The file list: typing filters it; arrows (or Ctrl-N/P, Ctrl-J/K) move.
    fn apply_picker(&mut self, key: &KeyEvent, selected: usize) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let count = self.picker_files().len();
        let last = count.saturating_sub(1);
        self.picker = match key.code {
            KeyCode::Esc if !self.picker_filter.is_empty() => {
                self.picker_filter.clear();
                Some(0)
            }
            KeyCode::Esc | KeyCode::Tab => None,
            KeyCode::Char('c') if ctrl => None,
            KeyCode::Enter => {
                if let Some(&file) = self.picker_files().get(selected) {
                    self.position = self.stream.header_position(file);
                }
                None
            }
            KeyCode::Down => Some((selected + 1).min(last)),
            KeyCode::Char('n' | 'j') if ctrl => Some((selected + 1).min(last)),
            KeyCode::Up => Some(selected.saturating_sub(1)),
            KeyCode::Char('p' | 'k') if ctrl => Some(selected.saturating_sub(1)),
            KeyCode::Home => Some(0),
            KeyCode::End => Some(last),
            KeyCode::Backspace => {
                self.picker_filter.pop();
                Some(0)
            }
            KeyCode::Char(c) if !ctrl => {
                self.picker_filter.push(c);
                Some(0)
            }
            _ => Some(selected),
        };
    }
}

pub fn restore_terminal(graphics: Graphics) {
    let mut out = io::stdout();
    // Only Kitty-capable terminals get graphics commands; others may print them as text.
    if graphics == Graphics::Kitty {
        let _ = out.write_all(ribbons::kitty_delete().as_bytes());
    }
    let _ = out.write_all(MOUSE_OFF.as_bytes());
    let _ = out.write_all(KEYS_OFF.as_bytes());
    let _ = out.write_all(b"\x1b[?2004l");
    let _ = execute!(out, terminal::LeaveAlternateScreen, cursor::Show);
    let _ = terminal::disable_raw_mode();
}

/// Open `path` at zero-based `line` in `$VISUAL` or `$EDITOR` (or vi), with the terminal back
/// in its normal state until the editor exits.
fn edit(path: &std::path::Path, line: usize, graphics: Graphics) -> io::Result<()> {
    let editor = std::env::var("VISUAL").ok().filter(|e| !e.trim().is_empty()).or_else(|| std::env::var("EDITOR").ok()).filter(|e| !e.trim().is_empty());
    let editor = editor.unwrap_or_else(|| "vi".into());
    let mut words = editor.split_whitespace();
    let program = words.next().unwrap_or("vi");
    let name = std::path::Path::new(program).file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned());
    let at = format!("{}:{}", path.display(), line + 1);
    let mut command = std::process::Command::new(program);
    command.args(words);
    // Editors that take `file:line`; the rest take `+line file`, like vi, emacs, and nano.
    match name.as_str() {
        "code" | "code-insiders" | "cursor" | "windsurf" | "codium" => command.args(["-g", &at]),
        "zed" | "zeditor" | "subl" | "hx" | "helix" | "mate" => command.arg(&at),
        _ => command.arg(format!("+{}", line + 1)).arg(path),
    };
    restore_terminal(graphics);
    let status = command.status();
    terminal::enable_raw_mode()?;
    execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
    io::stdout().write_all(MOUSE_ON.as_bytes())?;
    io::stdout().write_all(KEYS_ON.as_bytes())?;
    io::stdout().flush()?;
    status.map(|_| ())
}

/// A list to pick one line from, on its own screen before the viewer opens: typing filters
/// it (words in any order), arrows or Ctrl-N/P move, Enter picks, Esc quits.
pub fn pick(title: &str, lines: &[String]) -> anyhow::Result<Option<usize>> {
    terminal::enable_raw_mode()?;
    execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
    let result = pick_loop(title, lines);
    let _ = execute!(io::stdout(), terminal::LeaveAlternateScreen, cursor::Show);
    let _ = terminal::disable_raw_mode();
    result
}

fn pick_loop(title: &str, lines: &[String]) -> anyhow::Result<Option<usize>> {
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let (mut filter, mut selected) = (String::new(), 0usize);
    loop {
        let words: Vec<String> = filter.to_lowercase().split_whitespace().map(str::to_owned).collect();
        let shown: Vec<usize> = (0..lines.len()).filter(|&i| words.iter().all(|w| lines[i].to_lowercase().contains(w.as_str()))).collect();
        selected = selected.min(shown.len().saturating_sub(1));
        term.draw(|frame| {
            let area = frame.area();
            let buf = frame.buffer_mut();
            let heading = if filter.is_empty() {
                format!(" {title}  (type to filter, ↑/↓ move, Enter open, Esc quit)")
            } else {
                format!(" {}/{} · filter: {filter}█", shown.len(), lines.len())
            };
            buf.set_line(0, 0, &Line::styled(heading, Style::default().add_modifier(Modifier::BOLD)), area.width);
            let rows = area.height.saturating_sub(1) as usize;
            let first = selected.saturating_sub(rows.saturating_sub(1));
            for (row, (place, &index)) in shown.iter().enumerate().skip(first).take(rows).enumerate() {
                let style = if place == selected { Style::default().add_modifier(Modifier::REVERSED) } else { Style::default() };
                let row_area = Rect::new(0, 1 + row as u16, area.width, 1);
                clear(buf, row_area, style);
                buf.set_line(1, row_area.y, &Line::styled(lines[index].clone(), style), area.width.saturating_sub(1));
            }
        })?;
        let Event::Key(key) = event::read()? else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Char('c') if ctrl => return Ok(None),
            KeyCode::Enter => return Ok(shown.get(selected).copied()),
            KeyCode::Down => selected += 1,
            KeyCode::Char('n' | 'j') if ctrl => selected += 1,
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Char('p' | 'k') if ctrl => selected = selected.saturating_sub(1),
            KeyCode::Backspace => {
                filter.pop();
                selected = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                filter.push(c);
                selected = 0;
            }
            _ => {}
        }
    }
}

pub fn run(diffs: Vec<FileDiff>, settings: Settings) -> anyhow::Result<Outcome> {
    terminal::enable_raw_mode()?;
    execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
    io::stdout().write_all(MOUSE_ON.as_bytes())?;

    // Ask before the event loop starts reading the terminal, so the replies are not taken as keys.
    let terminal_colors = termcolors::query();
    let kitty = terminal_colors.kitty_graphics || settings.kitty_hint;
    let graphics = settings.graphics.unwrap_or(if kitty { Graphics::Kitty } else { Graphics::Blocks });
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal(graphics);
        previous_hook(info);
    }));
    io::stdout().write_all(KEYS_ON.as_bytes())?;
    io::stdout().flush()?;
    let result = event_loop(diffs, settings, graphics, terminal_colors);
    restore_terminal(graphics);
    result
}

fn event_loop(diffs: Vec<FileDiff>, settings: Settings, graphics: Graphics, terminal_colors: TermColors) -> anyhow::Result<Outcome> {
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut files: Vec<FileState> = diffs.into_iter().map(FileState::new).collect();
    let moves = if settings.moved { mark_moves(&mut files) } else { Vec::new() };
    let repeats = mark_repeats(&mut files);
    let stream = Stream::new(&files, Some(settings.context), &HashSet::new(), None, &[], false);
    let mut highlights = spawn_highlighting(&files, &settings.highlighter);
    let (max_width, number_width) = measure(&files);
    let mut app = App {
        position: stream.change_starts(settings.scroll_off).first().copied().unwrap_or(0),
        stream,
        files,
        whole_file: false,
        context: settings.context,
        picker: None,
        picker_filter: String::new(),
        scroll_off: settings.scroll_off,
        keys: settings.keys.clone(),
        visual: false,
        visual_anchor: (false, 0),
        layout_mode: settings.layout,
        unified: false,
        split_panes: Panes::Both,
        theme: Theme::new(&settings.highlighter, terminal_colors),
        number_width,
        hscroll: 0,
        max_width,
        code_width: 0,
        sides: (Rect::default(), Rect::default()),
        selection: None,
        dragging: false,
        gutter: false,
        notice: settings.notice.clone(),
        search: None,
        body_rows: 0,
        panes: Panes::Both,
        generation: 0,
        opened: HashSet::new(),
        wrap: settings.wrap,
        copy_menu: false,
        pending_edit: None,
        picked_change: None,
        notes: Vec::new(),
        notes_file: settings.notes_file.clone(),
        agent_file: settings.agent_context.clone(),
        notes_stamp: String::new(),
        note_width: 0,
        picked_note: None,
        note_draft: None,
        editor_columns: 40,
        goto: None,
        moves,
        repeats,
        label_start: 0,
        repeat_list: None,
        find_moves: settings.moved,
        note_list: None,
        mark: None,
        pull_request: settings.pull_request.clone(),
        checks: None,
        review: None,
    };
    app.load_notes();
    // The view opens on the first change.
    app.picked_change = (!app.stream.blocks.is_empty()).then_some(0);
    let mut last_image: Option<(bool, usize, View)> = None;
    let mut notes_checked = std::time::Instant::now();

    loop {
        let size = term.size()?;
        let area = Rect::new(0, 0, size.width, size.height);
        let unified = match app.layout_mode {
            LayoutMode::Unified => true,
            LayoutMode::Split => false,
            LayoutMode::Auto => area.width < AUTO_UNIFIED_BELOW,
        };
        app.set_unified(unified);
        let parts = layout(area, app.panes);
        let body_rows = parts.left.height as usize;
        // Wrapping follows the width of the code on screen; both sides wrap at the same width.
        let columns = {
            let width = |side: Rect, old_side: bool| code_area(side, app.number_width, app.numbers_right(old_side)).1 as usize;
            match app.panes {
                Panes::Both => width(parts.left, true).min(width(parts.right, false)),
                Panes::Old => width(parts.left, true),
                Panes::New => width(parts.right, false),
            }
        };
        let wrap_width = app.wrap.then_some(columns);
        // Notes wrap at the width of the code, after their two-column bar.
        let note_width = columns.saturating_sub(2);
        if wrap_width != app.stream.wrap || (note_width != app.note_width && !app.notes.is_empty()) {
            app.note_width = note_width;
            app.rebuild(wrap_width);
        }
        app.note_width = note_width;
        app.editor_columns = note_editor_columns(area);
        let max = app.stream.scroll.max_position(body_rows);
        app.position = app.position.min(max);
        let (left_top, right_top) = app.stream.scroll.tops(app.position);
        let (cell_width, cell_height) = cell_pixels(area.width, area.height);
        let view = View { left_top, right_top, columns: parts.ribbons.width, rows: parts.ribbons.height, cell_width, cell_height };
        let file = app.current_file();
        let shown = if app.panes == Panes::New { parts.right } else { parts.left };
        app.code_width = shown.width.saturating_sub(app.number_width + 1) as usize;
        app.sides = (parts.left, parts.right);
        app.body_rows = body_rows;

        term.draw(|frame| {
            let buf = frame.buffer_mut();
            draw_side(buf, parts.left, left_top, &app, true);
            draw_side(buf, parts.right, right_top, &app, false);
            // Pin the current file's title over the first row, like a pull request page.
            if let Some(sticky) = app.sticky_file() {
                for (area, old_side) in [(parts.left, true), (parts.right, false)] {
                    let selected = app.is_selected(old_side, app.header_row(sticky, old_side));
                    draw_file_title(buf, Rect { height: 1, ..area }, &app, sticky, old_side, selected);
                }
            }
            draw_ribbons(buf, parts.ribbons, &app.stream.blocks, view, graphics, &app.theme);
            draw_status(buf, parts.status, &app, file);
            if let Some(selected) = app.note_list {
                draw_note_list(buf, area, &app, selected);
            }
            if let Some((group, selected)) = app.repeat_list {
                draw_repeat_list(buf, area, &app, group, selected);
            }
            if let Some(selected) = app.picker {
                draw_picker(buf, area, &app, selected);
            }
            if let Some(draft) = &app.note_draft {
                draw_note_editor(buf, area, &app, draft);
            }
        })?;

        if graphics == Graphics::Kitty {
            // The lists cover the ribbon column, so the image comes down while one is open.
            let wanted = (app.picker.is_none() && app.note_list.is_none() && app.note_draft.is_none() && app.repeat_list.is_none()).then_some((app.whole_file, app.generation, view));
            if wanted != last_image {
                let backend = term.backend_mut();
                match wanted {
                    Some(_) if view.columns > 0 && view.rows > 0 => {
                        if let Some(pixmap) = ribbons::rasterize(&app.stream.blocks, view, &app.theme.ribbons) {
                            backend.write_all(ribbons::kitty_place(&pixmap, parts.ribbons.x, parts.ribbons.y, view).as_bytes())?;
                        }
                    }
                    _ => backend.write_all(ribbons::kitty_delete().as_bytes())?,
                }
                backend.flush()?;
                last_image = wanted;
            }
        }

        // Wait for input or for syntax colors, then handle everything already queued before
        // drawing again, so a wheel burst draws once.
        let mut next = None;
        while next.is_none() {
            // Claude, or a person in another terminal, can change the notes file at any time.
            if notes_checked.elapsed() >= Duration::from_secs(1) {
                notes_checked = std::time::Instant::now();
                if app.refresh_notes() {
                    break;
                }
            }
            if event::poll(Duration::from_millis(50))? {
                next = Some(event::read()?);
            } else if let Ok(done) = highlights.try_recv() {
                let file = &mut app.files[done.file];
                *(if done.is_old { &mut file.old_tokens } else { &mut file.new_tokens }) = done.tokens;
                break;
            } else if let Some(diffs) = settings.reloads.as_ref().and_then(|r| r.try_recv().ok()) {
                if app.reload(diffs) {
                    highlights = spawn_highlighting(&app.files, &settings.highlighter);
                }
                break;
            } else if let Some(line) = settings.checks.as_ref().and_then(|c| c.try_recv().ok()) {
                app.checks = Some(line);
                break;
            } else if let Some(request) = settings.commands.as_ref().and_then(|c| c.try_recv().ok()) {
                // The client gave up on it (the editor was open): doing it now would surprise.
                if std::time::Instant::now() > request.deadline {
                    continue;
                }
                let reply = match request.command {
                    crate::listen::Command::Reload => match &settings.load_again {
                        Some(load) => {
                            let diffs = load();
                            let changed = !diffs.is_empty();
                            if app.reload(diffs) {
                                highlights = spawn_highlighting(&app.files, &settings.highlighter);
                            }
                            app.load_notes();
                            app.rebuild(app.stream.wrap);
                            if changed { crate::listen::ok(serde_json::json!({ "files": app.files.len() })) } else { crate::listen::error("no changes now; the view keeps the old ones") }
                        }
                        None => crate::listen::error("this diff came from a patch or from git's temporary files; it can't be read again"),
                    },
                    command => app.command(command),
                };
                let _ = request.reply.send(reply);
                break;
            }
        }
        let page = body_rows.max(1);
        while let Some(current) = next {
            if let Event::Resize(..) = current {
                term.autoresize()?;
                last_image = None;
            }
            if let Some(outcome) = app.apply(&current, page, max) {
                return Ok(outcome);
            }
            if let Some((path, line)) = app.pending_edit.take() {
                let edited = edit(&path, line, graphics);
                // A new terminal draws every cell again. (Terminal::clear asks the terminal for
                // the cursor position, which can time out.)
                io::stdout().write_all(b"\x1b[2J")?;
                term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
                last_image = None;
                match edited {
                    Err(error) => app.notice = Some(format!("cannot open the editor: {error}")),
                    // With --watch, the watcher sees the change; otherwise read the files again.
                    Ok(()) if settings.reloads.is_none() => {
                        if let Some(load) = &settings.load_again {
                            if app.reload(load()) {
                                highlights = spawn_highlighting(&app.files, &settings.highlighter);
                            }
                        }
                    }
                    Ok(()) => {}
                }
            }
            let max = app.stream.scroll.max_position(body_rows);
            app.position = app.position.min(max);
            next = if event::poll(Duration::ZERO)? { Some(event::read()?) } else { None };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, old: &str, new: &str) -> FileState {
        FileState::new(FileDiff { old_name: name.into(), new_name: name.into(), model: Model::new(old, new), path: None })
    }

    #[test]
    fn skips_columns_across_spans_and_wide_characters() {
        let text = |spans: Vec<Span<'static>>| spans.iter().map(|s| s.content.to_string()).collect::<String>();
        let spans = vec![Span::raw("const "), Span::raw("total"), Span::raw(" = 1")];
        assert_eq!(text(skip_columns(spans.clone(), 0)), "const total = 1");
        assert_eq!(text(skip_columns(spans.clone(), 8)), "tal = 1");
        assert_eq!(text(skip_columns(spans, 40)), "");
        // A two-column character cut in half leaves one space in its place.
        assert_eq!(text(skip_columns(vec![Span::raw("a漢b")], 2)), " b");
    }

    #[test]
    fn wrapped_lines_keep_both_sides_level() {
        let long = "x".repeat(25);
        let old = format!("a\n{long}\nb\nc\nd\n");
        let new = format!("a\n{long}\nb\nCHANGED {long}\nd\n");
        let files = vec![file("a.ts", &old, &new)];
        let stream = Stream::new(&files, None, &HashSet::new(), Some(10), &[], false);
        // Header, a, the long line in 3 rows, b, then the change: "c" (1 row) vs 4 rows.
        assert_eq!(stream.right[2..6], [Row::Line(0, 1), Row::Wrap(0, 1, 1), Row::Wrap(0, 1, 2), Row::Line(0, 2)]);
        assert_eq!(stream.left[2..6], stream.right[2..6]);
        assert_eq!(stream.blocks[0], Block { a0: 6, a1: 7, b0: 6, b1: 10 });
        // The line after the change sits level on both sides.
        let d = |rows: &[Row]| rows.iter().position(|r| *r == Row::Line(0, 4)).unwrap();
        let (left, right) = stream.scroll.tops(stream.scroll.position_of_row(d(&stream.right), false));
        assert_eq!((left, right), (d(&stream.left), d(&stream.right)));
    }

    #[test]
    fn notes_keep_both_sides_level_and_open_their_fold() {
        let numbers = |n: usize| (1..=n).map(|i| format!("{i}\n")).collect::<String>();
        let files = vec![file("a.ts", &numbers(30), &numbers(30).replace("\n20\n", "\ntwenty\n"))];
        let note = |line: usize, old_side: bool, note: usize, rows: usize| NoteRows { file: 0, old_side, line, note, rows };
        // A note outside changes on the new side (line 5, inside the first fold), and one on
        // the old side of the change (line 20).
        let notes = [note(4, false, 0, 2), note(19, true, 1, 3)];
        let stream = Stream::new(&files, Some(3), &HashSet::new(), None, &notes, false);
        let at = |rows: &[Row], r: Row| rows.iter().position(|x| *x == r).unwrap();
        let (left, right) = (&stream.left, &stream.right);
        assert_eq!(&right[at(right, Row::Line(0, 4)) + 1..][..2], [Row::Note(0, 0), Row::Note(0, 1)]);
        // The other side gets blank rows, so line 6 stays level.
        assert_eq!(at(left, Row::Line(0, 5)), at(right, Row::Line(0, 5)));
        assert_eq!(&left[at(left, Row::Line(0, 19)) + 1..][..3], [Row::Note(1, 0), Row::Note(1, 1), Row::Note(1, 2)]);
        // Inside the change only the old side grows, and the line after it is level by scrolling.
        let block = stream.blocks[0];
        assert_eq!((block.a1 - block.a0, block.b1 - block.b0), (4, 1));
        let next = |rows: &[Row]| at(rows, Row::Line(0, 20));
        let (l, r) = stream.scroll.tops(stream.scroll.position_of_row(next(right), false));
        assert_eq!((l, r), (next(left), next(right)));
    }

    #[test]
    fn reads_what_follows_the_colon() {
        assert_eq!(parse_goto("42"), Ok((None, false, (42, 42))));
        assert_eq!(parse_goto(" 40-52 "), Ok((None, false, (40, 52))));
        assert_eq!(parse_goto("o7"), Ok((None, true, (7, 7))));
        assert_eq!(parse_goto("src/main.rs:3,5"), Ok((Some("src/main.rs"), false, (3, 5))));
        assert_eq!(parse_goto("main:o9"), Ok((Some("main"), true, (9, 9))));
        assert!(parse_goto("9-3").is_err());
        assert!(parse_goto("0").is_err());
        assert!(parse_goto("main.rs").is_err());
    }

    #[test]
    fn unified_puts_old_lines_above_new_ones() {
        let files = vec![file("a.ts", "a\nb\nc\nd\n", "a\nB\nB2\nc\nd\n")];
        let stream = Stream::new(&files, None, &HashSet::new(), None, &[], true);
        assert_eq!(stream.left, stream.right);
        assert_eq!(
            stream.right,
            [Row::Header(0), Row::Line(0, 0), Row::Old(0, 1, 0), Row::Line(0, 1), Row::Line(0, 2), Row::Line(0, 3), Row::Line(0, 4)]
        );
        assert_eq!(stream.blocks, [Block { a0: 2, a1: 5, b0: 2, b1: 5 }]);
        assert_eq!(stream.change_starts(0), [2]);
    }

    #[test]
    fn both_sides_reach_each_file_header_together() {
        let numbers = |n: usize| (1..=n).map(|i| format!("{i}\n")).collect::<String>();
        let files = vec![
            // The first file grows by four lines, so the sides are offset after it.
            file("a.ts", &numbers(20), &numbers(20).replace("\n18\n", "\n18\nx\ny\nz\nw\n")),
            file("b.ts", &numbers(30), &numbers(30).replace("\n5\n", "\nfive\n")),
        ];
        let stream = Stream::new(&files, Some(3), &HashSet::new(), None, &[], false);
        for (index, &row) in stream.headers.iter().enumerate() {
            let (left, right) = stream.scroll.tops(stream.header_position(index));
            assert_eq!(stream.left[left], Row::Header(index));
            assert_eq!(stream.right[right], Row::Header(index));
            assert_eq!(left, row);
            assert_eq!(stream.file_at(stream.header_position(index)), index);
        }
        assert_eq!(stream.blocks.len(), 2);

        // Wherever a header is on screen, it is on the same screen row on both sides.
        let height = 10;
        for position in 0..=stream.scroll.max_position(height) {
            let (left_top, right_top) = stream.scroll.tops(position);
            for &header in &stream.headers[1..] {
                let left_row = header.checked_sub(left_top).filter(|&r| r < height);
                let right_row = stream.right.iter().position(|r| *r == stream.left[header]).and_then(|h| h.checked_sub(right_top)).filter(|&r| r < height);
                if left_row.is_some() || right_row.is_some() {
                    assert_eq!(left_row, right_row, "position {position}");
                }
            }
        }
        assert_eq!(stream.sticky_file(stream.headers[1], true), None);
        assert_eq!(stream.sticky_file(stream.headers[1] + 1, true), Some(1));
        assert_eq!(stream.sticky_file(stream.headers[1] - 2, true), Some(0));
    }
}
