//! Git mode: ask git which files changed for a set of `git diff` arguments, then read both
//! versions of each file from git's object store or the working tree.

use anyhow::{bail, Context};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

/// Both sides of one changed path.
pub struct Change {
    pub old_name: String,
    pub new_name: String,
    pub old: Vec<u8>,
    pub new: Vec<u8>,
}

const ZERO: &str = "0000000000000000000000000000000000000000";
const SUBMODULE_MODE: &str = "160000";

fn git(args: &[&str]) -> anyhow::Result<Vec<u8>> {
    let output = Command::new("git").args(args).output().context("cannot run git")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(output.stdout)
}

/// One record of `git diff --raw -z`.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    old_mode: String,
    new_mode: String,
    old_id: String,
    new_id: String,
    old_path: String,
    new_path: String,
}

/// Parse `:old_mode new_mode old_id new_id status NUL path NUL [new_path NUL]`.
fn parse_raw(raw: &[u8]) -> Vec<Entry> {
    let text = String::from_utf8_lossy(raw);
    let mut fields = text.split('\0').filter(|f| !f.is_empty());
    let mut entries = Vec::new();
    while let Some(meta) = fields.next() {
        let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let [old_mode, new_mode, old_id, new_id, status] = parts[..] else { continue };
        let Some(old_path) = fields.next() else { break };
        let renamed = status.starts_with('R') || status.starts_with('C');
        let new_path = if renamed { fields.next().unwrap_or(old_path) } else { old_path };
        entries.push(Entry {
            old_mode: old_mode.into(),
            new_mode: new_mode.into(),
            old_id: old_id.into(),
            new_id: new_id.into(),
            old_path: old_path.into(),
            new_path: new_path.into(),
        });
    }
    entries
}

/// Read many blobs with one `git cat-file --batch`. A writer thread feeds the ids, so a large
/// reply can't fill the pipe while ids are still being written.
fn read_blobs(ids: Vec<String>) -> anyhow::Result<HashMap<String, Vec<u8>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut child = Command::new("git")
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("cannot run git cat-file")?;
    let mut stdin = child.stdin.take().context("git cat-file stdin")?;
    let request = ids.iter().map(|id| format!("{id}\n")).collect::<String>();
    let writer = thread::spawn(move || stdin.write_all(request.as_bytes()));
    let mut out = Vec::new();
    child.stdout.take().context("git cat-file stdout")?.read_to_end(&mut out)?;
    writer.join().map_err(|_| anyhow::anyhow!("git cat-file writer failed"))??;
    child.wait()?;

    let mut blobs = HashMap::new();
    let mut rest = &out[..];
    for id in ids {
        let Some(end) = rest.iter().position(|&b| b == b'\n') else { break };
        let header = String::from_utf8_lossy(&rest[..end]).into_owned();
        rest = &rest[end + 1..];
        let size: Option<usize> = header.split(' ').nth(2).and_then(|s| s.parse().ok());
        let Some(size) = size else { continue }; // "<id> missing"
        blobs.insert(id, rest[..size.min(rest.len())].to_vec());
        rest = &rest[(size + 1).min(rest.len())..];
    }
    Ok(blobs)
}

/// The changed files for `git diff <args>`, with both versions read into memory.
pub fn changes(args: &[String]) -> anyhow::Result<Vec<Change>> {
    let root = PathBuf::from(String::from_utf8_lossy(&git(&["rev-parse", "--show-toplevel"])?).trim());
    // Our format options come first so the user's pathspecs, after "--", stay last.
    let mut command = vec!["-c", "core.quotepath=false", "diff", "--raw", "-z", "--no-abbrev", "-M", "--no-ext-diff", "--no-textconv", "--no-color"];
    command.extend(args.iter().map(String::as_str));
    let entries: Vec<Entry> = parse_raw(&git(&command)?)
        .into_iter()
        .filter(|e| e.old_mode != SUBMODULE_MODE && e.new_mode != SUBMODULE_MODE)
        .collect();

    let ids = entries
        .iter()
        .flat_map(|e| [&e.old_id, &e.new_id])
        .filter(|id| id.as_str() != ZERO)
        .cloned()
        .collect();
    let blobs = read_blobs(ids)?;
    let blob = |id: &str| blobs.get(id).cloned().unwrap_or_default();
    // A zero id on the new side means the working tree (or nothing, for a deleted file).
    let worktree = |path: &str| std::fs::read(root.join(Path::new(path))).unwrap_or_default();

    Ok(entries
        .into_iter()
        .map(|e| Change {
            old: if e.old_id == ZERO { Vec::new() } else { blob(&e.old_id) },
            new: if e.new_id != ZERO { blob(&e.new_id) } else if e.new_mode == "000000" { Vec::new() } else { worktree(&e.new_path) },
            old_name: e.old_path,
            new_name: e.new_path,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modified_and_renamed_entries() {
        let raw = b":100644 100644 aaaa bbbb M\0src/a.ts\0:100644 100644 cccc dddd R087\0old/b.ts\0new/b.ts\0";
        let entries = parse_raw(raw);
        assert_eq!(entries.len(), 2);
        assert_eq!((entries[0].old_path.as_str(), entries[0].new_path.as_str()), ("src/a.ts", "src/a.ts"));
        assert_eq!((entries[1].old_path.as_str(), entries[1].new_path.as_str()), ("old/b.ts", "new/b.ts"));
        assert_eq!(entries[1].new_id, "dddd");
    }
}
