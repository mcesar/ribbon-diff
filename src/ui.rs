//! The interactive two-pane view over one or more changed files.

use crate::gutter::{self, RibbonColors, View};
use crate::highlight::{Highlighter, Paint, Rgb, Tokens};
use crate::termcolors::{self, TermColors};
use crate::model::{Block, BlockKind, Display, Item, Model, ScrollMap};
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

const GUTTER_COLUMNS: u16 = 6;
const TAB: &str = "    ";
const CONTEXT_ABOVE_CHANGE: usize = 3;

/// How much of the change color goes into a changed row and into its changed words. A faint
/// row with clearly stronger words keeps the edit itself easy to spot.
const ROW_TINT: f32 = 0.10;
const WORD_TINT: f32 = 0.42;

const INSERT: Rgb = [98, 186, 108];
const DELETE: Rgb = [224, 98, 88];

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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Graphics {
    Kitty,
    Blocks,
}

pub enum Outcome {
    Quit,
    QuitAll,
}

/// One changed file, as given to the viewer.
pub struct FileDiff {
    pub old_name: String,
    pub new_name: String,
    pub model: Model,
}

pub struct Settings {
    pub graphics: Graphics,
    /// Unchanged lines kept around each change; the `c` key shows the whole file instead.
    pub context: usize,
    pub highlighter: Arc<Highlighter>,
    /// With `--watch`: a fresh set of changed files whenever they change on disk.
    pub reloads: Option<mpsc::Receiver<Vec<FileDiff>>>,
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
            diff,
            old_tokens: Vec::new(),
            new_tokens: Vec::new(),
        }
    }
}

/// One row of one side of the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    /// The title row that starts a file.
    Header(usize),
    /// A source line: file, zero-based line index.
    Line(usize, usize),
    /// Hidden unchanged lines.
    Fold(usize),
    /// Space between two files.
    Gap,
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
}

