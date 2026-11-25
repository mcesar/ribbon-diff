//! The interactive two-pane view over one or more changed files.

use crate::gutter::{self, RibbonColors, View};
use crate::highlight::{Highlighter, Paint, Rgb, Tokens};
use crate::termcolors::{self, TermColors};
use crate::model::{Block, BlockKind, Display, Item, Model, ScrollMap};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use crossterm::{cursor, execute, terminal};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
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

/// Button and wheel reports only (SGR encoding). crossterm's EnableMouseCapture also turns on
/// "report every motion", which can flood the tty input queue while a frame is being written.
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1006h";
const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1000l";

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
    fn sticky_file(&self, left_top: usize) -> Option<usize> {
        match self.left.get(left_top) {
            Some(Row::Header(_)) | None => None,
            _ => Some(self.headers.iter().rposition(|&row| row <= left_top).unwrap_or(0)),
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

fn layout(area: Rect) -> Layout {
    let body_height = area.height.saturating_sub(1);
    let side = area.width.saturating_sub(GUTTER_COLUMNS) / 2;
    let gutter_width = area.width.saturating_sub(side * 2);
    let body = |x, width| Rect { x, y: area.y, width, height: body_height };
    Layout {
        left: body(area.x, side),
        gutter: body(area.x + side, gutter_width),
        right: body(area.x + side + gutter_width, side),
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

/// Line numbers sit next to the gutter: at the right edge of the old side, the left edge of the new.
fn draw_side(buf: &mut Buffer, area: Rect, top: usize, app: &App, old_side: bool) {
    let theme = &app.theme;
    let rows = if old_side { &app.stream.left } else { &app.stream.right };
    let number_width = app.number_width;
    let code_width = area.width.saturating_sub(number_width + 1);
    let (code_x, number_x) = if old_side {
        (area.x + 1, area.x + area.width.saturating_sub(number_width))
    } else {
        (area.x + number_width + 1, area.x)
    };

    for row in 0..area.height {
        let y = area.y + row;
        let row_area = Rect { y, height: 1, ..area };
        match rows.get(top + row as usize) {
            None | Some(Row::Gap) => clear(buf, row_area, theme.base()),
            Some(&Row::Header(index)) => draw_file_title(buf, row_area, app, index, old_side),
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
                let number = if old_side {
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
            }
        }
    }
}

/// One side of a file's title row: its path and its line count on that side.
fn draw_file_title(buf: &mut Buffer, area: Rect, app: &App, index: usize, old_side: bool) {
    let theme = &app.theme;
    let file = &app.files[index];
    clear(buf, area, theme.panel());
    let name = if old_side { &file.diff.old_name } else { &file.diff.new_name };
    let counts = if old_side { format!("-{}", file.removed) } else { format!("+{}", file.added) };
    let room = area.width.saturating_sub(counts.len() as u16 + 3) as usize;
    let spans = vec![
        Span::styled(format!(" {} ", fit_path(name, room)), theme.file_title()),
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
    let file_part = if app.files.len() > 1 {
        format!(" file {}/{}  ·", file + 1, app.files.len())
    } else {
        String::new()
    };
    let context = if app.whole_file { "c: context" } else { "c: whole file" };
    let text = format!(
        "{file_part} change {}/{}{}   n/N change  ]/[ file  f files  {context}  h/l j/k space/b g/G scroll  q quit",
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
}

impl App {
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
        self.stream = Stream::new(&self.files, (!self.whole_file).then_some(self.context));
        let starts = self.stream.change_starts();
        // Stay on the same change, or on the same file when it has none above the view.
        self.position = match starts.get(change.saturating_sub(1)) {
            Some(&start) if self.stream.file_at(start) == file => start,
            _ => self.stream.header_position(file),
        };
    }

    /// Apply one input event. Returns how to quit, if the event quits.
    fn apply(&mut self, event: &Event, page: usize, max: usize) -> Option<Outcome> {
        if let Some(selected) = self.picker {
            if let Event::Key(key) = event {
                self.apply_picker(key, selected);
            }
            return None;
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
                    KeyCode::Char('q') | KeyCode::Esc => return Some(Outcome::Quit),
                    KeyCode::Char('c') if ctrl => return Some(Outcome::QuitAll),
                    KeyCode::Char('Q') => return Some(Outcome::QuitAll),
                    KeyCode::Char('j') | KeyCode::Down | KeyCode::Enter => position + 1,
                    KeyCode::Char('k') | KeyCode::Up => position.saturating_sub(1),
                    KeyCode::Char('f') if ctrl => position + page,
                    KeyCode::Char('b') if ctrl => position.saturating_sub(page),
                    KeyCode::Char('d') => position + page / 2,
                    KeyCode::Char('u') => position.saturating_sub(page / 2),
                    KeyCode::Char(' ') | KeyCode::PageDown => position + page,
                    KeyCode::Char('b') | KeyCode::PageUp => position.saturating_sub(page),
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
                MouseEventKind::ScrollDown => self.position += 3,
                MouseEventKind::ScrollUp => self.position = position.saturating_sub(3),
                _ => {}
            },
            _ => {}
        }
        None
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
    let result = event_loop(diffs, settings, terminal_colors);
    restore_terminal(graphics);
    result
}

fn event_loop(diffs: Vec<FileDiff>, settings: Settings, terminal_colors: TermColors) -> anyhow::Result<Outcome> {
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let files: Vec<FileState> = diffs.into_iter().map(FileState::new).collect();
    let stream = Stream::new(&files, Some(settings.context));
    let max_line = files.iter().map(|f| f.diff.model.old.len().max(f.diff.model.new.len())).max().unwrap_or(1).max(1);

    // One background thread colors the files in stream order; each result redraws.
    let (sender, highlights) = mpsc::channel::<Highlighted>();
    let jobs: Vec<(usize, String, Vec<String>, Vec<String>)> = files
        .iter()
        .enumerate()
        .map(|(i, f)| (i, f.diff.new_name.clone(), f.diff.model.old.clone(), f.diff.model.new.clone()))
        .collect();
    let highlighter = Arc::clone(&settings.highlighter);
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

    let max_width = files
        .iter()
        .flat_map(|f| f.diff.model.old.iter().chain(&f.diff.model.new))
        .map(|line| line.replace('\t', TAB).width())
        .max()
        .unwrap_or(0);
    let mut app = App {
        position: stream.change_starts().first().copied().unwrap_or(0),
        stream,
        files,
        whole_file: false,
        context: settings.context,
        picker: None,
        theme: Theme::new(&settings.highlighter, terminal_colors),
        number_width: (max_line.to_string().len() + 1) as u16,
        hscroll: 0,
        max_width,
        code_width: 0,
    };
    let graphics = settings.graphics;
    let mut last_image: Option<(bool, View)> = None;

    loop {
        let size = term.size()?;
        let area = Rect::new(0, 0, size.width, size.height);
        let parts = layout(area);
        let body_rows = parts.left.height as usize;
        let max = app.stream.scroll.max_position(body_rows);
        app.position = app.position.min(max);
        let (left_top, right_top) = app.stream.scroll.tops(app.position);
        let (cell_width, cell_height) = cell_pixels(area.width, area.height);
        let view = View { left_top, right_top, columns: parts.gutter.width, rows: parts.gutter.height, cell_width, cell_height };
        let file = app.current_file();
        app.code_width = parts.left.width.saturating_sub(app.number_width + 1) as usize;

        term.draw(|frame| {
            let buf = frame.buffer_mut();
            draw_side(buf, parts.left, left_top, &app, true);
            draw_side(buf, parts.right, right_top, &app, false);
            // Pin the current file's title over the first row, like a pull request page.
            if let Some(sticky) = app.stream.sticky_file(left_top) {
                draw_file_title(buf, Rect { height: 1, ..parts.left }, &app, sticky, true);
                draw_file_title(buf, Rect { height: 1, ..parts.right }, &app, sticky, false);
            }
            draw_gutter(buf, parts.gutter, &app.stream.blocks, view, graphics, &app.theme);
            draw_status(buf, parts.status, &app, file);
            if let Some(selected) = app.picker {
                draw_picker(buf, area, &app, selected);
            }
        })?;

        if graphics == Graphics::Kitty {
            // The picker covers the gutter, so the image comes down while it is open.
            let wanted = app.picker.is_none().then_some((app.whole_file, view));
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
        assert_eq!(stream.sticky_file(stream.headers[1]), None);
        assert_eq!(stream.sticky_file(stream.headers[1] + 1), Some(1));
    }
}
