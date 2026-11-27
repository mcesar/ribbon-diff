//! ribbon: a side-by-side terminal diff viewer with IntelliJ-style ribbons.
//!
//! Usage:
//!   ribbon [options] [git diff arguments]     git mode: like `git diff`
//!   ribbon [options] OLD NEW                  compare two files or two folders
//!   git difftool (cmd = ribbon "$LOCAL" "$REMOTE" --name "$MERGED")
//!   GIT_EXTERNAL_DIFF=ribbon git --no-pager diff
//!
//! When stdout is not a terminal (a pager, a pipe), ribbon prints a unified diff instead.

mod config;
mod context;
mod git;
mod github;
mod ribbons;
mod highlight;
mod imports;
mod listen;
mod model;
mod notes;
mod patch;
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
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::time::Duration;
use ui::{FileDiff, Graphics, LayoutMode, Outcome, Settings};

/// Instructions for Claude to act on notes: `ribbon skill`.
const SKILL: &str = include_str!("../skills/ribbon-notes/SKILL.md");

const HELP: &str = "\
ribbon - side-by-side diff with IntelliJ-style ribbons (Kitty graphics)

USAGE:
    ribbon [OPTIONS] [GIT-DIFF-ARGS...]   changes from git, like `git diff` (the default)
    ribbon [OPTIONS] OLD NEW              two files, or two folders (git difftool --dir-diff)
    ribbon show [COMMIT] [-- PATHS]       one commit, like `git show` (default: HEAD)
    ribbon stash [STASH] [-- PATHS]       one stash, like `git stash show -p`
    ribbon pr [NUMBER|URL] [-- PATHS]     a pull request, with `gh` (default: this branch's)
    ribbon FILE.patch | ribbon -          a patch file, or a patch on stdin:
                                          git show | ribbon, or as git's pager:
                                          git config pager.diff ribbon
    ribbon PATH OLD-FILE OLD-HEX OLD-MODE NEW-FILE NEW-HEX NEW-MODE [NEW-PATH INFO]
                                          (as GIT_EXTERNAL_DIFF)
    ribbon send COMMAND ...               control a ribbon started with --listen (below)

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
    --agent-context FILE        show notes from an agent beside the diff (Hunk's format)
    --listen[=SOCKET]           take commands from `ribbon send` on a local socket
                                (default: .ribbon/ribbon.sock at the top of the repository)
    --layout auto|split|unified two sides, or one column with removed lines above added ones;
                                auto (the default) is unified in terminals under 120 columns
    --wrap                      wrap long lines (the w key switches it)
    --scroll-off N              rows kept above a change, match, or note the view jumps to,
                                and around the keyboard selection (default: 3)
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
    f, Tab              file list (type to filter, arrows move, Enter opens)
    c                   whole file / changes only
    s                   split / unified layout   v        select lines: j/k extend, o other end,
                                                          J/K move the selection
    click a line number select that line (drag or Shift-click for more), then as with v
    z, click a fold     show the hidden lines    Z        fold them again
    w                   wrap long lines
    g/G                 top/end                  q        quit
    < / >               old side / new side alone (again: both)
    /                   search (then n/N)        drag, y  select and copy
    e                   open in $EDITOR at the selected or first changed line
    y (no selection)    copy: p path, a absolute path, f file name, l path:line, c change
    Y                   copy the change as a patch (one side alone: its lines)
    a                   add a note for Claude on the selected or first changed line
    P                   (ribbon pr) post the open notes as a review on the pull request
    } / {               next/prev note          A        all notes      x  delete note
    F / D               on a review finding: fix it (Claude does) / dismiss it; then the next

CONFIG:
    ~/.config/ribbon/config.toml sets defaults with the long option names, such as
    context = 5, ignore-comments = true, layout = \"split\", watch = \"500ms\". A [keys] table
    makes one key act as another: \"ctrl-d\" = \"pagedown\". The command line wins over the file.

SEND (to a ribbon started with --listen; lines are 1-based, --old means the old version):
    ribbon send navigate PATH[:LINE]          scroll to a file or a line
    ribbon send highlight PATH:FIRST[-LAST]   mark lines and scroll to them
    ribbon send clear                         remove the mark
    ribbon send reload                        read the files and notes again
    ribbon send note PATH:FIRST[-LAST] TEXT [--rationale TEXT] [--author NAME]
                                              add a note (author: claude)
    ribbon send finding PATH:FIRST[-LAST] TEXT [--rationale TEXT]
                                              add a code-review finding to fix or dismiss
    ribbon send status                        where the view is, as JSON
    ribbon send 'JSON'                        one request as JSON
    Options: --old, --socket PATH. With no ribbon listening, note and finding write
    .ribbon/notes.json directly.

NOTES:
    Notes are kept in .ribbon/notes.json at the top of the repository, and show again when
    the file changes. `ribbon skill` prints instructions that teach Claude to act on them.
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
    /// A patch to show instead of asking git (from stdin, a .patch file, or as a pager).
    patch: Option<String>,
    /// Notes from an agent, in Hunk's agent-context format.
    agent_context: Option<PathBuf>,
    scroll_off: usize,
    wrap: bool,
    layout: LayoutMode,
    /// Keys from the config file: a key, and the key it acts as.
    keys: Vec<(config::Key, config::Key)>,
    /// `--listen`: the socket to take commands on; an empty path means the default one.
    listen: Option<PathBuf>,
    /// `ribbon pr`: the pull request shown.
    pull_request: Option<github::PullRequest>,
}

fn parse_layout(text: &str) -> anyhow::Result<LayoutMode> {
    Ok(match text {
        "auto" => LayoutMode::Auto,
        "split" => LayoutMode::Split,
        "unified" => LayoutMode::Unified,
        other => anyhow::bail!("layout must be auto, split, or unified, not {other:?}"),
    })
}

fn parse_graphics(text: &str) -> anyhow::Result<Option<Graphics>> {
    Ok(match text {
        "kitty" => Some(Graphics::Kitty),
        "blocks" => Some(Graphics::Blocks),
        "auto" => None,
        other => anyhow::bail!("graphics must be auto, kitty, or blocks, not {other:?}"),
    })
}

/// Defaults from the config file, before the command line.
fn apply_config(options: &mut Options, table: &toml::Table) -> anyhow::Result<()> {
    use toml::Value;
    for (name, value) in table {
        let key = name.replace('_', "-");
        let flag = |value: &Value| value.as_bool().with_context(|| format!("{name} must be true or false"));
        let number = |value: &Value| value.as_integer().filter(|n| *n >= 0).map(|n| n as usize).with_context(|| format!("{name} must be a number"));
        let text = |value: &Value| value.as_str().map(str::to_owned).with_context(|| format!("{name} must be a string"));
        match key.as_str() {
            "context" => options.context = number(value)?,
            "scroll-off" => options.scroll_off = number(value)?,
            "theme" => options.theme = text(value)?,
            "graphics" => options.graphics = parse_graphics(&text(value)?)?,
            "layout" => options.layout = parse_layout(&text(value)?)?,
            "ignore-all-space" | "ignore-whitespace" => options.ignore_whitespace = flag(value)?,
            "ignore-comments" => options.ignore_comments = flag(value)?,
            "ignore-imports" => options.ignore_imports = flag(value)?,
            "untracked" => options.untracked = flag(value)?,
            "wrap" => options.wrap = flag(value)?,
            "listen" => {
                options.listen = match value {
                    Value::Boolean(on) => on.then(PathBuf::new),
                    _ => Some(text(value)?.into()),
                }
            }
            "watch" => {
                options.watch = match value {
                    Value::Boolean(on) => on.then_some(Duration::from_secs(1)),
                    _ => Some(parse_interval(&text(value)?)?),
                }
            }
            "keys" => {
                let keys = value.as_table().context("keys must be a table, like [keys]")?;
                for (from, to) in keys {
                    let to = to.as_str().with_context(|| format!("key {from:?} must name a key, like \"pagedown\""))?;
                    options.keys.push((config::parse_key(from)?, config::parse_key(to)?));
                }
            }
            _ => anyhow::bail!("unknown setting {name:?}"),
        }
    }
    Ok(())
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
        patch: None,
        agent_context: None,
        scroll_off: 3,
        wrap: false,
        layout: LayoutMode::Auto,
        keys: Vec::new(),
        listen: None,
        pull_request: None,
    };
    let config = config::load()?;
    apply_config(&mut options, &config).with_context(|| format!("in {}", config::path().unwrap_or_default().display()))?;
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
            "--agent-context" => options.agent_context = Some(args.next().context("--agent-context needs a file")?.into()),
            _ if arg.starts_with("--agent-context=") => options.agent_context = Some(arg["--agent-context=".len()..].into()),
            "--theme" => options.theme = args.next().context("--theme needs a value")?,
            "--context" | "-U" => {
                options.context = args.next().context("--context needs a number")?.parse().context("--context needs a number")?
            }
            "--graphics" => options.graphics = parse_graphics(&args.next().unwrap_or_default()).context("--graphics")?,
            "--layout" => options.layout = parse_layout(&args.next().unwrap_or_default()).context("--layout")?,
            "--scroll-off" => {
                options.scroll_off = args.next().context("--scroll-off needs a number")?.parse().context("--scroll-off needs a number")?
            }
            "--wrap" => options.wrap = true,
            "--listen" => options.listen = Some(PathBuf::new()),
            _ if arg.starts_with("--listen=") => options.listen = Some(arg["--listen=".len()..].into()),
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
    /// The new side on disk, for the editor.
    path: Option<PathBuf>,
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
            path: Some(new_root.join(&name)).filter(|p| p.is_file()),
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
    /// A patch, read before anything else.
    Patch,
    /// Called by git as GIT_EXTERNAL_DIFF: 1, 7, or 9 arguments for one path.
    External,
    /// Two existing files or folders.
    Paths,
    /// Anything else goes to `git diff`.
    Git,
}

