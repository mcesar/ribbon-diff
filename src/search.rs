//! Text search over the shown rows of both sides.

use std::ops::Range;

/// One match: the side, the stream row, and the byte range in that row's line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub old_side: bool,
    pub row: usize,
    pub bytes: Range<usize>,
}

pub struct Search {
    pub query: String,
    /// True while the query is being typed.
    pub editing: bool,
    /// Matches in scroll order.
    pub matches: Vec<Match>,
    pub current: usize,
    /// Scroll position and sideways scroll when the search started, for Esc while typing.
    pub origin: (usize, usize),
}

/// Byte ranges of every match of `query` in `line`. A query with no capital letters ignores
/// case (ASCII only, so byte offsets stay the same).
pub fn find_all(line: &str, query: &str) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    let ignore_case = !query.chars().any(|c| c.is_uppercase());
    let (haystack, needle) = if ignore_case {
        (line.to_ascii_lowercase(), query.to_ascii_lowercase())
    } else {
        (line.to_owned(), query.to_owned())
    };
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = haystack[from..].find(&needle) {
        let start = from + at;
        found.push(start..start + needle.len());
        from = start + needle.len();
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_every_match_with_smart_case() {
        assert_eq!(find_all("total = Total + total", "total"), vec![0..5, 8..13, 16..21]);
        assert_eq!(find_all("total = Total + total", "Total"), vec![8..13]);
        assert_eq!(find_all("aaaa", "aa"), vec![0..2, 2..4]);
        assert_eq!(find_all("abc", ""), Vec::<Range<usize>>::new());
        assert_eq!(find_all("é total", "total"), vec![3..8]);
    }
}
