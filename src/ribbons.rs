//! The ribbons between the two sides: rasterize the visible ones, then show them with Kitty
//! graphics or approximate them with half-block characters.

use crate::highlight::Rgb;
use crate::model::{Block, BlockKind};
use base64::Engine;
use tiny_skia::{
    Color, FillRule, GradientStop, LinearGradient, Paint, PathBuilder, Pixmap, Point, SpreadMode, Stroke, Transform,
};

pub const IMAGE_ID: u32 = 0x7269; // "ri"

#[derive(Clone, Copy)]
pub struct RibbonColors {
    /// The color behind the ribbons. None leaves the image transparent.
    pub background: Option<Rgb>,
    pub insert: Rgb,
    pub delete: Rgb,
}

/// Fill or edge paint for one block: red for a deletion, green for an insertion, and a
/// red-to-green fade for an edit, like the old side flowing into the new.
fn ribbon_paint(kind: BlockKind, colors: &RibbonColors, width: f32, alpha: u8) -> Paint<'static> {
    let color = |[r, g, b]: Rgb| Color::from_rgba8(r, g, b, alpha);
    let mut paint = Paint::default();
    paint.anti_alias = true;
    match kind {
        BlockKind::Insert => paint.set_color(color(colors.insert)),
        BlockKind::Delete => paint.set_color(color(colors.delete)),
        BlockKind::Modify => {
            let stops = vec![GradientStop::new(0.15, color(colors.delete)), GradientStop::new(0.85, color(colors.insert))];
            match LinearGradient::new(Point::from_xy(0.0, 0.0), Point::from_xy(width, 0.0), stops, SpreadMode::Pad, Transform::identity()) {
                Some(shader) => paint.shader = shader,
                None => paint.set_color(color(colors.insert)),
            }
        }
    }
    paint
}

/// Where the ribbon column is and what it shows. Two frames with equal views draw the same pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct View {
    pub left_top: usize,
    pub right_top: usize,
    pub columns: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

