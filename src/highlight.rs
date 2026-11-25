//! Syntax colors with syntect.
//!
//! The default `terminal` theme colors code with the terminal's own ANSI palette, so it follows
//! the terminal's color scheme. Any syntect theme name gives that theme's RGB colors instead.

use std::path::Path;
use std::str::FromStr;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Color, FontStyle, ScopeSelectors, StyleModifier, Theme, ThemeItem, ThemeSet, ThemeSettings};
use syntect::parsing::{SyntaxReference, SyntaxSet};

pub type Rgb = [u8; 3];

/// How one piece of text is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paint {
    /// Terminal default color.
    Plain,
    /// Comments: dimmed and italic.
    Comment,
    /// An ANSI palette color, 1 to 7.
    Ansi(u8),
    /// A theme color.
    Rgb(Rgb),
}

/// Colored pieces of one line, in order, covering the line's original text.
pub type Tokens = Vec<(Paint, String)>;

const MAX_HIGHLIGHT_LINES: usize = 30_000;
pub const DEFAULT_THEME: &str = "terminal";

/// Category markers for the terminal theme, carried in the red channel: 0 plain, 1 to 7 an ANSI
/// palette color, 8 a comment.
const PLAIN: u8 = 0;
const RED: u8 = 1;
const YELLOW: u8 = 3;
const BLUE: u8 = 4;
const MAGENTA: u8 = 5;
const COMMENT: u8 = 8;

fn terminal_theme() -> Theme {
    let item = |selector: &str, marker: u8| ThemeItem {
        scope: ScopeSelectors::from_str(selector).expect("valid scope selector"),
        style: StyleModifier {
            foreground: Some(Color { r: marker, g: 0, b: 0, a: 255 }),
            background: None,
            font_style: Some(FontStyle::empty()),
        },
    };
    Theme {
        name: Some("terminal".into()),
        author: None,
        settings: ThemeSettings {
            foreground: Some(Color { r: PLAIN, g: 0, b: 0, a: 255 }),
            ..ThemeSettings::default()
        },
        scopes: vec![
            item("keyword, storage, variable.language, entity.name.tag", RED),
            item("entity.name.type, support.type, support.class, entity.name.class, entity.other.inherited-class, entity.other.attribute-name, storage.type.primitive", YELLOW),
            item("constant, variable.other.constant, support.constant, string, meta.attribute-with-value string", BLUE),
            item("entity.name.function, support.function, variable.function, meta.function-call.generic", MAGENTA),
            item("comment, punctuation.definition.comment", COMMENT),
        ],
    }
}

pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
    terminal: bool,
    /// The category theme, also used to find comments whatever theme draws the screen.
    markers: Theme,
}

impl Highlighter {
    pub fn new(theme_name: &str) -> anyhow::Result<Self> {
        // bat's syntax set: syntect's defaults lack TypeScript, TSX, TOML, and more.
        let syntaxes = two_face::syntax::extra_no_newlines();
        if theme_name == DEFAULT_THEME {
            return Ok(Highlighter { syntaxes, theme: terminal_theme(), terminal: true, markers: terminal_theme() });
        }
        let mut themes = ThemeSet::load_defaults();
        let Some(theme) = themes.themes.remove(theme_name) else {
            let mut names: Vec<_> = themes.themes.keys().cloned().collect();
            names.sort();
            anyhow::bail!("unknown theme {theme_name:?}; available: terminal, {}", names.join(", "));
        };
        Ok(Highlighter { syntaxes, theme, terminal: false, markers: terminal_theme() })
    }

    /// The theme's own background and text colors (RGB themes only).
    pub fn colors(&self) -> Option<(Rgb, Rgb)> {
        if self.terminal {
            return None;
        }
        let rgb = |c: Option<Color>, fallback: Rgb| c.map_or(fallback, |c| [c.r, c.g, c.b]);
        Some((rgb(self.theme.settings.background, [30, 30, 30]), rgb(self.theme.settings.foreground, [220, 220, 220])))
    }