fn source(options: &Options) -> Source {
    if options.patch.is_some() {
        return Source::Patch;
    }
    let positional = &options.positional;
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
    // As GIT_EXTERNAL_DIFF, git runs us at the top of the work tree.
    let in_tree = |name: &String| Some(PathBuf::from(name)).filter(|p| p.is_file());
    Ok(match source(options) {
        Source::Patch => patch_pairs(options.patch.as_deref().unwrap_or("")),
        Source::External => match p.len() {
            1 => {
                println!("* Unmerged path {}", p[0]);
                Vec::new()
            }
            7 => vec![Pair { old_name: p[0].clone(), new_name: p[0].clone(), old: read(&p[1])?, new: read(&p[4])?, path: in_tree(&p[0]) }],
            _ => vec![Pair { old_name: p[0].clone(), new_name: p[7].clone(), old: read(&p[1])?, new: read(&p[4])?, path: in_tree(&p[7]) }],
        },
        Source::Paths if Path::new(&p[0]).is_dir() => read_dirs(Path::new(&p[0]), Path::new(&p[1]))?,
        Source::Paths => {
            let name = |path: &String| options.name.clone().unwrap_or_else(|| path.clone());
            vec![Pair { old_name: name(&p[0]), new_name: name(&p[1]), old: read(&p[0])?, new: read(&p[1])?, path: Some(PathBuf::from(&p[1])) }]
        }
        Source::Git => {
            let mut changes = git::changes(p)?;
            if options.untracked {
                changes.extend(git::untracked(p)?);
                changes.sort_by(|a, b| a.new_name.cmp(&b.new_name));
            }
            changes.into_iter().map(|c| Pair { old_name: c.old_name, new_name: c.new_name, old: c.old, new: c.new, path: c.path }).collect()
        }
    })
}

