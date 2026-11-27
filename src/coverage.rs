//! Test coverage from an lcov file (`lcov.info`, which most test runners can write): which
//! lines ran in the tests. Ribbon marks the added lines that no test ran.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Hits per line (1-based), by the path the lcov file names.
#[derive(Debug, Default)]
pub struct Coverage {
    pub files: HashMap<String, HashMap<usize, u64>>,
    /// When the lcov file was written, to tell when it is older than the changes.
    pub written: Option<std::time::SystemTime>,
}

pub fn parse(text: &str) -> Coverage {
    let mut coverage = Coverage::default();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("SF:") {
            current = Some(path.trim().to_owned());
        } else if let Some(data) = line.strip_prefix("DA:") {
            let mut parts = data.split(',');
            let (Some(number), Some(hits)) = (parts.next().and_then(|n| n.trim().parse().ok()), parts.next().and_then(|h| h.trim().parse().ok())) else { continue };
            if let Some(path) = &current {
                coverage.files.entry(path.clone()).or_default().insert(number, hits);
            }
        } else if line.trim() == "end_of_record" {
            current = None;
        }
    }
    coverage
}

pub fn load(path: &Path) -> anyhow::Result<Coverage> {
    let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("cannot read coverage {}: {e}", path.display()))?;
    let mut coverage = parse(&text);
    coverage.written = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    Ok(coverage)
}

/// The lcov file to use: the one given, or `coverage/lcov.info` or `lcov.info` at the top of
/// the repository when one exists.
pub fn find(given: Option<&Path>, top: &Path) -> Option<PathBuf> {
    match given {
        Some(path) => Some(path.to_path_buf()),
        None => ["coverage/lcov.info", "lcov.info"].iter().map(|p| top.join(p)).find(|p| p.is_file()),
    }
}

impl Coverage {
    /// The lines of a file in the diff, by its path from the top of the repository. lcov paths
    /// can be absolute, or relative to a package folder: the longest one that ends the other
    /// path matches.
    pub fn for_path(&self, path: &str, top: &Path) -> Option<&HashMap<usize, u64>> {
        let top = top.to_string_lossy();
        let relative = |name: &str| name.strip_prefix(top.as_ref()).map_or(name, |n| n.trim_start_matches('/')).trim_start_matches("./").to_owned();
        self.files
            .iter()
            .filter(|(name, _)| {
                let name = relative(name);
                name == path || path.ends_with(&format!("/{name}")) || name.ends_with(&format!("/{path}"))
            })
            .max_by_key(|(name, _)| name.len())
            .map(|(_, lines)| lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_lcov_and_matches_paths() {
        let text = "TN:\nSF:/repo/src/a.rs\nDA:1,3\nDA:2,0\nend_of_record\nSF:src/b.ts\nDA:5,0\nend_of_record\n";
        let coverage = parse(text);
        let top = Path::new("/repo");
        let a = coverage.for_path("src/a.rs", top).unwrap();
        assert_eq!((a[&1], a[&2]), (3, 0));
        // A package's own lcov, with paths from the package folder.
        assert_eq!(coverage.for_path("frontend/client/src/b.ts", top).unwrap()[&5], 0);
        assert!(coverage.for_path("src/c.rs", top).is_none());
    }
}
