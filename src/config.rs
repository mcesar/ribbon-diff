//! The config file, `~/.config/ribbon/config.toml` (or under `$XDG_CONFIG_HOME`):
//!
//! ```toml
//! context = 5               # the same names as the long options
//! ignore-comments = true
//! watch = "500ms"           # or true
//! layout = "auto"
//! scroll-off = 4
//!
//! [keys]                    # a key = the key it acts as
//! "ctrl-d" = "pagedown"
//! "K" = "{"
//! ```
//!
//! Options on the command line win over the file.

use anyhow::{bail, Context};
use crossterm::event::{KeyCode, KeyModifiers};
use std::path::PathBuf;

/// A key and its modifiers.
pub type Key = (KeyCode, KeyModifiers);

pub fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("ribbon").join("config.toml"))
}

/// The config file's table, or an empty one when there is no file.
pub fn load() -> anyhow::Result<toml::Table> {
    let Some(path) = path() else { return Ok(toml::Table::new()) };
    match std::fs::read_to_string(&path) {
        Ok(text) => text.parse::<toml::Table>().with_context(|| format!("cannot read {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// A key name: one character (`x`, `{`), a name (`space`, `enter`, `tab`, `esc`, `backspace`,
/// `up`, `down`, `left`, `right`, `pageup`, `pagedown`, `home`, `end`), with `ctrl-` or
/// `alt-` in front if needed.
pub fn parse_key(spec: &str) -> anyhow::Result<Key> {
    let mut modifiers = KeyModifiers::NONE;
    let mut rest = spec;
    loop {
        if let Some(r) = rest.strip_prefix("ctrl-").filter(|r| !r.is_empty()) {
            modifiers |= KeyModifiers::CONTROL;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("alt-").filter(|r| !r.is_empty()) {
            modifiers |= KeyModifiers::ALT;
            rest = r;
        } else {
            break;
        }
    }
    let code = match rest {
        "space" => KeyCode::Char(' '),
        "enter" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "esc" => KeyCode::Esc,
        "backspace" => KeyCode::Backspace,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        _ => {
            let mut chars = rest.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => KeyCode::Char(c),
                _ => bail!("unknown key {spec:?}"),
            }
        }
    };
    Ok((code, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_key_names() {
        assert_eq!(parse_key("x").unwrap(), (KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(parse_key("ctrl-d").unwrap(), (KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(parse_key("pagedown").unwrap(), (KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(parse_key("-").unwrap(), (KeyCode::Char('-'), KeyModifiers::NONE));
        assert!(parse_key("nope").is_err());
    }
}