/// Draw every ribbon that crosses the view. With a known background the image is opaque:
/// terminals show transparent image pixels over their own default background, not ours.
pub fn rasterize(blocks: &[Block], view: View, colors: &RibbonColors) -> Option<Pixmap> {
    let width = view.columns as f32 * view.cell_width as f32;
    let height = view.rows as f32 * view.cell_height as f32;
    let mut pixmap = Pixmap::new(width as u32, height as u32)?;
    if let Some([r, g, b]) = colors.background {
        pixmap.fill(Color::from_rgba8(r, g, b, 255));
    }

    let cell = view.cell_height as f32;
    let row_y = |line: usize, top: usize| (line as f32 - top as f32) * cell;
    let edge_width = (cell / 16.0).max(1.0);

    for block in blocks {
        let (l0, l1) = (row_y(block.a0, view.left_top), row_y(block.a1, view.left_top));
        let (r0, r1) = (row_y(block.b0, view.right_top), row_y(block.b1, view.right_top));
        if l1.max(r1) < -cell || l0.min(r0) > height + cell {
            continue;
        }
        let mid = width / 2.0;

        let mut band = PathBuilder::new();
        band.move_to(0.0, l0);
        band.cubic_to(mid, l0, mid, r0, width, r0);
        band.line_to(width, r1);
        band.cubic_to(mid, r1, mid, l1, 0.0, l1);
        band.close();
        if let Some(path) = band.finish() {
            let paint = ribbon_paint(block.kind(), colors, width, 95);
            pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
        }

        // Thin edges keep a zero-height side (a pure insertion or deletion point) visible.
        let mut edges = PathBuilder::new();
        let inset = edge_width / 2.0;
        edges.move_to(0.0, l0 + inset);
        edges.cubic_to(mid, l0 + inset, mid, r0 + inset, width, r0 + inset);
        edges.move_to(0.0, l1 - inset);
        edges.cubic_to(mid, l1 - inset, mid, r1 - inset, width, r1 - inset);
        if let Some(path) = edges.finish() {
            let paint = ribbon_paint(block.kind(), colors, width, 200);
            let stroke = Stroke { width: edge_width, ..Stroke::default() };
            pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }
    Some(pixmap)
}

/// Kitty graphics: transmit a PNG and place it over the ribbon column in one command.
/// PNG keeps each frame small (the ribbon column is mostly flat color). The same image id is reused,
/// so each frame replaces the last one.
pub fn kitty_place(pixmap: &Pixmap, column: u16, row: u16, view: View) -> String {
    let png = pixmap.encode_png().unwrap_or_default();
    let data = base64::engine::general_purpose::STANDARD.encode(png);
    let mut out = format!("\x1b[{};{}H", row + 1, column + 1);
    let chunks: Vec<&[u8]> = data.as_bytes().chunks(4096).collect();
    for (index, chunk) in chunks.iter().enumerate() {
        let more = u8::from(index + 1 < chunks.len());
        let payload = std::str::from_utf8(chunk).unwrap_or_default();
        if index == 0 {
            out.push_str(&format!(
                "\x1b_Ga=T,f=100,i={IMAGE_ID},p=1,c={},r={},C=1,q=2,m={more};{payload}\x1b\\",
                view.columns, view.rows,
            ));
        } else {
            // Every chunk says q=2: some terminals reply per chunk, and unread replies can stall us.
            out.push_str(&format!("\x1b_Gm={more},q=2;{payload}\x1b\\"));
        }
    }
    out
}

pub fn kitty_delete() -> String {
    format!("\x1b_Ga=d,d=I,i={IMAGE_ID},q=2\x1b\\")
}

/// Half-block fallback: one pixel column per cell, two pixel rows per cell.
/// Returns (top, bottom) colors for each cell, row by row; None is the terminal background.
pub fn half_blocks(blocks: &[Block], view: View, colors: &RibbonColors) -> Vec<Vec<(Option<Rgb>, Option<Rgb>)>> {
    let small = View { cell_width: 1, cell_height: 2, ..view };
    let Some(pixmap) = rasterize(blocks, small, colors) else {
        return Vec::new();
    };
    let pixel = |x: u32, y: u32| {
        let p = pixmap.pixel(x, y)?.demultiply();
        // Without a known background the translucent fill can't be blended, so keep only edges.
        let empty = colors.background == Some([p.red(), p.green(), p.blue()]) || p.alpha() < 200;
        (!empty).then_some([p.red(), p.green(), p.blue()])
    };
    (0..view.rows as u32)
        .map(|row| {
            (0..view.columns as u32)
                .map(|col| (pixel(col, row * 2), pixel(col, row * 2 + 1)))
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLORS: RibbonColors = RibbonColors {
        background: Some([0, 0, 0]),
        insert: [0, 255, 0],
        delete: [255, 0, 0],
    };

    fn view(rows: u16) -> View {
        View { left_top: 0, right_top: 0, columns: 6, rows, cell_width: 10, cell_height: 20 }
    }

    #[test]
    fn draws_an_insert_ribbon_and_leaves_other_rows_empty() {
        let blocks = [Block { a0: 2, a1: 2, b0: 2, b1: 5 }];
        let pixmap = rasterize(&blocks, view(8), &COLORS).unwrap();
        let at = |x, y| pixmap.pixel(x, y).unwrap();
        assert_eq!(at(30, 10).green(), 0, "row 0 has no ribbon");
        assert!(at(55, 3 * 20 + 10).green() > 60, "right side of row 3 is inside the band");
    }

    /// `RIBBON_DUMP=/path/frame.png cargo test dump_frame -- --ignored` writes one frame to look at.
    #[test]
    #[ignore]
    fn dump_frame() {
        let blocks = [
            Block { a0: 2, a1: 3, b0: 2, b1: 9 },
            Block { a0: 12, a1: 16, b0: 18, b1: 19 },
            Block { a0: 20, a1: 22, b0: 22, b1: 22 },
        ];
        let colors = RibbonColors {
            background: Some([43, 48, 59]),
            insert: [98, 186, 108],
            delete: [224, 98, 88],
        };
        let pixmap = rasterize(&blocks, view(26), &colors).unwrap();
        pixmap.save_png(std::env::var("RIBBON_DUMP").unwrap()).unwrap();
    }

    #[test]
    fn kitty_command_names_cells_and_chunks_payload() {
        // A Retina-sized frame: 6 columns x 60 rows at 18x38 px per cell.
        let big = View { left_top: 0, right_top: 0, columns: 6, rows: 60, cell_width: 18, cell_height: 38 };
        let blocks = [Block { a0: 2, a1: 3, b0: 2, b1: 40 }, Block { a0: 45, a1: 50, b0: 41, b1: 42 }];
        let colors = RibbonColors { background: Some([43, 48, 59]), insert: [0, 200, 0], delete: [200, 0, 0] };
        let pixmap = rasterize(&blocks, big, &colors).unwrap();
        let sequence = kitty_place(&pixmap, 40, 1, big);
        assert!(sequence.starts_with("\x1b[2;41H\x1b_Ga=T,f=100,i="));
        assert!(sequence.contains("c=6,r=60,C=1,q=2,m=1;"));
        assert!(sequence.ends_with("\x1b\\"));
        assert!(sequence.contains("\x1b_Gm=0,q=2;"));
        let raw = pixmap.data().len() * 4 / 3;
        assert!(sequence.len() * 10 < raw, "PNG frame {} bytes vs raw {raw}", sequence.len());
    }
}
