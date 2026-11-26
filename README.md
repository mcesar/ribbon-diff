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
| `n` / `N` | next / previous change (continues into the next / previous file) |
| `]` / `[` | next / previous file |
| `/` | search both sides as you type (lowercase ignores case); `Enter` keeps it, `Esc` cancels |
| `n` / `N` after a search | next / previous match; `Esc` clears the search |
| `f`, `Tab` | file list |
| `c` | whole file / changes only |
| `<` / `>` | old side alone / new side alone, full width; the same key again shows both |
| `g` / `G` | top / end |
| mouse drag | select text on one side and copy it (OSC 52; also `pbcopy` on a local Mac); a file title copies its full path |
| `y` / `Esc` | copy the selection again / clear it |
| `q` | quit (in per-file mode, git opens the next file) |
| `Q` | quit and stop git difftool's file loop |

## Options

- `--untracked`: also show untracked files (not ignored) as new files, when the diff compares with the working tree (not with `--cached` or two commits). Pathspecs apply to them too.
- `--ignore-imports`: compare lines without import statements (JavaScript, TypeScript, Python, Rust, Go). A file whose only changes are imports drops out.
- `--watch[=INTERVAL]`: reload when the files change, and stay on the same file and line. It checks every second (or every INTERVAL, such as `500ms` or `2s`) by asking git for the changed files and looking at their times. Works in git mode and two-path mode.
- `-U, --context N`: unchanged lines shown around each change (default 3). When a fold hides the line that starts the function (or method, or class) around the next change, that line stays visible inside the fold.
- `--graphics auto|kitty|blocks`: how to draw ribbons. `auto` asks the terminal if it shows Kitty graphics.
- `--theme NAME`: syntect theme (default `base16-ocean.dark`).
- `--name PATH`: display name, also used to pick the syntax.

## License

MIT. See [LICENSE](LICENSE).