/// Both sides of each file in a patch. Full files come from git when its `index` lines name
/// objects git has; the new side can also be the working-tree file, when it hashes the same.
/// Otherwise both sides are built from the patch.
fn patch_pairs(text: &str) -> Vec<Pair> {
    let files: Vec<patch::FilePatch> = patch::parse(text).into_iter().filter(|f| !f.binary).collect();
    let root = git::top_level().ok();
    let ids: Vec<String> = files.iter().flat_map(|f| [f.old_id.clone(), f.new_id.clone()]).flatten().collect();
    let blobs = if root.is_some() { git::read_blobs(ids).unwrap_or_default() } else { Default::default() };
    let on_disk = |name: &str| Some(root.clone().unwrap_or_default().join(name)).filter(|p| p.is_file());
    files
        .into_iter()
        .map(|f| {
            let blob = |id: &Option<String>| id.as_ref().and_then(|id| blobs.get(id).cloned());
            let old = if f.old_missing { Some(Vec::new()) } else { blob(&f.old_id) };
            let path = on_disk(&f.new_name);
            let new = if f.new_missing {
                Some(Vec::new())
            } else {
                blob(&f.new_id).or_else(|| {
                    let (id, path) = (f.new_id.as_ref()?, path.as_ref()?);
                    let hashed = std::process::Command::new("git").arg("hash-object").arg(path).output().ok()?;
                    String::from_utf8_lossy(&hashed.stdout).starts_with(id.as_str()).then(|| std::fs::read(path).ok()).flatten()
                })
            };
            let (old, new) = match (old, new) {
                (Some(old), Some(new)) => (old, new),
                _ => {
                    let (old, new) = f.sides();
                    (old.into_bytes(), new.into_bytes())
                }
            };
            Pair { old_name: f.old_name, new_name: f.new_name, old, new, path }
        })
        .collect()
}

