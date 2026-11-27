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

A pull request, with the [GitHub CLI](https://cli.github.com):

```sh
ribbon pr 123                            # or a URL; no number: this branch's pull request
ribbon pr 123 -- '*.ts'                  # only some files
```

ribbon fetches the pull request's commits when the clone does not have them (only
`FETCH_HEAD` changes) and shows what the pull request page shows: the head against the point
where it left the base. The status line shows the checks, read again every 30 seconds, such as
`CI ✗ 1 failed (lint) · 12 passed`. `e` opens the editor only when the pull request is checked
out with no local changes.

`P` posts your open notes, and the findings marked to fix, as one review on the pull request,
after you press `y` to confirm. Each note becomes a comment on its lines; a note on lines the
pull request page does not show goes into the review text with its path and line. Posted notes
show `posted` and are no longer tasks for Claude.

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
| `f`, `Tab` | file list with each file's number of changes; type to filter it (words in any order), `↑`/`↓` to move, `Enter` to open |
| `c` | whole file / changes only |
| `z`, click a fold | show the lines of one fold (the first one on screen, for `z`) |
| `Z` | close every opened fold (`c` twice does too) |
| `w` | wrap long lines / scroll them sideways |
| `s` | split / unified layout (unified: removed lines above the lines that replace them, in one column) |
| `v` | select whole lines with the keyboard: `j`/`k` extend, then `y` copy, `a` note, `Y` change, `e` edit, `Esc` cancel |
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
| `P` | in `ribbon pr`: post the open notes as a review on the pull request (asks first) |
| `F` / `D` | on a code-review finding: fix it (Claude does) / dismiss it, then go to the next one; with no finding picked, go to the next one |
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

### Code-review findings

After a code review, Claude can put each finding on its lines with `ribbon send finding` (the
skill tells it to). A finding is a cyan note that waits for a decision. Press `F` to go to the
first one, then `F` again to have Claude fix it or `D` to dismiss it; ribbon goes on to the next
one. Then ask Claude to handle the notes: it fixes the findings you marked and replies on each.

The file uses [Hunk](https://github.com/modem-dev/hunk)'s agent-context format, so Hunk can read
it too. `--agent-context FILE` shows such a file from another tool, read-only.

## Claude controls the view

`ribbon --listen` takes commands on a local socket, `.ribbon/ribbon.sock` at the top of the
repository (or a short path under `/tmp` when that path is too long for a socket). `ribbon send`
is the client, so Claude can show you the line it talks about:

```sh
ribbon send navigate src/main.rs:120         # scroll to a line, opening its fold
ribbon send highlight src/main.rs:120-131    # mark lines (cyan line numbers) and scroll there
ribbon send clear                            # remove the mark
ribbon send reload                           # read the files and notes again
ribbon send note src/main.rs:120 "Why this changed" --rationale "..." [--author NAME]
ribbon send finding src/main.rs:120-131 "A problem" --rationale "How it fails"
ribbon send status                           # the file and line on screen, as JSON
ribbon send '{"command":"navigate","path":"src/main.rs","line":120}'
```

`--old` puts the lines on the old version; `--socket PATH` (or `RIBBON_SOCKET`) picks another
socket. Each request and reply is one line of JSON, so any program can talk to the socket. With
no ribbon listening, `note` and `finding` write `.ribbon/notes.json` directly.

## Options

- `--untracked`: also show untracked files (not ignored) as new files, when the diff compares with the working tree (not with `--cached` or two commits). Pathspecs apply to them too.
- `--ignore-imports`: compare lines without import statements (JavaScript, TypeScript, Python, Rust, Go). A file whose only changes are imports drops out.
- `--watch[=INTERVAL]`: reload when the files change, and stay on the same file and line. It checks every second (or every INTERVAL, such as `500ms` or `2s`) by asking git for the changed files and looking at their times. Works in git mode and two-path mode.
- `-U, --context N`: unchanged lines shown around each change (default 3). When a fold hides the line that starts the function (or method, or class) around the next change, that line stays visible inside the fold.
- `--layout auto|split|unified`: two sides with ribbons, or one column. `auto` (the default) is unified in terminals under 120 columns; `s` switches.
- `--wrap`: start with long lines wrapped.
- `--scroll-off N`: rows kept above a change, a match, or a note the view jumps to, and around the end of a keyboard selection (default 3).
- `--agent-context FILE`: show notes from an agent beside the diff, read-only (Hunk's agent-context JSON).
- `--listen[=SOCKET]`: take commands from `ribbon send` (see above). `listen = true` in the config file turns it on for every ribbon.
- `--graphics auto|kitty|blocks`: how to draw ribbons. `auto` asks the terminal if it shows Kitty graphics.
- `--theme NAME`: syntect theme (default `base16-ocean.dark`).
- `--name PATH`: display name, also used to pick the syntax.

## Config file

`~/.config/ribbon/config.toml` (or under `$XDG_CONFIG_HOME`) sets defaults with the long option
names. The command line wins over it.

```toml
context = 5
ignore-comments = true
layout = "split"
watch = "500ms"   # or true
scroll-off = 4
wrap = true

[keys]            # a key = the key it acts as
"ctrl-d" = "pagedown"
"K" = "{"
```

Key names are one character, or `space`, `enter`, `tab`, `esc`, `backspace`, `up`, `down`, `left`,
`right`, `pageup`, `pagedown`, `home`, `end`, with `ctrl-` or `alt-` in front.

## License

MIT. See [LICENSE](LICENSE).
