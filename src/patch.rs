//! Read a unified diff (`git diff`, `git show`, a `.patch` file) into both versions of each
//! file. With git's `index` lines, the full files come from git's object store; otherwise
//! each side is built from the patch alone, with blank lines where the patch says nothing.

/// One file in a patch.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FilePatch {
    pub old_name: String,
    pub new_name: String,
    /// Object ids from an `index` line, possibly abbreviated.
    pub old_id: Option<String>,
    pub new_id: Option<String>,
    /// Lines the patch shows of each side: zero-based line number and text.
    pub old_lines: Vec<(usize, String)>,
    pub new_lines: Vec<(usize, String)>,
    /// `/dev/null` on that side: the file was added or deleted.
    pub old_missing: bool,
    pub new_missing: bool,
    pub binary: bool,
}

impl FilePatch {
    /// Both sides from the lines the patch shows, with unknown lines left blank.
    pub fn sides(&self) -> (String, String) {
        let side = |lines: &[(usize, String)]| {
            let count = lines.iter().map(|(n, _)| n + 1).max().unwrap_or(0);
            let mut text = vec![String::new(); count];
            for (n, line) in lines {
                text[*n] = line.clone();
            }
            text.into_iter().map(|line| line + "\n").collect::<String>()
        };
        (side(&self.old_lines), side(&self.new_lines))
    }
}

/// Remove color and other escape sequences, which git adds when it writes to a pager.
pub fn strip_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            // Parameters and intermediates, then one final byte.
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            chars.next();
        }
    }
    out
}

fn strip_prefix(name: &str) -> String {
    let name = name.split('\t').next().unwrap_or(name).trim_end();
    let name = name.strip_prefix('"').and_then(|n| n.strip_suffix('"')).unwrap_or(name);
    name.strip_prefix("a/").or_else(|| name.strip_prefix("b/")).unwrap_or(name).to_owned()
}

/// `@@ -a,b +c,d @@`: zero-based starts and counts. A count of 0 has its start one line
/// before (as in `-0,0`), so it is not used.
fn hunk_header(line: &str) -> Option<(usize, usize, usize, usize)> {
    let mut parts = line.strip_prefix("@@ ")?.split(' ');
    let range = |part: Option<&str>, sign: char| -> Option<(usize, usize)> {
        let part = part?.strip_prefix(sign)?;
        let (start, count) = part.split_once(',').unwrap_or((part, "1"));
        Some((start.parse().ok()?, count.parse().ok()?))
    };
    let (a, b) = range(parts.next(), '-')?;
    let (c, d) = range(parts.next(), '+')?;
    Some((a.saturating_sub(1), b, c.saturating_sub(1), d))
}