/// A patch to read, if the arguments or stdin give one: `-` or nothing with a pipe or file on
/// stdin, or one `.patch` or `.diff` file.
fn read_patch(positional: &[String]) -> anyhow::Result<Option<String>> {
    use std::io::Read;
    use std::os::unix::fs::FileTypeExt;
    let from_stdin = match positional {
        [dash] if dash == "-" => true,
        // Only a pipe or a file: a terminal, /dev/null, or a closed stdin is not a patch.
        [] => std::fs::metadata("/dev/stdin").is_ok_and(|m| m.file_type().is_fifo() || m.is_file()),
        [path] if (path.ends_with(".patch") || path.ends_with(".diff")) && Path::new(path).is_file() => {
            return Ok(Some(String::from_utf8_lossy(&read(path)?).into_owned()));
        }
        _ => false,
    };
    if !from_stdin {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    std::io::stdin().read_to_end(&mut bytes)?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// `ribbon show [COMMIT]` and `ribbon stash [STASH]`: the git diff arguments for them, and a
/// line about the commit for the status line.
fn subcommand(positional: &[String]) -> Option<(Vec<String>, Option<String>)> {
    let command = positional.first()?;
    if command != "show" && command != "stash" {
        return None;
    }
    let rest = &positional[1..];
    let (revision, rest) = match rest.first() {
        Some(first) if !first.starts_with('-') => (first.clone(), &rest[1..]),
        _ => (if command == "show" { "HEAD".to_owned() } else { "stash@{0}".to_owned() }, rest),
    };
    let git = |args: &[&str]| {
        std::process::Command::new("git").args(args).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    // A root commit has no parent: compare with the empty tree.
    let parent = format!("{revision}^");
    let base = if git(&["rev-parse", "--verify", "--quiet", &parent]).is_some() { parent } else { "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_owned() };
    let notice = git(&["log", "-1", "--format=%h %s", &revision]);
    let mut args = vec![base, revision];
    args.extend(rest.iter().cloned());
    Some((args, notice))
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

/// Lines parsed before each change for `--ignore-comments`, so a comment that starts above
/// the change is still seen.
const COMMENT_LOOKBACK: usize = 100;

/// Diff one file, comparing by keys when whitespace or comments are ignored.
fn model_for(pair: &Pair, options: &Options, highlighter: &Highlighter) -> Model {
    let old = model::split_lines(&String::from_utf8_lossy(&pair.old));
    let new = model::split_lines(&String::from_utf8_lossy(&pair.new));
    if !options.ignore_whitespace && !options.ignore_comments && !options.ignore_imports {
        return Model::from_lines(old, new, None);
    }
    // Finding comments needs a syntax parse, so it is done only near the changes.
    let (old_near, new_near) =
        if options.ignore_comments { model::near_changes(&old, &new, COMMENT_LOOKBACK) } else { Default::default() };
    let keys = |lines: &[String], name: &str, near: &[bool]| -> Vec<String> {
        let comments = if options.ignore_comments { highlighter.comment_ranges(name, lines, near) } else { vec![Vec::new(); lines.len()] };
        let imports = if options.ignore_imports { imports::import_lines(lines) } else { vec![false; lines.len()] };
        // An empty key makes a line count as unchanged, like a blank line.
        lines
            .iter()
            .zip(&comments)
            .zip(&imports)
            .map(|((line, ranges), &import)| if import { String::new() } else { model::line_key(line, ranges, options.ignore_whitespace) })
            .collect()
    };
    let (old_keys, new_keys) = (keys(&old, &pair.old_name, &old_near), keys(&new, &pair.new_name, &new_near));
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
    let models = parallel_map(&text_pairs, |pair| model_for(pair, options, highlighter));
    Ok(text_pairs.into_iter().zip(models).filter(|(_, model)| !model.blocks.is_empty()).collect())
}

/// `items.iter().map(f)` on all cores, in order. Each thread takes the next item when it is
/// done, so one large file does not hold up the others.
fn parallel_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(items.len());
    let mut results: Vec<(usize, R)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(item) = items.get(index) else { break done };
                        done.push((index, f(item)));
                    }
                })
            })
            .collect();
        workers.into_iter().flat_map(|worker| worker.join().unwrap_or_default()).collect()
    });
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, result)| result).collect()
}

