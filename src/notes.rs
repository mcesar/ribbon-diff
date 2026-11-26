//! Notes on lines of the diff: the user's notes for Claude in `.ribbon/notes.json`, and notes
//! from an agent (`--agent-context`). Both files use Hunk's agent-context format:
//!
//! ```json
//! { "version": 1, "files": [ { "path": "src/a.ts", "annotations": [
//!     { "newRange": [12, 14], "summary": "...", "rationale": "...", "author": "claude" } ] } ] }
//! ```
//!
//! Ranges are 1-based and inclusive; `oldRange` puts a note on the old side. Ribbon's own
//! notes add `id`, `status` ("open" or "done"), `reply`, and `anchor` (the text of the range's
//! last line, to find the line again after edits move it). Hunk ignores those fields.

use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub id: Option<String>,
    pub path: String,
    pub old_side: bool,
    /// 1-based, inclusive.
    pub range: (usize, usize),
    pub anchor: Option<String>,
    pub summary: String,
    pub rationale: Option<String>,
    pub author: Option<String>,
    pub status: Option<String>,
    pub reply: Option<String>,
    /// From the notes file, so it can be deleted. Notes from `--agent-context` can't.
    pub editable: bool,
}

impl Note {
    /// A note from the person that is not done yet. Claude's own notes explain its changes
    /// and are never open.
    pub fn is_open(&self) -> bool {
        self.editable && self.is_from_person() && self.status.as_deref().unwrap_or("open") == "open"
    }

    /// The status to show: the one written, or "open" for the person's notes without one.
    pub fn state(&self) -> Option<&str> {
        self.status.as_deref().or((self.editable && self.is_from_person()).then_some("open"))
    }

    fn is_from_person(&self) -> bool {
        self.author.as_deref().unwrap_or("you") == "you"
    }
}

/// `.ribbon/notes.json` under `root`.
pub fn notes_file(root: &Path) -> PathBuf {
    root.join(".ribbon").join("notes.json")
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty()).map(str::to_owned)
}

fn range(value: &Value, key: &str) -> anyhow::Result<Option<(usize, usize)>> {
    let Some(range) = value.get(key) else { return Ok(None) };
    let numbers: Option<Vec<usize>> = range.as_array().and_then(|a| a.iter().map(|n| n.as_u64().map(|n| n as usize)).collect());
    match numbers.as_deref() {
        Some(&[start, end]) if start >= 1 && start <= end => Ok(Some((start, end))),
        _ => bail!("{key} must be [start, end] with 1-based line numbers"),
    }
}

/// The notes in a file's text.
pub fn parse(text_of_file: &str, editable: bool) -> anyhow::Result<Vec<Note>> {
    let root: Value = serde_json::from_str(text_of_file).context("not valid JSON")?;
    let mut notes = Vec::new();
    for file in root.get("files").and_then(Value::as_array).into_iter().flatten() {
        let path = text(file, "path").context("each file needs a path")?;
        for annotation in file.get("annotations").and_then(Value::as_array).into_iter().flatten() {
            let summary = text(annotation, "summary").context("each annotation needs a summary")?;
            // A note with both ranges sits on the new side.
            let (old_side, range) = match (range(annotation, "newRange")?, range(annotation, "oldRange")?) {
                (Some(new), _) => (false, new),
                (None, Some(old)) => (true, old),
                (None, None) => (false, (1, 1)),
            };
            notes.push(Note {
                id: text(annotation, "id"),
                path: path.clone(),
                old_side,
                range,
                anchor: annotation.get("anchor").and_then(Value::as_str).map(str::to_owned),
                summary,
                rationale: text(annotation, "rationale"),
                author: text(annotation, "author"),
                status: text(annotation, "status"),
                reply: text(annotation, "reply"),
                editable,
            });
        }
    }
    Ok(notes)
}