    fn syntax_for(&self, path: &str, first_line: Option<&str>) -> &SyntaxReference {
        let extension = Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("");
        self.syntaxes
            .find_syntax_by_extension(extension)
            .or_else(|| {
                let name = Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("");
                self.syntaxes.find_syntax_by_extension(name)
            })
            .or_else(|| first_line.and_then(|line| self.syntaxes.find_syntax_by_first_line(line)))
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text())
    }

    fn paint(&self, color: Color) -> Paint {
        if !self.terminal {
            return Paint::Rgb([color.r, color.g, color.b]);
        }
        match color.r {
            COMMENT => Paint::Comment,
            index @ 1..=7 => Paint::Ansi(index),
            _ => Paint::Plain,
        }
    }

    /// Byte ranges of comments in each line, for `--ignore-comments`.
    pub fn comment_ranges(&self, path: &str, lines: &[String]) -> Vec<Vec<std::ops::Range<usize>>> {
        if lines.len() > MAX_HIGHLIGHT_LINES {
            return vec![Vec::new(); lines.len()];
        }
        let syntax = self.syntax_for(path, lines.first().map(String::as_str));
        let mut state = HighlightLines::new(syntax, &self.markers);
        lines
            .iter()
            .map(|line| {
                let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
                let mut offset = 0;
                for (style, text) in state.highlight_line(line, &self.syntaxes).unwrap_or_default() {
                    let end = offset + text.len();
                    if style.foreground.r == COMMENT {
                        match ranges.last_mut() {
                            Some(last) if last.end == offset => last.end = end,
                            _ => ranges.push(offset..end),
                        }
                    }
                    offset = end;
                }
                ranges
            })
            .collect()
    }

    pub fn highlight(&self, path: &str, lines: &[String]) -> Vec<Tokens> {
        let plain = |line: &String| vec![(Paint::Plain, line.clone())];
        if lines.len() > MAX_HIGHLIGHT_LINES {
            return lines.iter().map(plain).collect();
        }
        let syntax = self.syntax_for(path, lines.first().map(String::as_str));
        let mut state = HighlightLines::new(syntax, &self.theme);
        lines
            .iter()
            .map(|line| match state.highlight_line(line, &self.syntaxes) {
                Ok(pieces) => pieces.into_iter().map(|(style, text)| (self.paint(style.foreground), text.to_owned())).collect(),
                Err(_) => plain(line),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_in_use_get_a_real_syntax() {
        let h = Highlighter::new(DEFAULT_THEME).unwrap();
        for path in ["a.ts", "a.tsx", "a.js", "a.json", "a.toml", "a.rs", "a.py", "a.sql", "a.yml", "a.md"] {
            assert_ne!(h.syntax_for(path, None).name, "Plain Text", "{path}");
        }
    }

    #[test]
    fn finds_comments_in_any_theme() {
        let h = Highlighter::new("base16-ocean.dark").unwrap();
        let lines = vec!["let a = 1 // note".to_owned(), "/* block".to_owned(), "   still */ b()".to_owned()];
        let ranges = h.comment_ranges("a.ts", &lines);
        assert_eq!(&lines[0][ranges[0][0].clone()], "// note");
        assert_eq!(&lines[1][ranges[1][0].clone()], "/* block");
        assert_eq!(&lines[2][ranges[2][0].clone()], "   still */");
    }

    #[test]
    fn terminal_theme_uses_the_palette() {
        let h = Highlighter::new(DEFAULT_THEME).unwrap();
        let lines = vec!["let total: number = 'a' // note".to_owned()];
        let tokens = &h.highlight("a.ts", &lines)[0];
        let paint_of = |needle: &str| tokens.iter().find(|(_, text)| text.trim() == needle).map(|(p, _)| *p);
        assert_eq!(paint_of("let"), Some(Paint::Ansi(RED)));
        assert_eq!(paint_of("number"), Some(Paint::Ansi(YELLOW)));
        assert_eq!(paint_of("a"), Some(Paint::Ansi(BLUE)));
        assert_eq!(paint_of("note"), Some(Paint::Comment));
        assert_eq!(paint_of("total"), Some(Paint::Plain));
    }
}

#[cfg(test)]
mod timing {
    /// `RIBBON_TIME=file cargo test --release timing -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn time_highlight() {
        let path = std::env::var("RIBBON_TIME").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines = crate::model::split_lines(&text);
        let start = std::time::Instant::now();
        let h = super::Highlighter::new(super::DEFAULT_THEME).unwrap();
        println!("load: {:?}", start.elapsed());
        let start = std::time::Instant::now();
        let _ = h.highlight(&path, &lines);
        println!("highlight {} lines: {:?}", lines.len(), start.elapsed());
    }
}