fn file_diffs(diffs: Vec<(Pair, Model)>) -> Vec<FileDiff> {
    diffs.into_iter().map(|(pair, model)| FileDiff { old_name: pair.old_name, new_name: pair.new_name, model, path: pair.path }).collect()
}

/// Something that changes when the files to compare change, for `--watch`. None when the
/// source can't be watched (git's temporary files in external-diff mode).
fn signature(options: &Options) -> Option<Vec<u8>> {
    let p = &options.positional;
    match source(options) {
        Source::External | Source::Patch => None,
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

/// A path from the command line as the diff names it: relative to the top of the repository.
fn repo_path(arg: &str, top: &Path) -> String {
    let given = Path::new(arg);
    let absolute = if given.is_absolute() { given.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(given) };
    // Resolve symlinks (/tmp is /private/tmp on macOS) on the longest part that exists.
    let resolved = absolute.canonicalize().ok().or_else(|| Some(absolute.parent()?.canonicalize().ok()?.join(absolute.file_name()?)));
    let top = top.canonicalize().unwrap_or_else(|_| top.to_path_buf());
    resolved
        .and_then(|path| path.strip_prefix(&top).ok().map(|p| p.to_string_lossy().into_owned()))
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| arg.to_owned())
}

/// `PATH`, `PATH:LINE`, or `PATH:FIRST-LAST`.
fn path_and_lines(arg: &str) -> anyhow::Result<(&str, Option<(usize, usize)>)> {
    let Some((path, lines)) = arg.rsplit_once(':').filter(|(_, l)| l.chars().next().is_some_and(|c| c.is_ascii_digit())) else {
        return Ok((arg, None));
    };
    let number = |n: &str| n.parse::<usize>().ok().filter(|&n| n >= 1).with_context(|| format!("{arg:?}: lines are 1-based numbers, like {path}:12 or {path}:12-20"));
    let (first, last) = match lines.split_once('-') {
        Some((first, last)) => (number(first)?, number(last)?),
        None => (number(lines)?, number(lines)?),
    };
    if first > last {
        anyhow::bail!("{arg:?}: the first line comes after the last");
    }
    Ok((path, Some((first, last))))
}

/// `ribbon send ...`: one request to a listening ribbon. Prints the reply as JSON.
fn send(args: &[String]) -> anyhow::Result<ExitCode> {
    let top = git::top_level().unwrap_or_else(|_| PathBuf::from("."));
    let (mut socket, mut old, mut rationale, mut author) = (None, false, None, None);
    let mut words = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--socket" => socket = Some(PathBuf::from(rest.next().context("--socket needs a path")?)),
            "--old" => old = true,
            "--rationale" => rationale = Some(rest.next().context("--rationale needs text")?.clone()),
            "--author" => author = Some(rest.next().context("--author needs a name")?.clone()),
            _ => words.push(arg.as_str()),
        }
    }
    let socket = socket.or_else(|| std::env::var_os("RIBBON_SOCKET").map(PathBuf::from)).unwrap_or_else(|| listen::socket_file(&top));
    let side = if old { "old" } else { "new" };
    let lines = |arg: Option<&&str>, needed: bool| -> anyhow::Result<(String, Option<(usize, usize)>)> {
        let arg = arg.context("needs PATH:LINE, like src/main.rs:12 or src/main.rs:12-20")?;
        let (path, lines) = path_and_lines(arg)?;
        if needed && lines.is_none() {
            anyhow::bail!("{arg:?} needs lines, like {arg}:12 or {arg}:12-20");
        }
        Ok((repo_path(path, &top), lines))
    };
    let request = match words.as_slice() {
        [json] if json.trim_start().starts_with('{') => serde_json::from_str(json).context("not valid JSON")?,
        ["navigate", target] => {
            let (path, lines) = lines(Some(target), false)?;
            serde_json::json!({ "command": "navigate", "path": path, "side": side, "line": lines.map(|l| l.0) })
        }
        ["highlight", target] => {
            let (path, lines) = lines(Some(target), true)?;
            let (first, last) = lines.unwrap_or_default();
            serde_json::json!({ "command": "highlight", "path": path, "side": side, "range": [first, last] })
        }
        [command @ ("clear" | "reload" | "status")] => serde_json::json!({ "command": command }),
        [command @ ("note" | "finding"), target, text @ ..] if !text.is_empty() => {
            let (path, lines) = lines(Some(target), true)?;
            let (first, last) = lines.unwrap_or_default();
            let mut request = serde_json::json!({ "command": command, "path": path, "side": side, "range": [first, last], "summary": text.join(" ") });
            if let Some(rationale) = &rationale {
                request["rationale"] = rationale.clone().into();
            }
            if let Some(author) = &author {
                request["author"] = author.clone().into();
            }
            request
        }
        _ => anyhow::bail!("usage: ribbon send navigate|highlight|clear|reload|note|finding|status ... (see ribbon --help)"),
    };
    let reply = match listen::send(&socket, &request) {
        Ok(reply) => reply,
        // Without a viewer, a note still goes into the notes file; ribbon shows it when it opens.
        Err(_) if matches!(request["command"].as_str(), Some("note" | "finding")) => write_note(&top, &request)?,
        Err(error) => return Err(error),
    };
    println!("{reply}");
    Ok(if reply["ok"] == true { ExitCode::SUCCESS } else { ExitCode::from(1) })
}

