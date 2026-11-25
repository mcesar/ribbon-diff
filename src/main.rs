//! ribbon: a side-by-side terminal diff viewer with IntelliJ-style ribbons.
//!
//! Usage:
//!   ribbon [options] [git diff arguments]     git mode: like `git diff`
//!   ribbon [options] OLD NEW                  compare two files or two folders
//!   git difftool (cmd = ribbon "$LOCAL" "$REMOTE" --name "$MERGED")
//!   GIT_EXTERNAL_DIFF=ribbon git --no-pager diff
//!
//! When stdout is not a terminal (a pager, a pipe), ribbon prints a unified diff instead.

mod context;
mod git;
mod gutter;
mod highlight;
mod imports;
mod model;
mod search;
mod select;
mod termcolors;
mod ui;

use anyhow::Context;
use highlight::Highlighter;
use model::Model;
use similar::TextDiff;
use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use ui::{FileDiff, Graphics, Outcome, Settings};

const HELP: &str = "\
ribbon - side-by-side diff with IntelliJ-style ribbons (Kitty graphics)

USAGE:
    ribbon [OPTIONS] [GIT-DIFF-ARGS...]   changes from git, like `git diff` (the default)
    ribbon [OPTIONS] OLD NEW              two files, or two folders (git difftool --dir-diff)
    ribbon PATH OLD-FILE OLD-HEX OLD-MODE NEW-FILE NEW-HEX NEW-MODE [NEW-PATH INFO]
                                          (as GIT_EXTERNAL_DIFF)

    Git mode passes everything it does not know to `git diff`: commits and ranges,
    --cached, and pathspecs after --. For example:
        ribbon
        ribbon origin/develop...HEAD
        ribbon -w --ignore-comments -- . ':!*.test.ts'

OPTIONS:
    -w, --ignore-all-space      compare lines without their whitespace
    --ignore-comments           compare lines without their comments; changes to comments or
                                blank lines alone are not highlighted or counted
    --untracked                 also show untracked files (not ignored) as new files, when
                                comparing with the working tree
    --ignore-imports            compare lines without import statements; changes to imports
                                alone are not highlighted or counted
    -U, --context N             unchanged lines shown around each change (default: 3)
    --theme NAME                terminal (default: the terminal's own colors), or a syntect theme
    --graphics auto|kitty|blocks
                                how to draw ribbons (default: auto)
    --name PATH                 name to show and to pick syntax by (two-file mode)
    -h, --help                  show this help
    -V, --version               show the version

KEYS:
    j/k, arrows, wheel  scroll        space/b  page        d/u  half page
    h/l, H/L, 0         scroll sideways
    n/N                 next/prev change         ]/[      next/prev file
    f, Tab              file list                c        whole file / changes only
    g/G                 top/end                  q        quit
    < / >               old side / new side alone (again: both)
    /                   search (then n/N)        drag, y  select and copy
";

struct Options {
    name: Option<String>,
    ignore_whitespace: bool,
    ignore_comments: bool,
    ignore_imports: bool,
    untracked: bool,
    graphics: Option<Graphics>,
    theme: String,
    context: usize,
    positional: Vec<String>,
}

fn parse_args() -> anyhow::Result<Option<Options>> {
    let mut options = Options {
        name: None,
        ignore_whitespace: false,
        ignore_comments: false,
        ignore_imports: false,
        untracked: false,
        graphics: None,
        theme: highlight::DEFAULT_THEME.into(),
        context: 3,
        positional: Vec::new(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        // After "--" everything is a pathspec for git.
        if arg == "--" {
            options.positional.push(arg);
            options.positional.extend(args.by_ref());
            break;
        }
        match arg.as_str() {
            "-w" | "--ignore-all-space" => options.ignore_whitespace = true,
            "--ignore-comments" => options.ignore_comments = true,
            "--ignore-imports" => options.ignore_imports = true,
            "--untracked" => options.untracked = true,
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("ribbon {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--name" => options.name = Some(args.next().context("--name needs a value")?),
            "--theme" => options.theme = args.next().context("--theme needs a value")?,
            "--context" | "-U" => {
                options.context = args.next().context("--context needs a number")?.parse().context("--context needs a number")?
            }
            "--graphics" => {
                options.graphics = match args.next().as_deref() {
                    Some("kitty") => Some(Graphics::Kitty),
                    Some("blocks") => Some(Graphics::Blocks),
                    Some("auto") => None,
                    other => anyhow::bail!("--graphics must be auto, kitty, or blocks, not {other:?}"),
                }
            }
            _ => options.positional.push(arg),
        }
    }
    Ok(Some(options))
}

fn detect_graphics() -> Graphics {
    let var = |name: &str| std::env::var(name).unwrap_or_default().to_lowercase();
    if !var("TMUX").is_empty() {
        return Graphics::Blocks;
    }
    let term = var("TERM");
    let program = var("TERM_PROGRAM");
    let kitty_like = !var("KITTY_WINDOW_ID").is_empty()
        || term.contains("kitty")
        || term.contains("ghostty")
        || program == "ghostty"
        || program == "wezterm";
    if kitty_like { Graphics::Kitty } else { Graphics::Blocks }
}

fn read(path: &str) -> anyhow::Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("cannot read {path}"))
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|&b| b == 0)
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("ribbon: {error:#}");
            ExitCode::from(2)
        }
    }
}