pub fn load(path: &Path, editable: bool) -> anyhow::Result<Vec<Note>> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text, editable).with_context(|| format!("cannot read notes in {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Change the notes file. Reads it again first, so changes another program (Claude) made
/// since are kept, and refuses to touch a file it can't parse. Writes a temporary file and
/// renames it, so a reader never sees half a file.
fn change(path: &Path, edit: impl FnOnce(&mut Value)) -> anyhow::Result<()> {
    let mut root = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON; fix it or move it away", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({ "version": 1, "files": [] }),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    if !root.is_object() {
        bail!("{} is not a JSON object", path.display());
    }
    edit(&mut root);
    let dir = path.parent().context("notes file has no folder")?;
    std::fs::create_dir_all(dir)?;
    // The folder ignores itself, so git status and --untracked don't show it.
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(&ignore, "*\n")?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_string_pretty(&root)? + "\n")?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

pub fn add(path: &Path, note: &Note) -> anyhow::Result<()> {
    let mut annotation = json!({
        "id": note.id,
        (if note.old_side { "oldRange" } else { "newRange" }): [note.range.0, note.range.1],
        "anchor": note.anchor,
        "summary": note.summary,
        "author": note.author,
        "status": note.status,
    });
    if let Some(object) = annotation.as_object_mut() {
        object.retain(|_, value| !value.is_null());
    }
    change(path, |root| {
        if !root["files"].is_array() {
            root["files"] = json!([]);
        }
        let Some(files) = root["files"].as_array_mut() else { return };
        match files.iter_mut().find(|f| f.get("path").and_then(Value::as_str) == Some(&note.path)) {
            Some(file) => {
                if !file["annotations"].is_array() {
                    file["annotations"] = json!([]);
                }
                if let Some(list) = file["annotations"].as_array_mut() {
                    list.push(annotation);
                }
            }
            None => files.push(json!({ "path": note.path, "annotations": [annotation] })),
        }
    })
}

pub fn remove(path: &Path, id: &str) -> anyhow::Result<()> {
    change(path, |root| {
        if let Some(files) = root.get_mut("files").and_then(Value::as_array_mut) {
            for file in files.iter_mut() {
                if let Some(list) = file.get_mut("annotations").and_then(Value::as_array_mut) {
                    list.retain(|a| a.get("id").and_then(Value::as_str) != Some(id));
                }
            }
            files.retain(|f| f.get("annotations").and_then(Value::as_array).is_some_and(|a| !a.is_empty()));
        }
    })
}

/// A new note id: the time in milliseconds, which is unique enough for one person's notes.
pub fn new_id() -> String {
    let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    format!("n{millis}")
}

/// The zero-based line a note sits under: the last line of its range, or, when that line's
/// text is no longer its anchor, the nearest line that has that text.
pub fn place(note: &Note, lines: &[String]) -> Option<usize> {
    if lines.is_empty() {
        return None;
    }
    let line = (note.range.1 - 1).min(lines.len() - 1);
    match &note.anchor {
        Some(anchor) if lines[line] != *anchor => {
            let found = lines.iter().enumerate().filter(|(_, l)| *l == anchor).map(|(i, _)| i).min_by_key(|i| i.abs_diff(line));
            Some(found.unwrap_or(line))
        }
        _ => Some(line),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Title,
    Summary,
    Rationale,
    Reply,
}

/// Break text into rows of at most `width` columns, at spaces when it can.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(4);
    let mut rows = Vec::new();
    for paragraph in text.split('\n') {
        let mut row = String::new();
        let mut used = 0;
        for word in paragraph.split(' ') {
            let word_width: usize = word.chars().map(|c| c.width().unwrap_or(0)).sum();
            if used > 0 && used + 1 + word_width > width {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            if used > 0 {
                row.push(' ');
                used += 1;
            }
            for c in word.chars() {
                let w = c.width().unwrap_or(0);
                if used + w > width {
                    rows.push(std::mem::take(&mut row));
                    used = 0;
                }
                row.push(c);
                used += w;
            }
        }
        rows.push(row);
    }
    rows
}

/// The rows a note takes on screen, `width` columns wide.
pub fn rows(note: &Note, width: usize) -> Vec<(Part, String)> {
    let mut title = note.author.clone().unwrap_or_else(|| "note".into());
    if let Some(state) = note.state() {
        title.push_str(&format!(" · {state}"));
    }
    if note.range.0 != note.range.1 {
        title.push_str(&format!(" · lines {}–{}", note.range.0, note.range.1));
    }
    let mut rows = vec![(Part::Title, title)];
    rows.extend(wrap(&note.summary, width).into_iter().map(|r| (Part::Summary, r)));
    if let Some(rationale) = &note.rationale {
        rows.extend(wrap(rationale, width).into_iter().map(|r| (Part::Rationale, r)));
    }
    if let Some(reply) = &note.reply {
        let mut reply_rows = wrap(reply, width.saturating_sub(2)).into_iter();
        if let Some(first) = reply_rows.next() {
            rows.push((Part::Reply, format!("↳ {first}")));
        }
        rows.extend(reply_rows.map(|r| (Part::Reply, format!("  {r}"))));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    const HUNK_EXAMPLE: &str = r#"{
      "version": 1,
      "summary": "Improves matching.",
      "files": [
        { "path": "src/search.ts", "summary": "Scores matches.",
          "annotations": [
            { "newRange": [15, 35], "summary": "Prefix matches rank first.", "rationale": "The top result should be the obvious one.", "author": "sonnet" },
            { "oldRange": [3, 4], "summary": "Old loop removed." } ] } ] }"#;

    #[test]
    fn reads_hunk_agent_context() {
        let notes = parse(HUNK_EXAMPLE, false).unwrap();
        assert_eq!(notes.len(), 2);
        assert_eq!((notes[0].path.as_str(), notes[0].old_side, notes[0].range), ("src/search.ts", false, (15, 35)));
        assert_eq!(notes[0].author.as_deref(), Some("sonnet"));
        assert_eq!(notes[0].rationale.as_deref(), Some("The top result should be the obvious one."));
        assert_eq!((notes[1].old_side, notes[1].range), (true, (3, 4)));
        assert!(!notes[0].is_open());
        assert!(parse(r#"{"files":[{"path":"a","annotations":[{"newRange":[0,1],"summary":"x"}]}]}"#, true).is_err());
    }

    #[test]
    fn adds_and_removes_notes_and_keeps_other_fields() {
        let dir = std::env::temp_dir().join(format!("ribbon-notes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = notes_file(&dir);
        let note = Note {
            id: Some("n1".into()),
            path: "src/a.ts".into(),
            old_side: false,
            range: (3, 4),
            anchor: Some("  return x".into()),
            summary: "Use a constant".into(),
            rationale: None,
            author: Some("you".into()),
            status: Some("open".into()),
            reply: None,
            editable: true,
        };
        add(&path, &note).unwrap();
        // Claude marks it done and adds a field ribbon doesn't know.
        let text = std::fs::read_to_string(&path).unwrap().replace("\"open\"", "\"done\", \"reply\": \"Done.\", \"extra\": 1");
        std::fs::write(&path, text).unwrap();
        add(&path, &Note { id: Some("n2".into()), range: (9, 9), ..note.clone() }).unwrap();
        let read = load(&path, true).unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!((read[0].status.as_deref(), read[0].reply.as_deref()), (Some("done"), Some("Done.")));
        assert!(std::fs::read_to_string(&path).unwrap().contains("\"extra\": 1"));
        assert_eq!(std::fs::read_to_string(dir.join(".ribbon/.gitignore")).unwrap(), "*\n");
        remove(&path, "n1").unwrap();
        assert_eq!(load(&path, true).unwrap().iter().map(|n| n.id.clone().unwrap()).collect::<Vec<_>>(), ["n2"]);
        // A broken file is never overwritten.
        std::fs::write(&path, "{ broken").unwrap();
        assert!(add(&path, &note).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ broken");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn places_notes_by_their_anchor_text() {
        let lines: Vec<String> = ["a", "b", "target", "c", "target"].iter().map(|s| s.to_string()).collect();
        let mut note = parse(r#"{"files":[{"path":"f","annotations":[{"newRange":[1,2],"summary":"s","anchor":"target"}]}]}"#, true).unwrap().remove(0);
        // Line 2 is "b" now: the nearest "target" is line index 2.
        assert_eq!(place(&note, &lines), Some(2));
        note.anchor = None;
        assert_eq!(place(&note, &lines), Some(1));
        note.range = (40, 50);
        assert_eq!(place(&note, &lines), Some(4));
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
    }
}
