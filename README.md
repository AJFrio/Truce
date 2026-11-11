# Truce

Resolve git merge conflicts using fast System One decision models.

`truce` is a Rust CLI that you run inside a git working tree. It finds unmerged
files, parses each conflict hunk (including optional diff3 / zdiff3 base
sections), and prints a report you can read or pipe as JSON.

This release (ENG-55) covers detection, parsing, and output. `--apply` is wired
as the write path but does not resolve hunks yet; the OpenRouter resolver lands
in ENG-56.

## Install

From a clone of this repository:

```bash
cargo install --path .
```

Requires Rust 1.80 or newer (see `rust-version` in `Cargo.toml`). `git` must be
on `PATH`.

## Usage

Run from anywhere inside a repository that has a merge (or rebase / cherry-pick
/ revert) in progress:

```bash
truce
truce --json
truce --context 5 src/
truce --apply          # writes only when every hunk in a file is resolved
```

`--dry-run` is the default: Truce never writes files unless you pass `--apply`.
It never runs `git add`.

```
truce [OPTIONS] [PATH]...

  --json            JSON report on stdout (schema_version 1)
  --dry-run         Preview only (default)
  --apply           Write accepted resolutions to the working tree
  --context <N>     Surrounding context lines per hunk [default: 3]
  --no-color        Disable ANSI color even on a TTY
  PATH              Limit scanning to these files or directories
```

Human output is colorized only when stdout is a TTY. Color is disabled if you
pass `--no-color`, if `NO_COLOR` is set, or if `TERM=dumb`. Status words such as
`OURS`, `THEIRS`, `BASE`, `CONFLICTED`, `SKIPPED`, and `RESOLVED` are always
printed, so the report does not depend on color.

Binary files (NUL bytes) and non-UTF-8 files are skipped and listed; they are
never rewritten. Paths with spaces are handled. Malformed or unterminated
conflict markers are reported as parse errors — Truce does not panic.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | No conflicts, or every conflict was resolved |
| 1 | Unresolved conflicts remain (including skipped binaries) |
| 2 | Usage error or runtime failure (not a git repo, parse error, IO, …) |

Clap usage errors (`truce --bogus`) also exit 2. `--help` and `--version` exit 0.

## JSON schema (`schema_version`: 1)

`--json` prints one object to stdout:

```json
{
  "schema_version": 1,
  "tool": "truce",
  "repo_root": "/abs/path/to/repo",
  "merge_in_progress": true,
  "dry_run": true,
  "files": [
    {
      "path": "src/app.rs",
      "status": "conflicted",
      "line_ending": "lf",
      "trailing_newline": true,
      "hunks": [
        {
          "id": 1,
          "start_line": 2,
          "end_line": 8,
          "ours_label": "HEAD",
          "theirs_label": "feature",
          "base_label": "merged common ancestors",
          "ours": ["boot_v1();"],
          "theirs": ["boot_v2();"],
          "base": ["boot();"],
          "context_before": ["fn start() {"],
          "context_after": ["}"],
          "resolution": {
            "text": "boot_v2();",
            "choice": "theirs",
            "rationale": "…",
            "confidence": 0.9
          }
        }
      ]
    }
  ],
  "summary": {
    "conflicted_files": 1,
    "hunks": 1,
    "skipped_files": 0,
    "resolved_hunks": 0,
    "unresolved_hunks": 1,
    "parse_errors": 0,
    "files_written": 0
  }
}
```

Notes:

- `path` is repository-relative with `/` separators.
- `status` is one of `conflicted`, `binary`, `non_utf8`, `missing`,
  `parse_error`, `unmerged_without_markers`.
- Hunk `ours` / `theirs` / `base` / context arrays are lines **without** EOL
  characters. `line_ending` is `lf` or `crlf` for the file as a whole.
- `base` / `base_label` / `error` / `resolution` are omitted when null.
- `resolution` is always absent in this release (no model yet).
- Field order is the struct declaration order; treat names, not column
  position, as the contract.

## How detection works

Truce shells out to `git` (no libgit2):

1. `git rev-parse --show-toplevel` — must be inside a work tree.
2. `git ls-files -z -u` and `git diff -z --name-only --diff-filter=U` — unmerged
   paths, NUL-separated so names with spaces round-trip.
3. `MERGE_HEAD` / `REBASE_HEAD` / `CHERRY_PICK_HEAD` / `REVERT_HEAD` (and the
   rebase directories) — whether a merge-like operation is in progress.

Conflict markers `<<<<<<<`, optional `|||||||` (diff3/zdiff3 base), `=======`,
and `>>>>>>>` are parsed with a state machine. Multiple hunks per file, CRLF
line endings, and a missing trailing newline are all supported.