impl Stream {
    fn new(files: &[FileState], context: Option<usize>) -> Self {
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
            let display = Display::new(&file.diff.model, context);
            let row = |item: &Item| match *item {
                Item::Line(line) => Row::Line(index, line),
                Item::Fold(hidden) => Row::Fold(hidden),
                Item::Blank => Row::Gap,
            };
            let mut file_left: Vec<Row> = display.left.iter().map(row).collect();
            let mut file_right: Vec<Row> = display.right.iter().map(row).collect();
            let mut file_scroll_blocks = display.scroll_blocks.clone();

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

            let (left_start, right_start) = (left.len(), right.len());
            let shift = |b: &Block| Block { a0: b.a0 + left_start, a1: b.a1 + left_start, b0: b.b0 + right_start, b1: b.b1 + right_start };
            blocks.extend(display.blocks.iter().map(shift));
            scroll_blocks.extend(file_scroll_blocks.iter().map(shift));
            left.extend(file_left);
            right.extend(file_right);
        }
        let scroll = ScrollMap::new(&scroll_blocks, left.len(), right.len());
        Stream { left, right, blocks, scroll, headers }
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
    fn change_starts(&self) -> Vec<usize> {
        self.blocks
            .iter()
            .filter_map(|b| self.scroll.block_start(b.a0, b.b0))
            .map(|s| s.saturating_sub(CONTEXT_ABOVE_CHANGE))
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

fn rgb([r, g, b]: Rgb) -> Color {
    Color::Rgb(r, g, b)
}

fn mix(a: Rgb, b: Rgb, amount: f32) -> Rgb {
    [0, 1, 2].map(|i| (a[i] as f32 * amount + b[i] as f32 * (1.0 - amount)).round() as u8)
}

struct Layout {
    left: Rect,
    gutter: Rect,
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

/// One side alone takes the whole width, with no gutter; the other side gets no columns.
fn layout(area: Rect, panes: Panes) -> Layout {
    let body_height = area.height.saturating_sub(1);
    let (left_width, gutter_width) = match panes {
        Panes::Both => {
            let side = area.width.saturating_sub(GUTTER_COLUMNS) / 2;
            (side, area.width.saturating_sub(side * 2))
        }
        Panes::Old => (area.width, 0),
        Panes::New => (0, 0),
    };
    let right_width = area.width - left_width - gutter_width;
    let body = |x, width| Rect { x, y: area.y, width, height: body_height };
    Layout {
        left: body(area.x, left_width),
        gutter: body(area.x + left_width, gutter_width),
        right: body(area.x + left_width + gutter_width, right_width),
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

/// Line numbers sit next to the gutter: at the right edge of the old side, the left edge of the
/// new. A side alone has them on the left.
fn draw_side(buf: &mut Buffer, area: Rect, top: usize, app: &App, old_side: bool) {
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
            Some(Row::Fold(hidden)) => {
                let style = theme.base().fg(theme.muted()).add_modifier(Modifier::ITALIC);
                clear(buf, row_area, style);
                let label = format!("⋯ {hidden} unchanged lines");
                buf.set_line(code_x, y, &Line::styled(label, style), code_width);
            }
            Some(&Row::Line(file_index, index)) => {
                let file = &app.files[file_index];
                let (lines, tokens, kinds, inline) = if old_side {
                    (&file.diff.model.old, &file.old_tokens, &file.old_kinds, &file.diff.model.old_inline)
                } else {
                    (&file.diff.model.new, &file.new_tokens, &file.new_kinds, &file.diff.model.new_inline)
                };
                let changed = kinds[index].is_some();
                let tints = if changed { theme.tints(old_side) } else { None };
                let row_style = match tints {
                    Some((tint, _)) => theme.base().bg(tint),
                    None => theme.base(),
                };
                clear(buf, row_area, row_style);
                let number_style = if changed {
                    row_style.fg(theme.change_color(old_side))
                } else {
                    row_style.fg(theme.muted())
                };
                let number = if numbers_right {
                    format!("{:>width$} ", index + 1, width = number_width as usize - 1)
                } else {
                    format!(" {:<width$}", index + 1, width = number_width as usize - 1)
                };
                buf.set_line(number_x, y, &Line::styled(number, number_style), number_width);
                let plain = || vec![(Paint::Plain, lines[index].clone())];
                let line_tokens = tokens.get(index).cloned().unwrap_or_else(plain);
                let change = changed.then(|| Change {
                    color: theme.change_color(old_side),
                    strong_bg: tints.map(|(_, strong)| strong),
                    // A whole-line change, or a modified line without word matches, is all changed.
                    whole_line: inline[index].is_empty(),
                });
                let spans = skip_columns(styled_tokens(&line_tokens, &inline[index], row_style, change, theme), app.hscroll);
                buf.set_line(code_x, y, &Line::from(spans), code_width);
                let line = &lines[index];
                let code = (code_x, code_width);
                if let Some(found) = &app.search {
                    let current = found.matches.get(found.current);
                    for bytes in search::find_all(line, &found.query) {
                        let is_current = current.is_some_and(|m| m.old_side == old_side && m.row == at && m.bytes == bytes);
                        // Yellow like most editors; the current match is orange.
                        let bg = if is_current { Color::Indexed(208) } else { Color::Yellow };
                        let columns = (select::line_width(&line[..bytes.start]), select::line_width(&line[..bytes.end]));
                        paint_columns(buf, code, y, app.hscroll, columns, |cell| {
                            cell.set_bg(bg).set_fg(Color::Black);
                        });
                    }
                }
                let selected = app.selection.filter(|s| s.old_side == old_side).and_then(|s| s.columns(at));
                if let Some((from, to)) = selected {
                    // Selected text is reversed, up to the end of the line.
                    let columns = (from, to.min(select::line_width(line)));
                    paint_columns(buf, code, y, app.hscroll, columns, |cell| {
                        cell.modifier.insert(Modifier::REVERSED);
                    });
                }
            }
        }
    }
}

/// One side of a file's title row: its path and its line count on that side.
/// A selected title shows its path reversed; copying it copies the whole path.
fn draw_file_title(buf: &mut Buffer, area: Rect, app: &App, index: usize, old_side: bool, selected: bool) {
    let theme = &app.theme;
    let file = &app.files[index];
    clear(buf, area, theme.panel());
    let name = if old_side { &file.diff.old_name } else { &file.diff.new_name };
    let counts = if old_side { format!("-{}", file.removed) } else { format!("+{}", file.added) };
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
fn draw_gutter(buf: &mut Buffer, area: Rect, blocks: &[Block], view: View, graphics: Graphics, theme: &Theme) {
    let base = theme.base();
    clear(buf, area, base);
    if graphics == Graphics::Kitty {
        return;
    }
    let background = base.bg.unwrap_or(Color::Reset);
    let color = |c: Option<Rgb>| c.map_or(background, rgb);
    let cells = gutter::half_blocks(blocks, view, &theme.ribbons);
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
        Panes::Both => "",
        Panes::Old => " OLD SIDE ONLY (< both)  ·",
        Panes::New => " NEW SIDE ONLY (> both)  ·",
    };
    let file_part = if app.files.len() > 1 {
        format!("{side} file {}/{}  ·", file + 1, app.files.len())
    } else {
        side.to_owned()
    };
    let message = match (&app.notice, &app.search) {
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
        (None, None) => None,
    };
    if let Some(message) = message {
        buf.set_line(area.x, area.y, &Line::styled(message, style), area.width);
        return;
    }
    let context = if app.whole_file { "c: context" } else { "c: whole file" };
    let text = format!(
        "{file_part} change {}/{}{}   n/N change  ]/[ file  f files  {context}  </> one side  / search  h/l j/k space/S-space g/G scroll  q quit",
        app.current_change(),
        app.stream.blocks.len(),
        if app.hscroll > 0 { format!("  ·  col {}", app.hscroll + 1) } else { String::new() },
    );
    buf.set_line(area.x, area.y, &Line::styled(text, style), area.width);
}

/// A centered list of files; the selected one is highlighted.
fn draw_picker(buf: &mut Buffer, area: Rect, app: &App, selected: usize) {
    let theme = &app.theme;
    let width = area.width.saturating_sub(8).min(100).max(20);
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
    let title = format!(" {count} {noun}  (Enter open, Esc close)");
    buf.set_line(x, y, &Line::styled(title, frame.add_modifier(Modifier::BOLD)), width);

    let rows = (height - 2) as usize;
    let first = selected.saturating_sub(rows.saturating_sub(1));
    for (row, (index, file)) in app.files.iter().enumerate().skip(first).take(rows).enumerate() {
        let style = if index == selected { frame.add_modifier(Modifier::REVERSED) } else { frame };
        let row_area = Rect::new(x, y + 1 + row as u16, width, 1);
        clear(buf, row_area, style);
        let spans = vec![
            Span::styled(format!(" {:>5} ", format!("+{}", file.added)), style.fg(theme.change_color(false))),
            Span::styled(format!("{:>5}  ", format!("-{}", file.removed)), style.fg(theme.change_color(true))),
            Span::styled(fit_path(&file.diff.new_name, width.saturating_sub(15) as usize), style),
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
    picker: Option<usize>,
    theme: Theme,
    number_width: u16,
    /// The two sides from the last frame, to place mouse events.
    sides: (Rect, Rect),
    selection: Option<Selection>,
    /// True while the mouse button that started the selection is down.
    dragging: bool,
    /// A short message that replaces the key help until the next key.
    notice: Option<String>,
    search: Option<Search>,
    /// Body rows from the last frame.
    body_rows: usize,
    panes: Panes,
    /// Counts reloads, so the ribbon image is drawn again for new changes.
    generation: usize,
}

impl App {
    /// Show a new set of changed files (from `--watch`) and stay on the same file and line.
    /// Files whose text did not change keep their colors. Returns false when nothing is shown
    /// differently.
    fn reload(&mut self, diffs: Vec<FileDiff>) -> bool {
        if diffs.is_empty() {
            self.notice = Some("no changes now; still watching".into());
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
        self.stream = Stream::new(&self.files, (!self.whole_file).then_some(self.context));
        self.generation += 1;
        self.selection = None;
        self.dragging = false;
        if let Some(selected) = self.picker {
            self.picker = Some(selected.min(self.files.len() - 1));
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
        let passed = self.stream.change_starts().iter().filter(|&&s| s <= self.position).count();
        if self.stream.blocks.is_empty() { 0 } else { passed.max(1) }
    }

    fn toggle_whole_file(&mut self) {
        let change = self.current_change();
        let file = self.current_file();
        self.whole_file = !self.whole_file;
        self.selection = None;
        let had_search = self.search.is_some();
        self.stream = Stream::new(&self.files, (!self.whole_file).then_some(self.context));
        let starts = self.stream.change_starts();
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
        if let Event::Key(key) = event {
            if key.kind != KeyEventKind::Release {
                self.notice = None;
            }
        }
        if self.search.as_ref().is_some_and(|s| s.editing) {
            if let Event::Key(key) = event {
                self.apply_search_key(key);
            }
            return None;
        }
        if let Event::Mouse(mouse) = event {
            if self.select(mouse) {
                return None;
            }
        }
        if let Some(columns) = self.horizontal(event) {
            self.hscroll = columns.min(self.max_width.saturating_sub(self.code_width / 2));
            return None;
        }
        let file = self.current_file();
        let files = self.files.len();
        let starts = self.stream.change_starts();
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
                    KeyCode::Char('y') => {
                        self.copy_selection();
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
                    KeyCode::Char('n') => starts.iter().copied().find(|&s| s > position).unwrap_or(position),
                    KeyCode::Char('N') | KeyCode::Char('p') => {
                        starts.iter().copied().rev().find(|&s| s < position).unwrap_or(position)
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
        None
    }

    fn line_at(&self, old_side: bool, row: usize) -> Option<&str> {
        let rows = if old_side { &self.stream.left } else { &self.stream.right };
        let &Row::Line(file, index) = rows.get(row)? else { return None };
        let model = &self.files[file].diff.model;
        Some(if old_side { &model.old[index] } else { &model.new[index] })
    }

    /// Find the query in every shown line of both sides, in scroll order.
    fn update_matches(&mut self) {
        let Some(query) = self.search.as_ref().map(|s| s.query.clone()) else { return };
        let mut matches = Vec::new();
        for old_side in [true, false] {
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
        // The first row can be under the sticky file title.
        if m.row <= top || m.row >= top + self.body_rows {
            self.position = self.stream.scroll.position_of_row(m.row, m.old_side).saturating_sub(CONTEXT_ABOVE_CHANGE);
        }
        let Some(line) = self.line_at(m.old_side, m.row) else { return };
        let (from, to) = (select::line_width(&line[..m.bytes.start]), select::line_width(&line[..m.bytes.end]));
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
        (shown.get(row).or(shown.last()).copied().unwrap_or(top), column + self.hscroll)
    }

    /// Start, extend, or finish a selection with the left button. Returns true when the event
    /// was used.
    fn select(&mut self, mouse: &MouseEvent) -> bool {
        let (x, y) = (mouse.column, mouse.row);
        let inside = |r: Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let side = if inside(self.sides.0) { Some(true) } else if inside(self.sides.1) { Some(false) } else { None };
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
                self.selection = Some(Selection { head, ..selection });
                true
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging => {
                self.dragging = false;
                match self.selection {
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
        for row in start.0..=end.0 {
            match rows.get(row) {
                Some(&Row::Line(file, index)) => {
                    let model = &self.files[file].diff.model;
                    let line = if selection.old_side { &model.old[index] } else { &model.new[index] };
                    let (from, to) = selection.columns(row).unwrap_or((0, 0));
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

    fn apply_picker(&mut self, key: &KeyEvent, selected: usize) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let last = self.files.len() - 1;
        self.picker = match key.code {
            KeyCode::Esc | KeyCode::Char('f') | KeyCode::Char('q') | KeyCode::Tab => None,
            KeyCode::Enter => {
                self.position = self.stream.header_position(selected);
                None
            }
            KeyCode::Char('j') | KeyCode::Down => Some((selected + 1).min(last)),
            KeyCode::Char('k') | KeyCode::Up => Some(selected.saturating_sub(1)),
            KeyCode::Char('g') | KeyCode::Home => Some(0),
            KeyCode::Char('G') | KeyCode::End => Some(last),
            _ => Some(selected),
        };
    }
}

pub fn restore_terminal(graphics: Graphics) {
    let mut out = io::stdout();
    // Only Kitty-capable terminals get graphics commands; others may print them as text.
    if graphics == Graphics::Kitty {
        let _ = out.write_all(gutter::kitty_delete().as_bytes());
    }
    let _ = out.write_all(MOUSE_OFF.as_bytes());
    let _ = out.write_all(KEYS_OFF.as_bytes());
    let _ = execute!(out, terminal::LeaveAlternateScreen, cursor::Show);
    let _ = terminal::disable_raw_mode();
}

pub fn run(diffs: Vec<FileDiff>, settings: Settings) -> anyhow::Result<Outcome> {
    terminal::enable_raw_mode()?;
    execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
    io::stdout().write_all(MOUSE_ON.as_bytes())?;
    let graphics = settings.graphics;
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal(graphics);
        previous_hook(info);
    }));

    // Ask before the event loop starts reading the terminal, so the replies are not taken as keys.
    let terminal_colors = termcolors::query();
    io::stdout().write_all(KEYS_ON.as_bytes())?;
    io::stdout().flush()?;
    let result = event_loop(diffs, settings, terminal_colors);
    restore_terminal(graphics);
    result
}

fn event_loop(diffs: Vec<FileDiff>, settings: Settings, terminal_colors: TermColors) -> anyhow::Result<Outcome> {
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let files: Vec<FileState> = diffs.into_iter().map(FileState::new).collect();
    let stream = Stream::new(&files, Some(settings.context));
    let mut highlights = spawn_highlighting(&files, &settings.highlighter);
    let (max_width, number_width) = measure(&files);
    let mut app = App {
        position: stream.change_starts().first().copied().unwrap_or(0),
        stream,
        files,
        whole_file: false,
        context: settings.context,
        picker: None,
        theme: Theme::new(&settings.highlighter, terminal_colors),
        number_width,
        hscroll: 0,
        max_width,
        code_width: 0,
        sides: (Rect::default(), Rect::default()),
        selection: None,
        dragging: false,
        notice: None,
        search: None,
        body_rows: 0,
        panes: Panes::Both,
        generation: 0,
    };
    let graphics = settings.graphics;
    let mut last_image: Option<(bool, usize, View)> = None;

    loop {
        let size = term.size()?;
        let area = Rect::new(0, 0, size.width, size.height);
        let parts = layout(area, app.panes);
        let body_rows = parts.left.height as usize;
        let max = app.stream.scroll.max_position(body_rows);
        app.position = app.position.min(max);
        let (left_top, right_top) = app.stream.scroll.tops(app.position);
        let (cell_width, cell_height) = cell_pixels(area.width, area.height);
        let view = View { left_top, right_top, columns: parts.gutter.width, rows: parts.gutter.height, cell_width, cell_height };
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
            draw_gutter(buf, parts.gutter, &app.stream.blocks, view, graphics, &app.theme);
            draw_status(buf, parts.status, &app, file);
            if let Some(selected) = app.picker {
                draw_picker(buf, area, &app, selected);
            }
        })?;

        if graphics == Graphics::Kitty {
            // The picker covers the gutter, so the image comes down while it is open.
            let wanted = app.picker.is_none().then_some((app.whole_file, app.generation, view));
            if wanted != last_image {
                let backend = term.backend_mut();
                match wanted {
                    Some(_) if view.columns > 0 && view.rows > 0 => {
                        if let Some(pixmap) = gutter::rasterize(&app.stream.blocks, view, &app.theme.ribbons) {
                            backend.write_all(gutter::kitty_place(&pixmap, parts.gutter.x, parts.gutter.y, view).as_bytes())?;
                        }
                    }
                    _ => backend.write_all(gutter::kitty_delete().as_bytes())?,
                }
                backend.flush()?;
                last_image = wanted;
            }
        }

        // Wait for input or for syntax colors, then handle everything already queued before
        // drawing again, so a wheel burst draws once.
        let mut next = None;
        while next.is_none() {
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
        FileState::new(FileDiff { old_name: name.into(), new_name: name.into(), model: Model::new(old, new) })
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
    fn both_sides_reach_each_file_header_together() {
        let numbers = |n: usize| (1..=n).map(|i| format!("{i}\n")).collect::<String>();
        let files = vec![
            // The first file grows by four lines, so the sides are offset after it.
            file("a.ts", &numbers(20), &numbers(20).replace("\n18\n", "\n18\nx\ny\nz\nw\n")),
            file("b.ts", &numbers(30), &numbers(30).replace("\n5\n", "\nfive\n")),
        ];
        let stream = Stream::new(&files, Some(3));
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
