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
use std::sync::{mpsc, Arc};
use std::time::Duration;
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
    --watch[=INTERVAL]          reload when the files change (git and two-path modes); checks
                                every second, or every INTERVAL such as 500ms or 2s
    -U, --context N             unchanged lines shown around each change (default: 3)
    --theme NAME                terminal (default: the terminal's own colors), or a syntect theme
    --graphics auto|kitty|blocks
                                how to draw ribbons (default: auto)
    --name PATH                 name to show and to pick syntax by (two-file mode)
    -h, --help                  show this help
    -V, --version               show the version

KEYS:
    j/k, arrows, wheel  scroll        space/S-space  page        d/u  half page
    h/l, H/L, 0         scroll sideways
    n/N                 next/prev change         ]/[      next/prev file
    f, Tab              file list                c        whole file / changes only
    g/G                 top/end                  q        quit
    < / >               old side / new side alone (again: both)
    /                   search (then n/N)        drag, y  select and copy
";

#[derive(Clone)]
struct Options {
    name: Option<String>,
    ignore_whitespace: bool,
    ignore_comments: bool,
    ignore_imports: bool,
    untracked: bool,
    watch: Option<Duration>,
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
        watch: None,
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
            "--watch" => options.watch = Some(Duration::from_secs(1)),
            _ if arg.starts_with("--watch=") => options.watch = Some(parse_interval(&arg["--watch=".len()..])?),
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

/// `500ms`, `2s`, or a number of seconds.
fn parse_interval(text: &str) -> anyhow::Result<Duration> {
    let (number, scale) = match text.strip_suffix("ms") {
        Some(ms) => (ms, 0.001),
        None => (text.strip_suffix('s').unwrap_or(text), 1.0),
    };
    let seconds = number.parse::<f64>().ok().filter(|n| *n > 0.0).with_context(|| format!("--watch needs an interval like 500ms or 2s, not {text:?}"))?;
    Ok(Duration::from_secs_f64((seconds * scale).max(0.05)))
}

/// Auto mode: None lets the viewer ask the terminal. Inside tmux the graphics do not reach the
/// outer terminal, so it is blocks.
fn auto_graphics() -> Option<Graphics> {
    let tmux = std::env::var_os("TMUX").is_some_and(|v| !v.is_empty());
    tmux.then_some(Graphics::Blocks)
}

/// The environment names a terminal known to show Kitty graphics.
fn kitty_hint() -> bool {
    let var = |name: &str| std::env::var(name).unwrap_or_default().to_lowercase();
    let term = var("TERM");
    let program = var("TERM_PROGRAM");
    !var("KITTY_WINDOW_ID").is_empty()
        || term.contains("kitty")
        || term.contains("ghostty")
        || program == "ghostty"
        || program == "wezterm"
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

/// Read the changed files and diff them. `report` prints renames and binary files, which only
/// the first load does: later loads happen while the viewer is on screen.
fn load(options: &Options, highlighter: &Highlighter, report: bool) -> anyhow::Result<Vec<(Pair, Model)>> {
    // Read both sides now: git deletes its temporary files when this process exits.
    let pairs = read_pairs(options)?;

    let mut text_pairs = Vec::new();
    for pair in pairs {
        if pair.old == pair.new {
            if pair.old_name != pair.new_name && report {
                println!("renamed {} -> {}", pair.old_name, pair.new_name);
            }
        } else if is_binary(&pair.old) || is_binary(&pair.new) {
            if report {
                println!("Binary files {} and {} differ", pair.old_name, pair.new_name);
            }
        } else {
            text_pairs.push(pair);
        }
    }

    // Files whose only changes are ignored (whitespace, comments) drop out, as with `git diff -w`.
    Ok(text_pairs
        .into_iter()
        .map(|pair| {
            let model = model_for(&pair, options, highlighter);
            (pair, model)
        })
        .filter(|(_, model)| !model.blocks.is_empty())
        .collect())
}

fn file_diffs(diffs: Vec<(Pair, Model)>) -> Vec<FileDiff> {
    diffs.into_iter().map(|(pair, model)| FileDiff { old_name: pair.old_name, new_name: pair.new_name, model }).collect()
}

/// Something that changes when the files to compare change, for `--watch`. None when the
/// source can't be watched (git's temporary files in external-diff mode).
fn signature(options: &Options) -> Option<Vec<u8>> {
    let p = &options.positional;
    match source(p) {
        Source::External => None,
        Source::Git => git::signature(p, options.untracked).ok(),
        Source::Paths => {
            let mut signature = String::new();
            for root in [Path::new(&p[0]), Path::new(&p[1])] {
                let files: Vec<std::path::PathBuf> = if root.is_dir() {
                    walk(root).unwrap_or_default().into_iter().map(|name| root.join(name)).collect()
                } else {
                    vec![root.to_path_buf()]
                };
                for file in files {
                    signature.push_str(&format!("{}={}\0", file.display(), git::stamp(&file)));
                }
            }
            Some(signature.into_bytes())
        }
    }
}

/// Check for changes every `interval` and send each new set of changed files to the viewer,
/// until the viewer is gone.
fn spawn_watcher(options: Options, highlighter: Arc<Highlighter>, interval: Duration, mut last: Option<Vec<u8>>) -> mpsc::Receiver<Vec<FileDiff>> {
    let (sender, reloads) = mpsc::channel();
    std::thread::spawn(move || loop {
        std::thread::sleep(interval);
        let now = signature(&options);
        if now == last {
            continue;
        }
        last = now;
        // A file caught in the middle of a save can fail to read; the next check tries again.
        if let Ok(diffs) = load(&options, &highlighter, false) {
            if sender.send(file_diffs(diffs)).is_err() {
                return;
            }
        }
    });
    reloads
}

fn run() -> anyhow::Result<ExitCode> {
    let Some(options) = parse_args()? else {
        return Ok(ExitCode::SUCCESS);
    };
    let highlighter = Arc::new(Highlighter::new(&options.theme)?);
    let watch = options.watch.filter(|_| std::io::stdout().is_terminal() && !matches!(source(&options.positional), Source::External));
    // Take the signature before reading, so a change made while reading is not missed.
    let mut last = watch.and_then(|_| signature(&options));
    let mut diffs = load(&options, &highlighter, true)?;
    if diffs.is_empty() {
        if let Some(interval) = watch {
            eprintln!("ribbon: no changes; watching for changes (Ctrl-C to stop)");
            while diffs.is_empty() {
                std::thread::sleep(interval);
                let now = signature(&options);
                if now != last {
                    last = now;
                    diffs = load(&options, &highlighter, false).unwrap_or_default();
                }
            }
        }
    }
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

    let reloads = watch.map(|interval| spawn_watcher(options.clone(), Arc::clone(&highlighter), interval, last));
    let settings = Settings {
        graphics: options.graphics.or_else(auto_graphics),
        kitty_hint: kitty_hint(),
        context: options.context,
        highlighter,
        reloads,
    };
    let files = file_diffs(diffs);
    Ok(match ui::run(files, settings)? {
        Outcome::Quit => ExitCode::SUCCESS,
        Outcome::QuitAll => ExitCode::from(1),
    })
}
