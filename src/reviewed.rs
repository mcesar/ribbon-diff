//! Changes the person marked as reviewed, kept by a hash of their content in
//! `.ribbon/reviewed.json`. A change that is the same after a new push keeps its mark; a
//! change whose lines differ in any way is a new change and shows as not reviewed.

use anyhow::Context;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// `.ribbon/reviewed.json` next to the notes file.
pub fn reviewed_file(notes_file: &Path) -> PathBuf {
    notes_file.with_file_name("reviewed.json")
}

/// The hash of one change: its path, its removed lines, and its added lines. FNV-1a, so it is
/// the same in every build.
pub fn hunk_hash(path: &str, old: &[String], new: &[String]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            hash = (hash ^ b as u64).wrapping_mul(0x100000001b3);
        }
    };
    feed(path.as_bytes());
    for (mark, lines) in [(b"\x00-", old), (b"\x00+", new)] {
        feed(mark);
        for line in lines {
            feed(line.as_bytes());
            feed(b"\n");
        }
    }
    format!("{hash:016x}")
}

pub fn load(path: &Path) -> HashSet<String> {
    let Ok(text) = std::fs::read_to_string(path) else { return HashSet::new() };
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    value["reviewed"].as_array().into_iter().flatten().filter_map(|r| r["hash"].as_str().map(str::to_owned)).collect()
}

/// Mark changes reviewed, or not. Reads the file again first, so marks made by another ribbon
/// are kept.
pub fn set(path: &Path, changes: &[(String, String)], reviewed: bool) -> anyhow::Result<()> {
    let mut root: Value = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON; fix it or move it away", path.display()))?,
        Err(_) => json!({ "version": 1, "reviewed": [] }),
    };
    if !root["reviewed"].is_array() {
        root["reviewed"] = json!([]);
    }
    let hashes: HashSet<&str> = changes.iter().map(|(hash, _)| hash.as_str()).collect();
    let list = root["reviewed"].as_array_mut().context("reviewed is not a list")?;
    list.retain(|r| !r["hash"].as_str().is_some_and(|h| hashes.contains(h)));
    if reviewed {
        let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        list.extend(changes.iter().map(|(hash, path)| json!({ "hash": hash, "path": path, "at": at })));
    }
    let dir = path.parent().context("no folder for the reviewed file")?;
    std::fs::create_dir_all(dir)?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(&ignore, "*\n")?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_string_pretty(&root)? + "\n")?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_survive_by_content() {
        let lines = |text: &str| text.lines().map(str::to_owned).collect::<Vec<_>>();
        let a = hunk_hash("a.rs", &lines("x"), &lines("y"));
        assert_eq!(a, hunk_hash("a.rs", &lines("x"), &lines("y")));
        assert_ne!(a, hunk_hash("a.rs", &lines("x"), &lines("y2")));
        assert_ne!(a, hunk_hash("b.rs", &lines("x"), &lines("y")));
        assert_ne!(hunk_hash("a.rs", &lines("x\ny"), &[]), hunk_hash("a.rs", &lines("x"), &lines("y")));

        let dir = PathBuf::from(format!("/tmp/ribbon-reviewed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = reviewed_file(&dir.join(".ribbon/notes.json"));
        set(&file, &[(a.clone(), "a.rs".into()), ("b".into(), "b.rs".into())], true).unwrap();
        assert_eq!(load(&file), HashSet::from([a.clone(), "b".to_owned()]));
        set(&file, &[(a.clone(), "a.rs".into())], false).unwrap();
        assert_eq!(load(&file), HashSet::from(["b".to_owned()]));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
