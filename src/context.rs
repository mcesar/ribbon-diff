//! The lines that open the blocks around a line (function, class, `if`), found by indentation,
//! for the context rows pinned under the file title.

const TAB_WIDTH: usize = 4;

fn indent(line: &str) -> usize {
    line.chars()
        .take_while(|c| c.is_whitespace())
        .map(|c| if c == '\t' { TAB_WIDTH } else { 1 })
        .sum()
}

/// True for a line that starts a block: it ends with an opening bracket, `=>`, `->`, or `:`, or
/// it starts with a definition keyword.
fn opens_block(line: &str) -> bool {
    let text = line.trim();
    const ENDINGS: [&str; 6] = ["{", "(", "[", "=>", "->", ":"];
    const KEYWORDS: [&str; 12] =
        ["function ", "fn ", "def ", "class ", "impl ", "struct ", "enum ", "interface ", "trait ", "mod ", "async ", "export "];
    let defines = KEYWORDS.iter().any(|k| text.starts_with(k)) || text.starts_with("pub ") && text.contains("fn ");
    ENDINGS.iter().any(|e| text.ends_with(e)) || defines && !text.ends_with(';')
}

fn closes_first(line: &str) -> bool {
    line.trim_start().starts_with([')', ']', '}'])
}

/// True for a line that starts a function, method, or type definition, as opposed to an `if`,
/// a loop, or an object literal.
pub fn is_definition(line: &str) -> bool {
    let mut text = line.trim();
    for prefix in ["export ", "default ", "pub(crate) ", "pub ", "public ", "private ", "protected ", "static ", "async ", "unsafe ", "const fn "] {
        if let Some(rest) = text.strip_prefix(prefix) {
            text = rest.trim_start();
        }
    }
    const KEYWORDS: [&str; 11] = ["function", "fn ", "def ", "class ", "impl", "struct ", "enum ", "interface ", "trait ", "mod ", "type "];
    if KEYWORDS.iter().any(|k| text.starts_with(k)) {
        return true;
    }
    // `const load = async (id) => {`, `const View = ({`, `handler = function () {`
    if let Some((_, value)) = text.split_once(" = ") {
        let value = value.trim_start_matches("async ").trim_start();
        return value.starts_with('(') && (text.contains("=>") || text.ends_with('(') || text.ends_with("({")) || value.starts_with("function");
    }
    // A method: `name(args) {`, but not `if (a) {` or a call with a callback, `run(async () => {`.
    const CONTROL: [&str; 7] = ["if", "for", "while", "switch", "catch", "return", "else"];
    let name_end = text.find('(').unwrap_or(0);
    let name = &text[..name_end];
    name_end > 0
        && name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$')
        && !CONTROL.contains(&name)
        && !text.contains("=>")
        && text.ends_with('{')
}

/// Indexes of the definitions (functions, methods, classes) around `line`, outermost first.
pub fn definitions(lines: &[String], line: usize) -> Vec<usize> {
    enclosing(lines, line, usize::MAX).into_iter().filter(|&i| is_definition(&lines[i])).collect()
}

/// Indexes of the lines that open the blocks around `line`, outermost first, at most `max` (the
/// innermost ones).
pub fn enclosing(lines: &[String], line: usize, max: usize) -> Vec<usize> {
    let Some(start) = (0..=line.min(lines.len().saturating_sub(1))).rev().find(|&i| !lines[i].trim().is_empty()) else {
        return Vec::new();
    };
    let mut level = indent(&lines[start]);
    let mut found = Vec::new();
    let mut i = start;
    while i > 0 && level > 0 {
        i -= 1;
        let text = &lines[i];
        if text.trim().is_empty() || indent(text) >= level || !opens_block(text) {
            continue;
        }
        level = indent(text);
        // `) {` ends a signature that started higher up, at the same indentation.
        let mut opener = i;
        if closes_first(text) {
            if let Some(head) = (0..i).rev().find(|&j| !lines[j].trim().is_empty() && indent(&lines[j]) <= level && !closes_first(&lines[j])) {
                opener = head;
                i = head;
            }
        }
        found.push(opener);
    }
    found.reverse();
    let extra = found.len().saturating_sub(max);
    found.split_off(extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    #[test]
    fn finds_class_method_and_if() {
        let code = lines(
            "class Upload {\n  start() {\n    const a = 1\n    if (a) {\n      run()\n\n      done()\n    }\n  }\n}",
        );
        assert_eq!(enclosing(&code, 6, 5), vec![0, 1, 3]);
        assert_eq!(enclosing(&code, 6, 2), vec![1, 3]);
        assert_eq!(enclosing(&code, 2, 5), vec![0, 1]);
        assert_eq!(enclosing(&code, 0, 5), Vec::<usize>::new());
    }

    #[test]
    fn skips_finished_siblings() {
        let code = lines("function f() {\n  if (a) {\n    x()\n  }\n  y()\n}");
        assert_eq!(enclosing(&code, 4, 5), vec![0]);
    }

    #[test]
    fn a_multi_line_signature_shows_its_first_line() {
        let code = lines("export function load(\n  id: string,\n): Promise<void> {\n  fetch(id)\n}");
        assert_eq!(enclosing(&code, 3, 5), vec![0]);
    }

    #[test]
    fn python_and_yaml_blocks_end_with_a_colon() {
        let code = lines("class A:\n    def run(self):\n        return 1");
        assert_eq!(enclosing(&code, 2, 5), vec![0, 1]);
        let yaml = lines("jobs:\n  test:\n    steps:\n      - run: npm test");
        assert_eq!(enclosing(&yaml, 3, 5), vec![0, 1, 2]);
    }

    #[test]
    fn tells_definitions_from_other_blocks() {
        for line in [
            "export const BulkPriceListsUploadDialog = ({",
            "  const runUpload = async (upload: () => Promise<void>) => {",
            "  const handleCreateVersions = () =>",
            "export async function load(",
            "  start() {",
            "  async start(id: string) {",
            "pub fn changes(args: &[String]) -> anyhow::Result<Vec<Change>> {",
            "impl App {",
            "    def run(self):",
            "class Upload extends Base {",
        ] {
            assert!(is_definition(line), "{line}");
        }
        for line in ["    if (a) {", "  } catch (error) {", "  const options = {", "    runUpload(async () => {", "  return (", "jobs:", "    try {"] {
            assert!(!is_definition(line), "{line}");
        }
    }
}
