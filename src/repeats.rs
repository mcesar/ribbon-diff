//! The same edit made in many places, such as `fetch(url)` becoming `fetch(url, ctx)` in 30
//! calls: changes whose removed and added words are the same, wherever they are.

use crate::model::Model;
use std::collections::HashMap;

/// One edit and every change that makes it, in stream order: (file, block index).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub places: Vec<(usize, usize)>,
    /// A short description, such as "adds `, ctx`" or "`old` → `new`".
    pub what: String,
}

/// An edit's text: for each changed line pair, the removed and the added words (whole lines,
/// without spacing, when a line has no word changes or the line counts differ).
fn signature(model: &Model, block: &crate::model::Block) -> Vec<(String, String)> {
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    // Word ranges next to each other (or apart only by spaces) are one piece of the edit.
    let words = |line: &str, ranges: &[std::ops::Range<usize>]| {
        let mut pieces: Vec<std::ops::Range<usize>> = Vec::new();
        for range in ranges {
            match pieces.last_mut() {
                Some(last) if line.get(last.end..range.start).is_some_and(|gap| gap.trim().is_empty()) => last.end = range.end,
                _ => pieces.push(range.clone()),
            }
        }
        pieces.iter().filter_map(|r| line.get(r.clone())).map(squash).collect::<Vec<_>>().join(" … ")
    };
    let (old_count, new_count) = (block.a1 - block.a0, block.b1 - block.b0);
    if old_count == new_count {
        // The same edit on several lines of one change is still that one edit.
        let mut pairs: Vec<(String, String)> = Vec::new();
        for pair in (0..old_count)
            .map(|i| {
                let (old, new) = (block.a0 + i, block.b0 + i);
                let (old_ranges, new_ranges) = (&model.old_inline[old], &model.new_inline[new]);
                if old_ranges.is_empty() && new_ranges.is_empty() {
                    (squash(&model.old[old]), squash(&model.new[new]))
                } else {
                    (words(&model.old[old], old_ranges), words(&model.new[new], new_ranges))
                }
            })
        {
            if !pairs.contains(&pair) {
                pairs.push(pair);
            }
        }
        pairs
    } else {
        let lines = |lines: &[String]| lines.iter().map(|l| squash(l)).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n");
        vec![(lines(&model.old[block.a0..block.a1]), lines(&model.new[block.b0..block.b1]))]
    }
}

fn describe(signature: &[(String, String)]) -> String {
    let short = |s: &str| {
        let line = s.lines().next().unwrap_or("");
        if line.chars().count() > 40 { format!("{}…", line.chars().take(40).collect::<String>()) } else { line.to_owned() }
    };
    let (old, new) = &signature[0];
    let first = match (old.is_empty(), new.is_empty()) {
        (true, _) => format!("adds `{}`", short(new)),
        (_, true) => format!("removes `{}`", short(old)),
        _ => format!("`{}` → `{}`", short(old), short(new)),
    };
    match signature.len() {
        1 => first,
        2 => format!("{first} (and one more edit)"),
        n => format!("{first} (and {} more edits)", n - 1),
    }
}

/// Edits made in more than one place. `skip(file, block)` leaves out changes that are
/// already explained, such as moved code.
pub fn find(models: &[&Model], skip: impl Fn(usize, usize) -> bool) -> Vec<Group> {
    let mut groups: HashMap<Vec<(String, String)>, Vec<(usize, usize)>> = HashMap::new();
    let mut order = Vec::new();
    for (file, model) in models.iter().enumerate() {
        for (index, block) in model.blocks.iter().enumerate() {
            if skip(file, index) {
                continue;
            }
            let key = signature(model, block);
            // Too little text to tell one edit from a coincidence, like `1` → `2`.
            let letters: usize = key.iter().map(|(old, new)| old.chars().chain(new.chars()).filter(|c| !c.is_whitespace()).count()).sum();
            if letters < 4 {
                continue;
            }
            let places = groups.entry(key.clone()).or_default();
            if places.is_empty() {
                order.push(key);
            }
            places.push((file, index));
        }
    }
    order
        .into_iter()
        .filter_map(|key| {
            let places = groups.remove(&key)?;
            (places.len() > 1).then(|| Group { what: describe(&key), places })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_the_same_edit_in_different_lines_and_files() {
        let a = Model::new("let a = fetch(url);\nkeep\nreturn fetch(url);\n", "let a = fetch(url, ctx);\nkeep\nreturn fetch(url, ctx);\n");
        // Two lines in one change, each with the edit: one more place, not a new edit.
        let b = Model::new("x\nfoo(fetch(url))\nbar(fetch(url))\ny\n", "x\nfoo(fetch(url, ctx))\nbar(fetch(url, ctx))\ny\n");
        let c = Model::new("count = 1\n", "count = 2\n");
        let groups = find(&[&a, &b, &c], |_, _| false);
        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(groups[0].places, [(0, 0), (0, 1), (1, 0)]);
        assert_eq!(groups[0].what, "adds `, ctx`");
        // Left out (moved code, for example), the rest is still a group of two.
        assert_eq!(find(&[&a, &b], |file, _| file == 1)[0].places, [(0, 0), (0, 1)]);
        assert!(find(&[&a, &b], |file, block| file == 1 || block == 1).is_empty());
    }

    #[test]
    fn whole_lines_added_in_many_files() {
        let a = Model::new("use std::io;\nfn a() {}\n", "use std::io;\nuse tracing::info;\nfn a() {}\n");
        let b = Model::new("fn b() {}\n", "use tracing::info;\nfn b() {}\n");
        let groups = find(&[&a, &b], |_, _| false);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].what, "adds `use tracing::info;`");
    }
}
