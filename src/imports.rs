//! Which lines belong to import statements, for `--ignore-imports`.

/// True for a line that starts an import: JavaScript and TypeScript `import` and
/// `export … from`, Python `import` and `from … import`, Rust `use`, Go `import`.
fn starts_import(line: &str) -> bool {
    let text = line.trim_start();
    let text = text.strip_prefix("pub ").or_else(|| text.strip_prefix("pub(crate) ")).unwrap_or(text);
    text.starts_with("import ")
        || text.starts_with("import{")
        || text.starts_with("import(") && !text.contains(").then")
        || text.starts_with("use ")
        || text.starts_with("from ") && text.contains(" import")
        || text.starts_with("export ") && (text.contains(" from '") || text.contains(" from \"") || text.ends_with('{') && text.starts_with("export {"))
}

/// For each line, true when it is part of an import statement. A statement continues while
/// its brackets are open, so `import {` … `} from 'x'` and `use a::{` … `};` count whole.
pub fn import_lines(lines: &[String]) -> Vec<bool> {
    let mut marks = vec![false; lines.len()];
    let mut depth: i32 = 0;
    for (index, line) in lines.iter().enumerate() {
        if depth == 0 && !starts_import(line) {
            continue;
        }
        marks[index] = true;
        for c in line.chars() {
            match c {
                '{' | '(' | '[' => depth += 1,
                '}' | ')' | ']' => depth -= 1,
                _ => {}
            }
        }
        depth = depth.max(0);
    }
    marks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks(text: &str) -> Vec<bool> {
        import_lines(&text.lines().map(str::to_owned).collect::<Vec<_>>())
    }

    #[test]
    fn marks_single_and_multi_line_imports() {
        let ts = "import { a } from 'x'\nimport {\n  b,\n  c,\n} from 'y'\nexport { d } from './d'\n\nconst e = 1\nexport const f = {\n}";
        assert_eq!(marks(ts), [true, true, true, true, true, true, false, false, false, false]);
        let rust = "use std::io;\nuse crate::{\n    a,\n};\n\nfn main() {}";
        assert_eq!(marks(rust), [true, true, true, true, false, false]);
        let python = "from a import (\n    b,\n)\nimport os\nx = 1";
        assert_eq!(marks(python), [true, true, true, true, false]);
        let go = "import (\n\t\"fmt\"\n)\nfunc main() {}";
        assert_eq!(marks(go), [true, true, true, false]);
    }
}