/// Every file in a patch. Text before and between files (commit messages) is skipped.
pub fn parse(text: &str) -> Vec<FilePatch> {
    let mut files: Vec<FilePatch> = Vec::new();
    let mut lines = text.lines().peekable();
    // Lines left in the current hunk: old, new.
    let mut left: (usize, usize) = (0, 0);
    let (mut old_at, mut new_at) = (0, 0);
    while let Some(line) = lines.next() {
        if left.0 > 0 || left.1 > 0 {
            let Some(file) = files.last_mut() else { break };
            match line.chars().next() {
                Some('-') => {
                    file.old_lines.push((old_at, line[1..].to_owned()));
                    old_at += 1;
                    left.0 = left.0.saturating_sub(1);
                }
                Some('+') => {
                    file.new_lines.push((new_at, line[1..].to_owned()));
                    new_at += 1;
                    left.1 = left.1.saturating_sub(1);
                }
                Some('\\') => {}
                _ => {
                    // Context: a space, or an empty line from a tool that trims it.
                    let text = line.strip_prefix(' ').unwrap_or(line).to_owned();
                    file.old_lines.push((old_at, text.clone()));
                    file.new_lines.push((new_at, text));
                    old_at += 1;
                    new_at += 1;
                    left = (left.0.saturating_sub(1), left.1.saturating_sub(1));
                }
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // Names come from ---/+++ or rename lines; this is a fallback for files without hunks.
            let (old, new) = rest.split_once(" b/").map_or((rest, rest), |(a, b)| (a, b));
            files.push(FilePatch { old_name: strip_prefix(old), new_name: strip_prefix(new), ..Default::default() });
        } else if line.starts_with("--- ") && lines.peek().is_some_and(|next| next.starts_with("+++ ")) {
            let old = &line[4..];
            let new = &lines.next().unwrap_or_default()[4..];
            // A plain unified diff has no `diff --git` line before.
            let starts_file = files.last().is_none_or(|f| !f.old_lines.is_empty() || !f.new_lines.is_empty() || f.binary);
            if starts_file {
                files.push(FilePatch::default());
            }
            let file = files.last_mut().expect("pushed above");
            file.old_missing = old.starts_with("/dev/null");
            file.new_missing = new.starts_with("/dev/null");
            if !file.old_missing {
                file.old_name = strip_prefix(old);
            }
            if !file.new_missing {
                file.new_name = strip_prefix(new);
            }
            if file.old_missing {
                file.old_name = file.new_name.clone();
            }
            if file.new_missing {
                file.new_name = file.old_name.clone();
            }
        } else if let Some((a, b, c, d)) = hunk_header(line).filter(|_| !files.is_empty()) {
            (old_at, new_at) = (a, c);
            left = (b, d);
            // `-0,0` and `+0,0` sides are empty.
            if b == 0 {
                old_at = 0;
            }
            if d == 0 {
                new_at = 0;
            }
        } else if let Some(file) = files.last_mut() {
            if let Some(ids) = line.strip_prefix("index ") {
                let ids = ids.split(' ').next().unwrap_or("");
                if let Some((old, new)) = ids.split_once("..") {
                    file.old_id = Some(old.to_owned()).filter(|id| id.chars().any(|c| c != '0'));
                    file.new_id = Some(new.to_owned()).filter(|id| id.chars().any(|c| c != '0'));
                }
            } else if let Some(name) = line.strip_prefix("rename from ") {
                file.old_name = name.to_owned();
            } else if let Some(name) = line.strip_prefix("rename to ") {
                file.new_name = name.to_owned();
            } else if line.starts_with("new file mode") {
                file.old_missing = true;
            } else if line.starts_with("deleted file mode") {
                file.new_missing = true;
            } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
                file.binary = true;
            }
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHOW: &str = "commit 1234\nAuthor: someone\n\n    Fix the total\n\ndiff --git a/src/a.ts b/src/a.ts\nindex 1111111..2222222 100644\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -2,3 +2,3 @@ function f() {\n one\n-two\n+TWO\n three\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\nindex 0000000..3333333\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+x\n+y\n";

    #[test]
    fn reads_files_hunks_and_ids() {
        let files = parse(SHOW);
        assert_eq!(files.len(), 2);
        let a = &files[0];
        assert_eq!((a.old_name.as_str(), a.new_name.as_str()), ("src/a.ts", "src/a.ts"));
        assert_eq!((a.old_id.as_deref(), a.new_id.as_deref()), (Some("1111111"), Some("2222222")));
        assert_eq!(a.old_lines, vec![(1, "one".into()), (2, "two".into()), (3, "three".into())]);
        assert_eq!(a.new_lines, vec![(1, "one".into()), (2, "TWO".into()), (3, "three".into())]);
        assert_eq!(a.sides(), ("\none\ntwo\nthree\n".into(), "\none\nTWO\nthree\n".into()));
        let new = &files[1];
        assert!(new.old_missing && !new.new_missing);
        assert_eq!((new.old_id.as_deref(), new.new_id.as_deref()), (None, Some("3333333")));
        assert_eq!(new.sides(), (String::new(), "x\ny\n".into()));
    }

    #[test]
    fn reads_a_plain_unified_diff_and_strips_colors() {
        let colored = "\x1b[1m--- a.txt\x1b[m\n\x1b[1m+++ b.txt\x1b[m\n\x1b[36m@@ -1 +1 @@\x1b[m\n\x1b[31m-old\x1b[m\n\x1b[32m+new\x1b[m\n";
        let files = parse(&strip_escapes(colored));
        assert_eq!(files.len(), 1);
        assert_eq!((files[0].old_name.as_str(), files[0].new_name.as_str()), ("a.txt", "b.txt"));
        assert_eq!(files[0].sides(), ("old\n".into(), "new\n".into()));
    }
}
