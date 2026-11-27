//! Who wrote the lines a change removes, and when: `git blame` on the old side, run in the
//! background. Removing code that was added recently, often as a fix, is worth a second look.

use std::collections::HashMap;
use std::io::Write;
use std::ops::Range;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub id: String,
    pub author: String,
    /// Seconds since 1970.
    pub time: u64,
    pub summary: String,
}

impl Commit {
    /// Lines that are only in the index or the working tree have no commit yet.
    pub fn is_committed(&self) -> bool {
        !self.id.chars().all(|c| c == '0')
    }

    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(8)]
    }
}

/// The commit that wrote each line, by zero-based line, for the lines asked about.
#[derive(Debug, Default, Clone)]
pub struct Blame {
    pub lines: HashMap<usize, String>,
    pub commits: HashMap<String, Commit>,
}

/// Read `git blame --porcelain` output.
pub fn parse_porcelain(text: &str) -> Blame {
    let mut blame = Blame::default();
    let mut current: Option<(String, usize)> = None;
    let mut pending: Option<Commit> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('\t') {
            let _ = rest;
            if let Some(commit) = pending.take() {
                blame.commits.entry(commit.id.clone()).or_insert(commit);
            }
            if let Some((id, final_line)) = current.take() {
                blame.lines.insert(final_line - 1, id);
            }
            continue;
        }
        let mut words = line.split(' ');
        let first = words.next().unwrap_or("");
        if first.len() == 40 && first.chars().all(|c| c.is_ascii_hexdigit()) {
            let final_line = words.nth(1).and_then(|n| n.parse().ok()).unwrap_or(1);
            current = Some((first.to_owned(), final_line));
            if !blame.commits.contains_key(first) {
                pending = Some(Commit { id: first.to_owned(), author: String::new(), time: 0, summary: String::new() });
            }
            continue;
        }
        let Some(commit) = pending.as_mut() else { continue };
        let value = line.split_once(' ').map_or("", |(_, v)| v);
        match first {
            "author" => commit.author = value.to_owned(),
            "author-time" => commit.time = value.parse().unwrap_or(0),
            "summary" => commit.summary = value.to_owned(),
            _ => {}
        }
    }
    blame
}

/// Blame some lines of the old version of a file. `text` is that version, so the line numbers
/// match it even when it is the index; `revision` is the commit it came from.
pub fn blame(path: &str, text: &str, revision: &str, ranges: &[Range<usize>]) -> Option<Blame> {
    if ranges.is_empty() {
        return None;
    }
    let mut command = Command::new("git");
    command.args(["blame", "--porcelain", "--contents", "-"]);
    for range in ranges {
        command.arg("-L").arg(format!("{},{}", range.start + 1, range.end));
    }
    command.arg(revision).arg("--").arg(path);
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut stdin = child.stdin.take()?;
    let text = text.to_owned();
    // Write from another thread: git can fill its output pipe before it reads all its input.
    let writer = std::thread::spawn(move || stdin.write_all(text.as_bytes()));
    let out = child.wait_with_output().ok()?;
    let _ = writer.join();
    out.status.success().then(|| parse_porcelain(&String::from_utf8_lossy(&out.stdout)))
}

/// "9 days ago", "3 months ago".
pub fn age(time: u64, now: u64) -> String {
    let days = now.saturating_sub(time) / 86_400;
    match days {
        0 => "today".to_owned(),
        1 => "yesterday".to_owned(),
        2..=59 => format!("{days} days ago"),
        60..=729 => format!("{} months ago", days / 30),
        _ => format!("{} years ago", days / 365),
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_porcelain_output() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let text = format!(
            "{a} 3 5 2\nauthor Ana\nauthor-time 1000\nsummary Fix the lock\nfilename x\n\tline five\n{a} 4 6\n\tline six\n{b} 1 9 1\nauthor Bo\nauthor-time 2000\nsummary Start\nfilename x\n\tline nine\n"
        );
        let blame = parse_porcelain(&text);
        assert_eq!(blame.lines.get(&4), Some(&a));
        assert_eq!(blame.lines.get(&5), Some(&a));
        assert_eq!(blame.lines.get(&8), Some(&b));
        assert_eq!(blame.commits[&a].summary, "Fix the lock");
        assert_eq!(blame.commits[&b].author, "Bo");
        assert_eq!(age(0, 9 * 86_400), "9 days ago");
        assert_eq!(age(0, 400 * 86_400), "13 months ago");
    }

    #[test]
    fn blames_this_repository() {
        // This file's first line, as committed: written by some commit.
        let Ok(text) = std::fs::read_to_string("src/main.rs") else { return };
        let Some(blame) = blame("src/main.rs", &text, "HEAD", &[0..1]) else { return };
        assert!(blame.lines.contains_key(&0));
    }
}
