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
| `space` / `b` | page down / up |
| `d` / `u` | half page down / up |
| `n` / `N` | next / previous change (continues into the next / previous file) |
| `]` / `[` | next / previous file |
| `f`, `Tab` | file list |
| `c` | whole file / changes only |
| `g` / `G` | top / end |
| `q` | quit (in per-file mode, git opens the next file) |
| `Q` | quit and stop git difftool's file loop |

## Options

- `-U, --context N`: unchanged lines shown around each change (default 3).
- `--graphics auto|kitty|blocks`: how to draw ribbons.
- `--theme NAME`: syntect theme (default `base16-ocean.dark`).
- `--name PATH`: display name, also used to pick the syntax.

## License

MIT. See [LICENSE](LICENSE).