/// Add a note to `.ribbon/notes.json` when no ribbon listens. The anchor comes from the file
/// on disk, so it is only set for the new side.
fn write_note(top: &Path, request: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
    let listen::Command::Note { path, old_side, range, summary, rationale, author, status } = listen::parse(&request.to_string())? else {
        anyhow::bail!("not a note");
    };
    let anchor = (!old_side).then(|| std::fs::read_to_string(top.join(&path)).ok()).flatten().and_then(|text| model::split_lines(&text).get(range.1 - 1).cloned());
    let id = notes::new_id();
    let note = notes::Note { id: Some(id.clone()), path, old_side, range, anchor, summary, rationale, author: Some(author), status, reply: None, editable: true };
    notes::add(&notes::notes_file(top), &note)?;
    Ok(listen::ok(serde_json::json!({ "id": id, "written": "notes file (no ribbon listening)" })))
}

fn run() -> anyhow::Result<ExitCode> {
    // Before the options: `ribbon send` has its own, and its stdin is not a patch.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "send") {
        return send(&args[1..]);
    }
    let Some(mut options) = parse_args()? else {
        return Ok(ExitCode::SUCCESS);
    };
    let mut notice = None;
    if options.positional.first().is_some_and(|c| c == "skill") {
        print!("{SKILL}");
        return Ok(ExitCode::SUCCESS);
    }
    if options.positional.first().is_some_and(|c| c == "pr") {
        let rest = &options.positional[1..];
        let (which, rest) = match rest.first() {
            Some(first) if first != "--" => (Some(first.as_str()), &rest[1..]),
            _ => (None, rest),
        };
        let pr = github::view(which)?;
        github::fetch(&pr)?;
        let mut args = github::diff_args(&pr);
        args.extend(rest.iter().cloned());
        options.positional = args;
        notice = Some(format!("PR #{} {}", pr.number, pr.title));
        options.pull_request = Some(pr);
    } else if let Some((args, about)) = subcommand(&options.positional) {
        options.positional = args;
        notice = about;
    } else if let Some(text) = read_patch(&options.positional)? {
        // Nothing to show, for example git's pager after an empty `git diff`.
        if text.is_empty() {
            return Ok(ExitCode::SUCCESS);
        }
        let text = patch::strip_escapes(&text);
        if patch::parse(&text).is_empty() {
            // Not a diff (ribbon as the pager for another git command): show it as it is.
            print!("{text}");
            return Ok(ExitCode::SUCCESS);
        }
        options.patch = Some(text);
    }
    let highlighter = Arc::new(Highlighter::new(&options.theme)?);
    let watch = options.watch.filter(|_| std::io::stdout().is_terminal() && !matches!(source(&options), Source::External | Source::Patch));
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
        if std::io::stderr().is_terminal() && !matches!(source(&options), Source::External | Source::Patch) {
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

    // A ribbon that can't listen (another one does already) still shows the diff, and says so.
    let top = git::top_level().unwrap_or_else(|_| PathBuf::from("."));
    let (commands, _listening) = match &options.listen {
        Some(path) => {
            let path = if path.as_os_str().is_empty() { listen::socket_file(&top) } else { path.clone() };
            match listen::listen(&path) {
                Ok((commands, listening)) => (Some(commands), Some(listening)),
                Err(error) => {
                    notice = Some(format!("not listening: {error:#}"));
                    (None, None)
                }
            }
        }
        None => (None, None),
    };
    let reloads = watch.map(|interval| spawn_watcher(options.clone(), Arc::clone(&highlighter), interval, last));
    // After the editor closes, read the files again (a patch or git's temporary files can't be).
    let load_again: Option<Box<dyn Fn() -> Vec<FileDiff>>> = match source(&options) {
        Source::Patch | Source::External => None,
        _ => {
            let (options, highlighter) = (options.clone(), Arc::clone(&highlighter));
            Some(Box::new(move || load(&options, &highlighter, false).map(file_diffs).unwrap_or_default()))
        }
    };
    // Checks change while the view is open: ask every 30 seconds.
    let checks = options.pull_request.clone().map(|pr| {
        let (sender, checks) = mpsc::channel();
        std::thread::spawn(move || loop {
            if let Some(line) = github::checks(&pr) {
                if sender.send(line).is_err() {
                    return;
                }
            }
            std::thread::sleep(Duration::from_secs(30));
        });
        checks
    });
    let settings = Settings {
        graphics: options.graphics.or_else(auto_graphics),
        kitty_hint: kitty_hint(),
        context: options.context,
        highlighter,
        reloads,
        load_again,
        notice,
        // Notes live at the top of the repository, or in the current folder outside git.
        scroll_off: options.scroll_off,
        wrap: options.wrap,
        layout: options.layout,
        keys: options.keys.clone(),
        notes_file: notes::notes_file(&top),
        commands,
        checks,
        pull_request: options.pull_request.clone(),
        agent_context: options.agent_context.clone(),
    };
    let mut files = file_diffs(diffs);
    // The editor opens the working-tree file: only when it is the pull request's version.
    if let Some(pr) = &options.pull_request {
        let checked_out = std::process::Command::new("git").args(["rev-parse", "HEAD"]).output().is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == pr.head);
        let clean = std::process::Command::new("git").args(["diff", "--quiet", "HEAD"]).status().is_ok_and(|s| s.success());
        if !(checked_out && clean) {
            files.iter_mut().for_each(|f| f.path = None);
        }
    }
    Ok(match ui::run(files, settings)? {
        Outcome::Quit => ExitCode::SUCCESS,
        Outcome::QuitAll => ExitCode::from(1),
    })
}
