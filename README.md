# Truce

Resolve git merge conflicts using fast System One decision models.

`truce` is a Rust CLI that you run inside a git working tree. It finds unmerged
files, parses each conflict hunk (including optional diff3 / zdiff3 base
sections), sends each hunk to an OpenRouter decision model, and either prints a
diff-style preview or writes the resolution.

## Install

From a clone of this repository:

```bash
cargo install --path .
```

From git without cloning:

```bash
cargo install --git https://github.com/AJFrio/Truce --locked
```

Requires Rust 1.80 or newer (see `rust-version` in `Cargo.toml`). `git` must be
on `PATH`. Prebuilt binaries for Linux, macOS, and Windows are attached to
[GitHub Releases](https://github.com/AJFrio/Truce/releases) (checksums in
`SHA256SUMS`).

## Usage

Export an OpenRouter key (Truce never prints it):

```bash
export OPENROUTER_API_KEY=sk-or-...
```

Run from anywhere inside a repository that has a merge (or rebase / cherry-pick
/ revert) in progress:

```bash
truce                      # call the model, print a preview (dry-run)
truce --json
truce --apply              # write files whose hunks all resolved
truce --apply --stage      # write, then git add those files
truce --model openai/gpt-4o-mini --min-confidence 0.8
truce --context 5 src/
```

`--dry-run` is the default: the model still runs when a key is set, but files
are not modified. A file is rewritten only when **every** hunk in it resolved
validly. Truce never runs `git add` unless you pass `--stage`.

Without `OPENROUTER_API_KEY`, `truce` lists hunks and does not call a model.
`--apply` without a key exits 2.

```
truce [OPTIONS] [PATH]...

  --json                 JSON report on stdout (schema_version 1)
  --dry-run              Preview only (default)
  --apply                Write accepted resolutions to the working tree
  --stage                git add files after a successful --apply
  --model <MODEL>        OpenRouter model id
  --min-confidence <N>   Reject below this confidence [default: 0.7]
  --concurrency <N>      Max in-flight requests [default: 8]
  --timeout <SECS>       Per-request timeout [default: 30]
  --retries <N>          Retries on 429/5xx/timeouts [default: 3]
  --base-url <URL>       API base (tests / proxy)
  --context <N>          Surrounding context lines per hunk [default: 3]
  --no-color             Disable ANSI color even on a TTY
  --ci                   GitHub Actions mode (also auto-detected)
  PATH                   Limit scanning to these files or directories
```

Human output is colorized only when stdout is a TTY. Color is disabled if you
pass `--no-color` or `--ci`, if `GITHUB_ACTIONS=true`, if `NO_COLOR` is set, or
if `TERM=dumb`. Status words such as `OURS`, `THEIRS`, `BASE`, `CONFLICTED`,
`SKIPPED`, `RESOLVED`, and `REJECTED` are always printed, so the report does
not depend on color.

Binary files (NUL bytes) and non-UTF-8 files are skipped and listed; they are
never rewritten. Paths with spaces are handled. Malformed or unterminated
conflict markers are reported as parse errors — Truce does not panic.

Rejected hunks (leftover conflict markers, empty/garbage output, or confidence
below `--min-confidence`) stay unresolved and are reported. They are never
silently applied.

## Model selection

First match wins:

1. `--model`
2. `TRUCE_MODEL`
3. `.truce.toml` in the repo root
4. `$XDG_CONFIG_HOME/truce/config.toml` (or `~/.config/truce/config.toml`)
5. Built-in default: `openai/gpt-4o-mini`

Example `.truce.toml`:

```toml
model = "openai/gpt-4o-mini"
min_confidence = 0.7
concurrency = 8
timeout_secs = 30
retries = 3
```

The API base URL defaults to `https://openrouter.ai/api/v1` and can be
overridden with `--base-url`, `TRUCE_BASE_URL`, or `OPENROUTER_BASE_URL`.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | No conflicts, or every conflict was resolved |
| 1 | Unresolved conflicts remain (including skipped binaries) |
| 2 | Usage error or runtime failure (not a git repo, parse error, missing key on `--apply`, auth failure, IO, …) |

Clap usage errors (`truce --bogus`) also exit 2. `--help` and `--version` exit 0.

## JSON schema (`schema_version`: 1)

`--json` prints one object to stdout. Optional fields (`model`, `base`,
`base_label`, `error`, `resolution`, `rejection`) are omitted when null.

