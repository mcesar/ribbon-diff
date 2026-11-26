# ribbon

A side-by-side terminal diff viewer in the style of IntelliJ. Changed blocks on the two sides
are joined by curved ribbons. The two sides scroll together, so unchanged lines always sit on
the same row, and the longer side of a change scrolls faster than the shorter side.

The ribbons use the Kitty graphics protocol (Kitty, Ghostty, WezTerm). Other terminals and tmux
get a half-block approximation.

## Build

```sh
cargo build --release   # binary: target/release/ribbon
```

## Use

Inside a git repository, `ribbon` takes the same arguments as `git diff`:

```sh
ribbon                                   # uncommitted changes
ribbon --cached                          # staged changes
ribbon origin/develop...HEAD             # this branch
ribbon -w --ignore-comments -- . ':!*.test.ts' ':!*__tests__*'
```

It passes everything it does not know to `git diff`, reads both versions of each changed file
from git, and shows them all in one view.

- `-w` compares lines without their whitespace. Files with only whitespace changes drop out.
- `--ignore-comments` compares lines without their comments. Changes to comments or blank
  lines alone are shown without highlighting and are not counted; files with only such
  changes drop out.

One commit, one stash, or a patch:

```sh
ribbon show [COMMIT]                     # like git show (default: HEAD)
ribbon stash [STASH]                     # like git stash show -p (default: stash@{0})
git show abc123 | ribbon                 # a patch on stdin (also `ribbon -`)
ribbon fix.patch                         # a .patch or .diff file
git config pager.diff ribbon             # git diff opens ribbon (also pager.show)
```

A patch from git names the file versions on its `index` lines; ribbon reads the full files
from git when it can, so `c` shows whole files. Otherwise it shows only what the patch has.
As a pager, input that is not a diff is printed as it is.

Outside git mode:

```sh
ribbon OLD NEW          # two files
ribbon OLD_DIR NEW_DIR  # every changed file under two folders (git difftool --dir-diff)
GIT_EXTERNAL_DIFF=ribbon git --no-pager diff   # one view per file
```

When stdout is not a terminal (a pipe, a pager), ribbon prints a plain unified diff.

## Keys

| Key | Action |
| --- | --- |
| `j` / `k`, arrows, mouse wheel | scroll |
| `h` / `l`, `H` / `L`, `0`, Shift + wheel | scroll sideways |
| `space` / `Shift+Space` | page down / up (`Shift+Space` needs the Kitty keyboard protocol: Ghostty, Kitty, WezTerm; `PageUp` works everywhere) |
| `d` / `u` | half page down / up |
| `n` / `N` | next / previous change (continues into the next / previous file); `Y` copies it |
| `]` / `[` | next / previous file |
| `/` | search both sides as you type (lowercase ignores case); `Enter` keeps it, `Esc` cancels |
| `n` / `N` after a search | next / previous match; `Esc` clears the search |
| `f`, `Tab` | file list |
| `c` | whole file / changes only |
| `z`, click a fold | show the lines of one fold (the first one on screen, for `z`) |
| `Z` | close every opened fold (`c` twice does too) |
| `w` | wrap long lines / scroll them sideways |
| `e` | open `$VISUAL` or `$EDITOR` at the line: the start of the selection, or else the first changed line on screen; the view reloads after the editor closes |
| `<` / `>` | old side alone / new side alone, full width; the same key again shows both |
| `g` / `G` | top / end |
| mouse drag | select text on one side and copy it (OSC 52; also `pbcopy` on a local Mac); a file title copies its full path |
| `y` / `Esc` | copy the selection again / clear it |
| `y` with no selection | copy: `p` path, `a` absolute path, `f` file name, `l` path:line, `c` the change |
| `Y` | copy the current change (or the one with the selection) as a patch; with one side alone, that side's lines |
| `a` | add a note for Claude on the selected lines, or the line `e` would open |
| `}` / `{` | next / previous note |
| `A` | list of all notes (`Enter` goes to one, `x` deletes it) |
| `x` | delete the note picked with `}` / `{` |
| `q` | quit (in per-file mode, git opens the next file) |
| `Q` | quit and stop git difftool's file loop |

## Notes for Claude

Select lines (or scroll to a change) and press `a` to write a note, such as "use a constant
here". Notes are kept in `.ribbon/notes.json` at the top of the repository; the `.ribbon`
folder ignores itself in git. Each note shows under its last line, and a note inside a fold
opens the fold.

Then ask Claude to handle the notes. `ribbon skill` prints a Claude Code skill that teaches it
the file: Claude makes the changes, marks each note `done`, and adds a reply. It can also add
its own notes that explain why it changed something. ribbon reads the file again every
second, so replies show at once; with `--watch`, the code changes do too.

To install the skill:

```sh
mkdir -p ~/.claude/skills/ribbon-notes && ribbon skill > ~/.claude/skills/ribbon-notes/SKILL.md
```

The file uses [Hunk](https://github.com/modem-dev/hunk)'s agent-context format, so Hunk can read
it too. `--agent-context FILE` shows such a file from another tool, read-only.

## Options

- `--untracked`: also show untracked files (not ignored) as new files, when the diff compares with the working tree (not with `--cached` or two commits). Pathspecs apply to them too.
- `--ignore-imports`: compare lines without import statements (JavaScript, TypeScript, Python, Rust, Go). A file whose only changes are imports drops out.
- `--watch[=INTERVAL]`: reload when the files change, and stay on the same file and line. It checks every second (or every INTERVAL, such as `500ms` or `2s`) by asking git for the changed files and looking at their times. Works in git mode and two-path mode.
- `-U, --context N`: unchanged lines shown around each change (default 3). When a fold hides the line that starts the function (or method, or class) around the next change, that line stays visible inside the fold.
- `--agent-context FILE`: show notes from an agent beside the diff, read-only (Hunk's agent-context JSON).
- `--graphics auto|kitty|blocks`: how to draw ribbons. `auto` asks the terminal if it shows Kitty graphics.
- `--theme NAME`: syntect theme (default `base16-ocean.dark`).
- `--name PATH`: display name, also used to pick the syntax.

## License

MIT. See [LICENSE](LICENSE).
