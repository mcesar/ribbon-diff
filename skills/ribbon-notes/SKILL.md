---
name: ribbon-notes
description: Act on the notes a person left on a diff in ribbon (the side-by-side terminal diff viewer), explain your own code changes with notes they see beside the code, show code-review findings in ribbon, and move the person's ribbon view with `ribbon send`. Use when the user says they left notes, comments, or review remarks in ribbon, mentions .ribbon/notes.json, asks you to explain your changes in ribbon, asks you to show something in ribbon, runs a code review in a repository that has a .ribbon folder, or when you change code in such a repository.
---

# Notes in ribbon

ribbon shows a git diff side by side. The person can select lines and press `a` to leave a
note for you. Notes are kept in `.ribbon/notes.json` at the top of the repository. ribbon
reads the file again every second, so your changes to it show at once.

## The file

```json
{
  "version": 1,
  "files": [
    {
      "path": "src/search.ts",
      "annotations": [
        {
          "id": "n1764000000000",
          "newRange": [12, 14],
          "anchor": "  return results;",
          "summary": "Sort the results before the slice.",
          "author": "you",
          "status": "open"
        }
      ]
    }
  ]
}
```

- `path` is relative to the top of the repository.
- `newRange` is `[first, last]`, 1-based and inclusive, in the new version of the file (the
  working tree, for an uncommitted diff). `oldRange` instead means lines of the old version:
  usually lines the change removed.
- `anchor` is the text of the last line of the range when the note was written. Line numbers
  move when code is edited: when they no longer match, find the line by its anchor text (the
  copy nearest to the range).
- `summary` is the note. `author` is `"you"` for the person's notes.
- `status` is `"open"` or `"done"`. A missing status means open. `"posted"` means the person
  posted the note to a pull request on GitHub (`P` in `ribbon pr`): it is not a task for you.

## Act on the open notes

1. Read `.ribbon/notes.json`. The open notes are the ones with `author` `"you"` and a status
   that is `"open"` or missing, and the findings (`author` `"review"`) with status `"open"`:
   the person chose to fix those.
2. For each open note, read the code at its range, then do what it asks. A question needs an
   answer, not a code change.
3. Then update that note:
   - set `"status": "done"`, and
   - add `"reply"`: one or two sentences that say what you changed, or the answer.

   If you can't do what the note asks, or the person must decide, keep the status `"open"`
   and write the question in `"reply"`.
4. Change only `status` and `reply`. Do not remove notes. Do not change `id`, the ranges,
   `anchor`, `summary`, or `author`, and keep fields you don't know.
5. Keep the file valid JSON. Read it again just before you write it, because the person can
   add notes while you work, and change only the notes you handled. ribbon refuses to
   overwrite a file it can't parse, so a broken file stops the person from adding notes.

Then tell the person how many notes you handled. In ribbon each one shows `done` with your
reply. With `ribbon --watch`, the diff also shows your code changes at once; without it, they
show after the person opens and closes the editor with `e`, or opens ribbon again.

## Explain your own changes

In a repository with a `.ribbon` folder, the person reads your changes in ribbon. Whenever you
change code there, for a note or for any other request, add one note of your own for each
change, so they see why beside the code. Add them when you finish the change, before you
reply. Skip changes too small to need a reason, such as a typo fix.

```json
{
  "id": "c1",
  "newRange": [40, 52],
  "anchor": "  }",
  "summary": "Retries the upload three times before it gives up.",
  "rationale": "The storage API fails about 1% of first attempts; the old code showed those as errors.",
  "author": "claude"
}
```

- Use `"author": "claude"` and leave out `status`: these are not tasks.
- Put the range only on lines your change added or changed, as the diff shows them. Don't stretch
  it over a whole function when you changed part of it. For code you only removed, use
  `oldRange`.
- When you change the same lines again, replace your earlier note on them instead of adding a
  second one. You may change or remove your own notes; never the person's.
- Give each one a unique `id`, and the text of the range's last line as `anchor`.
- Keep `summary` to one sentence about what changed. Put the reason in `rationale`.
- Add the file entry if the file has none yet.

ribbon shows each note under the last line of its range. A note on a line inside a fold opens
that fold.

## Code-review findings

In a repository with a `.ribbon` folder, after a code review (`/code-review`, `/review`, or a
review the person asks for), also put each finding in ribbon, so the person can go through them
beside the code. Add one per finding, after you report them:

```sh
ribbon send finding src/search.ts:40-52 "Sorts after the slice, so page 2 repeats items." \
  --rationale "With 30 results and a page size of 20, page 2 shows items 1-10 again."
```

- The range is the lines the finding is about, in the new version (`--old` for removed lines).
- The text is one sentence about the problem; `--rationale` holds the failure scenario.
- It is a note with `"author": "review"` and `"status": "new"`. Without a listening ribbon,
  `ribbon send finding` writes it to the notes file itself, so it works either way.

In ribbon, the person presses `F` on each finding to have you fix it (status `"open"`) or `D`
to dismiss it (status `"dismissed"`). When they ask you to handle the notes, fix the `"open"`
findings as in "Act on the open notes": set `"done"` and a `"reply"`. Never fix a finding that
is still `"new"` or `"dismissed"`.

## Control the view

When the person runs `ribbon --listen`, you can move their view. Lines are 1-based; paths can
be relative to the current folder or to the top of the repository; `--old` means the old
version. Each command prints a JSON reply; `"ok": false` has an `"error"`.

```sh
ribbon send navigate src/search.ts:40        # scroll to a line (or just a file)
ribbon send highlight src/search.ts:40-52    # mark lines and scroll to them
ribbon send clear                            # remove the mark
ribbon send reload                           # read the files and notes again
ribbon send note src/search.ts:40-52 "Retries three times." --rationale "Why."
ribbon send status                           # the file and line on screen, open notes, findings
```

Use `navigate` or `highlight` when you explain code to the person ("this is the line that
fails"), and `reload` after you change files when ribbon does not run with `--watch`. `note`
adds a note with `"author": "claude"`, like the notes in "Explain your own changes". When
nothing listens, `navigate`, `highlight`, `clear`, `reload`, and `status` fail with "no ribbon
listens": do not start ribbon yourself; it needs the person's terminal.

## Notes from other tools

`ribbon --agent-context FILE` shows a file in the same format (Hunk's agent-context format),
read-only, beside the diff. Hunk can read `.ribbon/notes.json` too; it ignores the `id`,
`anchor`, `status`, and `reply` fields.
