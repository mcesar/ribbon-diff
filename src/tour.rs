//! A guided tour of a change: stops in the order to read them, each with a sentence, written
//! by Claude in `.ribbon/tour.json`:
//!
//! ```json
//! { "version": 1, "title": "How the retry works",
//!   "stops": [ { "path": "src/upload.rs", "newRange": [40, 52], "text": "Start here: ..." } ] }
//! ```

use anyhow::{bail, Context};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stop {
    pub path: String,
    pub old_side: bool,
    /// 1-based, inclusive.
    pub range: (usize, usize),
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tour {
    pub title: Option<String>,
    pub stops: Vec<Stop>,
}

/// `.ribbon/tour.json` next to the notes file.
pub fn tour_file(notes_file: &Path) -> PathBuf {
    notes_file.with_file_name("tour.json")
}

pub fn parse(text: &str) -> anyhow::Result<Tour> {
    let root: Value = serde_json::from_str(text).context("not valid JSON")?;
    let mut tour = Tour { title: root["title"].as_str().map(str::to_owned), stops: Vec::new() };
    for (number, stop) in root["stops"].as_array().context("needs \"stops\": [...]")?.iter().enumerate() {
        let path = stop["path"].as_str().with_context(|| format!("stop {} needs a path", number + 1))?.to_owned();
        let range = |key: &str| -> Option<(usize, usize)> {
            let pair = stop[key].as_array()?;
            let (a, b) = (pair.first()?.as_u64()? as usize, pair.get(1)?.as_u64()? as usize);
            (a >= 1 && a <= b).then_some((a, b))
        };
        let (old_side, range) = match (range("newRange"), range("oldRange")) {
            (Some(new), _) => (false, new),
            (None, Some(old)) => (true, old),
            (None, None) => bail!("stop {} needs \"newRange\": [first, last] (1-based) or \"oldRange\"", number + 1),
        };
        let text = stop["text"].as_str().unwrap_or("").trim().to_owned();
        tour.stops.push(Stop { path, old_side, range, text });
    }
    Ok(tour)
}

/// The tour in a file: None when there is no file.
pub fn load(path: &Path) -> Option<anyhow::Result<Tour>> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(parse(&text).with_context(|| format!("cannot read the tour in {}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_stops() {
        let tour = parse(r#"{"title":"T","stops":[{"path":"a.rs","newRange":[3,5],"text":"Start."},{"path":"b.rs","oldRange":[1,1],"text":"Gone."}]}"#).unwrap();
        assert_eq!(tour.title.as_deref(), Some("T"));
        assert_eq!(tour.stops[0], Stop { path: "a.rs".into(), old_side: false, range: (3, 5), text: "Start.".into() });
        assert!(tour.stops[1].old_side);
        assert!(parse(r#"{"stops":[{"path":"a.rs"}]}"#).is_err());
        assert!(parse(r#"{"stops":[{"path":"a.rs","newRange":[5,3]}]}"#).is_err());
    }
}