```json
{
  "schema_version": 1,
  "tool": "truce",
  "repo_root": "/abs/path/to/repo",
  "merge_in_progress": true,
  "dry_run": true,
  "model": "openai/gpt-4o-mini",
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
          "ours": ["boot_v1();"],
          "theirs": ["boot_v2();"],
          "context_before": ["fn start() {"],
          "context_after": ["}"],
          "resolution": {
            "text": "boot_v2();",
            "choice": "theirs",
            "rationale": "feature already migrated the API",
            "confidence": 0.91
          }
        }
      ]
    }
  ],
  "summary": {
    "conflicted_files": 1,
    "hunks": 1,
    "skipped_files": 0,
    "resolved_hunks": 1,
    "unresolved_hunks": 0,
    "parse_errors": 0,
    "files_written": 0,
    "files_staged": 0
  }
}
```

Notes:

- `path` is repository-relative with `/` separators.
- `status` is one of `conflicted`, `binary`, `non_utf8`, `missing`,
  `parse_error`, `unmerged_without_markers`.
- Hunk `ours` / `theirs` / `base` / context arrays are lines **without** EOL
  characters. `line_ending` is `lf` or `crlf` for the file as a whole.
- `choice` is `ours`, `theirs`, or `merged`.
- Field order is the struct declaration order; treat names, not column
  position, as the contract.

## How it works

1. `git rev-parse --show-toplevel` — must be inside a work tree.
2. `git ls-files -z -u` and `git diff -z --name-only --diff-filter=U` — unmerged
   paths, NUL-separated so names with spaces round-trip.
3. Conflict markers `<<<<<<<`, optional `|||||||` (diff3/zdiff3 base),
   `=======`, and `>>>>>>>` are parsed with a state machine.
4. Each hunk is POSTed to `{base_url}/chat/completions` with a JSON schema
   response format. Requests run in parallel up to `--concurrency`, with
   jittered exponential backoff on 429/5xx/timeouts (honoring `Retry-After`).
   Other 4xx errors are not retried.
5. Valid resolutions are previewed as a diff. `--apply` writes atomically
   (temp file + rename) and preserves the file's line endings.

## GitHub Actions

`--ci`, or `GITHUB_ACTIONS=true`, turns on CI mode: no color, non-interactive,
`::error` / `::warning` workflow annotations (file + line), and a Markdown job
summary appended to `$GITHUB_STEP_SUMMARY`. Exit codes stay 0 / 1 / 2.

A composite action lives at the repository root (`action.yml`). Copy
[`examples/truce-pr.yml`](examples/truce-pr.yml) into a consuming repo's
`.github/workflows/` — that file is an example only and is not active here.

```yaml
- uses: AJFrio/Truce@v1
  with:
    openrouter-api-key: ${{ secrets.OPENROUTER_API_KEY }}
    mode: report          # or commit
    # model: openai/gpt-4o-mini
    # min-confidence: "0.7"
    # target-branch: main
```

| Input | Default | Notes |
| --- | --- | --- |
| `openrouter-api-key` | required | Pass `secrets.OPENROUTER_API_KEY`. Never echoed. |
| `mode` | `report` | `report` previews; `commit` applies, stages, commits, and pushes a `truce/resolve-*` branch **only when every conflict resolved** (truce exit 0). A clean merge exits 0 without pushing. Unresolved hunks skip commit/push and keep truce's exit code. |
| `model` | config / `openai/gpt-4o-mini` | OpenRouter model id. |
| `min-confidence` | `0.7` | Reject below this confidence. |
| `target-branch` | `github.base_ref` | Branch merged into the PR head before Truce runs. |
| `github-token` | `github.token` | Checkout (and push in commit mode). |

Store the OpenRouter key as the repository secret `OPENROUTER_API_KEY`. Do not
put it in logs, `echo`, or workflow `run:` argv. Report mode needs
`contents: read`. Commit mode needs `contents: write` so the default token can
push.

This repository's own CI (`.github/workflows/ci.yml`) runs `cargo fmt`,
`clippy -D warnings`, and `cargo test` on Ubuntu, macOS, and Windows. A
release workflow (`.github/workflows/release.yml`) builds tagged `v*` releases
for linux x86_64/aarch64, macOS arm64/x86_64, and Windows x86_64, and attaches
SHA-256 checksums.