/// Both sides of one changed path, read into memory.
struct Pair {
    old_name: String,
    new_name: String,
    old: Vec<u8>,
    new: Vec<u8>,
}

/// Every file under a directory, relative to it. Follows symlinks: git difftool --dir-diff
/// links working-tree files into its temporary directory.
fn walk(root: &Path) -> anyhow::Result<BTreeSet<String>> {
    let mut found = BTreeSet::new();
    let mut pending = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = pending.pop() {
        for entry in std::fs::read_dir(&dir).with_context(|| format!("cannot list {}", dir.display()))? {
            let path = entry?.path();
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            if meta.is_dir() && depth < 64 {
                pending.push((path, depth + 1));
            } else if meta.is_file() {
                if let Ok(relative) = path.strip_prefix(root) {
                    found.insert(relative.to_string_lossy().into_owned());
                }
            }
        }
    }
    Ok(found)
}

fn read_dirs(old_root: &Path, new_root: &Path) -> anyhow::Result<Vec<Pair>> {
    let mut names = walk(old_root)?;
    names.extend(walk(new_root)?);
    let read_or_empty = |root: &Path, name: &str| std::fs::read(root.join(name)).unwrap_or_default();
    Ok(names
        .into_iter()
        .map(|name| Pair {
            old: read_or_empty(old_root, &name),
            new: read_or_empty(new_root, &name),
            old_name: name.clone(),
            new_name: name,
        })
        .collect())
}

fn plain_diff(out: &mut impl Write, pair: &Pair) -> std::io::Result<()> {
    let old = String::from_utf8_lossy(&pair.old);
    let new = String::from_utf8_lossy(&pair.new);
    let diff = TextDiff::from_lines(old.as_ref(), new.as_ref());
    let (header_old, header_new) = (format!("a/{}", pair.old_name), format!("b/{}", pair.new_name));
    write!(out, "{}", diff.unified_diff().context_radius(3).header(&header_old, &header_new))
}

/// Where the files come from.
enum Source {
    /// Called by git as GIT_EXTERNAL_DIFF: 1, 7, or 9 arguments for one path.
    External,
    /// Two existing files or folders.
    Paths,
    /// Anything else goes to `git diff`.
    Git,
}

fn source(positional: &[String]) -> Source {
    let external = std::env::var_os("GIT_DIFF_PATH_COUNTER").is_some() || std::env::var_os("GIT_DIFF_PATH_TOTAL").is_some();
    if external && matches!(positional.len(), 1 | 7 | 9) {
        return Source::External;
    }
    if positional.len() == 2 && positional.iter().all(|p| Path::new(p).exists()) {
        return Source::Paths;
    }
    Source::Git
}

fn read_pairs(options: &Options) -> anyhow::Result<Vec<Pair>> {
    let p = &options.positional;
    Ok(match source(p) {
        Source::External => match p.len() {
            1 => {
                println!("* Unmerged path {}", p[0]);
                Vec::new()
            }
            7 => vec![Pair { old_name: p[0].clone(), new_name: p[0].clone(), old: read(&p[1])?, new: read(&p[4])? }],
            _ => vec![Pair { old_name: p[0].clone(), new_name: p[7].clone(), old: read(&p[1])?, new: read(&p[4])? }],
        },
        Source::Paths if Path::new(&p[0]).is_dir() => read_dirs(Path::new(&p[0]), Path::new(&p[1]))?,
        Source::Paths => {
            let name = |path: &String| options.name.clone().unwrap_or_else(|| path.clone());
            vec![Pair { old_name: name(&p[0]), new_name: name(&p[1]), old: read(&p[0])?, new: read(&p[1])? }]
        }
        Source::Git => {
            let mut changes = git::changes(p)?;
            if options.untracked {
                changes.extend(git::untracked(p)?);
                changes.sort_by(|a, b| a.new_name.cmp(&b.new_name));
            }
            changes.into_iter().map(|c| Pair { old_name: c.old_name, new_name: c.new_name, old: c.old, new: c.new }).collect()
        }
    })
}

/// True when an argument after `--` names a commit rather than a path that exists.
fn looks_like_revision(arg: &str) -> bool {
    !arg.starts_with(':')
        && !Path::new(arg).exists()
        && std::process::Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", &format!("{arg}^{{commit}}")])
            .output()
            .is_ok_and(|out| out.status.success())
}

/// Diff one file, comparing by keys when whitespace or comments are ignored.
fn model_for(pair: &Pair, options: &Options, highlighter: &Highlighter) -> Model {
    let old = model::split_lines(&String::from_utf8_lossy(&pair.old));
    let new = model::split_lines(&String::from_utf8_lossy(&pair.new));
    if !options.ignore_whitespace && !options.ignore_comments && !options.ignore_imports {
        return Model::from_lines(old, new, None);
    }
    let keys = |lines: &[String], name: &str| -> Vec<String> {
        let comments = if options.ignore_comments { highlighter.comment_ranges(name, lines) } else { vec![Vec::new(); lines.len()] };
        let imports = if options.ignore_imports { imports::import_lines(lines) } else { vec![false; lines.len()] };
        // An empty key makes a line count as unchanged, like a blank line.
        lines
            .iter()
            .zip(&comments)
            .zip(&imports)
            .map(|((line, ranges), &import)| if import { String::new() } else { model::line_key(line, ranges, options.ignore_whitespace) })
            .collect()
    };
    let (old_keys, new_keys) = (keys(&old, &pair.old_name), keys(&new, &pair.new_name));
    Model::from_lines(old, new, Some((old_keys, new_keys)))
}

fn run() -> anyhow::Result<ExitCode> {
    let Some(options) = parse_args()? else {
        return Ok(ExitCode::SUCCESS);
    };
    // Read both sides now: git deletes its temporary files when this process exits.
    let pairs = read_pairs(&options)?;

    let mut text_pairs = Vec::new();
    for pair in pairs {
        if pair.old == pair.new {
            if pair.old_name != pair.new_name {
                println!("renamed {} -> {}", pair.old_name, pair.new_name);
            }
        } else if is_binary(&pair.old) || is_binary(&pair.new) {
            println!("Binary files {} and {} differ", pair.old_name, pair.new_name);
        } else {
            text_pairs.push(pair);
        }
    }

    let highlighter = Arc::new(Highlighter::new(&options.theme)?);
    // Files whose only changes are ignored (whitespace, comments) drop out, as with `git diff -w`.
    let diffs: Vec<(Pair, Model)> = text_pairs
        .into_iter()
        .map(|pair| {
            let model = model_for(&pair, &options, &highlighter);
            (pair, model)
        })
        .filter(|(_, model)| !model.blocks.is_empty())
        .collect();
    if diffs.is_empty() {
        // Say so on a terminal; git and pipes expect silence for "no changes".
        if std::io::stderr().is_terminal() && !matches!(source(&options.positional), Source::External) {
            let hint = match options.positional.iter().position(|a| a == "--") {
                Some(dash) if options.positional[dash + 1..].iter().any(|a| looks_like_revision(a)) => {
                    " (arguments after -- are paths; put commits before --)"
                }
                _ => "",
            };
            eprintln!("ribbon: no changes{hint}");
        }
        return Ok(ExitCode::SUCCESS);
    }

    if !std::io::stdout().is_terminal() {
        let mut out = std::io::stdout().lock();
        let written = diffs.iter().try_for_each(|(pair, _)| plain_diff(&mut out, pair)).and_then(|()| out.flush());
        // A reader that stops early (head, a closed pager) is not an error.
        return match written {
            Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => Err(error.into()),
            _ => Ok(ExitCode::SUCCESS),
        };
    }

    let files = diffs
        .into_iter()
        .map(|(pair, model)| FileDiff { old_name: pair.old_name, new_name: pair.new_name, model })
        .collect();
    let settings = Settings {
        graphics: options.graphics.unwrap_or_else(detect_graphics),
        context: options.context,
        highlighter,
    };
    Ok(match ui::run(files, settings)? {
        Outcome::Quit => ExitCode::SUCCESS,
        Outcome::QuitAll => ExitCode::from(1),
    })
}
